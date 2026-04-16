use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::amount::Amount;
use crate::{Address, BlockNumber, H256};

/// Voting period in blocks (~7 days at 3s/block).
pub const ASSET_VOTING_PERIOD_BLOCKS: u64 = 201_600;
/// Quorum: 33% of total supply must vote.
pub const ASSET_QUORUM_BPS: u64 = 3300;
/// Pass threshold: >50% yes votes (of yes+no).
pub const ASSET_PASS_THRESHOLD_BPS: u64 = 5001;
/// Supermajority: 67% for critical actions (e.g., ChangeIssuer).
pub const ASSET_SUPERMAJORITY_BPS: u64 = 6700;

/// Actions that can be proposed on an asset by its token holders.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum AssetProposalAction {
    /// Distribute VTT revenue to all holders (amount taken from proposer on execution).
    DistributeRevenue { total_amount: Amount },
    /// Transfer issuer role to a new address.
    ChangeIssuer { new_issuer: Address },
    /// Generic text proposal (signal vote, no on-chain execution).
    Signal { description: String },
    /// Dispose of the underlying asset (sell the property, liquidate, etc.).
    /// On-chain: freezes the asset and marks it Redeemed after execution.
    /// Off-chain: authorizes the SPV to proceed with the sale via notary/legal.
    /// Requires supermajority (67%).
    DisposeAsset { reason: String },
}

/// Status of an asset governance proposal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum AssetProposalStatus {
    /// Voting is open.
    Active,
    /// Passed quorum and threshold.
    Passed,
    /// Failed to reach quorum or threshold.
    Rejected,
    /// Executed on-chain.
    Executed,
}

/// A governance proposal scoped to a specific asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct AssetProposal {
    /// Unique proposal ID.
    pub id: H256,
    /// The asset this proposal targets.
    pub asset_id: H256,
    /// Who created the proposal.
    pub proposer: Address,
    /// The proposed action.
    pub action: AssetProposalAction,
    /// Human-readable description.
    pub description: String,
    /// Block number when the proposal was created.
    pub created_at: BlockNumber,
    /// Block number when voting ends.
    pub voting_end: BlockNumber,
    /// Current status.
    pub status: AssetProposalStatus,
    /// Total yes votes (weighted by token holdings).
    pub votes_yes: Amount,
    /// Total no votes (weighted by token holdings).
    pub votes_no: Amount,
    /// Total abstain votes (weighted by token holdings).
    pub votes_abstain: Amount,
    /// Addresses that have already voted (prevents double voting).
    pub voters: Vec<Address>,
}

impl AssetProposal {
    /// Check if the voting period has ended.
    pub fn is_voting_ended(&self, current_block: BlockNumber) -> bool {
        current_block >= self.voting_end
    }

    /// Total votes cast.
    pub fn total_votes(&self) -> Amount {
        self.votes_yes + self.votes_no + self.votes_abstain
    }

    /// Check if quorum is reached (total votes >= ASSET_QUORUM_BPS of total_supply).
    pub fn has_quorum(&self, total_supply: Amount) -> bool {
        if total_supply.is_zero() {
            return false;
        }
        self.total_votes().raw() * 10_000 >= total_supply.raw() * ASSET_QUORUM_BPS as u128
    }

    /// Check if the proposal passes the simple majority threshold (>50%).
    pub fn passes_threshold(&self) -> bool {
        let yes_plus_no = self.votes_yes.raw() + self.votes_no.raw();
        if yes_plus_no == 0 {
            return false;
        }
        self.votes_yes.raw() * 10_000 >= yes_plus_no * ASSET_PASS_THRESHOLD_BPS as u128
    }

    /// Check if the proposal passes the supermajority threshold (67%).
    pub fn passes_supermajority(&self) -> bool {
        let yes_plus_no = self.votes_yes.raw() + self.votes_no.raw();
        if yes_plus_no == 0 {
            return false;
        }
        self.votes_yes.raw() * 10_000 >= yes_plus_no * ASSET_SUPERMAJORITY_BPS as u128
    }

    /// Check if a voter has already voted.
    pub fn has_voted(&self, voter: &Address) -> bool {
        self.voters.contains(voter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_proposal_action_borsh_roundtrip() {
        let actions = vec![
            AssetProposalAction::DistributeRevenue {
                total_amount: Amount::from_vtt(1_000),
            },
            AssetProposalAction::ChangeIssuer {
                new_issuer: Address::from([0x02; 20]),
            },
            AssetProposalAction::Signal {
                description: "Test signal".to_string(),
            },
            AssetProposalAction::DisposeAsset {
                reason: "Sell underlying property".to_string(),
            },
        ];
        for action in actions {
            let bytes = borsh::to_vec(&action).unwrap();
            let action2 = AssetProposalAction::try_from_slice(&bytes).unwrap();
            assert_eq!(action, action2);
        }
    }

    #[test]
    fn asset_proposal_borsh_roundtrip() {
        let proposal = AssetProposal {
            id: H256::from([0xAA; 32]),
            asset_id: H256::from([0xBB; 32]),
            proposer: Address::from([0x01; 20]),
            action: AssetProposalAction::DistributeRevenue {
                total_amount: Amount::from_vtt(500),
            },
            description: "Distribute Q1 revenue".to_string(),
            created_at: 100,
            voting_end: 100 + ASSET_VOTING_PERIOD_BLOCKS,
            status: AssetProposalStatus::Active,
            votes_yes: Amount::from_vtt(100_000),
            votes_no: Amount::from_vtt(20_000),
            votes_abstain: Amount::from_vtt(5_000),
            voters: vec![Address::from([0x10; 20]), Address::from([0x20; 20])],
        };
        let bytes = borsh::to_vec(&proposal).unwrap();
        let proposal2 = AssetProposal::try_from_slice(&bytes).unwrap();
        assert_eq!(proposal, proposal2);
    }

    #[test]
    fn quorum_and_threshold_checks() {
        let mut proposal = AssetProposal {
            id: H256::ZERO,
            asset_id: H256::ZERO,
            proposer: Address::ZERO,
            action: AssetProposalAction::Signal {
                description: "test".to_string(),
            },
            description: "test".to_string(),
            created_at: 0,
            voting_end: 201_600,
            status: AssetProposalStatus::Active,
            votes_yes: Amount::from_vtt(400_000),
            votes_no: Amount::ZERO,
            votes_abstain: Amount::ZERO,
            voters: vec![],
        };

        let total_supply = Amount::from_vtt(1_000_000);

        // 400k / 1M = 40% > 33% quorum
        assert!(proposal.has_quorum(total_supply));
        // 400k yes, 0 no = 100% > 50% threshold
        assert!(proposal.passes_threshold());
        // 100% > 67% supermajority
        assert!(proposal.passes_supermajority());

        // Now test failing quorum: only 100k votes (10% < 33%)
        proposal.votes_yes = Amount::from_vtt(100_000);
        assert!(!proposal.has_quorum(total_supply));

        // Test failing threshold: 200k yes, 300k no
        proposal.votes_yes = Amount::from_vtt(200_000);
        proposal.votes_no = Amount::from_vtt(300_000);
        assert!(proposal.has_quorum(total_supply)); // 500k/1M = 50% > 33%
        assert!(!proposal.passes_threshold()); // 200k/500k = 40% < 50%
    }
}
