# VTT DEX/AMM Module — Design Spec

## Overview

On-chain DEX with Automated Market Maker (constant product x*y=k) for the VTT blockchain. Includes a native stablecoin (vUSDT), liquidity mining incentives, and a revenue share token (VTT-REV). All implemented as a new `vtt-dex` crate in the workspace.

## Goals

- Enable token swapping on VTT chain without external dependencies
- Bootstrap liquidity through mining rewards (no capital required from team)
- Create first tokenized asset use case (revenue share) to demonstrate RWA capabilities
- Generate protocol revenue from swap fees

## Architecture

### New Crate: `vtt-dex`

Lives in `crates/vtt-dex/`. Consumed by `vtt-executor` when processing DEX transaction actions.

**Dependencies:**
- `vtt-primitives` — Amount, Address, H256, ChainId, Epoch
- `vtt-state` — read/write asset balances, account balances

**Does NOT depend on:** vtt-vm, vtt-network, vtt-consensus, vtt-rpc

**Pattern:** Same as vtt-consensus for governance, vtt-compliance for KYC — a domain module with a clean interface called by the executor.

### New Transaction Actions

Added to `TransactionAction` enum in `vtt-primitives`:

```rust
CreatePool {
    token_a: H256,        // H256::ZERO = VTT native
    token_b: H256,        // asset ID
    amount_a: Amount,     // initial liquidity (required)
    amount_b: Amount,     // initial liquidity (required)
}

AddLiquidity {
    pool_id: H256,
    amount_a: Amount,
    amount_b: Amount,
    min_lp: Amount,       // slippage protection
}

RemoveLiquidity {
    pool_id: H256,
    lp_amount: Amount,
    min_a: Amount,        // slippage protection
    min_b: Amount,        // slippage protection
}

Swap {
    pool_id: H256,
    token_in: H256,       // which token you're giving
    amount_in: Amount,
    min_amount_out: Amount, // slippage protection
}

ClaimRevenue {
    pool_id: H256,
}

ClaimMiningRewards {
    pool_id: H256,
}
```

### Gas Costs

| Action | Gas Cost |
|--------|----------|
| CreatePool | 50,000 |
| AddLiquidity | 30,000 |
| RemoveLiquidity | 30,000 |
| Swap | 25,000 |
| ClaimRevenue | 10,000 |
| ClaimMiningRewards | 10,000 |

## Pool State

Each pool is identified by `pool_id: H256` — deterministic hash of `sorted(token_a, token_b)`. This guarantees exactly one pool per pair.

```rust
pub struct PoolState {
    pub pool_id: H256,
    pub token_a: H256,           // H256::ZERO = VTT native
    pub token_b: H256,
    pub reserve_a: Amount,
    pub reserve_b: Amount,
    pub lp_token_id: H256,       // asset ID of LP token (auto-created)
    pub lp_total_supply: Amount,
    pub fee_bps: u16,            // 30 = 0.3%
    pub protocol_fee_bps: u16,   // 5 = 0.05%
    pub protocol_fees_a: Amount, // accumulated protocol fees token A
    pub protocol_fees_b: Amount, // accumulated protocol fees token B
    pub creator: Address,
    pub created_at_epoch: Epoch,
}
```

**VTT native representation:** `H256::ZERO` is not an asset — it's the account's native VTT balance. The crate must handle both cases:
- If `token == H256::ZERO` → transfer via account balance (debit/credit)
- If `token != H256::ZERO` → transfer via asset balance (existing asset system)

**LP Token:** Created automatically via `CreateAssetClass` when pool is created. Name: `LP-{SYMBOL_A}/{SYMBOL_B}`, symbol: `LP-{A}/{B}`. LP tokens are regular on-chain assets — transferable, visible in explorer, usable as collateral in future.

## Swap Math

Constant product formula: `x * y = k`

For a swap of `amount_in` of token A → token B:

1. `fee = amount_in * 30 / 10000` (0.3%)
2. `protocol_fee = amount_in * 5 / 10000` (0.05% — subset of total fee)
3. `lp_fee = fee - protocol_fee` (0.25%)
4. `amount_in_net = amount_in - fee`
5. `amount_out = (reserve_b * amount_in_net) / (reserve_a + amount_in_net)`
6. If `amount_out < min_amount_out` → transaction fails (slippage exceeded)

**Arithmetic:** All calculations in `u128`. For intermediate product `reserve_b * amount_in_net` that may overflow u128, use `U256` (implement as pair of u128 or use `uint` crate).

**Fee distribution:**
- 83% to liquidity providers (0.25% — stays in pool, increases reserves)
- 17% to protocol treasury (0.05% — accumulated in `protocol_fees_a/b`, claimed via `ClaimRevenue`)

