use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use vtt_crypto::{blake3_hash, merkle_root, Keypair};
use vtt_primitives::amount::Amount;
use vtt_primitives::block::{Block, BlockHeader};
use vtt_primitives::chain::{ChainConfig, ConsensusParams, GasConfig};
use vtt_primitives::{Address, ChainId, Signature, Timestamp, H256};
use vtt_state::account::AccountState;
use vtt_state::asset::{AssetClass, AssetRecord, AssetStatus};
use vtt_state::StateDB;

/// Configuration for the genesis block and initial chain state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisConfig {
    /// Chain configuration.
    pub chain: ChainConfig,
    /// Timestamp for the genesis block (ms since Unix epoch).
    pub timestamp: Timestamp,
    /// Initial account allocations.
    pub allocations: Vec<GenesisAllocation>,
    /// Initial validators (address, self-stake, commission_bps).
    pub validators: Vec<GenesisValidator>,
}

/// An initial account allocation in the genesis block.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisAllocation {
    pub address: Address,
    pub balance: Amount,
}

/// An initial validator in the genesis block.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenesisValidator {
    pub address: Address,
    pub self_stake: Amount,
    pub commission_bps: u16,
}

impl GenesisConfig {
    /// Create a genesis config for a multi-validator testnet (3 validators).
    ///
    /// Seeds (hex):
    ///   Validator 1: `[0xA1; 32]` — primary bootnode
    ///   Validator 2: `[0xA2; 32]`
    ///   Validator 3: `[0xA3; 32]`
    ///
    /// Each validator gets 600,000 VTT (500,000 staked + 100,000 for txs).
    /// Treasury is set to validator 1's address.
    pub fn testnet_default() -> Self {
        let val1_kp = Keypair::from_seed(&[0xA1; 32]);
        let val2_kp = Keypair::from_seed(&[0xA2; 32]);
        let val3_kp = Keypair::from_seed(&[0xA3; 32]);
        let val1 = val1_kp.address();
        let val2 = val2_kp.address();
        let val3 = val3_kp.address();

        // Faucet / test accounts
        let faucet1 = Keypair::from_seed(&[0x01; 32]).address();
        let faucet2 = Keypair::from_seed(&[0x02; 32]).address();

        Self {
            chain: ChainConfig {
                chain_id: ChainId::RELAY,
                name: "VTT Testnet".to_string(),
                consensus: ConsensusParams {
                    active_validators: 3,
                    epoch_length: 1200, // 1 hour at 3s blocks
                    treasury_address: val1,
                    ..Default::default()
                },
                gas: GasConfig::default(),
            },
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            allocations: vec![
                GenesisAllocation {
                    address: val1,
                    balance: Amount::from_vtt(600_000),
                },
                GenesisAllocation {
                    address: val2,
                    balance: Amount::from_vtt(600_000),
                },
                GenesisAllocation {
                    address: val3,
                    balance: Amount::from_vtt(600_000),
                },
                GenesisAllocation {
                    address: faucet1,
                    balance: Amount::from_vtt(1_000_000),
                },
                GenesisAllocation {
                    address: faucet2,
                    balance: Amount::from_vtt(1_000_000),
                },
            ],
            validators: vec![
                GenesisValidator {
                    address: val1,
                    self_stake: Amount::from_vtt(500_000),
                    commission_bps: 500,
                },
                GenesisValidator {
                    address: val2,
                    self_stake: Amount::from_vtt(500_000),
                    commission_bps: 500,
                },
                GenesisValidator {
                    address: val3,
                    self_stake: Amount::from_vtt(500_000),
                    commission_bps: 500,
                },
            ],
        }
    }

