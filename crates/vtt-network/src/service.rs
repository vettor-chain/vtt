use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use libp2p::gossipsub::{self, IdentTopic, MessageAuthenticity};
use libp2p::identity::Keypair;
use libp2p::kad;
use libp2p::swarm::NetworkBehaviour;
use libp2p::{PeerId, Swarm, SwarmBuilder};
use thiserror::Error;
use tracing::{debug, info};

use crate::config::NetworkConfig;
use crate::messages::{topics, NetworkMessage};

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
    local_peer_id: PeerId,
    block_topic: IdentTopic,
    tx_topic: IdentTopic,
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
            local_peer_id,
            block_topic,
            tx_topic,
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
                libp2p::swarm::SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    return NetworkEvent::PeerConnected {
                        peer_id: peer_id.to_string(),
                    };
                }
                libp2p::swarm::SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    return NetworkEvent::PeerDisconnected {
                        peer_id: peer_id.to_string(),
                    };
                }
                libp2p::swarm::SwarmEvent::Behaviour(VttBehaviourEvent::Gossipsub(
                    gossipsub::Event::Message { message, .. },
                )) => {
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

    #[test]
    fn create_network_service() {
        let config = NetworkConfig::dev(0);
        let service = NetworkService::new(&config).unwrap();
        assert!(!service.local_peer_id().to_string().is_empty());
        assert_eq!(service.connected_peers(), 0);
    }
}
