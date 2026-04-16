use std::env;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use vtt_chain::Chain;
use vtt_consensus::finality::{FinalityTracker, FinalityVote};
use vtt_consensus::rewards::{calculate_epoch_reward, split_block_reward, split_gas_fees};
use vtt_consensus::ConsensusEngine;
use vtt_crypto::{blake3_hash, merkle_root, Keypair};
use vtt_executor::{
    execute_block_transactions_at, execute_queued_proposals, finalize_governance_proposals,
};
use vtt_genesis::{build_genesis, genesis_hash, GenesisConfig};
use vtt_network::messages::NetworkMessage;
use vtt_network::{NetworkConfig, NetworkEvent, NetworkService};
use vtt_primitives::amount::Amount;
use vtt_primitives::block::{Block, BlockHeader};
use vtt_primitives::chain::GasConfig;
use vtt_primitives::{Address, Signature, H256};
use vtt_rpc::RpcServer;
use vtt_storage::rocks::RocksStore;
use vtt_telemetry::NodeMetrics;
use vtt_txpool::{TxPool, TxPoolConfig};

/// Maximum number of blocks to request in a single sync batch.
const SYNC_BATCH_SIZE: u32 = 100;

/// How often (in blocks) to trigger RocksDB pruning.
const PRUNE_INTERVAL_BLOCKS: u64 = 10_000;

/// Number of recent blocks to keep when pruning (~3.5 days at 3s/block).
const KEEP_RECENT_BLOCKS: u64 = 100_000;

