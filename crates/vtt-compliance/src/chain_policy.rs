use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use vtt_primitives::amount::Amount;
use vtt_primitives::H256;

use crate::identity::{ClaimType, OnChainIdentity};

/// Compliance configuration for a chain in the VTT multichain network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct ChainComplianceConfig {
    /// Whether this chain requires identity verification for all participants.
    pub requires_identity: bool,
    /// Minimum claim requirements for any transaction on this chain.
    pub minimum_claims: Vec<ClaimType>,
    /// Trusted claim issuers for this chain.
    pub trusted_claim_issuers: Vec<H256>,
    /// Whether anonymous (identity-less) accounts can transact.
    pub allow_anonymous: bool,
    /// Jurisdiction whitelist (ISO 3166-1 codes). Empty = all allowed.
    pub jurisdiction_whitelist: Vec<String>,
    /// Jurisdiction blacklist.
    pub jurisdiction_blacklist: Vec<String>,
    /// Maximum number of unique holders per asset (0 = unlimited).
    pub max_holders_per_asset: u32,
}

impl ChainComplianceConfig {
    /// Permissionless config — no compliance requirements.
    pub fn permissionless() -> Self {
        Self {
            requires_identity: false,
            minimum_claims: Vec::new(),
            trusted_claim_issuers: Vec::new(),
            allow_anonymous: true,
            jurisdiction_whitelist: Vec::new(),
            jurisdiction_blacklist: Vec::new(),
            max_holders_per_asset: 0,
        }
    }

    /// Permissioned config — requires KYC and AML.
    pub fn permissioned(trusted_issuers: Vec<H256>) -> Self {
        Self {
            requires_identity: true,
            minimum_claims: vec![ClaimType::KYC, ClaimType::AMLCleared],
            trusted_claim_issuers: trusted_issuers,
            allow_anonymous: false,
            jurisdiction_whitelist: Vec::new(),
            jurisdiction_blacklist: Vec::new(),
            max_holders_per_asset: 0,
        }
    }
}

/// Result of a compliance check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComplianceResult {
    Allowed,
    Denied { reason: ComplianceDenialReason },
}

impl ComplianceResult {
    pub fn is_allowed(&self) -> bool {
        matches!(self, ComplianceResult::Allowed)
    }
}

/// Reasons why a compliance check may fail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComplianceDenialReason {
    IdentityRequired,
    MissingClaim(ClaimType),
    ExpiredClaim(ClaimType),
    UntrustedIssuer,
    JurisdictionRestricted(String),
    MaxHoldersReached,
}

/// Check if an identity is compliant with a chain's compliance policy.
pub fn check_identity_compliance(
    identity: &OnChainIdentity,
    config: &ChainComplianceConfig,
    current_time: u64,
) -> ComplianceResult {
    // Check all minimum claims
    for required_claim in &config.minimum_claims {
        if !identity.has_valid_claim(required_claim, current_time) {
            return ComplianceResult::Denied {
                reason: ComplianceDenialReason::MissingClaim(required_claim.clone()),
            };
        }
    }

    // Check trusted issuers (if configured)
    if !config.trusted_claim_issuers.is_empty() {
        let has_trusted_claim = identity.claims.iter().any(|c| {
            !c.revoked
                && (c.expires_at == 0 || c.expires_at > current_time)
                && config.trusted_claim_issuers.contains(&c.issuer)
        });
        if !has_trusted_claim {
            return ComplianceResult::Denied {
                reason: ComplianceDenialReason::UntrustedIssuer,
            };
        }
    }

    // Check jurisdiction whitelist
    if !config.jurisdiction_whitelist.is_empty() {
        if let Some(jurisdiction) = identity.jurisdiction(current_time) {
            if !config.jurisdiction_whitelist.contains(&jurisdiction) {
                return ComplianceResult::Denied {
                    reason: ComplianceDenialReason::JurisdictionRestricted(jurisdiction),
                };
            }
        } else {
            return ComplianceResult::Denied {
                reason: ComplianceDenialReason::MissingClaim(ClaimType::Jurisdiction),
            };
        }
    }

    // Check jurisdiction blacklist
    if !config.jurisdiction_blacklist.is_empty() {
        if let Some(jurisdiction) = identity.jurisdiction(current_time) {
            if config.jurisdiction_blacklist.contains(&jurisdiction) {
                return ComplianceResult::Denied {
                    reason: ComplianceDenialReason::JurisdictionRestricted(jurisdiction),
                };
            }
        }
    }

    ComplianceResult::Allowed
}

