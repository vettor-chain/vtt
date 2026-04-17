use std::path::Path;
use std::sync::Arc;

use rocksdb::{ColumnFamilyDescriptor, Options, DB};

use crate::{BatchOp, Column, KeyValueStore, Result, StorageError};

/// RocksDB-backed key-value store for production use.
pub struct RocksStore {
    db: Arc<DB>,
}

impl RocksStore {
    /// Prune old block data, keeping the most recent `keep_recent` blocks.
    /// Removes BlockBodies and Receipts for blocks older than (current_height - keep_recent).
    /// Returns the number of entries pruned.
    pub fn prune_old_blocks(&self, keep_recent: u64, current_height: u64) -> Result<u64> {
        if current_height <= keep_recent {
            return Ok(0);
        }

        let prune_below = current_height - keep_recent;
        let mut pruned = 0u64;

        for column in [Column::BlockBodies, Column::Receipts] {
            for height in 0..prune_below {
                let key = height.to_be_bytes();
                if self.contains(column, &key)? {
                    self.delete(column, &key)?;
                    pruned += 1;
                }
            }
        }

        Ok(pruned)
    }

    /// Trigger manual compaction across all column families.
    pub fn compact(&self) {
        for column in Column::ALL {
            if let Some(cf) = self.db.cf_handle(column.name()) {
                self.db.compact_range_cf(&cf, None::<&[u8]>, None::<&[u8]>);
            }
        }
    }

    /// Open or create a RocksDB database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);

        let cf_descriptors: Vec<ColumnFamilyDescriptor> = Column::ALL
            .iter()
            .map(|col| {
                let mut cf_opts = Options::default();
                cf_opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
                ColumnFamilyDescriptor::new(col.name(), cf_opts)
            })
            .collect();

        let db = DB::open_cf_descriptors(&db_opts, path, cf_descriptors)
            .map_err(|e| StorageError::Io(e.to_string()))?;

        Ok(Self { db: Arc::new(db) })
    }
}

impl KeyValueStore for RocksStore {
    fn get(&self, column: Column, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let cf = self
            .db
            .cf_handle(column.name())
            .ok_or_else(|| StorageError::ColumnNotFound(column.name().to_string()))?;
        self.db
            .get_cf(&cf, key)
            .map_err(|e| StorageError::Io(e.to_string()))
    }

    fn put(&self, column: Column, key: &[u8], value: &[u8]) -> Result<()> {
        let cf = self
            .db
            .cf_handle(column.name())
            .ok_or_else(|| StorageError::ColumnNotFound(column.name().to_string()))?;
        self.db
            .put_cf(&cf, key, value)
            .map_err(|e| StorageError::Io(e.to_string()))
    }

    fn delete(&self, column: Column, key: &[u8]) -> Result<()> {
        let cf = self
            .db
            .cf_handle(column.name())
            .ok_or_else(|| StorageError::ColumnNotFound(column.name().to_string()))?;
        self.db
            .delete_cf(&cf, key)
            .map_err(|e| StorageError::Io(e.to_string()))
    }

    fn write_batch(&self, ops: Vec<BatchOp>) -> Result<()> {
        let mut batch = rocksdb::WriteBatch::default();
        for op in ops {
            match op {
                BatchOp::Put { column, key, value } => {
                    let cf = self
                        .db
                        .cf_handle(column.name())
                        .ok_or_else(|| StorageError::ColumnNotFound(column.name().to_string()))?;
                    batch.put_cf(&cf, key, value);
                }
                BatchOp::Delete { column, key } => {
                    let cf = self
                        .db
                        .cf_handle(column.name())
                        .ok_or_else(|| StorageError::ColumnNotFound(column.name().to_string()))?;
                    batch.delete_cf(&cf, key);
                }
            }
        }
        self.db
            .write(batch)
            .map_err(|e| StorageError::Io(e.to_string()))
    }

