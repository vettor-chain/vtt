use thiserror::Error;
use tracing::debug;

use vtt_crypto::{blake3_hash, verify};
use vtt_primitives::block::BlockHeader;
use vtt_primitives::chain::ConsensusParams;
use vtt_primitives::{Address, BlockNumber, Epoch};
use vtt_state::StateDB;

use crate::slashing::DoubleSignEvidence;
use crate::validator::{ValidatorInfo, ValidatorSet};

#[derive(Debug, Error)]
pub enum ConsensusError {
    #[error("wrong block producer: expected {expected}, got {got}")]
    WrongBlockProducer { expected: Address, got: Address },
    #[error("invalid block signature")]
    InvalidSignature,
    #[error("block timestamp {got} not after parent timestamp {parent}")]
    InvalidTimestamp { got: u64, parent: u64 },
    #[error("wrong parent hash: expected {expected}, got {got}")]
    WrongParentHash {
        expected: vtt_primitives::H256,
        got: vtt_primitives::H256,
    },
    #[error("wrong block number: expected {expected}, got {got}")]
    WrongBlockNumber {
        expected: BlockNumber,
        got: BlockNumber,
    },
    #[error("empty validator set")]
    EmptyValidatorSet,
    #[error("validator not in active set: {0}")]
    ValidatorNotInSet(Address),
    #[error("invalid double-sign evidence")]
    InvalidEvidence,
}

pub type Result<T> = std::result::Result<T, ConsensusError>;

/// The DPoS consensus engine.
pub struct ConsensusEngine {
    params: ConsensusParams,
}

impl ConsensusEngine {
    pub fn new(params: ConsensusParams) -> Self {
        Self { params }
    }

    /// Get the consensus parameters.
    pub fn params(&self) -> &ConsensusParams {
        &self.params
    }

    /// Calculate which epoch a block number belongs to.
    pub fn epoch_for_block(&self, block_number: BlockNumber) -> Epoch {
        if block_number == 0 {
            return 0;
        }
        block_number / self.params.epoch_length
    }

    /// Calculate the first block number of an epoch.
    pub fn epoch_start_block(&self, epoch: Epoch) -> BlockNumber {
        epoch * self.params.epoch_length
    }

    /// Calculate the slot index within an epoch for a given block number.
    pub fn slot_for_block(&self, block_number: BlockNumber) -> u32 {
        (block_number % self.params.epoch_length) as u32
    }

    /// Elect the active validator set for a given epoch from the state.
    ///
    /// Selects the top `active_validators` by total_stake.
    /// Ties are broken by address (lower address wins) for determinism.
    pub fn elect_validators(&self, state: &StateDB, epoch: Epoch) -> ValidatorSet {
        let mut candidates: Vec<ValidatorInfo> = state
            .iter_accounts()
            .filter_map(|(addr, account)| {
                let staking = account.staking.as_ref()?;
                if staking.self_stake < self.params.min_self_stake {
                    return None;
                }
                Some(ValidatorInfo {
                    address: *addr,
                    public_key: None,
                    total_stake: staking.total_stake,
                    self_stake: staking.self_stake,
                    commission_bps: staking.commission_bps,
                })
            })
            .collect();

        // Sort by total_stake descending, then address ascending for determinism
        candidates.sort_by(|a, b| {
            b.total_stake
                .cmp(&a.total_stake)
                .then_with(|| a.address.cmp(&b.address))
        });

        // Take top N
        candidates.truncate(self.params.active_validators as usize);

        debug!(
            epoch,
            validator_count = candidates.len(),
            "elected validator set"
        );

        ValidatorSet {
            epoch,
            validators: candidates,
        }
    }