    /// Create a default genesis config for a development/test network.
    pub fn dev_default() -> Self {
        // Derive addresses from seeds (matching vtt-validator default seed)
        let dev_addr_1 = Keypair::from_seed(&[0x01; 32]).address();
        let dev_addr_2 = Keypair::from_seed(&[0x02; 32]).address();
        let dev_addr_3 = Keypair::from_seed(&[0x03; 32]).address();
        let validator_addr = Keypair::from_seed(&[0x10; 32]).address();

        Self {
            chain: ChainConfig {
                chain_id: ChainId::RELAY,
                name: "VTT Dev Network".to_string(),
                consensus: ConsensusParams {
                    active_validators: 1,
                    epoch_length: 100,
                    treasury_address: validator_addr,
                    ..Default::default()
                },
                gas: GasConfig::default(),
            },
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            allocations: vec![
                GenesisAllocation {
                    address: dev_addr_1,
                    balance: Amount::from_vtt(1_000_000),
                },
                GenesisAllocation {
                    address: dev_addr_2,
                    balance: Amount::from_vtt(1_000_000),
                },
                GenesisAllocation {
                    address: dev_addr_3,
                    balance: Amount::from_vtt(1_000_000),
                },
                GenesisAllocation {
                    address: validator_addr,
                    balance: Amount::from_vtt(500_000),
                },
            ],
            validators: vec![GenesisValidator {
                address: validator_addr,
                self_stake: Amount::from_vtt(100_000),
                commission_bps: 500,
            }],
        }
    }
}

/// Validate a genesis configuration before building the genesis block.
///
/// Returns `Ok(())` if the configuration is valid, or an error string describing
/// the first validation failure found.
pub fn validate_genesis(config: &GenesisConfig) -> Result<(), String> {
    // Must have at least one validator
    if config.validators.is_empty() {
        return Err("Genesis must have at least 1 validator".into());
    }

    // chain_id of 0 is valid (relay chain), so we do not reject it.
    // However, the consensus epoch_length must be non-zero.
    if config.chain.consensus.epoch_length == 0 {
        return Err("epoch_length cannot be 0".into());
    }

    // Check total supply is non-zero (allocations + staked)
    let total_allocated: u128 = config.allocations.iter().map(|a| a.balance.raw()).sum();
    let total_staked: u128 = config.validators.iter().map(|v| v.self_stake.raw()).sum();
    if total_allocated == 0 && total_staked == 0 {
        return Err("Genesis has no tokens allocated".into());
    }

    // Check min self-stake for each validator
    let min_stake = config.chain.consensus.min_self_stake;
    for validator in &config.validators {
        if validator.self_stake < min_stake {
            return Err(format!(
                "Validator {} stake {} below minimum {}",
                validator.address, validator.self_stake, min_stake
            ));
        }
    }

    Ok(())
}

/// The result of building a genesis block.
pub struct GenesisResult {
    /// The genesis block (block 0).
    pub block: Block,
    /// The initial state database with all genesis accounts loaded.
    pub state: StateDB,
    /// The state root hash.
    pub state_root: H256,
}

/// Build the genesis block and initial state from a genesis configuration.
///
/// Panics if `validate_genesis` finds configuration errors (call `validate_genesis`
/// first if you want to handle errors gracefully).
pub fn build_genesis(config: &GenesisConfig) -> GenesisResult {
    if let Err(e) = validate_genesis(config) {
        panic!("Invalid genesis configuration: {e}");
    }

    let mut state = StateDB::new();

    // 1. Apply initial allocations
    for alloc in &config.allocations {
        state.put_account(alloc.address, AccountState::with_balance(alloc.balance));
    }

    // 2. Set up initial validators
    for validator in &config.validators {
        let mut account = state.get_account(&validator.address);

        // Deduct self-stake from balance
        account.balance = account
            .balance
            .checked_sub(validator.self_stake)
            .expect("validator balance must cover self-stake");

        account.staking = Some(vtt_state::account::StakingState {
            total_stake: validator.self_stake,
            self_stake: validator.self_stake,
            commission_bps: validator.commission_bps,
            active: true,
            delegations: Vec::new(),
            unbonding: Vec::new(),
        });

        state.put_account(validator.address, account);
    }

    // 3. Store chain parameters in state for use by the executor
    state.set_treasury_address(config.chain.consensus.treasury_address);
    state.set_epoch_length(config.chain.consensus.epoch_length);

    // 4. Create DEX genesis assets (vUSDT and VTT-REV) minted to validator (acts as treasury in dev)
    let treasury = config
        .validators
        .first()
        .map(|v| v.address)
        .or_else(|| config.allocations.first().map(|a| a.address))
        .unwrap_or(Address::ZERO);
    setup_dex_genesis(&mut state, treasury, config.chain.chain_id);

    // 5. Compute state root
    let state_root = state.compute_state_root();

    // 6. Build genesis block header
    let header = BlockHeader {
        version: 1,
        chain_id: config.chain.chain_id,
        number: 0,
        parent_hash: H256::ZERO,
        transactions_root: merkle_root(&[]),
        state_root,
        receipts_root: merkle_root(&[]),
        validator: config
            .validators
            .first()
            .map(|v| v.address)
            .unwrap_or(Address::ZERO),
        epoch: 0,
        slot: 0,
        timestamp: config.timestamp,
        gas_limit: 10_000_000,
        gas_used: 0,
        cross_chain_root: None,
        signature: Signature::ZERO, // Genesis block has no signature
    };

    let block = Block::new(header, vec![]);

    GenesisResult {
        block,
        state,
        state_root,
    }
}