    fn prefix_scan(&self, column: Column, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let cf = self
            .db
            .cf_handle(column.name())
            .ok_or_else(|| StorageError::ColumnNotFound(column.name().to_string()))?;
        let mut out = Vec::new();
        if prefix.is_empty() {
            let iter = self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start);
            for item in iter {
                let (k, v) = item.map_err(|e| StorageError::Io(e.to_string()))?;
                out.push((k.to_vec(), v.to_vec()));
            }
        } else {
            let iter = self.db.prefix_iterator_cf(&cf, prefix);
            for item in iter {
                let (k, v) = item.map_err(|e| StorageError::Io(e.to_string()))?;
                if !k.starts_with(prefix) {
                    break;
                }
                out.push((k.to_vec(), v.to_vec()));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_test_db() -> (RocksStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = RocksStore::open(dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn rocks_put_and_get() {
        let (store, _dir) = open_test_db();
        store
            .put(Column::BlockHeaders, b"block_0", b"genesis")
            .unwrap();
        let val = store.get(Column::BlockHeaders, b"block_0").unwrap();
        assert_eq!(val, Some(b"genesis".to_vec()));
    }

    #[test]
    fn rocks_get_nonexistent() {
        let (store, _dir) = open_test_db();
        let val = store.get(Column::Transactions, b"missing").unwrap();
        assert_eq!(val, None);
    }

    #[test]
    fn rocks_delete() {
        let (store, _dir) = open_test_db();
        store.put(Column::Receipts, b"r1", b"data").unwrap();
        store.delete(Column::Receipts, b"r1").unwrap();
        assert_eq!(store.get(Column::Receipts, b"r1").unwrap(), None);
    }

    #[test]
    fn rocks_columns_independent() {
        let (store, _dir) = open_test_db();
        store.put(Column::BlockHeaders, b"key", b"header").unwrap();
        store.put(Column::Transactions, b"key", b"tx").unwrap();

        assert_eq!(
            store.get(Column::BlockHeaders, b"key").unwrap(),
            Some(b"header".to_vec())
        );
        assert_eq!(
            store.get(Column::Transactions, b"key").unwrap(),
            Some(b"tx".to_vec())
        );
    }

    #[test]
    fn rocks_write_batch() {
        let (store, _dir) = open_test_db();
        store.put(Column::StateTrie, b"old", b"value").unwrap();

        let ops = vec![
            BatchOp::Put {
                column: Column::StateTrie,
                key: b"new".to_vec(),
                value: b"fresh".to_vec(),
            },
            BatchOp::Delete {
                column: Column::StateTrie,
                key: b"old".to_vec(),
            },
        ];

        store.write_batch(ops).unwrap();

        assert_eq!(
            store.get(Column::StateTrie, b"new").unwrap(),
            Some(b"fresh".to_vec())
        );
        assert_eq!(store.get(Column::StateTrie, b"old").unwrap(), None);
    }

    #[test]
    fn rocks_contains() {
        let (store, _dir) = open_test_db();
        store
            .put(Column::ContractCode, b"wasm", b"0x0061736d")
            .unwrap();
        assert!(store.contains(Column::ContractCode, b"wasm").unwrap());
        assert!(!store.contains(Column::ContractCode, b"nope").unwrap());
    }

    #[test]
    fn rocks_overwrite() {
        let (store, _dir) = open_test_db();
        store.put(Column::ChainIndex, b"height", b"100").unwrap();
        store.put(Column::ChainIndex, b"height", b"101").unwrap();
        assert_eq!(
            store.get(Column::ChainIndex, b"height").unwrap(),
            Some(b"101".to_vec())
        );
    }

    #[test]
    fn rocks_prune_old_blocks() {
        let (store, _dir) = open_test_db();

        // Insert block bodies and receipts for blocks 0..10
        for height in 0u64..10 {
            let key = height.to_be_bytes();
            store.put(Column::BlockBodies, &key, b"body").unwrap();
            store.put(Column::Receipts, &key, b"receipt").unwrap();
        }

        // Prune keeping last 5 blocks (current_height=10, keep_recent=5 -> prune below 5)
        let pruned = store.prune_old_blocks(5, 10).unwrap();
        // 5 bodies + 5 receipts = 10 entries pruned
        assert_eq!(pruned, 10);

        // Blocks 0..5 should be gone
        for height in 0u64..5 {
            let key = height.to_be_bytes();
            assert!(!store.contains(Column::BlockBodies, &key).unwrap());
            assert!(!store.contains(Column::Receipts, &key).unwrap());
        }

        // Blocks 5..10 should still exist
        for height in 5u64..10 {
            let key = height.to_be_bytes();
            assert!(store.contains(Column::BlockBodies, &key).unwrap());
            assert!(store.contains(Column::Receipts, &key).unwrap());
        }
    }

    #[test]
    fn rocks_prune_nothing_when_below_threshold() {
        let (store, _dir) = open_test_db();
        // current_height <= keep_recent, nothing to prune
        let pruned = store.prune_old_blocks(100, 50).unwrap();
        assert_eq!(pruned, 0);
    }

    #[test]
    fn rocks_compact_does_not_panic() {
        let (store, _dir) = open_test_db();
        store.put(Column::BlockHeaders, b"key", b"value").unwrap();
        // Just verify compact does not panic
        store.compact();
    }
}
