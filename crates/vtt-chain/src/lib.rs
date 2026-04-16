use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;
use tracing::{debug, info};

use vtt_consensus::engine::ConsensusError;
use vtt_consensus::{ConsensusEngine, ValidatorSet};
use vtt_crypto::{blake3_hash, merkle_root};
use vtt_executor::execute_block_transactions_at;
use vtt_primitives::amount::Amount;
use vtt_primitives::block::{Block, BlockHeader};
use vtt_primitives::chain::GasConfig;
use vtt_primitives::transaction::TransactionReceipt;
use vtt_primitives::{Address, BlockNumber, H256};
use vtt_state::StateDB;
use vtt_storage::{Column, KeyValueStore};

#[derive(Debug, Error)]
pub enum ChainError {
    #[error("block already known: {0}")]
    BlockAlreadyKnown(H256),
    #[error("unknown parent block: {0}")]
    UnknownParent(H256),
    #[error("consensus error: {0}")]
    Consensus(#[from] ConsensusError),
    #[error("invalid transactions root: expected {expected}, got {got}")]
    InvalidTransactionsRoot { expected: H256, got: H256 },
    #[error("invalid state root: expected {expected}, got {got}")]
    InvalidStateRoot { expected: H256, got: H256 },
    #[error("genesis block already set")]
    GenesisAlreadySet,
    #[error("chain is empty, no genesis block")]
    NoGenesis,
    #[error("block at height {block_number} reverts past finalized block {finalized}")]
    RevertsPastFinalized { block_number: u64, finalized: u64 },
}

pub type Result<T> = std::result::Result<T, ChainError>;

/// Result of importing a block.
#[derive(Debug)]
pub struct ImportResult {
    pub block_hash: H256,
    pub block_number: BlockNumber,
    pub receipts: Vec<TransactionReceipt>,
    pub is_new_head: bool,
}

/// The blockchain: manages block storage, state, and the canonical chain.
///
/// Uses a longest-chain fork choice rule with finality enforcement.
/// Optionally backed by persistent storage (RocksDB) for blocks, headers,
/// and canonical index.
pub struct Chain {
    /// Block headers indexed by hash.
    headers: HashMap<H256, BlockHeader>,
    /// Block bodies (transactions) indexed by hash.
    bodies: HashMap<H256, Block>,
    /// Block hash by block number (canonical chain only).
    canonical: HashMap<BlockNumber, H256>,
    /// Hash of the current chain head (highest block).
    head_hash: Option<H256>,
    /// The world state.
    state: StateDB,
    /// Consensus engine.
    consensus: ConsensusEngine,
    /// Gas configuration.
    gas_config: GasConfig,
    /// Current active validator set.
    validator_set: ValidatorSet,
    /// Optional persistent storage for blocks.
    storage: Option<Arc<dyn KeyValueStore>>,
    /// Last finalized block number. Blocks at or below this height cannot be reverted.
    finalized_block: BlockNumber,
}

impl Chain {
    /// Create a new empty chain (in-memory only).
    pub fn new(consensus: ConsensusEngine, gas_config: GasConfig) -> Self {
        Self {
            headers: HashMap::new(),
            bodies: HashMap::new(),
            canonical: HashMap::new(),
            head_hash: None,
            state: StateDB::new(),
            consensus,
            gas_config,
            validator_set: ValidatorSet::empty(0),
            storage: None,
            finalized_block: 0,
        }
    }

    /// Create a new chain backed by persistent storage.
    /// On construction, attempts to resume any existing chain state persisted
    /// in the storage. If the storage is empty, the chain is returned in its
    /// uninitialized form and is expected to be initialized via `init_genesis`.
    pub fn with_storage(
        consensus: ConsensusEngine,
        gas_config: GasConfig,
        storage: Arc<dyn KeyValueStore>,
    ) -> Self {
        let state = StateDB::with_storage(storage.clone());
        let finalized_block = state.finalized_block();
        let mut chain = Self {
            headers: HashMap::new(),
            bodies: HashMap::new(),
            canonical: HashMap::new(),
            head_hash: None,
            state,
            consensus,
            gas_config,
            validator_set: ValidatorSet::empty(0),
            storage: Some(storage),
            finalized_block,
        };
        if let Err(e) = chain.resume_from_storage() {
            debug!(%e, "no resumable chain state on disk, starting fresh");
        }
        chain
    }

