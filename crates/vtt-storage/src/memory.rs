use std::collections::HashMap;
use std::sync::RwLock;

use crate::{BatchOp, Column, KeyValueStore, Result};

type ColumnData = HashMap<Column, HashMap<Vec<u8>, Vec<u8>>>;

/// In-memory key-value store for testing. Thread-safe via RwLock.
pub struct InMemoryStore {
    data: RwLock<ColumnData>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        let mut data = HashMap::new();
        for col in Column::ALL {
            data.insert(*col, HashMap::new());
        }
        Self {
            data: RwLock::new(data),
        }
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyValueStore for InMemoryStore {
    fn get(&self, column: Column, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let data = self.data.read().unwrap();
        Ok(data.get(&column).and_then(|col| col.get(key).cloned()))
    }

    fn put(&self, column: Column, key: &[u8], value: &[u8]) -> Result<()> {
        let mut data = self.data.write().unwrap();
        data.entry(column)
            .or_default()
            .insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&self, column: Column, key: &[u8]) -> Result<()> {
        let mut data = self.data.write().unwrap();
        if let Some(col) = data.get_mut(&column) {
            col.remove(key);
        }
        Ok(())
    }

    fn write_batch(&self, ops: Vec<BatchOp>) -> Result<()> {
        let mut data = self.data.write().unwrap();
        for op in ops {
            match op {
                BatchOp::Put { column, key, value } => {
                    data.entry(column).or_default().insert(key, value);
                }
                BatchOp::Delete { column, key } => {
                    if let Some(col) = data.get_mut(&column) {
                        col.remove(&key);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_get() {
        let store = InMemoryStore::new();
        store.put(Column::BlockHeaders, b"key1", b"value1").unwrap();
        let val = store.get(Column::BlockHeaders, b"key1").unwrap();
        assert_eq!(val, Some(b"value1".to_vec()));
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let store = InMemoryStore::new();
        let val = store.get(Column::BlockHeaders, b"missing").unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn delete_removes_key() {
        let store = InMemoryStore::new();
        store.put(Column::Transactions, b"tx1", b"data").unwrap();
        store.delete(Column::Transactions, b"tx1").unwrap();
        assert_eq!(store.get(Column::Transactions, b"tx1").unwrap(), None);
    }

    #[test]
    fn columns_are_independent() {
        let store = InMemoryStore::new();
        store
            .put(Column::BlockHeaders, b"key", b"header_data")
            .unwrap();
        store.put(Column::Transactions, b"key", b"tx_data").unwrap();

        assert_eq!(
            store.get(Column::BlockHeaders, b"key").unwrap(),
            Some(b"header_data".to_vec())
        );
        assert_eq!(
            store.get(Column::Transactions, b"key").unwrap(),
            Some(b"tx_data".to_vec())
        );
    }

    #[test]
    fn write_batch_atomic() {
        let store = InMemoryStore::new();
        store.put(Column::StateTrie, b"to_delete", b"old").unwrap();

        let ops = vec![
            BatchOp::Put {
                column: Column::StateTrie,
                key: b"new_key".to_vec(),
                value: b"new_value".to_vec(),
            },
            BatchOp::Delete {
                column: Column::StateTrie,
                key: b"to_delete".to_vec(),
            },
        ];

        store.write_batch(ops).unwrap();

        assert_eq!(
            store.get(Column::StateTrie, b"new_key").unwrap(),
            Some(b"new_value".to_vec())
        );
        assert_eq!(store.get(Column::StateTrie, b"to_delete").unwrap(), None);
    }

    #[test]
    fn contains_key() {
        let store = InMemoryStore::new();
        store.put(Column::ChainIndex, b"exists", b"yes").unwrap();
        assert!(store.contains(Column::ChainIndex, b"exists").unwrap());
        assert!(!store.contains(Column::ChainIndex, b"nope").unwrap());
    }

    #[test]
    fn overwrite_value() {
        let store = InMemoryStore::new();
        store.put(Column::ContractCode, b"code", b"v1").unwrap();
        store.put(Column::ContractCode, b"code", b"v2").unwrap();
        assert_eq!(
            store.get(Column::ContractCode, b"code").unwrap(),
            Some(b"v2".to_vec())
        );
    }
}
