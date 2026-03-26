use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use thiserror::Error;

use vtt_primitives::{Address, PublicKey, Signature};

use crate::hash::blake3_hash;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("invalid signature")]
    InvalidSignature,
    #[error("invalid public key")]
    InvalidPublicKey,
}

/// An Ed25519 keypair for signing transactions and blocks.
pub struct Keypair {
    signing_key: SigningKey,
}

impl Keypair {
    /// Generate a new random keypair.
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self { signing_key }
    }

    /// Create a keypair from a 32-byte secret seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let signing_key = SigningKey::from_bytes(seed);
        Self { signing_key }
    }

    /// Get the public key.
    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.signing_key.verifying_key().to_bytes())
    }

    /// Get the address derived from the public key.
    pub fn address(&self) -> Address {
        address_from_public_key(&self.public_key())
    }

    /// Sign a message.
    pub fn sign(&self, message: &[u8]) -> Signature {
        let sig = self.signing_key.sign(message);
        Signature(sig.to_bytes())
    }
}

/// Derive an address from a public key.
/// Address = last 20 bytes of BLAKE3(public_key_bytes).
pub fn address_from_public_key(pubkey: &PublicKey) -> Address {
    let hash = blake3_hash(pubkey.as_bytes());
    let mut addr_bytes = [0u8; 20];
    addr_bytes.copy_from_slice(&hash.as_bytes()[12..32]);
    Address(addr_bytes)
}

/// Sign a message with a raw signing key.
pub fn sign(message: &[u8], keypair: &Keypair) -> Signature {
    keypair.sign(message)
}

/// Verify an Ed25519 signature.
pub fn verify(
    message: &[u8],
    signature: &Signature,
    pubkey: &PublicKey,
) -> Result<(), CryptoError> {
    let verifying_key =
        VerifyingKey::from_bytes(pubkey.as_bytes()).map_err(|_| CryptoError::InvalidPublicKey)?;
    let sig = ed25519_dalek::Signature::from_bytes(signature.as_bytes());
    verifying_key
        .verify(message, &sig)
        .map_err(|_| CryptoError::InvalidSignature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_generate_and_sign_verify() {
        let kp = Keypair::generate();
        let msg = b"hello VTT blockchain";

        let sig = kp.sign(msg);
        let pk = kp.public_key();

        assert!(verify(msg, &sig, &pk).is_ok());
    }

    #[test]
    fn keypair_from_seed_deterministic() {
        let seed = [42u8; 32];
        let kp1 = Keypair::from_seed(&seed);
        let kp2 = Keypair::from_seed(&seed);

        assert_eq!(kp1.public_key(), kp2.public_key());
        assert_eq!(kp1.address(), kp2.address());

        let msg = b"deterministic test";
        let sig1 = kp1.sign(msg);
        let sig2 = kp2.sign(msg);
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn verify_wrong_message_fails() {
        let kp = Keypair::generate();
        let sig = kp.sign(b"original message");
        let pk = kp.public_key();

        assert!(verify(b"wrong message", &sig, &pk).is_err());
    }

    #[test]
    fn verify_wrong_key_fails() {
        let kp1 = Keypair::generate();
        let kp2 = Keypair::generate();
        let msg = b"test message";

        let sig = kp1.sign(msg);
        assert!(verify(msg, &sig, &kp2.public_key()).is_err());
    }

    #[test]
    fn address_derivation_deterministic() {
        let kp = Keypair::from_seed(&[1u8; 32]);
        let addr1 = kp.address();
        let addr2 = address_from_public_key(&kp.public_key());
        assert_eq!(addr1, addr2);
    }

    #[test]
    fn different_keys_different_addresses() {
        let kp1 = Keypair::from_seed(&[1u8; 32]);
        let kp2 = Keypair::from_seed(&[2u8; 32]);
        assert_ne!(kp1.address(), kp2.address());
    }

    #[test]
    fn address_is_20_bytes() {
        let kp = Keypair::generate();
        let addr = kp.address();
        assert_eq!(addr.as_bytes().len(), 20);
    }

    #[test]
    fn sign_and_verify_block_header_bytes() {
        let kp = Keypair::generate();

        // Simulate signing block header signable bytes
        let header_bytes = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let sig = kp.sign(&header_bytes);

        assert!(verify(&header_bytes, &sig, &kp.public_key()).is_ok());
    }
}