/// Create the DEX genesis assets (vUSDT and VTT-REV) and mint them to the treasury address.
///
/// - **vUSDT**: stablecoin proxy, 6 decimals, 100M supply, minted to treasury.
/// - **VTT-REV**: revenue share token, 0 decimals, 10,000 supply, minted to treasury.
pub fn setup_dex_genesis(state: &mut StateDB, treasury: Address, chain_id: ChainId) {
    // vUSDT — asset id derived from blake3_hash(b"asset:vUSDT")
    let vusdt_id = blake3_hash(b"asset:vUSDT");
    let vusdt_supply = Amount::from_raw(100_000_000 * 10u128.pow(6)); // 100M with 6 decimals
    let vusdt = AssetRecord {
        id: vusdt_id,
        name: "VTT USD".to_string(),
        symbol: "vUSDT".to_string(),
        class: AssetClass::Custom("Stablecoin".to_string()),
        origin_chain: chain_id,
        issuer: treasury,
        total_supply: vusdt_supply,
        decimals: 6,
        status: AssetStatus::Active,
        compliance_policy: None,
        valuation_oracle: None,
        documents: BTreeMap::new(),
        metadata_uri: String::new(),
        jurisdiction: String::new(),
        legal_entity: String::new(),
        transfer_mode: vtt_state::asset::TransferMode::PeerToPeer,
        registrar: None,
        redemption_pool: Amount::ZERO,
        requires_kyc: false,
        created_at: 0,
    };
    state
        .register_asset(vusdt)
        .expect("vUSDT registration failed");
    let mut vusdt_ownership = state.get_ownership(&vusdt_id, &treasury);
    vusdt_ownership.credit(vusdt_supply);
    state.put_ownership(vusdt_ownership);

    // Create VTT/vUSDT liquidity pool at launchpad price (~$0.108/VTT)
    // 100,000 VTT paired with 10,800 vUSDT
    let pool_vtt_amount = Amount::from_vtt(100_000); // 100K VTT (18 decimals)
    let pool_usdt_amount = Amount::from_raw(10_800_000_000); // 10,800 vUSDT (6 decimals)

    // Create the pool (token_a = native VTT = H256::ZERO, token_b = vUSDT)
    // create_pool handles debiting both tokens from the treasury
    let pool = vtt_dex::liquidity::create_pool(
        state,
        &treasury,
        H256::ZERO,
        vusdt_id,
        pool_vtt_amount,
        pool_usdt_amount,
        0, // epoch 0
    )
    .expect("pool creation failed");
    vtt_dex::liquidity::save_pool(state, &pool).expect("pool save failed");

    // VTT-REV — asset id derived from blake3_hash(b"asset:VTT-REV")
    let vttrev_id = blake3_hash(b"asset:VTT-REV");
    let vttrev_supply = Amount::from_raw(10_000); // 10,000 tokens, 0 decimals
    let vttrev = AssetRecord {
        id: vttrev_id,
        name: "VTT Revenue Share".to_string(),
        symbol: "VTT-REV".to_string(),
        class: AssetClass::Custom("RevenueShare".to_string()),
        origin_chain: chain_id,
        issuer: treasury,
        total_supply: vttrev_supply,
        decimals: 0,
        status: AssetStatus::Active,
        compliance_policy: None,
        valuation_oracle: None,
        documents: BTreeMap::new(),
        metadata_uri: String::new(),
        jurisdiction: String::new(),
        legal_entity: String::new(),
        transfer_mode: vtt_state::asset::TransferMode::PeerToPeer,
        registrar: None,
        redemption_pool: Amount::ZERO,
        requires_kyc: false,
        created_at: 0,
    };
    state
        .register_asset(vttrev)
        .expect("VTT-REV registration failed");
    let mut vttrev_ownership = state.get_ownership(&vttrev_id, &treasury);
    vttrev_ownership.credit(vttrev_supply);
    state.put_ownership(vttrev_ownership);
}

