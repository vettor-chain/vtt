use serde::{Deserialize, Serialize};

use vtt_crypto::blake3_hash;
use vtt_primitives::{Address, ChainId, Epoch};

/// Assignment of relay chain validators to an application chain for a given epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatorAssignment {
    /// The app chain these validators are assigned to.
    pub chain_id: ChainId,
    /// The epoch this assignment is for.
    pub epoch: Epoch,
    /// Assigned validator addresses (subset of relay chain active set).
    pub validators: Vec<Address>,
}

/// Deterministically assign a subset of relay validators to an app chain.
///
/// Uses a VRF-like deterministic shuffle seeded by (epoch, chain_id) to select
/// `count` validators from the relay set. This ensures:
/// - Same inputs always produce same assignment (deterministic)
/// - Different chains/epochs get different subsets (fair rotation)
/// - All validators eventually serve all chains
pub fn assign_validators(
    relay_validators: &[Address],
    chain_id: ChainId,
    epoch: Epoch,
    count: u32,
) -> ValidatorAssignment {
    let count = (count as usize).min(relay_validators.len());

    if count == 0 || relay_validators.is_empty() {
        return ValidatorAssignment {
            chain_id,
            epoch,
            validators: Vec::new(),
        };
    }

    // If we need all validators, just return them all
    if count >= relay_validators.len() {
        return ValidatorAssignment {
            chain_id,
            epoch,
            validators: relay_validators.to_vec(),
        };
    }

    // Deterministic seed from epoch + chain_id
    let seed_data = borsh::to_vec(&(epoch, chain_id.0)).unwrap();
    let seed_hash = blake3_hash(&seed_data);

    // Fisher-Yates shuffle using seed bytes as randomness source
    let mut indices: Vec<usize> = (0..relay_validators.len()).collect();
    let seed_bytes = seed_hash.as_bytes();

    for i in 0..count {
        // Use 4 bytes from the seed hash to generate a random index
        let byte_offset = (i * 4) % 32;
        let rand_val = u32::from_le_bytes([
            seed_bytes[byte_offset],
            seed_bytes[(byte_offset + 1) % 32],
            seed_bytes[(byte_offset + 2) % 32],
            seed_bytes[(byte_offset + 3) % 32],
        ]);

        let remaining = indices.len() - i;
        let j = i + (rand_val as usize % remaining);
        indices.swap(i, j);
    }

    let validators = indices[..count]
        .iter()
        .map(|&idx| relay_validators[idx])
        .collect();

    ValidatorAssignment {
        chain_id,
        epoch,
        validators,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_validators(n: u8) -> Vec<Address> {
        (1..=n).map(|i| Address::from([i; 20])).collect()
    }

    #[test]
    fn assign_subset() {
        let validators = make_validators(21);
        let assignment = assign_validators(&validators, ChainId::new(1), 0, 11);

        assert_eq!(assignment.chain_id, ChainId::new(1));
        assert_eq!(assignment.epoch, 0);
        assert_eq!(assignment.validators.len(), 11);

        // All assigned validators should be from the relay set
        for v in &assignment.validators {
            assert!(validators.contains(v));
        }

        // No duplicates
        let mut unique = assignment.validators.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 11);
    }

    #[test]
    fn deterministic_assignment() {
        let validators = make_validators(21);

        let a1 = assign_validators(&validators, ChainId::new(1), 0, 11);
        let a2 = assign_validators(&validators, ChainId::new(1), 0, 11);

        assert_eq!(a1.validators, a2.validators);
    }

    #[test]
    fn different_chains_get_different_sets() {
        let validators = make_validators(21);

        let a1 = assign_validators(&validators, ChainId::new(1), 0, 11);
        let a2 = assign_validators(&validators, ChainId::new(2), 0, 11);

        // Very likely different (not guaranteed but practically certain)
        assert_ne!(a1.validators, a2.validators);
    }

    #[test]
    fn different_epochs_get_different_sets() {
        let validators = make_validators(21);

        let a1 = assign_validators(&validators, ChainId::new(1), 0, 11);
        let a2 = assign_validators(&validators, ChainId::new(1), 1, 11);

        assert_ne!(a1.validators, a2.validators);
    }

    #[test]
    fn count_exceeds_available() {
        let validators = make_validators(5);
        let assignment = assign_validators(&validators, ChainId::new(1), 0, 11);

        // Should return all 5 validators
        assert_eq!(assignment.validators.len(), 5);
    }

    #[test]
    fn zero_count() {
        let validators = make_validators(21);
        let assignment = assign_validators(&validators, ChainId::new(1), 0, 0);
        assert!(assignment.validators.is_empty());
    }

    #[test]
    fn empty_relay_set() {
        let assignment = assign_validators(&[], ChainId::new(1), 0, 11);
        assert!(assignment.validators.is_empty());
    }

    #[test]
    fn all_validators_covered_over_epochs() {
        let validators = make_validators(21);
        let mut seen = std::collections::HashSet::new();

        // Over many epochs, all validators should be assigned at least once
        for epoch in 0..100 {
            let assignment = assign_validators(&validators, ChainId::new(1), epoch, 11);
            for v in &assignment.validators {
                seen.insert(*v);
            }
        }

        // All 21 validators should have been assigned at least once
        assert_eq!(seen.len(), 21);
    }
}
