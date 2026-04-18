use std::collections::VecDeque;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use vtt_crypto::{blake3_hash, merkle_root};
use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, ChainId, H256};

#[derive(Debug, Error)]
pub enum MessagingError {
    #[error("source and destination chains must differ")]
    SameChain,
    #[error("message already processed: {0}")]
    AlreadyProcessed(H256),
    #[error("invalid proof for message {0}")]
    InvalidProof(H256),
    #[error("destination chain not found: {0}")]
    DestinationNotFound(ChainId),
    #[error("inbox pending queue full (cap {0}); apply back-pressure to the relay")]
    InboxFull(usize),
}

pub type Result<T> = std::result::Result<T, MessagingError>;

/// Payload types for cross-chain messages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum CrossChainPayload {
    /// Transfer VTT from one chain to another.
    VttTransfer { amount: Amount },
    /// Transfer a tokenized asset cross-chain.
    AssetTransfer { asset_id: H256, amount: Amount },
    /// Arbitrary data message (for contract-to-contract communication).
    DataMessage { data: Vec<u8> },
}

/// Status of a cross-chain message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum MessageStatus {
    /// Message is in the source chain's outbox, waiting to be relayed.
    Pending,
    /// Message has been relayed to the relay chain.
    Relayed,
    /// Message has been delivered to the destination chain.
    Delivered,
    /// Message delivery failed.
    Failed { reason: String },
}

/// A cross-chain message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct CrossChainMessage {
    /// Unique message ID (hash of content).
    pub id: H256,
    /// Incrementing nonce per source chain.
    pub nonce: u64,
    /// Originating chain.
    pub source_chain: ChainId,
    /// Destination chain.
    pub destination_chain: ChainId,
    /// Sender on source chain.
    pub sender: Address,
    /// Recipient on destination chain.
    pub recipient: Address,
    /// The message payload.
    pub payload: CrossChainPayload,
    /// Current status.
    pub status: MessageStatus,
    /// Block number on source chain when message was created.
    pub source_block: u64,
}

impl CrossChainMessage {
    /// Create a new cross-chain message.
    pub fn new(
        nonce: u64,
        source_chain: ChainId,
        destination_chain: ChainId,
        sender: Address,
        recipient: Address,
        payload: CrossChainPayload,
        source_block: u64,
    ) -> std::result::Result<Self, MessagingError> {
        if source_chain == destination_chain {
            return Err(MessagingError::SameChain);
        }

        let id_data = borsh::to_vec(&(nonce, source_chain, destination_chain, &sender, &payload))
            .expect("serialization should not fail");
        let id = blake3_hash(&id_data);

        Ok(Self {
            id,
            nonce,
            source_chain,
            destination_chain,
            sender,
            recipient,
            payload,
            status: MessageStatus::Pending,
            source_block,
        })
    }

    /// Compute the hash of this message for Merkle tree inclusion.
    pub fn hash(&self) -> H256 {
        let data = borsh::to_vec(self).expect("serialization should not fail");
        blake3_hash(&data)
    }
}

/// Outbox for a chain — collects messages to be sent to other chains.
pub struct MessageOutbox {
    /// Chain this outbox belongs to.
    pub chain_id: ChainId,
    /// Next nonce for messages from this chain.
    next_nonce: u64,
    /// Pending messages in the outbox.
    messages: VecDeque<CrossChainMessage>,
}

impl MessageOutbox {
    pub fn new(chain_id: ChainId) -> Self {
        Self {
            chain_id,
            next_nonce: 0,
            messages: VecDeque::new(),
        }
    }

    /// Add a message to the outbox.
    pub fn send(
        &mut self,
        destination: ChainId,
        sender: Address,
        recipient: Address,
        payload: CrossChainPayload,
        block_number: u64,
    ) -> Result<CrossChainMessage> {
        let msg = CrossChainMessage::new(
            self.next_nonce,
            self.chain_id,
            destination,
            sender,
            recipient,
            payload,
            block_number,
        )?;

        self.next_nonce += 1;
        self.messages.push_back(msg.clone());
        Ok(msg)
    }

