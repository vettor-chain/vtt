use std::env;
use std::time::Duration;

use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use vtt_chain::Chain;
use vtt_consensus::ConsensusEngine;
use vtt_genesis::{build_genesis, genesis_hash, GenesisConfig};
use vtt_network::{NetworkConfig, NetworkEvent, NetworkService};
use vtt_txpool::{TxPool, TxPoolConfig};

#[tokio::main]
async fn main() {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("VTT Node v{}", env!("CARGO_PKG_VERSION"));

    // Parse simple CLI args
    let args: Vec<String> = env::args().collect();
    let dev_mode = args.iter().any(|a| a == "--dev");
    let port: u16 = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(30333);

    // Genesis configuration
    let genesis_config = if dev_mode {
        info!("running in dev mode");
        GenesisConfig::dev_default()
    } else {
        GenesisConfig::dev_default() // TODO: load from file
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

    // Initialize chain
    let consensus = ConsensusEngine::new(genesis_config.chain.consensus.clone());
    let mut chain = Chain::new(consensus, genesis_config.chain.gas.clone());
    chain
        .init_genesis(genesis_result.block, genesis_result.state)
        .expect("failed to initialize genesis");

    info!(height = chain.height().unwrap_or(0), "chain initialized");

    // Initialize transaction pool
    let _txpool = TxPool::new(TxPoolConfig::default());

    // Initialize network
    let net_config = NetworkConfig::dev(port);
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
        chain_id = %genesis_config.chain.chain_id,
        "node started"
    );

    // Connect to boot nodes
    for bootnode in &net_config.boot_nodes {
        if let Err(e) = network.dial_bootnode(bootnode) {
            error!(%e, bootnode, "failed to dial boot node");
        }
    }

    // Main event loop
    info!("entering main event loop (Ctrl+C to stop)");
    loop {
        tokio::select! {
            event = network.next_event() => {
                match event {
                    NetworkEvent::Listening { address } => {
                        info!(%address, "listening on");
                    }
                    NetworkEvent::PeerConnected { peer_id } => {
                        info!(%peer_id, peers = network.connected_peers(), "peer connected");
                    }
                    NetworkEvent::PeerDisconnected { peer_id } => {
                        info!(%peer_id, peers = network.connected_peers(), "peer disconnected");
                    }
                    NetworkEvent::Message(msg) => {
                        info!(?msg, "received network message");
                        let _ = msg; // consume
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                break;
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                info!(
                    height = chain.height().unwrap_or(0),
                    peers = network.connected_peers(),
                    "status"
                );
            }
        }
    }
}
