use std::collections::{BTreeMap, HashMap, HashSet};

use thiserror::Error;
use tracing::debug;

use vtt_crypto::blake3_hash;
use vtt_primitives::amount::Amount;
use vtt_primitives::transaction::SignedTransaction;
use vtt_primitives::{Address, H256};

#[derive(Debug, Error)]
pub enum TxPoolError {
    #[error("transaction already exists: {0}")]
    AlreadyExists(H256),
    #[error("pool is full (max {max} transactions)")]
    PoolFull { max: usize },
    #[error("gas price {got} below minimum {min}")]
    GasPriceTooLow { got: Amount, min: Amount },
    #[error("nonce too low: account nonce is {current}, tx nonce is {got}")]
    NonceTooLow { current: u64, got: u64 },
}

pub type Result<T> = std::result::Result<T, TxPoolError>;

/// Configuration for the transaction pool.
#[derive(Clone, Debug)]
pub struct TxPoolConfig {
    /// Maximum number of transactions in the pool.
    pub max_size: usize,
    /// Maximum number of pending transactions per account.
    pub max_per_account: usize,
    /// Minimum gas price to accept a transaction.
    pub min_gas_price: Amount,
}

impl Default for TxPoolConfig {
    fn default() -> Self {
        Self {
            max_size: 10_000,
            max_per_account: 100,
            min_gas_price: Amount::from_raw(1_000_000_000), // 1 gwei
        }
    }
}

/// Transaction pool (mempool) for pending transactions.
///
/// Transactions are organized by sender address and ordered by nonce.
/// When selecting transactions for block production, they are ordered by
/// gas price (highest first), then by nonce within each sender.
pub struct TxPool {
    config: TxPoolConfig,
    /// All transactions indexed by hash.
    by_hash: HashMap<H256, SignedTransaction>,
    /// Transactions grouped by sender, ordered by nonce.
    by_sender: HashMap<Address, BTreeMap<u64, H256>>,
    /// Transaction hashes for deduplication.
    known: HashSet<H256>,
}

impl TxPool {
    pub fn new(config: TxPoolConfig) -> Self {
        Self {
            config,
            by_hash: HashMap::new(),
            by_sender: HashMap::new(),
            known: HashSet::new(),
        }
    }

    /// Add a transaction to the pool.
    /// `account_nonce` is the current nonce of the sender on-chain.
    pub fn add(
        &mut self,
        tx: SignedTransaction,
        sender: Address,
        account_nonce: u64,
    ) -> Result<H256> {
        let tx_hash = self.tx_hash(&tx);

        // Check for duplicates
        if self.known.contains(&tx_hash) {
            return Err(TxPoolError::AlreadyExists(tx_hash));
        }

        // Check pool capacity
        if self.by_hash.len() >= self.config.max_size {
            return Err(TxPoolError::PoolFull {
                max: self.config.max_size,
            });
        }

        // Check minimum gas price
        if tx.payload.gas_price < self.config.min_gas_price {
            return Err(TxPoolError::GasPriceTooLow {
                got: tx.payload.gas_price,
                min: self.config.min_gas_price,
            });
        }

        // Check nonce isn't too low
        if tx.payload.nonce < account_nonce {
            return Err(TxPoolError::NonceTooLow {
                current: account_nonce,
                got: tx.payload.nonce,
            });
        }

        // Check per-account limit
        let sender_txs = self.by_sender.entry(sender).or_default();
        if sender_txs.len() >= self.config.max_per_account {
            return Err(TxPoolError::PoolFull {
                max: self.config.max_per_account,
            });
        }

        debug!(
            ?tx_hash,
            ?sender,
            nonce = tx.payload.nonce,
            "adding tx to pool"
        );

        sender_txs.insert(tx.payload.nonce, tx_hash);
        self.by_hash.insert(tx_hash, tx);
        self.known.insert(tx_hash);

        Ok(tx_hash)
    }

    /// Remove a transaction by hash.
    pub fn remove(&mut self, tx_hash: &H256) -> Option<SignedTransaction> {
        if let Some(tx) = self.by_hash.remove(tx_hash) {
            let sender = vtt_crypto::address_from_public_key(&tx.public_key);
            if let Some(sender_txs) = self.by_sender.get_mut(&sender) {
                sender_txs.remove(&tx.payload.nonce);
                if sender_txs.is_empty() {
                    self.by_sender.remove(&sender);
                }
            }
            // Keep in `known` to prevent re-adding
            Some(tx)
        } else {
            None
        }
    }

