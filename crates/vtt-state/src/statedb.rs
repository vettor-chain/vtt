use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use thiserror::Error;

use vtt_primitives::amount::Amount;
use vtt_primitives::asset_governance::AssetProposal;
use vtt_primitives::{Address, Timestamp, H256};
use vtt_storage::{Column, KeyValueStore};

use crate::account::UnbondingEntry;

use crate::account::AccountState;
use crate::asset::{AssetRecord, OwnershipRecord};
use crate::oracle::OracleFeed;
use crate::trie::StateTrie;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("insufficient balance: have {have}, need {need}")]
    InsufficientBalance { have: Amount, need: Amount },
    #[error("account not found: {0}")]
    AccountNotFound(Address),
    #[error("nonce mismatch: expected {expected}, got {got}")]
    NonceMismatch { expected: u64, got: u64 },
    #[error("serialization error: {0}")]
    Serialization(String),
}

pub type Result<T> = std::result::Result<T, StateError>;

/// On-disk state schema version. Bumped whenever a storage format change
/// requires a migration. Mismatched versions refuse to start rather than
/// silently corrupt state.
pub const DB_SCHEMA_VERSION: u32 = 1;

/// ChainMeta key holding the current schema version (little-endian u32).
const SCHEMA_VERSION_KEY: &[u8] = b"schema:version";

/// State database providing read/write access to account state.
/// Operates on an in-memory overlay that can be committed to produce a state root.
/// Optionally backed by a persistent `KeyValueStore` (e.g., RocksDB).
/// When `storage` is `Some`, reads fall through from cache to disk, and writes
/// go to both cache and disk (write-through cache).
pub struct StateDB {
    /// In-memory account cache / overlay.
    accounts: HashMap<Address, AccountState>,
    /// Asset registry: asset_id -> AssetRecord.
    assets: HashMap<H256, AssetRecord>,
    /// Ownership records: (asset_id, owner) -> OwnershipRecord.
    ownership: HashMap<(H256, Address), OwnershipRecord>,
    /// Oracle feeds: feed_id -> OracleFeed.
    oracles: HashMap<H256, OracleFeed>,
    /// Contract code storage: code_hash -> bytecode.
    contract_code: HashMap<H256, Vec<u8>>,
    /// Contract storage: (contract_address, key) -> value.
    contract_storage: HashMap<(Address, Vec<u8>), Vec<u8>>,
    /// Pool storage: pool_id -> raw serialized pool data.
    pools: HashMap<H256, Vec<u8>>,
    /// Asset governance proposals: proposal_id -> AssetProposal.
    asset_proposals: HashMap<H256, AssetProposal>,
    /// Liquidity mining state: pool_id -> raw serialized MiningState.
    mining_states: HashMap<H256, Vec<u8>>,
    /// Protocol governance proposals: proposal_id -> raw serialized Proposal.
    governance_proposals: HashMap<H256, Vec<u8>>,
    /// Unbonding entries by staker address (waiting to be released after unbonding period).
    unbonding_entries: HashMap<Address, Vec<UnbondingEntry>>,
    /// Processed slashing evidence (dedup key: offender + epoch + slot).
    /// Prevents double-slashing from duplicate evidence submissions.
    slashing_seen: HashSet<(Address, u64, u32)>,
    /// KYC-approved address set. Required for sender/recipient on transfers
    /// of regulated assets (requires_kyc = true).
    kyc_approved: HashSet<Address>,
    /// Map from (validator, epoch, slot) to the hash of the first block
    /// observed at that commit. A second block at the same key with a
    /// different hash is direct double-sign evidence.
    block_commitments: HashMap<(Address, u64, u32), H256>,
    /// Missed-slot counter per (epoch, validator) for downtime detection.
    missed_slots: HashMap<(u64, Address), u32>,
    /// Addresses that currently have a staking record. Persisted index so
    /// elect_validators can iterate validators on a fresh StateDB whose
    /// `accounts` HashMap has not yet been warmed from disk.
    stakers: HashSet<Address>,
    /// Protocol treasury address (set from consensus params at genesis/init).
    treasury_address: Address,
    /// Epoch length in blocks (set from consensus params at genesis/init).
    epoch_length: u64,
    /// Whether the DEX is paused (governance action can toggle this).
    dex_paused: bool,
    /// The underlying trie for computing state roots.
    trie: StateTrie,
    /// Tracks which accounts have been modified.
    dirty: Vec<Address>,
    /// Tracks which assets have been modified.
    dirty_assets: Vec<H256>,
    /// Tracks which pools have been modified.
    dirty_pools: Vec<H256>,
    /// Optional persistent storage backend (RocksDB).
    storage: Option<Arc<dyn KeyValueStore>>,
}

impl StateDB {
    /// Create a new empty state database (in-memory only).
    pub fn new() -> Self {
        Self {
            accounts: HashMap::new(),
            assets: HashMap::new(),
            ownership: HashMap::new(),
            oracles: HashMap::new(),
            contract_code: HashMap::new(),
            contract_storage: HashMap::new(),
            pools: HashMap::new(),
            asset_proposals: HashMap::new(),
            mining_states: HashMap::new(),
            governance_proposals: HashMap::new(),
            unbonding_entries: HashMap::new(),
            slashing_seen: HashSet::new(),
            kyc_approved: HashSet::new(),
            block_commitments: HashMap::new(),
            missed_slots: HashMap::new(),
            stakers: HashSet::new(),
            treasury_address: Address::ZERO,
            epoch_length: 1200,
            dex_paused: false,
            trie: StateTrie::new(),
            dirty: Vec::new(),
            dirty_assets: Vec::new(),
            dirty_pools: Vec::new(),
            storage: None,
        }
    }

    /// Create a state database backed by persistent storage.
    /// Reads fall through from cache to disk; writes go to both.
    pub fn with_storage(storage: Arc<dyn KeyValueStore>) -> Self {
        let mut db = Self {
            accounts: HashMap::new(),
            assets: HashMap::new(),
            ownership: HashMap::new(),
            oracles: HashMap::new(),
            contract_code: HashMap::new(),
            contract_storage: HashMap::new(),
            pools: HashMap::new(),
            asset_proposals: HashMap::new(),
            mining_states: HashMap::new(),
            governance_proposals: HashMap::new(),
            unbonding_entries: HashMap::new(),
            slashing_seen: HashSet::new(),
            kyc_approved: HashSet::new(),
            block_commitments: HashMap::new(),
            missed_slots: HashMap::new(),
            stakers: HashSet::new(),
            treasury_address: Address::ZERO,
            epoch_length: 1200,
            dex_paused: false,
            trie: StateTrie::new(),
            dirty: Vec::new(),
            dirty_assets: Vec::new(),
            dirty_pools: Vec::new(),
            storage: Some(storage),
        };
        // Fail fast on an incompatible on-disk layout rather than silently
        // corrupting state by running newer code against an older DB.
        db.verify_or_stamp_schema_version();
        // State that doesn't fall through cache -> disk on read must be
        // eagerly loaded from storage so the restart path is consistent.
        db.load_non_fallthrough_state();
        db
    }

    /// Read the DB schema version from storage. If absent, stamp the current
    /// version (fresh DB). If present and incompatible, panic — running a
    /// binary against a DB of a different schema would corrupt state.
    fn verify_or_stamp_schema_version(&self) {
        let storage = match &self.storage {
            Some(s) => s.clone(),
            None => return,
        };
        match storage.get(Column::ChainMeta, SCHEMA_VERSION_KEY) {
            Ok(Some(bytes)) if bytes.len() == 4 => {
                let mut buf = [0u8; 4];
                buf.copy_from_slice(&bytes);
                let on_disk = u32::from_le_bytes(buf);
                if on_disk != DB_SCHEMA_VERSION {
                    panic!(
                        "DB schema version mismatch: binary expects v{DB_SCHEMA_VERSION}, on-disk is v{on_disk}. \
                         Refusing to start — run the matching migration or wipe the data directory.",
                    );
                }
            }
            Ok(Some(_)) => panic!(
                "DB schema version key is corrupt (expected 4 bytes). \
                 Refusing to start."
            ),
            Ok(None) => {
                let _ = storage.put(
                    Column::ChainMeta,
                    SCHEMA_VERSION_KEY,
                    &DB_SCHEMA_VERSION.to_le_bytes(),
                );
                tracing::info!(version = DB_SCHEMA_VERSION, "stamped DB schema version");
            }
            Err(e) => panic!("failed to read DB schema version: {e}"),
        }
    }

    /// Create a state database with pre-loaded accounts (e.g., from genesis).
    pub fn with_accounts(accounts: Vec<(Address, AccountState)>) -> Self {
        let mut db = Self::new();
        for (addr, state) in accounts {
            db.put_account(addr, state);
        }
        db
    }

    /// Get account state. Returns default (empty) state if account doesn't exist.
    /// Checks in-memory cache first, then falls back to persistent storage.
    pub fn get_account(&self, address: &Address) -> AccountState {
        if let Some(account) = self.accounts.get(address) {
            return account.clone();
        }
        // Fall back to persistent storage
        if let Some(ref storage) = self.storage {
            if let Ok(Some(bytes)) = storage.get(Column::Accounts, address.as_bytes()) {
                if let Ok(account) = borsh::from_slice::<AccountState>(&bytes) {
                    return account;
                }
            }
        }
        AccountState::default()
    }

    /// Get account state if it exists.
    pub fn get_account_opt(&self, address: &Address) -> Option<&AccountState> {
        self.accounts.get(address)
    }

    /// Set account state. Writes to both cache and persistent storage.
    pub fn put_account(&mut self, address: Address, state: AccountState) {
        self.dirty.push(address);
        if let Some(ref storage) = self.storage {
            if let Ok(bytes) = borsh::to_vec(&state) {
                let _ = storage.put(Column::Accounts, address.as_bytes(), &bytes);
            }
        }
        // Maintain the stakers index so validator election works on a cold
        // StateDB (the `accounts` cache is populated lazily from disk).
        let has_stake = state
            .staking
            .as_ref()
            .map(|s| !s.self_stake.is_zero())
            .unwrap_or(false);
        let changed = if has_stake {
            self.stakers.insert(address)
        } else {
            self.stakers.remove(&address)
        };
        if changed {
            self.persist_stakers();
        }
        self.accounts.insert(address, state);
    }

    /// Whether the stakers index is empty. Used by Chain::init_genesis to
    /// decide if a one-time rebuild is needed on resume (for chains produced
    /// before the index was introduced).
    pub fn stakers_empty(&self) -> bool {
        self.stakers.is_empty()
    }

