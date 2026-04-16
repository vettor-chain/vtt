use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use vtt_primitives::amount::Amount;
use vtt_primitives::block::BlockHeader;
use vtt_primitives::{Address, Epoch};

/// Evidence of a double-sign: two different block headers signed by the same
/// validator for the same epoch and slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct DoubleSignEvidence {
    pub header_a: BlockHeader,
    pub header_b: BlockHeader,
}

impl DoubleSignEvidence {
    /// Validate that this evidence is well-formed:
    /// - Same validator
    /// - Same epoch and slot
    /// - Different block hashes (different signable bytes)
    pub fn is_valid(&self) -> bool {
        self.header_a.validator == self.header_b.validator
            && self.header_a.epoch == self.header_b.epoch
            && self.header_a.slot == self.header_b.slot
            && self.header_a.signable_bytes() != self.header_b.signable_bytes()
    }

    /// Get the address of the offending validator.
    pub fn offender(&self) -> Address {
        self.header_a.validator
    }
}

/// Reason for slashing a validator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlashingReason {
    DoubleSigning {
        evidence: Box<DoubleSignEvidence>,
    },
    Downtime {
        missed_slots: u32,
        total_slots: u32,
        epoch: Epoch,
    },
}

/// A record of a slashing event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashRecord {
    pub validator: Address,
    pub reason: SlashingReason,
    pub slashed_amount: Amount,
    pub epoch: Epoch,
}

/// Calculate the slash amount for double signing.
/// Penalty: `slash_double_sign_bps` basis points of total stake.
pub fn calculate_double_sign_slash(total_stake: Amount, slash_bps: u16) -> Amount {
    let raw = total_stake.raw() * slash_bps as u128 / 10_000;
    Amount::from_raw(raw)
}

/// Calculate the slash amount for downtime.
/// Penalty: `slash_downtime_bps` basis points of total stake.
pub fn calculate_downtime_slash(total_stake: Amount, slash_bps: u16) -> Amount {
    let raw = total_stake.raw() * slash_bps as u128 / 10_000;
    Amount::from_raw(raw)
}

/// Check if a validator exceeded the downtime threshold.
pub fn is_downtime_violation(missed_slots: u32, total_slots: u32, threshold_pct: u8) -> bool {
    if total_slots == 0 {
        return false;
    }
    let missed_pct = (missed_slots as u64 * 100) / total_slots as u64;
    missed_pct > threshold_pct as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtt_primitives::{ChainId, Signature, H256};

    fn make_header(validator: Address, epoch: u64, slot: u32, number: u64) -> BlockHeader {
        BlockHeader {
            version: 1,
            chain_id: ChainId::RELAY,
            number,
            parent_hash: H256::from([number as u8; 32]),
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

    #[test]
    fn double_sign_evidence_valid() {
        let val = Address::from([0x01; 20]);
        let evidence = DoubleSignEvidence {
            header_a: make_header(val, 0, 0, 1),
            header_b: make_header(val, 0, 0, 2), // different block number = different hash
        };
        assert!(evidence.is_valid());
        assert_eq!(evidence.offender(), val);
    }

    #[test]
    fn double_sign_evidence_invalid_different_validator() {
        let evidence = DoubleSignEvidence {
            header_a: make_header(Address::from([0x01; 20]), 0, 0, 1),
            header_b: make_header(Address::from([0x02; 20]), 0, 0, 2),
        };
        assert!(!evidence.is_valid());
    }

    #[test]
    fn double_sign_evidence_invalid_different_slot() {
        let val = Address::from([0x01; 20]);
        let evidence = DoubleSignEvidence {
            header_a: make_header(val, 0, 0, 1),
            header_b: make_header(val, 0, 1, 2), // different slot
        };
        assert!(!evidence.is_valid());
    }

    #[test]
    fn double_sign_evidence_invalid_same_block() {
        let val = Address::from([0x01; 20]);
        let h = make_header(val, 0, 0, 1);
        let evidence = DoubleSignEvidence {
            header_a: h.clone(),
            header_b: h,
        };
        assert!(!evidence.is_valid()); // same signable bytes
    }

    #[test]
    fn calculate_double_sign_slash_5_percent() {
        let stake = Amount::from_vtt(200_000);
        let slash = calculate_double_sign_slash(stake, 500); // 5%
        assert_eq!(slash, Amount::from_vtt(10_000));
    }

    #[test]
    fn calculate_downtime_slash_01_percent() {
        let stake = Amount::from_vtt(200_000);
        let slash = calculate_downtime_slash(stake, 10); // 0.1%
        assert_eq!(slash, Amount::from_vtt(200));
    }

    #[test]
    fn downtime_violation_check() {
        // 50% threshold
        assert!(!is_downtime_violation(50, 100, 50)); // exactly 50% = not a violation
        assert!(is_downtime_violation(51, 100, 50)); // 51% > 50% = violation
        assert!(!is_downtime_violation(10, 100, 50)); // 10% < 50% = no violation
        assert!(!is_downtime_violation(0, 0, 50)); // no slots = no violation
    }
}
