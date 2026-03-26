use vtt_primitives::amount::Amount;

/// Block reward distribution percentages.
const PRODUCER_SHARE_PCT: u128 = 80;
const TREASURY_SHARE_PCT: u128 = 20;

/// Gas fee distribution percentages.
const GAS_BURN_PCT: u128 = 70;
const GAS_PRODUCER_PCT: u128 = 30;

/// Calculate block reward for the current epoch.
///
/// Target: 5% annual inflation, adjusted by staking ratio.
/// If staking_ratio < 0.6, increase to incentivize staking.
/// If staking_ratio > 0.6, decrease.
pub fn calculate_epoch_reward(total_supply: Amount, staking_ratio_pct: u64) -> Amount {
    // Target 5% annual, ~8760 epochs/year (1h epochs)
    let base_rate_bps: u128 = 50; // 5% = 50 per mille annuo

    // Adjust by staking ratio: target is 60%
    let adjusted = if staking_ratio_pct == 0 {
        base_rate_bps * 2 // max multiplier if nobody staking
    } else {
        let ratio = (60u128 * base_rate_bps) / staking_ratio_pct as u128;
        ratio.min(base_rate_bps * 2).max(base_rate_bps / 2)
    };

    let epochs_per_year: u128 = 8760;
    let epoch_reward_raw = total_supply.raw() * adjusted / (1000 * epochs_per_year);
    Amount::from_raw(epoch_reward_raw)
}

/// Calculate how a single block reward is split.
pub struct BlockRewardSplit {
    /// Amount going to the block producer.
    pub producer: Amount,
    /// Amount going to the protocol treasury.
    pub treasury: Amount,
}

/// Split a block reward between producer and treasury.
pub fn split_block_reward(total_reward: Amount) -> BlockRewardSplit {
    let producer_raw = total_reward.raw() * PRODUCER_SHARE_PCT / 100;
    let treasury_raw = total_reward.raw() * TREASURY_SHARE_PCT / 100;

    BlockRewardSplit {
        producer: Amount::from_raw(producer_raw),
        treasury: Amount::from_raw(treasury_raw),
    }
}

/// Split gas fees between burning and block producer.
pub struct GasFeeSplit {
    /// Amount burned (removed from circulation).
    pub burned: Amount,
    /// Amount going to the block producer.
    pub producer: Amount,
}

/// Split gas fees between burning and producer.
pub fn split_gas_fees(total_fees: Amount) -> GasFeeSplit {
    let burned_raw = total_fees.raw() * GAS_BURN_PCT / 100;
    let producer_raw = total_fees.raw() * GAS_PRODUCER_PCT / 100;

    GasFeeSplit {
        burned: Amount::from_raw(burned_raw),
        producer: Amount::from_raw(producer_raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_reward_split_80_20() {
        let reward = Amount::from_vtt(100);
        let split = split_block_reward(reward);
        assert_eq!(split.producer, Amount::from_vtt(80));
        assert_eq!(split.treasury, Amount::from_vtt(20));
    }

    #[test]
    fn gas_fee_split_70_30() {
        let fees = Amount::from_vtt(100);
        let split = split_gas_fees(fees);
        assert_eq!(split.burned, Amount::from_vtt(70));
        assert_eq!(split.producer, Amount::from_vtt(30));
    }

    #[test]
    fn epoch_reward_at_target_staking() {
        let supply = Amount::from_vtt(1_000_000_000); // 1 billion
        let reward = calculate_epoch_reward(supply, 60); // 60% staked = target
                                                         // ~5% / 8760 epochs = ~5708 VTT per epoch
        assert!(reward.whole_vtt() > 0);
        assert!(reward.whole_vtt() < 10_000); // sanity check
    }

    #[test]
    fn epoch_reward_low_staking_higher() {
        let supply = Amount::from_vtt(1_000_000_000);
        let reward_low = calculate_epoch_reward(supply, 30); // 30% staked
        let reward_target = calculate_epoch_reward(supply, 60); // 60% staked
                                                                // Lower staking ratio = higher reward to incentivize
        assert!(reward_low > reward_target);
    }

    #[test]
    fn epoch_reward_high_staking_lower() {
        let supply = Amount::from_vtt(1_000_000_000);
        let reward_high = calculate_epoch_reward(supply, 90); // 90% staked
        let reward_target = calculate_epoch_reward(supply, 60); // 60% staked
                                                                // Higher staking ratio = lower reward
        assert!(reward_high < reward_target);
    }

    #[test]
    fn epoch_reward_zero_staking() {
        let supply = Amount::from_vtt(1_000_000_000);
        let reward = calculate_epoch_reward(supply, 0);
        // Should give max reward when nobody is staking
        assert!(reward.whole_vtt() > 0);
    }

    #[test]
    fn zero_fees_zero_split() {
        let split = split_gas_fees(Amount::ZERO);
        assert_eq!(split.burned, Amount::ZERO);
        assert_eq!(split.producer, Amount::ZERO);
    }
}