    /// Rebuild the stakers index from the given source StateDB (typically the
    /// fresh genesis_state passed to init_genesis). For each staker, warms
    /// this StateDB's account cache either from disk (if the address already
    /// has state there) or from the source (for truly fresh chains). The cache
    /// population is required because elect_validators iterates the in-memory
    /// `accounts` HashMap and doesn't fall through to storage.
    pub fn bootstrap_stakers_from(&mut self, source: &StateDB) {
        for (addr, acc) in &source.accounts {
            let Some(ref s) = acc.staking else { continue };
            if s.self_stake.is_zero() {
                continue;
            }
            // Load the on-disk account (if any) to preserve deltas from
            // previous staking/unstaking events; otherwise use the source.
            let existing = self.get_account(addr);
            let account_to_cache = if existing.staking.is_some() {
                existing
            } else {
                acc.clone()
            };
            self.accounts.insert(*addr, account_to_cache);
            self.stakers.insert(*addr);
        }
        self.persist_stakers();
    }

    fn persist_stakers(&self) {
        if let Some(ref storage) = self.storage {
            let items: Vec<Address> = self.stakers.iter().copied().collect();
            if let Ok(bytes) = borsh::to_vec(&items) {
                let _ = storage.put(Column::ChainMeta, b"stakers:set", &bytes);
            }
        }
    }

    /// Check if an account exists (has been explicitly set).
    pub fn account_exists(&self, address: &Address) -> bool {
        if self.accounts.contains_key(address) {
            return true;
        }
        if let Some(ref storage) = self.storage {
            if let Ok(exists) = storage.contains(Column::Accounts, address.as_bytes()) {
                return exists;
            }
        }
        false
    }

    /// Get the balance of an account.
    pub fn get_balance(&self, address: &Address) -> Amount {
        self.get_account(address).balance
    }

    /// Get the nonce of an account.
    pub fn get_nonce(&self, address: &Address) -> u64 {
        self.get_account(address).nonce
    }

    /// Add balance to an account (creates account if it doesn't exist).
    pub fn add_balance(&mut self, address: &Address, amount: Amount) -> Result<()> {
        let mut account = self.get_account(address);
        account.balance = account
            .balance
            .checked_add(amount)
            .ok_or_else(|| StateError::Serialization("balance overflow".to_string()))?;
        self.put_account(*address, account);
        Ok(())
    }

    /// Subtract balance from an account.
    pub fn sub_balance(&mut self, address: &Address, amount: Amount) -> Result<()> {
        let mut account = self.get_account(address);
        account.balance =
            account
                .balance
                .checked_sub(amount)
                .ok_or(StateError::InsufficientBalance {
                    have: account.balance,
                    need: amount,
                })?;
        self.put_account(*address, account);
        Ok(())
    }

    /// Transfer VTT from one account to another.
    pub fn transfer(&mut self, from: &Address, to: &Address, amount: Amount) -> Result<()> {
        if amount.is_zero() {
            return Ok(());
        }
        self.sub_balance(from, amount)?;
        self.add_balance(to, amount)?;
        Ok(())
    }

    /// Increment the nonce of an account. Returns the old nonce.
    pub fn increment_nonce(&mut self, address: &Address) -> u64 {
        let mut account = self.get_account(address);
        let old_nonce = account.nonce;
        account.nonce += 1;
        self.put_account(*address, account);
        old_nonce
    }

    // --- Asset Registry Methods ---

    /// Register a new asset. Returns error if asset ID already exists.
    pub fn register_asset(&mut self, asset: AssetRecord) -> Result<()> {
        if self.assets.contains_key(&asset.id) {
            return Err(StateError::Serialization(format!(
                "asset already exists: {}",
                asset.id
            )));
        }
        // Also check persistent storage
        if let Some(ref storage) = self.storage {
            if let Ok(true) = storage.contains(Column::Assets, asset.id.as_bytes()) {
                return Err(StateError::Serialization(format!(
                    "asset already exists: {}",
                    asset.id
                )));
            }
        }
        self.dirty_assets.push(asset.id);
        if let Some(ref storage) = self.storage {
            if let Ok(bytes) = borsh::to_vec(&asset) {
                let _ = storage.put(Column::Assets, asset.id.as_bytes(), &bytes);
            }
        }
        self.assets.insert(asset.id, asset);
        Ok(())
    }

    /// Get an asset record by ID.
    pub fn get_asset(&self, asset_id: &H256) -> Option<&AssetRecord> {
        self.assets.get(asset_id)
    }

    /// Get an asset record by ID (owned), falling back to persistent storage.
    pub fn get_asset_owned(&self, asset_id: &H256) -> Option<AssetRecord> {
        if let Some(asset) = self.assets.get(asset_id) {
            return Some(asset.clone());
        }
        if let Some(ref storage) = self.storage {
            if let Ok(Some(bytes)) = storage.get(Column::Assets, asset_id.as_bytes()) {
                if let Ok(asset) = borsh::from_slice::<AssetRecord>(&bytes) {
                    return Some(asset);
                }
            }
        }
        None
    }

    /// Get a mutable asset record by ID.
    pub fn get_asset_mut(&mut self, asset_id: &H256) -> Option<&mut AssetRecord> {
        if let Some(asset) = self.assets.get_mut(asset_id) {
            self.dirty_assets.push(*asset_id);
            Some(asset)
        } else {
            None
        }
    }

    /// Get ownership record for an (asset, owner) pair.
    pub fn get_ownership(&self, asset_id: &H256, owner: &Address) -> OwnershipRecord {
        self.ownership
            .get(&(*asset_id, *owner))
            .cloned()
            .unwrap_or_else(|| OwnershipRecord::new(*asset_id, *owner))
    }

    /// Set ownership record.
    pub fn put_ownership(&mut self, record: OwnershipRecord) {
        let key = (record.asset_id, record.owner);
        self.dirty_assets.push(record.asset_id);
        if let Some(ref storage) = self.storage {
            let mut storage_key = record.asset_id.as_bytes().to_vec();
            storage_key.extend_from_slice(record.owner.as_bytes());
            if let Ok(bytes) = borsh::to_vec(&record) {
                let _ = storage.put(Column::Ownership, &storage_key, &bytes);
            }
        }
        self.ownership.insert(key, record);
    }

    /// Transfer asset tokens between owners. Returns error if insufficient balance.
    pub fn transfer_asset(
        &mut self,
        asset_id: &H256,
        from: &Address,
        to: &Address,
        amount: Amount,
    ) -> Result<()> {
        if amount.is_zero() {
            return Ok(());
        }

        // Check asset exists and is tradeable
        let asset = self
            .assets
            .get(asset_id)
            .ok_or_else(|| StateError::Serialization(format!("asset not found: {asset_id}")))?;
        if !asset.is_tradeable() {
            return Err(StateError::Serialization(format!(
                "asset not tradeable: {}",
                asset.status_str()
            )));
        }

        // Debit sender
        let mut from_record = self.get_ownership(asset_id, from);
        if !from_record.debit(amount) {
            return Err(StateError::InsufficientBalance {
                have: from_record.available,
                need: amount,
            });
        }
        self.put_ownership(from_record);

        // Credit recipient
        let mut to_record = self.get_ownership(asset_id, to);
        to_record.credit(amount);
        self.put_ownership(to_record);

        Ok(())
    }

    /// Iterate all ownership records for a given asset.
    pub fn iter_ownership_for_asset(
        &self,
        asset_id: &H256,
    ) -> impl Iterator<Item = &OwnershipRecord> {
        let asset_id = *asset_id;
        self.ownership
            .iter()
            .filter(move |((aid, _), _)| *aid == asset_id)
            .map(|(_, record)| record)
    }

    /// Get the number of registered assets.
    pub fn asset_count(&self) -> usize {
        self.assets.len()
    }

    /// Iterate over all assets.
    pub fn iter_assets(&self) -> impl Iterator<Item = (&H256, &AssetRecord)> {
        self.assets.iter()
    }

    // --- Asset Governance Proposal Methods ---

    /// Get an asset proposal by ID.
    pub fn get_asset_proposal(&self, id: &H256) -> Option<&AssetProposal> {
        self.asset_proposals.get(id)
    }

    /// Get a mutable reference to an asset proposal by ID.
    pub fn get_asset_proposal_mut(&mut self, id: &H256) -> Option<&mut AssetProposal> {
        self.asset_proposals.get_mut(id)
    }

    /// Store an asset proposal.
    pub fn put_asset_proposal(&mut self, proposal: AssetProposal) {
        if let Some(ref storage) = self.storage {
            if let Ok(bytes) = borsh::to_vec(&proposal) {
                let _ = storage.put(Column::AssetProposals, proposal.id.as_bytes(), &bytes);
            }
        }
        self.asset_proposals.insert(proposal.id, proposal);
    }

    /// Iterate over all asset proposals.
    pub fn iter_asset_proposals(&self) -> impl Iterator<Item = (&H256, &AssetProposal)> {
        self.asset_proposals.iter()
    }

    /// Get all proposals for a given asset.
    pub fn iter_asset_proposals_for_asset(&self, asset_id: &H256) -> Vec<&AssetProposal> {
        let asset_id = *asset_id;
        self.asset_proposals
            .values()
            .filter(move |p| p.asset_id == asset_id)
            .collect()
    }

    // --- Oracle Methods ---

    /// Register a new oracle feed.
    pub fn register_oracle(&mut self, feed: OracleFeed) -> Result<()> {
        if self.oracles.contains_key(&feed.feed_id) {
            return Err(StateError::Serialization(format!(
                "oracle feed already exists: {}",
                feed.feed_id
            )));
        }
        if let Some(ref storage) = self.storage {
            if let Ok(bytes) = borsh::to_vec(&feed) {
                let _ = storage.put(Column::Oracles, feed.feed_id.as_bytes(), &bytes);
            }
        }
        self.oracles.insert(feed.feed_id, feed);
        Ok(())
    }

    /// Get an oracle feed by ID.
    pub fn get_oracle(&self, feed_id: &H256) -> Option<&OracleFeed> {
        self.oracles.get(feed_id)
    }

    /// Get a mutable oracle feed by ID.
    pub fn get_oracle_mut(&mut self, feed_id: &H256) -> Option<&mut OracleFeed> {
        self.oracles.get_mut(feed_id)
    }

    /// Number of registered oracle feeds.
    pub fn oracle_count(&self) -> usize {
        self.oracles.len()
    }

    /// Iterate over all registered oracle feeds.
    pub fn iter_oracles(&self) -> impl Iterator<Item = (&H256, &OracleFeed)> {
        self.oracles.iter()
    }

