pub mod account;
pub mod asset;
pub mod oracle;
pub mod statedb;
pub mod trie;

pub use account::{AccountState, StakingState};
pub use asset::{AssetClass, AssetRecord, AssetStatus, OwnershipRecord};
pub use oracle::OracleFeed;
pub use statedb::StateDB;

use vtt_primitives::amount::Amount;
use vtt_primitives::Address;

/// A slashing event record. Emitted by `StateDB::slashing_history` for RPC.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlashRecord {
    pub validator: Address,
    pub epoch: u64,
    pub reason: String,
    pub amount: Amount,
}
