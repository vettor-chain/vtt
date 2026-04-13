use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use libp2p::gossipsub::{self, IdentTopic, MessageAuthenticity};
use libp2p::identity::Keypair;
use libp2p::kad;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::NetworkBehaviour;
use libp2p::{PeerId, Swarm, SwarmBuilder};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::config::NetworkConfig;
use crate::messages::{topics, NetworkMessage};

/// Maximum allowed gossipsub message size (4 MB).
const MAX_GOSSIP_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

/// Default initial reputation score for new peers.
const DEFAULT_REPUTATION_SCORE: i32 = 100;

/// Tracks a peer's reputation and ban status.
struct PeerReputation {
    score: i32,
    banned_until: Option<Instant>,
}

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("gossipsub error: {0}")]
    Gossipsub(String),
    #[error("listen error: {0}")]
    Listen(String),
    #[error("publish error: {0}")]
    Publish(String),
}

/// Combined libp2p behaviour for VTT nodes.
#[derive(NetworkBehaviour)]
struct VttBehaviour {
    gossipsub: gossipsub::Behaviour,
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
}

/// The VTT network service manages P2P connections and message passing.
pub struct NetworkService {
    swarm: Swarm<VttBehaviour>,
    config: NetworkConfig,
    local_peer_id: PeerId,
    block_topic: IdentTopic,
    tx_topic: IdentTopic,
    /// Reputation tracking for connected peers.
    reputations: HashMap<PeerId, PeerReputation>,
    /// Tracks the number of connections per IP address.
    connections_per_ip: HashMap<IpAddr, u32>,
    /// Maps peer IDs to their remote IP address (for connection-limit tracking).
    peer_ips: HashMap<PeerId, IpAddr>,
}

/// High-level network events emitted by the service.
#[derive(Clone, Debug)]
pub enum NetworkEvent {
    Listening { address: String },
    PeerConnected { peer_id: String },
    PeerDisconnected { peer_id: String },
    Message(Box<NetworkMessage>),
}

impl NetworkService {
    /// Create a new network service.
    pub fn new(config: &NetworkConfig) -> std::result::Result<Self, NetworkError> {
        let local_key = Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());

        info!(%local_peer_id, "starting VTT network node");