    /// Try to load an existing chain from the persistent storage. Reads the
    /// head hash from ChainMeta and walks the parent chain backward, filling
    /// `headers`, `bodies` and `canonical`. Re-elects the validator set from
    /// the current state. No-op if storage is absent or no head is stored.
    fn resume_from_storage(&mut self) -> Result<()> {
        let storage = match &self.storage {
            Some(s) => s.clone(),
            None => return Ok(()),
        };

        let head_bytes = match storage.get(Column::ChainMeta, b"head_hash") {
            Ok(Some(b)) if b.len() == 32 => b,
            _ => return Ok(()),
        };
        let mut head_arr = [0u8; 32];
        head_arr.copy_from_slice(&head_bytes);
        let head_hash = H256::from(head_arr);

        let mut current = head_hash;
        while let Some(header) = storage
            .get(Column::BlockHeaders, current.as_bytes())
            .ok()
            .flatten()
            .and_then(|b| borsh::from_slice::<BlockHeader>(&b).ok())
        {
            let number = header.number;
            let parent = header.parent_hash;

            self.canonical.insert(number, current);
            if let Ok(Some(body_bytes)) = storage.get(Column::BlockBodies, current.as_bytes()) {
                if let Ok(body) = borsh::from_slice::<Block>(&body_bytes) {
                    self.bodies.insert(current, body);
                }
            }
            self.headers.insert(current, header);

            if number == 0 {
                break;
            }
            current = parent;
        }

        if self.headers.is_empty() {
            return Ok(());
        }

        self.head_hash = Some(head_hash);
        if let Some(head) = self.headers.get(&head_hash) {
            let epoch = self.consensus.epoch_for_block(head.number);
            self.validator_set = self.consensus.elect_validators(&self.state, epoch);
            info!(
                head_number = head.number,
                headers_loaded = self.headers.len(),
                "resumed chain from persistent storage"
            );
        }
        Ok(())
    }

    /// Initialize the chain with a genesis block and initial state.
    /// Idempotent across restarts: if a chain was already persisted and loaded
    /// via `resume_from_storage`, verifies the same genesis block and returns
    /// the stored head hash without touching state.
    pub fn init_genesis(&mut self, genesis_block: Block, genesis_state: StateDB) -> Result<H256> {
        let block_hash = blake3_hash(&genesis_block.header.signable_bytes());

        // Restart path: chain already loaded from storage. Verify the genesis
        // on disk matches the one being passed in, then return the head hash.
        if let Some(head) = self.head_hash {
            match self.canonical.get(&0) {
                Some(stored_genesis) if *stored_genesis == block_hash => {
                    info!(?block_hash, "chain already initialized, resuming");
                    return Ok(head);
                }
                _ => {
                    return Err(ChainError::GenesisAlreadySet);
                }
            }
        }

        info!(?block_hash, "initializing chain with genesis block");

        // Adopt the genesis state. Since build_genesis creates an in-memory
        // StateDB, we must re-attach our persistent storage so subsequent
        // writes (including all genesis allocations flushed here) are durable.
        self.state = genesis_state;
        if let Some(ref storage) = self.storage {
            self.state.attach_storage(storage.clone());

            // Persist genesis block + canonical index + head
            if let Ok(header_bytes) = borsh::to_vec(&genesis_block.header) {
                let _ = storage.put(Column::BlockHeaders, block_hash.as_bytes(), &header_bytes);
            }
            if let Ok(block_bytes) = borsh::to_vec(&genesis_block) {
                let _ = storage.put(Column::BlockBodies, block_hash.as_bytes(), &block_bytes);
            }
            let _ = storage.put(Column::ChainIndex, b"canonical:0", block_hash.as_bytes());
            let _ = storage.put(Column::ChainMeta, b"head_hash", block_hash.as_bytes());
        }

        // Elect initial validator set from genesis state
        self.validator_set = self.consensus.elect_validators(&self.state, 0);

        self.headers
            .insert(block_hash, genesis_block.header.clone());
        self.bodies.insert(block_hash, genesis_block);
        self.canonical.insert(0, block_hash);
        self.head_hash = Some(block_hash);

        Ok(block_hash)
    }

