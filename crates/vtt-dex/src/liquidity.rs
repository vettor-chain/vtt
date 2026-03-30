use borsh::BorshDeserialize;
use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, ChainId, H256};
use vtt_state::asset::{AssetClass, AssetRecord, AssetStatus};
use vtt_state::StateDB;

use crate::error::DexError;
use crate::math::{sqrt_u128, U256};
use crate::pool::*;

/// Create a new liquidity pool
pub fn create_pool(
    state: &mut StateDB,
    sender: &Address,
    token_a: H256,
    token_b: H256,
    amount_a: Amount,
    amount_b: Amount,
    current_epoch: Epoch,
) -> Result<PoolState, DexError> {
    if token_a == token_b {
        return Err(DexError::SameToken);
    }
    if amount_a.0 == 0 || amount_b.0 == 0 {
        return Err(DexError::ZeroAmount);
    }

    let pool_id = compute_pool_id(&token_a, &token_b);

    if state.has_pool(&pool_id) {
        return Err(DexError::PoolAlreadyExists { pool_id });
    }

    // Transfer tokens from sender to pool (pool is virtual — tokens stay in state)
    transfer_token_in(state, sender, &token_a, amount_a)?;
    transfer_token_in(state, sender, &token_b, amount_b)?;

    // Mint LP tokens: sqrt(amount_a * amount_b)
    let lp_minted = sqrt_u128(
        U256::mul_u128(amount_a.0, amount_b.0)
            .div_u128(1)
            .map_err(|_| DexError::Overflow)?,
    );

    if lp_minted <= MINIMUM_LIQUIDITY {
        return Err(DexError::ZeroLiquidity);
    }

    // Create LP token as an on-chain asset
    let lp_token_id = compute_lp_token_id(&pool_id);
    let lp_for_user = Amount::from_raw(lp_minted - MINIMUM_LIQUIDITY);
    let lp_total = Amount::from_raw(lp_minted);

    // Register LP token asset
    register_lp_asset(state, &lp_token_id, &pool_id, lp_total)?;

    // Mint LP tokens: MINIMUM_LIQUIDITY burned (to zero address), rest to sender
    mint_lp_to(state, &lp_token_id, sender, lp_for_user)?;

    let pool = PoolState {
        pool_id,
        token_a,
        token_b,
        reserve_a: amount_a,
        reserve_b: amount_b,
        lp_token_id,
        lp_total_supply: lp_total,
        fee_bps: DEFAULT_FEE_BPS,
        protocol_fee_bps: DEFAULT_PROTOCOL_FEE_BPS,
        protocol_fees_a: Amount::ZERO,
        protocol_fees_b: Amount::ZERO,
        creator: *sender,
        created_at_epoch: current_epoch,
    };

    let data = borsh::to_vec(&pool).map_err(|_| DexError::Overflow)?;
    state.put_pool_raw(pool_id, data);

    Ok(pool)
}

/// Add liquidity to existing pool
pub fn add_liquidity(
    state: &mut StateDB,
    sender: &Address,
    pool_id: &H256,
    amount_a: Amount,
    amount_b: Amount,
    min_lp: Amount,
) -> Result<Amount, DexError> {
    let mut pool = load_pool(state, pool_id)?;

    if pool.reserve_a.0 == 0 || pool.reserve_b.0 == 0 {
        return Err(DexError::ZeroLiquidity);
    }

    // Calculate optimal amounts maintaining ratio
    let optimal_b = U256::mul_u128(amount_a.0, pool.reserve_b.0)
        .div_u128(pool.reserve_a.0)?;
    let (actual_a, actual_b) = if optimal_b <= amount_b.0 {
        (amount_a.0, optimal_b)
    } else {
        let optimal_a = U256::mul_u128(amount_b.0, pool.reserve_a.0)
            .div_u128(pool.reserve_b.0)?;
        (optimal_a, amount_b.0)
    };

    if actual_a == 0 || actual_b == 0 {
        return Err(DexError::ZeroAmount);
    }

    // Mint LP tokens proportional to contribution
    let lp_a = U256::mul_u128(actual_a, pool.lp_total_supply.0)
        .div_u128(pool.reserve_a.0)?;
    let lp_b = U256::mul_u128(actual_b, pool.lp_total_supply.0)
        .div_u128(pool.reserve_b.0)?;
    let lp_minted = std::cmp::min(lp_a, lp_b);

    if lp_minted < min_lp.0 {
        return Err(DexError::SlippageExceeded {
            expected: min_lp.0,
            got: lp_minted,
        });
    }

    // Transfer tokens in
    transfer_token_in(state, sender, &pool.token_a, Amount::from_raw(actual_a))?;
    transfer_token_in(state, sender, &pool.token_b, Amount::from_raw(actual_b))?;

    // Mint LP tokens to sender
    let lp_amount = Amount::from_raw(lp_minted);
    mint_lp_to(state, &pool.lp_token_id, sender, lp_amount)?;

    // Update pool
    pool.reserve_a = Amount::from_raw(pool.reserve_a.0 + actual_a);
    pool.reserve_b = Amount::from_raw(pool.reserve_b.0 + actual_b);
    pool.lp_total_supply = Amount::from_raw(pool.lp_total_supply.0 + lp_minted);
    save_pool(state, &pool)?;

    Ok(lp_amount)
}

