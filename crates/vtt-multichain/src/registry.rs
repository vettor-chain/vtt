use std::collections::HashMap;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;

use vtt_compliance::ChainComplianceConfig;
use vtt_primitives::chain::{ConsensusParams, GasConfig};
use vtt_primitives::{Address, ChainId, Epoch, H256};

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("chain already registered: {0}")]
    AlreadyRegistered(ChainId),
    #[error("chain not found: {0}")]
    NotFound(ChainId),
    #[error("cannot modify relay chain")]
    CannotModifyRelay,
    #[error("invalid chain config: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, RegistryError>;

/// A registered application chain in the VTT multichain network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct RegisteredChain {
    /// Chain identifier.
    pub chain_id: ChainId,
    /// Human-readable name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Number of validators assigned to this chain (subset of relay validators).
    pub validator_count: u32,
    /// Consensus parameters for this chain.
    pub consensus: ConsensusParams,
    /// Gas configuration.
    pub gas: GasConfig,
    /// Compliance configuration.
    pub compliance: ChainComplianceConfig,
    /// Genesis hash of this chain.
    pub genesis_hash: H256,
    /// Whether the chain is active.
    pub active: bool,
    /// Epoch when the chain was registered.
    pub registered_at: Epoch,
    /// Address that proposed the chain registration.
    pub proposer: Address,
}

/// Registry of all chains in the VTT multichain network.
/// Managed by the relay chain.
pub struct ChainRegistry {
    /// All registered chains indexed by ChainId.
    chains: HashMap<ChainId, RegisteredChain>,
    /// Next available chain ID.
    next_chain_id: u32,
}

/// Request to register a new application chain.
pub struct RegisterChainRequest {
    pub name: String,
    pub description: String,
    pub validator_count: u32,
    pub consensus: ConsensusParams,
    pub gas: GasConfig,
    pub compliance: ChainComplianceConfig,
    pub proposer: Address,
    pub epoch: Epoch,
}

impl ChainRegistry {
    /// Create a new registry with the relay chain pre-registered.
    pub fn new() -> Self {
        let mut registry = Self {
            chains: HashMap::new(),
            next_chain_id: 1, // 0 is reserved for relay
        };

        // Register relay chain
        let relay = RegisteredChain {
            chain_id: ChainId::RELAY,
            name: "VTT Relay Chain".to_string(),
            description: "The root chain for validator registry and cross-chain messaging"
                .to_string(),
            validator_count: 21,
            consensus: ConsensusParams::default(),
            gas: GasConfig::default(),
            compliance: ChainComplianceConfig::permissionless(),
            genesis_hash: H256::ZERO,
            active: true,
            registered_at: 0,
            proposer: Address::ZERO,
        };
        registry.chains.insert(ChainId::RELAY, relay);

        registry
    }

    /// Register a new application chain. Returns the assigned ChainId.
    pub fn register_chain(&mut self, req: RegisterChainRequest) -> Result<ChainId> {
        if req.validator_count == 0 {
            return Err(RegistryError::InvalidConfig(
                "validator_count must be > 0".to_string(),
            ));
        }

        let chain_id = ChainId::new(self.next_chain_id);
        self.next_chain_id += 1;

        let chain = RegisteredChain {
            chain_id,
            name: req.name.clone(),
            description: req.description,
            validator_count: req.validator_count,
            consensus: req.consensus,
            gas: req.gas,
            compliance: req.compliance,
            genesis_hash: H256::ZERO,
            active: true,
            registered_at: req.epoch,
            proposer: req.proposer,
        };

        info!(%chain_id, name = %req.name, "registered new application chain");
        self.chains.insert(chain_id, chain);

        Ok(chain_id)
    }

    /// Get a registered chain by ID.
    pub fn get(&self, chain_id: &ChainId) -> Option<&RegisteredChain> {
        self.chains.get(chain_id)
    }

    /// Deactivate a chain (governance action).
    pub fn deactivate(&mut self, chain_id: &ChainId) -> Result<()> {
        if chain_id.is_relay() {
            return Err(RegistryError::CannotModifyRelay);
        }
        let chain = self
            .chains
            .get_mut(chain_id)
            .ok_or(RegistryError::NotFound(*chain_id))?;
        chain.active = false;
        Ok(())
    }

    /// List all active chains.
    pub fn active_chains(&self) -> Vec<&RegisteredChain> {
        self.chains.values().filter(|c| c.active).collect()
    }