    /// Submit an oracle value to an existing feed, persisting the updated
    /// feed back to storage on success.
    ///
    /// Returns:
    /// - `Ok(true)` if quorum was reached and the aggregated value was updated.
    /// - `Ok(false)` if the submission was accepted but quorum not yet reached.
    /// - `Err` if the feed does not exist or the sender is not authorized.
    pub fn submit_oracle(
        &mut self,
        feed_id: &H256,
        source: Address,
        value: Amount,
        timestamp: Timestamp,
    ) -> Result<bool> {
        let feed = self.oracles.get_mut(feed_id).ok_or_else(|| {
            StateError::Serialization(format!("oracle feed not found: {feed_id}"))
        })?;
        if !feed.is_authorized(&source) {
            return Err(StateError::Serialization(format!(
                "address is not an authorized source for feed {feed_id}",
            )));
        }
        let reached = feed.submit(source, value, timestamp);
        if let Some(ref storage) = self.storage {
            if let Ok(bytes) = borsh::to_vec(feed) {
                let _ = storage.put(Column::Oracles, feed_id.as_bytes(), &bytes);
            }
        }
        Ok(reached)
    }

    // --- Contract Code Methods ---

    /// Store contract bytecode. Returns the code hash.
    pub fn store_code(&mut self, code: Vec<u8>) -> H256 {
        let hash = vtt_crypto::blake3_hash(&code);
        if let Some(ref storage) = self.storage {
            let _ = storage.put(Column::ContractCode, hash.as_bytes(), &code);
        }
        self.contract_code.insert(hash, code);
        hash
    }

    /// Get contract bytecode by code hash.
    pub fn get_code(&self, code_hash: &H256) -> Option<&Vec<u8>> {
        // Note: returns a reference, so can only check cache.
        // For persistent fallback, use get_code_owned.
        self.contract_code.get(code_hash)
    }

    /// Get contract bytecode by code hash (owned), falling back to storage.
    pub fn get_code_owned(&self, code_hash: &H256) -> Option<Vec<u8>> {
        if let Some(code) = self.contract_code.get(code_hash) {
            return Some(code.clone());
        }
        if let Some(ref storage) = self.storage {
            if let Ok(Some(bytes)) = storage.get(Column::ContractCode, code_hash.as_bytes()) {
                return Some(bytes);
            }
        }
        None
    }

    // --- Contract Storage Methods ---

    /// Read a value from a contract's storage.
    pub fn get_contract_storage(&self, address: &Address, key: &[u8]) -> Option<Vec<u8>> {
        if let Some(val) = self.contract_storage.get(&(*address, key.to_vec())) {
            return Some(val.clone());
        }
        if let Some(ref storage) = self.storage {
            let mut storage_key = address.as_bytes().to_vec();
            storage_key.extend_from_slice(key);
            if let Ok(Some(bytes)) = storage.get(Column::ContractStorage, &storage_key) {
                return Some(bytes);
            }
        }
        None
    }

    /// Write a value to a contract's storage.
    pub fn put_contract_storage(&mut self, address: Address, key: Vec<u8>, value: Vec<u8>) {
        self.dirty.push(address);
        if let Some(ref storage) = self.storage {
            let mut storage_key = address.as_bytes().to_vec();
            storage_key.extend_from_slice(&key);
            let _ = storage.put(Column::ContractStorage, &storage_key, &value);
        }
        self.contract_storage.insert((address, key), value);
    }

    /// Delete a value from a contract's storage.
    pub fn delete_contract_storage(&mut self, address: &Address, key: &[u8]) {
        self.dirty.push(*address);
        if let Some(ref storage) = self.storage {
            let mut storage_key = address.as_bytes().to_vec();
            storage_key.extend_from_slice(key);
            let _ = storage.delete(Column::ContractStorage, &storage_key);
        }
        self.contract_storage.remove(&(*address, key.to_vec()));
    }

    /// Load all storage entries for a contract address into a HashMap.
    pub fn load_contract_storage(&self, address: &Address) -> HashMap<Vec<u8>, Vec<u8>> {
        self.contract_storage
            .iter()
            .filter(|((addr, _), _)| addr == address)
            .map(|((_, key), value)| (key.clone(), value.clone()))
            .collect()
    }

    // --- Pool Methods ---

    pub fn get_pool_raw(&self, pool_id: &H256) -> Option<&[u8]> {
        if let Some(data) = self.pools.get(pool_id) {
            return Some(data.as_slice());
        }
        // Note: cannot return a reference to storage data from an immutable borrow,
        // so persistent pool reads require the caller to go through `get_pool_raw_owned`.
        None
    }

    /// Get pool data as an owned Vec, falling back to persistent storage.
    pub fn get_pool_raw_owned(&self, pool_id: &H256) -> Option<Vec<u8>> {
        if let Some(data) = self.pools.get(pool_id) {
            return Some(data.clone());
        }
        if let Some(ref storage) = self.storage {
            if let Ok(Some(bytes)) = storage.get(Column::Pools, pool_id.as_bytes()) {
                return Some(bytes);
            }
        }
        None
    }

    pub fn put_pool_raw(&mut self, pool_id: H256, data: Vec<u8>) {
        if let Some(ref storage) = self.storage {
            let _ = storage.put(Column::Pools, pool_id.as_bytes(), &data);
        }
        self.pools.insert(pool_id, data);
        self.dirty_pools.push(pool_id);
    }

    pub fn has_pool(&self, pool_id: &H256) -> bool {
        if self.pools.contains_key(pool_id) {
            return true;
        }
        if let Some(ref storage) = self.storage {
            if let Ok(exists) = storage.contains(Column::Pools, pool_id.as_bytes()) {
                return exists;
            }
        }
        false
    }

    pub fn iter_pools(&self) -> impl Iterator<Item = (&H256, &[u8])> {
        self.pools.iter().map(|(k, v)| (k, v.as_slice()))
    }

    // --- Mining State Methods ---

    /// Get raw mining state for a pool.
    pub fn get_mining_state_raw(&self, pool_id: &H256) -> Option<&[u8]> {
        self.mining_states.get(pool_id).map(|v| v.as_slice())
    }

    /// Store raw mining state for a pool.
    pub fn put_mining_state_raw(&mut self, pool_id: H256, data: Vec<u8>) {
        if let Some(ref storage) = self.storage {
            let _ = storage.put(Column::MiningStates, pool_id.as_bytes(), &data);
        }
        self.mining_states.insert(pool_id, data);
    }

    /// Check if mining state exists for a pool.
    pub fn has_mining_state(&self, pool_id: &H256) -> bool {
        if self.mining_states.contains_key(pool_id) {
            return true;
        }
        if let Some(ref storage) = self.storage {
            if let Ok(exists) = storage.contains(Column::MiningStates, pool_id.as_bytes()) {
                return exists;
            }
        }
        false
    }

    // --- Chain Parameter Methods ---

    /// Get the protocol treasury address.
    pub fn get_treasury_address(&self) -> Address {
        self.treasury_address
    }

    /// Set the protocol treasury address (typically from consensus params at init).
    pub fn set_treasury_address(&mut self, addr: Address) {
        if let Some(ref storage) = self.storage {
            let _ = storage.put(Column::ChainMeta, b"treasury_address", addr.as_bytes());
        }
        self.treasury_address = addr;
    }

    /// Get the epoch length in blocks.
    pub fn get_epoch_length(&self) -> u64 {
        self.epoch_length
    }

    /// Set the epoch length in blocks (typically from consensus params at init).
    pub fn set_epoch_length(&mut self, length: u64) {
        if let Some(ref storage) = self.storage {
            let _ = storage.put(Column::ChainMeta, b"epoch_length", &length.to_le_bytes());
        }
        self.epoch_length = length;
    }

    /// Check whether the DEX is paused.
    pub fn is_dex_paused(&self) -> bool {
        self.dex_paused
    }

    /// Set the DEX paused state (controlled via governance).
    pub fn set_dex_paused(&mut self, paused: bool) {
        if let Some(ref storage) = self.storage {
            let val = if paused { [1u8] } else { [0u8] };
            let _ = storage.put(Column::ChainMeta, b"dex:paused", &val);
        }
        self.dex_paused = paused;
    }

    /// Check whether the bridge is paused.
    pub fn is_bridge_paused(&self) -> bool {
        if let Some(ref storage) = self.storage {
            if let Ok(Some(v)) = storage.get(Column::ChainMeta, b"bridge:paused") {
                return v == [1u8];
            }
        }
        false
    }

    /// Set the bridge paused state (controlled via governance).
    pub fn set_bridge_paused(&mut self, paused: bool) {
        if let Some(ref storage) = self.storage {
            let val = if paused { [1u8] } else { [0u8] };
            let _ = storage.put(Column::ChainMeta, b"bridge:paused", &val);
        }
    }

    /// Get the bridge relayer address (the only address allowed to submit
    /// BridgeDeposit transactions). Returns Address::ZERO if not set.
    pub fn bridge_relayer(&self) -> Address {
        if let Some(ref storage) = self.storage {
            if let Ok(Some(v)) = storage.get(Column::ChainMeta, b"bridge:relayer") {
                if v.len() == 20 {
                    let mut bytes = [0u8; 20];
                    bytes.copy_from_slice(&v);
                    return Address::from(bytes);
                }
            }
        }
        Address::ZERO
    }

    /// Set the bridge relayer address (governance-controlled).
    pub fn set_bridge_relayer(&mut self, relayer: Address) {
        if let Some(ref storage) = self.storage {
            let _ = storage.put(Column::ChainMeta, b"bridge:relayer", relayer.as_bytes());
        }
    }

    /// Get a governance-set protocol parameter override (raw bytes).
    /// Returns `None` if not set, in which case consumers fall back to the
    /// consensus / gas defaults baked into `ChainConfig`.
    fn get_param_raw(&self, key: &str) -> Option<Vec<u8>> {
        let storage = self.storage.as_ref()?;
        let mut k = b"param:".to_vec();
        k.extend_from_slice(key.as_bytes());
        storage.get(Column::ChainMeta, &k).ok().flatten()
    }

    /// Set a governance-controlled protocol parameter (raw bytes).
    fn set_param_raw(&self, key: &str, value: &[u8]) {
        if let Some(ref storage) = self.storage {
            let mut k = b"param:".to_vec();
            k.extend_from_slice(key.as_bytes());
            let _ = storage.put(Column::ChainMeta, &k, value);
        }
    }

