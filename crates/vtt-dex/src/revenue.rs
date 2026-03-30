use borsh::{BorshDeserialize, BorshSerialize};
use std::collections::BTreeMap;
use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, H256};
use vtt_state::StateDB;

use crate::error::DexError;
use crate::liquidity::{load_pool, save_pool, transfer_token_out};

/// Revenue distributor for VTT-REV holders
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, Default)]
pub struct RevenueDistributor {
    pub revenue_token_id: H256,
    pub revenue_token_supply: u128,
    pub total_accumulated_a: u128,
    pub total_accumulated_b: u128,
    pub claims: BTreeMap<[u8; 20], (u128, u128)>, // address bytes -> (claimed_a, claimed_b)
}

/// Claim protocol fees (treasury) from a pool
pub fn claim_protocol_fees(
    state: &mut StateDB,
    sender: &Address,
    pool_id: &H256,
    treasury: &Address,
) -> Result<(Amount, Amount), DexError> {
    if sender != treasury {
        return Err(DexError::NotAuthorized);
    }

    let mut pool = load_pool(state, pool_id)?;

    let fees_a = pool.protocol_fees_a;
    let fees_b = pool.protocol_fees_b;

    if fees_a.0 == 0 && fees_b.0 == 0 {
        return Err(DexError::NothingToClaim);
    }

    // Transfer accumulated fees to treasury
    if fees_a.0 > 0 {
        transfer_token_out(state, treasury, &pool.token_a, fees_a)?;
    }
    if fees_b.0 > 0 {
        transfer_token_out(state, treasury, &pool.token_b, fees_b)?;
    }

    // Reset accumulators
    pool.protocol_fees_a = Amount::ZERO;
    pool.protocol_fees_b = Amount::ZERO;
    save_pool(state, &pool)?;

    Ok((fees_a, fees_b))
}