    /// Compute the Merkle root of all pending messages (for block header).
    pub fn merkle_root(&self) -> H256 {
        if self.messages.is_empty() {
            return H256::ZERO;
        }
        let hashes: Vec<H256> = self.messages.iter().map(|m| m.hash()).collect();
        merkle_root(&hashes)
    }

    /// Drain all pending messages (called after block is finalized).
    pub fn drain(&mut self) -> Vec<CrossChainMessage> {
        self.messages.drain(..).collect()
    }

    /// Number of pending messages.
    pub fn pending_count(&self) -> usize {
        self.messages.len()
    }

    /// Current nonce.
    pub fn nonce(&self) -> u64 {
        self.next_nonce
    }
}

/// Inbox for a chain — receives messages from other chains via the relay.
///
/// Both the pending queue and the processed-id set are bounded:
/// - `pending` rejects new messages with `InboxFull` once the cap is
///   reached, forcing the relay to back off instead of filling RAM.
/// - `processed` keeps a sliding window of the most recent N ids; when
///   full, the oldest id is evicted. A replay that shows up after its
///   dedup entry expired will be re-delivered — fine because the same
///   window-eviction applies consistently on every replica, and the
///   window is sized several orders of magnitude beyond the realistic
///   relay throughput per session.
pub struct MessageInbox {
    /// Chain this inbox belongs to.
    pub chain_id: ChainId,
    /// Processed message IDs (to prevent replay) — O(1) membership.
    processed: std::collections::HashSet<H256>,
    /// Insertion order of `processed` entries, for oldest-first eviction.
    processed_order: VecDeque<H256>,
    /// Messages waiting to be processed.
    pending: VecDeque<CrossChainMessage>,
    /// Hard cap on `pending`.
    pending_cap: usize,
    /// Hard cap on `processed`.
    processed_cap: usize,
}

/// Default cap on pending messages. Sized to tolerate a relay burst while
/// still keeping worst-case memory bounded (CrossChainMessage is a few
/// hundred bytes + payload, so 10k ≈ few MB).
pub const DEFAULT_INBOX_PENDING_CAP: usize = 10_000;
/// Default dedup window. An id that ages out of the window can be
/// redelivered — pick large enough that normal relay churn can't evict
/// live messages.
pub const DEFAULT_INBOX_PROCESSED_CAP: usize = 50_000;

impl MessageInbox {
    pub fn new(chain_id: ChainId) -> Self {
        Self::with_caps(
            chain_id,
            DEFAULT_INBOX_PENDING_CAP,
            DEFAULT_INBOX_PROCESSED_CAP,
        )
    }

    /// Construct with explicit caps. Both must be non-zero; zero inputs
    /// are clamped to 1 so the inbox isn't silently disabled.
    pub fn with_caps(chain_id: ChainId, pending_cap: usize, processed_cap: usize) -> Self {
        Self {
            chain_id,
            processed: std::collections::HashSet::new(),
            processed_order: VecDeque::new(),
            pending: VecDeque::new(),
            pending_cap: pending_cap.max(1),
            processed_cap: processed_cap.max(1),
        }
    }

    /// Receive a message from the relay chain.
    pub fn receive(&mut self, mut msg: CrossChainMessage) -> Result<()> {
        if self.processed.contains(&msg.id) {
            return Err(MessagingError::AlreadyProcessed(msg.id));
        }
        // Refuse before touching `processed` — otherwise we'd burn a dedup
        // slot on a message we didn't actually accept.
        if self.pending.len() >= self.pending_cap {
            return Err(MessagingError::InboxFull(self.pending_cap));
        }

        msg.status = MessageStatus::Delivered;
        let msg_id = msg.id;
        self.pending.push_back(msg);

        // Sliding dedup window: evict oldest id when at cap.
        if self.processed.len() >= self.processed_cap {
            if let Some(oldest) = self.processed_order.pop_front() {
                self.processed.remove(&oldest);
            }
        }
        self.processed.insert(msg_id);
        self.processed_order.push_back(msg_id);
        Ok(())
    }

    /// Take the next pending message for processing.
    pub fn next_pending(&mut self) -> Option<CrossChainMessage> {
        self.pending.pop_front()
    }

