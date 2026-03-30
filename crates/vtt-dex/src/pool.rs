use borsh::{BorshDeserialize, BorshSerialize};
use vtt_crypto::blake3_hash;
use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, H256};

pub type Epoch = u64;

/// Deterministic pool ID from sorted token pair
pub fn compute_pool_id(token_a: &H256, token_b: &H256) -> H256 {
    let (first, second) = if token_a <= token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    };
    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(first.as_bytes());
    data.extend_from_slice(second.as_bytes());
    blake3_hash(&data)
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct PoolState {
    pub pool_id: H256,
    pub token_a: H256,
    pub token_b: H256,
    pub reserve_a: Amount,
    pub reserve_b: Amount,
    pub lp_token_id: H256,
    pub lp_total_supply: Amount,
    pub fee_bps: u16,
    pub protocol_fee_bps: u16,
    pub protocol_fees_a: Amount,
    pub protocol_fees_b: Amount,
    pub creator: Address,
    pub created_at_epoch: Epoch,
}

impl PoolState {
    /// The canonical "zero" H256 represents native VTT (not an asset)
    pub const NATIVE_VTT: H256 = H256::ZERO;

    pub fn is_native(token: &H256) -> bool {
        *token == Self::NATIVE_VTT
    }
}

/// Minimum LP tokens burned on first deposit to prevent manipulation
pub const MINIMUM_LIQUIDITY: u128 = 1000;

/// Default fee: 0.3% (30 basis points)
pub const DEFAULT_FEE_BPS: u16 = 30;

/// Default protocol fee: 0.05% (5 basis points of total)
pub const DEFAULT_PROTOCOL_FEE_BPS: u16 = 5;
