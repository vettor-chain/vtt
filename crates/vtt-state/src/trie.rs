use std::collections::BTreeMap;

use vtt_crypto::blake3_hash;
use vtt_primitives::H256;

/// A simple Merkle trie for computing deterministic state roots.
///
/// This implementation uses a sorted BTreeMap of key-value pairs and computes
/// the state root as a Merkle tree over the sorted entries. Each leaf is
/// H(key || value), and internal nodes are H(left || right).
///
/// This is simpler than a full Merkle Patricia Trie but provides the same
/// guarantees: deterministic root for any set of key-value pairs, and the
/// ability to detect state changes via root comparison.
///
/// A full MPT with proof support will replace this in a later phase.
pub struct StateTrie {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
    dirty: bool,
    cached_root: Option<H256>,
}

impl StateTrie {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            dirty: true,
            cached_root: None,
        }
    }

    /// Create a trie from existing entries.
    pub fn from_entries(entries: BTreeMap<Vec<u8>, Vec<u8>>) -> Self {
        Self {
            entries,
            dirty: true,
            cached_root: None,
        }
    }

    /// Insert or update a key-value pair.
    pub fn insert(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.entries.insert(key, value);
        self.dirty = true;
        self.cached_root = None;
    }

    /// Remove a key.
    pub fn remove(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let result = self.entries.remove(key);
        if result.is_some() {
            self.dirty = true;
            self.cached_root = None;
        }
        result
    }

    /// Get a value by key.
    pub fn get(&self, key: &[u8]) -> Option<&Vec<u8>> {
        self.entries.get(key)
    }

    /// Check if the trie contains a key.
    pub fn contains(&self, key: &[u8]) -> bool {
        self.entries.contains_key(key)
    }

    /// Number of entries in the trie.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the trie is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Compute the state root hash.
    /// Returns H256::ZERO for an empty trie.
    pub fn root(&mut self) -> H256 {
        if let Some(cached) = self.cached_root {
            if !self.dirty {
                return cached;
            }
        }

        let root = self.compute_root();
        self.cached_root = Some(root);
        self.dirty = false;
        root
    }

    fn compute_root(&self) -> H256 {
        if self.entries.is_empty() {
            return H256::ZERO;
        }

        // Hash each leaf: H(key || value)
        let leaf_hashes: Vec<H256> = self
            .entries
            .iter()
            .map(|(k, v)| {
                let mut data = Vec::with_capacity(k.len() + v.len());
                data.extend_from_slice(k);
                data.extend_from_slice(v);
                blake3_hash(&data)
            })
            .collect();

        // Build Merkle tree from leaves
        merkle_root_from_hashes(&leaf_hashes)
    }

    /// Iterate over all entries.
    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u8>, &Vec<u8>)> {
        self.entries.iter()
    }
}

impl Default for StateTrie {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the Merkle root from a list of hashes (same algo as vtt_crypto::merkle_root
/// but inlined here to avoid circular dependencies on the exact function signature).
fn merkle_root_from_hashes(hashes: &[H256]) -> H256 {
    if hashes.is_empty() {
        return H256::ZERO;
    }
    if hashes.len() == 1 {
        return hashes[0];
    }

    let mut current_level: Vec<H256> = hashes.to_vec();

    while current_level.len() > 1 {
        let mut next_level = Vec::with_capacity(current_level.len().div_ceil(2));

        for chunk in current_level.chunks(2) {
            if chunk.len() == 2 {
                let mut combined = [0u8; 64];
                combined[..32].copy_from_slice(chunk[0].as_bytes());
                combined[32..].copy_from_slice(chunk[1].as_bytes());
                next_level.push(blake3_hash(&combined));
            } else {
                next_level.push(chunk[0]);
            }
        }

        current_level = next_level;
    }

    current_level[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_trie_root_is_zero() {
        let mut trie = StateTrie::new();
        assert_eq!(trie.root(), H256::ZERO);
    }

    #[test]
    fn single_entry_deterministic() {
        let mut trie = StateTrie::new();
        trie.insert(b"key1".to_vec(), b"value1".to_vec());
        let r1 = trie.root();

        let mut trie2 = StateTrie::new();
        trie2.insert(b"key1".to_vec(), b"value1".to_vec());
        let r2 = trie2.root();

        assert_eq!(r1, r2);
        assert_ne!(r1, H256::ZERO);
    }

    #[test]
    fn insertion_order_does_not_matter() {
        let mut trie1 = StateTrie::new();
        trie1.insert(b"a".to_vec(), b"1".to_vec());
        trie1.insert(b"b".to_vec(), b"2".to_vec());
        trie1.insert(b"c".to_vec(), b"3".to_vec());

        let mut trie2 = StateTrie::new();
        trie2.insert(b"c".to_vec(), b"3".to_vec());
        trie2.insert(b"a".to_vec(), b"1".to_vec());
        trie2.insert(b"b".to_vec(), b"2".to_vec());

        assert_eq!(trie1.root(), trie2.root());
    }

    #[test]
    fn different_values_different_roots() {
        let mut trie1 = StateTrie::new();
        trie1.insert(b"key".to_vec(), b"value1".to_vec());

        let mut trie2 = StateTrie::new();
        trie2.insert(b"key".to_vec(), b"value2".to_vec());

        assert_ne!(trie1.root(), trie2.root());
    }

    #[test]
    fn remove_entry_changes_root() {
        let mut trie = StateTrie::new();
        trie.insert(b"a".to_vec(), b"1".to_vec());
        trie.insert(b"b".to_vec(), b"2".to_vec());
        let root_with_both = trie.root();

        trie.remove(b"b");
        let root_with_one = trie.root();

        assert_ne!(root_with_both, root_with_one);
    }

    #[test]
    fn remove_all_returns_to_zero() {
        let mut trie = StateTrie::new();
        trie.insert(b"key".to_vec(), b"val".to_vec());
        trie.remove(b"key");
        assert_eq!(trie.root(), H256::ZERO);
    }

    #[test]
    fn root_caching_works() {
        let mut trie = StateTrie::new();
        trie.insert(b"k".to_vec(), b"v".to_vec());
        let r1 = trie.root();
        let r2 = trie.root(); // should use cache
        assert_eq!(r1, r2);
    }

    #[test]
    fn len_and_contains() {
        let mut trie = StateTrie::new();
        assert!(trie.is_empty());
        assert_eq!(trie.len(), 0);

        trie.insert(b"key".to_vec(), b"val".to_vec());
        assert!(!trie.is_empty());
        assert_eq!(trie.len(), 1);
        assert!(trie.contains(b"key"));
        assert!(!trie.contains(b"other"));
    }
}