/// Check if a transfer is allowed between two identities.
pub fn check_transfer_compliance(
    from: &OnChainIdentity,
    to: &OnChainIdentity,
    _asset_id: &H256,
    _amount: &Amount,
    config: &ChainComplianceConfig,
    current_time: u64,
) -> ComplianceResult {
    // Both parties must be compliant
    let from_check = check_identity_compliance(from, config, current_time);
    if !from_check.is_allowed() {
        return from_check;
    }

    let to_check = check_identity_compliance(to, config, current_time);
    if !to_check.is_allowed() {
        return to_check;
    }

    ComplianceResult::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Claim, IdentityType};
    use vtt_primitives::Address;

    fn make_identity(jurisdiction: &str, has_kyc: bool, issuer: H256) -> OnChainIdentity {
        let mut claims = vec![Claim {
            claim_type: ClaimType::Jurisdiction,
            value: jurisdiction.as_bytes().to_vec(),
            issuer,
            issued_at: 0,
            expires_at: 0,
            revoked: false,
        }];

        if has_kyc {
            claims.push(Claim {
                claim_type: ClaimType::KYC,
                value: b"passed".to_vec(),
                issuer,
                issued_at: 0,
                expires_at: 0,
                revoked: false,
            });
            claims.push(Claim {
                claim_type: ClaimType::AMLCleared,
                value: b"yes".to_vec(),
                issuer,
                issued_at: 0,
                expires_at: 0,
                revoked: false,
            });
        }

        OnChainIdentity {
            id: H256::from([0x01; 32]),
            did_uri: "did:vtt:test".to_string(),
            claims,
            controlled_addresses: vec![Address::from([0x01; 20])],
            identity_type: IdentityType::Individual,
            created_at: 0,
        }
    }

    #[test]
    fn permissionless_allows_anyone() {
        let config = ChainComplianceConfig::permissionless();
        let id = make_identity("IT", false, H256::ZERO);
        assert!(check_identity_compliance(&id, &config, 1000).is_allowed());
    }

    #[test]
    fn permissioned_requires_kyc() {
        let issuer = H256::from([0xAA; 32]);
        let config = ChainComplianceConfig::permissioned(vec![issuer]);

        let id_with_kyc = make_identity("IT", true, issuer);
        assert!(check_identity_compliance(&id_with_kyc, &config, 1000).is_allowed());

        let id_without_kyc = make_identity("IT", false, issuer);
        let result = check_identity_compliance(&id_without_kyc, &config, 1000);
        assert!(!result.is_allowed());
        assert!(matches!(
            result,
            ComplianceResult::Denied {
                reason: ComplianceDenialReason::MissingClaim(ClaimType::KYC)
            }
        ));
    }

    #[test]
    fn untrusted_issuer_denied() {
        let trusted = H256::from([0xAA; 32]);
        let untrusted = H256::from([0xBB; 32]);
        let config = ChainComplianceConfig::permissioned(vec![trusted]);

        let id = make_identity("IT", true, untrusted); // claims from untrusted issuer
        let result = check_identity_compliance(&id, &config, 1000);
        assert!(!result.is_allowed());
    }

    #[test]
    fn jurisdiction_whitelist() {
        let config = ChainComplianceConfig {
            jurisdiction_whitelist: vec!["IT".to_string(), "DE".to_string(), "FR".to_string()],
            ..ChainComplianceConfig::permissionless()
        };

        let it_id = make_identity("IT", false, H256::ZERO);
        assert!(check_identity_compliance(&it_id, &config, 1000).is_allowed());

        let us_id = make_identity("US", false, H256::ZERO);
        let result = check_identity_compliance(&us_id, &config, 1000);
        assert!(!result.is_allowed());
        assert!(matches!(
            result,
            ComplianceResult::Denied {
                reason: ComplianceDenialReason::JurisdictionRestricted(_)
            }
        ));
    }

    #[test]
    fn jurisdiction_blacklist() {
        let config = ChainComplianceConfig {
            jurisdiction_blacklist: vec!["KP".to_string(), "IR".to_string()],
            ..ChainComplianceConfig::permissionless()
        };

        let it_id = make_identity("IT", false, H256::ZERO);
        assert!(check_identity_compliance(&it_id, &config, 1000).is_allowed());

        let kp_id = make_identity("KP", false, H256::ZERO);
        assert!(!check_identity_compliance(&kp_id, &config, 1000).is_allowed());
    }

    #[test]
    fn transfer_compliance_both_must_pass() {
        let issuer = H256::from([0xAA; 32]);
        let config = ChainComplianceConfig::permissioned(vec![issuer]);

        let from = make_identity("IT", true, issuer);
        let to = make_identity("DE", true, issuer);
        let asset = H256::from([0x01; 32]);
        let amount = Amount::from_vtt(100);

        assert!(check_transfer_compliance(&from, &to, &asset, &amount, &config, 1000).is_allowed());

        // If recipient has no KYC, transfer denied
        let to_no_kyc = make_identity("DE", false, issuer);
        assert!(
            !check_transfer_compliance(&from, &to_no_kyc, &asset, &amount, &config, 1000)
                .is_allowed()
        );
    }

    #[test]
    fn config_borsh_roundtrip() {
        let config = ChainComplianceConfig::permissioned(vec![H256::from([0xAA; 32])]);
        let bytes = borsh::to_vec(&config).unwrap();
        let config2 = ChainComplianceConfig::try_from_slice(&bytes).unwrap();
        assert_eq!(config, config2);
    }
}