        // Configure GossipSub
        let message_id_fn = |message: &gossipsub::Message| {
            let mut hasher = DefaultHasher::new();
            message.data.hash(&mut hasher);
            message.topic.hash(&mut hasher);
            gossipsub::MessageId::from(hasher.finish().to_string())
        };

        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(1))
            .validation_mode(gossipsub::ValidationMode::Strict)
            .message_id_fn(message_id_fn)
            .build()
            .map_err(|e| NetworkError::Gossipsub(e.to_string()))?;

        let gossipsub = gossipsub::Behaviour::new(
            MessageAuthenticity::Signed(local_key.clone()),
            gossipsub_config,
        )
        .map_err(|e| NetworkError::Gossipsub(e.to_string()))?;

        // Configure Kademlia
        let kademlia =
            kad::Behaviour::new(local_peer_id, kad::store::MemoryStore::new(local_peer_id));

        let behaviour = VttBehaviour {
            gossipsub,
            kademlia,
        };

        // Build the swarm
        let swarm = SwarmBuilder::with_existing_identity(local_key)
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )
            .map_err(|e| NetworkError::Transport(e.to_string()))?
            .with_behaviour(|_| behaviour)
            .map_err(|e| NetworkError::Transport(e.to_string()))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        // Set up topics
        let chain_id = config.chain_id.0;
        let block_topic = IdentTopic::new(topics::block_announce(chain_id));
        let tx_topic = IdentTopic::new(topics::transactions(chain_id));

        let mut service = Self {
            swarm,
            config: config.clone(),
            local_peer_id,
            block_topic,
            tx_topic,
            reputations: HashMap::new(),
            connections_per_ip: HashMap::new(),
            peer_ips: HashMap::new(),
        };

        // Subscribe to topics
        service.subscribe_topics()?;

        Ok(service)
    }

    fn subscribe_topics(&mut self) -> std::result::Result<(), NetworkError> {
        self.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&self.block_topic)
            .map_err(|e| NetworkError::Gossipsub(e.to_string()))?;
        self.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&self.tx_topic)
            .map_err(|e| NetworkError::Gossipsub(e.to_string()))?;

        debug!("subscribed to block and transaction topics");
        Ok(())
    }

    /// Start listening on the configured address.
    pub fn start_listening(
        &mut self,
        config: &NetworkConfig,
    ) -> std::result::Result<(), NetworkError> {
        let addr = config
            .listen_multiaddr()
            .map_err(|e| NetworkError::Listen(e.to_string()))?;
        self.swarm
            .listen_on(addr)
            .map_err(|e| NetworkError::Listen(e.to_string()))?;
        info!(address = %config.listen_address, "listening for connections");
        Ok(())
    }

    /// Connect to a boot node.
    pub fn dial_bootnode(&mut self, addr_str: &str) -> std::result::Result<(), NetworkError> {
        let addr: libp2p::Multiaddr = addr_str
            .parse()
            .map_err(|e: libp2p::multiaddr::Error| NetworkError::Transport(e.to_string()))?;
        self.swarm
            .dial(addr)
            .map_err(|e| NetworkError::Transport(e.to_string()))?;
        debug!(bootnode = addr_str, "dialing boot node");
        Ok(())
    }

    /// Broadcast a block announcement to peers.
    pub fn broadcast_block(
        &mut self,
        message: &NetworkMessage,
    ) -> std::result::Result<(), NetworkError> {
        let data = message.encode();
        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(self.block_topic.clone(), data)
            .map_err(|e| NetworkError::Publish(e.to_string()))?;
        Ok(())
    }

    /// Broadcast a transaction to peers.
    pub fn broadcast_transaction(
        &mut self,
        message: &NetworkMessage,
    ) -> std::result::Result<(), NetworkError> {
        let data = message.encode();
        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(self.tx_topic.clone(), data)
            .map_err(|e| NetworkError::Publish(e.to_string()))?;
        Ok(())
    }

    /// Get the local peer ID.
    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    // -- Reputation and banning --------------------------------------------------

    /// Returns true if the given peer is currently banned.
    pub fn is_banned(&self, peer_id: &PeerId) -> bool {
        self.reputations
            .get(peer_id)
            .map(|r| r.banned_until.map_or(false, |t| Instant::now() < t))
            .unwrap_or(false)
    }

    /// Apply a reputation penalty to a peer. If the score drops to zero or
    /// below, the peer is banned for `ban_duration_secs` and disconnected.
    pub fn penalize(&mut self, peer_id: &PeerId, penalty: i32) {
        let ban_duration = self.config.ban_duration_secs;
        let rep = self.reputations.entry(*peer_id).or_insert(PeerReputation {
            score: DEFAULT_REPUTATION_SCORE,
            banned_until: None,
        });
        rep.score = rep.score.saturating_sub(penalty).max(0);
        if rep.score == 0 {
            rep.banned_until = Some(Instant::now() + Duration::from_secs(ban_duration));
            warn!(%peer_id, "peer banned for {ban_duration}s (score dropped to {})", rep.score);
            let _ = self.swarm.disconnect_peer_id(*peer_id);
        }
    }

    /// Remove expired ban entries from the reputation table.
    pub fn cleanup_reputations(&mut self) {
        let now = Instant::now();
        self.reputations.retain(|_, r| {
            // Keep entries that are still banned OR still have a positive score
            r.banned_until.map_or(true, |t| now < t) || r.score > 0
        });
    }

    /// Get the current reputation score for a peer (or None if unknown).
    pub fn peer_reputation(&self, peer_id: &PeerId) -> Option<i32> {
        self.reputations.get(peer_id).map(|r| r.score)
    }

    // -- IP-based connection limits ----------------------------------------------

    /// Extract an IP address from a libp2p `Multiaddr`, if present.
    fn ip_from_multiaddr(addr: &libp2p::Multiaddr) -> Option<IpAddr> {
        for proto in addr.iter() {
            match proto {
                Protocol::Ip4(ip) => return Some(IpAddr::V4(ip)),
                Protocol::Ip6(ip) => return Some(IpAddr::V6(ip)),
                _ => {}
            }
        }
        None
    }

    /// Record a new connection for the given peer and IP. Returns `true` if
    /// the connection should be accepted, `false` if the per-IP limit is
    /// exceeded.
    fn track_connection(&mut self, peer_id: PeerId, ip: IpAddr) -> bool {
        let count = self.connections_per_ip.entry(ip).or_insert(0);
        if *count >= self.config.max_peers_per_ip {
            return false;
        }
        *count += 1;
        self.peer_ips.insert(peer_id, ip);
        true
    }

    /// Remove tracking for a disconnected peer.
    fn untrack_connection(&mut self, peer_id: &PeerId) {
        if let Some(ip) = self.peer_ips.remove(peer_id) {
            if let Some(count) = self.connections_per_ip.get_mut(&ip) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.connections_per_ip.remove(&ip);
                }
            }
        }
    }

    // -- Event loop --------------------------------------------------------------

    /// Poll the swarm for the next event. Returns a high-level network event.
    pub async fn next_event(&mut self) -> NetworkEvent {
        use libp2p::futures::StreamExt;
        loop {
            match self.swarm.select_next_some().await {
                libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } => {
                    return NetworkEvent::Listening {
                        address: address.to_string(),
                    };
                }
                libp2p::swarm::SwarmEvent::ConnectionEstablished {
                    peer_id, endpoint, ..
                } => {
                    // Enforce global max-peers limit
                    if self.swarm.connected_peers().count() > self.config.max_peers {
                        warn!(
                            %peer_id,
                            "max peers limit ({}) exceeded, disconnecting",
                            self.config.max_peers,
                        );
                        let _ = self.swarm.disconnect_peer_id(peer_id);
                        continue;
                    }

                    // Enforce per-IP connection limit
                    if let Some(ip) = Self::ip_from_multiaddr(endpoint.get_remote_address()) {
                        if !self.track_connection(peer_id, ip) {
                            warn!(
                                %peer_id,
                                %ip,
                                "per-IP connection limit ({}) exceeded, disconnecting",
                                self.config.max_peers_per_ip,
                            );
                            let _ = self.swarm.disconnect_peer_id(peer_id);
                            continue;
                        }
                    }

                    // Reject banned peers
                    if self.is_banned(&peer_id) {
                        debug!(%peer_id, "banned peer attempted to connect, disconnecting");
                        let _ = self.swarm.disconnect_peer_id(peer_id);
                        continue;
                    }

                    return NetworkEvent::PeerConnected {
                        peer_id: peer_id.to_string(),
                    };
                }
                libp2p::swarm::SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    self.untrack_connection(&peer_id);
                    return NetworkEvent::PeerDisconnected {
                        peer_id: peer_id.to_string(),
                    };
                }
                libp2p::swarm::SwarmEvent::Behaviour(VttBehaviourEvent::Gossipsub(
                    gossipsub::Event::Message {
                        propagation_source,
                        message,
                        ..
                    },
                )) => {
                    // Drop messages from banned peers
                    if self.is_banned(&propagation_source) {
                        debug!(
                            %propagation_source,
                            "dropping message from banned peer",
                        );
                        continue;
                    }

                    // Enforce message size limit
                    if message.data.len() > MAX_GOSSIP_MESSAGE_SIZE {
                        warn!(
                            %propagation_source,
                            size = message.data.len(),
                            "oversized gossip message, penalizing peer",
                        );
                        self.penalize(&propagation_source, 50);
                        continue;
                    }

                    if let Ok(msg) = NetworkMessage::decode(&message.data) {
                        return NetworkEvent::Message(Box::new(msg));
                    }
                }
                _ => {}
            }
        }
    }

    /// Get the number of connected peers.
    pub fn connected_peers(&self) -> usize {
        self.swarm.connected_peers().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn create_network_service() {
        let config = NetworkConfig::dev(0);
        let service = NetworkService::new(&config).unwrap();
        assert!(!service.local_peer_id().to_string().is_empty());
        assert_eq!(service.connected_peers(), 0);
    }

    #[test]
    fn peer_not_banned_by_default() {
        let config = NetworkConfig::dev(0);
        let service = NetworkService::new(&config).unwrap();
        let peer = PeerId::random();
        assert!(!service.is_banned(&peer));
    }

    #[test]
    fn penalize_reduces_score() {
        let config = NetworkConfig::dev(0);
        let mut service = NetworkService::new(&config).unwrap();
        let peer = PeerId::random();

        service.penalize(&peer, 30);
        assert_eq!(service.peer_reputation(&peer), Some(70));
        assert!(!service.is_banned(&peer));
    }

    #[test]
    fn penalize_to_zero_bans_peer() {
        let config = NetworkConfig::dev(0);
        let mut service = NetworkService::new(&config).unwrap();
        let peer = PeerId::random();

        // First penalty brings score to 0
        service.penalize(&peer, 100);
        assert!(service.is_banned(&peer));
        assert_eq!(service.peer_reputation(&peer), Some(0));
    }

    #[test]
    fn multiple_penalties_accumulate() {
        let config = NetworkConfig::dev(0);
        let mut service = NetworkService::new(&config).unwrap();
        let peer = PeerId::random();

        service.penalize(&peer, 40);
        assert_eq!(service.peer_reputation(&peer), Some(60));
        assert!(!service.is_banned(&peer));

        service.penalize(&peer, 40);
        assert_eq!(service.peer_reputation(&peer), Some(20));
        assert!(!service.is_banned(&peer));

        service.penalize(&peer, 40);
        assert_eq!(service.peer_reputation(&peer), Some(0));
        assert!(service.is_banned(&peer));
    }

    #[test]
    fn score_does_not_underflow() {
        let config = NetworkConfig::dev(0);
        let mut service = NetworkService::new(&config).unwrap();
        let peer = PeerId::random();

        service.penalize(&peer, 200);
        // saturating_sub prevents underflow
        assert_eq!(service.peer_reputation(&peer), Some(0));
        assert!(service.is_banned(&peer));
    }

    #[test]
    fn cleanup_removes_expired_bans() {
        let config = NetworkConfig::dev(0);
        let mut service = NetworkService::new(&config).unwrap();
        let peer = PeerId::random();

        // Insert a reputation entry that has already expired and has score <= 0
        service.reputations.insert(
            peer,
            PeerReputation {
                score: 0,
                banned_until: Some(Instant::now() - Duration::from_secs(1)),
            },
        );

        assert!(!service.is_banned(&peer)); // ban expired
        service.cleanup_reputations();
        // Entry should be removed: score is 0 and ban expired
        assert!(service.peer_reputation(&peer).is_none());
    }

    #[test]
    fn cleanup_keeps_active_bans() {
        let config = NetworkConfig::dev(0);
        let mut service = NetworkService::new(&config).unwrap();
        let peer = PeerId::random();

        service.reputations.insert(
            peer,
            PeerReputation {
                score: 0,
                banned_until: Some(Instant::now() + Duration::from_secs(3600)),
            },
        );

        service.cleanup_reputations();
        // Entry should still be present because the ban is active
        assert!(service.peer_reputation(&peer).is_some());
        assert!(service.is_banned(&peer));
    }

    #[test]
    fn ip_from_multiaddr_ipv4() {
        let addr: libp2p::Multiaddr = "/ip4/192.168.1.1/tcp/30333".parse().unwrap();
        let ip = NetworkService::ip_from_multiaddr(&addr);
        assert_eq!(ip, Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn ip_from_multiaddr_ipv6() {
        let addr: libp2p::Multiaddr = "/ip6/::1/tcp/30333".parse().unwrap();
        let ip = NetworkService::ip_from_multiaddr(&addr);
        assert_eq!(ip, Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn track_connection_enforces_per_ip_limit() {
        let config = NetworkConfig::dev(0);
        let mut service = NetworkService::new(&config).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

        // max_peers_per_ip defaults to 3
        for _ in 0..3 {
            let peer = PeerId::random();
            assert!(service.track_connection(peer, ip));
        }

        // Fourth connection from the same IP should be rejected
        let peer4 = PeerId::random();
        assert!(!service.track_connection(peer4, ip));
    }

    #[test]
    fn untrack_connection_frees_slot() {
        let config = NetworkConfig::dev(0);
        let mut service = NetworkService::new(&config).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        let peer1 = PeerId::random();
        let peer2 = PeerId::random();
        let peer3 = PeerId::random();

        assert!(service.track_connection(peer1, ip));
        assert!(service.track_connection(peer2, ip));
        assert!(service.track_connection(peer3, ip));

        // Limit reached
        assert!(!service.track_connection(PeerId::random(), ip));

        // Disconnect one peer
        service.untrack_connection(&peer2);

        // Should now have a free slot
        let peer4 = PeerId::random();
        assert!(service.track_connection(peer4, ip));
    }

    #[test]
    fn untrack_removes_ip_entry_when_count_zero() {
        let config = NetworkConfig::dev(0);
        let mut service = NetworkService::new(&config).unwrap();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3));

        let peer = PeerId::random();
        service.track_connection(peer, ip);
        assert!(service.connections_per_ip.contains_key(&ip));

        service.untrack_connection(&peer);
        assert!(!service.connections_per_ip.contains_key(&ip));
    }

    #[test]
    fn max_gossip_message_size_is_4mb() {
        assert_eq!(MAX_GOSSIP_MESSAGE_SIZE, 4 * 1024 * 1024);
    }

    #[test]
    fn default_reputation_score_is_100() {
        assert_eq!(DEFAULT_REPUTATION_SCORE, 100);
    }

    #[test]
    fn config_has_new_fields() {
        let config = NetworkConfig::default();
        assert_eq!(config.max_peers_per_ip, 3);
        assert_eq!(config.ban_duration_secs, 3600);
    }
}
