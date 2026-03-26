use std::collections::BTreeMap;

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, ChainId, H256};

/// Classification of a real-world asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum AssetClass {
    Equity,
    Debt,
    RealEstate,
    Commodity,
    Fund,
    IntellectualProperty,
    CarbonCredit,
    Invoice,
    Custom(String),
}

/// Lifecycle status of a tokenized asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum AssetStatus {
    /// Created but not yet verified/active.
    Draft,
    /// Trading enabled.
    Active,
    /// Regulatory freeze.
    Frozen,
    /// For debt instruments that have matured.
    Matured,
    /// Fully redeemed, no outstanding tokens.
    Redeemed,
}

/// Type of legal document attached to an asset.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
)]
pub enum DocumentType {
    Prospectus,
    TermSheet,
    LegalOpinion,
    Valuation,
    AuditReport,
    RegulatoryApproval,
    Custom(String),
}

/// A legal document record (hash only, content off-chain).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct DocumentRecord {
    pub doc_type: DocumentType,
    pub hash: H256,
    pub uri: String,
    pub added_at: u64,
    pub added_by: Address,
}

/// Core asset record in the global asset registry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct AssetRecord {
    /// Globally unique asset ID.
    pub id: H256,
    /// Human-readable name.
    pub name: String,
    /// Ticker symbol.
    pub symbol: String,
    /// Asset classification.
    pub class: AssetClass,
    /// The chain this asset was issued on.
    pub origin_chain: ChainId,
    /// Issuer address.
    pub issuer: Address,
    /// Total supply of tokens representing this asset.
    pub total_supply: Amount,
    /// Number of decimal places.
    pub decimals: u8,
    /// Current lifecycle status.
    pub status: AssetStatus,
    /// Compliance policy contract address (optional).
    pub compliance_policy: Option<Address>,
    /// Oracle feed for valuation (optional).
    pub valuation_oracle: Option<H256>,
    /// Legal document hashes.
    pub documents: BTreeMap<DocumentType, DocumentRecord>,
    /// Metadata URI (IPFS, etc.).
    pub metadata_uri: String,
    /// Block number at creation.
    pub created_at: u64,
}

impl AssetRecord {
    /// Get status as a string.
    pub fn status_str(&self) -> &'static str {
        match self.status {
            AssetStatus::Draft => "Draft",
            AssetStatus::Active => "Active",
            AssetStatus::Frozen => "Frozen",
            AssetStatus::Matured => "Matured",
            AssetStatus::Redeemed => "Redeemed",
        }
    }

    /// Check if the asset can be traded.
    pub fn is_tradeable(&self) -> bool {
        self.status == AssetStatus::Active
    }

    /// Check if the asset is frozen.
    pub fn is_frozen(&self) -> bool {
        self.status == AssetStatus::Frozen
    }

    /// Freeze the asset.
    pub fn freeze(&mut self) {
        self.status = AssetStatus::Frozen;
    }

    /// Activate the asset (from Draft).
    pub fn activate(&mut self) -> bool {
        if self.status == AssetStatus::Draft {
            self.status = AssetStatus::Active;
            true
        } else {
            false
        }
    }

    /// Attach a document.
    pub fn attach_document(&mut self, doc: DocumentRecord) {
        self.documents.insert(doc.doc_type.clone(), doc);
    }
}

/// Fractional ownership record for an account on a specific asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct OwnershipRecord {
    pub asset_id: H256,
    pub owner: Address,
    /// Available (unlocked) balance.
    pub available: Amount,
    /// Locked balance (lock-up, collateral, pending settlement).
    pub locked: Amount,
}

impl OwnershipRecord {
    pub fn new(asset_id: H256, owner: Address) -> Self {
        Self {
            asset_id,
            owner,
            available: Amount::ZERO,
            locked: Amount::ZERO,
        }
    }

    /// Total balance (available + locked).
    pub fn total(&self) -> Amount {
        self.available + self.locked
    }

    /// Credit available balance.
    pub fn credit(&mut self, amount: Amount) {
        self.available = self.available + amount;
    }

    /// Debit available balance. Returns false if insufficient.
    pub fn debit(&mut self, amount: Amount) -> bool {
        if self.available >= amount {
            self.available = self.available - amount;
            true
        } else {
            false
        }
    }

    /// Lock a portion of available balance.
    pub fn lock(&mut self, amount: Amount) -> bool {
        if self.available >= amount {
            self.available = self.available - amount;
            self.locked = self.locked + amount;
            true
        } else {
            false
        }
    }

