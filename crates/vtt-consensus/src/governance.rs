use std::collections::HashMap;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, BlockNumber, Vote, H256};

/// Voting period in blocks (~7 days at 3s/block = 201600 blocks).
pub const VOTING_PERIOD_BLOCKS: u64 = 201_600;
/// Quorum: 33% of staked VTT must vote.
pub const QUORUM_BPS: u64 = 3300;
/// Pass threshold: 50% + 1 of votes cast.
pub const PASS_THRESHOLD_BPS: u64 = 5001;

/// Types of governance proposals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum ProposalAction {
    /// Change a consensus parameter.
    ParameterChange { key: String, value: String },
    /// Register a new application chain.
    RegisterChain { name: String, config_json: String },
    /// Spend from the protocol treasury.
    TreasurySpend { recipient: Address, amount: Amount },
    /// Signal readiness for a protocol upgrade.
    ProtocolUpgrade { version: u32, description: String },
    /// Pause or unpause the DEX.
    DexPause(bool),
}

/// Status of a governance proposal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum ProposalStatus {
    /// Voting is open.
    Active,
    /// Passed quorum and threshold — ready to execute.
    Passed,
    /// Failed to reach quorum or threshold.
    Rejected,
    /// Executed on-chain.
    Executed,
}

/// A governance proposal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Proposal {
    pub id: H256,
    pub proposer: Address,
    pub action: ProposalAction,
    pub description: String,
    pub created_at: BlockNumber,
    pub voting_end: BlockNumber,
    pub status: ProposalStatus,
    pub votes_yes: Amount,
    pub votes_no: Amount,
    pub votes_abstain: Amount,
    /// Who has voted (to prevent double voting).
    pub voters: Vec<Address>,
}

impl Proposal {
    /// Check if the voting period has ended.
    pub fn is_voting_ended(&self, current_block: BlockNumber) -> bool {
        current_block >= self.voting_end
    }

    /// Total votes cast.
    pub fn total_votes(&self) -> Amount {
        self.votes_yes + self.votes_no + self.votes_abstain
    }

    /// Check if quorum is reached.
    pub fn has_quorum(&self, total_staked: Amount) -> bool {
        if total_staked.is_zero() {
            return false;
        }
        // quorum = total_votes / total_staked >= QUORUM_BPS / 10000
        self.total_votes().raw() * 10_000 >= total_staked.raw() * QUORUM_BPS as u128
    }

    /// Check if the proposal passes the threshold.
    pub fn passes_threshold(&self) -> bool {
        let yes_plus_no = self.votes_yes.raw() + self.votes_no.raw();
        if yes_plus_no == 0 {
            return false;
        }
        // yes / (yes + no) >= PASS_THRESHOLD_BPS / 10000
        self.votes_yes.raw() * 10_000 >= yes_plus_no * PASS_THRESHOLD_BPS as u128
    }

    /// Check if a voter has already voted.
    pub fn has_voted(&self, voter: &Address) -> bool {
        self.voters.contains(voter)
    }
}

/// The governance system managing all proposals.
pub struct GovernanceSystem {
    proposals: HashMap<H256, Proposal>,
    next_id: u64,
}

impl GovernanceSystem {
    pub fn new() -> Self {
        Self {
            proposals: HashMap::new(),
            next_id: 0,
        }
    }

    /// Create a new proposal. Returns the proposal ID.
    pub fn create_proposal(
        &mut self,
        proposer: Address,
        action: ProposalAction,
        description: String,
        current_block: BlockNumber,
    ) -> H256 {
        let id_data = borsh::to_vec(&(self.next_id, &proposer, current_block)).unwrap();
        let id = vtt_crypto::blake3_hash(&id_data);
        self.next_id += 1;

        let proposal = Proposal {
            id,
            proposer,
            action,
            description,
            created_at: current_block,
            voting_end: current_block + VOTING_PERIOD_BLOCKS,
            status: ProposalStatus::Active,
            votes_yes: Amount::ZERO,
            votes_no: Amount::ZERO,
            votes_abstain: Amount::ZERO,
            voters: Vec::new(),
        };

        self.proposals.insert(id, proposal);
        id
    }

    /// Cast a vote on a proposal. `voting_power` is the voter's staked VTT.
    pub fn vote(
        &mut self,
        proposal_id: &H256,
        voter: Address,
        vote: Vote,
        voting_power: Amount,
        current_block: BlockNumber,
    ) -> Result<(), GovernanceError> {
        let proposal = self
            .proposals
            .get_mut(proposal_id)
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Active {
            return Err(GovernanceError::NotActive);
        }

        if proposal.is_voting_ended(current_block) {
            return Err(GovernanceError::VotingEnded);
        }

        if proposal.has_voted(&voter) {
            return Err(GovernanceError::AlreadyVoted);
        }

        if voting_power.is_zero() {
            return Err(GovernanceError::NoVotingPower);
        }

        match vote {
            Vote::Yes => proposal.votes_yes = proposal.votes_yes + voting_power,
            Vote::No => proposal.votes_no = proposal.votes_no + voting_power,
            Vote::Abstain => proposal.votes_abstain = proposal.votes_abstain + voting_power,
        }

        proposal.voters.push(voter);
        Ok(())
    }

