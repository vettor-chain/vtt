use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::transaction::SignedTransaction;
use crate::{Address, BlockNumber, ChainId, Epoch, Signature, Timestamp, H256};

/// Block header — compact, hashable, sufficient for light clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct BlockHeader {
    /// Protocol version (allows hard forks).
    pub version: u32,
    /// Which chain this block belongs to.
    pub chain_id: ChainId,
    /// Height of this block.
    pub number: BlockNumber,
    /// BLAKE3 hash of the parent block header.
    pub parent_hash: H256,
    /// Merkle root of all transactions in this block.
    pub transactions_root: H256,
    /// Root hash of the world state trie AFTER executing this block.
    pub state_root: H256,
    /// Root hash of transaction receipts.
    pub receipts_root: H256,
    /// Address of the validator who produced this block.
    pub validator: Address,
    /// Epoch in which this block was produced.
    pub epoch: Epoch,
    /// Slot index within the epoch.
    pub slot: u32,
    /// Timestamp (milliseconds since Unix epoch).
    pub timestamp: Timestamp,
    /// Gas limit for this block.
    pub gas_limit: u64,
    /// Total gas used by all transactions.
    pub gas_used: u64,
    /// Hash of cross-chain messages included in this block (None if no cross-chain activity).
    pub cross_chain_root: Option<H256>,
    /// Ed25519 signature by the validator over the header (excluding this field).
    pub signature: Signature,
}

impl BlockHeader {
    /// Serialize the header without the signature field, for hashing/signing.
    pub fn signable_bytes(&self) -> Vec<u8> {
        let mut header_for_signing = self.clone();
        header_for_signing.signature = Signature::ZERO;
        borsh::to_vec(&header_for_signing).expect("header serialization should not fail")
    }
}

/// A complete block: header + transactions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Block {
    pub header: BlockHeader,
    pub transactions: Vec<SignedTransaction>,
}

impl Block {
    pub fn new(header: BlockHeader, transactions: Vec<SignedTransaction>) -> Self {
        Self {
            header,
            transactions,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty()
    }

    pub fn tx_count(&self) -> usize {
        self.transactions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_header() -> BlockHeader {
        BlockHeader {
            version: 1,
            chain_id: ChainId::RELAY,
            number: 0,
            parent_hash: H256::ZERO,
            transactions_root: H256::ZERO,
            state_root: H256::ZERO,
            receipts_root: H256::ZERO,
            validator: Address::ZERO,
            epoch: 0,
            slot: 0,
            timestamp: 1700000000000,
            gas_limit: 10_000_000,
            gas_used: 0,
            cross_chain_root: None,
            signature: Signature::ZERO,
        }
    }

    #[test]
    fn block_header_signable_bytes_excludes_signature() {
        let mut h1 = test_header();
        let mut h2 = test_header();
        h2.signature = Signature::from([0xFF; 64]);

        assert_eq!(h1.signable_bytes(), h2.signable_bytes());

        // But changing other fields changes the signable bytes
        h1.number = 1;
        assert_ne!(h1.signable_bytes(), h2.signable_bytes());
    }

    #[test]
    fn block_empty() {
        let block = Block::new(test_header(), vec![]);
        assert!(block.is_empty());
        assert_eq!(block.tx_count(), 0);
    }

    #[test]
    fn block_header_borsh_roundtrip() {
        let h = test_header();
        let bytes = borsh::to_vec(&h).unwrap();
        let h2 = BlockHeader::try_from_slice(&bytes).unwrap();
        assert_eq!(h, h2);
    }
}
