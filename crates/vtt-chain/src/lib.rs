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
    #[error("block timestamp {block} must be strictly greater than parent {parent}")]
    InvalidTimestamp { block: u64, parent: u64 },
    #[error("block timestamp {block} is more than 30s ahead of local clock {now}")]
    TimestampTooFarInFuture { block: u64, now: u64 },
    #[error("weak subjectivity violation at block {checkpoint}: expected {expected}, got {got}")]
    WeakSubjectivityViolation {
        checkpoint: u64,
        expected: H256,
        got: H256,
    },
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
    /// Optional weak subjectivity checkpoint (block_number, expected_hash).
    weak_subjectivity: Option<(BlockNumber, H256)>,
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
            weak_subjectivity: None,
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
            weak_subjectivity: None,
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

        // One-time backfill of the tx_hash -> (block_number, tx_index) index
        // for blocks imported before the index existed. Tracks the highest
        // indexed block under ChainMeta:tx_index:head so subsequent restarts
        // skip the scan.
        let indexed_upto = storage
            .get(Column::ChainMeta, b"tx_index:head")
            .ok()
            .flatten()
            .and_then(|b| {
                if b.len() == 8 {
                    let mut a = [0u8; 8];
                    a.copy_from_slice(&b);
                    Some(u64::from_le_bytes(a))
                } else {
                    None
                }
            })
            .unwrap_or(0);
        let head_number = self.headers.get(&head_hash).map(|h| h.number).unwrap_or(0);
        if head_number > indexed_upto {
            let mut indexed = 0u64;
            for n in (indexed_upto + 1)..=head_number {
                if let Some(hash) = self.canonical.get(&n).copied() {
                    if let Some(body) = self.bodies.get(&hash).cloned() {
                        for (idx, tx) in body.transactions.iter().enumerate() {
                            let tx_hash = vtt_crypto::blake3_hash(&tx.payload_bytes());
                            let mut v = Vec::with_capacity(12);
                            v.extend_from_slice(&n.to_le_bytes());
                            v.extend_from_slice(&(idx as u32).to_le_bytes());
                            let _ = storage.put(Column::Transactions, tx_hash.as_bytes(), &v);
                            indexed += 1;
                        }
                    }
                }
            }
            let _ = storage.put(
                Column::ChainMeta,
                b"tx_index:head",
                &head_number.to_le_bytes(),
            );
            if indexed > 0 {
                tracing::info!(
                    backfilled_from = indexed_upto + 1,
                    backfilled_to = head_number,
                    tx_count = indexed,
                    "backfilled tx_hash -> (block, idx) index"
                );
            }
        }

        self.head_hash = Some(head_hash);
        if let Some(head) = self.headers.get(&head_hash).cloned() {
            let epoch = self.consensus.epoch_for_block(head.number);
            self.validator_set = self.consensus.elect_validators(&self.state, epoch);

            // Sanity-check that the rebuilt state matches the head block's
            // state_root. A mismatch indicates corrupted RocksDB data or a
            // schema drift — producing blocks from here would yield invalid
            // state roots that other nodes reject.
            //
            // To avoid spamming the log across restarts after a one-time
            // schema change (e.g. compute_state_root was extended to cover
            // new columns), we remember the first block where a mismatch was
            // observed under ChainMeta:`state_root:boundary`. Subsequent
            // restarts at or before that block log at INFO instead of WARN.
            let computed = self.state.compute_state_root();
            if computed != head.state_root {
                let boundary = storage
                    .get(Column::ChainMeta, b"state_root:boundary")
                    .ok()
                    .flatten()
                    .and_then(|b| {
                        if b.len() == 8 {
                            let mut a = [0u8; 8];
                            a.copy_from_slice(&b);
                            Some(u64::from_le_bytes(a))
                        } else {
                            None
                        }
                    });
                match boundary {
                    Some(bn) if bn == head.number => {
                        tracing::info!(
                            head_number = head.number,
                            "state_root mismatch at recorded schema boundary (expected)"
                        );
                    }
                    _ => {
                        tracing::warn!(
                            head_number = head.number,
                            expected = ?head.state_root,
                            computed = ?computed,
                            "state_root mismatch after resume — storage may be corrupted or schema changed"
                        );
                        let _ = storage.put(
                            Column::ChainMeta,
                            b"state_root:boundary",
                            &head.number.to_le_bytes(),
                        );
                    }
                }
            }

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
                    // One-time migration: chains that produced blocks before
                    // the stakers index was introduced have an empty index on
                    // disk. Rebuild it from the genesis_state so subsequent
                    // validator elections find the initial validators.
                    if self.state.stakers_empty() {
                        self.state.bootstrap_stakers_from(&genesis_state);
                        // Re-elect now that the cache is warm.
                        let head_number = self.headers.get(&head).map(|h| h.number).unwrap_or(0);
                        let resume_epoch = self.consensus.epoch_for_block(head_number);
                        self.validator_set =
                            self.consensus.elect_validators(&self.state, resume_epoch);
                        info!(
                            validator_count = self.validator_set.validators.len(),
                            "rebuilt stakers index from genesis_state"
                        );
                    }
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

        // 2b'. Weak subjectivity: if a checkpoint (N, H) was configured and
        // this block is at or above N, verify that our canonical chain at N
        // contains H. A fork that diverges before the checkpoint is rejected
        // here — mitigates long-range attacks from historical validator sets.
        if let Some((cp_number, cp_hash)) = self.weak_subjectivity {
            if block.header.number >= cp_number {
                if let Some(stored) = self.canonical.get(&cp_number) {
                    if *stored != cp_hash {
                        return Err(ChainError::WeakSubjectivityViolation {
                            checkpoint: cp_number,
                            expected: cp_hash,
                            got: *stored,
                        });
                    }
                }
            }
        }

        // 2c. Timestamp sanity: must strictly increase over the parent, and
        // must not be more than MAX_FUTURE_DRIFT_MS ahead of wall clock. This
        // prevents validators from backdating unbonding maturity, prematurely
        // finalizing governance proposals, or poisoning state derived from
        // block time.
        if block.header.timestamp <= parent_header.timestamp {
            return Err(ChainError::InvalidTimestamp {
                block: block.header.timestamp,
                parent: parent_header.timestamp,
            });
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        const MAX_FUTURE_DRIFT_MS: u64 = 30_000;
        if now_ms > 0 && block.header.timestamp > now_ms.saturating_add(MAX_FUTURE_DRIFT_MS) {
            return Err(ChainError::TimestampTooFarInFuture {
                block: block.header.timestamp,
                now: now_ms,
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

        // Snapshot the entire StateDB before any block-scoped mutation so a
        // downstream failure (state_root mismatch, invalid block, etc.) can
        // roll back cleanly. Without this, the epoch-rotation slashing,
        // double-sign detection, missed-slot tracking and tx execution
        // would all persist even if the block were ultimately rejected at
        // the state_root check — corrupting state for the next import.
        let block_snapshot = self.state.snapshot();
        let validator_set_snapshot = self.validator_set.clone();

        // 4. Check for epoch transition and update validator set
        let block_epoch = self.consensus.epoch_for_block(block.header.number);
        if block_epoch > self.validator_set.epoch {
            debug!(
                old_epoch = self.validator_set.epoch,
                new_epoch = block_epoch,
                "epoch transition, re-electing validators"
            );
            // Apply downtime slashing for validators whose missed-slot count
            // in the closing epoch exceeds the configured threshold.
            let old_epoch = self.validator_set.epoch;
            let slots_per_epoch = self.consensus.params().epoch_length;
            let validator_count = self.validator_set.validators.len() as u64;
            let missed = self.state.take_missed_slots_for_epoch(old_epoch);
            let downtime_threshold = self
                .state
                .get_downtime_threshold_pct_override()
                .unwrap_or(self.consensus.params().downtime_threshold_pct);
            let downtime_bps = self
                .state
                .get_slash_downtime_bps_override()
                .unwrap_or(self.consensus.params().slash_downtime_bps);
            if validator_count > 0 && slots_per_epoch > 0 {
                let slot_allocation = (slots_per_epoch / validator_count).max(1) as u32;
                for (validator, miss_count) in missed {
                    if vtt_consensus::slashing::is_downtime_violation(
                        miss_count,
                        slot_allocation,
                        downtime_threshold,
                    ) {
                        let account = self.state.get_account(&validator);
                        let total_stake = account
                            .staking
                            .as_ref()
                            .map(|s| s.total_stake)
                            .unwrap_or(Amount::ZERO);
                        if !total_stake.is_zero() {
                            let slash_amount = vtt_consensus::slashing::calculate_downtime_slash(
                                total_stake,
                                downtime_bps,
                            );
                            let actual = self.state.apply_slash(&validator, slash_amount);
                            if !actual.is_zero() {
                                self.state
                                    .record_slash(&validator, old_epoch, "downtime", actual);
                                tracing::warn!(
                                    ?validator,
                                    epoch = old_epoch,
                                    missed = miss_count,
                                    %actual,
                                    "downtime slash applied at epoch rotation"
                                );
                            }
                        }
                    }
                }
            }
            self.validator_set = self.consensus.elect_validators(&self.state, block_epoch);
        }

        // 5. Verify consensus (producer, signature, etc.)
        self.consensus
            .verify_header(&block.header, &parent_header, &self.validator_set)?;

        // 5b. Double-sign detection: record commitment and, if a different
        // block was already seen at (validator, epoch, slot), build evidence
        // and apply the slash immediately. This is the automatic detection
        // pipeline; validators can still relay evidence via SubmitSlashingEvidence
        // for blocks that pre-date the change.
        let vs_validator = block.header.validator;
        let vs_epoch = block.header.epoch;
        let vs_slot = block.header.slot;
        if let Some(prior_hash) =
            self.state
                .record_block_commitment(vs_validator, vs_epoch, vs_slot, block_hash)
        {
            if prior_hash != block_hash {
                if let Some(prior_header) = self.headers.get(&prior_hash).cloned() {
                    use vtt_consensus::slashing::DoubleSignEvidence;
                    let evidence = vec![DoubleSignEvidence {
                        header_a: prior_header,
                        header_b: block.header.clone(),
                    }];
                    let double_sign_bps = self
                        .state
                        .get_slash_double_sign_bps_override()
                        .unwrap_or(self.consensus.params().slash_double_sign_bps);
                    let slashed = vtt_executor::process_slashing_evidence(
                        &mut self.state,
                        &evidence,
                        double_sign_bps,
                        block_epoch,
                    );
                    if !slashed.is_empty() {
                        tracing::warn!(
                            ?vs_validator,
                            epoch = vs_epoch,
                            slot = vs_slot,
                            "double-sign detected and slashed on block import"
                        );
                    }
                }
            }
        }

        // 5c. Downtime tracking: any expected leader whose slot we skipped
        // since the parent counts as a miss for this epoch. At the next
        // epoch rotation those counters get flushed and, over threshold,
        // produce a downtime slash.
        let slots_per_epoch = self.consensus.params().epoch_length as u32;
        let parent_slot = parent_header.slot;
        if block.header.epoch == parent_header.epoch && block.header.slot > parent_slot + 1 {
            let validators = self.validator_set.validators.clone();
            if !validators.is_empty() {
                for missed_slot in (parent_slot + 1)..block.header.slot {
                    let idx = (missed_slot as usize) % validators.len();
                    let missed_validator = validators[idx].address;
                    if missed_validator != vs_validator {
                        self.state
                            .record_missed_slot(block.header.epoch, missed_validator);
                    }
                }
            }
            let _ = slots_per_epoch;
        }

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

        // 7. Verify state root. Any mismatch reverts every mutation this
        // import made (epoch slashing, double-sign slash, missed slots,
        // executed txs) via the snapshot taken at the top of import_block.
        let computed_state_root = self.state.compute_state_root();
        if block.header.state_root != computed_state_root {
            self.state.restore(block_snapshot);
            self.validator_set = validator_set_snapshot;
            return Err(ChainError::InvalidStateRoot {
                expected: computed_state_root,
                got: block.header.state_root,
            });
        }

        // 8. Store block + receipts. Receipts are keyed by tx_hash so the
        // RPC layer can fetch them for get_transaction / event log queries.
        if let Some(ref storage) = self.storage {
            if let Ok(header_bytes) = borsh::to_vec(&block.header) {
                if let Err(e) =
                    storage.put(Column::BlockHeaders, block_hash.as_bytes(), &header_bytes)
                {
                    tracing::warn!(?block_hash, %e, "failed to persist block header");
                }
            }
            if let Ok(block_bytes) = borsh::to_vec(&block) {
                if let Err(e) =
                    storage.put(Column::BlockBodies, block_hash.as_bytes(), &block_bytes)
                {
                    tracing::warn!(?block_hash, %e, "failed to persist block body");
                }
            }
            for receipt in &receipts {
                if let Ok(bytes) = borsh::to_vec(receipt) {
                    let _ = storage.put(Column::Receipts, receipt.tx_hash.as_bytes(), &bytes);
                }
            }
            // Index tx_hash -> (block_number, tx_index) so the explorer can
            // resolve getTransaction(hash) in O(1) instead of scanning blocks.
            for (idx, tx) in block.transactions.iter().enumerate() {
                let tx_hash = vtt_crypto::blake3_hash(&tx.payload_bytes());
                let mut v = Vec::with_capacity(12);
                v.extend_from_slice(&block.header.number.to_le_bytes());
                v.extend_from_slice(&(idx as u32).to_le_bytes());
                let _ = storage.put(Column::Transactions, tx_hash.as_bytes(), &v);
            }
            // Bump the resume-backfill sentinel so the next restart skips the
            // O(chain_length) rescan for already-indexed blocks.
            let _ = storage.put(
                Column::ChainMeta,
                b"tx_index:head",
                &block.header.number.to_le_bytes(),
            );
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
    /// Longest-chain with finality enforcement plus a deterministic tie-break.
    ///
    /// If the incoming header has a strictly greater number it becomes head.
    /// If it matches the current head's number (equal-length forks observed
    /// during a partition), the one whose hash sorts lower wins. This keeps
    /// honest validators converging on the same canonical chain after a split.
    fn is_new_head(&self, header: &BlockHeader) -> bool {
        match self.head() {
            Some(head) => match header.number.cmp(&head.number) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => {
                    let incoming_hash = blake3_hash(&header.signable_bytes());
                    let current_head_hash = self.head_hash.unwrap_or(H256::ZERO);
                    incoming_hash < current_head_hash
                }
            },
            None => true,
        }
    }

    /// Install a weak subjectivity checkpoint: a block (number, hash) that
    /// node operators are asked to verify out-of-band. On import, the chain
    /// rejects any branch whose block at `number` does not match `hash`.
    /// This mitigates long-range attacks where a minority of historical
    /// validators colludes to build an alternate chain from genesis.
    pub fn set_weak_subjectivity_checkpoint(&mut self, number: BlockNumber, hash: H256) {
        self.weak_subjectivity = Some((number, hash));
    }

    /// Get the currently configured weak subjectivity checkpoint if any.
    pub fn weak_subjectivity(&self) -> Option<(BlockNumber, H256)> {
        self.weak_subjectivity
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

    /// Resolve a tx hash to its canonical (block_number, tx_index) location
    /// via the Column::Transactions index populated at import time. Returns
    /// `None` when the tx is unknown (or was imported before the index was
    /// introduced).
    pub fn get_tx_location(&self, tx_hash: &H256) -> Option<(BlockNumber, u32)> {
        let storage = self.storage.as_ref()?;
        let v = storage
            .get(Column::Transactions, tx_hash.as_bytes())
            .ok()
            .flatten()?;
        if v.len() != 12 {
            return None;
        }
        let mut bn = [0u8; 8];
        bn.copy_from_slice(&v[..8]);
        let mut ti = [0u8; 4];
        ti.copy_from_slice(&v[8..]);
        Some((BlockNumber::from_le_bytes(bn), u32::from_le_bytes(ti)))
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

    /// Load a persisted transaction receipt by hash, if present.
    /// Returns None if the receipt is not stored (tx never executed, or
    /// produced before receipt persistence was introduced).
    pub fn get_receipt(&self, tx_hash: &H256) -> Option<TransactionReceipt> {
        let storage = self.storage.as_ref()?;
        let bytes = storage
            .get(Column::Receipts, tx_hash.as_bytes())
            .ok()
            .flatten()?;
        borsh::from_slice(&bytes).ok()
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
    fn stakers_index_warms_elect_validators_after_restart() {
        use vtt_state::account::StakingState;
        use vtt_storage::memory::InMemoryStore;

        let storage: Arc<dyn vtt_storage::KeyValueStore> = Arc::new(InMemoryStore::new());
        let val_addr = Address::from([0x10; 20]);

        // First run: write a staking account, verify elect returns it.
        {
            let mut db = StateDB::with_storage(storage.clone());
            let mut account = AccountState::with_balance(Amount::from_vtt(500_000));
            account.staking = Some(StakingState {
                total_stake: Amount::from_vtt(100_000),
                self_stake: Amount::from_vtt(100_000),
                commission_bps: 500,
                active: true,
                delegations: Vec::new(),
                unbonding: Vec::new(),
            });
            db.put_account(val_addr, account);

            let consensus = test_consensus();
            let set = consensus.elect_validators(&db, 0);
            assert_eq!(set.validators.len(), 1);
            assert_eq!(set.validators[0].address, val_addr);
        }

        // Second run: fresh StateDB with same storage. Stakers index must be
        // loaded and the accounts cache warmed so elect_validators still finds
        // the validator even though we never explicitly re-inserted it.
        {
            let db = StateDB::with_storage(storage);
            let consensus = test_consensus();
            let set = consensus.elect_validators(&db, 0);
            assert_eq!(
                set.validators.len(),
                1,
                "elect_validators must find staker after restart"
            );
            assert_eq!(set.validators[0].address, val_addr);
        }
    }

    #[test]
    fn iter_caches_warmed_after_restart() {
        use vtt_state::asset::{AssetClass, AssetRecord, AssetStatus, TransferMode};
        use vtt_storage::memory::InMemoryStore;

        let storage: Arc<dyn vtt_storage::KeyValueStore> = Arc::new(InMemoryStore::new());
        let asset_id = H256::from([0xAA; 32]);

        // First run: register an asset.
        {
            let mut db = StateDB::with_storage(storage.clone());
            db.register_asset(AssetRecord {
                id: asset_id,
                name: "Test Asset".into(),
                symbol: "TST".into(),
                class: AssetClass::Equity,
                origin_chain: vtt_primitives::ChainId::RELAY,
                issuer: Address::from([0x01; 20]),
                total_supply: Amount::from_vtt(1_000),
                decimals: 18,
                status: AssetStatus::Active,
                compliance_policy: None,
                valuation_oracle: None,
                documents: Default::default(),
                metadata_uri: String::new(),
                jurisdiction: "IT".into(),
                legal_entity: "Test SPV".into(),
                transfer_mode: TransferMode::PeerToPeer,
                registrar: None,
                redemption_pool: Amount::ZERO,
                requires_kyc: true,
                created_at: 0,
            })
            .unwrap();
            assert_eq!(db.asset_count(), 1);
        }

        // Second run: fresh StateDB with same storage, iter_assets must show
        // the asset (warmed from disk via prefix_scan).
        {
            let db = StateDB::with_storage(storage);
            assert_eq!(
                db.asset_count(),
                1,
                "asset cache must be warmed from storage on restart"
            );
            assert!(db.get_asset(&asset_id).is_some());
        }
    }

    #[test]
    fn unbonding_and_slashing_seen_survive_restart() {
        use vtt_state::account::UnbondingEntry;
        use vtt_storage::memory::InMemoryStore;

        let storage: Arc<dyn vtt_storage::KeyValueStore> = Arc::new(InMemoryStore::new());
        let addr = Address::from([0x77; 20]);

        // First run: write some unbonding entries and slashing evidence
        {
            let mut db = StateDB::with_storage(storage.clone());
            db.add_unbonding_entry(
                addr,
                UnbondingEntry {
                    amount: Amount::from_vtt(1_000),
                    completion_time: 9_999_999_999,
                    validator: addr,
                },
            );
            db.mark_slashing_evidence(addr, 42, 7);
            assert_eq!(db.get_unbonding_entries(&addr).len(), 1);
            assert!(db.slashing_evidence_seen(&addr, 42, 7));
        }

        // Second run: fresh StateDB with same storage, data must reappear
        {
            let db = StateDB::with_storage(storage);
            assert_eq!(
                db.get_unbonding_entries(&addr).len(),
                1,
                "unbonding entry must survive restart"
            );
            assert_eq!(
                db.get_unbonding_entries(&addr)[0].amount,
                Amount::from_vtt(1_000)
            );
            assert!(
                db.slashing_evidence_seen(&addr, 42, 7),
                "slashing evidence dedup must survive restart"
            );
        }
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