    /// Governance-set override for the minimum gas price (raw u128).
    pub fn get_min_gas_price_override(&self) -> Option<Amount> {
        let raw = self.get_param_raw("min_gas_price")?;
        if raw.len() != 16 {
            return None;
        }
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&raw);
        Some(Amount::from_raw(u128::from_le_bytes(buf)))
    }

    pub fn set_min_gas_price(&self, value: Amount) {
        self.set_param_raw("min_gas_price", &value.raw().to_le_bytes());
    }

    /// Governance-set override for the base transfer gas cost.
    pub fn get_base_transfer_cost_override(&self) -> Option<u64> {
        let raw = self.get_param_raw("base_transfer_cost")?;
        if raw.len() != 8 {
            return None;
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&raw);
        Some(u64::from_le_bytes(buf))
    }

    pub fn set_base_transfer_cost(&self, value: u64) {
        self.set_param_raw("base_transfer_cost", &value.to_le_bytes());
    }

    /// Governance-set override for the per-byte gas cost.
    pub fn get_cost_per_byte_override(&self) -> Option<u64> {
        let raw = self.get_param_raw("cost_per_byte")?;
        if raw.len() != 8 {
            return None;
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&raw);
        Some(u64::from_le_bytes(buf))
    }

    pub fn set_cost_per_byte(&self, value: u64) {
        self.set_param_raw("cost_per_byte", &value.to_le_bytes());
    }

    /// Governance-set override for the double-sign slash basis points.
    pub fn get_slash_double_sign_bps_override(&self) -> Option<u16> {
        let raw = self.get_param_raw("slash_double_sign_bps")?;
        if raw.len() != 2 {
            return None;
        }
        let mut buf = [0u8; 2];
        buf.copy_from_slice(&raw);
        Some(u16::from_le_bytes(buf))
    }

    pub fn set_slash_double_sign_bps(&self, value: u16) {
        self.set_param_raw("slash_double_sign_bps", &value.to_le_bytes());
    }

    /// Governance-set override for the downtime slash basis points.
    pub fn get_slash_downtime_bps_override(&self) -> Option<u16> {
        let raw = self.get_param_raw("slash_downtime_bps")?;
        if raw.len() != 2 {
            return None;
        }
        let mut buf = [0u8; 2];
        buf.copy_from_slice(&raw);
        Some(u16::from_le_bytes(buf))
    }

    pub fn set_slash_downtime_bps(&self, value: u16) {
        self.set_param_raw("slash_downtime_bps", &value.to_le_bytes());
    }

    /// Governance-set override for the downtime threshold (percentage).
    pub fn get_downtime_threshold_pct_override(&self) -> Option<u8> {
        let raw = self.get_param_raw("downtime_threshold_pct")?;
        if raw.len() != 1 {
            return None;
        }
        Some(raw[0])
    }

    pub fn set_downtime_threshold_pct(&self, value: u8) {
        self.set_param_raw("downtime_threshold_pct", &[value]);
    }

    /// Governance-set override for the unbonding period in seconds.
    pub fn get_unbonding_period_secs_override(&self) -> Option<u64> {
        let raw = self.get_param_raw("unbonding_period_secs")?;
        if raw.len() != 8 {
            return None;
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&raw);
        Some(u64::from_le_bytes(buf))
    }

    pub fn set_unbonding_period_secs(&self, value: u64) {
        self.set_param_raw("unbonding_period_secs", &value.to_le_bytes());
    }

    /// Governance-set maximum unique holders allowed per tokenized asset.
    /// 0 or unset = unlimited.
    pub fn get_max_holders_per_asset(&self) -> u32 {
        let raw = match self.get_param_raw("max_holders_per_asset") {
            Some(r) if r.len() == 4 => r,
            _ => return 0,
        };
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&raw);
        u32::from_le_bytes(buf)
    }

    pub fn set_max_holders_per_asset(&self, value: u32) {
        self.set_param_raw("max_holders_per_asset", &value.to_le_bytes());
    }

    /// Governance-set jurisdiction whitelist (ISO 3166-1 alpha-2 codes).
    /// Empty = no whitelist restriction. Stored as comma-separated ascii.
    pub fn get_jurisdiction_whitelist(&self) -> Vec<String> {
        decode_country_list(self.get_param_raw("jurisdiction_whitelist"))
    }

    pub fn set_jurisdiction_whitelist(&self, codes_csv: &str) {
        self.set_param_raw("jurisdiction_whitelist", codes_csv.as_bytes());
    }

    /// Governance-set jurisdiction blacklist. Empty = no blacklist.
    pub fn get_jurisdiction_blacklist(&self) -> Vec<String> {
        decode_country_list(self.get_param_raw("jurisdiction_blacklist"))
    }

    pub fn set_jurisdiction_blacklist(&self, codes_csv: &str) {
        self.set_param_raw("jurisdiction_blacklist", codes_csv.as_bytes());
    }

    /// Get the jurisdiction (ISO 3166-1 alpha-2) recorded for an address, if any.
    pub fn get_address_jurisdiction(&self, addr: &Address) -> Option<String> {
        let storage = self.storage.as_ref()?;
        let mut k = b"jur:".to_vec();
        k.extend_from_slice(addr.as_bytes());
        let bytes = storage.get(Column::ChainMeta, &k).ok().flatten()?;
        String::from_utf8(bytes).ok()
    }

    /// Set the jurisdiction code for an address. Empty clears the mapping.
    pub fn set_address_jurisdiction(&self, addr: &Address, country: &str) {
        if let Some(ref storage) = self.storage {
            let mut k = b"jur:".to_vec();
            k.extend_from_slice(addr.as_bytes());
            if country.is_empty() {
                let _ = storage.delete(Column::ChainMeta, &k);
            } else {
                let _ = storage.put(Column::ChainMeta, &k, country.as_bytes());
            }
        }
    }

    /// Check whether a bridge deposit with this source tx hash has already
    /// been credited on this chain. Prevents replay of relayer-submitted deposits.
    pub fn bridge_deposit_processed(&self, source_tx_hash: &H256) -> bool {
        if let Some(ref storage) = self.storage {
            let mut key = b"bridge:deposit:".to_vec();
            key.extend_from_slice(source_tx_hash.as_bytes());
            if let Ok(Some(_)) = storage.get(Column::ChainMeta, &key) {
                return true;
            }
        }
        false
    }

    /// Mark a bridge deposit as processed (call after successful credit).
    pub fn mark_bridge_deposit_processed(&mut self, source_tx_hash: &H256) {
        if let Some(ref storage) = self.storage {
            let mut key = b"bridge:deposit:".to_vec();
            key.extend_from_slice(source_tx_hash.as_bytes());
            let _ = storage.put(Column::ChainMeta, &key, &[1u8]);
        }
    }

    /// Get the next governance proposal ID and atomically increment the counter.
    /// The counter is persisted in ChainMeta storage so IDs are unique across blocks.
    pub fn next_governance_id(&mut self) -> u64 {
        let key = b"governance:next_id";
        let current = if let Some(ref storage) = self.storage {
            storage
                .get(Column::ChainMeta, key)
                .ok()
                .flatten()
                .map(|v| {
                    let mut bytes = [0u8; 8];
                    bytes.copy_from_slice(&v);
                    u64::from_be_bytes(bytes)
                })
                .unwrap_or(0)
        } else {
            0
        };
        let next = current + 1;
        if let Some(ref storage) = self.storage {
            let _ = storage.put(Column::ChainMeta, key, &next.to_be_bytes());
        }
        current
    }

    /// Update an existing asset record in the registry.
    pub fn put_asset(&mut self, asset_id: &H256, asset: &AssetRecord) {
        self.dirty_assets.push(*asset_id);
        if let Some(ref storage) = self.storage {
            if let Ok(bytes) = borsh::to_vec(asset) {
                let _ = storage.put(Column::Assets, asset_id.as_bytes(), &bytes);
            }
        }
        self.assets.insert(*asset_id, asset.clone());
    }

    // --- Slashing Methods ---

    /// Apply a slash to a validator's stake. Reduces total_stake and self_stake by the
    /// slash amount (capped to the available stake). Returns the actual amount slashed.
    pub fn apply_slash(&mut self, validator: &Address, amount: Amount) -> Amount {
        let mut account = self.get_account(validator);
        let staking = match account.staking.as_mut() {
            Some(s) => s,
            None => return Amount::ZERO,
        };

        // Cap slash to total_stake
        let slash = if amount > staking.total_stake {
            staking.total_stake
        } else {
            amount
        };

        // Reduce self_stake first, overflow goes to delegations proportionally
        let from_self = if slash > staking.self_stake {
            staking.self_stake
        } else {
            slash
        };
        staking.self_stake = staking.self_stake - from_self;
        staking.total_stake = staking.total_stake - slash;

        self.put_account(*validator, account);
        slash
    }

    /// Record a slashing event in persistent storage for audit/query purposes.
    /// Writes two keys: the per-event `slash:<addr><epoch>` entry and a
    /// per-validator `slash:history:<addr>` blob containing the accumulated
    /// history as Vec<(epoch, reason, amount_raw)> for easy enumeration.
    pub fn record_slash(&mut self, validator: &Address, epoch: u64, reason: &str, amount: Amount) {
        if let Some(ref storage) = self.storage {
            let mut key = b"slash:".to_vec();
            key.extend_from_slice(validator.as_bytes());
            key.extend_from_slice(&epoch.to_be_bytes());
            let value = format!("{}:{}", reason, amount.raw());
            let _ = storage.put(Column::ChainMeta, &key, value.as_bytes());

            // Append to per-validator history blob
            let mut history = self.slashing_history_raw(validator);
            history.push((epoch, reason.to_string(), amount.raw()));
            let mut hist_key = b"slash:history:".to_vec();
            hist_key.extend_from_slice(validator.as_bytes());
            if let Ok(bytes) = borsh::to_vec(&history) {
                let _ = storage.put(Column::ChainMeta, &hist_key, &bytes);
            }
        }
    }

    /// Read the raw slashing history blob for a validator.
    /// Returns Vec<(epoch, reason, amount_raw)>.
    pub fn slashing_history_raw(&self, validator: &Address) -> Vec<(u64, String, u128)> {
        if let Some(ref storage) = self.storage {
            let mut key = b"slash:history:".to_vec();
            key.extend_from_slice(validator.as_bytes());
            if let Ok(Some(bytes)) = storage.get(Column::ChainMeta, &key) {
                if let Ok(items) = borsh::from_slice::<Vec<(u64, String, u128)>>(&bytes) {
                    return items;
                }
            }
        }
        Vec::new()
    }

    /// Slashing history for a validator, as RPC-ready records.
    pub fn slashing_history(&self, validator: &Address) -> Vec<crate::SlashRecord> {
        self.slashing_history_raw(validator)
            .into_iter()
            .map(|(epoch, reason, amount_raw)| crate::SlashRecord {
                validator: *validator,
                epoch,
                reason,
                amount: Amount::from_raw(amount_raw),
            })
            .collect()
    }

    /// Check if slashing evidence for (offender, epoch, slot) has already been
    /// processed. Used to ensure evidence submissions are idempotent.
    pub fn slashing_evidence_seen(&self, offender: &Address, epoch: u64, slot: u32) -> bool {
        self.slashing_seen.contains(&(*offender, epoch, slot))
    }

    /// Mark slashing evidence as processed (prevents double-slashing).
    pub fn mark_slashing_evidence(&mut self, offender: Address, epoch: u64, slot: u32) {
        self.slashing_seen.insert((offender, epoch, slot));
        self.persist_slashing_seen();
    }

    /// Record the hash of a block committed at (validator, epoch, slot).
    /// Returns the previously recorded hash if any — a caller that sees a
    /// Some(other_hash) where other_hash != new_hash has detected a
    /// double-sign.
    pub fn record_block_commitment(
        &mut self,
        validator: Address,
        epoch: u64,
        slot: u32,
        hash: H256,
    ) -> Option<H256> {
        let key = (validator, epoch, slot);
        let prior = self.block_commitments.get(&key).copied();
        self.block_commitments.insert(key, hash);
        if prior.is_none() {
            self.persist_block_commitments();
        }
        prior
    }

    fn persist_block_commitments(&self) {
        if let Some(ref storage) = self.storage {
            let items: Vec<((Address, u64, u32), H256)> = self
                .block_commitments
                .iter()
                .map(|(k, v)| (*k, *v))
                .collect();
            if let Ok(bytes) = borsh::to_vec(&items) {
                let _ = storage.put(Column::ChainMeta, b"block:commitments", &bytes);
            }
        }
    }

    /// Increment the missed-slot counter for a validator in a given epoch.
    pub fn record_missed_slot(&mut self, epoch: u64, validator: Address) {
        let counter = self.missed_slots.entry((epoch, validator)).or_insert(0);
        *counter = counter.saturating_add(1);
        self.persist_missed_slots();
    }

    /// Get (and remove) missed slot counts for a whole epoch. Used at epoch
    /// rotation to apply downtime slashing and reset the counters.
    pub fn take_missed_slots_for_epoch(&mut self, epoch: u64) -> Vec<(Address, u32)> {
        let mut out = Vec::new();
        self.missed_slots.retain(|(e, addr), count| {
            if *e == epoch {
                out.push((*addr, *count));
                false
            } else {
                true
            }
        });
        self.persist_missed_slots();
        out
    }

    fn persist_missed_slots(&self) {
        if let Some(ref storage) = self.storage {
            let items: Vec<((u64, Address), u32)> =
                self.missed_slots.iter().map(|(k, v)| (*k, *v)).collect();
            if let Ok(bytes) = borsh::to_vec(&items) {
                let _ = storage.put(Column::ChainMeta, b"missed:slots", &bytes);
            }
        }
    }

    /// Check whether an address has been KYC-approved on this chain.
    pub fn is_kyc_approved(&self, address: &Address) -> bool {
        self.kyc_approved.contains(address)
    }

    /// Set or clear the KYC approval flag for an address. Typically only
    /// callable by the treasury / admin via a governance-gated transaction.
    pub fn set_kyc_approved(&mut self, address: &Address, approved: bool) {
        if approved {
            self.kyc_approved.insert(*address);
        } else {
            self.kyc_approved.remove(address);
        }
        if let Some(ref storage) = self.storage {
            let items: Vec<Address> = self.kyc_approved.iter().copied().collect();
            if let Ok(bytes) = borsh::to_vec(&items) {
                let _ = storage.put(Column::ChainMeta, b"kyc:approved", &bytes);
            }
        }
    }

    /// Rewrite the slashing_seen blob to storage. Called after mutations.
    fn persist_slashing_seen(&self) {
        if let Some(ref storage) = self.storage {
            let items: Vec<(Address, u64, u32)> = self.slashing_seen.iter().copied().collect();
            if let Ok(bytes) = borsh::to_vec(&items) {
                let _ = storage.put(Column::ChainMeta, b"slashing:seen", &bytes);
            }
        }
    }

    /// Rewrite the unbonding_entries blob to storage. Called after mutations.
    fn persist_unbonding_entries(&self) {
        if let Some(ref storage) = self.storage {
            let items: Vec<(Address, Vec<UnbondingEntry>)> = self
                .unbonding_entries
                .iter()
                .map(|(a, v)| (*a, v.clone()))
                .collect();
            if let Ok(bytes) = borsh::to_vec(&items) {
                let _ = storage.put(Column::ChainMeta, b"unbonding:all", &bytes);
            }
        }
    }

    /// Load slashing_seen and unbonding_entries from storage on startup.
    /// Called by `with_storage` so the restart path starts with the same
    /// contents the validator was running with when it stopped.
    fn load_non_fallthrough_state(&mut self) {
        let storage = match &self.storage {
            Some(s) => s.clone(),
            None => return,
        };
        if let Ok(Some(bytes)) = storage.get(Column::ChainMeta, b"slashing:seen") {
            if let Ok(items) = borsh::from_slice::<Vec<(Address, u64, u32)>>(&bytes) {
                self.slashing_seen = items.into_iter().collect();
            }
        }
        if let Ok(Some(bytes)) = storage.get(Column::ChainMeta, b"unbonding:all") {
            if let Ok(items) = borsh::from_slice::<Vec<(Address, Vec<UnbondingEntry>)>>(&bytes) {
                self.unbonding_entries = items.into_iter().collect();
            }
        }
        if let Ok(Some(bytes)) = storage.get(Column::ChainMeta, b"kyc:approved") {
            if let Ok(items) = borsh::from_slice::<Vec<Address>>(&bytes) {
                self.kyc_approved = items.into_iter().collect();
            }
        }
        if let Ok(Some(bytes)) = storage.get(Column::ChainMeta, b"block:commitments") {
            if let Ok(items) = borsh::from_slice::<Vec<((Address, u64, u32), H256)>>(&bytes) {
                self.block_commitments = items.into_iter().collect();
            }
        }
        if let Ok(Some(bytes)) = storage.get(Column::ChainMeta, b"missed:slots") {
            if let Ok(items) = borsh::from_slice::<Vec<((u64, Address), u32)>>(&bytes) {
                self.missed_slots = items.into_iter().collect();
            }
        }
        if let Ok(Some(bytes)) = storage.get(Column::ChainMeta, b"stakers:set") {
            if let Ok(items) = borsh::from_slice::<Vec<Address>>(&bytes) {
                self.stakers = items.into_iter().collect();
            }
        }
        // Warm the account cache for every staker so elect_validators (which
        // iterates the in-memory map) sees their staking state after resume.
        let addrs: Vec<Address> = self.stakers.iter().copied().collect();
        for addr in addrs {
            if let Ok(Some(bytes)) = storage.get(Column::Accounts, addr.as_bytes()) {
                if let Ok(account) = borsh::from_slice::<AccountState>(&bytes) {
                    self.accounts.insert(addr, account);
                }
            }
        }

        // Warm the asset / pool / proposal / oracle caches via prefix_scan.
        // Without this the corresponding iter_* methods (used by explorer +
        // governance UI) return empty until a new entry is created, even
        // though the data is present on disk and `get_*` returns it.
        if let Ok(entries) = storage.prefix_scan(Column::Assets, b"") {
            for (key, value) in entries {
                if key.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&key);
                    if let Ok(asset) = borsh::from_slice::<AssetRecord>(&value) {
                        self.assets.insert(H256::from(arr), asset);
                    }
                }
            }
        }
        if let Ok(entries) = storage.prefix_scan(Column::Pools, b"") {
            for (key, value) in entries {
                if key.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&key);
                    self.pools.insert(H256::from(arr), value);
                }
            }
        }
        if let Ok(entries) = storage.prefix_scan(Column::MiningStates, b"") {
            for (key, value) in entries {
                if key.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&key);
                    self.mining_states.insert(H256::from(arr), value);
                }
            }
        }
        if let Ok(entries) = storage.prefix_scan(Column::AssetProposals, b"") {
            for (key, value) in entries {
                if key.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&key);
                    if let Ok(proposal) = borsh::from_slice::<AssetProposal>(&value) {
                        self.asset_proposals.insert(H256::from(arr), proposal);
                    }
                }
            }
        }
        if let Ok(entries) = storage.prefix_scan(Column::Oracles, b"") {
            for (key, value) in entries {
                if key.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&key);
                    if let Ok(feed) = borsh::from_slice::<OracleFeed>(&value) {
                        self.oracles.insert(H256::from(arr), feed);
                    }
                }
            }
        }
        // Governance proposals live in ChainMeta with the "gov:" prefix.
        if let Ok(entries) = storage.prefix_scan(Column::ChainMeta, b"gov:") {
            for (key, value) in entries {
                if key.len() == 4 + 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&key[4..]);
                    self.governance_proposals.insert(H256::from(arr), value);
                }
            }
        }
        // Ownership records: key = asset_id (32) || owner (20) = 52 bytes.
        if let Ok(entries) = storage.prefix_scan(Column::Ownership, b"") {
            for (key, value) in entries {
                if key.len() == 52 {
                    let mut asset_arr = [0u8; 32];
                    asset_arr.copy_from_slice(&key[..32]);
                    let mut owner_arr = [0u8; 20];
                    owner_arr.copy_from_slice(&key[32..]);
                    if let Ok(record) = borsh::from_slice::<OwnershipRecord>(&value) {
                        self.ownership
                            .insert((H256::from(asset_arr), Address::from(owner_arr)), record);
                    }
                }
            }
        }
        // Contract code: key = code_hash (32 bytes).
        if let Ok(entries) = storage.prefix_scan(Column::ContractCode, b"") {
            for (key, value) in entries {
                if key.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&key);
                    self.contract_code.insert(H256::from(arr), value);
                }
            }
        }
        // Contract storage: key = contract (20) || storage_key (any length).
        if let Ok(entries) = storage.prefix_scan(Column::ContractStorage, b"") {
            for (key, value) in entries {
                if key.len() >= 20 {
                    let mut contract_arr = [0u8; 20];
                    contract_arr.copy_from_slice(&key[..20]);
                    let storage_key = key[20..].to_vec();
                    self.contract_storage
                        .insert((Address::from(contract_arr), storage_key), value);
                }
            }
        }

        // Rebuild the Merkle trie from all warmed state so compute_state_root
        // yields the same root the previous node computed. Without this the
        // trie is empty post-restart and only reflects the blocks produced
        // after the restart, causing state root divergence vs any node that
        // has been continuously running.
        self.rebuild_trie_from_cache();
    }

    /// Insert every cached state entry into the Merkle trie without marking
    /// anything dirty. Must stay in lockstep with `compute_state_root` — the
    /// same key schemes and same value encodings apply.
    fn rebuild_trie_from_cache(&mut self) {
        for (addr, acc) in &self.accounts {
            if acc.is_empty() {
                continue;
            }
            let key = addr.as_bytes().to_vec();
            if let Ok(value) = borsh::to_vec(acc) {
                self.trie.insert(key, value);
            }
        }
        for (asset_id, asset) in &self.assets {
            let mut key = b"asset:".to_vec();
            key.extend_from_slice(asset_id.as_bytes());
            if let Ok(value) = borsh::to_vec(asset) {
                self.trie.insert(key, value);
            }
        }
        for (pool_id, data) in &self.pools {
            let mut key = b"pool:".to_vec();
            key.extend_from_slice(pool_id.as_bytes());
            self.trie.insert(key, data.clone());
        }
        for ((asset_id, owner), rec) in &self.ownership {
            let mut key = b"own:".to_vec();
            key.extend_from_slice(asset_id.as_bytes());
            key.extend_from_slice(owner.as_bytes());
            if let Ok(value) = borsh::to_vec(rec) {
                self.trie.insert(key, value);
            }
        }
        for (feed_id, feed) in &self.oracles {
            let mut key = b"oracle:".to_vec();
            key.extend_from_slice(feed_id.as_bytes());
            if let Ok(value) = borsh::to_vec(feed) {
                self.trie.insert(key, value);
            }
        }
        for (pool_id, data) in &self.mining_states {
            let mut key = b"mining:".to_vec();
            key.extend_from_slice(pool_id.as_bytes());
            self.trie.insert(key, data.clone());
        }
        for (code_hash, code) in &self.contract_code {
            let mut key = b"code:".to_vec();
            key.extend_from_slice(code_hash.as_bytes());
            self.trie.insert(key, code.clone());
        }
        for ((contract, k), v) in &self.contract_storage {
            let mut key = b"cs:".to_vec();
            key.extend_from_slice(contract.as_bytes());
            key.extend_from_slice(k);
            self.trie.insert(key, v.clone());
        }
        for (proposal_id, proposal) in &self.asset_proposals {
            let mut key = b"aprop:".to_vec();
            key.extend_from_slice(proposal_id.as_bytes());
            if let Ok(value) = borsh::to_vec(proposal) {
                self.trie.insert(key, value);
            }
        }
        for (proposal_id, data) in &self.governance_proposals {
            let mut key = b"gprop:".to_vec();
            key.extend_from_slice(proposal_id.as_bytes());
            self.trie.insert(key, data.clone());
        }
    }

    /// Attach a persistent storage backend to a StateDB that was created
    /// without one. All current in-memory contents are flushed to disk so
    /// that subsequent reads and the restart path see a consistent snapshot.
    pub fn attach_storage(&mut self, storage: Arc<dyn KeyValueStore>) {
        // Accounts
        for (addr, acc) in &self.accounts {
            if let Ok(bytes) = borsh::to_vec(acc) {
                let _ = storage.put(Column::Accounts, addr.as_bytes(), &bytes);
            }
        }
        // Assets
        for (id, asset) in &self.assets {
            if let Ok(bytes) = borsh::to_vec(asset) {
                let _ = storage.put(Column::Assets, id.as_bytes(), &bytes);
            }
        }
        // Ownership — key = asset_id || owner
        for ((asset_id, owner), rec) in &self.ownership {
            if let Ok(bytes) = borsh::to_vec(rec) {
                let mut key = Vec::with_capacity(32 + 20);
                key.extend_from_slice(asset_id.as_bytes());
                key.extend_from_slice(owner.as_bytes());
                let _ = storage.put(Column::Ownership, &key, &bytes);
            }
        }
        // Oracles
        for (feed_id, feed) in &self.oracles {
            if let Ok(bytes) = borsh::to_vec(feed) {
                let _ = storage.put(Column::Oracles, feed_id.as_bytes(), &bytes);
            }
        }
        // Contract code
        for (hash, code) in &self.contract_code {
            let _ = storage.put(Column::ContractCode, hash.as_bytes(), code);
        }
        // Contract storage — key = contract || k
        for ((contract, k), v) in &self.contract_storage {
            let mut key = Vec::with_capacity(20 + k.len());
            key.extend_from_slice(contract.as_bytes());
            key.extend_from_slice(k);
            let _ = storage.put(Column::ContractStorage, &key, v);
        }
        // Pools
        for (pool_id, data) in &self.pools {
            let _ = storage.put(Column::Pools, pool_id.as_bytes(), data);
        }
        // Mining states
        for (pool_id, data) in &self.mining_states {
            let _ = storage.put(Column::MiningStates, pool_id.as_bytes(), data);
        }
        // Asset proposals
        for (id, proposal) in &self.asset_proposals {
            if let Ok(bytes) = borsh::to_vec(proposal) {
                let _ = storage.put(Column::AssetProposals, id.as_bytes(), &bytes);
            }
        }
        // Governance proposals — key = "gov:" || id
        for (id, data) in &self.governance_proposals {
            let key = [b"gov:", id.as_bytes().as_slice()].concat();
            let _ = storage.put(Column::ChainMeta, &key, data);
        }
        // Scalar chain meta
        if self.treasury_address != Address::ZERO {
            let _ = storage.put(
                Column::ChainMeta,
                b"treasury_address",
                self.treasury_address.as_bytes(),
            );
        }
        let _ = storage.put(
            Column::ChainMeta,
            b"epoch_length",
            &self.epoch_length.to_le_bytes(),
        );
        let _ = storage.put(
            Column::ChainMeta,
            b"dex:paused",
            if self.dex_paused { &[1u8] } else { &[0u8] },
        );

        self.storage = Some(storage);

        // Stamp / verify the schema version before anything else writes through
        // the attached storage. Panics on mismatch.
        self.verify_or_stamp_schema_version();

        // Now that storage is attached, flush unbonding + slashing dedup set + KYC
        self.persist_unbonding_entries();
        self.persist_slashing_seen();
        if !self.kyc_approved.is_empty() {
            if let Some(ref storage) = self.storage {
                let items: Vec<Address> = self.kyc_approved.iter().copied().collect();
                if let Ok(bytes) = borsh::to_vec(&items) {
                    let _ = storage.put(Column::ChainMeta, b"kyc:approved", &bytes);
                }
            }
        }
        self.persist_block_commitments();
        self.persist_missed_slots();

        // Rebuild the stakers index from the current in-memory accounts, then
        // persist it. This ensures a freshly adopted genesis_state has its
        // staker list on disk for the next restart.
        self.stakers.clear();
        for (addr, acc) in &self.accounts {
            if let Some(ref s) = acc.staking {
                if !s.self_stake.is_zero() {
                    self.stakers.insert(*addr);
                }
            }
        }
        self.persist_stakers();
    }

    // --- Finality Methods ---

    /// Persist the finalized block number.
    pub fn set_finalized_block(&mut self, number: u64) {
        if let Some(ref storage) = self.storage {
            let _ = storage.put(Column::ChainMeta, b"finalized_block", &number.to_be_bytes());
        }
    }

    /// Read the persisted finalized block number (0 if not yet set).
    pub fn finalized_block(&self) -> u64 {
        if let Some(ref storage) = self.storage {
            if let Ok(Some(v)) = storage.get(Column::ChainMeta, b"finalized_block") {
                if v.len() == 8 {
                    let mut bytes = [0u8; 8];
                    bytes.copy_from_slice(&v);
                    return u64::from_be_bytes(bytes);
                }
            }
        }
        0
    }

    // --- Protocol Governance Methods ---

    /// Store a protocol governance proposal (serialized).
    pub fn put_governance_proposal(&mut self, id: H256, data: Vec<u8>) {
        if let Some(ref storage) = self.storage {
            let _ = storage.put(
                Column::ChainMeta,
                &[b"gov:", id.as_bytes().as_slice()].concat(),
                &data,
            );
        }
        self.governance_proposals.insert(id, data);
    }

    /// Get a protocol governance proposal by ID (raw bytes).
    pub fn get_governance_proposal(&self, id: &H256) -> Option<&[u8]> {
        self.governance_proposals.get(id).map(|v| v.as_slice())
    }

    /// Get a protocol governance proposal by ID (owned, with storage fallback).
    pub fn get_governance_proposal_owned(&self, id: &H256) -> Option<Vec<u8>> {
        if let Some(data) = self.governance_proposals.get(id) {
            return Some(data.clone());
        }
        if let Some(ref storage) = self.storage {
            if let Ok(Some(bytes)) = storage.get(
                Column::ChainMeta,
                &[b"gov:", id.as_bytes().as_slice()].concat(),
            ) {
                return Some(bytes);
            }
        }
        None
    }

    /// Iterate all protocol governance proposals (id, raw bytes).
    pub fn iter_governance_proposals(&self) -> impl Iterator<Item = (&H256, &Vec<u8>)> {
        self.governance_proposals.iter()
    }

    // --- Unbonding Entry Methods ---

    /// Add an unbonding entry for an address.
    pub fn add_unbonding_entry(&mut self, address: Address, entry: UnbondingEntry) {
        let entries = self.unbonding_entries.entry(address).or_default();
        entries.push(entry);
        self.persist_unbonding_entries();
    }

    /// Process matured unbonding entries: release funds for entries whose completion_time <= current_timestamp.
    /// Returns the total amount released across all addresses.
    pub fn process_unbonding(&mut self, current_timestamp: Timestamp) -> Amount {
        let mut total_released = Amount::ZERO;
        let mut releases: Vec<(Address, Amount)> = Vec::new();

        for (addr, entries) in self.unbonding_entries.iter_mut() {
            let mut released = Amount::ZERO;
            entries.retain(|e| {
                if e.completion_time <= current_timestamp {
                    released = released + e.amount;
                    false // remove matured entries
                } else {
                    true // keep pending entries
                }
            });
            if !released.is_zero() {
                releases.push((*addr, released));
                total_released = total_released + released;
            }
        }

        // Credit released amounts
        for (addr, amount) in releases {
            let _ = self.add_balance(&addr, amount);
        }

        // Clean up empty entries
        self.unbonding_entries.retain(|_, v| !v.is_empty());

        // Persist any removals/credits that occurred
        if !total_released.is_zero() {
            self.persist_unbonding_entries();
        }

        total_released
    }

    /// Get unbonding entries for an address.
    pub fn get_unbonding_entries(&self, address: &Address) -> &[UnbondingEntry] {
        self.unbonding_entries
            .get(address)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Compute the state root.
    ///
    /// Flushes every piece of state that contributes to consensus into the
    /// Merkle trie: accounts, assets, ownership records, DEX pools and
    /// mining state, contract code and storage, oracle feeds.
    ///
    /// Dirty-drain is used for the collections that have it (accounts,
    /// assets, pools) — they track writes incrementally. The other
    /// collections don't have dirty tracking yet, so we conservatively
    /// flush the full HashMap. All collections must be covered here
    /// otherwise restarted nodes whose trie was rebuilt from storage
    /// would compute a different root than nodes with hot in-memory state.
    pub fn compute_state_root(&mut self) -> H256 {
        for addr in self.dirty.drain(..) {
            if let Some(account) = self.accounts.get(&addr) {
                let key = addr.as_bytes().to_vec();
                if account.is_empty() {
                    self.trie.remove(&key);
                } else {
                    let value = borsh::to_vec(account).expect("account serialization failed");
                    self.trie.insert(key, value);
                }
            }
        }

        // Flush dirty assets into the trie
        for asset_id in self.dirty_assets.drain(..) {
            if let Some(asset) = self.assets.get(&asset_id) {
                let mut key = b"asset:".to_vec();
                key.extend_from_slice(asset_id.as_bytes());
                let value = borsh::to_vec(asset).expect("asset serialization failed");
                self.trie.insert(key, value);
            }
        }

        for pool_id in self.dirty_pools.drain(..) {
            if let Some(pool_data) = self.pools.get(&pool_id) {
                let mut key = b"pool:".to_vec();
                key.extend_from_slice(pool_id.as_bytes());
                self.trie.insert(key, pool_data.clone());
            }
        }

        // Ownership records: key = "own:" || asset_id || owner
        for ((asset_id, owner), rec) in &self.ownership {
            let mut key = b"own:".to_vec();
            key.extend_from_slice(asset_id.as_bytes());
            key.extend_from_slice(owner.as_bytes());
            if let Ok(value) = borsh::to_vec(rec) {
                self.trie.insert(key, value);
            }
        }
        // Oracle feeds: key = "oracle:" || feed_id
        for (feed_id, feed) in &self.oracles {
            let mut key = b"oracle:".to_vec();
            key.extend_from_slice(feed_id.as_bytes());
            if let Ok(value) = borsh::to_vec(feed) {
                self.trie.insert(key, value);
            }
        }
        // Mining state: key = "mining:" || pool_id
        for (pool_id, data) in &self.mining_states {
            let mut key = b"mining:".to_vec();
            key.extend_from_slice(pool_id.as_bytes());
            self.trie.insert(key, data.clone());
        }
        // Contract code: key = "code:" || code_hash
        for (code_hash, code) in &self.contract_code {
            let mut key = b"code:".to_vec();
            key.extend_from_slice(code_hash.as_bytes());
            self.trie.insert(key, code.clone());
        }
        // Contract storage: key = "cs:" || contract || k
        for ((contract, k), v) in &self.contract_storage {
            let mut key = b"cs:".to_vec();
            key.extend_from_slice(contract.as_bytes());
            key.extend_from_slice(k);
            self.trie.insert(key, v.clone());
        }
        // Asset proposals: key = "aprop:" || proposal_id
        for (proposal_id, proposal) in &self.asset_proposals {
            let mut key = b"aprop:".to_vec();
            key.extend_from_slice(proposal_id.as_bytes());
            if let Ok(value) = borsh::to_vec(proposal) {
                self.trie.insert(key, value);
            }
        }
        // Governance proposals: key = "gprop:" || proposal_id
        for (proposal_id, data) in &self.governance_proposals {
            let mut key = b"gprop:".to_vec();
            key.extend_from_slice(proposal_id.as_bytes());
            self.trie.insert(key, data.clone());
        }

        self.trie.root()
    }

    /// Get the number of accounts in the state.
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    /// Iterate over all accounts.
    pub fn iter_accounts(&self) -> impl Iterator<Item = (&Address, &AccountState)> {
        self.accounts.iter()
    }

    /// Create a snapshot (clone) of the current state for rollback purposes.
    pub fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            accounts: self.accounts.clone(),
            assets: self.assets.clone(),
            ownership: self.ownership.clone(),
            oracles: self.oracles.clone(),
            contract_code: self.contract_code.clone(),
            contract_storage: self.contract_storage.clone(),
            pools: self.pools.clone(),
            asset_proposals: self.asset_proposals.clone(),
            mining_states: self.mining_states.clone(),
            governance_proposals: self.governance_proposals.clone(),
            unbonding_entries: self.unbonding_entries.clone(),
            treasury_address: self.treasury_address,
            epoch_length: self.epoch_length,
            dex_paused: self.dex_paused,
            dirty_pools: self.dirty_pools.clone(),
        }
    }

    /// Restore state from a snapshot, rolling back both the in-memory cache
    /// and any persisted writes that occurred since the snapshot was taken.
    ///
    /// Because `put_*` writes are dual-path (cache + storage) and fire
    /// immediately, a mid-tx failure would leave storage ahead of cache.
    /// After the cache-level restore below we re-persist the entire
    /// snapshot to storage so a subsequent restart reads the rolled-back
    /// state, not the partially-mutated one.
    pub fn restore(&mut self, snapshot: StateSnapshot) {
        for addr in self.accounts.keys() {
            self.dirty.push(*addr);
        }
        for addr in snapshot.accounts.keys() {
            self.dirty.push(*addr);
        }
        for id in self.assets.keys() {
            self.dirty_assets.push(*id);
        }
        for id in snapshot.assets.keys() {
            self.dirty_assets.push(*id);
        }
        for id in self.pools.keys() {
            self.dirty_pools.push(*id);
        }
        for id in snapshot.pools.keys() {
            self.dirty_pools.push(*id);
        }
        self.accounts = snapshot.accounts;
        self.assets = snapshot.assets;
        self.ownership = snapshot.ownership;
        self.oracles = snapshot.oracles;
        self.contract_code = snapshot.contract_code;
        self.contract_storage = snapshot.contract_storage;
        self.pools = snapshot.pools;
        self.asset_proposals = snapshot.asset_proposals;
        self.mining_states = snapshot.mining_states;
        self.governance_proposals = snapshot.governance_proposals;
        self.unbonding_entries = snapshot.unbonding_entries;
        self.treasury_address = snapshot.treasury_address;
        self.epoch_length = snapshot.epoch_length;
        self.dex_paused = snapshot.dex_paused;
        self.dirty_pools = snapshot.dirty_pools;

        // Re-persist the restored cache so storage matches in-memory state.
        // Without this, a mid-tx write that was rolled back would still be
        // visible on restart (storage ahead of cache).
        if let Some(storage) = self.storage.clone() {
            for (addr, acc) in &self.accounts {
                if let Ok(bytes) = borsh::to_vec(acc) {
                    let _ = storage.put(Column::Accounts, addr.as_bytes(), &bytes);
                }
            }
            for (id, asset) in &self.assets {
                if let Ok(bytes) = borsh::to_vec(asset) {
                    let _ = storage.put(Column::Assets, id.as_bytes(), &bytes);
                }
            }
            for ((asset_id, owner), rec) in &self.ownership {
                if let Ok(bytes) = borsh::to_vec(rec) {
                    let mut key = Vec::with_capacity(52);
                    key.extend_from_slice(asset_id.as_bytes());
                    key.extend_from_slice(owner.as_bytes());
                    let _ = storage.put(Column::Ownership, &key, &bytes);
                }
            }
            for (feed_id, feed) in &self.oracles {
                if let Ok(bytes) = borsh::to_vec(feed) {
                    let _ = storage.put(Column::Oracles, feed_id.as_bytes(), &bytes);
                }
            }
            for (code_hash, code) in &self.contract_code {
                let _ = storage.put(Column::ContractCode, code_hash.as_bytes(), code);
            }
            for ((contract, k), v) in &self.contract_storage {
                let mut key = Vec::with_capacity(20 + k.len());
                key.extend_from_slice(contract.as_bytes());
                key.extend_from_slice(k);
                let _ = storage.put(Column::ContractStorage, &key, v);
            }
            for (pool_id, data) in &self.pools {
                let _ = storage.put(Column::Pools, pool_id.as_bytes(), data);
            }
            for (pool_id, data) in &self.mining_states {
                let _ = storage.put(Column::MiningStates, pool_id.as_bytes(), data);
            }
            for (id, proposal) in &self.asset_proposals {
                if let Ok(bytes) = borsh::to_vec(proposal) {
                    let _ = storage.put(Column::AssetProposals, id.as_bytes(), &bytes);
                }
            }
            for (id, data) in &self.governance_proposals {
                let key = [b"gov:", id.as_bytes().as_slice()].concat();
                let _ = storage.put(Column::ChainMeta, &key, data);
            }
        }
    }
}