    /// Finalize a proposal after voting ends. Returns the final status.
    pub fn finalize(
        &mut self,
        proposal_id: &H256,
        total_staked: Amount,
        current_block: BlockNumber,
    ) -> Result<ProposalStatus, GovernanceError> {
        let proposal = self
            .proposals
            .get_mut(proposal_id)
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Active {
            return Err(GovernanceError::NotActive);
        }

        if !proposal.is_voting_ended(current_block) {
            return Err(GovernanceError::VotingNotEnded);
        }

        let status = if proposal.has_quorum(total_staked) && proposal.passes_threshold() {
            ProposalStatus::Passed
        } else {
            ProposalStatus::Rejected
        };

        proposal.status = status.clone();
        Ok(status)
    }

    /// Mark a passed proposal as executed.
    pub fn mark_executed(&mut self, proposal_id: &H256) -> Result<(), GovernanceError> {
        let proposal = self
            .proposals
            .get_mut(proposal_id)
            .ok_or(GovernanceError::ProposalNotFound)?;

        if proposal.status != ProposalStatus::Passed {
            return Err(GovernanceError::NotPassed);
        }

        proposal.status = ProposalStatus::Executed;
        Ok(())
    }

    /// Get a proposal by ID.
    pub fn get(&self, proposal_id: &H256) -> Option<&Proposal> {
        self.proposals.get(proposal_id)
    }

    /// List active proposals.
    pub fn active_proposals(&self) -> Vec<&Proposal> {
        self.proposals
            .values()
            .filter(|p| p.status == ProposalStatus::Active)
            .collect()
    }

    /// Total number of proposals.
    pub fn proposal_count(&self) -> usize {
        self.proposals.len()
    }
}

impl Default for GovernanceSystem {
    fn default() -> Self {
        Self::new()
    }
}

/// Governance errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GovernanceError {
    ProposalNotFound,
    NotActive,
    VotingEnded,
    VotingNotEnded,
    AlreadyVoted,
    NoVotingPower,
    NotPassed,
}

