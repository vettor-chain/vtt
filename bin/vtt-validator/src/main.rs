use std::env;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use vtt_chain::Chain;
use vtt_consensus::ConsensusEngine;
use vtt_crypto::{blake3_hash, merkle_root, Keypair};
use vtt_executor::execute_block_transactions;
use vtt_genesis::{build_genesis, genesis_hash, GenesisConfig};
use vtt_network::{NetworkConfig, NetworkEvent, NetworkService};
use vtt_primitives::block::{Block, BlockHeader};
use vtt_primitives::chain::GasConfig;
use vtt_primitives::{Signature, H256};
use vtt_rpc::RpcServer;
use vtt_txpool::{TxPool, TxPoolConfig};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("VTT Validator v{}", env!("CARGO_PKG_VERSION"));

    let args: Vec<String> = env::args().collect();
    let port: u16 = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(30333);

    let rpc_port: u16 = args
        .iter()
        .position(|a| a == "--rpc-port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(9944);

    let validator_seed: [u8; 32] = args
        .iter()
        .position(|a| a == "--seed")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| b.try_into().ok())
        .unwrap_or([0x10; 32]);

    let keypair = Keypair::from_seed(&validator_seed);
    let validator_addr = keypair.address();
    info!(%validator_addr, "validator identity loaded");

    // Genesis
    let genesis_config = GenesisConfig::dev_default();
    let genesis_result = build_genesis(&genesis_config);
    let gen_hash = genesis_hash(&genesis_result.block);
    info!(?gen_hash, "genesis block built");

    // Chain
    let consensus = ConsensusEngine::new(genesis_config.chain.consensus.clone());
    let gas_config = genesis_config.chain.gas.clone();
    let block_time_ms = genesis_config.chain.consensus.block_time_ms;
    let mut chain = Chain::new(consensus, gas_config.clone());
    chain
        .init_genesis(genesis_result.block, genesis_result.state)
        .expect("genesis init failed");

    let chain = Arc::new(RwLock::new(chain));
    let txpool = Arc::new(RwLock::new(TxPool::new(TxPoolConfig::default())));

    // RPC
    let rpc = RpcServer::new(chain.clone(), txpool.clone());
    let rpc_addr = format!("127.0.0.1:{rpc_port}").parse().unwrap();
    if let Err(e) = rpc.start(rpc_addr).await {
        error!(%e, "failed to start RPC server");
        return;
    }

    // Network
    let net_config = NetworkConfig::dev(port);
    let mut network = match NetworkService::new(&net_config) {
        Ok(n) => n,
        Err(e) => {
            error!(%e, "failed to create network service");
            return;
        }
    };
    let _ = network.start_listening(&net_config);

    info!(
        %validator_addr,
        peer_id = %network.local_peer_id(),
        port,
        rpc_port,
        "validator started"
    );

    // Block production loop
    let mut block_timer = tokio::time::interval(Duration::from_millis(block_time_ms));

    loop {
        tokio::select! {
            _ = block_timer.tick() => {
                try_produce_block(&chain, &txpool, &keypair, &gas_config);
            }
            event = network.next_event() => {
                match event {
                    NetworkEvent::Listening { address } => {
                        info!(%address, "listening on");
                    }
                    NetworkEvent::PeerConnected { peer_id } => {
                        info!(%peer_id, "peer connected");
                    }
                    NetworkEvent::PeerDisconnected { peer_id } => {
                        debug!(%peer_id, "peer disconnected");
                    }
                    NetworkEvent::Message(msg) => {
                        debug!(?msg, "received message");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                break;
            }
        }
    }
}

fn try_produce_block(
    chain: &Arc<RwLock<Chain>>,
    txpool: &Arc<RwLock<TxPool>>,
    keypair: &Keypair,
    gas_config: &GasConfig,
) {
    let mut chain = chain.write().unwrap();
    let validator_addr = keypair.address();

    // Check if it's our turn
    let head = match chain.head() {
        Some(h) => h.clone(),
        None => return,
    };

    let next_number = head.number + 1;
    let validator_set = chain.validator_set().clone();

    let expected = match chain
        .consensus()
        .block_producer(&validator_set, next_number)
    {
        Ok(v) => v.address,
        Err(_) => return,
    };

    if expected != validator_addr {
        return; // not our slot
    }

    // Collect transactions from pool
    let pool = txpool.read().unwrap();
    let nonces = std::collections::HashMap::new(); // TODO: populate from state
    let txs = pool.select_transactions(100, &nonces);
    drop(pool);

    // Execute transactions
    let (receipts, gas_used) =
        execute_block_transactions(chain.state_mut(), &txs, gas_config, 10_000_000);

    let state_root = chain.state_mut().compute_state_root();

    let tx_hashes: Vec<H256> = txs
        .iter()
        .map(|tx| blake3_hash(&tx.payload_bytes()))
        .collect();
    let tx_root = merkle_root(&tx_hashes);

    let receipt_hashes: Vec<H256> = receipts
        .iter()
        .map(|r| blake3_hash(&borsh::to_vec(r).unwrap()))
        .collect();
    let receipts_root = merkle_root(&receipt_hashes);

    let parent_hash = blake3_hash(&head.signable_bytes());

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let mut header = BlockHeader {
        version: 1,
        chain_id: head.chain_id,
        number: next_number,
        parent_hash,
        transactions_root: tx_root,
        state_root,
        receipts_root,
        validator: validator_addr,
        epoch: chain.consensus().epoch_for_block(next_number),
        slot: chain.consensus().slot_for_block(next_number),
        timestamp: now_ms,
        gas_limit: 10_000_000,
        gas_used,
        cross_chain_root: None,
        signature: Signature::ZERO,
    };

    // Sign the block
    let signable = header.signable_bytes();
    header.signature = keypair.sign(&signable);

    let block = Block::new(header, txs);
    let block_hash = blake3_hash(&block.header.signable_bytes());

    // Import our own block
    match chain.import_block(block) {
        Ok(result) => {
            info!(
                number = result.block_number,
                ?block_hash,
                txs = result.receipts.len(),
                gas_used,
                "produced block"
            );
        }
        Err(e) => {
            warn!(%e, "failed to import produced block");
        }
    }
}
