use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::amount::Amount;
use crate::ChainId;

/// DPoS consensus parameters for a chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct ConsensusParams {
    /// Number of blocks per epoch.
    pub epoch_length: u64,
    /// Target block time in milliseconds.
    pub block_time_ms: u64,
    /// Number of active validators per epoch.
    pub active_validators: u32,
    /// Minimum self-stake to become a validator candidate.
    pub min_self_stake: Amount,
    /// Unbonding period in seconds.
    pub unbonding_period_secs: u64,
    /// Slash percentage for double signing (basis points, e.g., 500 = 5%).
    pub slash_double_sign_bps: u16,
    /// Slash percentage for downtime per epoch (basis points, e.g., 10 = 0.1%).
    pub slash_downtime_bps: u16,
    /// Downtime threshold: missing more than this fraction of slots triggers slash.
    /// Expressed as percentage (e.g., 50 = 50%).
    pub downtime_threshold_pct: u8,
}

impl Default for ConsensusParams {
    fn default() -> Self {
        Self {
            epoch_length: 1200,
            block_time_ms: 3000,
            active_validators: 21,
            min_self_stake: Amount::from_vtt(100_000),
            unbonding_period_secs: 21 * 24 * 3600, // 21 days
            slash_double_sign_bps: 500,            // 5%
            slash_downtime_bps: 10,                // 0.1%
            downtime_threshold_pct: 50,
        }
    }
}

/// Gas configuration for a chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct GasConfig {
    /// Minimum gas price accepted.
    pub min_gas_price: Amount,
    /// Base cost of a simple VTT transfer.
    pub base_transfer_cost: u64,
    /// Cost per byte of transaction data.
    pub cost_per_byte: u64,
}

impl Default for GasConfig {
    fn default() -> Self {
        Self {
            min_gas_price: Amount::from_raw(1_000_000_000), // 1 gwei equivalent
            base_transfer_cost: 21_000,
            cost_per_byte: 16,
        }
    }
}

/// Configuration for a chain in the VTT multichain network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct ChainConfig {
    pub chain_id: ChainId,
    pub name: String,
    pub consensus: ConsensusParams,
    pub gas: GasConfig,
}

impl ChainConfig {
    /// Create a default relay chain configuration.
    pub fn relay_default() -> Self {
        Self {
            chain_id: ChainId::RELAY,
            name: "VTT Relay Chain".to_string(),
            consensus: ConsensusParams::default(),
            gas: GasConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_consensus_params() {
        let p = ConsensusParams::default();
        assert_eq!(p.epoch_length, 1200);
        assert_eq!(p.block_time_ms, 3000);
        assert_eq!(p.active_validators, 21);
        assert_eq!(p.min_self_stake, Amount::from_vtt(100_000));
    }

    #[test]
    fn relay_chain_config() {
        let config = ChainConfig::relay_default();
        assert!(config.chain_id.is_relay());
        assert_eq!(config.name, "VTT Relay Chain");
    }

    #[test]
    fn chain_config_borsh_roundtrip() {
        let config = ChainConfig::relay_default();
        let bytes = borsh::to_vec(&config).unwrap();
        let config2 = ChainConfig::try_from_slice(&bytes).unwrap();
        assert_eq!(config, config2);
    }
}