impl Default for StateDB {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a comma-separated list of country codes into an uppercased Vec<String>.
/// Empty codes are skipped.
fn decode_country_list(bytes: Option<Vec<u8>>) -> Vec<String> {
    let Some(bytes) = bytes else {
        return Vec::new();
    };
    let s = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    s.split(',')
        .map(|c| c.trim().to_ascii_uppercase())
        .filter(|c| !c.is_empty())
        .collect()
}

/// A snapshot of the state for rollback.
#[derive(Clone)]
pub struct StateSnapshot {
    accounts: HashMap<Address, AccountState>,
    assets: HashMap<H256, AssetRecord>,
    ownership: HashMap<(H256, Address), OwnershipRecord>,
    oracles: HashMap<H256, OracleFeed>,
    contract_code: HashMap<H256, Vec<u8>>,
    contract_storage: HashMap<(Address, Vec<u8>), Vec<u8>>,
    pools: HashMap<H256, Vec<u8>>,
    asset_proposals: HashMap<H256, AssetProposal>,
    mining_states: HashMap<H256, Vec<u8>>,
    governance_proposals: HashMap<H256, Vec<u8>>,
    unbonding_entries: HashMap<Address, Vec<UnbondingEntry>>,
    treasury_address: Address,
    epoch_length: u64,
    dex_paused: bool,
    dirty_pools: Vec<H256>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_statedb_is_empty() {
        let mut db = StateDB::new();
        assert_eq!(db.account_count(), 0);
        assert_eq!(db.compute_state_root(), H256::ZERO);
    }