/// Remove liquidity from pool
pub fn remove_liquidity(
    state: &mut StateDB,
    sender: &Address,
    pool_id: &H256,
    lp_amount: Amount,
    min_a: Amount,
    min_b: Amount,
) -> Result<(Amount, Amount), DexError> {
    let mut pool = load_pool(state, pool_id)?;

    if lp_amount.0 == 0 {
        return Err(DexError::ZeroAmount);
    }

    // Calculate proportional share
    let amount_a = U256::mul_u128(lp_amount.0, pool.reserve_a.0)
        .div_u128(pool.lp_total_supply.0)?;
    let amount_b = U256::mul_u128(lp_amount.0, pool.reserve_b.0)
        .div_u128(pool.lp_total_supply.0)?;

    if amount_a < min_a.0 {
        return Err(DexError::SlippageExceeded { expected: min_a.0, got: amount_a });
    }
    if amount_b < min_b.0 {
        return Err(DexError::SlippageExceeded { expected: min_b.0, got: amount_b });
    }

    // Burn LP tokens from sender
    burn_lp_from(state, &pool.lp_token_id, sender, lp_amount)?;

    // Transfer tokens out
    let out_a = Amount::from_raw(amount_a);
    let out_b = Amount::from_raw(amount_b);
    transfer_token_out(state, sender, &pool.token_a, out_a)?;
    transfer_token_out(state, sender, &pool.token_b, out_b)?;

    // Update pool
    pool.reserve_a = Amount::from_raw(pool.reserve_a.0 - amount_a);
    pool.reserve_b = Amount::from_raw(pool.reserve_b.0 - amount_b);
    pool.lp_total_supply = Amount::from_raw(pool.lp_total_supply.0 - lp_amount.0);
    save_pool(state, &pool)?;

    Ok((out_a, out_b))
}

// --- Helpers ---

pub fn load_pool(state: &StateDB, pool_id: &H256) -> Result<PoolState, DexError> {
    let data = state
        .get_pool_raw(pool_id)
        .ok_or(DexError::PoolNotFound { pool_id: *pool_id })?;
    PoolState::try_from_slice(data).map_err(|_| DexError::PoolNotFound { pool_id: *pool_id })
}

pub fn save_pool(state: &mut StateDB, pool: &PoolState) -> Result<(), DexError> {
    let data = borsh::to_vec(pool).map_err(|_| DexError::Overflow)?;
    state.put_pool_raw(pool.pool_id, data);
    Ok(())
}

fn compute_lp_token_id(pool_id: &H256) -> H256 {
    let mut data = b"lp:".to_vec();
    data.extend_from_slice(pool_id.as_bytes());
    vtt_crypto::blake3_hash(&data)
}

pub(crate) fn transfer_token_in(
    state: &mut StateDB,
    sender: &Address,
    token: &H256,
    amount: Amount,
) -> Result<(), DexError> {
    if PoolState::is_native(token) {
        state.sub_balance(sender, amount).map_err(|_| DexError::InsufficientBalance)?;
    } else {
        // For assets, debit from sender's ownership. Pool address = Address::ZERO.
        state
            .transfer_asset(token, sender, &Address::ZERO, amount)
            .map_err(|_| DexError::InsufficientBalance)?;
    }
    Ok(())
}