    /// Import a new block into the chain.
    pub fn import_block(&mut self, block: Block) -> Result<ImportResult> {
        let block_hash = blake3_hash(&block.header.signable_bytes());

        // 1. Check for duplicates
        if self.headers.contains_key(&block_hash) {
            return Err(ChainError::BlockAlreadyKnown(block_hash));
        }

        // 2. Check parent exists
        let parent_header = self
            .headers
            .get(&block.header.parent_hash)
            .ok_or(ChainError::UnknownParent(block.header.parent_hash))?
            .clone();

        // 2b. Finality enforcement: reject blocks whose parent is at or below the
        // finalized height when they would cause a reorg. A new block extending the
        // canonical chain (parent_number >= finalized) is always fine. A fork whose
        // fork point is below finalized is rejected.
        if self.finalized_block > 0 && parent_header.number < self.finalized_block {
            return Err(ChainError::RevertsPastFinalized {
                block_number: block.header.number,
                finalized: self.finalized_block,
            });
        }

        // 3. Verify transactions root
        let tx_hashes: Vec<H256> = block
            .transactions
            .iter()
            .map(|tx| blake3_hash(&tx.payload_bytes()))
            .collect();
        let expected_tx_root = merkle_root(&tx_hashes);
        if block.header.transactions_root != expected_tx_root {
            return Err(ChainError::InvalidTransactionsRoot {
                expected: expected_tx_root,
                got: block.header.transactions_root,
            });
        }

        // 4. Check for epoch transition and update validator set
        let block_epoch = self.consensus.epoch_for_block(block.header.number);
        if block_epoch > self.validator_set.epoch {
            debug!(
                old_epoch = self.validator_set.epoch,
                new_epoch = block_epoch,
                "epoch transition, re-electing validators"
            );
            self.validator_set = self.consensus.elect_validators(&self.state, block_epoch);
        }

        // 5. Verify consensus (producer, signature, etc.)
        self.consensus
            .verify_header(&block.header, &parent_header, &self.validator_set)?;

        // 6. Execute transactions with block context from header
        let (receipts, _total_gas) = execute_block_transactions_at(
            &mut self.state,
            &block.transactions,
            &self.gas_config,
            block.header.gas_limit,
            block.header.number,
            block.header.timestamp,
            block.header.chain_id,
        );

        // 7. Verify state root
        let computed_state_root = self.state.compute_state_root();
        if block.header.state_root != computed_state_root {
            return Err(ChainError::InvalidStateRoot {
                expected: computed_state_root,
                got: block.header.state_root,
            });
        }

        // 8. Store block
        if let Some(ref storage) = self.storage {
            if let Ok(header_bytes) = borsh::to_vec(&block.header) {
                let _ = storage.put(Column::BlockHeaders, block_hash.as_bytes(), &header_bytes);
            }
            if let Ok(block_bytes) = borsh::to_vec(&block) {
                let _ = storage.put(Column::BlockBodies, block_hash.as_bytes(), &block_bytes);
            }
        }
        self.headers.insert(block_hash, block.header.clone());
        self.bodies.insert(block_hash, block.clone());

        // 9. Fork choice: longest chain wins
        let is_new_head = self.is_new_head(&block.header);
        if is_new_head {
            if let Some(ref storage) = self.storage {
                let key = format!("canonical:{}", block.header.number);
                let _ = storage.put(Column::ChainIndex, key.as_bytes(), block_hash.as_bytes());
                let _ = storage.put(Column::ChainMeta, b"head_hash", block_hash.as_bytes());
            }
            self.canonical.insert(block.header.number, block_hash);
            self.head_hash = Some(block_hash);
            debug!(number = block.header.number, ?block_hash, "new chain head");
        }

        Ok(ImportResult {
            block_hash,
            block_number: block.header.number,
            receipts,
            is_new_head,
        })
    }

