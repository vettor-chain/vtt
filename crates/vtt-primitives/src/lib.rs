pub mod amount;
pub mod block;
pub mod chain;
pub mod transaction;

use std::fmt;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// 32-byte BLAKE3 hash used for block hashes, tx hashes, state roots, etc.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, Default, BorshSerialize, BorshDeserialize, PartialOrd, Ord,
)]
pub struct H256(pub [u8; 32]);

impl Serialize for H256 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("0x{}", hex::encode(self.0)))
    }
}

impl<'de> Deserialize<'de> for H256 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        if bytes.len() != 32 {
            return Err(serde::de::Error::custom("H256 must be 32 bytes"));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }
}

impl H256 {
    pub const ZERO: Self = Self([0u8; 32]);

    pub fn from_slice(slice: &[u8]) -> Self {
        let mut bytes = [0u8; 32];
        let len = slice.len().min(32);
        bytes[..len].copy_from_slice(&slice[..len]);
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for H256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "H256(0x{})", hex::encode(self.0))
    }
}

impl fmt::Display for H256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

impl From<[u8; 32]> for H256 {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// 20-byte address derived from the last 20 bytes of BLAKE3(public_key).
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, Default, BorshSerialize, BorshDeserialize, PartialOrd, Ord,
)]
pub struct Address(pub [u8; 20]);

impl Serialize for Address {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("0x{}", hex::encode(self.0)))
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        if bytes.len() != 20 {
            return Err(serde::de::Error::custom("Address must be 20 bytes"));
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }
}

impl Address {
    pub const ZERO: Self = Self([0u8; 20]);

    pub fn from_slice(slice: &[u8]) -> Self {
        let mut bytes = [0u8; 20];
        let len = slice.len().min(20);
        bytes[..len].copy_from_slice(&slice[..len]);
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address(0x{})", hex::encode(self.0))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

impl From<[u8; 20]> for Address {
    fn from(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }
}

/// Ed25519 public key (32 bytes).
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub struct PublicKey(pub [u8; 32]);

impl PublicKey {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey(0x{}...)", hex::encode(&self.0[..4]))
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", hex::encode(self.0))
    }
}

impl From<[u8; 32]> for PublicKey {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// Ed25519 signature (64 bytes).
#[derive(Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Signature(pub [u8; 64]);

impl Serialize for Signature {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
        if bytes.len() != 64 {
            return Err(serde::de::Error::custom("signature must be 64 bytes"));
        }
        let mut arr = [0u8; 64];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }
}

impl Signature {
    pub const ZERO: Self = Self([0u8; 64]);

    pub fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }
}

impl Default for Signature {
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature(0x{}...)", hex::encode(&self.0[..4]))
    }
}

impl From<[u8; 64]> for Signature {
    fn from(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }
}

/// Identifies a chain within the VTT multichain network.
/// Chain 0 is the relay chain; Chain 1+ are application chains.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    PartialOrd,
    Ord,
)]
pub struct ChainId(pub u32);

impl ChainId {
    /// The relay chain always has ID 0.
    pub const RELAY: Self = Self(0);

    pub fn new(id: u32) -> Self {
        Self(id)
    }

    pub fn is_relay(&self) -> bool {
        self.0 == 0
    }
}

impl fmt::Debug for ChainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChainId({})", self.0)
    }
}

impl fmt::Display for ChainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Block height.
pub type BlockNumber = u64;

/// DPoS epoch number.
pub type Epoch = u64;

/// Unix timestamp in milliseconds.
pub type Timestamp = u64;

/// Governance vote choice.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub enum Vote {
    Yes,
    No,
    Abstain,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h256_zero_and_display() {
        let h = H256::ZERO;
        assert_eq!(h.0, [0u8; 32]);
        let s = format!("{h}");
        assert!(s.starts_with("0x"));
        assert_eq!(s.len(), 66); // "0x" + 64 hex chars
    }

    #[test]
    fn h256_from_slice() {
        let data = [0xAB; 32];
        let h = H256::from(data);
        assert_eq!(h.0, data);
    }

    #[test]
    fn h256_serde_roundtrip() {
        let h = H256::from([42u8; 32]);
        let json = serde_json::to_string(&h).unwrap();
        let h2: H256 = serde_json::from_str(&json).unwrap();
        assert_eq!(h, h2);
    }

    #[test]
    fn h256_borsh_roundtrip() {
        let h = H256::from([42u8; 32]);
        let bytes = borsh::to_vec(&h).unwrap();
        let h2 = H256::try_from_slice(&bytes).unwrap();
        assert_eq!(h, h2);
    }

    #[test]
    fn address_zero_and_display() {
        let a = Address::ZERO;
        assert_eq!(a.0, [0u8; 20]);
        let s = format!("{a}");
        assert_eq!(s.len(), 42); // "0x" + 40 hex chars
    }

    #[test]
    fn address_serde_roundtrip() {
        let a = Address::from([0xFF; 20]);
        let json = serde_json::to_string(&a).unwrap();
        let a2: Address = serde_json::from_str(&json).unwrap();
        assert_eq!(a, a2);
    }

    #[test]
    fn chain_id_relay() {
        let relay = ChainId::RELAY;
        assert!(relay.is_relay());
        assert_eq!(relay.0, 0);

        let app = ChainId::new(1);
        assert!(!app.is_relay());
    }

    #[test]
    fn public_key_debug_shows_prefix() {
        let pk = PublicKey::from([0xAB; 32]);
        let dbg = format!("{pk:?}");
        assert!(dbg.contains("0xabababab..."));
    }

    #[test]
    fn signature_default_is_zero() {
        let sig = Signature::default();
        assert_eq!(sig.0, [0u8; 64]);
    }

    #[test]
    fn vote_serialization() {
        let v = Vote::Yes;
        let bytes = borsh::to_vec(&v).unwrap();
        let v2 = Vote::try_from_slice(&bytes).unwrap();
        assert_eq!(v, v2);
    }
}