    #[test]
    fn get_nonexistent_returns_default() {
        let db = StateDB::new();
        let acc = db.get_account(&Address::from([0x01; 20]));
        assert!(acc.is_empty());
    }

    #[test]
    fn put_and_get_account() {
        let mut db = StateDB::new();
        let addr = Address::from([0x01; 20]);
        let acc = AccountState::with_balance(Amount::from_vtt(100));
        db.put_account(addr, acc.clone());

        assert_eq!(db.get_account(&addr), acc);
        assert!(db.account_exists(&addr));
    }

    #[test]
    fn add_and_sub_balance() {
        let mut db = StateDB::new();
        let addr = Address::from([0x01; 20]);

        db.add_balance(&addr, Amount::from_vtt(100)).unwrap();
        assert_eq!(db.get_balance(&addr), Amount::from_vtt(100));

        db.sub_balance(&addr, Amount::from_vtt(30)).unwrap();
        assert_eq!(db.get_balance(&addr), Amount::from_vtt(70));
    }

    #[test]
    fn sub_balance_insufficient_fails() {
        let mut db = StateDB::new();
        let addr = Address::from([0x01; 20]);
        db.add_balance(&addr, Amount::from_vtt(10)).unwrap();

        let err = db.sub_balance(&addr, Amount::from_vtt(20));
        assert!(err.is_err());
        assert!(matches!(
            err.unwrap_err(),
            StateError::InsufficientBalance { .. }
        ));
    }