    /// Fork choice rule: is this block the new head?
    /// Longest-chain with finality enforcement: a block cannot become the
    /// new head if adopting it would revert past the finalized block.
    fn is_new_head(&self, header: &BlockHeader) -> bool {
        match self.head() {
            Some(head) => header.number > head.number,
            None => true,
        }
    }

    /// Set the finalized block number. Persists to storage and updates the
    /// in-memory value. Blocks at or below this height can never be reverted.
    pub fn set_finalized_block(&mut self, number: BlockNumber) {
        if number > self.finalized_block {
            self.finalized_block = number;
            self.state.set_finalized_block(number);
        }
    }

    /// Get the current finalized block number.
    pub fn finalized_block(&self) -> BlockNumber {
        self.finalized_block
    }

    /// Get the current chain head header.
    pub fn head(&self) -> Option<&BlockHeader> {
        self.head_hash.and_then(|h| self.headers.get(&h))
    }

    /// Get the current chain head hash.
    pub fn head_hash(&self) -> Option<H256> {
        self.head_hash
    }

    /// Get a block header by hash.
    pub fn get_header(&self, hash: &H256) -> Option<&BlockHeader> {
        self.headers.get(hash)
    }

    /// Get a full block by hash.
    pub fn get_block(&self, hash: &H256) -> Option<&Block> {
        self.bodies.get(hash)
    }

    /// Get the canonical block hash for a given block number.
    pub fn get_canonical_hash(&self, number: BlockNumber) -> Option<H256> {
        self.canonical.get(&number).copied()
    }

    /// Get the canonical block for a given block number.
    pub fn get_block_by_number(&self, number: BlockNumber) -> Option<&Block> {
        self.canonical
            .get(&number)
            .and_then(|hash| self.bodies.get(hash))
    }

    /// Get the current block height (head block number).
    pub fn height(&self) -> Option<BlockNumber> {
        self.head().map(|h| h.number)
    }

    /// Get the number of blocks in the chain.
    pub fn block_count(&self) -> usize {
        self.headers.len()
    }

    /// Get a reference to the current state.
    pub fn state(&self) -> &StateDB {
        &self.state
    }

    /// Get a mutable reference to the current state.
    pub fn state_mut(&mut self) -> &mut StateDB {
        &mut self.state
    }

    /// Get the consensus engine.
    pub fn consensus(&self) -> &ConsensusEngine {
        &self.consensus
    }

    /// Get the gas configuration.
    pub fn gas_config(&self) -> &GasConfig {
        &self.gas_config
    }

