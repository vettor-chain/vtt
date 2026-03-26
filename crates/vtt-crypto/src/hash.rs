use vtt_primitives::H256;

/// Compute BLAKE3 hash of arbitrary data.
pub fn blake3_hash(data: &[u8]) -> H256 {
    let hash = blake3::hash(data);
    H256(*hash.as_bytes())
}

/// Compute the Merkle root of a list of hashes.
/// Uses BLAKE3 for internal nodes: H(left || right).
/// Returns H256::ZERO for an empty list.
pub fn merkle_root(hashes: &[H256]) -> H256 {
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
                // Odd element: promote to next level
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
    fn blake3_hash_deterministic() {
        let h1 = blake3_hash(b"hello VTT");
        let h2 = blake3_hash(b"hello VTT");
        assert_eq!(h1, h2);
    }

    #[test]
    fn blake3_hash_different_inputs() {
        let h1 = blake3_hash(b"hello");
        let h2 = blake3_hash(b"world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn blake3_hash_not_zero() {
        let h = blake3_hash(b"test");
        assert_ne!(h, H256::ZERO);
    }

    #[test]
    fn merkle_root_empty() {
        assert_eq!(merkle_root(&[]), H256::ZERO);
    }

    #[test]
    fn merkle_root_single() {
        let h = blake3_hash(b"only one");
        assert_eq!(merkle_root(&[h]), h);
    }

    #[test]
    fn merkle_root_two_elements() {
        let h1 = blake3_hash(b"first");
        let h2 = blake3_hash(b"second");
        let root = merkle_root(&[h1, h2]);

        // Manually compute expected
        let mut combined = [0u8; 64];
        combined[..32].copy_from_slice(h1.as_bytes());
        combined[32..].copy_from_slice(h2.as_bytes());
        let expected = blake3_hash(&combined);

        assert_eq!(root, expected);
    }

    #[test]
    fn merkle_root_three_elements() {
        let h1 = blake3_hash(b"a");
        let h2 = blake3_hash(b"b");
        let h3 = blake3_hash(b"c");
        let root = merkle_root(&[h1, h2, h3]);

        // h1h2 = hash(h1 || h2), h3 promoted, then hash(h1h2 || h3)
        let mut combined12 = [0u8; 64];
        combined12[..32].copy_from_slice(h1.as_bytes());
        combined12[32..].copy_from_slice(h2.as_bytes());
        let h12 = blake3_hash(&combined12);

        let mut combined_root = [0u8; 64];
        combined_root[..32].copy_from_slice(h12.as_bytes());
        combined_root[32..].copy_from_slice(h3.as_bytes());
        let expected = blake3_hash(&combined_root);

        assert_eq!(root, expected);
    }

    #[test]
    fn merkle_root_four_elements() {
        let hashes: Vec<H256> = (0..4).map(|i| blake3_hash(&[i])).collect();
        let root = merkle_root(&hashes);
        assert_ne!(root, H256::ZERO);

        // Changing one element changes the root
        let mut modified = hashes.clone();
        modified[2] = blake3_hash(b"different");
        let root2 = merkle_root(&modified);
        assert_ne!(root, root2);
    }

    #[test]
    fn merkle_root_deterministic() {
        let hashes: Vec<H256> = (0..10).map(|i| blake3_hash(&[i])).collect();
        let r1 = merkle_root(&hashes);
        let r2 = merkle_root(&hashes);
        assert_eq!(r1, r2);
    }
}