/// Compute the hash of a genesis block.
pub fn genesis_hash(block: &Block) -> H256 {
    blake3_hash(&block.header.signable_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_dev_genesis() {
        let config = GenesisConfig::dev_default();
        let result = build_genesis(&config);

        assert_eq!(result.block.header.number, 0);
        assert_eq!(result.block.header.parent_hash, H256::ZERO);
        assert!(result.block.is_empty()); // no transactions in genesis
        assert_ne!(result.state_root, H256::ZERO);
        assert_eq!(result.block.header.state_root, result.state_root);
    }

    #[test]
    fn genesis_allocations_applied() {
        let config = GenesisConfig::dev_default();
        let result = build_genesis(&config);

        let addr1 = Keypair::from_seed(&[0x01; 32]).address();
        assert_eq!(
            result.state.get_balance(&addr1),
            Amount::from_vtt(1_000_000)
        );
    }

    #[test]
    fn genesis_validator_staked() {
        let config = GenesisConfig::dev_default();
        let result = build_genesis(&config);

        let val_addr = Keypair::from_seed(&[0x10; 32]).address();
        let val_account = result.state.get_account(&val_addr);

        // Balance should be 500k - 100k stake - 100k pool = 300k
        assert_eq!(val_account.balance, Amount::from_vtt(300_000));

        let staking = val_account.staking.unwrap();
        assert_eq!(staking.self_stake, Amount::from_vtt(100_000));
        assert_eq!(staking.total_stake, Amount::from_vtt(100_000));
        assert!(staking.active);
        assert_eq!(staking.commission_bps, 500);
    }

    #[test]
    fn genesis_deterministic() {
        let config = GenesisConfig::dev_default();
        let result1 = build_genesis(&config);
        let result2 = build_genesis(&config);

        assert_eq!(result1.state_root, result2.state_root);
        assert_eq!(genesis_hash(&result1.block), genesis_hash(&result2.block));
    }

    #[test]
    fn genesis_hash_not_zero() {
        let config = GenesisConfig::dev_default();
        let result = build_genesis(&config);
        let hash = genesis_hash(&result.block);
        assert_ne!(hash, H256::ZERO);
    }

    #[test]
    fn genesis_config_json_roundtrip() {
        let config = GenesisConfig::dev_default();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let config2: GenesisConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.chain.chain_id, config2.chain.chain_id);
        assert_eq!(config.allocations.len(), config2.allocations.len());
        assert_eq!(config.validators.len(), config2.validators.len());
    }

    #[test]
    fn genesis_with_custom_config() {
        let validator_addr = Address::from([0xAA; 20]);
        let config = GenesisConfig {
            chain: ChainConfig {
                chain_id: ChainId::new(1),
                name: "Real Estate Chain".to_string(),
                consensus: ConsensusParams::default(),
                gas: GasConfig::default(),
            },
            timestamp: 1_700_000_000_000,
            allocations: vec![GenesisAllocation {
                address: validator_addr,
                balance: Amount::from_vtt(10_000_000),
            }],
            validators: vec![GenesisValidator {
                address: validator_addr,
                self_stake: Amount::from_vtt(100_000),
                commission_bps: 500,
            }],
        };

        let result = build_genesis(&config);
        assert_eq!(result.block.header.chain_id, ChainId::new(1));
        assert_ne!(result.state_root, H256::ZERO);

        // Balance = 10M allocation - 100K stake - 100K DEX pool
        let balance = result.state.get_balance(&validator_addr);
        assert_eq!(balance, Amount::from_vtt(9_800_000));
    }

    #[test]
    fn build_testnet_genesis() {
        let config = GenesisConfig::testnet_default();
        let result = build_genesis(&config);

        assert_eq!(result.block.header.number, 0);
        assert_ne!(result.state_root, H256::ZERO);
        assert_eq!(config.validators.len(), 3);
        assert_eq!(config.chain.consensus.active_validators, 3);
        assert_eq!(config.chain.consensus.epoch_length, 1200);

        // Verify all 3 validators are staked
        for validator in &config.validators {
            let val_account = result.state.get_account(&validator.address);
            let staking = val_account.staking.unwrap();
            assert_eq!(staking.self_stake, Amount::from_vtt(500_000));
            assert!(staking.active);
        }
    }

    #[test]
    fn testnet_genesis_json_roundtrip() {
        let config = GenesisConfig::testnet_default();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let config2: GenesisConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.chain.chain_id, config2.chain.chain_id);
        assert_eq!(config.validators.len(), config2.validators.len());
        assert_eq!(config2.validators.len(), 3);
        assert_eq!(config2.chain.consensus.active_validators, 3);
    }

    #[test]
    fn generate_testnet_genesis_file() {
        // This test generates the genesis-testnet.json content for reference.
        // Use a fixed timestamp so the file is deterministic.
        let mut config = GenesisConfig::testnet_default();
        config.timestamp = 1_711_929_600_000; // 2024-04-01T00:00:00Z

        let json = serde_json::to_string_pretty(&config).unwrap();
        // Verify it parses back correctly
        let parsed: GenesisConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.validators.len(), 3);
        assert_eq!(parsed.chain.name, "VTT Testnet");

        // Build it to verify it's valid
        let result = build_genesis(&parsed);
        assert_eq!(result.block.header.number, 0);
    }

    // --- Genesis validation tests ---

    #[test]
    fn validate_genesis_ok() {
        let config = GenesisConfig::dev_default();
        assert!(validate_genesis(&config).is_ok());
    }

    #[test]
    fn validate_genesis_empty_validators() {
        let config = GenesisConfig {
            chain: ChainConfig {
                chain_id: ChainId::new(1),
                name: "Test".to_string(),
                consensus: ConsensusParams::default(),
                gas: GasConfig::default(),
            },
            timestamp: 1_000_000,
            allocations: vec![GenesisAllocation {
                address: Address::from([0x01; 20]),
                balance: Amount::from_vtt(1_000),
            }],
            validators: vec![],
        };
        let err = validate_genesis(&config).unwrap_err();
        assert!(err.contains("at least 1 validator"), "got: {err}");
    }

    #[test]
    fn validate_genesis_zero_epoch_length() {
        let addr = Address::from([0x01; 20]);
        let config = GenesisConfig {
            chain: ChainConfig {
                chain_id: ChainId::new(1),
                name: "Test".to_string(),
                consensus: ConsensusParams {
                    epoch_length: 0,
                    ..Default::default()
                },
                gas: GasConfig::default(),
            },
            timestamp: 1_000_000,
            allocations: vec![GenesisAllocation {
                address: addr,
                balance: Amount::from_vtt(200_000),
            }],
            validators: vec![GenesisValidator {
                address: addr,
                self_stake: Amount::from_vtt(100_000),
                commission_bps: 500,
            }],
        };
        let err = validate_genesis(&config).unwrap_err();
        assert!(err.contains("epoch_length"), "got: {err}");
    }

    #[test]
    fn validate_genesis_zero_supply() {
        let addr = Address::from([0x01; 20]);
        let config = GenesisConfig {
            chain: ChainConfig {
                chain_id: ChainId::new(1),
                name: "Test".to_string(),
                consensus: ConsensusParams {
                    min_self_stake: Amount::ZERO,
                    ..Default::default()
                },
                gas: GasConfig::default(),
            },
            timestamp: 1_000_000,
            allocations: vec![],
            validators: vec![GenesisValidator {
                address: addr,
                self_stake: Amount::ZERO,
                commission_bps: 500,
            }],
        };
        let err = validate_genesis(&config).unwrap_err();
        assert!(err.contains("no tokens allocated"), "got: {err}");
    }

    #[test]
    fn validate_genesis_insufficient_stake() {
        let addr = Address::from([0x01; 20]);
        let config = GenesisConfig {
            chain: ChainConfig {
                chain_id: ChainId::new(1),
                name: "Test".to_string(),
                consensus: ConsensusParams {
                    min_self_stake: Amount::from_vtt(100_000),
                    ..Default::default()
                },
                gas: GasConfig::default(),
            },
            timestamp: 1_000_000,
            allocations: vec![GenesisAllocation {
                address: addr,
                balance: Amount::from_vtt(200_000),
            }],
            validators: vec![GenesisValidator {
                address: addr,
                self_stake: Amount::from_vtt(50_000), // below min_self_stake of 100k
                commission_bps: 500,
            }],
        };
        let err = validate_genesis(&config).unwrap_err();
        assert!(err.contains("below minimum"), "got: {err}");
    }

    #[test]
    fn validate_genesis_testnet_ok() {
        let config = GenesisConfig::testnet_default();
        assert!(validate_genesis(&config).is_ok());
    }
}