    #[test]
    fn transfer_between_accounts() {
        let mut db = StateDB::new();
        let alice = Address::from([0x01; 20]);
        let bob = Address::from([0x02; 20]);

        db.add_balance(&alice, Amount::from_vtt(100)).unwrap();
        db.transfer(&alice, &bob, Amount::from_vtt(40)).unwrap();

        assert_eq!(db.get_balance(&alice), Amount::from_vtt(60));
        assert_eq!(db.get_balance(&bob), Amount::from_vtt(40));
    }

    #[test]
    fn transfer_insufficient_fails() {
        let mut db = StateDB::new();
        let alice = Address::from([0x01; 20]);
        let bob = Address::from([0x02; 20]);

        db.add_balance(&alice, Amount::from_vtt(10)).unwrap();
        let err = db.transfer(&alice, &bob, Amount::from_vtt(20));
        assert!(err.is_err());

        // Alice should still have 10 (rollback not automatic at this level)
        assert_eq!(db.get_balance(&alice), Amount::from_vtt(10));
    }

    #[test]
    fn increment_nonce() {
        let mut db = StateDB::new();
        let addr = Address::from([0x01; 20]);
        db.add_balance(&addr, Amount::from_vtt(1)).unwrap();

        assert_eq!(db.get_nonce(&addr), 0);
        let old = db.increment_nonce(&addr);
        assert_eq!(old, 0);
        assert_eq!(db.get_nonce(&addr), 1);
    }