## Liquidity Management

### CreatePool

1. Verify both token amounts > 0
2. Verify pool doesn't already exist for this pair
3. Compute `pool_id = blake3(sorted(token_a, token_b))`
4. Transfer `amount_a` and `amount_b` from caller to pool
5. Create LP token asset via `CreateAssetClass`
6. Mint initial LP tokens: `lp_minted = sqrt(amount_a * amount_b)`
7. Burn first 1000 LP tokens (send to zero address) — prevents first-depositor manipulation
8. Give remaining LP tokens to caller
9. Store `PoolState`

### AddLiquidity

1. Load pool state
2. Calculate optimal amounts maintaining current ratio: `amount_b_optimal = amount_a * reserve_b / reserve_a`
3. If caller provides more of one token than needed, use optimal amount (refund excess)
4. Mint LP tokens proportional to contribution: `lp_minted = min(amount_a * lp_supply / reserve_a, amount_b * lp_supply / reserve_b)`
5. If `lp_minted < min_lp` → fail (slippage)
6. Transfer tokens from caller to pool, mint LP tokens to caller
7. Update reserves

### RemoveLiquidity

1. Load pool state
2. Calculate proportional share: `amount_a = lp_amount * reserve_a / lp_supply`, same for B
3. If `amount_a < min_a` or `amount_b < min_b` → fail (slippage)
4. Burn LP tokens from caller
5. Transfer tokens from pool to caller
6. Update reserves

## Revenue System

### Protocol Fee Collection

Every swap accumulates protocol fees in `protocol_fees_a` and `protocol_fees_b` fields of the pool.

### ClaimRevenue

- Callable by treasury address only (configured at genesis or via governance)
- Transfers accumulated `protocol_fees_a` and `protocol_fees_b` to treasury
- Resets both accumulators to zero

### VTT-REV Token

A separate mechanism on top of protocol fees:

- Asset: `VTT-REV`, supply 10,000, decimals 0
- Each token = 0.01% of protocol fee revenue
- Distribution: holder calls `ClaimRevenue` — the module checks if caller is treasury (transfers pool fees) or VTT-REV holder (transfers proportional share of accumulated fees)
- The DEX module tracks: total fees collected since last distribution, and each holder's claimed amount
- Formula: `claimable = (holder_balance / total_supply) * unclaimed_fees - already_claimed`

Implementation: the `vtt-dex` crate maintains a `RevenueDistributor` struct:

```rust
pub struct RevenueDistributor {
    pub revenue_token_id: H256,       // VTT-REV asset ID
    pub total_accumulated_a: Amount,  // lifetime protocol fees token A
    pub total_accumulated_b: Amount,  // lifetime protocol fees token B
    pub claims: BTreeMap<Address, (Amount, Amount)>, // per-address claimed amounts
}
```

When a holder claims:
1. Read their VTT-REV balance from asset state
2. Calculate share: `share = balance * total_accumulated / revenue_token_supply`
3. Subtract already claimed: `claimable = share - claims[address]`
4. Transfer claimable to holder
5. Update claims map

## Liquidity Mining

### Configuration

```rust
pub struct MiningConfig {
    pub pool_id: H256,
    pub total_budget: Amount,        // 50M VTT
    pub source: Address,             // Ecosystem allocation account
    pub phases: Vec<MiningPhase>,
}

pub struct MiningPhase {
    pub duration_epochs: u64,
    pub reward_per_epoch: Amount,
}
```

### Default Configuration (VTT/vUSDT pool)

| Phase | Duration | Reward/Epoch | Monthly | Total |
|-------|----------|-------------|---------|-------|
| 1 (Month 1-3) | 2,160 epochs | ~3,703 VTT | ~8M VTT | 24M VTT |
| 2 (Month 4-6) | 2,160 epochs | ~2,314 VTT | ~5M VTT | 15M VTT |
| 3 (Month 7-12) | 4,320 epochs | ~1,157 VTT | ~2.5M VTT | 11M VTT |
| **Total** | **8,640 epochs (12 months)** | | | **50M VTT** |

Note: 24 epochs/day (1 epoch = 1 hour), 720 epochs/month.

### Distribution

Per-epoch allocation is distributed proportionally to LP token holders. The module tracks:

```rust
pub struct MiningState {
    pub config: MiningConfig,
    pub start_epoch: Epoch,
    pub reward_per_lp_accumulated: U256, // scaled by 1e18 for precision
    pub last_update_epoch: Epoch,
    pub claims: BTreeMap<Address, MiningClaim>,
}

pub struct MiningClaim {
    pub lp_balance_at_last_claim: Amount,
    pub reward_debt: Amount,          // already accounted rewards
    pub unclaimed: Amount,            // pending claim
}
```

