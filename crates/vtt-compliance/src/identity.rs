use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use vtt_primitives::{Address, Timestamp, H256};

/// An on-chain identity for a participant in the VTT network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct OnChainIdentity {
    /// Unique identity ID.
    pub id: H256,
    /// DID document URI (off-chain, W3C DID format).
    pub did_uri: String,
    /// Claims/attestations about this identity.
    pub claims: Vec<Claim>,
    /// Addresses controlled by this identity.
    pub controlled_addresses: Vec<Address>,
    /// Identity type.
    pub identity_type: IdentityType,
    /// Block number when created.
    pub created_at: u64,
}

/// Type of identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum IdentityType {
    Individual,
    Institution,
    ClaimIssuer,
    OracleNode,
    Validator,
}

/// A claim/attestation about an identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Claim {
    pub claim_type: ClaimType,
    /// Claim value (interpretation depends on claim_type).
    pub value: Vec<u8>,
    /// Who issued this claim.
    pub issuer: H256,
    /// When the claim was issued (block number).
    pub issued_at: u64,
    /// When the claim expires (0 = never).
    pub expires_at: Timestamp,
    /// Whether the claim has been revoked.
    pub revoked: bool,
}

/// Types of claims that can be made about an identity.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub enum ClaimType {
    /// Know Your Customer verification passed.
    KYC,
    /// Anti-Money Laundering clearance.
    AMLCleared,
    /// Accredited investor status.
    AccreditedInvestor,
    /// Qualified purchaser status.
    QualifiedPurchaser,
    /// Jurisdiction (value = ISO 3166-1 country code).
    Jurisdiction,
    /// Custom claim type.
    Custom(u32),
}

impl OnChainIdentity {
    /// Check if this identity has a valid (non-expired, non-revoked) claim of the given type.
    pub fn has_valid_claim(&self, claim_type: &ClaimType, current_time: Timestamp) -> bool {
        self.claims.iter().any(|c| {
            c.claim_type == *claim_type
                && !c.revoked
                && (c.expires_at == 0 || c.expires_at > current_time)
        })
    }

    /// Get all valid claims of a specific type.
    pub fn get_valid_claims(&self, claim_type: &ClaimType, current_time: Timestamp) -> Vec<&Claim> {
        self.claims
            .iter()
            .filter(|c| {
                c.claim_type == *claim_type
                    && !c.revoked
                    && (c.expires_at == 0 || c.expires_at > current_time)
            })
            .collect()
    }

    /// Get the jurisdiction claim value (if any).
    pub fn jurisdiction(&self, current_time: Timestamp) -> Option<String> {
        self.get_valid_claims(&ClaimType::Jurisdiction, current_time)
            .first()
            .map(|c| String::from_utf8_lossy(&c.value).to_string())
    }

    /// Add a new claim.
    pub fn add_claim(&mut self, claim: Claim) {
        self.claims.push(claim);
    }

    /// Revoke a claim by index.
    pub fn revoke_claim(&mut self, index: usize) -> bool {
        if let Some(claim) = self.claims.get_mut(index) {
            claim.revoked = true;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_identity() -> OnChainIdentity {
        OnChainIdentity {
            id: H256::from([0x01; 32]),
            did_uri: "did:vtt:0x01".to_string(),
            claims: vec![
                Claim {
                    claim_type: ClaimType::KYC,
                    value: b"passed".to_vec(),
                    issuer: H256::from([0xAA; 32]),
                    issued_at: 100,
                    expires_at: 2_000_000_000_000, // far future
                    revoked: false,
                },
                Claim {
                    claim_type: ClaimType::Jurisdiction,
                    value: b"IT".to_vec(),
                    issuer: H256::from([0xAA; 32]),
                    issued_at: 100,
                    expires_at: 0, // never expires
                    revoked: false,
                },
                Claim {
                    claim_type: ClaimType::AccreditedInvestor,
                    value: b"yes".to_vec(),
                    issuer: H256::from([0xAA; 32]),
                    issued_at: 100,
                    expires_at: 500, // expired
                    revoked: false,
                },
            ],
            controlled_addresses: vec![Address::from([0x01; 20])],
            identity_type: IdentityType::Individual,
            created_at: 0,
        }
    }

    #[test]
    fn has_valid_kyc_claim() {
        let id = test_identity();
        assert!(id.has_valid_claim(&ClaimType::KYC, 1_000_000_000_000));
    }

    #[test]
    fn expired_claim_not_valid() {
        let id = test_identity();
        // AccreditedInvestor expired at 500, current time is 1000
        assert!(!id.has_valid_claim(&ClaimType::AccreditedInvestor, 1000));
    }

    #[test]
    fn never_expires_claim() {
        let id = test_identity();
        // Jurisdiction has expires_at = 0 (never)
        assert!(id.has_valid_claim(&ClaimType::Jurisdiction, 999_999_999_999));
    }

    #[test]
    fn jurisdiction_value() {
        let id = test_identity();
        assert_eq!(id.jurisdiction(1000), Some("IT".to_string()));
    }

    #[test]
    fn revoke_claim() {
        let mut id = test_identity();
        assert!(id.has_valid_claim(&ClaimType::KYC, 1000));

        id.revoke_claim(0); // revoke KYC claim
        assert!(!id.has_valid_claim(&ClaimType::KYC, 1000));
    }

    #[test]
    fn add_claim() {
        let mut id = test_identity();
        assert!(!id.has_valid_claim(&ClaimType::AMLCleared, 1000));

        id.add_claim(Claim {
            claim_type: ClaimType::AMLCleared,
            value: b"cleared".to_vec(),
            issuer: H256::from([0xBB; 32]),
            issued_at: 200,
            expires_at: 0,
            revoked: false,
        });
        assert!(id.has_valid_claim(&ClaimType::AMLCleared, 1000));
    }

    #[test]
    fn identity_borsh_roundtrip() {
        let id = test_identity();
        let bytes = borsh::to_vec(&id).unwrap();
        let id2 = OnChainIdentity::try_from_slice(&bytes).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn no_matching_claim() {
        let id = test_identity();
        assert!(!id.has_valid_claim(&ClaimType::QualifiedPurchaser, 1000));
    }
}
