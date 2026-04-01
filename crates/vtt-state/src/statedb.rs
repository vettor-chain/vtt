use std::collections::HashMap;

use thiserror::Error;

use vtt_primitives::amount::Amount;
use vtt_primitives::asset_governance::AssetProposal;
use vtt_primitives::{Address, H256};

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
    /// The underlying trie for computing state roots.
    trie: StateTrie,
    /// Tracks which accounts have been modified.
    dirty: Vec<Address>,
    /// Tracks which assets have been modified.
    dirty_assets: Vec<H256>,
    /// Tracks which pools have been modified.
    dirty_pools: Vec<H256>,
}

impl StateDB {
    /// Create a new empty state database.
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
            trie: StateTrie::new(),
            dirty: Vec::new(),
            dirty_assets: Vec::new(),
            dirty_pools: Vec::new(),
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
    pub fn get_account(&self, address: &Address) -> AccountState {
        self.accounts.get(address).cloned().unwrap_or_default()
    }

    /// Get account state if it exists.
    pub fn get_account_opt(&self, address: &Address) -> Option<&AccountState> {
        self.accounts.get(address)
    }

    /// Set account state.
    pub fn put_account(&mut self, address: Address, state: AccountState) {
        self.dirty.push(address);
        self.accounts.insert(address, state);
    }

    /// Check if an account exists (has been explicitly set).
    pub fn account_exists(&self, address: &Address) -> bool {
        self.accounts.contains_key(address)
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
        self.dirty_assets.push(asset.id);
        self.assets.insert(asset.id, asset);
        Ok(())
    }

    /// Get an asset record by ID.
    pub fn get_asset(&self, asset_id: &H256) -> Option<&AssetRecord> {
        self.assets.get(asset_id)
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
        self.contract_code.insert(hash, code);
        hash
    }

    /// Get contract bytecode by code hash.
    pub fn get_code(&self, code_hash: &H256) -> Option<&Vec<u8>> {
        self.contract_code.get(code_hash)
    }

    // --- Contract Storage Methods ---

    /// Read a value from a contract's storage.
    pub fn get_contract_storage(&self, address: &Address, key: &[u8]) -> Option<Vec<u8>> {
        self.contract_storage
            .get(&(*address, key.to_vec()))
            .cloned()
    }

    /// Write a value to a contract's storage.
    pub fn put_contract_storage(&mut self, address: Address, key: Vec<u8>, value: Vec<u8>) {
        self.dirty.push(address);
        self.contract_storage.insert((address, key), value);
    }

    /// Delete a value from a contract's storage.
    pub fn delete_contract_storage(&mut self, address: &Address, key: &[u8]) {
        self.dirty.push(*address);
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
        self.pools.get(pool_id).map(|v| v.as_slice())
    }

    pub fn put_pool_raw(&mut self, pool_id: H256, data: Vec<u8>) {
        self.pools.insert(pool_id, data);
        self.dirty_pools.push(pool_id);
    }

    pub fn has_pool(&self, pool_id: &H256) -> bool {
        self.pools.contains_key(pool_id)
    }

    pub fn iter_pools(&self) -> impl Iterator<Item = (&H256, &[u8])> {
        self.pools.iter().map(|(k, v)| (k, v.as_slice()))
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
}