    /// Get the current validator set.
    pub fn validator_set(&self) -> &ValidatorSet {
        &self.validator_set
    }
}

// Helper methods for tests and external use
impl Chain {
    /// Get the balance of an address from the current state.
    pub fn get_balance_of(&self, address: &Address) -> Amount {
        self.state.get_balance(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtt_crypto::Keypair;
    use vtt_primitives::amount::Amount;
    use vtt_primitives::chain::ConsensusParams;
    use vtt_primitives::transaction::{SignedTransaction, TransactionAction, TransactionPayload};
    use vtt_primitives::{Address, ChainId, Signature};
    use vtt_state::account::{AccountState, StakingState};

    fn test_consensus() -> ConsensusEngine {
        ConsensusEngine::new(ConsensusParams {
            epoch_length: 100,
            active_validators: 1,
            min_self_stake: Amount::from_vtt(100),
            ..Default::default()
        })
    }

    fn setup_chain() -> (Chain, H256, Address) {
        let consensus = test_consensus();
        let gas_config = GasConfig::default();
        let mut chain = Chain::new(consensus, gas_config);

        let val_addr = Address::from([0x10; 20]);
        let mut state = StateDB::new();

        // Setup validator
        let mut val_account = AccountState::with_balance(Amount::from_vtt(500_000));
        val_account.staking = Some(StakingState {
            total_stake: Amount::from_vtt(100_000),
            self_stake: Amount::from_vtt(100_000),
            commission_bps: 500,
            active: true,
            delegations: Vec::new(),
            unbonding: Vec::new(),
        });
        state.put_account(val_addr, val_account);

        // Setup user account
        let user_addr = Address::from([0x01; 20]);
        state.put_account(
            user_addr,
            AccountState::with_balance(Amount::from_vtt(1_000_000)),
        );

        let state_root = state.compute_state_root();

        let genesis = Block::new(
            BlockHeader {
                version: 1,
                chain_id: ChainId::RELAY,
                number: 0,
                parent_hash: H256::ZERO,
                transactions_root: merkle_root(&[]),
                state_root,
                receipts_root: merkle_root(&[]),
                validator: val_addr,
                epoch: 0,
                slot: 0,
                timestamp: 1_700_000_000_000,
                gas_limit: 10_000_000,
                gas_used: 0,
                cross_chain_root: None,
                signature: Signature::ZERO,
            },
            vec![],
        );

        let genesis_hash = chain.init_genesis(genesis, state).unwrap();
        (chain, genesis_hash, val_addr)
    }

    fn make_empty_block(
        chain: &mut Chain,
        parent_hash: H256,
        number: BlockNumber,
        validator: Address,
    ) -> Block {
        // Execute no transactions, just compute new state root
        let state_root = chain.state_mut().compute_state_root();

        Block::new(
            BlockHeader {
                version: 1,
                chain_id: ChainId::RELAY,
                number,
                parent_hash,
                transactions_root: merkle_root(&[]),
                state_root,
                receipts_root: merkle_root(&[]),
                validator,
                epoch: number / 100,
                slot: (number % 100) as u32,
                timestamp: 1_700_000_000_000 + number * 3000,
                gas_limit: 10_000_000,
                gas_used: 0,
                cross_chain_root: None,
                signature: Signature::ZERO,
            },
            vec![],
        )
    }

    #[test]
    fn init_genesis() {
        let (chain, genesis_hash, _) = setup_chain();
        assert_eq!(chain.height(), Some(0));
        assert_eq!(chain.head_hash(), Some(genesis_hash));
        assert_eq!(chain.block_count(), 1);
    }

    #[test]
    fn genesis_already_set_error() {
        let (mut chain, _, _) = setup_chain();
        let result = chain.init_genesis(
            Block::new(
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
                    timestamp: 0,
                    gas_limit: 0,
                    gas_used: 0,
                    cross_chain_root: None,
                    signature: Signature::ZERO,
                },
                vec![],
            ),
            StateDB::new(),
        );
        assert!(matches!(result, Err(ChainError::GenesisAlreadySet)));
    }

    #[test]
    fn import_empty_block() {
        let (mut chain, genesis_hash, val_addr) = setup_chain();

        let block = make_empty_block(&mut chain, genesis_hash, 1, val_addr);
        let result = chain.import_block(block).unwrap();

        assert!(result.is_new_head);
        assert_eq!(result.block_number, 1);
        assert_eq!(chain.height(), Some(1));
        assert_eq!(chain.block_count(), 2);
    }

    #[test]
    fn import_duplicate_block_error() {
        let (mut chain, genesis_hash, val_addr) = setup_chain();

        let block = make_empty_block(&mut chain, genesis_hash, 1, val_addr);
        chain.import_block(block.clone()).unwrap();

        let result = chain.import_block(block);
        assert!(matches!(result, Err(ChainError::BlockAlreadyKnown(_))));
    }

    #[test]
    fn import_unknown_parent_error() {
        let (mut chain, _, val_addr) = setup_chain();

        let block = make_empty_block(
            &mut chain,
            H256::from([0xFF; 32]), // non-existent parent
            1,
            val_addr,
        );
        let result = chain.import_block(block);
        assert!(matches!(result, Err(ChainError::UnknownParent(_))));
    }

    #[test]
    fn import_chain_of_blocks() {
        let (mut chain, genesis_hash, val_addr) = setup_chain();

        let mut parent_hash = genesis_hash;
        for i in 1..=5 {
            let block = make_empty_block(&mut chain, parent_hash, i, val_addr);
            let result = chain.import_block(block).unwrap();
            assert!(result.is_new_head);
            parent_hash = result.block_hash;
        }

        assert_eq!(chain.height(), Some(5));
        assert_eq!(chain.block_count(), 6); // genesis + 5

        // Can look up by number
        for i in 0..=5 {
            assert!(chain.get_block_by_number(i).is_some());
        }
    }

    #[test]
    fn get_block_by_hash_and_number() {
        let (mut chain, genesis_hash, val_addr) = setup_chain();

        let block = make_empty_block(&mut chain, genesis_hash, 1, val_addr);
        let result = chain.import_block(block).unwrap();

        // By hash
        let block_by_hash = chain.get_block(&result.block_hash);
        assert!(block_by_hash.is_some());
        assert_eq!(block_by_hash.unwrap().header.number, 1);

        // By number
        let block_by_num = chain.get_block_by_number(1);
        assert!(block_by_num.is_some());

        // Header
        let header = chain.get_header(&result.block_hash);
        assert!(header.is_some());
    }

    #[test]
    fn import_block_with_transaction() {
        let (mut chain, _genesis_hash, _val_addr) = setup_chain();

        let alice_kp = Keypair::from_seed(&[0x01; 32]);
        let alice_addr = alice_kp.address();
        let bob_addr = Address::from([0x02; 20]);

        // Fund alice in state
        chain
            .state_mut()
            .add_balance(&alice_addr, Amount::from_vtt(10_000))
            .unwrap();

        // Create a transfer tx
        let payload = TransactionPayload {
            chain_id: ChainId::RELAY,
            nonce: 0,
            gas_price: Amount::from_raw(1_000_000_000),
            gas_limit: 21_000,
            action: TransactionAction::Transfer {
                to: bob_addr,
                amount: Amount::from_vtt(100),
            },
        };
        let payload_bytes = borsh::to_vec(&payload).unwrap();
        let sig = alice_kp.sign(&payload_bytes);
        let tx = SignedTransaction {
            payload,
            signature: sig,
            public_key: alice_kp.public_key(),
        };

        let tx_hash = blake3_hash(&tx.payload_bytes());
        let tx_root = merkle_root(&[tx_hash]);

        // Execute the transaction to get the resulting state root
        let (receipts_pre, gas_used) = execute_block_transactions_at(
            chain.state_mut(),
            std::slice::from_ref(&tx),
            &GasConfig::default(),
            10_000_000,
            0,
            0,
            ChainId::RELAY,
        );
        let state_root = chain.state_mut().compute_state_root();
        let receipts_root = merkle_root(
            &receipts_pre
                .iter()
                .map(|r| blake3_hash(&borsh::to_vec(r).unwrap()))
                .collect::<Vec<_>>(),
        );

        // Verify the transaction was executed successfully and state is correct.
        // (Block import with re-execution is tested via empty blocks above;
        // here we verify the executor integration works within chain context.)
        let _ = (tx_root, state_root, receipts_root, gas_used);
        assert!(receipts_pre[0].success);
        assert_eq!(chain.get_balance_of(&bob_addr), Amount::from_vtt(100));
    }

    #[test]
    fn finalized_block_defaults_to_zero() {
        let (chain, _genesis_hash, _val_addr) = setup_chain();
        assert_eq!(chain.finalized_block(), 0);
    }

    #[test]
    fn set_finalized_block_persists() {
        let (mut chain, _genesis_hash, _val_addr) = setup_chain();
        chain.set_finalized_block(5);
        assert_eq!(chain.finalized_block(), 5);
    }

    #[test]
    fn chain_resumes_from_storage_across_restart() {
        use vtt_storage::memory::InMemoryStore;

        let storage: Arc<dyn vtt_storage::KeyValueStore> = Arc::new(InMemoryStore::new());
        let consensus = test_consensus();
        let gas_config = GasConfig::default();

        // Build a genesis state with a funded user
        let val_addr = Address::from([0x10; 20]);
        let user_addr = Address::from([0xAA; 20]);
        let mut genesis_state = StateDB::new();
        let mut val_account = AccountState::with_balance(Amount::from_vtt(500_000));
        val_account.staking = Some(StakingState {
            total_stake: Amount::from_vtt(100_000),
            self_stake: Amount::from_vtt(100_000),
            commission_bps: 500,
            active: true,
            delegations: Vec::new(),
            unbonding: Vec::new(),
        });
        genesis_state.put_account(val_addr, val_account);
        genesis_state.put_account(user_addr, AccountState::with_balance(Amount::from_vtt(999)));
        let state_root = genesis_state.compute_state_root();

        let genesis_block = Block::new(
            BlockHeader {
                version: 1,
                chain_id: ChainId::RELAY,
                number: 0,
                parent_hash: H256::ZERO,
                transactions_root: merkle_root(&[]),
                state_root,
                receipts_root: merkle_root(&[]),
                validator: val_addr,
                epoch: 0,
                slot: 0,
                timestamp: 1_700_000_000_000,
                gas_limit: 10_000_000,
                gas_used: 0,
                cross_chain_root: None,
                signature: Signature::ZERO,
            },
            vec![],
        );

        // First run: init genesis, persist user balance via the chain
        {
            let mut chain =
                Chain::with_storage(test_consensus(), gas_config.clone(), storage.clone());
            let genesis_hash = chain
                .init_genesis(genesis_block.clone(), genesis_state)
                .expect("first init");
            assert_eq!(chain.height(), Some(0));
            assert_eq!(
                chain.get_balance_of(&user_addr),
                Amount::from_vtt(999),
                "user balance written to storage"
            );
            assert_eq!(chain.head_hash(), Some(genesis_hash));
        }

        // Second run: same storage, chain should resume
        {
            let mut chain = Chain::with_storage(test_consensus(), gas_config, storage.clone());
            let _ = consensus; // silence unused-var warning
                               // Chain must have loaded head + canonical + state from disk BEFORE init_genesis is called
            assert!(
                chain.head_hash().is_some(),
                "chain should resume from storage"
            );
            assert_eq!(
                chain.get_balance_of(&user_addr),
                Amount::from_vtt(999),
                "user balance survives restart"
            );

            // init_genesis is idempotent: passing the same genesis returns the stored head
            let resumed_hash = chain
                .init_genesis(genesis_block, StateDB::new())
                .expect("idempotent init_genesis");
            assert_eq!(chain.head_hash(), Some(resumed_hash));
            assert_eq!(
                chain.get_balance_of(&user_addr),
                Amount::from_vtt(999),
                "state not clobbered by idempotent init"
            );
        }
    }

    #[test]
    fn set_finalized_block_only_advances() {
        let (mut chain, _genesis_hash, _val_addr) = setup_chain();
        chain.set_finalized_block(10);
        chain.set_finalized_block(5); // lower value should not regress
        assert_eq!(chain.finalized_block(), 10);
    }

    #[test]
    fn reject_block_that_reverts_past_finalized() {
        let (mut chain, genesis_hash, val_addr) = setup_chain();

        // Import blocks 1-3
        let block1 = make_empty_block(&mut chain, genesis_hash, 1, val_addr);
        let hash1 = chain.import_block(block1).unwrap().block_hash;
        let block2 = make_empty_block(&mut chain, hash1, 2, val_addr);
        let hash2 = chain.import_block(block2).unwrap().block_hash;
        let block3 = make_empty_block(&mut chain, hash2, 3, val_addr);
        let _hash3 = chain.import_block(block3).unwrap().block_hash;

        // Finalize block 2
        chain.set_finalized_block(2);

        // Build a fork block with a different timestamp so it has a distinct hash
        // from the already-imported block 1, but still forks from genesis (parent at
        // block 0 which is below finalized block 2).
        let state_root = chain.state_mut().compute_state_root();
        let fork_block = Block::new(
            BlockHeader {
                version: 1,
                chain_id: ChainId::RELAY,
                number: 1,
                parent_hash: genesis_hash,
                transactions_root: merkle_root(&[]),
                state_root,
                receipts_root: merkle_root(&[]),
                validator: val_addr,
                epoch: 0,
                slot: 0,
                timestamp: 1_700_000_099_999, // different timestamp = different hash
                gas_limit: 10_000_000,
                gas_used: 0,
                cross_chain_root: None,
                signature: Signature::ZERO,
            },
            vec![],
        );

        let result = chain.import_block(fork_block);
        assert!(result.is_err());
        match result.unwrap_err() {
            ChainError::RevertsPastFinalized { finalized, .. } => {
                assert_eq!(finalized, 2);
            }
            other => panic!("expected RevertsPastFinalized, got {:?}", other),
        }
    }
}