    /// Determine which validator should produce a block at a given block number.
    pub fn block_producer<'a>(
        &self,
        validator_set: &'a ValidatorSet,
        block_number: BlockNumber,
    ) -> Result<&'a ValidatorInfo> {
        let slot = self.slot_for_block(block_number);
        validator_set
            .slot_leader(slot)
            .ok_or(ConsensusError::EmptyValidatorSet)
    }

    /// Verify a block header against the expected consensus rules.
    pub fn verify_header(
        &self,
        header: &BlockHeader,
        parent: &BlockHeader,
        validator_set: &ValidatorSet,
    ) -> Result<()> {
        // 1. Check block number is sequential
        if header.number != parent.number + 1 {
            return Err(ConsensusError::WrongBlockNumber {
                expected: parent.number + 1,
                got: header.number,
            });
        }

        // 2. Check parent hash
        let expected_parent_hash = blake3_hash(&parent.signable_bytes());
        if header.parent_hash != expected_parent_hash {
            return Err(ConsensusError::WrongParentHash {
                expected: expected_parent_hash,
                got: header.parent_hash,
            });
        }

        // 3. Check timestamp is after parent
        if header.timestamp <= parent.timestamp {
            return Err(ConsensusError::InvalidTimestamp {
                got: header.timestamp,
                parent: parent.timestamp,
            });
        }

        // 4. Check the block producer is correct for this slot
        let expected_producer = self.block_producer(validator_set, header.number)?;
        if header.validator != expected_producer.address {
            return Err(ConsensusError::WrongBlockProducer {
                expected: expected_producer.address,
                got: header.validator,
            });
        }

        // 5. Verify signature (if validator has a public key registered)
        if let Some(ref pubkey) = expected_producer.public_key {
            let signable = header.signable_bytes();
            verify(&signable, &header.signature, pubkey)
                .map_err(|_| ConsensusError::InvalidSignature)?;
        }

        Ok(())
    }

    /// Verify double-sign evidence and return the offender address if valid.
    pub fn verify_double_sign(
        &self,
        evidence: &DoubleSignEvidence,
        validator_set: &ValidatorSet,
    ) -> Result<Address> {
        // Check evidence is well-formed
        if !evidence.is_valid() {
            return Err(ConsensusError::InvalidEvidence);
        }

        let offender = evidence.offender();

        // Check offender is in the validator set
        if !validator_set.contains(&offender) {
            return Err(ConsensusError::ValidatorNotInSet(offender));
        }

        // Verify signatures on both headers (if public key available)
        if let Some(validator_info) = validator_set.get(&offender) {
            if let Some(ref pubkey) = validator_info.public_key {
                verify(
                    &evidence.header_a.signable_bytes(),
                    &evidence.header_a.signature,
                    pubkey,
                )
                .map_err(|_| ConsensusError::InvalidSignature)?;
                verify(
                    &evidence.header_b.signable_bytes(),
                    &evidence.header_b.signature,
                    pubkey,
                )
                .map_err(|_| ConsensusError::InvalidSignature)?;
            }
        }

        Ok(offender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtt_primitives::amount::Amount;
    use vtt_primitives::{ChainId, Signature, H256};
    use vtt_state::account::{AccountState, StakingState};

    fn default_engine() -> ConsensusEngine {
        ConsensusEngine::new(ConsensusParams::default())
    }

    fn small_engine() -> ConsensusEngine {
        ConsensusEngine::new(ConsensusParams {
            epoch_length: 10,
            active_validators: 3,
            min_self_stake: Amount::from_vtt(100),
            ..Default::default()
        })
    }

    fn make_header(number: BlockNumber, validator: Address, parent_hash: H256) -> BlockHeader {
        let epoch = number / 10;
        let slot = (number % 10) as u32;
        BlockHeader {
            version: 1,
            chain_id: ChainId::RELAY,
            number,
            parent_hash,
            transactions_root: H256::ZERO,
            state_root: H256::ZERO,
            receipts_root: H256::ZERO,
            validator,
            epoch,
            slot,
            timestamp: 1_700_000_000_000 + number * 3000,
            gas_limit: 10_000_000,
            gas_used: 0,
            cross_chain_root: None,
            signature: Signature::ZERO,
        }
    }

    fn setup_state_with_validators(stakes: &[(u8, u64)]) -> StateDB {
        let mut state = StateDB::new();
        for (addr_byte, stake) in stakes {
            let addr = Address::from([*addr_byte; 20]);
            let mut account = AccountState::with_balance(Amount::from_vtt(*stake * 2));
            account.staking = Some(StakingState {
                total_stake: Amount::from_vtt(*stake),
                self_stake: Amount::from_vtt(*stake),
                commission_bps: 500,
                active: true,
                delegations: Vec::new(),
                unbonding: Vec::new(),
            });
            state.put_account(addr, account);
        }
        state
    }

    #[test]
    fn epoch_for_block_calculation() {
        let engine = default_engine();
        assert_eq!(engine.epoch_for_block(0), 0);
        assert_eq!(engine.epoch_for_block(1), 0);
        assert_eq!(engine.epoch_for_block(1199), 0);
        assert_eq!(engine.epoch_for_block(1200), 1);
        assert_eq!(engine.epoch_for_block(2400), 2);
    }

    #[test]
    fn slot_for_block_calculation() {
        let engine = default_engine();
        assert_eq!(engine.slot_for_block(0), 0);
        assert_eq!(engine.slot_for_block(1), 1);
        assert_eq!(engine.slot_for_block(1200), 0); // new epoch
        assert_eq!(engine.slot_for_block(1201), 1);
    }

    #[test]
    fn elect_validators_selects_top_by_stake() {
        let engine = small_engine(); // top 3 validators
        let state = setup_state_with_validators(&[
            (1, 300_000),
            (2, 500_000),
            (3, 100_000), // below minimum? No, min is 100 VTT in small_engine
            (4, 200_000),
            (5, 400_000),
        ]);

        let vs = engine.elect_validators(&state, 0);
        assert_eq!(vs.len(), 3);

        // Should be sorted by stake descending: 500k, 400k, 300k
        assert_eq!(vs.validators[0].address, Address::from([2; 20])); // 500k
        assert_eq!(vs.validators[1].address, Address::from([5; 20])); // 400k
        assert_eq!(vs.validators[2].address, Address::from([1; 20])); // 300k
    }

    #[test]
    fn elect_validators_excludes_below_min_stake() {
        let engine = ConsensusEngine::new(ConsensusParams {
            epoch_length: 10,
            active_validators: 10,
            min_self_stake: Amount::from_vtt(100_000),
            ..Default::default()
        });

        let state = setup_state_with_validators(&[
            (1, 200_000), // above minimum
            (2, 50_000),  // below minimum
            (3, 150_000), // above minimum
        ]);

        let vs = engine.elect_validators(&state, 0);
        assert_eq!(vs.len(), 2);
        assert!(vs.contains(&Address::from([1; 20])));
        assert!(vs.contains(&Address::from([3; 20])));
        assert!(!vs.contains(&Address::from([2; 20])));
    }

    #[test]
    fn block_producer_round_robin() {
        let engine = small_engine();
        let state = setup_state_with_validators(&[(1, 300_000), (2, 200_000), (3, 100_000)]);

        let vs = engine.elect_validators(&state, 0);

        // Block 0 -> slot 0 -> validator[0]
        let p0 = engine.block_producer(&vs, 0).unwrap();
        // Block 1 -> slot 1 -> validator[1]
        let p1 = engine.block_producer(&vs, 1).unwrap();
        // Block 2 -> slot 2 -> validator[2]
        let p2 = engine.block_producer(&vs, 2).unwrap();
        // Block 3 -> slot 3 -> validator[0] (wraps)
        let p3 = engine.block_producer(&vs, 3).unwrap();

        assert_eq!(p0.address, vs.validators[0].address);
        assert_eq!(p1.address, vs.validators[1].address);
        assert_eq!(p2.address, vs.validators[2].address);
        assert_eq!(p3.address, vs.validators[0].address);
    }

    #[test]
    fn verify_header_valid() {
        let engine = small_engine();
        let state = setup_state_with_validators(&[(1, 300_000), (2, 200_000), (3, 100_000)]);
        let vs = engine.elect_validators(&state, 0);

        let parent = make_header(0, vs.validators[0].address, H256::ZERO);
        let parent_hash = blake3_hash(&parent.signable_bytes());

        let expected_producer = engine.block_producer(&vs, 1).unwrap();
        let child = make_header(1, expected_producer.address, parent_hash);

        assert!(engine.verify_header(&child, &parent, &vs).is_ok());
    }

    #[test]
    fn verify_header_wrong_producer() {
        let engine = small_engine();
        let state = setup_state_with_validators(&[(1, 300_000), (2, 200_000), (3, 100_000)]);
        let vs = engine.elect_validators(&state, 0);

        let parent = make_header(0, vs.validators[0].address, H256::ZERO);
        let parent_hash = blake3_hash(&parent.signable_bytes());

        // Use wrong validator for block 1
        let wrong_producer = vs.validators[2].address; // should be validators[1]
        let child = make_header(1, wrong_producer, parent_hash);

        let result = engine.verify_header(&child, &parent, &vs);
        assert!(matches!(
            result,
            Err(ConsensusError::WrongBlockProducer { .. })
        ));
    }

    #[test]
    fn verify_header_wrong_parent_hash() {
        let engine = small_engine();
        let state = setup_state_with_validators(&[(1, 300_000), (2, 200_000), (3, 100_000)]);
        let vs = engine.elect_validators(&state, 0);

        let parent = make_header(0, vs.validators[0].address, H256::ZERO);

        let expected_producer = engine.block_producer(&vs, 1).unwrap();
        let child = make_header(1, expected_producer.address, H256::from([0xFF; 32])); // wrong parent

        let result = engine.verify_header(&child, &parent, &vs);
        assert!(matches!(
            result,
            Err(ConsensusError::WrongParentHash { .. })
        ));
    }

    #[test]
    fn verify_header_wrong_block_number() {
        let engine = small_engine();
        let state = setup_state_with_validators(&[(1, 300_000), (2, 200_000), (3, 100_000)]);
        let vs = engine.elect_validators(&state, 0);

        let parent = make_header(0, vs.validators[0].address, H256::ZERO);
        let parent_hash = blake3_hash(&parent.signable_bytes());

        // Block number 5 instead of 1
        let child = make_header(5, vs.validators[0].address, parent_hash);

        let result = engine.verify_header(&child, &parent, &vs);
        assert!(matches!(
            result,
            Err(ConsensusError::WrongBlockNumber { .. })
        ));
    }

    #[test]
    fn verify_double_sign_valid() {
        let engine = small_engine();
        let val_addr = Address::from([1; 20]);
        let state = setup_state_with_validators(&[(1, 300_000)]);
        let vs = engine.elect_validators(&state, 0);

        let evidence = DoubleSignEvidence {
            header_a: make_header(0, val_addr, H256::ZERO),
            header_b: make_header(0, val_addr, H256::from([0xFF; 32])), // different parent
        };

        // Adjust headers to have same epoch/slot but different content
        let mut ha = evidence.header_a.clone();
        let mut hb = evidence.header_b.clone();
        ha.epoch = 0;
        ha.slot = 0;
        hb.epoch = 0;
        hb.slot = 0;

        let ev = DoubleSignEvidence {
            header_a: ha,
            header_b: hb,
        };

        let result = engine.verify_double_sign(&ev, &vs);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), val_addr);
    }

    #[test]
    fn verify_double_sign_invalid_evidence() {
        let engine = small_engine();
        let state = setup_state_with_validators(&[(1, 300_000)]);
        let vs = engine.elect_validators(&state, 0);

        // Same block = not a double sign
        let h = make_header(0, Address::from([1; 20]), H256::ZERO);
        let evidence = DoubleSignEvidence {
            header_a: h.clone(),
            header_b: h,
        };

        let result = engine.verify_double_sign(&evidence, &vs);
        assert!(matches!(result, Err(ConsensusError::InvalidEvidence)));
    }

    #[test]
    fn verify_double_sign_not_in_set() {
        let engine = small_engine();
        let state = setup_state_with_validators(&[(1, 300_000)]);
        let vs = engine.elect_validators(&state, 0);

        let non_validator = Address::from([0xFF; 20]);
        let evidence = DoubleSignEvidence {
            header_a: make_header(0, non_validator, H256::ZERO),
            header_b: make_header(0, non_validator, H256::from([0xAA; 32])),
        };

        let result = engine.verify_double_sign(&evidence, &vs);
        assert!(matches!(result, Err(ConsensusError::ValidatorNotInSet(_))));
    }

    #[test]
    fn epoch_start_block_calculation() {
        let engine = default_engine();
        assert_eq!(engine.epoch_start_block(0), 0);
        assert_eq!(engine.epoch_start_block(1), 1200);
        assert_eq!(engine.epoch_start_block(5), 6000);
    }
}
