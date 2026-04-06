pub mod memory;
pub mod rocks;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage I/O error: {0}")]
    Io(String),
    #[error("column family not found: {0}")]
    ColumnNotFound(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// Logical column families for data separation in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Column {
    BlockHeaders,
    BlockBodies,
    Transactions,
    Receipts,
    StateTrie,
    ContractCode,
    ChainIndex,
    CrossChainQueue,
    /// Account state keyed by Address bytes.
    Accounts,
    /// Asset registry keyed by asset H256.
    Assets,
    /// Ownership records keyed by (asset_id, owner).
    Ownership,
    /// Oracle feeds keyed by feed_id H256.
    Oracles,
    /// Contract storage keyed by (address, key).
    ContractStorage,
    /// DEX pools keyed by pool_id H256.
    Pools,
    /// Asset governance proposals keyed by proposal_id H256.
    AssetProposals,
    /// Liquidity mining state keyed by pool_id H256.
    MiningStates,
    /// Chain metadata (e.g., head hash, treasury address, epoch length).
    ChainMeta,
}

impl Column {
    pub const ALL: &[Column] = &[
        Column::BlockHeaders,
        Column::BlockBodies,
        Column::Transactions,
        Column::Receipts,
        Column::StateTrie,
        Column::ContractCode,
        Column::ChainIndex,
        Column::CrossChainQueue,
        Column::Accounts,
        Column::Assets,
        Column::Ownership,
        Column::Oracles,
        Column::ContractStorage,
        Column::Pools,
        Column::AssetProposals,
        Column::MiningStates,
        Column::ChainMeta,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            Column::BlockHeaders => "block_headers",
            Column::BlockBodies => "block_bodies",
            Column::Transactions => "transactions",
            Column::Receipts => "receipts",
            Column::StateTrie => "state_trie",
            Column::ContractCode => "contract_code",
            Column::ChainIndex => "chain_index",
            Column::CrossChainQueue => "cross_chain_queue",
            Column::Accounts => "accounts",
            Column::Assets => "assets",
            Column::Ownership => "ownership",
            Column::Oracles => "oracles",
            Column::ContractStorage => "contract_storage",
            Column::Pools => "pools",
            Column::AssetProposals => "asset_proposals",
            Column::MiningStates => "mining_states",
            Column::ChainMeta => "chain_meta",
        }
    }
}

/// A batch write operation.
#[derive(Debug, Clone)]
pub enum BatchOp {
    Put {
        column: Column,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        column: Column,
        key: Vec<u8>,
    },
}

/// Abstract key-value store trait. Enables testing with in-memory backend.
pub trait KeyValueStore: Send + Sync {
    fn get(&self, column: Column, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn put(&self, column: Column, key: &[u8], value: &[u8]) -> Result<()>;
    fn delete(&self, column: Column, key: &[u8]) -> Result<()>;
    fn write_batch(&self, ops: Vec<BatchOp>) -> Result<()>;
    fn contains(&self, column: Column, key: &[u8]) -> Result<bool> {
        Ok(self.get(column, key)?.is_some())
    }
}