    /// Remove all transactions for a given sender with nonce <= the given nonce.
    /// Called after a block is committed to clear executed transactions.
    pub fn remove_committed(&mut self, sender: &Address, committed_nonce: u64) {
        if let Some(sender_txs) = self.by_sender.get_mut(sender) {
            let to_remove: Vec<u64> = sender_txs
                .range(..=committed_nonce)
                .map(|(nonce, _)| *nonce)
                .collect();

            for nonce in to_remove {
                if let Some(tx_hash) = sender_txs.remove(&nonce) {
                    self.by_hash.remove(&tx_hash);
                }
            }

            if sender_txs.is_empty() {
                self.by_sender.remove(sender);
            }
        }
    }

    /// Get a transaction by hash.
    pub fn get(&self, tx_hash: &H256) -> Option<&SignedTransaction> {
        self.by_hash.get(tx_hash)
    }

    /// Check if the pool contains a transaction.
    pub fn contains(&self, tx_hash: &H256) -> bool {
        self.by_hash.contains_key(tx_hash)
    }

    /// Select transactions for block production.
    /// Returns transactions ordered by gas price (descending), then nonce (ascending).
    /// Only includes transactions with consecutive nonces starting from `account_nonce`.
    pub fn select_transactions(
        &self,
        max_count: usize,
        account_nonces: &HashMap<Address, u64>,
    ) -> Vec<SignedTransaction> {
        // Collect executable transactions (those with the right next nonce)
        let mut candidates: Vec<&SignedTransaction> = Vec::new();

        for (sender, sender_txs) in &self.by_sender {
            let start_nonce = account_nonces.get(sender).copied().unwrap_or(0);
            let mut expected_nonce = start_nonce;

            for (nonce, tx_hash) in sender_txs {
                if *nonce != expected_nonce {
                    break; // Gap in nonces, stop
                }
                if let Some(tx) = self.by_hash.get(tx_hash) {
                    candidates.push(tx);
                }
                expected_nonce += 1;
            }
        }

        // Sort by gas price descending (higher gas price = higher priority)
        candidates.sort_by(|a, b| b.payload.gas_price.cmp(&a.payload.gas_price));

        candidates.into_iter().take(max_count).cloned().collect()
    }

    /// Number of transactions in the pool.
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    /// Number of unique senders with pending transactions.
    pub fn sender_count(&self) -> usize {
        self.by_sender.len()
    }

    fn tx_hash(&self, tx: &SignedTransaction) -> H256 {
        blake3_hash(&borsh::to_vec(&tx.payload).unwrap())
    }
}

