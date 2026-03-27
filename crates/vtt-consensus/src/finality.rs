use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use vtt_primitives::{Address, BlockNumber, H256};

/// A finality vote from a validator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalityVote {
    pub voter: Address,
    pub block_hash: H256,
    pub block_number: BlockNumber,
}

/// Tracks BFT finality — a block is final when 2/3+1 of validators attest to it.
pub struct FinalityTracker {
    /// Votes per block hash.
    votes: HashMap<H256, HashSet<Address>>,
    /// Number per block hash (for lookup).
    block_numbers: HashMap<H256, BlockNumber>,
    /// The last finalized block number.
    finalized_number: BlockNumber,
    /// The last finalized block hash.
    finalized_hash: H256,
    /// Total number of active validators (for threshold calculation).
    validator_count: usize,
}

impl FinalityTracker {
    pub fn new(validator_count: usize) -> Self {
        Self {
            votes: HashMap::new(),
            block_numbers: HashMap::new(),
            finalized_number: 0,
            finalized_hash: H256::ZERO,
            validator_count,
        }
    }

    /// Update the validator count (on epoch transitions).
    pub fn set_validator_count(&mut self, count: usize) {
        self.validator_count = count;
    }

    /// Required votes for finality: 2/3 + 1.
    pub fn threshold(&self) -> usize {
        if self.validator_count == 0 {
            return 0;
        }
        (self.validator_count * 2 / 3) + 1
    }

    /// Submit a finality vote. Returns true if the block became finalized.
    pub fn submit_vote(&mut self, vote: FinalityVote) -> bool {
        // Only consider votes for blocks after the currently finalized block
        if vote.block_number <= self.finalized_number {
            return false;
        }

        self.block_numbers
            .insert(vote.block_hash, vote.block_number);

        let voters = self.votes.entry(vote.block_hash).or_default();
        voters.insert(vote.voter);

        if voters.len() >= self.threshold() {
            // Block is finalized — also finalizes all ancestors
            self.finalized_number = vote.block_number;
            self.finalized_hash = vote.block_hash;

            // Clean up votes for blocks at or below finalized height
            self.cleanup();
            return true;
        }

        false
    }

    /// Clean up votes for blocks that are now below finalized height.
    fn cleanup(&mut self) {
        let finalized = self.finalized_number;
        let to_remove: Vec<H256> = self
            .block_numbers
            .iter()
            .filter(|(_, &num)| num <= finalized)
            .map(|(hash, _)| *hash)
            .collect();

        for hash in to_remove {
            self.votes.remove(&hash);
            self.block_numbers.remove(&hash);
        }
    }

    /// Get the latest finalized block number.
    pub fn finalized_number(&self) -> BlockNumber {
        self.finalized_number
    }

    /// Get the latest finalized block hash.
    pub fn finalized_hash(&self) -> H256 {
        self.finalized_hash
    }

    /// Get the number of votes for a specific block.
    pub fn votes_for(&self, block_hash: &H256) -> usize {
        self.votes.get(block_hash).map(|v| v.len()).unwrap_or(0)
    }

