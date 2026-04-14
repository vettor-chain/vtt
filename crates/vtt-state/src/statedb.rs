use std::collections::HashMap;
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
            treasury_address: Address::ZERO,
            epoch_length: 1200,
            dex_paused: false,
            trie: StateTrie::new(),
            dirty: Vec::new(),
            dirty_assets: Vec::new(),
            dirty_pools: Vec::new(),
            storage: Some(storage),
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
        self.accounts.insert(address, state);
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
    pub fn record_slash(&mut self, validator: &Address, epoch: u64, reason: &str, amount: Amount) {
        if let Some(ref storage) = self.storage {
            let mut key = b"slash:".to_vec();
            key.extend_from_slice(validator.as_bytes());
            key.extend_from_slice(&epoch.to_be_bytes());
            let value = format!("{}:{}", reason, amount.raw());
            let _ = storage.put(Column::ChainMeta, &key, value.as_bytes());
        }
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

        total_released
    }

    /// Get unbonding entries for an address.
    pub fn get_unbonding_entries(&self, address: &Address) -> &[UnbondingEntry] {
        self.unbonding_entries
            .get(address)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Compute the state root by flushing dirty accounts and assets into the trie.
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

    /// Restore state from a snapshot.
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
    }
}

impl Default for StateDB {
    fn default() -> Self {
        Self::new()
    }
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
}