    #[test]
    fn state_root_changes_with_state() {
        let mut db = StateDB::new();
        let root_empty = db.compute_state_root();

        let addr = Address::from([0x01; 20]);
        db.add_balance(&addr, Amount::from_vtt(100)).unwrap();
        let root_with_account = db.compute_state_root();

        assert_ne!(root_empty, root_with_account);

        db.add_balance(&addr, Amount::from_vtt(50)).unwrap();
        let root_modified = db.compute_state_root();

        assert_ne!(root_with_account, root_modified);
    }

    #[test]
    fn state_root_deterministic() {
        let mut db1 = StateDB::new();
        let mut db2 = StateDB::new();

        let addr1 = Address::from([0x01; 20]);
        let addr2 = Address::from([0x02; 20]);

        // Same operations, same order
        db1.add_balance(&addr1, Amount::from_vtt(100)).unwrap();
        db1.add_balance(&addr2, Amount::from_vtt(200)).unwrap();

        db2.add_balance(&addr1, Amount::from_vtt(100)).unwrap();
        db2.add_balance(&addr2, Amount::from_vtt(200)).unwrap();

        assert_eq!(db1.compute_state_root(), db2.compute_state_root());
    }

    #[test]
    fn snapshot_and_restore() {
        let mut db = StateDB::new();
        let addr = Address::from([0x01; 20]);
        db.add_balance(&addr, Amount::from_vtt(100)).unwrap();

        let snap = db.snapshot();

        db.add_balance(&addr, Amount::from_vtt(50)).unwrap();
        assert_eq!(db.get_balance(&addr), Amount::from_vtt(150));

        db.restore(snap);
        assert_eq!(db.get_balance(&addr), Amount::from_vtt(100));
    }

    #[test]
    fn with_accounts_constructor() {
        let addr1 = Address::from([0x01; 20]);
        let addr2 = Address::from([0x02; 20]);

        let db = StateDB::with_accounts(vec![
            (addr1, AccountState::with_balance(Amount::from_vtt(100))),
            (addr2, AccountState::with_balance(Amount::from_vtt(200))),
        ]);

        assert_eq!(db.get_balance(&addr1), Amount::from_vtt(100));
        assert_eq!(db.get_balance(&addr2), Amount::from_vtt(200));
        assert_eq!(db.account_count(), 2);
    }

    #[test]
    fn register_asset_round_trip() {
        use crate::asset::{AssetClass, AssetStatus};
        use std::collections::BTreeMap;
        use vtt_primitives::ChainId;

        let mut db = StateDB::new();
        let asset_id = H256::from([0xAA; 32]);
        let issuer = Address::from([0x01; 20]);

        let asset = AssetRecord {
            id: asset_id,
            name: "TestAsset".into(),
            symbol: "TST".into(),
            class: AssetClass::Equity,
            origin_chain: ChainId(1),
            issuer,
            total_supply: Amount::from_vtt(1_000),
            decimals: 18,
            status: AssetStatus::Active,
            compliance_policy: None,
            valuation_oracle: None,
            documents: BTreeMap::new(),
            metadata_uri: String::new(),
            jurisdiction: String::new(),
            legal_entity: String::new(),
            transfer_mode: crate::asset::TransferMode::PeerToPeer,
            registrar: None,
            redemption_pool: Amount::ZERO,
            requires_kyc: false,
            created_at: 0,
        };

        assert_eq!(db.asset_count(), 0);
        db.register_asset(asset.clone()).unwrap();
        assert_eq!(db.asset_count(), 1);

        let retrieved = db.get_asset(&asset_id).expect("asset should exist");
        assert_eq!(retrieved.symbol, "TST");
        assert_eq!(retrieved.issuer, issuer);

        // Duplicate registration should fail
        let err = db.register_asset(asset);
        assert!(err.is_err());
    }

    #[test]
    fn treasury_address_persists() {
        let mut db = StateDB::new();
        assert_eq!(db.get_treasury_address(), Address::ZERO);

        let addr = Address::from([0xBB; 20]);
        db.set_treasury_address(addr);
        assert_eq!(db.get_treasury_address(), addr);
    }

    #[test]
    fn apply_slash_reduces_stake() {
        use crate::account::StakingState;

        let mut db = StateDB::new();
        let val = Address::from([0x10; 20]);
        let mut account = AccountState::with_balance(Amount::from_vtt(500_000));
        account.staking = Some(StakingState {
            total_stake: Amount::from_vtt(100_000),
            self_stake: Amount::from_vtt(100_000),
            commission_bps: 500,
            active: true,
            delegations: Vec::new(),
            unbonding: Vec::new(),
        });
        db.put_account(val, account);

        // Slash 5% = 5,000 VTT
        let slashed = db.apply_slash(&val, Amount::from_vtt(5_000));
        assert_eq!(slashed, Amount::from_vtt(5_000));

        let after = db.get_account(&val);
        let staking = after.staking.unwrap();
        assert_eq!(staking.total_stake, Amount::from_vtt(95_000));
        assert_eq!(staking.self_stake, Amount::from_vtt(95_000));
    }

    #[test]
    fn apply_slash_capped_to_total_stake() {
        use crate::account::StakingState;

        let mut db = StateDB::new();
        let val = Address::from([0x10; 20]);
        let mut account = AccountState::with_balance(Amount::from_vtt(100));
        account.staking = Some(StakingState {
            total_stake: Amount::from_vtt(1_000),
            self_stake: Amount::from_vtt(1_000),
            commission_bps: 0,
            active: true,
            delegations: Vec::new(),
            unbonding: Vec::new(),
        });
        db.put_account(val, account);

        // Slash more than total stake -- should cap
        let slashed = db.apply_slash(&val, Amount::from_vtt(999_999));
        assert_eq!(slashed, Amount::from_vtt(1_000));

        let after = db.get_account(&val);
        let staking = after.staking.unwrap();
        assert_eq!(staking.total_stake, Amount::ZERO);
        assert_eq!(staking.self_stake, Amount::ZERO);
    }

    #[test]
    fn apply_slash_no_staking_returns_zero() {
        let mut db = StateDB::new();
        let val = Address::from([0x10; 20]);
        db.put_account(val, AccountState::with_balance(Amount::from_vtt(100)));

        let slashed = db.apply_slash(&val, Amount::from_vtt(50));
        assert_eq!(slashed, Amount::ZERO);
    }

    #[test]
    fn finalized_block_defaults_to_zero() {
        let db = StateDB::new();
        assert_eq!(db.finalized_block(), 0);
    }

    #[test]
    fn schema_version_stamped_on_fresh_db() {
        use vtt_storage::memory::InMemoryStore;
        let storage = Arc::new(InMemoryStore::new());
        let _db = StateDB::with_storage(storage.clone());
        let bytes = storage
            .get(Column::ChainMeta, SCHEMA_VERSION_KEY)
            .unwrap()
            .expect("schema version should be stamped");
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&bytes);
        assert_eq!(u32::from_le_bytes(buf), DB_SCHEMA_VERSION);
    }

    #[test]
    #[should_panic(expected = "DB schema version mismatch")]
    fn schema_version_mismatch_panics() {
        use vtt_storage::memory::InMemoryStore;
        let storage = Arc::new(InMemoryStore::new());
        // Pre-stamp a future version that this binary doesn't understand.
        storage
            .put(
                Column::ChainMeta,
                SCHEMA_VERSION_KEY,
                &(DB_SCHEMA_VERSION + 1).to_le_bytes(),
            )
            .unwrap();
        let _db = StateDB::with_storage(storage);
    }

    #[test]
    fn schema_version_matches_passes() {
        use vtt_storage::memory::InMemoryStore;
        let storage = Arc::new(InMemoryStore::new());
        storage
            .put(
                Column::ChainMeta,
                SCHEMA_VERSION_KEY,
                &DB_SCHEMA_VERSION.to_le_bytes(),
            )
            .unwrap();
        let _db = StateDB::with_storage(storage);
    }
}
