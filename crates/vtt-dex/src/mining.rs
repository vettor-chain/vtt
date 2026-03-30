use borsh::{BorshDeserialize, BorshSerialize};
use std::collections::BTreeMap;
use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, H256};
use vtt_state::StateDB;

use crate::error::DexError;
use crate::liquidity::load_pool;
use crate::math::U256;
use crate::pool::Epoch;

const PRECISION: u128 = 10u128.pow(18);

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct MiningPhase {
    pub duration_epochs: u64,
    pub reward_per_epoch: Amount,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct MiningConfig {
    pub pool_id: H256,
    pub total_budget: Amount,
    pub source: Address,
    pub phases: Vec<MiningPhase>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct MiningState {
    pub config: MiningConfig,
    pub start_epoch: Epoch,
    /// Accumulated rewards per LP token, scaled by PRECISION
    pub reward_per_lp_accumulated: u128,
    pub last_update_epoch: Epoch,
    pub total_distributed: Amount,
    pub claims: BTreeMap<[u8; 20], MiningClaim>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, Default)]
pub struct MiningClaim {
    pub reward_debt: u128,
    pub unclaimed: u128,
}

impl MiningState {
    /// Get the reward per epoch for a given epoch number
    pub fn reward_at_epoch(&self, epoch: Epoch) -> u128 {
        let elapsed = epoch.saturating_sub(self.start_epoch);
        let mut boundary = 0u64;
        for phase in &self.config.phases {
            boundary += phase.duration_epochs;
            if elapsed < boundary {
                return phase.reward_per_epoch.0;
            }
        }
        0 // Past all phases — no more rewards
    }

    /// Update accumulated rewards to current epoch
    pub fn update_to_epoch(&mut self, current_epoch: Epoch, lp_total_supply: u128) {
        if current_epoch <= self.last_update_epoch || lp_total_supply == 0 {
            self.last_update_epoch = current_epoch;
            return;
        }

        for epoch in (self.last_update_epoch + 1)..=current_epoch {
            let reward = self.reward_at_epoch(epoch);
            if reward > 0 && lp_total_supply > 0 {
                // reward_per_lp += reward * PRECISION / lp_total_supply
                let delta = U256::mul_u128(reward, PRECISION)
                    .div_u128(lp_total_supply)
                    .unwrap_or(0);
                self.reward_per_lp_accumulated = self.reward_per_lp_accumulated.saturating_add(delta);
            }
        }
        self.last_update_epoch = current_epoch;
    }

    /// Calculate pending rewards for a user
    pub fn pending_rewards(&self, user_lp_balance: u128, user: &Address) -> u128 {
        let key = address_key(user);
        let claim = self.claims.get(&key).cloned().unwrap_or_default();

        let total_earned = U256::mul_u128(user_lp_balance, self.reward_per_lp_accumulated)
            .div_u128(PRECISION)
            .unwrap_or(0);

        total_earned.saturating_sub(claim.reward_debt) + claim.unclaimed
    }

    /// Claim rewards for a user, returning the amount to transfer
    pub fn claim(&mut self, user_lp_balance: u128, user: &Address) -> u128 {
        let pending = self.pending_rewards(user_lp_balance, user);
        let key = address_key(user);

        let new_debt = U256::mul_u128(user_lp_balance, self.reward_per_lp_accumulated)
            .div_u128(PRECISION)
            .unwrap_or(0);

        self.claims.insert(key, MiningClaim {
            reward_debt: new_debt,
            unclaimed: 0,
        });

        self.total_distributed = Amount::from_raw(self.total_distributed.0.saturating_add(pending));
        pending
    }
}

/// Claim mining rewards for a user
pub fn claim_mining_rewards(
    state: &mut StateDB,
    sender: &Address,
    pool_id: &H256,
    current_epoch: Epoch,
    mining_state: &mut MiningState,
) -> Result<Amount, DexError> {
    let pool = load_pool(state, pool_id)?;

    // Update accumulated rewards
    mining_state.update_to_epoch(current_epoch, pool.lp_total_supply.0);

    // Get user's LP balance
    let ownership = state.get_ownership(&pool.lp_token_id, sender);
    let user_lp = ownership.available.0;

    if user_lp == 0 {
        return Err(DexError::NothingToClaim);
    }

    // Calculate and claim
    let reward = mining_state.claim(user_lp, sender);
    if reward == 0 {
        return Err(DexError::NothingToClaim);
    }

    // Transfer VTT from source to user
    let reward_amount = Amount::from_raw(reward);
    state
        .sub_balance(&mining_state.config.source, reward_amount)
        .map_err(|_| DexError::InsufficientBalance)?;
    state
        .add_balance(sender, reward_amount)
        .map_err(|_| DexError::Overflow)?;

    Ok(reward_amount)
}

fn address_key(addr: &Address) -> [u8; 20] {
    let mut key = [0u8; 20];
    key.copy_from_slice(addr.as_bytes());
    key
}
