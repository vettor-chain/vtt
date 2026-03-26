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