/// Monotonically increasing request ID for block range requests.
static NEXT_REQUEST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

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
        .or_else(|| {
            std::env::var("VALIDATOR_SEED")
                .ok()
                .and_then(|s| hex::decode(s).ok())
                .and_then(|b| b.try_into().ok())
        })
        .unwrap_or_else(|| {
            // In dev mode (no genesis file provided = dev_default with chain_id 0), allow hardcoded seed
            // For production (genesis file supplied), panic to prevent running with insecure default
            let is_dev = !args.iter().any(|a| a == "--genesis");
            if is_dev {
                warn!("no seed provided — using dev default (NOT SAFE FOR PRODUCTION)");
                [0x10; 32]
            } else {
                panic!("VALIDATOR_SEED not provided. Set --seed or VALIDATOR_SEED env var");
            }
        });

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
        .map(|s| {
            s.split(',')
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // --data-dir <path> : directory for persistent RocksDB storage
    let data_dir: Option<String> = args
        .iter()
        .position(|a| a == "--data-dir")
        .and_then(|i| args.get(i + 1))
        .cloned();

    // --metrics-port <port> : Prometheus metrics HTTP endpoint (default 9615)
    let metrics_port: u16 = args
        .iter()
        .position(|a| a == "--metrics-port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(9615);

    let keypair = Keypair::from_seed(&validator_seed);
    let validator_addr = keypair.address();
    info!(%validator_addr, "validator identity loaded");

    // --testnet : use multi-validator testnet genesis (T5)
    let is_testnet = args.iter().any(|a| a == "--testnet");

    // Genesis
    let genesis_config = if is_testnet {
        info!("using testnet genesis config (3 validators)");
        GenesisConfig::testnet_default()
    } else {
        match &genesis_path {
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
        }
    };

    let genesis_result = build_genesis(&genesis_config);
    let gen_hash = genesis_hash(&genesis_result.block);
    info!(?gen_hash, "genesis block built");

    // Chain
    let consensus = ConsensusEngine::new(genesis_config.chain.consensus.clone());
    let gas_config = genesis_config.chain.gas.clone();
    let block_time_ms = genesis_config.chain.consensus.block_time_ms;
    let chain_id = genesis_config.chain.chain_id;
    let rocks_store: Option<Arc<RocksStore>> = if let Some(ref dir) = data_dir {
        let db_path = std::path::Path::new(dir);
        if let Err(e) = std::fs::create_dir_all(db_path) {
            error!(%e, path = dir, "failed to create data directory");
            return;
        }
        match RocksStore::open(db_path) {
            Ok(s) => {
                info!(path = dir, "opened RocksDB for persistent storage");
                Some(Arc::new(s))
            }
            Err(e) => {
                error!(%e, path = dir, "failed to open RocksDB");
                return;
            }
        }
    } else {
        None
    };
    let mut chain = if let Some(ref store) = rocks_store {
        Chain::with_storage(
            consensus,
            gas_config.clone(),
            store.clone() as Arc<dyn vtt_storage::KeyValueStore>,
        )
    } else {
        Chain::new(consensus, gas_config.clone())
    };
    chain
        .init_genesis(genesis_result.block, genesis_result.state)
        .expect("genesis init failed");

    // Telemetry
    let metrics = Arc::new(NodeMetrics::new());
    if let Some(height) = chain.height() {
        metrics.block_height.set(height as i64);
    }
    metrics
        .active_validators
        .set(chain.validator_set().validators.len() as i64);

    let chain = Arc::new(RwLock::new(chain));
    let txpool = Arc::new(RwLock::new(TxPool::new(TxPoolConfig::default())));

    // RPC
    let rpc = RpcServer::with_metrics(chain.clone(), txpool.clone(), metrics.clone());
    let rpc_state = rpc.shared_state();
    let rpc_addr = format!("0.0.0.0:{rpc_port}").parse().unwrap();
    if let Err(e) = rpc.start(rpc_addr).await {
        error!(%e, "failed to start RPC server");
        return;
    }

    // Prometheus metrics HTTP server
    {
        let metrics_clone = metrics.clone();
        let metrics_addr: std::net::SocketAddr = format!("0.0.0.0:{metrics_port}").parse().unwrap();
        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(metrics_addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!(%e, "failed to bind metrics server");
                    return;
                }
            };
            info!(%metrics_addr, "Prometheus metrics server started");
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let metrics_ref = metrics_clone.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let body = metrics_ref.export();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
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
        metrics_port,
        bootnodes = bootnodes.len(),
        %chain_id,
        data_dir = data_dir.as_deref().unwrap_or("(in-memory)"),
        "validator started"
    );

    // Finality tracker — T2
    let validator_count = {
        let chain_r = chain.read().expect("chain lock poisoned during init");
        chain_r.validator_set().validators.len()
    };
    let mut finality_tracker = FinalityTracker::new(validator_count);

    // Block production loop
    let mut block_timer = tokio::time::interval(Duration::from_millis(block_time_ms));
    let mut txpool_eviction_interval = tokio::time::interval(Duration::from_secs(60));
    txpool_eviction_interval.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = block_timer.tick() => {
                let maybe_broadcast = try_produce_block(&chain, &txpool, &keypair, &gas_config, &rpc_state);
                if let Some((msg, block_hash, block_number)) = maybe_broadcast {
                    // Update telemetry after block production
                    {
                        if let Ok(chain_r) = chain.read() {
                            if let Some(height) = chain_r.height() {
                                metrics.block_height.set(height as i64);
                            }
                            metrics.blocks_imported.inc();
                        }
                        if let Ok(pool_r) = txpool.read() {
                            metrics.txpool_size.set(pool_r.len() as i64);
                        }
                    }
                    metrics.connected_peers.set(network.connected_peers() as i64);

                    if let Err(e) = network.broadcast_block(&msg) {
                        warn!(%e, "failed to broadcast block");
                    }

                    // T2: Submit finality vote for our own block
                    let vote = FinalityVote {
                        voter: validator_addr,
                        block_hash,
                        block_number,
                    };
                    let became_final = finality_tracker.submit_vote(vote);
                    if became_final {
                        info!(block_number, "block finalized");
                        // Persist finalized block number into chain state
                        if let Ok(mut chain_w) = chain.write() {
                            chain_w.set_finalized_block(block_number);
                        }
                    }

                    // Broadcast finality vote
                    let signable = borsh::to_vec(&(validator_addr, block_hash, block_number))
                        .expect("finality vote serialization cannot fail");
                    let vote_sig = keypair.sign(&signable);
                    let vote_msg = NetworkMessage::FinalityVote {
                        voter: validator_addr,
                        block_hash,
                        block_number,
                        signature: vote_sig,
                    };
                    let _ = network.broadcast_block(&vote_msg);

                    // Periodic RocksDB pruning
                    if let Some(ref store) = rocks_store {
                        if block_number > 0 && block_number % PRUNE_INTERVAL_BLOCKS == 0 {
                            match store.prune_old_blocks(KEEP_RECENT_BLOCKS, block_number) {
                                Ok(pruned) if pruned > 0 => {
                                    info!(height = block_number, pruned, "pruned old block data");
                                    store.compact();
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    warn!(%e, "failed to prune old blocks");
                                }
                            }
                        }
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
                        metrics.connected_peers.set(network.connected_peers() as i64);

                        // T1: Send our status to the new peer
                        let status_msg = {
                            let chain_r = chain.read().expect("chain lock poisoned");
                            NetworkMessage::Status {
                                chain_id,
                                best_block_hash: chain_r.head_hash().unwrap_or(H256::ZERO),
                                best_block_number: chain_r.height().unwrap_or(0),
                                genesis_hash: gen_hash,
                            }
                        };
                        let _ = network.broadcast_block(&status_msg);
                    }
                    NetworkEvent::PeerDisconnected { peer_id } => {
                        debug!(%peer_id, "peer disconnected");
                        metrics.connected_peers.set(network.connected_peers() as i64);
                    }
                    NetworkEvent::Message(msg) => {
                        let maybe_broadcast = handle_network_message(
                            *msg,
                            &chain,
                            &txpool,
                            &validator_addr,
                            &mut finality_tracker,
                        );
                        // Broadcast any response messages (sync responses, etc.)
                        for resp in maybe_broadcast {
                            let _ = network.broadcast_block(&resp);
                        }
                        // Update metrics after importing peer blocks
                        if let Ok(chain_r) = chain.read() {
                            if let Some(height) = chain_r.height() {
                                metrics.block_height.set(height as i64);
                            }
                            // Update validator count in finality tracker on epoch changes
                            let new_count = chain_r.validator_set().validators.len();
                            finality_tracker.set_validator_count(new_count);
                        }
                    }
                }
            }
            _ = txpool_eviction_interval.tick() => {
                let mut pool = txpool.write().expect("txpool lock poisoned");
                let evicted = pool.evict_expired();
                if evicted > 0 {
                    debug!(evicted, remaining = pool.len(), "evicted expired transactions from pool");
                    metrics.txpool_size.set(pool.len() as i64);
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
/// Returns a list of messages to broadcast in response.
fn handle_network_message(
    msg: NetworkMessage,
    chain: &Arc<RwLock<Chain>>,
    txpool: &Arc<RwLock<TxPool>>,
    validator_addr: &Address,
    finality_tracker: &mut FinalityTracker,
) -> Vec<NetworkMessage> {
    match msg {
        NetworkMessage::BlockAnnounce {
            block_hash,
            block_number,
            block,
        } => {
            // Don't import our own blocks (they're already imported during production)
            if block.header.validator == *validator_addr {
                return vec![];
            }
            debug!(?block_hash, block_number, "received block from peer");

            let peer_validator = block.header.validator;
            let mut chain = match chain.write() {
                Ok(c) => c,
                Err(_) => {
                    warn!("chain lock poisoned, cannot import block");
                    return vec![];
                }
            };
            match chain.import_block(block) {
                Ok(result) => {
                    info!(
                        number = result.block_number,
                        ?block_hash,
                        "imported peer block"
                    );

                    // T2: Submit finality vote for peer's block
                    let vote = FinalityVote {
                        voter: peer_validator,
                        block_hash,
                        block_number: result.block_number,
                    };
                    let became_final = finality_tracker.submit_vote(vote);
                    if became_final {
                        info!(block_number = result.block_number, "block finalized");
                        chain.set_finalized_block(result.block_number);
                    }
                }
                Err(e) => {
                    warn!(%e, block_number, "failed to import peer block");
                }
            }
            vec![]
        }
        NetworkMessage::TransactionBroadcast { transaction } => {
            let sender = vtt_crypto::address_from_public_key(&transaction.public_key);
            let account_nonce = match chain.read() {
                Ok(chain) => chain.state().get_nonce(&sender),
                Err(_) => {
                    warn!("chain lock poisoned, cannot read nonce");
                    return vec![];
                }
            };
            let mut pool = match txpool.write() {
                Ok(p) => p,
                Err(_) => {
                    warn!("txpool lock poisoned, cannot add transaction");
                    return vec![];
                }
            };
            if let Err(e) = pool.add(transaction, sender, account_nonce) {
                debug!(%e, %sender, "rejected peer transaction");
            }
            vec![]
        }

        // T1: Block sync protocol — Status exchange
        NetworkMessage::Status {
            best_block_number,
            genesis_hash: _peer_genesis,
            ..
        } => {
            let chain_r = match chain.read() {
                Ok(c) => c,
                Err(_) => return vec![],
            };
            let our_height = chain_r.height().unwrap_or(0);

            if best_block_number > our_height {
                // Peer is ahead — request missing blocks
                let from = our_height + 1;
                let count = ((best_block_number - our_height) as u32).min(SYNC_BATCH_SIZE);
                let request_id = NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                info!(
                    our_height,
                    peer_height = best_block_number,
                    from,
                    count,
                    "peer is ahead, requesting blocks"
                );
                vec![NetworkMessage::BlockRangeRequest {
                    request_id,
                    from_number: from,
                    count,
                }]
            } else {
                debug!(
                    our_height,
                    peer_height = best_block_number,
                    "peer is at or behind our height"
                );
                vec![]
            }
        }

        // T1: Respond to block range requests
        NetworkMessage::BlockRangeRequest {
            request_id,
            from_number,
            count,
        } => {
            let chain_r = match chain.read() {
                Ok(c) => c,
                Err(_) => return vec![],
            };
            let mut blocks = Vec::new();
            let capped_count = count.min(SYNC_BATCH_SIZE);
            for i in 0..capped_count as u64 {
                let num = from_number + i;
                if let Some(block) = chain_r.get_block_by_number(num) {
                    blocks.push(block.clone());
                } else {
                    break;
                }
            }
            debug!(
                request_id,
                from_number,
                sent = blocks.len(),
                "responding to block range request"
            );
            vec![NetworkMessage::BlockRangeResponse { request_id, blocks }]
        }

        // T1: Import blocks from a range response
        NetworkMessage::BlockRangeResponse { request_id, blocks } => {
            if blocks.is_empty() {
                debug!(request_id, "received empty block range response");
                return vec![];
            }
            // blocks is guaranteed non-empty by the guard above
            let first = blocks[0].header.number;
            let last = blocks[blocks.len() - 1].header.number;
            info!(
                request_id,
                first,
                last,
                count = blocks.len(),
                "received block range response"
            );

            let mut chain = match chain.write() {
                Ok(c) => c,
                Err(_) => {
                    warn!("chain lock poisoned, cannot import sync blocks");
                    return vec![];
                }
            };
            let mut imported = 0u64;
            let block_count = blocks.len() as u32;
            for block in blocks {
                let num = block.header.number;
                match chain.import_block(block) {
                    Ok(result) => {
                        imported += 1;
                        debug!(number = result.block_number, "imported sync block");
                    }
                    Err(e) => {
                        debug!(%e, number = num, "failed to import sync block");
                        break;
                    }
                }
            }

            // Check if we need more blocks
            if imported > 0 {
                let our_height = chain.height().unwrap_or(0);
                // If the peer sent a full batch, there may be more
                if block_count == SYNC_BATCH_SIZE {
                    let next_from = our_height + 1;
                    let next_id =
                        NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    info!(our_height, next_from, "requesting more sync blocks");
                    return vec![NetworkMessage::BlockRangeRequest {
                        request_id: next_id,
                        from_number: next_from,
                        count: SYNC_BATCH_SIZE,
                    }];
                }
            }
            vec![]
        }

        // T1: Respond to single block requests
        NetworkMessage::BlockRequest { block_hash } => {
            let chain_r = match chain.read() {
                Ok(c) => c,
                Err(_) => return vec![],
            };
            let block = chain_r.get_block(&block_hash).cloned();
            let request_id = NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            vec![NetworkMessage::BlockResponse { request_id, block }]
        }

        // T1: Import a single block from a response
        NetworkMessage::BlockResponse { block, .. } => {
            if let Some(block) = block {
                let block_number = block.header.number;
                let mut chain = match chain.write() {
                    Ok(c) => c,
                    Err(_) => {
                        warn!("chain lock poisoned, cannot import block response");
                        return vec![];
                    }
                };
                match chain.import_block(block) {
                    Ok(result) => {
                        info!(number = result.block_number, "imported requested block");
                    }
                    Err(e) => {
                        debug!(%e, block_number, "failed to import requested block");
                    }
                }
            }
            vec![]
        }

        // T2: Handle finality votes from peers (with signature verification)
        NetworkMessage::FinalityVote {
            voter,
            block_hash,
            block_number,
            signature,
        } => {
            // Look up voter's public key from the current validator set
            let voter_pubkey = match chain.read() {
                Ok(c) => c.validator_set().get(&voter).and_then(|v| v.public_key),
                Err(_) => None,
            };
            let Some(pubkey) = voter_pubkey else {
                debug!(?voter, "finality vote from unknown/non-validator, dropping");
                return vec![];
            };
            let signable = borsh::to_vec(&(voter, block_hash, block_number))
                .expect("finality vote serialization cannot fail");
            if vtt_crypto::verify(&signable, &signature, &pubkey).is_err() {
                warn!(
                    ?voter,
                    block_number, "invalid finality vote signature, dropping"
                );
                return vec![];
            }
            let vote = FinalityVote {
                voter,
                block_hash,
                block_number,
            };
            let became_final = finality_tracker.submit_vote(vote);
            if became_final {
                info!(block_number, "block finalized via peer votes");
                if let Ok(mut chain_w) = chain.write() {
                    chain_w.set_finalized_block(block_number);
                }
            }
            vec![]
        }
    }
}

/// Try to produce a block if it's our turn.
/// Returns (NetworkMessage, block_hash, block_number) on success.
fn try_produce_block(
    chain: &Arc<RwLock<Chain>>,
    txpool: &Arc<RwLock<TxPool>>,
    keypair: &Keypair,
    gas_config: &GasConfig,
    rpc_state: &Arc<vtt_rpc::RpcState>,
) -> Option<(NetworkMessage, H256, u64)> {
    let mut chain = match chain.write() {
        Ok(c) => c,
        Err(_) => {
            warn!("chain lock poisoned, cannot produce block");
            return None;
        }
    };
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
    let pool = match txpool.read() {
        Ok(p) => p,
        Err(_) => {
            warn!("txpool lock poisoned, producing empty block");
            // Continue with empty tx set rather than failing to produce
            drop(chain);
            return None;
        }
    };
    let mut nonces = std::collections::HashMap::new();
    for sender in pool.senders() {
        nonces.insert(sender, chain.state().get_nonce(&sender));
    }
    let txs = pool.select_transactions(100, &nonces);
    drop(pool);

    // Compute block timestamp before execution so contracts see the real time
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64;

    // Execute transactions with actual block number and timestamp.
    // process_unbonding is called deterministically inside execute_block_transactions_at.
    let (receipts, gas_used) = execute_block_transactions_at(
        chain.state_mut(),
        &txs,
        gas_config,
        10_000_000,
        next_number,
        now_ms,
        head.chain_id,
    );

    // --- M4: Auto-finalize governance proposals whose voting period has ended ---
    let total_staked = chain.validator_set().total_stake();
    let gov_finalized = finalize_governance_proposals(chain.state_mut(), next_number, total_staked);
    if gov_finalized > 0 {
        info!(count = gov_finalized, "governance proposals auto-finalized");
    }

    // --- Execute queued governance proposals whose timelock has expired ---
    let gov_executed = execute_queued_proposals(chain.state_mut(), next_number);
    if gov_executed > 0 {
        info!(
            count = gov_executed,
            "queued governance proposals executed after timelock"
        );
    }

    // --- Block rewards & gas fee distribution ---
    let treasury_addr = chain.consensus().params().treasury_address;

    // 1. Block reward: inflation-based, split 80% producer / 20% treasury
    let total_staked = validator_set.total_stake();
    // Circulating supply = initial genesis supply (1B) + total minted - total burned
    let initial_supply = Amount::from_vtt(1_000_000_000);
    let minted = Amount::from_raw(
        rpc_state
            .total_minted_milli
            .load(std::sync::atomic::Ordering::Relaxed) as u128
            * 10u128.pow(15),
    );
    let burned = Amount::from_raw(
        rpc_state
            .total_burned_milli
            .load(std::sync::atomic::Ordering::Relaxed) as u128
            * 10u128.pow(15),
    );
    let total_supply = Amount::from_raw(
        initial_supply
            .raw()
            .saturating_add(minted.raw())
            .saturating_sub(burned.raw()),
    );
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
        // TODO: When delegator reward distribution is implemented, use
        // split_producer_reward(split.producer, validator.commission_bps) to split
        // the producer share into validator commission and delegator rewards.
        // Currently the full producer share goes to the validator address.
        let _ = chain
            .state_mut()
            .add_balance(&validator_addr, split.producer);
        let _ = chain
            .state_mut()
            .add_balance(&treasury_addr, split.treasury);
        // Track in milli-VTT (raw / 10^15)
        let minted_milli = (per_block_reward.raw() / 10u128.pow(15)) as u64;
        rpc_state
            .total_minted_milli
            .fetch_add(minted_milli, std::sync::atomic::Ordering::Relaxed);
    }

    // 2. Gas fees: 70% burned, 30% to producer
    let total_gas_fees = Amount::from_raw(gas_used as u128 * gas_config.min_gas_price.raw());
    if total_gas_fees.raw() > 0 {
        let gas_split = split_gas_fees(total_gas_fees);
        // burned portion is simply not credited to anyone (effectively removed)
        let _ = chain
            .state_mut()
            .add_balance(&validator_addr, gas_split.producer);
        let burned_milli = (gas_split.burned.raw() / 10u128.pow(15)) as u64;
        rpc_state
            .total_burned_milli
            .fetch_add(burned_milli, std::sync::atomic::Ordering::Relaxed);
    }

    let state_root = chain.state_mut().compute_state_root();

    let tx_hashes: Vec<H256> = txs
        .iter()
        .map(|tx| blake3_hash(&tx.payload_bytes()))
        .collect();
    let tx_root = merkle_root(&tx_hashes);

    let receipt_hashes: Vec<H256> = receipts
        .iter()
        .map(|r| blake3_hash(&borsh::to_vec(r).expect("receipt serialization cannot fail")))
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
    let produced_number = block.header.number;
    match chain.import_block(block) {
        Ok(result) => {
            // Remove mined transactions from the pool by committed nonce
            let mut pool = match txpool.write() {
                Ok(p) => p,
                Err(_) => {
                    warn!("txpool lock poisoned, cannot prune mined transactions");
                    return Some((broadcast_msg, block_hash, produced_number));
                }
            };
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

            Some((broadcast_msg, block_hash, produced_number))
        }
        Err(e) => {
            warn!(%e, "failed to import produced block");
            None
        }
    }
}