    /// Unlock a portion of locked balance.
    pub fn unlock(&mut self, amount: Amount) -> bool {
        if self.locked >= amount {
            self.locked = self.locked - amount;
            self.available = self.available + amount;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_asset() -> AssetRecord {
        AssetRecord {
            id: H256::from([0x01; 32]),
            name: "Milan Real Estate Fund".to_string(),
            symbol: "MREF".to_string(),
            class: AssetClass::RealEstate,
            origin_chain: ChainId::new(1),
            issuer: Address::from([0x10; 20]),
            total_supply: Amount::from_vtt(1_000_000),
            decimals: 8,
            status: AssetStatus::Draft,
            compliance_policy: None,
            valuation_oracle: None,
            documents: BTreeMap::new(),
            metadata_uri: "ipfs://QmTest".to_string(),
            created_at: 100,
        }
    }

    #[test]
    fn asset_lifecycle_draft_to_active() {
        let mut asset = test_asset();
        assert!(!asset.is_tradeable());
        assert!(asset.activate());
        assert!(asset.is_tradeable());
    }

    #[test]
    fn asset_freeze_and_check() {
        let mut asset = test_asset();
        asset.activate();
        assert!(asset.is_tradeable());

        asset.freeze();
        assert!(asset.is_frozen());
        assert!(!asset.is_tradeable());
    }

    #[test]
    fn asset_attach_document() {
        let mut asset = test_asset();
        asset.attach_document(DocumentRecord {
            doc_type: DocumentType::Prospectus,
            hash: H256::from([0xAA; 32]),
            uri: "ipfs://QmProspectus".to_string(),
            added_at: 200,
            added_by: Address::from([0x10; 20]),
        });
        assert_eq!(asset.documents.len(), 1);
        assert!(asset.documents.contains_key(&DocumentType::Prospectus));
    }

    #[test]
    fn asset_borsh_roundtrip() {
        let mut asset = test_asset();
        asset.attach_document(DocumentRecord {
            doc_type: DocumentType::TermSheet,
            hash: H256::from([0xBB; 32]),
            uri: "https://docs.example.com/term-sheet".to_string(),
            added_at: 150,
            added_by: Address::from([0x10; 20]),
        });
        let bytes = borsh::to_vec(&asset).unwrap();
        let asset2 = AssetRecord::try_from_slice(&bytes).unwrap();
        assert_eq!(asset, asset2);
    }

    #[test]
    fn ownership_credit_debit() {
        let mut ownership = OwnershipRecord::new(H256::from([0x01; 32]), Address::from([0x01; 20]));
        assert_eq!(ownership.total(), Amount::ZERO);

        ownership.credit(Amount::from_vtt(1000));
        assert_eq!(ownership.available, Amount::from_vtt(1000));

        assert!(ownership.debit(Amount::from_vtt(300)));
        assert_eq!(ownership.available, Amount::from_vtt(700));

        assert!(!ownership.debit(Amount::from_vtt(800))); // insufficient
        assert_eq!(ownership.available, Amount::from_vtt(700));
    }

    #[test]
    fn ownership_lock_unlock() {
        let mut ownership = OwnershipRecord::new(H256::from([0x01; 32]), Address::from([0x01; 20]));
        ownership.credit(Amount::from_vtt(1000));

        assert!(ownership.lock(Amount::from_vtt(400)));
        assert_eq!(ownership.available, Amount::from_vtt(600));
        assert_eq!(ownership.locked, Amount::from_vtt(400));
        assert_eq!(ownership.total(), Amount::from_vtt(1000));

        assert!(ownership.unlock(Amount::from_vtt(200)));
        assert_eq!(ownership.available, Amount::from_vtt(800));
        assert_eq!(ownership.locked, Amount::from_vtt(200));

        assert!(!ownership.lock(Amount::from_vtt(900))); // insufficient available
    }

    #[test]
    fn all_asset_classes() {
        let classes = vec![
            AssetClass::Equity,
            AssetClass::Debt,
            AssetClass::RealEstate,
            AssetClass::Commodity,
            AssetClass::Fund,
            AssetClass::IntellectualProperty,
            AssetClass::CarbonCredit,
            AssetClass::Invoice,
            AssetClass::Custom("SpecialAsset".to_string()),
        ];
        for class in classes {
            let bytes = borsh::to_vec(&class).unwrap();
            let class2 = AssetClass::try_from_slice(&bytes).unwrap();
            assert_eq!(class, class2);
        }
    }
}
