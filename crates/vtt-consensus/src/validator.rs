use serde::{Deserialize, Serialize};

use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, Epoch, PublicKey};

/// Information about a single validator in the active set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorInfo {
    pub address: Address,
    pub public_key: Option<PublicKey>,
    pub total_stake: Amount,
    pub self_stake: Amount,
    pub commission_bps: u16,
}

/// The active validator set for a given epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorSet {
    /// The epoch this validator set is active for.
    pub epoch: Epoch,
    /// Validators sorted by total_stake descending, then by address for determinism.
    pub validators: Vec<ValidatorInfo>,
}

impl ValidatorSet {
    /// Create a new empty validator set.
    pub fn empty(epoch: Epoch) -> Self {
        Self {
            epoch,
            validators: Vec::new(),
        }
    }

    /// Number of validators in the set.
    pub fn len(&self) -> usize {
        self.validators.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    /// Get the validator assigned to a given slot (round-robin).
    pub fn slot_leader(&self, slot: u32) -> Option<&ValidatorInfo> {
        if self.validators.is_empty() {
            return None;
        }
        let index = slot as usize % self.validators.len();
        Some(&self.validators[index])
    }

    /// Check if an address is in the active validator set.
    pub fn contains(&self, address: &Address) -> bool {
        self.validators.iter().any(|v| v.address == *address)
    }

    /// Get validator info by address.
    pub fn get(&self, address: &Address) -> Option<&ValidatorInfo> {
        self.validators.iter().find(|v| v.address == *address)
    }

    /// Total stake across all validators.
    pub fn total_stake(&self) -> Amount {
        self.validators
            .iter()
            .fold(Amount::ZERO, |acc, v| acc + v.total_stake)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_validator(addr_byte: u8, stake: u64) -> ValidatorInfo {
        ValidatorInfo {
            address: Address::from([addr_byte; 20]),
            public_key: None,
            total_stake: Amount::from_vtt(stake),
            self_stake: Amount::from_vtt(stake),
            commission_bps: 500,
        }
    }

    #[test]
    fn empty_validator_set() {
        let vs = ValidatorSet::empty(0);
        assert!(vs.is_empty());
        assert_eq!(vs.len(), 0);
        assert!(vs.slot_leader(0).is_none());
    }

    #[test]
    fn slot_leader_round_robin() {
        let vs = ValidatorSet {
            epoch: 0,
            validators: vec![
                make_validator(1, 300_000),
                make_validator(2, 200_000),
                make_validator(3, 100_000),
            ],
        };

        assert_eq!(vs.slot_leader(0).unwrap().address, Address::from([1; 20]));
        assert_eq!(vs.slot_leader(1).unwrap().address, Address::from([2; 20]));
        assert_eq!(vs.slot_leader(2).unwrap().address, Address::from([3; 20]));
        // Wraps around
        assert_eq!(vs.slot_leader(3).unwrap().address, Address::from([1; 20]));
        assert_eq!(vs.slot_leader(6).unwrap().address, Address::from([1; 20]));
    }

    #[test]
    fn contains_and_get() {
        let vs = ValidatorSet {
            epoch: 0,
            validators: vec![make_validator(1, 100_000), make_validator(2, 200_000)],
        };

        assert!(vs.contains(&Address::from([1; 20])));
        assert!(!vs.contains(&Address::from([3; 20])));

        let v = vs.get(&Address::from([2; 20])).unwrap();
        assert_eq!(v.total_stake, Amount::from_vtt(200_000));
    }

    #[test]
    fn total_stake() {
        let vs = ValidatorSet {
            epoch: 0,
            validators: vec![make_validator(1, 100_000), make_validator(2, 200_000)],
        };

        assert_eq!(vs.total_stake(), Amount::from_vtt(300_000));
    }
}
