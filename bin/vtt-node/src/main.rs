use std::env;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use vtt_chain::Chain;
use vtt_consensus::ConsensusEngine;
use vtt_genesis::{build_genesis, genesis_hash, GenesisConfig};
use vtt_network::messages::NetworkMessage;
use vtt_network::{NetworkConfig, NetworkEvent, NetworkService};
use vtt_primitives::H256;
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
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("VTT Node v{}", env!("CARGO_PKG_VERSION"));

    // Parse CLI args
    let args: Vec<String> = env::args().collect();
    let dev_mode = args.iter().any(|a| a == "--dev");
    let port: u16 = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(30333);

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

    // --testnet : use multi-validator testnet genesis (T5)
    let is_testnet = args.iter().any(|a| a == "--testnet");

    // Genesis configuration
    let genesis_config = if is_testnet {
        info!("using testnet genesis config (3 validators)");
        GenesisConfig::testnet_default()
    } else if dev_mode {
        info!("running in dev mode");
        GenesisConfig::dev_default()
    } else if let Some(ref path) = genesis_path {
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
    } else {
        info!("no --genesis or --dev flag, using dev default");
        GenesisConfig::dev_default()
    };

    // Build genesis
    let genesis_result = build_genesis(&genesis_config);
    let gen_hash = genesis_hash(&genesis_result.block);
    info!(
        ?gen_hash,
        state_root = ?genesis_result.state_root,
        validators = genesis_config.validators.len(),
        accounts = genesis_config.allocations.len(),
        "genesis block built"
    );

    // Initialize chain (with optional persistent storage)
    let consensus = ConsensusEngine::new(genesis_config.chain.consensus.clone());
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
            genesis_config.chain.gas.clone(),
            store.clone() as Arc<dyn vtt_storage::KeyValueStore>,
        )
    } else {
        Chain::new(consensus, genesis_config.chain.gas.clone())
    };
    // Only init genesis when the chain is actually empty — a warm restart
    // has already rehydrated the chain from RocksDB via with_storage.
    if chain.head_hash().is_none() {
        chain
            .init_genesis(genesis_result.block, genesis_result.state)
            .expect("failed to initialize genesis");
    }

    info!(height = chain.height().unwrap_or(0), "chain initialized");

    let chain = Arc::new(RwLock::new(chain));
    let txpool = Arc::new(RwLock::new(TxPool::new(TxPoolConfig::default())));

    // Telemetry
    let metrics = Arc::new(NodeMetrics::new());
    {
        let chain_r = chain.read().expect("chain lock poisoned during init");
        if let Some(height) = chain_r.height() {
            metrics.block_height.set(height as i64);
        }
        metrics
            .active_validators
            .set(chain_r.validator_set().validators.len() as i64);
    }

    // Initialize network
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

    if let Err(e) = network.start_listening(&net_config) {
        error!(%e, "failed to start listening");
        return;
    }

    info!(
        peer_id = %network.local_peer_id(),
        port,
        metrics_port,
        chain_id = %chain_id,
        bootnodes = bootnodes.len(),
        data_dir = data_dir.as_deref().unwrap_or("(in-memory)"),
        "node started"
    );

    // Connect to boot nodes
    for bootnode in &bootnodes {
        if let Err(e) = network.dial_bootnode(bootnode) {
            warn!(%e, bootnode, "failed to dial boot node");
        }
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

    // Main event loop
    info!("entering main event loop (Ctrl+C to stop)");
    let mut txpool_eviction_interval = tokio::time::interval(Duration::from_secs(60));
    txpool_eviction_interval.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            event = network.next_event() => {
                match event {
                    NetworkEvent::Listening { address } => {
                        info!(%address, "listening on");
                    }
                    NetworkEvent::PeerConnected { peer_id } => {
                        info!(%peer_id, peers = network.connected_peers(), "peer connected");
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
                        info!(%peer_id, peers = network.connected_peers(), "peer disconnected");
                        metrics.connected_peers.set(network.connected_peers() as i64);
                    }
                    NetworkEvent::Message(msg) => {
                        let responses = handle_network_message(*msg, &chain, &txpool, &metrics);
                        for resp in responses {
                            let _ = network.broadcast_block(&resp);
                        }

                        // Periodic RocksDB pruning
                        if let Some(ref store) = rocks_store {
                            if let Ok(chain_r) = chain.read() {
                                if let Some(height) = chain_r.height() {
                                    if height > 0 && height % PRUNE_INTERVAL_BLOCKS == 0 {
                                        match store.prune_old_blocks(KEEP_RECENT_BLOCKS, height) {
                                            Ok(pruned) if pruned > 0 => {
                                                info!(height, pruned, "pruned old block data");
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
            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                let chain_r = chain.read().expect("chain lock poisoned");
                let pool_r = txpool.read().expect("txpool lock poisoned");
                info!(
                    height = chain_r.height().unwrap_or(0),
                    peers = network.connected_peers(),
                    txpool = pool_r.len(),
                    "status"
                );
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
    metrics: &Arc<NodeMetrics>,
) -> Vec<NetworkMessage> {
    match msg {
        NetworkMessage::BlockAnnounce {
            block_hash,
            block_number,
            block,
        } => {
            debug!(?block_hash, block_number, "received block from peer");

            let start = std::time::Instant::now();
            let mut chain = match chain.write() {
                Ok(c) => c,
                Err(_) => {
                    warn!("chain lock poisoned, cannot import block");
                    return vec![];
                }
            };
            match chain.import_block(block) {
                Ok(result) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    metrics.block_import_duration.observe(elapsed);
                    metrics.blocks_imported.inc();
                    metrics.block_height.set(result.block_number as i64);
                    metrics
                        .transactions_executed
                        .inc_by(result.receipts.len() as u64);

                    if let Some(height) = chain.height() {
                        let epoch = chain.consensus().epoch_for_block(height);
                        metrics.current_epoch.set(epoch as i64);
                    }

                    info!(
                        number = result.block_number,
                        ?block_hash,
                        txs = result.receipts.len(),
                        elapsed_ms = (elapsed * 1000.0) as u64,
                        "imported block"
                    );
                }
                Err(e) => {
                    warn!(%e, block_number, "failed to import block");
                }
            }
            vec![]
        }
        NetworkMessage::TransactionBroadcast { transaction } => {
            let sender = vtt_crypto::address_from_public_key(&transaction.public_key);
            let account_nonce = match chain.read() {
                Ok(chain) => chain.state().get_nonce(&sender),
                Err(_) => return vec![],
            };
            let mut pool = match txpool.write() {
                Ok(p) => p,
                Err(_) => return vec![],
            };
            match pool.add(transaction, sender, account_nonce) {
                Ok(_) => {
                    metrics.txpool_size.set(pool.len() as i64);
                    debug!(%sender, pool_size = pool.len(), "added peer transaction to pool");
                }
                Err(e) => {
                    debug!(%e, %sender, "rejected peer transaction");
                }
            }
            vec![]
        }

        // T1: Block sync protocol — Status exchange
        NetworkMessage::Status {
            best_block_number, ..
        } => {
            let chain_r = match chain.read() {
                Ok(c) => c,
                Err(_) => return vec![],
            };
            let our_height = chain_r.height().unwrap_or(0);

            if best_block_number > our_height {
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
            for block in &blocks {
                match chain.import_block(block.clone()) {
                    Ok(result) => {
                        imported += 1;
                        metrics.blocks_imported.inc();
                        metrics.block_height.set(result.block_number as i64);
                    }
                    Err(e) => {
                        debug!(%e, number = block.header.number, "failed to import sync block");
                        break;
                    }
                }
            }

            if imported > 0 && blocks.len() as u32 == SYNC_BATCH_SIZE {
                let our_height = chain.height().unwrap_or(0);
                let next_from = our_height + 1;
                let next_id = NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return vec![NetworkMessage::BlockRangeRequest {
                    request_id: next_id,
                    from_number: next_from,
                    count: SYNC_BATCH_SIZE,
                }];
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
                        metrics.blocks_imported.inc();
                        metrics.block_height.set(result.block_number as i64);
                    }
                    Err(e) => {
                        debug!(%e, block_number, "failed to import requested block");
                    }
                }
            }
            vec![]
        }

        // T2: Finality votes — node tracks but doesn't produce them
        NetworkMessage::FinalityVote { block_number, .. } => {
            debug!(block_number, "received finality vote (node does not track)");
            vec![]
        }
    }
}