impl std::fmt::Display for GovernanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProposalNotFound => write!(f, "proposal not found"),
            Self::NotActive => write!(f, "proposal is not active"),
            Self::VotingEnded => write!(f, "voting period ended"),
            Self::VotingNotEnded => write!(f, "voting period not ended yet"),
            Self::AlreadyVoted => write!(f, "already voted"),
            Self::NoVotingPower => write!(f, "no voting power"),
            Self::NotPassed => write!(f, "proposal did not pass"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (GovernanceSystem, H256) {
        let mut gov = GovernanceSystem::new();
        let id = gov.create_proposal(
            Address::from([0x01; 20]),
            ProposalAction::ParameterChange {
                key: "block_time_ms".to_string(),
                value: "2000".to_string(),
            },
            "Reduce block time to 2s".to_string(),
            100,
        );
        (gov, id)
    }

    #[test]
    fn create_proposal() {
        let (gov, id) = setup();
        let p = gov.get(&id).unwrap();
        assert_eq!(p.status, ProposalStatus::Active);
        assert_eq!(p.voting_end, 100 + VOTING_PERIOD_BLOCKS);
        assert_eq!(p.proposer, Address::from([0x01; 20]));
    }

    #[test]
    fn vote_yes() {
        let (mut gov, id) = setup();
        gov.vote(
            &id,
            Address::from([0x10; 20]),
            Vote::Yes,
            Amount::from_vtt(100_000),
            200,
        )
        .unwrap();

        let p = gov.get(&id).unwrap();
        assert_eq!(p.votes_yes, Amount::from_vtt(100_000));
        assert_eq!(p.voters.len(), 1);
    }

    #[test]
    fn double_vote_rejected() {
        let (mut gov, id) = setup();
        let voter = Address::from([0x10; 20]);
        gov.vote(&id, voter, Vote::Yes, Amount::from_vtt(100_000), 200)
            .unwrap();
        let err = gov.vote(&id, voter, Vote::No, Amount::from_vtt(100_000), 200);
        assert_eq!(err, Err(GovernanceError::AlreadyVoted));
    }

    #[test]
    fn vote_after_period_rejected() {
        let (mut gov, id) = setup();
        let after_end = 100 + VOTING_PERIOD_BLOCKS + 1;
        let err = gov.vote(
            &id,
            Address::from([0x10; 20]),
            Vote::Yes,
            Amount::from_vtt(100_000),
            after_end,
        );
        assert_eq!(err, Err(GovernanceError::VotingEnded));
    }

    #[test]
    fn zero_voting_power_rejected() {
        let (mut gov, id) = setup();
        let err = gov.vote(&id, Address::from([0x10; 20]), Vote::Yes, Amount::ZERO, 200);
        assert_eq!(err, Err(GovernanceError::NoVotingPower));
    }

    #[test]
    fn proposal_passes() {
        let (mut gov, id) = setup();
        let total_staked = Amount::from_vtt(1_000_000);

        // 400k yes votes (40% of total staked > 33% quorum, 100% yes > 50% threshold)
        gov.vote(
            &id,
            Address::from([0x10; 20]),
            Vote::Yes,
            Amount::from_vtt(400_000),
            200,
        )
        .unwrap();

        let after_end = 100 + VOTING_PERIOD_BLOCKS;
        let status = gov.finalize(&id, total_staked, after_end).unwrap();
        assert_eq!(status, ProposalStatus::Passed);
    }

    #[test]
    fn proposal_rejected_no_quorum() {
        let (mut gov, id) = setup();
        let total_staked = Amount::from_vtt(1_000_000);

        // Only 100k votes (10% < 33% quorum)
        gov.vote(
            &id,
            Address::from([0x10; 20]),
            Vote::Yes,
            Amount::from_vtt(100_000),
            200,
        )
        .unwrap();

        let after_end = 100 + VOTING_PERIOD_BLOCKS;
        let status = gov.finalize(&id, total_staked, after_end).unwrap();
        assert_eq!(status, ProposalStatus::Rejected);
    }

    #[test]
    fn proposal_rejected_no_majority() {
        let (mut gov, id) = setup();
        let total_staked = Amount::from_vtt(1_000_000);

        // 200k yes, 300k no (quorum ok, but majority no)
        gov.vote(
            &id,
            Address::from([0x10; 20]),
            Vote::Yes,
            Amount::from_vtt(200_000),
            200,
        )
        .unwrap();
        gov.vote(
            &id,
            Address::from([0x20; 20]),
            Vote::No,
            Amount::from_vtt(300_000),
            200,
        )
        .unwrap();

        let after_end = 100 + VOTING_PERIOD_BLOCKS;
        let status = gov.finalize(&id, total_staked, after_end).unwrap();
        assert_eq!(status, ProposalStatus::Rejected);
    }

    #[test]
    fn finalize_before_end_rejected() {
        let (mut gov, id) = setup();
        let err = gov.finalize(&id, Amount::from_vtt(1_000_000), 150);
        assert_eq!(err, Err(GovernanceError::VotingNotEnded));
    }

    #[test]
    fn execute_passed_proposal() {
        let (mut gov, id) = setup();
        let total_staked = Amount::from_vtt(1_000_000);

        gov.vote(
            &id,
            Address::from([0x10; 20]),
            Vote::Yes,
            Amount::from_vtt(500_000),
            200,
        )
        .unwrap();

        let after_end = 100 + VOTING_PERIOD_BLOCKS;
        gov.finalize(&id, total_staked, after_end).unwrap();
        gov.mark_executed(&id).unwrap();

        assert_eq!(gov.get(&id).unwrap().status, ProposalStatus::Executed);
    }

    #[test]
    fn execute_rejected_fails() {
        let (mut gov, id) = setup();
        let total_staked = Amount::from_vtt(1_000_000);

        let after_end = 100 + VOTING_PERIOD_BLOCKS;
        gov.finalize(&id, total_staked, after_end).unwrap(); // no votes = rejected

        let err = gov.mark_executed(&id);
        assert_eq!(err, Err(GovernanceError::NotPassed));
    }

    #[test]
    fn treasury_spend_proposal() {
        let mut gov = GovernanceSystem::new();
        let id = gov.create_proposal(
            Address::from([0x01; 20]),
            ProposalAction::TreasurySpend {
                recipient: Address::from([0x50; 20]),
                amount: Amount::from_vtt(50_000),
            },
            "Fund ecosystem grant".to_string(),
            0,
        );
        assert!(gov.get(&id).is_some());
    }

    #[test]
    fn active_proposals_list() {
        let mut gov = GovernanceSystem::new();
        gov.create_proposal(
            Address::ZERO,
            ProposalAction::ProtocolUpgrade {
                version: 2,
                description: "v2 upgrade".to_string(),
            },
            "Upgrade".to_string(),
            0,
        );
        gov.create_proposal(
            Address::ZERO,
            ProposalAction::ParameterChange {
                key: "gas_limit".to_string(),
                value: "20000000".to_string(),
            },
            "Increase gas".to_string(),
            0,
        );
        assert_eq!(gov.active_proposals().len(), 2);
        assert_eq!(gov.proposal_count(), 2);
    }
}