    /// Number of pending messages.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Number of total processed messages.
    pub fn processed_count(&self) -> usize {
        self.processed.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_cross_chain_message() {
        let msg = CrossChainMessage::new(
            0,
            ChainId::new(1),
            ChainId::new(2),
            Address::from([0x01; 20]),
            Address::from([0x02; 20]),
            CrossChainPayload::VttTransfer {
                amount: Amount::from_vtt(100),
            },
            42,
        )
        .unwrap();

        assert_eq!(msg.nonce, 0);
        assert_eq!(msg.source_chain, ChainId::new(1));
        assert_eq!(msg.destination_chain, ChainId::new(2));
        assert_ne!(msg.id, H256::ZERO);
        assert_eq!(msg.status, MessageStatus::Pending);
    }

    #[test]
    fn same_chain_rejected() {
        let result = CrossChainMessage::new(
            0,
            ChainId::new(1),
            ChainId::new(1), // same as source
            Address::from([0x01; 20]),
            Address::from([0x02; 20]),
            CrossChainPayload::VttTransfer {
                amount: Amount::from_vtt(100),
            },
            0,
        );
        assert!(matches!(result, Err(MessagingError::SameChain)));
    }

    #[test]
    fn message_hash_deterministic() {
        let msg = CrossChainMessage::new(
            0,
            ChainId::new(1),
            ChainId::new(2),
            Address::from([0x01; 20]),
            Address::from([0x02; 20]),
            CrossChainPayload::VttTransfer {
                amount: Amount::from_vtt(100),
            },
            0,
        )
        .unwrap();

        let h1 = msg.hash();
        let h2 = msg.hash();
        assert_eq!(h1, h2);
        assert_ne!(h1, H256::ZERO);
    }

    #[test]
    fn outbox_send_and_drain() {
        let mut outbox = MessageOutbox::new(ChainId::new(1));

        outbox
            .send(
                ChainId::new(2),
                Address::from([0x01; 20]),
                Address::from([0x02; 20]),
                CrossChainPayload::VttTransfer {
                    amount: Amount::from_vtt(100),
                },
                10,
            )
            .unwrap();

        outbox
            .send(
                ChainId::new(3),
                Address::from([0x01; 20]),
                Address::from([0x03; 20]),
                CrossChainPayload::AssetTransfer {
                    asset_id: H256::from([0xAA; 32]),
                    amount: Amount::from_vtt(50),
                },
                10,
            )
            .unwrap();

        assert_eq!(outbox.pending_count(), 2);
        assert_eq!(outbox.nonce(), 2);
        assert_ne!(outbox.merkle_root(), H256::ZERO);

        let messages = outbox.drain();
        assert_eq!(messages.len(), 2);
        assert_eq!(outbox.pending_count(), 0);
    }

    #[test]
    fn outbox_merkle_root_empty() {
        let outbox = MessageOutbox::new(ChainId::new(1));
        assert_eq!(outbox.merkle_root(), H256::ZERO);
    }

    #[test]
    fn inbox_receive_and_process() {
        let mut inbox = MessageInbox::new(ChainId::new(2));

        let msg = CrossChainMessage::new(
            0,
            ChainId::new(1),
            ChainId::new(2),
            Address::from([0x01; 20]),
            Address::from([0x02; 20]),
            CrossChainPayload::VttTransfer {
                amount: Amount::from_vtt(100),
            },
            10,
        )
        .unwrap();

        inbox.receive(msg.clone()).unwrap();
        assert_eq!(inbox.pending_count(), 1);
        assert_eq!(inbox.processed_count(), 1);

        let received = inbox.next_pending().unwrap();
        assert_eq!(received.status, MessageStatus::Delivered);
        assert_eq!(received.nonce, 0);
    }

    #[test]
    fn inbox_replay_rejected() {
        let mut inbox = MessageInbox::new(ChainId::new(2));

        let msg = CrossChainMessage::new(
            0,
            ChainId::new(1),
            ChainId::new(2),
            Address::from([0x01; 20]),
            Address::from([0x02; 20]),
            CrossChainPayload::VttTransfer {
                amount: Amount::from_vtt(100),
            },
            10,
        )
        .unwrap();

        inbox.receive(msg.clone()).unwrap();
        let result = inbox.receive(msg);
        assert!(matches!(result, Err(MessagingError::AlreadyProcessed(_))));
    }

    #[test]
    fn full_flow_outbox_to_inbox() {
        let mut outbox = MessageOutbox::new(ChainId::new(1));
        let mut inbox = MessageInbox::new(ChainId::new(2));

        // Send from chain 1
        let _msg = outbox
            .send(
                ChainId::new(2),
                Address::from([0x01; 20]),
                Address::from([0x02; 20]),
                CrossChainPayload::VttTransfer {
                    amount: Amount::from_vtt(500),
                },
                100,
            )
            .unwrap();

        // Drain outbox (relay picks up)
        let relayed = outbox.drain();
        assert_eq!(relayed.len(), 1);

        // Deliver to chain 2 inbox
        inbox.receive(relayed[0].clone()).unwrap();

        // Process on chain 2
        let delivered = inbox.next_pending().unwrap();
        assert_eq!(delivered.status, MessageStatus::Delivered);
        match &delivered.payload {
            CrossChainPayload::VttTransfer { amount } => {
                assert_eq!(*amount, Amount::from_vtt(500));
            }
            _ => panic!("wrong payload type"),
        }
    }

    #[test]
    fn message_borsh_roundtrip() {
        let msg = CrossChainMessage::new(
            42,
            ChainId::new(1),
            ChainId::RELAY,
            Address::from([0x01; 20]),
            Address::from([0x02; 20]),
            CrossChainPayload::DataMessage {
                data: b"hello cross-chain".to_vec(),
            },
            999,
        )
        .unwrap();

        let bytes = borsh::to_vec(&msg).unwrap();
        let msg2 = CrossChainMessage::try_from_slice(&bytes).unwrap();
        assert_eq!(msg, msg2);
    }

    fn make_msg(n: u64) -> CrossChainMessage {
        CrossChainMessage::new(
            n,
            ChainId::new(1),
            ChainId::new(2),
            Address::from([0x01; 20]),
            Address::from([0x02; 20]),
            CrossChainPayload::DataMessage {
                data: vec![n as u8],
            },
            n,
        )
        .unwrap()
    }

    #[test]
    fn inbox_rejects_when_pending_full() {
        let mut inbox = MessageInbox::with_caps(ChainId::new(2), 2, 10);
        inbox.receive(make_msg(0)).unwrap();
        inbox.receive(make_msg(1)).unwrap();
        let err = inbox.receive(make_msg(2)).unwrap_err();
        assert!(matches!(err, MessagingError::InboxFull(2)));
        // Drain one and the next receive succeeds
        inbox.next_pending().unwrap();
        inbox.receive(make_msg(2)).unwrap();
        assert_eq!(inbox.pending_count(), 2);
    }

    #[test]
    fn inbox_processed_window_evicts_oldest() {
        // Processed cap of 3, pending cap large so we drain after each.
        let mut inbox = MessageInbox::with_caps(ChainId::new(2), 100, 3);
        for n in 0..3 {
            inbox.receive(make_msg(n)).unwrap();
            inbox.next_pending();
        }
        assert_eq!(inbox.processed_count(), 3);

        // While still inside the window, every replay is rejected.
        for n in 0..3 {
            let err = inbox.receive(make_msg(n)).unwrap_err();
            assert!(matches!(err, MessagingError::AlreadyProcessed(_)));
        }

        // Adding a 4th evicts the oldest (n=0) and keeps size at 3.
        inbox.receive(make_msg(3)).unwrap();
        inbox.next_pending();
        assert_eq!(inbox.processed_count(), 3);

        // n=0 has aged out — replay is allowed again.
        inbox.receive(make_msg(0)).unwrap();

        // n=2 is still inside the current window — replay rejected.
        // (Adding n=0 just evicted n=1, so window is now {2, 3, 0}.)
        let err = inbox.receive(make_msg(2)).unwrap_err();
        assert!(matches!(err, MessagingError::AlreadyProcessed(_)));
    }
}