    /// List all chains (including inactive).
    pub fn all_chains(&self) -> Vec<&RegisteredChain> {
        self.chains.values().collect()
    }

    /// Number of registered chains (including relay).
    pub fn chain_count(&self) -> usize {
        self.chains.len()
    }

    /// Number of active app chains (excluding relay).
    pub fn active_app_chain_count(&self) -> usize {
        self.chains
            .values()
            .filter(|c| c.active && !c.chain_id.is_relay())
            .count()
    }
}

impl Default for ChainRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(name: &str, validators: u32, compliance: ChainComplianceConfig) -> RegisterChainRequest {
        RegisterChainRequest {
            name: name.to_string(),
            description: format!("{name} chain"),
            validator_count: validators,
            consensus: ConsensusParams {
                active_validators: validators,
                ..Default::default()
            },
            gas: GasConfig::default(),
            compliance,
            proposer: Address::from([0x01; 20]),
            epoch: 0,
        }
    }

    #[test]
    fn new_registry_has_relay() {
        let registry = ChainRegistry::new();
        assert_eq!(registry.chain_count(), 1);
        assert!(registry.get(&ChainId::RELAY).is_some());

        let relay = registry.get(&ChainId::RELAY).unwrap();
        assert!(relay.active);
        assert!(relay.chain_id.is_relay());
    }

    #[test]
    fn register_app_chain() {
        let mut registry = ChainRegistry::new();

        let chain_id = registry
            .register_chain(req(
                "Real Estate EU",
                11,
                ChainComplianceConfig::permissioned(vec![H256::from([0xAA; 32])]),
            ))
            .unwrap();

        assert_eq!(chain_id, ChainId::new(1));
        assert_eq!(registry.chain_count(), 2);
        assert_eq!(registry.active_app_chain_count(), 1);

        let chain = registry.get(&chain_id).unwrap();
        assert_eq!(chain.name, "Real Estate EU");
        assert!(chain.compliance.requires_identity);
    }

    #[test]
    fn register_multiple_chains() {
        let mut registry = ChainRegistry::new();

        let id1 = registry
            .register_chain(req(
                "Commodities",
                7,
                ChainComplianceConfig::permissionless(),
            ))
            .unwrap();

        let id2 = registry
            .register_chain(req(
                "Equity",
                11,
                ChainComplianceConfig::permissioned(vec![]),
            ))
            .unwrap();

        assert_eq!(id1, ChainId::new(1));
        assert_eq!(id2, ChainId::new(2));
        assert_eq!(registry.chain_count(), 3);
    }

    #[test]
    fn deactivate_chain() {
        let mut registry = ChainRegistry::new();

        let chain_id = registry
            .register_chain(req("Test", 5, ChainComplianceConfig::permissionless()))
            .unwrap();

        assert_eq!(registry.active_app_chain_count(), 1);

        registry.deactivate(&chain_id).unwrap();
        assert_eq!(registry.active_app_chain_count(), 0);
        assert!(!registry.get(&chain_id).unwrap().active);
    }

    #[test]
    fn cannot_deactivate_relay() {
        let mut registry = ChainRegistry::new();
        let result = registry.deactivate(&ChainId::RELAY);
        assert!(matches!(result, Err(RegistryError::CannotModifyRelay)));
    }

    #[test]
    fn invalid_validator_count() {
        let mut registry = ChainRegistry::new();
        let result =
            registry.register_chain(req("Bad", 0, ChainComplianceConfig::permissionless()));
        assert!(matches!(result, Err(RegistryError::InvalidConfig(_))));
    }

    #[test]
    fn registered_chain_borsh_roundtrip() {
        let chain = RegisteredChain {
            chain_id: ChainId::new(1),
            name: "Test Chain".to_string(),
            description: "A test".to_string(),
            validator_count: 11,
            consensus: ConsensusParams::default(),
            gas: GasConfig::default(),
            compliance: ChainComplianceConfig::permissionless(),
            genesis_hash: H256::from([0x01; 32]),
            active: true,
            registered_at: 5,
            proposer: Address::from([0x01; 20]),
        };
        let bytes = borsh::to_vec(&chain).unwrap();
        let chain2 = RegisteredChain::try_from_slice(&bytes).unwrap();
        assert_eq!(chain, chain2);
    }
}