When epoch advances, `reward_per_lp_accumulated` increases by `reward_this_epoch * 1e18 / lp_total_supply`.

When user claims (`ClaimMiningRewards`):
1. Update global `reward_per_lp_accumulated` to current epoch
2. Calculate: `pending = user_lp_balance * reward_per_lp_accumulated / 1e18 - reward_debt`
3. Transfer pending VTT from source account to user
4. Update user's reward_debt

This is the standard "MasterChef" pattern used by SushiSwap and most DeFi protocols.

## Predefined Assets

Created at genesis or via admin transactions post-genesis:

### vUSDT — Native Stablecoin

- **Name:** VTT USD
- **Symbol:** vUSDT
- **Decimals:** 6
- **Total Supply:** 100,000,000 (100M)
- **Minted to:** Treasury account
- **Purpose:** Trading pair for DEX, launchpad payments
- **Backing:** Nominal/fiduciary. Real peg comes with bridge (future)

### VTT-REV — Revenue Share Token

- **Name:** VTT Revenue Share
- **Symbol:** VTT-REV
- **Decimals:** 0
- **Total Supply:** 10,000
- **Minted to:** Treasury account
- **Purpose:** Each token = 0.01% of protocol swap fees
- **Distribution:** Sold via launchpad or distributed to early participants

### Initial Pool: VTT/vUSDT

- **Created by:** Treasury account
- **Initial liquidity:** 10,000,000 VTT + 1,000,000 vUSDT
- **Implied price:** 1 VTT = 0.10 vUSDT
- **Fee:** 0.3% (30 bps)
- **Protocol fee:** 0.05% (5 bps)
- **Liquidity mining:** Enabled with 50M VTT budget over 12 months

## RPC Endpoints

New methods for the web frontend (added to `vtt-rpc`):

### Implemented

```
vtt_getPool(pool_id: H256) → PoolInfo | null
vtt_listPools() → Vec<PoolInfo>
vtt_getSwapQuote(pool_id: H256, amount_in: String, a_to_b: bool) → SwapQuoteRpc
vtt_getTokenPrice(token_id: H256) → TokenPriceRpc | null
vtt_getPoolPrices() → Vec<PoolPriceRpc>
```

### Not Implemented

The following endpoints from the original spec are not yet implemented:

```
vtt_getMiningInfo(pool_id: H256) → MiningInfo
vtt_getMiningRewards(pool_id: H256, address: Address) → Amount
vtt_getRevenueInfo(pool_id: H256) → RevenueInfo
vtt_getRevenueClaimable(address: Address) → RevenueClaimable
```

### DEX Pause Mechanism

The DEX can be paused and unpaused via on-chain governance proposals (action types `dex_pause` / `dex_unpause`). When paused, all pool operations (create, add/remove liquidity, swap) are rejected with `DexPaused` error. The pause state is stored in `StateDB` under the `dex:paused` chain metadata key.

## Integration with Executor

In `vtt-executor`, the `execute_transaction` function matches on action type. Add new match arms:

```rust
TransactionAction::CreatePool { .. } => dex::create_pool(state, tx),
TransactionAction::AddLiquidity { .. } => dex::add_liquidity(state, tx),
TransactionAction::RemoveLiquidity { .. } => dex::remove_liquidity(state, tx),
TransactionAction::Swap { .. } => dex::swap(state, tx),
TransactionAction::ClaimRevenue { .. } => dex::claim_revenue(state, tx),
TransactionAction::ClaimMiningRewards { .. } => dex::claim_mining_rewards(state, tx),
```

Gas costs are deducted by the executor before calling into `vtt-dex`.

## Integration with Borsh Serialization

Add new action variant indices to the Borsh enum:

```
CreatePool = 9
AddLiquidity = 10
RemoveLiquidity = 11
Swap = 12
ClaimRevenue = 13
ClaimMiningRewards = 14
```

These must also be added to the web frontend's `borsh.ts` serializer.

## Testing Strategy

Unit tests in `vtt-dex`:
- Pool creation (happy path, duplicate pool, zero amounts)
- Swap math (correct output, fee calculation, slippage failure)
- Add/remove liquidity (proportional minting/burning, slippage)
- First deposit burn (1000 LP tokens destroyed)
- Revenue accumulation and claiming
- Mining rewards calculation across epochs
- Edge cases: empty pool, single-sided liquidity, overflow protection

Integration tests in `tests/`:
- Full flow: create pool → add liquidity → swap → remove liquidity
- Mining rewards: provide liquidity → advance epochs → claim rewards
- Revenue share: swap generates fees → claim revenue → verify distribution
- Multiple users interacting with same pool
