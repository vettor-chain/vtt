use serde::{Deserialize, Serialize};

use vtt_primitives::amount::Amount;
use vtt_primitives::block::BlockHeader;
use vtt_primitives::transaction::TransactionReceipt;
use vtt_primitives::{Address, BlockNumber, ChainId, Epoch, H256};

/// RPC response for block info.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockInfo {
    pub hash: H256,
    pub number: BlockNumber,
    pub parent_hash: H256,
    pub state_root: H256,
    pub transactions_root: H256,
    pub validator: Address,
    pub epoch: Epoch,
    pub slot: u32,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub gas_used: u64,
    pub tx_count: usize,
}

impl BlockInfo {
    pub fn from_header(header: &BlockHeader, hash: H256, tx_count: usize) -> Self {
        Self {
            hash,
            number: header.number,
            parent_hash: header.parent_hash,
            state_root: header.state_root,
            transactions_root: header.transactions_root,
            validator: header.validator,
            epoch: header.epoch,
            slot: header.slot,
            timestamp: header.timestamp,
            gas_limit: header.gas_limit,
            gas_used: header.gas_used,
            tx_count,
        }
    }
}

/// RPC response for chain status.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainStatus {
    pub chain_id: ChainId,
    pub height: BlockNumber,
    pub head_hash: H256,
    pub validator_count: usize,
    pub total_stake: Amount,
}

/// RPC response for validator info.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorInfoRpc {
    pub address: Address,
    pub total_stake: Amount,
    pub self_stake: Amount,
    pub commission_bps: u16,
}

/// RPC response for account info.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountInfo {
    pub address: Address,
    pub balance: Amount,
    pub nonce: u64,
    pub is_contract: bool,
}

/// RPC response for transaction receipt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceiptInfo {
    pub tx_hash: H256,
    pub success: bool,
    pub gas_used: u64,
    pub log_count: usize,
}

impl From<&TransactionReceipt> for ReceiptInfo {
    fn from(r: &TransactionReceipt) -> Self {
        Self {
            tx_hash: r.tx_hash,
            success: r.success,
            gas_used: r.gas_used,
            log_count: r.logs.len(),
        }
    }
}

/// RPC response for asset info.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetInfo {
    pub id: H256,
    pub name: String,
    pub symbol: String,
    pub issuer: Address,
    pub total_supply: Amount,
    pub status: String,
    pub decimals: u8,
}

/// RPC response for asset balance.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetBalanceInfo {
    pub asset_id: H256,
    pub owner: Address,
    pub available: Amount,
    pub locked: Amount,
}

/// RPC response for oracle feed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OracleFeedInfo {
    pub feed_id: H256,
    pub name: String,
    pub latest_value: Option<Amount>,
    pub updated_at: u64,
    pub quorum: u8,
    pub sources: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_status_serializes() {
        let status = ChainStatus {
            chain_id: ChainId::RELAY,
            height: 42,
            head_hash: H256::from([0xAB; 32]),
            validator_count: 21,
            total_stake: Amount::from_vtt(1_000_000),
        };
        let json = serde_json::to_string(&status).unwrap();
        let status2: ChainStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status2.height, 42);
    }

    #[test]
    fn account_info_serializes() {
        let info = AccountInfo {
            address: Address::from([0x01; 20]),
            balance: Amount::from_vtt(100),
            nonce: 5,
            is_contract: false,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("nonce"));
    }
}
