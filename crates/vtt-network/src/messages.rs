use serde::{Deserialize, Serialize};

use vtt_primitives::block::Block;
use vtt_primitives::transaction::SignedTransaction;
use vtt_primitives::{BlockNumber, H256};

/// Messages exchanged between VTT nodes on the P2P network.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum NetworkMessage {
    /// Announce a new block to peers.
    BlockAnnounce {
        block_hash: H256,
        block_number: BlockNumber,
        /// The full block (for small blocks) or just the header.
        block: Block,
    },

    /// Broadcast a new transaction to peers.
    TransactionBroadcast { transaction: SignedTransaction },

    /// Request a specific block by hash.
    BlockRequest { block_hash: H256 },

    /// Request a range of blocks by number.
    BlockRangeRequest { from: BlockNumber, to: BlockNumber },

    /// Response to a block request.
    BlockResponse { block: Option<Block> },

    /// Response to a block range request.
    BlockRangeResponse { blocks: Vec<Block> },

    /// Peer status exchange (handshake).
    Status {
        chain_id: vtt_primitives::ChainId,
        best_block_hash: H256,
        best_block_number: BlockNumber,
        genesis_hash: H256,
    },
}

impl NetworkMessage {
    /// Serialize a message to JSON bytes for network transmission.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("message serialization failed")
    }

    /// Deserialize a message from JSON bytes.
    pub fn decode(data: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(data)
    }
}

/// GossipSub topic names for the VTT network.
pub mod topics {
    pub fn block_announce(chain_id: u32) -> String {
        format!("/vtt/chain/{chain_id}/blocks")
    }

    pub fn transactions(chain_id: u32) -> String {
        format!("/vtt/chain/{chain_id}/txs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtt_primitives::block::BlockHeader;
    use vtt_primitives::{Address, ChainId, Signature};

    fn test_block() -> Block {
        Block::new(
            BlockHeader {
                version: 1,
                chain_id: ChainId::RELAY,
                number: 1,
                parent_hash: H256::ZERO,
                transactions_root: H256::ZERO,
                state_root: H256::ZERO,
                receipts_root: H256::ZERO,
                validator: Address::ZERO,
                epoch: 0,
                slot: 1,
                timestamp: 1_700_000_000_000,
                gas_limit: 10_000_000,
                gas_used: 0,
                cross_chain_root: None,
                signature: Signature::ZERO,
            },
            vec![],
        )
    }

    #[test]
    fn encode_decode_block_announce() {
        let msg = NetworkMessage::BlockAnnounce {
            block_hash: H256::from([0xAB; 32]),
            block_number: 1,
            block: test_block(),
        };
        let bytes = msg.encode();
        let decoded = NetworkMessage::decode(&bytes).unwrap();
        match decoded {
            NetworkMessage::BlockAnnounce {
                block_number,
                block,
                ..
            } => {
                assert_eq!(block_number, 1);
                assert_eq!(block.header.number, 1);
            }
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn encode_decode_status() {
        let msg = NetworkMessage::Status {
            chain_id: ChainId::RELAY,
            best_block_hash: H256::ZERO,
            best_block_number: 0,
            genesis_hash: H256::from([0x01; 32]),
        };
        let bytes = msg.encode();
        let decoded = NetworkMessage::decode(&bytes).unwrap();
        match decoded {
            NetworkMessage::Status {
                best_block_number, ..
            } => assert_eq!(best_block_number, 0),
            _ => panic!("wrong message type"),
        }
    }

    #[test]
    fn topic_names() {
        assert_eq!(topics::block_announce(0), "/vtt/chain/0/blocks");
        assert_eq!(topics::transactions(1), "/vtt/chain/1/txs");
    }
}