    /// Check if a block is finalized.
    pub fn is_finalized(&self, block_number: BlockNumber) -> bool {
        block_number <= self.finalized_number
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_vote(voter_byte: u8, block_hash: H256, block_number: BlockNumber) -> FinalityVote {
        FinalityVote {
            voter: Address::from([voter_byte; 20]),
            block_hash,
            block_number,
        }
    }

    #[test]
    fn threshold_calculation() {
        let tracker = FinalityTracker::new(21);
        assert_eq!(tracker.threshold(), 15); // 21 * 2/3 + 1 = 15

        let tracker3 = FinalityTracker::new(3);
        assert_eq!(tracker3.threshold(), 3); // 3 * 2/3 + 1 = 3

        let tracker1 = FinalityTracker::new(1);
        assert_eq!(tracker1.threshold(), 1);
    }

    #[test]
    fn single_vote_no_finality() {
        let mut tracker = FinalityTracker::new(3);
        let hash = H256::from([0x01; 32]);

        let finalized = tracker.submit_vote(make_vote(1, hash, 1));
        assert!(!finalized);
        assert_eq!(tracker.finalized_number(), 0);
        assert_eq!(tracker.votes_for(&hash), 1);
    }

    #[test]
    fn threshold_reached_finalizes() {
        let mut tracker = FinalityTracker::new(3);
        let hash = H256::from([0x01; 32]);

        tracker.submit_vote(make_vote(1, hash, 10));
        tracker.submit_vote(make_vote(2, hash, 10));
        let finalized = tracker.submit_vote(make_vote(3, hash, 10));

        assert!(finalized);
        assert_eq!(tracker.finalized_number(), 10);
        assert_eq!(tracker.finalized_hash(), hash);
        assert!(tracker.is_finalized(10));
        assert!(tracker.is_finalized(5)); // ancestors also finalized
        assert!(!tracker.is_finalized(11));
    }

    #[test]
    fn duplicate_voter_not_counted() {
        let mut tracker = FinalityTracker::new(3);
        let hash = H256::from([0x01; 32]);

        tracker.submit_vote(make_vote(1, hash, 10));
        tracker.submit_vote(make_vote(1, hash, 10)); // duplicate
        tracker.submit_vote(make_vote(2, hash, 10));

        assert_eq!(tracker.votes_for(&hash), 2); // only 2 unique voters
        assert!(!tracker.is_finalized(10));
    }

    #[test]
    fn votes_below_finalized_ignored() {
        let mut tracker = FinalityTracker::new(1); // threshold = 1

        let hash1 = H256::from([0x01; 32]);
        tracker.submit_vote(make_vote(1, hash1, 10));
        assert!(tracker.is_finalized(10));

        // Vote for block 5 (below finalized) should be ignored
        let hash2 = H256::from([0x02; 32]);
        let result = tracker.submit_vote(make_vote(2, hash2, 5));
        assert!(!result);
    }

    #[test]
    fn cleanup_removes_old_votes() {
        let mut tracker = FinalityTracker::new(1);

        let hash_a = H256::from([0x0A; 32]);
        let hash_b = H256::from([0x0B; 32]);

        // Vote for block 5
        tracker.submit_vote(make_vote(1, hash_a, 5));
        assert!(tracker.is_finalized(5));

        // Vote for block 10 — should clean up block 5 votes
        tracker.submit_vote(make_vote(1, hash_b, 10));
        assert!(tracker.is_finalized(10));
        assert_eq!(tracker.votes_for(&hash_a), 0); // cleaned up
    }

    #[test]
    fn update_validator_count() {
        let mut tracker = FinalityTracker::new(3);
        assert_eq!(tracker.threshold(), 3);

        tracker.set_validator_count(21);
        assert_eq!(tracker.threshold(), 15);
    }

    #[test]
    fn progressive_finality() {
        let mut tracker = FinalityTracker::new(3);

        let hash10 = H256::from([0x10; 32]);
        let hash20 = H256::from([0x20; 32]);

        // Finalize block 10
        tracker.submit_vote(make_vote(1, hash10, 10));
        tracker.submit_vote(make_vote(2, hash10, 10));
        tracker.submit_vote(make_vote(3, hash10, 10));
        assert_eq!(tracker.finalized_number(), 10);

        // Finalize block 20
        tracker.submit_vote(make_vote(1, hash20, 20));
        tracker.submit_vote(make_vote(2, hash20, 20));
        tracker.submit_vote(make_vote(3, hash20, 20));
        assert_eq!(tracker.finalized_number(), 20);

        // Both 10 and 20 are finalized
        assert!(tracker.is_finalized(10));
        assert!(tracker.is_finalized(20));
    }
}
