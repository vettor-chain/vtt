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
    pub total_burned: Amount,
    pub total_minted: Amount,
}

/// RPC response for consensus parameters.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConsensusParamsRpc {
    pub epoch_length: u64,
    pub block_time_ms: u64,
    pub active_validators: u32,
    pub min_self_stake: Amount,
    pub unbonding_period_secs: u64,
    pub slash_double_sign_bps: u16,
    pub slash_downtime_bps: u16,
    pub downtime_threshold_pct: u8,
}

/// RPC response for gas configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GasConfigRpc {
    pub min_gas_price: Amount,
    pub base_transfer_cost: u64,
    pub cost_per_byte: u64,
}

/// RPC response for validator info.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorInfoRpc {
    pub address: Address,
    pub total_stake: Amount,
    pub self_stake: Amount,
    pub commission_bps: u16,
    pub is_active: bool,
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

/// RPC response for transaction info.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransactionInfo {
    pub hash: H256,
    pub block_number: BlockNumber,
    pub from: Address,
    pub to: Option<Address>,
    pub action_type: String,
    pub amount: Amount,
    pub nonce: u64,
    pub gas_price: Amount,
    pub gas_limit: u64,
    pub timestamp: u64,
    /// Swap-specific: pool ID (hex)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_pool_id: Option<String>,
    /// Swap-specific: token being sold (hex)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_token_in: Option<String>,
    /// Swap-specific: minimum output amount
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_min_out: Option<Amount>,
}

/// Paginated result wrapper.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaginatedResult<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub page: usize,
    pub page_size: usize,
}

/// RPC response for staking info.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StakingInfo {
    pub address: Address,
    pub self_stake: Amount,
    pub total_stake: Amount,
    pub commission_bps: u16,
    pub active: bool,
    pub delegations: Vec<DelegationInfo>,
}

/// RPC response for DEX pool info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolInfo {
    pub pool_id: String,
    pub token_a: String,
    pub token_b: String,
    pub reserve_a: String,
    pub reserve_b: String,
    pub lp_token_id: String,
    pub lp_total_supply: String,
    pub fee_bps: u16,
    pub protocol_fee_bps: u16,
    pub protocol_fees_a: String,
    pub protocol_fees_b: String,
}

/// RPC response for a DEX swap quote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapQuoteRpc {
    pub amount_in: String,
    pub amount_out: String,
    pub price_impact_bps: u32,
    pub fee: String,
}

/// RPC response for token price in VTT (derived from DEX pool reserves).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPriceRpc {
    pub token_id: H256,
    /// Price in VTT as raw Amount string (18 decimals).
    pub price_in_vtt: String,
    pub pool_id: H256,
}

/// RPC response for a single pool's spot prices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolPriceRpc {
    pub pool_id: H256,
    pub token_a: H256,
    pub token_b: H256,
    /// How much token_b per 1 token_a (18-decimal string).
    pub price_a_in_b: String,
    /// How much token_a per 1 token_b (18-decimal string).
    pub price_b_in_a: String,
    /// Reserve of token_a.
    pub tvl_a: String,
    /// Reserve of token_b.
    pub tvl_b: String,
}

/// Delegation info within staking.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DelegationInfo {
    pub delegator: Address,
    pub amount: Amount,
}

/// RPC response for asset governance proposal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssetProposalInfo {
    pub id: H256,
    pub asset_id: H256,
    pub proposer: Address,
    pub action_type: String,
    pub description: String,
    pub status: String,
    pub votes_yes: Amount,
    pub votes_no: Amount,
    pub votes_abstain: Amount,
    pub voting_end: BlockNumber,
    pub created_at: BlockNumber,
}

/// RPC response for governance proposal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProposalInfo {
    pub id: H256,
    pub proposer: Address,
    pub description: String,
    pub action_type: String,
    pub status: String,
    pub votes_yes: Amount,
    pub votes_no: Amount,
    pub votes_abstain: Amount,
    pub created_at: BlockNumber,
    pub voting_end: BlockNumber,
    /// Human-readable detail of the proposal action (e.g. treasury amount, parameter key=value).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_detail: Option<String>,
}

/// RPC response for bridge withdrawal events (used by the relayer).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeWithdrawalInfo {
    pub tx_hash: H256,
    pub block_number: BlockNumber,
    pub sender: Address,
    pub token: H256,
    pub amount: Amount,
    pub destination_chain: u32,
    pub destination_address: Address,
    pub timestamp: u64,
}

/// Node metrics for monitoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMetricsInfo {
    pub block_height: i64,
    pub connected_peers: i64,
    pub txpool_size: i64,
    pub blocks_imported: u64,
    pub transactions_executed: u64,
    pub current_epoch: i64,
    pub active_validators: i64,
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
            total_burned: Amount::ZERO,
            total_minted: Amount::ZERO,
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