pub(crate) fn transfer_token_out(
    state: &mut StateDB,
    recipient: &Address,
    token: &H256,
    amount: Amount,
) -> Result<(), DexError> {
    if PoolState::is_native(token) {
        state.add_balance(recipient, amount).map_err(|_| DexError::Overflow)?;
    } else {
        state
            .transfer_asset(token, &Address::ZERO, recipient, amount)
            .map_err(|_| DexError::InsufficientLiquidity)?;
    }
    Ok(())
}

fn register_lp_asset(
    state: &mut StateDB,
    lp_token_id: &H256,
    pool_id: &H256,
    total_supply: Amount,
) -> Result<(), DexError> {
    let id_bytes = pool_id.as_bytes();
    let name = format!(
        "LP-{:02x}{:02x}{:02x}{:02x}",
        id_bytes[0], id_bytes[1], id_bytes[2], id_bytes[3]
    );
    let symbol = format!("LP-{:02x}{:02x}", id_bytes[0], id_bytes[1]);

    let asset = AssetRecord {
        id: *lp_token_id,
        name,
        symbol,
        class: AssetClass::Fund,
        origin_chain: ChainId::RELAY,
        issuer: Address::ZERO,
        total_supply,
        decimals: 18,
        status: AssetStatus::Active,
        compliance_policy: None,
        valuation_oracle: None,
        documents: Default::default(),
        metadata_uri: String::new(),
        created_at: 0,
    };

    state.register_asset(asset).map_err(|_| DexError::Overflow)?;
    Ok(())
}

fn mint_lp_to(
    state: &mut StateDB,
    lp_token_id: &H256,
    recipient: &Address,
    amount: Amount,
) -> Result<(), DexError> {
    let mut record = state.get_ownership(lp_token_id, recipient);
    record.credit(amount);
    state.put_ownership(record);
    Ok(())
}

