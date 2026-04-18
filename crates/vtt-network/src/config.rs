use libp2p::Multiaddr;
use serde::{Deserialize, Serialize};

use vtt_primitives::ChainId;

/// Network configuration for a VTT node.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Address to listen on (e.g., "/ip4/0.0.0.0/tcp/30333").
    pub listen_address: String,
    /// Bootstrap nodes for initial peer discovery.
    pub boot_nodes: Vec<String>,
    /// Maximum number of peers.
    pub max_peers: usize,
    /// Maximum number of connections allowed from a single IP address.
    pub max_peers_per_ip: u32,
    /// Duration in seconds for which a misbehaving peer is banned.
    pub ban_duration_secs: u64,
    /// Chain ID this node is operating on.
    pub chain_id: ChainId,
    /// Optional 32-byte seed used to derive a deterministic libp2p
    /// identity (and therefore a stable `peer_id` across restarts). When
    /// `None`, a fresh random identity is generated on every start.
    /// Operators that want a fixed bootnode multiaddr must set this.
    #[serde(default)]
    pub node_key_seed: Option<[u8; 32]>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            listen_address: "/ip4/0.0.0.0/tcp/30333".to_string(),
            boot_nodes: Vec::new(),
            max_peers: 50,
            max_peers_per_ip: 3,
            ban_duration_secs: 3600,
            chain_id: ChainId::RELAY,
            node_key_seed: None,
        }
    }
}

impl NetworkConfig {
    /// Parse listen address into a Multiaddr.
    pub fn listen_multiaddr(&self) -> Result<Multiaddr, libp2p::multiaddr::Error> {
        self.listen_address.parse()
    }

    /// Create a config for development (localhost, no boot nodes).
    pub fn dev(port: u16) -> Self {
        Self {
            listen_address: format!("/ip4/127.0.0.1/tcp/{port}"),
            boot_nodes: Vec::new(),
            max_peers: 10,
            max_peers_per_ip: 3,
            ban_duration_secs: 3600,
            chain_id: ChainId::RELAY,
            node_key_seed: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = NetworkConfig::default();
        assert_eq!(config.max_peers, 50);
        assert_eq!(config.max_peers_per_ip, 3);
        assert_eq!(config.ban_duration_secs, 3600);
        assert!(config.boot_nodes.is_empty());
    }

    #[test]
    fn dev_config() {
        let config = NetworkConfig::dev(30333);
        assert!(config.listen_address.contains("30333"));
        assert_eq!(config.max_peers, 10);
        assert_eq!(config.max_peers_per_ip, 3);
        assert_eq!(config.ban_duration_secs, 3600);
    }

    #[test]
    fn parse_listen_address() {
        let config = NetworkConfig::default();
        let addr = config.listen_multiaddr().unwrap();
        assert!(!addr.to_string().is_empty());
    }
}
