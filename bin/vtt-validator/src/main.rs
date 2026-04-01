use std::env;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use vtt_chain::Chain;
use vtt_consensus::rewards::{calculate_epoch_reward, split_block_reward, split_gas_fees};
use vtt_consensus::ConsensusEngine;
use vtt_crypto::{blake3_hash, merkle_root, Keypair};
use vtt_executor::execute_block_transactions_at;
use vtt_genesis::{build_genesis, genesis_hash, GenesisConfig};
use vtt_network::messages::NetworkMessage;
use vtt_network::{NetworkConfig, NetworkEvent, NetworkService};
use vtt_primitives::amount::Amount;
use vtt_primitives::block::{Block, BlockHeader};
use vtt_primitives::chain::GasConfig;
use vtt_primitives::{Address, Signature, H256};
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

    // --genesis <path> : load genesis config from JSON file
    let genesis_path: Option<String> = args
        .iter()
        .position(|a| a == "--genesis")
        .and_then(|i| args.get(i + 1))
        .cloned();

    // --bootnodes <addr1,addr2,...> : comma-separated multiaddrs of boot nodes
    let bootnodes: Vec<String> = args
        .iter()
        .position(|a| a == "--bootnodes")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect())
        .unwrap_or_default();

    let keypair = Keypair::from_seed(&validator_seed);
    let validator_addr = keypair.address();
    info!(%validator_addr, "validator identity loaded");

    // Genesis
    let genesis_config = match &genesis_path {
        Some(path) => {
            let data = match std::fs::read_to_string(path) {
                Ok(d) => d,
                Err(e) => {
                    error!(%e, path, "failed to read genesis file");
                    return;
                }
            };
            match serde_json::from_str::<GenesisConfig>(&data) {
                Ok(c) => {
                    info!(path, "loaded genesis config from file");
                    c
                }
                Err(e) => {
                    error!(%e, path, "failed to parse genesis config");
                    return;
                }
            }
        }
        None => GenesisConfig::dev_default(),
    };

    let genesis_result = build_genesis(&genesis_config);
    let gen_hash = genesis_hash(&genesis_result.block);
    info!(?gen_hash, "genesis block built");

    // Chain
    let consensus = ConsensusEngine::new(genesis_config.chain.consensus.clone());
    let gas_config = genesis_config.chain.gas.clone();
    let block_time_ms = genesis_config.chain.consensus.block_time_ms;
    let chain_id = genesis_config.chain.chain_id;
    let mut chain = Chain::new(consensus, gas_config.clone());
    chain
        .init_genesis(genesis_result.block, genesis_result.state)
        .expect("genesis init failed");

    let chain = Arc::new(RwLock::new(chain));
    let txpool = Arc::new(RwLock::new(TxPool::new(TxPoolConfig::default())));

    // RPC
    let rpc = RpcServer::new(chain.clone(), txpool.clone());
    let rpc_state = rpc.shared_state();
    let rpc_addr = format!("127.0.0.1:{rpc_port}").parse().unwrap();
    if let Err(e) = rpc.start(rpc_addr).await {
        error!(%e, "failed to start RPC server");
        return;
    }

    // Network — use the chain_id from genesis for topic isolation
    let mut net_config = NetworkConfig::dev(port);
    net_config.chain_id = chain_id;
    net_config.boot_nodes = bootnodes.clone();

    let mut network = match NetworkService::new(&net_config) {
        Ok(n) => n,
        Err(e) => {
            error!(%e, "failed to create network service");
            return;
        }
    };
    let _ = network.start_listening(&net_config);

    // Dial boot nodes
    for bootnode in &bootnodes {
        if let Err(e) = network.dial_bootnode(bootnode) {
            warn!(%e, bootnode, "failed to dial boot node");
        }
    }

    info!(
        %validator_addr,
        peer_id = %network.local_peer_id(),
        port,
        rpc_port,
        bootnodes = bootnodes.len(),
        %chain_id,
        "validator started"
    );

    // Block production loop
    let mut block_timer = tokio::time::interval(Duration::from_millis(block_time_ms));

    loop {
        tokio::select! {
            _ = block_timer.tick() => {
                let maybe_broadcast = try_produce_block(&chain, &txpool, &keypair, &gas_config, &rpc_state);
                if let Some(msg) = maybe_broadcast {
                    if let Err(e) = network.broadcast_block(&msg) {
                        warn!(%e, "failed to broadcast block");
                    }
                }
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
                        handle_network_message(*msg, &chain, &txpool, &validator_addr);
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

/// Handle an incoming network message from a peer.
fn handle_network_message(
    msg: NetworkMessage,
    chain: &Arc<RwLock<Chain>>,
    txpool: &Arc<RwLock<TxPool>>,
    validator_addr: &Address,
) {
    match msg {
        NetworkMessage::BlockAnnounce { block_hash, block_number, block } => {
            // Don't import our own blocks (they're already imported during production)
            if block.header.validator == *validator_addr {
                return;
            }
            debug!(?block_hash, block_number, "received block from peer");

            let mut chain = chain.write().unwrap();
            match chain.import_block(block) {
                Ok(result) => {
                    info!(
                        number = result.block_number,
                        ?block_hash,
                        "imported peer block"
                    );
                }
                Err(e) => {
                    warn!(%e, block_number, "failed to import peer block");
                }
            }
        }
        NetworkMessage::TransactionBroadcast { transaction } => {
            let sender = vtt_crypto::address_from_public_key(&transaction.public_key);
            let account_nonce = {
                let chain = chain.read().unwrap();
                chain.state().get_nonce(&sender)
            };
            let mut pool = txpool.write().unwrap();
            if let Err(e) = pool.add(transaction, sender, account_nonce) {
                debug!(%e, %sender, "rejected peer transaction");
            }
        }
        // Other message types (BlockRequest, BlockResponse, etc.) are not handled yet
        other => {
            debug!(?other, "unhandled network message type");
        }
    }
}

/// Try to produce a block if it's our turn. Returns a NetworkMessage to broadcast on success.
fn try_produce_block(
    chain: &Arc<RwLock<Chain>>,
    txpool: &Arc<RwLock<TxPool>>,
    keypair: &Keypair,
    gas_config: &GasConfig,
    rpc_state: &Arc<vtt_rpc::RpcState>,
) -> Option<NetworkMessage> {
    let mut chain = chain.write().unwrap();
    let validator_addr = keypair.address();

    // Check if it's our turn
    let head = match chain.head() {
        Some(h) => h.clone(),
        None => return None,
    };

    let next_number = head.number + 1;
    let validator_set = chain.validator_set().clone();

    let expected = match chain
        .consensus()
        .block_producer(&validator_set, next_number)
    {
        Ok(v) => v.address,
        Err(_) => return None,
    };

    if expected != validator_addr {
        return None; // not our slot
    }

    // Collect transactions from pool with correct account nonces
    let pool = txpool.read().unwrap();
    let mut nonces = std::collections::HashMap::new();
    for sender in pool.senders() {
        nonces.insert(sender, chain.state().get_nonce(&sender));
    }
    let txs = pool.select_transactions(100, &nonces);
    drop(pool);

    // Compute block timestamp before execution so contracts see the real time
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // Execute transactions with actual block number and timestamp
    let (receipts, gas_used) =
        execute_block_transactions_at(
            chain.state_mut(),
            &txs,
            gas_config,
            10_000_000,
            next_number,
            now_ms,
        );

    // --- Block rewards & gas fee distribution ---
    let treasury_addr = Address::ZERO; // protocol treasury = zero address

    // 1. Block reward: inflation-based, split 80% producer / 20% treasury
    let total_staked = validator_set.total_stake();
    let total_supply = Amount::from_vtt(1_000_000_000); // TODO: track actual circulating supply
    let staking_ratio_pct = if total_supply.raw() > 0 {
        (total_staked.raw() * 100 / total_supply.raw()) as u64
    } else {
        0
    };
    let epoch_reward = calculate_epoch_reward(total_supply, staking_ratio_pct);
    let epoch_length = chain.consensus().params().epoch_length;
    let per_block_reward = if epoch_length > 0 {
        Amount::from_raw(epoch_reward.raw() / epoch_length as u128)
    } else {
        Amount::ZERO
    };

    if per_block_reward.raw() > 0 {
        let split = split_block_reward(per_block_reward);
        let _ = chain.state_mut().add_balance(&validator_addr, split.producer);
        let _ = chain.state_mut().add_balance(&treasury_addr, split.treasury);
        // Track in milli-VTT (raw / 10^15)
        let minted_milli = (per_block_reward.raw() / 10u128.pow(15)) as u64;
        rpc_state.total_minted_milli.fetch_add(minted_milli, std::sync::atomic::Ordering::Relaxed);
    }

    // 2. Gas fees: 70% burned, 30% to producer
    let total_gas_fees = Amount::from_raw(gas_used as u128 * gas_config.min_gas_price.raw());
    if total_gas_fees.raw() > 0 {
        let gas_split = split_gas_fees(total_gas_fees);
        // burned portion is simply not credited to anyone (effectively removed)
        let _ = chain.state_mut().add_balance(&validator_addr, gas_split.producer);
        let burned_milli = (gas_split.burned.raw() / 10u128.pow(15)) as u64;
        rpc_state.total_burned_milli.fetch_add(burned_milli, std::sync::atomic::Ordering::Relaxed);
    }

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

    // Build the broadcast message BEFORE importing (import_block takes ownership via clone)
    let broadcast_msg = NetworkMessage::BlockAnnounce {
        block_hash,
        block_number: block.header.number,
        block: block.clone(),
    };

    // Import our own block
    match chain.import_block(block) {
        Ok(result) => {
            // Remove mined transactions from the pool by committed nonce
            let mut pool = txpool.write().unwrap();
            for sender in &nonces {
                let committed_nonce = chain.state().get_nonce(sender.0);
                if committed_nonce > *sender.1 {
                    pool.remove_committed(sender.0, committed_nonce - 1);
                }
            }
            drop(pool);

            info!(
                number = result.block_number,
                ?block_hash,
                txs = result.receipts.len(),
                gas_used,
                "produced block"
            );

            Some(broadcast_msg)
        }
        Err(e) => {
            warn!(%e, "failed to import produced block");
            None
        }
    }
}