fn burn_lp_from(
    state: &mut StateDB,
    lp_token_id: &H256,
    sender: &Address,
    amount: Amount,
) -> Result<(), DexError> {
    let mut record = state.get_ownership(lp_token_id, sender);
    if !record.debit(amount) {
        return Err(DexError::InsufficientBalance);
    }
    state.put_ownership(record);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtt_state::StateDB;

    fn make_asset_token(id: u8) -> H256 {
        H256::from([id; 32])
    }

    fn make_address(id: u8) -> Address {
        Address::from([id; 20])
    }

    fn setup_asset(state: &mut StateDB, token_id: H256, holder: Address, amount: Amount) {
        let asset = AssetRecord {
            id: token_id,
            name: "Test".to_string(),
            symbol: "TST".to_string(),
            class: AssetClass::Equity,
            origin_chain: ChainId::RELAY,
            issuer: holder,
            total_supply: amount,
            decimals: 18,
            status: AssetStatus::Active,
            compliance_policy: None,
            valuation_oracle: None,
            documents: Default::default(),
            metadata_uri: String::new(),
            created_at: 0,
        };
        state.register_asset(asset).unwrap();
        let mut rec = state.get_ownership(&token_id, &holder);
        rec.credit(amount);
        state.put_ownership(rec);
    }

    #[test]
    fn test_create_pool_same_token() {
        let mut state = StateDB::new();
        let token = make_asset_token(1);
        let sender = make_address(1);
        let err = create_pool(
            &mut state,
            &sender,
            token,
            token,
            Amount::from_raw(1000),
            Amount::from_raw(1000),
            0,
        );
        assert!(matches!(err, Err(DexError::SameToken)));
    }

    #[test]
    fn test_create_pool_zero_amount() {
        let mut state = StateDB::new();
        let token_a = make_asset_token(1);
        let token_b = make_asset_token(2);
        let sender = make_address(1);
        let err = create_pool(
            &mut state,
            &sender,
            token_a,
            token_b,
            Amount::ZERO,
            Amount::from_raw(1000),
            0,
        );
        assert!(matches!(err, Err(DexError::ZeroAmount)));
    }

    #[test]
    fn test_create_pool_with_assets() {
        let mut state = StateDB::new();
        let token_a = make_asset_token(1);
        let token_b = make_asset_token(2);
        let sender = make_address(10);

        let amount_a = Amount::from_raw(1_000_000);
        let amount_b = Amount::from_raw(4_000_000);

        setup_asset(&mut state, token_a, sender, amount_a);
        setup_asset(&mut state, token_b, sender, amount_b);

        let pool = create_pool(
            &mut state,
            &sender,
            token_a,
            token_b,
            amount_a,
            amount_b,
            1,
        )
        .unwrap();

        assert_eq!(pool.reserve_a, amount_a);
        assert_eq!(pool.reserve_b, amount_b);
        // lp_total = sqrt(1e6 * 4e6) = sqrt(4e12) = 2e6
        assert_eq!(pool.lp_total_supply.0, 2_000_000);
        // user gets lp_total - MINIMUM_LIQUIDITY
        let user_lp = state.get_ownership(&pool.lp_token_id, &sender);
        assert_eq!(user_lp.available.0, 2_000_000 - MINIMUM_LIQUIDITY);
    }

    #[test]
    fn test_create_pool_with_native() {
        let mut state = StateDB::new();
        let native = H256::ZERO;
        let token_b = make_asset_token(2);
        let sender = make_address(10);

        let amount_native = Amount::from_raw(1_000_000);
        let amount_b = Amount::from_raw(1_000_000);

        // Give sender native balance
        state.add_balance(&sender, amount_native).unwrap();
        setup_asset(&mut state, token_b, sender, amount_b);

        let pool = create_pool(
            &mut state,
            &sender,
            native,
            token_b,
            amount_native,
            amount_b,
            1,
        )
        .unwrap();

        assert_eq!(pool.reserve_a.0, 1_000_000);
        assert_eq!(pool.reserve_b.0, 1_000_000);
        // Sender's native balance should be 0
        assert_eq!(state.get_balance(&sender).0, 0);
    }

    #[test]
    fn test_create_pool_duplicate() {
        let mut state = StateDB::new();
        let token_a = make_asset_token(1);
        let token_b = make_asset_token(2);
        let sender = make_address(10);

        setup_asset(&mut state, token_a, sender, Amount::from_raw(2_000_000));
        setup_asset(&mut state, token_b, sender, Amount::from_raw(8_000_000));

        create_pool(&mut state, &sender, token_a, token_b, Amount::from_raw(1_000_000), Amount::from_raw(4_000_000), 0).unwrap();

        // Re-register tokens for second create attempt
        // (already registered — but we need balance for the second attempt, it won't reach that point)
        let err = create_pool(&mut state, &sender, token_a, token_b, Amount::from_raw(1_000_000), Amount::from_raw(4_000_000), 0);
        assert!(matches!(err, Err(DexError::PoolAlreadyExists { .. })));
    }

    #[test]
    fn test_add_remove_liquidity() {
        let mut state = StateDB::new();
        let token_a = make_asset_token(1);
        let token_b = make_asset_token(2);
        let sender = make_address(10);
        let lp_provider = make_address(11);

        setup_asset(&mut state, token_a, sender, Amount::from_raw(10_000_000));
        setup_asset(&mut state, token_b, sender, Amount::from_raw(10_000_000));

        // Also give lp_provider some tokens (register first, then credit)
        let mut rec_a = state.get_ownership(&token_a, &lp_provider);
        rec_a.credit(Amount::from_raw(5_000_000));
        state.put_ownership(rec_a);
        let mut rec_b = state.get_ownership(&token_b, &lp_provider);
        rec_b.credit(Amount::from_raw(5_000_000));
        state.put_ownership(rec_b);

        let pool = create_pool(
            &mut state,
            &sender,
            token_a,
            token_b,
            Amount::from_raw(1_000_000),
            Amount::from_raw(4_000_000),
            0,
        )
        .unwrap();

        let pool_id = pool.pool_id;

        // Add liquidity
        let lp_minted = add_liquidity(
            &mut state,
            &lp_provider,
            &pool_id,
            Amount::from_raw(500_000),
            Amount::from_raw(2_000_000),
            Amount::ZERO,
        )
        .unwrap();
        assert!(lp_minted.0 > 0);

        // Remove liquidity
        let (out_a, out_b) = remove_liquidity(
            &mut state,
            &lp_provider,
            &pool_id,
            lp_minted,
            Amount::ZERO,
            Amount::ZERO,
        )
        .unwrap();
        assert!(out_a.0 > 0);
        assert!(out_b.0 > 0);
    }
}
