use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, H256};
use vtt_state::StateDB;

use crate::error::DexError;
use crate::liquidity::{load_pool, save_pool, transfer_token_in, transfer_token_out};
use crate::math::{calculate_fees, get_amount_out};

/// Execute a token swap on an existing pool
pub fn execute_swap(
    state: &mut StateDB,
    sender: &Address,
    pool_id: &H256,
    token_in: &H256,
    amount_in: Amount,
    min_amount_out: Amount,
) -> Result<Amount, DexError> {
    if state.is_dex_paused() {
        return Err(DexError::DexPaused);
    }

    let mut pool = load_pool(state, pool_id)?;

    if amount_in.0 == 0 {
        return Err(DexError::ZeroAmount);
    }

    // Determine direction
    let (reserve_in, reserve_out, is_a_to_b) = if *token_in == pool.token_a {
        (pool.reserve_a.0, pool.reserve_b.0, true)
    } else if *token_in == pool.token_b {
        (pool.reserve_b.0, pool.reserve_a.0, false)
    } else {
        return Err(DexError::InvalidTokenPair);
    };

    // Calculate fees
    let (amount_in_net, lp_fee, protocol_fee) =
        calculate_fees(amount_in.0, pool.fee_bps, pool.protocol_fee_bps)?;

    // Calculate output
    let amount_out = get_amount_out(amount_in_net, reserve_in, reserve_out)?;

    if amount_out < min_amount_out.0 {
        return Err(DexError::SlippageExceeded {
            expected: min_amount_out.0,
            got: amount_out,
        });
    }

    if amount_out == 0 {
        return Err(DexError::ZeroAmount);
    }

    // Transfer input from sender
    transfer_token_in(state, sender, token_in, amount_in)?;

    // Transfer output to sender
    let token_out = if is_a_to_b {
        &pool.token_b
    } else {
        &pool.token_a
    };
    let out = Amount::from_raw(amount_out);
    transfer_token_out(state, sender, token_out, out)?;

    // Update reserves: input increases by amount_in_net + lp_fee, output decreases by amount_out
    // protocol_fee is accumulated separately
    let effective_in = amount_in_net + lp_fee; // what goes into reserves

    if is_a_to_b {
        pool.reserve_a = Amount::from_raw(pool.reserve_a.0 + effective_in);
        pool.reserve_b = Amount::from_raw(pool.reserve_b.0 - amount_out);
        pool.protocol_fees_a = Amount::from_raw(pool.protocol_fees_a.0 + protocol_fee);
    } else {
        pool.reserve_b = Amount::from_raw(pool.reserve_b.0 + effective_in);
        pool.reserve_a = Amount::from_raw(pool.reserve_a.0 - amount_out);
        pool.protocol_fees_b = Amount::from_raw(pool.protocol_fees_b.0 + protocol_fee);
    }

    save_pool(state, &pool)?;

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtt_primitives::amount::Amount;
    use vtt_primitives::{Address, ChainId, H256};
    use vtt_state::asset::{AssetClass, AssetRecord, AssetStatus};
    use vtt_state::StateDB;

    use crate::liquidity::create_pool;
    use crate::pool::compute_pool_id;

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
            jurisdiction: String::new(),
            legal_entity: String::new(),
            transfer_mode: vtt_state::asset::TransferMode::PeerToPeer,
            registrar: None,
            redemption_pool: Amount::ZERO,
            created_at: 0,
        };
        state.register_asset(asset).unwrap();
        let mut rec = state.get_ownership(&token_id, &holder);
        rec.credit(amount);
        state.put_ownership(rec);
    }

    #[test]
    fn test_swap_a_to_b() {
        let mut state = StateDB::new();
        let token_a = make_asset_token(1);
        let token_b = make_asset_token(2);
        let liquidity_provider = make_address(10);
        let trader = make_address(20);

        // Provide liquidity: 1000 A, 2000 B
        setup_asset(
            &mut state,
            token_a,
            liquidity_provider,
            Amount::from_raw(1_000_000),
        );
        setup_asset(
            &mut state,
            token_b,
            liquidity_provider,
            Amount::from_raw(2_000_000),
        );

        let pool = create_pool(
            &mut state,
            &liquidity_provider,
            token_a,
            token_b,
            Amount::from_raw(1_000_000),
            Amount::from_raw(2_000_000),
            0,
        )
        .unwrap();

        // Give trader some token_a
        let mut rec = state.get_ownership(&token_a, &trader);
        rec.credit(Amount::from_raw(100_000));
        state.put_ownership(rec);

        let pool_id = compute_pool_id(&token_a, &token_b);

        // Swap 100 token_a for token_b
        let amount_out = execute_swap(
            &mut state,
            &trader,
            &pool_id,
            &token_a,
            Amount::from_raw(100_000),
            Amount::ZERO,
        )
        .unwrap();

        assert!(amount_out.0 > 0);

        // Verify trader received token_b
        let trader_b = state.get_ownership(&token_b, &trader);
        assert_eq!(trader_b.available, amount_out);

        // Verify pool state updated
        let updated_pool = crate::liquidity::load_pool(&state, &pool_id).unwrap();
        assert!(updated_pool.reserve_a.0 > pool.reserve_a.0);
        assert!(updated_pool.reserve_b.0 < pool.reserve_b.0);
        assert!(updated_pool.protocol_fees_a.0 > 0);
    }

    #[test]
    fn test_swap_b_to_a() {
        let mut state = StateDB::new();
        let token_a = make_asset_token(1);
        let token_b = make_asset_token(2);
        let lp = make_address(10);
        let trader = make_address(20);

        setup_asset(&mut state, token_a, lp, Amount::from_raw(1_000_000));
        setup_asset(&mut state, token_b, lp, Amount::from_raw(2_000_000));

        create_pool(
            &mut state,
            &lp,
            token_a,
            token_b,
            Amount::from_raw(1_000_000),
            Amount::from_raw(2_000_000),
            0,
        )
        .unwrap();

        let mut rec = state.get_ownership(&token_b, &trader);
        rec.credit(Amount::from_raw(200_000));
        state.put_ownership(rec);

        let pool_id = compute_pool_id(&token_a, &token_b);

        let amount_out = execute_swap(
            &mut state,
            &trader,
            &pool_id,
            &token_b,
            Amount::from_raw(200_000),
            Amount::ZERO,
        )
        .unwrap();

        assert!(amount_out.0 > 0);

        let trader_a = state.get_ownership(&token_a, &trader);
        assert_eq!(trader_a.available, amount_out);
    }

    #[test]
    fn test_swap_invalid_token() {
        let mut state = StateDB::new();
        let token_a = make_asset_token(1);
        let token_b = make_asset_token(2);
        let token_c = make_asset_token(3);
        let lp = make_address(10);

        setup_asset(&mut state, token_a, lp, Amount::from_raw(1_000_000));
        setup_asset(&mut state, token_b, lp, Amount::from_raw(2_000_000));

        create_pool(
            &mut state,
            &lp,
            token_a,
            token_b,
            Amount::from_raw(1_000_000),
            Amount::from_raw(2_000_000),
            0,
        )
        .unwrap();

        let pool_id = compute_pool_id(&token_a, &token_b);

        let err = execute_swap(
            &mut state,
            &lp,
            &pool_id,
            &token_c,
            Amount::from_raw(100),
            Amount::ZERO,
        );
        assert!(matches!(err, Err(DexError::InvalidTokenPair)));
    }

    #[test]
    fn test_swap_slippage_exceeded() {
        let mut state = StateDB::new();
        let token_a = make_asset_token(1);
        let token_b = make_asset_token(2);
        let lp = make_address(10);
        let trader = make_address(20);

        setup_asset(&mut state, token_a, lp, Amount::from_raw(1_000_000));
        setup_asset(&mut state, token_b, lp, Amount::from_raw(2_000_000));

        create_pool(
            &mut state,
            &lp,
            token_a,
            token_b,
            Amount::from_raw(1_000_000),
            Amount::from_raw(2_000_000),
            0,
        )
        .unwrap();

        let mut rec = state.get_ownership(&token_a, &trader);
        rec.credit(Amount::from_raw(100_000));
        state.put_ownership(rec);

        let pool_id = compute_pool_id(&token_a, &token_b);

        // Set an impossibly high min_amount_out
        let err = execute_swap(
            &mut state,
            &trader,
            &pool_id,
            &token_a,
            Amount::from_raw(100_000),
            Amount::from_raw(999_999_999),
        );
        assert!(matches!(err, Err(DexError::SlippageExceeded { .. })));
    }

    #[test]
    fn test_swap_with_native() {
        let mut state = StateDB::new();
        let native = H256::ZERO;
        let token_b = make_asset_token(2);
        let lp = make_address(10);
        let trader = make_address(20);

        setup_asset(&mut state, token_b, lp, Amount::from_raw(2_000_000));
        state.add_balance(&lp, Amount::from_raw(1_000_000)).unwrap();

        create_pool(
            &mut state,
            &lp,
            native,
            token_b,
            Amount::from_raw(1_000_000),
            Amount::from_raw(2_000_000),
            0,
        )
        .unwrap();

        // Give trader native VTT
        state
            .add_balance(&trader, Amount::from_raw(100_000))
            .unwrap();

        // Pool ID: sorted(native, token_b)
        let pool_id = compute_pool_id(&native, &token_b);

        let amount_out = execute_swap(
            &mut state,
            &trader,
            &pool_id,
            &native,
            Amount::from_raw(100_000),
            Amount::ZERO,
        )
        .unwrap();

        assert!(amount_out.0 > 0);
        // Trader's native balance should be 0
        assert_eq!(state.get_balance(&trader).0, 0);
        // Trader received token_b
        let trader_b = state.get_ownership(&token_b, &trader);
        assert_eq!(trader_b.available, amount_out);
    }
}