impl Default for TxPool {
    fn default() -> Self {
        Self::new(TxPoolConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtt_crypto::Keypair;
    use vtt_primitives::transaction::{TransactionAction, TransactionPayload};
    use vtt_primitives::ChainId;

    fn make_tx(keypair: &Keypair, nonce: u64, gas_price: u128) -> SignedTransaction {
        let payload = TransactionPayload {
            chain_id: ChainId::RELAY,
            nonce,
            gas_price: Amount::from_raw(gas_price),
            gas_limit: 21_000,
            action: TransactionAction::Transfer {
                to: Address::from([0xFF; 20]),
                amount: Amount::from_vtt(1),
            },
        };
        let bytes = borsh::to_vec(&payload).unwrap();
        SignedTransaction {
            payload,
            signature: keypair.sign(&bytes),
            public_key: keypair.public_key(),
        }
    }

    #[test]
    fn add_and_get_transaction() {
        let mut pool = TxPool::default();
        let kp = Keypair::from_seed(&[1u8; 32]);
        let sender = kp.address();
        let tx = make_tx(&kp, 0, 2_000_000_000);

        let hash = pool.add(tx.clone(), sender, 0).unwrap();
        assert!(pool.contains(&hash));
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.get(&hash).unwrap().payload.nonce, 0);
    }

    #[test]
    fn reject_duplicate() {
        let mut pool = TxPool::default();
        let kp = Keypair::from_seed(&[1u8; 32]);
        let sender = kp.address();
        let tx = make_tx(&kp, 0, 2_000_000_000);

        pool.add(tx.clone(), sender, 0).unwrap();
        let err = pool.add(tx, sender, 0);
        assert!(matches!(err, Err(TxPoolError::AlreadyExists(_))));
    }

    #[test]
    fn reject_low_gas_price() {
        let mut pool = TxPool::default();
        let kp = Keypair::from_seed(&[1u8; 32]);
        let sender = kp.address();
        let tx = make_tx(&kp, 0, 100); // way below minimum

        let err = pool.add(tx, sender, 0);
        assert!(matches!(err, Err(TxPoolError::GasPriceTooLow { .. })));
    }

    #[test]
    fn reject_nonce_too_low() {
        let mut pool = TxPool::default();
        let kp = Keypair::from_seed(&[1u8; 32]);
        let sender = kp.address();
        let tx = make_tx(&kp, 0, 2_000_000_000);

        let err = pool.add(tx, sender, 5); // account nonce is 5, tx nonce is 0
        assert!(matches!(err, Err(TxPoolError::NonceTooLow { .. })));
    }

    #[test]
    fn remove_transaction() {
        let mut pool = TxPool::default();
        let kp = Keypair::from_seed(&[1u8; 32]);
        let sender = kp.address();
        let tx = make_tx(&kp, 0, 2_000_000_000);

        let hash = pool.add(tx, sender, 0).unwrap();
        let removed = pool.remove(&hash);
        assert!(removed.is_some());
        assert!(!pool.contains(&hash));
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn remove_committed_transactions() {
        let mut pool = TxPool::default();
        let kp = Keypair::from_seed(&[1u8; 32]);
        let sender = kp.address();

        let tx0 = make_tx(&kp, 0, 2_000_000_000);
        let tx1 = make_tx(&kp, 1, 2_000_000_000);
        let tx2 = make_tx(&kp, 2, 2_000_000_000);

        pool.add(tx0, sender, 0).unwrap();
        let h1 = pool.add(tx1, sender, 0).unwrap();
        let h2 = pool.add(tx2, sender, 0).unwrap();
        assert_eq!(pool.len(), 3);

        // Commit nonce 0 (remove tx with nonce 0)
        pool.remove_committed(&sender, 0);
        assert_eq!(pool.len(), 2);
        assert!(pool.contains(&h1));
        assert!(pool.contains(&h2));
    }

    #[test]
    fn select_transactions_by_gas_price() {
        let mut pool = TxPool::default();
        let kp1 = Keypair::from_seed(&[1u8; 32]);
        let kp2 = Keypair::from_seed(&[2u8; 32]);

        let tx1 = make_tx(&kp1, 0, 1_000_000_000); // low gas price
        let tx2 = make_tx(&kp2, 0, 5_000_000_000); // high gas price

        pool.add(tx1, kp1.address(), 0).unwrap();
        pool.add(tx2, kp2.address(), 0).unwrap();

        let nonces = HashMap::from([(kp1.address(), 0u64), (kp2.address(), 0u64)]);
        let selected = pool.select_transactions(10, &nonces);

        assert_eq!(selected.len(), 2);
        // Higher gas price should come first
        assert!(selected[0].payload.gas_price > selected[1].payload.gas_price);
    }

    #[test]
    fn select_skips_nonce_gaps() {
        let mut pool = TxPool::default();
        let kp = Keypair::from_seed(&[1u8; 32]);
        let sender = kp.address();

        let tx0 = make_tx(&kp, 0, 2_000_000_000);
        let tx2 = make_tx(&kp, 2, 2_000_000_000); // gap: nonce 1 missing

        pool.add(tx0, sender, 0).unwrap();
        pool.add(tx2, sender, 0).unwrap();

        let nonces = HashMap::from([(sender, 0u64)]);
        let selected = pool.select_transactions(10, &nonces);

        // Only tx0 should be selected (tx2 has a gap)
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].payload.nonce, 0);
    }

    #[test]
    fn pool_full_rejection() {
        let config = TxPoolConfig {
            max_size: 2,
            ..Default::default()
        };
        let mut pool = TxPool::new(config);
        let kp = Keypair::from_seed(&[1u8; 32]);
        let sender = kp.address();

        pool.add(make_tx(&kp, 0, 2_000_000_000), sender, 0).unwrap();
        pool.add(make_tx(&kp, 1, 2_000_000_000), sender, 0).unwrap();

        let err = pool.add(make_tx(&kp, 2, 2_000_000_000), sender, 0);
        assert!(matches!(err, Err(TxPoolError::PoolFull { .. })));
    }
}
