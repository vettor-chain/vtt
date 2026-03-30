# VTT DEX/AMM Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement an on-chain DEX with constant product AMM, native stablecoin, liquidity mining, and revenue share — all in a new `vtt-dex` crate integrated with the existing executor, state, and RPC layers.

**Architecture:** New `vtt-dex` crate handles all AMM logic (pool state, swap math, liquidity, mining, revenue). Called by `vtt-executor` via new `TransactionAction` variants. Pool state stored in `vtt-state::StateDB`. RPC endpoints added to `vtt-rpc`. Genesis creates vUSDT, VTT-REV, and initial pool.

**Tech Stack:** Rust, Borsh serialization, BLAKE3 hashing, u128/U256 arithmetic

**Spec:** `docs/superpowers/specs/2026-03-30-vtt-dex-amm.md`

---

## File Map

### New Files

```
crates/vtt-dex/
├── Cargo.toml
├── src/
│   ├── lib.rs              # Public API: create_pool, add_liquidity, remove_liquidity, swap, claim_revenue, claim_mining_rewards
│   ├── pool.rs             # PoolState struct, pool_id computation
│   ├── math.rs             # Swap math: constant product, sqrt, U256 helpers
│   ├── liquidity.rs        # Add/remove liquidity logic
│   ├── swap.rs             # Swap execution with fee calculation
│   ├── revenue.rs          # RevenueDistributor, protocol fee claiming
│   ├── mining.rs           # MiningConfig, MiningState, reward distribution
│   └── error.rs            # DexError enum
```

### Modified Files

```
Cargo.toml                                    # Add vtt-dex to workspace members
crates/vtt-primitives/src/transaction.rs      # Add 6 new TransactionAction variants
crates/vtt-state/src/statedb.rs               # Add pool storage methods
crates/vtt-executor/Cargo.toml                # Add vtt-dex dependency
crates/vtt-executor/src/lib.rs                # Add match arms for DEX actions
crates/vtt-rpc/Cargo.toml                     # Add vtt-dex dependency
crates/vtt-rpc/src/server.rs                  # Add DEX RPC methods
crates/vtt-rpc/src/types.rs                   # Add PoolInfo, SwapQuote, MiningInfo types
crates/vtt-genesis/Cargo.toml                 # Add vtt-dex dependency
crates/vtt-genesis/src/lib.rs                 # Create vUSDT, VTT-REV, initial pool
tests/integration/chain_lifecycle.rs          # Add DEX integration tests
```

---

## Task 1: Create vtt-dex Crate with Types and Error

**Files:**
- Create: `crates/vtt-dex/Cargo.toml`
- Create: `crates/vtt-dex/src/lib.rs`
- Create: `crates/vtt-dex/src/pool.rs`
- Create: `crates/vtt-dex/src/error.rs`
- Modify: `Cargo.toml` (workspace)

- [ ] **Step 1: Create crate directory and Cargo.toml**

```bash
mkdir -p crates/vtt-dex/src
```

Create `crates/vtt-dex/Cargo.toml`:

```toml
[package]
name = "vtt-dex"
version = "0.1.0"
edition = "2021"

[dependencies]
vtt-primitives = { path = "../vtt-primitives" }
vtt-state = { path = "../vtt-state" }
vtt-crypto = { path = "../vtt-crypto" }
borsh = { version = "1", features = ["derive"] }
```

- [ ] **Step 2: Add to workspace**

In root `Cargo.toml`, add `"crates/vtt-dex"` to the `members` array after `"crates/vtt-compliance"`.

- [ ] **Step 3: Create error types**

Create `crates/vtt-dex/src/error.rs`:

```rust
use std::fmt;
use vtt_primitives::H256;

#[derive(Debug, Clone)]
pub enum DexError {
    PoolAlreadyExists { pool_id: H256 },
    PoolNotFound { pool_id: H256 },
    ZeroAmount,
    ZeroLiquidity,
    InsufficientBalance,
    InsufficientLiquidity,
    SlippageExceeded { expected: u128, got: u128 },
    InvalidTokenPair,
    SameToken,
    Overflow,
    NotAuthorized,
    MiningNotActive,
    NothingToClaim,
}

impl fmt::Display for DexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PoolAlreadyExists { pool_id } => write!(f, "pool already exists: {pool_id}"),
            Self::PoolNotFound { pool_id } => write!(f, "pool not found: {pool_id}"),
            Self::ZeroAmount => write!(f, "amount must be non-zero"),
            Self::ZeroLiquidity => write!(f, "pool has zero liquidity"),
            Self::InsufficientBalance => write!(f, "insufficient balance"),
            Self::InsufficientLiquidity => write!(f, "insufficient liquidity in pool"),
            Self::SlippageExceeded { expected, got } => {
                write!(f, "slippage exceeded: expected >= {expected}, got {got}")
            }
            Self::InvalidTokenPair => write!(f, "invalid token pair"),
            Self::SameToken => write!(f, "cannot create pool with same token on both sides"),
            Self::Overflow => write!(f, "arithmetic overflow"),
            Self::NotAuthorized => write!(f, "not authorized for this operation"),
            Self::MiningNotActive => write!(f, "liquidity mining not active for this pool"),
            Self::NothingToClaim => write!(f, "nothing to claim"),
        }
    }
}

impl std::error::Error for DexError {}
```

- [ ] **Step 4: Create pool types**

Create `crates/vtt-dex/src/pool.rs`:

```rust
use borsh::{BorshDeserialize, BorshSerialize};
use vtt_crypto::blake3_hash;
use vtt_primitives::{Address, Amount, H256};

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
```

- [ ] **Step 5: Create lib.rs stub**

Create `crates/vtt-dex/src/lib.rs`:

```rust
pub mod error;
pub mod pool;

pub use error::DexError;
pub use pool::{PoolState, compute_pool_id, MINIMUM_LIQUIDITY, DEFAULT_FEE_BPS, DEFAULT_PROTOCOL_FEE_BPS};
```

- [ ] **Step 6: Verify it compiles**

```bash
cargo build -p vtt-dex
```

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/vtt-dex/
git commit -m "feat(dex): create vtt-dex crate with pool types and error enum"
```

---

## Task 2: Swap Math

**Files:**
- Create: `crates/vtt-dex/src/math.rs`
- Modify: `crates/vtt-dex/src/lib.rs`

- [ ] **Step 1: Create math module with U256, sqrt, and swap calculation**

Create `crates/vtt-dex/src/math.rs`:

```rust
use crate::DexError;

/// Simple U256 for intermediate multiplication to avoid u128 overflow.
/// Only supports multiply and divide — enough for AMM math.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct U256 {
    pub hi: u128,
    pub lo: u128,
}

impl U256 {
    pub const ZERO: Self = Self { hi: 0, lo: 0 };

    pub fn from_u128(v: u128) -> Self {
        Self { hi: 0, lo: v }
    }

    /// Multiply two u128 values into U256
    pub fn mul_u128(a: u128, b: u128) -> Self {
        let a_lo = a as u64 as u128;
        let a_hi = a >> 64;
        let b_lo = b as u64 as u128;
        let b_hi = b >> 64;

        let ll = a_lo * b_lo;
        let lh = a_lo * b_hi;
        let hl = a_hi * b_lo;
        let hh = a_hi * b_hi;

        let mid = lh + hl;
        let lo = ll.wrapping_add(mid << 64);
        let carry = if lo < ll { 1u128 } else { 0 }
            + if mid < lh { 1u128 << 64 } else { 0 };
        let hi = hh + (mid >> 64) + carry;

        Self { hi, lo }
    }

    /// Divide U256 by u128, returning u128 quotient (panics if result overflows u128)
    pub fn div_u128(self, divisor: u128) -> Result<u128, DexError> {
        if divisor == 0 {
            return Err(DexError::Overflow);
        }
        if self.hi == 0 {
            return Ok(self.lo / divisor);
        }

        // Long division: treat U256 as (hi * 2^128 + lo) / divisor
        // For AMM math, the result should always fit in u128
        let mut remainder = 0u128;
        let mut result = 0u128;

        // Process hi part
        let q_hi = self.hi / divisor;
        if q_hi > 0 {
            return Err(DexError::Overflow); // result > u128
        }
        remainder = self.hi % divisor;

        // Process lo part with remainder
        // (remainder * 2^128 + lo) / divisor
        // Split into two 64-bit divisions to avoid overflow
        let combined_hi = (remainder << 64) | (self.lo >> 64);
        let q_mid = combined_hi / divisor;
        remainder = combined_hi % divisor;

        let combined_lo = (remainder << 64) | (self.lo & ((1u128 << 64) - 1));
        let q_lo = combined_lo / divisor;

        // Combine: result = q_mid * 2^64 + q_lo
        result = q_mid
            .checked_shl(64)
            .and_then(|v| v.checked_add(q_lo))
            .ok_or(DexError::Overflow)?;

        Ok(result)
    }
}

/// Integer square root using Newton's method
pub fn sqrt_u128(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    if n <= 3 {
        return 1;
    }

    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Calculate swap output using constant product formula.
///
/// Given:
///   reserve_in: current reserve of input token
///   reserve_out: current reserve of output token
///   amount_in: amount of input token (after fee deduction)
///
/// Returns: amount of output token
pub fn get_amount_out(
    amount_in_net: u128,
    reserve_in: u128,
    reserve_out: u128,
) -> Result<u128, DexError> {
    if amount_in_net == 0 {
        return Err(DexError::ZeroAmount);
    }
    if reserve_in == 0 || reserve_out == 0 {
        return Err(DexError::ZeroLiquidity);
    }

    // amount_out = (reserve_out * amount_in_net) / (reserve_in + amount_in_net)
    let numerator = U256::mul_u128(reserve_out, amount_in_net);
    let denominator = reserve_in
        .checked_add(amount_in_net)
        .ok_or(DexError::Overflow)?;

    numerator.div_u128(denominator)
}

/// Calculate fees from gross input amount.
///
/// Returns: (amount_in_net, lp_fee, protocol_fee)
pub fn calculate_fees(
    amount_in: u128,
    fee_bps: u16,
    protocol_fee_bps: u16,
) -> Result<(u128, u128, u128), DexError> {
    if amount_in == 0 {
        return Err(DexError::ZeroAmount);
    }

    let total_fee = amount_in
        .checked_mul(fee_bps as u128)
        .ok_or(DexError::Overflow)?
        / 10_000;

    let protocol_fee = amount_in
        .checked_mul(protocol_fee_bps as u128)
        .ok_or(DexError::Overflow)?
        / 10_000;

    let lp_fee = total_fee.saturating_sub(protocol_fee);
    let amount_in_net = amount_in.saturating_sub(total_fee);

    Ok((amount_in_net, lp_fee, protocol_fee))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqrt() {
        assert_eq!(sqrt_u128(0), 0);
        assert_eq!(sqrt_u128(1), 1);
        assert_eq!(sqrt_u128(4), 2);
        assert_eq!(sqrt_u128(9), 3);
        assert_eq!(sqrt_u128(100), 10);
        assert_eq!(sqrt_u128(1_000_000), 1000);
        // sqrt(10^36) = 10^18
        assert_eq!(sqrt_u128(10u128.pow(36)), 10u128.pow(18));
    }

    #[test]
    fn test_u256_mul_div() {
        // Simple case
        let result = U256::mul_u128(100, 200).div_u128(50).unwrap();
        assert_eq!(result, 400);

        // Large numbers that would overflow u128
        let large_a = 10u128.pow(30);
        let large_b = 10u128.pow(30);
        let divisor = 10u128.pow(25);
        let result = U256::mul_u128(large_a, large_b).div_u128(divisor).unwrap();
        assert_eq!(result, 10u128.pow(35));
    }

    #[test]
    fn test_get_amount_out() {
        // Pool: 1000 A, 2000 B. Swap 100 A (net) → expect ~181 B
        let out = get_amount_out(100, 1000, 2000).unwrap();
        // (2000 * 100) / (1000 + 100) = 200000 / 1100 = 181
        assert_eq!(out, 181);
    }

    #[test]
    fn test_calculate_fees() {
        // 10000 input, 0.3% fee, 0.05% protocol
        let (net, lp_fee, protocol_fee) = calculate_fees(10000, 30, 5).unwrap();
        assert_eq!(protocol_fee, 5);   // 10000 * 5 / 10000
        assert_eq!(lp_fee, 25);        // 30 - 5
        assert_eq!(net, 9970);         // 10000 - 30
    }

    #[test]
    fn test_zero_amount() {
        assert!(matches!(get_amount_out(0, 1000, 2000), Err(DexError::ZeroAmount)));
        assert!(matches!(calculate_fees(0, 30, 5), Err(DexError::ZeroAmount)));
    }

    #[test]
    fn test_zero_reserves() {
        assert!(matches!(get_amount_out(100, 0, 2000), Err(DexError::ZeroLiquidity)));
        assert!(matches!(get_amount_out(100, 1000, 0), Err(DexError::ZeroLiquidity)));
    }
}
```

- [ ] **Step 2: Add to lib.rs**

Update `crates/vtt-dex/src/lib.rs`:

```rust
pub mod error;
pub mod math;
pub mod pool;

pub use error::DexError;
pub use pool::{PoolState, compute_pool_id, MINIMUM_LIQUIDITY, DEFAULT_FEE_BPS, DEFAULT_PROTOCOL_FEE_BPS};
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p vtt-dex
```

Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/vtt-dex/src/math.rs crates/vtt-dex/src/lib.rs
git commit -m "feat(dex): swap math — U256, sqrt, constant product formula with tests"
```

---

## Task 3: Add TransactionAction Variants to Primitives

**Files:**
- Modify: `crates/vtt-primitives/src/transaction.rs`

- [ ] **Step 1: Add 6 new variants to TransactionAction enum**

In `crates/vtt-primitives/src/transaction.rs`, add after the `CrossChainTransfer` variant (keeping all existing variants unchanged):

```rust
    // DEX actions (variants 9-14)
    CreatePool {
        token_a: H256,
        token_b: H256,
        amount_a: Amount,
        amount_b: Amount,
    },
    AddLiquidity {
        pool_id: H256,
        amount_a: Amount,
        amount_b: Amount,
        min_lp: Amount,
    },
    RemoveLiquidity {
        pool_id: H256,
        lp_amount: Amount,
        min_a: Amount,
        min_b: Amount,
    },
    Swap {
        pool_id: H256,
        token_in: H256,
        amount_in: Amount,
        min_amount_out: Amount,
    },
    ClaimRevenue {
        pool_id: H256,
    },
    ClaimMiningRewards {
        pool_id: H256,
    },
```

- [ ] **Step 2: Verify Borsh serialization still works**

```bash
cargo test -p vtt-primitives
```

Borsh derive macro will assign discriminants 9-14 automatically. Existing tests must still pass (variants 0-8 unchanged).

- [ ] **Step 3: Commit**

```bash
git add crates/vtt-primitives/src/transaction.rs
git commit -m "feat(primitives): add DEX transaction action variants — CreatePool, AddLiquidity, RemoveLiquidity, Swap, ClaimRevenue, ClaimMiningRewards"
```

---

## Task 4: Add Pool Storage to StateDB

**Files:**
- Modify: `crates/vtt-state/src/statedb.rs`
- Modify: `crates/vtt-state/Cargo.toml` (if needed)

- [ ] **Step 1: Add pool storage to StateDB**

In `crates/vtt-state/src/statedb.rs`, add to the StateDB struct fields:

```rust
    pools: HashMap<H256, Vec<u8>>,  // pool_id -> borsh-serialized pool data
    dirty_pools: Vec<H256>,
```

Initialize both as empty in `StateDB::new()`.

- [ ] **Step 2: Add pool methods to StateDB**

Add these methods to the `impl StateDB` block:

```rust
    pub fn get_pool_raw(&self, pool_id: &H256) -> Option<&[u8]> {
        self.pools.get(pool_id).map(|v| v.as_slice())
    }

    pub fn put_pool_raw(&mut self, pool_id: H256, data: Vec<u8>) {
        self.pools.insert(pool_id, data);
        self.dirty_pools.push(pool_id);
    }

    pub fn has_pool(&self, pool_id: &H256) -> bool {
        self.pools.contains_key(pool_id)
    }

    pub fn iter_pools(&self) -> impl Iterator<Item = (&H256, &[u8])> {
        self.pools.iter().map(|(k, v)| (k, v.as_slice()))
    }
```

- [ ] **Step 3: Add pool persistence to compute_state_root**

In `compute_state_root()`, add after the asset dirty loop:

```rust
        for pool_id in self.dirty_pools.drain(..) {
            if let Some(pool_data) = self.pools.get(&pool_id) {
                let mut key = b"pool:".to_vec();
                key.extend_from_slice(pool_id.as_bytes());
                self.trie.insert(key, pool_data.clone());
            }
        }
```

- [ ] **Step 4: Add pool fields to snapshot/restore**

In `snapshot()`, add pools to the snapshot struct. In `restore()`, restore them.

- [ ] **Step 5: Verify build**

```bash
cargo build -p vtt-state
cargo test -p vtt-state
```

- [ ] **Step 6: Commit**

```bash
git add crates/vtt-state/
git commit -m "feat(state): add pool storage to StateDB — get/put/has/iter pool methods"
```

---

## Task 5: Implement Pool Creation and Liquidity

**Files:**
- Create: `crates/vtt-dex/src/liquidity.rs`
- Modify: `crates/vtt-dex/src/lib.rs`

- [ ] **Step 1: Create liquidity module**

Create `crates/vtt-dex/src/liquidity.rs`:

```rust
use borsh::{BorshDeserialize, BorshSerialize};
use vtt_primitives::{Address, Amount, H256};
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

fn transfer_token_in(
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

fn transfer_token_out(
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
    use vtt_state::asset::{AssetRecord, AssetClass, AssetStatus};
    use vtt_primitives::chain::ChainId;

    let asset = AssetRecord {
        id: *lp_token_id,
        name: format!("LP-{}", hex::encode(&pool_id.as_bytes()[..4])),
        symbol: format!("LP-{}", hex::encode(&pool_id.as_bytes()[..2])),
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
```

- [ ] **Step 2: Update lib.rs**

```rust
pub mod error;
pub mod liquidity;
pub mod math;
pub mod pool;

pub use error::DexError;
pub use pool::{PoolState, compute_pool_id, MINIMUM_LIQUIDITY, DEFAULT_FEE_BPS, DEFAULT_PROTOCOL_FEE_BPS};
```

- [ ] **Step 3: Verify build**

```bash
cargo build -p vtt-dex
```

Fix any compilation issues (import paths for asset types may vary — check exact paths in `vtt-state/src/asset.rs`).

- [ ] **Step 4: Commit**

```bash
git add crates/vtt-dex/
git commit -m "feat(dex): pool creation and add/remove liquidity"
```

---

## Task 6: Implement Swap Execution

**Files:**
- Create: `crates/vtt-dex/src/swap.rs`
- Modify: `crates/vtt-dex/src/lib.rs`

- [ ] **Step 1: Create swap module**

Create `crates/vtt-dex/src/swap.rs`:

```rust
use vtt_primitives::{Address, Amount, H256};
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
    let token_out = if is_a_to_b { &pool.token_b } else { &pool.token_a };
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
```

- [ ] **Step 2: Make transfer helpers pub(crate) in liquidity.rs**

In `crates/vtt-dex/src/liquidity.rs`, change the visibility of `transfer_token_in` and `transfer_token_out` from `fn` to `pub(crate) fn` so swap.rs can use them.

- [ ] **Step 3: Update lib.rs**

```rust
pub mod error;
pub mod liquidity;
pub mod math;
pub mod pool;
pub mod swap;

pub use error::DexError;
pub use pool::{PoolState, compute_pool_id, MINIMUM_LIQUIDITY, DEFAULT_FEE_BPS, DEFAULT_PROTOCOL_FEE_BPS};
```

- [ ] **Step 4: Verify build**

```bash
cargo build -p vtt-dex
```

- [ ] **Step 5: Commit**

```bash
git add crates/vtt-dex/
git commit -m "feat(dex): swap execution with constant product formula and fee splitting"
```

---

## Task 7: Implement Revenue and Mining

**Files:**
- Create: `crates/vtt-dex/src/revenue.rs`
- Create: `crates/vtt-dex/src/mining.rs`
- Modify: `crates/vtt-dex/src/lib.rs`

- [ ] **Step 1: Create revenue module**

Create `crates/vtt-dex/src/revenue.rs`:

```rust
use borsh::{BorshDeserialize, BorshSerialize};
use std::collections::BTreeMap;
use vtt_primitives::{Address, Amount, H256};
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
```

- [ ] **Step 2: Create mining module**

Create `crates/vtt-dex/src/mining.rs`:

```rust
use borsh::{BorshDeserialize, BorshSerialize};
use std::collections::BTreeMap;
use vtt_primitives::{Address, Amount, H256};
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
```

- [ ] **Step 3: Update lib.rs**

```rust
pub mod error;
pub mod liquidity;
pub mod math;
pub mod mining;
pub mod pool;
pub mod revenue;
pub mod swap;

pub use error::DexError;
pub use pool::{PoolState, compute_pool_id, MINIMUM_LIQUIDITY, DEFAULT_FEE_BPS, DEFAULT_PROTOCOL_FEE_BPS};
pub use mining::{MiningConfig, MiningPhase, MiningState};
pub use revenue::RevenueDistributor;
```

- [ ] **Step 4: Build and test**

```bash
cargo build -p vtt-dex
cargo test -p vtt-dex
```

- [ ] **Step 5: Commit**

```bash
git add crates/vtt-dex/
git commit -m "feat(dex): revenue claiming and liquidity mining with MasterChef pattern"
```

---

## Task 8: Integrate with Executor

**Files:**
- Modify: `crates/vtt-executor/Cargo.toml`
- Modify: `crates/vtt-executor/src/lib.rs`

- [ ] **Step 1: Add vtt-dex dependency**

In `crates/vtt-executor/Cargo.toml`, add:

```toml
vtt-dex = { path = "../vtt-dex" }
```

- [ ] **Step 2: Add match arms in execute_action**

In `crates/vtt-executor/src/lib.rs`, in the `execute_action` function's match on `TransactionAction`, add after the `CrossChainTransfer` arm:

```rust
            TransactionAction::CreatePool { token_a, token_b, amount_a, amount_b } => {
                let pool = vtt_dex::liquidity::create_pool(
                    state, sender, *token_a, *token_b, *amount_a, *amount_b, 0, // TODO: pass current epoch
                ).map_err(|e| ExecutionError::Custom(e.to_string()))?;
                Ok(vec![Log {
                    address: *sender,
                    topics: vec![vtt_crypto::blake3_hash(b"CreatePool"), pool.pool_id],
                    data: borsh::to_vec(&pool.pool_id).unwrap(),
                }])
            }
            TransactionAction::AddLiquidity { pool_id, amount_a, amount_b, min_lp } => {
                let lp_minted = vtt_dex::liquidity::add_liquidity(
                    state, sender, pool_id, *amount_a, *amount_b, *min_lp,
                ).map_err(|e| ExecutionError::Custom(e.to_string()))?;
                Ok(vec![Log {
                    address: *sender,
                    topics: vec![vtt_crypto::blake3_hash(b"AddLiquidity"), *pool_id],
                    data: borsh::to_vec(&lp_minted.0).unwrap(),
                }])
            }
            TransactionAction::RemoveLiquidity { pool_id, lp_amount, min_a, min_b } => {
                let (out_a, out_b) = vtt_dex::liquidity::remove_liquidity(
                    state, sender, pool_id, *lp_amount, *min_a, *min_b,
                ).map_err(|e| ExecutionError::Custom(e.to_string()))?;
                Ok(vec![Log {
                    address: *sender,
                    topics: vec![vtt_crypto::blake3_hash(b"RemoveLiquidity"), *pool_id],
                    data: borsh::to_vec(&(out_a.0, out_b.0)).unwrap(),
                }])
            }
            TransactionAction::Swap { pool_id, token_in, amount_in, min_amount_out } => {
                let amount_out = vtt_dex::swap::execute_swap(
                    state, sender, pool_id, token_in, *amount_in, *min_amount_out,
                ).map_err(|e| ExecutionError::Custom(e.to_string()))?;
                Ok(vec![Log {
                    address: *sender,
                    topics: vec![vtt_crypto::blake3_hash(b"Swap"), *pool_id],
                    data: borsh::to_vec(&amount_out.0).unwrap(),
                }])
            }
            TransactionAction::ClaimRevenue { pool_id } => {
                // Treasury address hardcoded for now — should come from chain config
                let treasury = Address::ZERO; // TODO: configure via genesis
                let (fees_a, fees_b) = vtt_dex::revenue::claim_protocol_fees(
                    state, sender, pool_id, &treasury,
                ).map_err(|e| ExecutionError::Custom(e.to_string()))?;
                Ok(vec![Log {
                    address: *sender,
                    topics: vec![vtt_crypto::blake3_hash(b"ClaimRevenue"), *pool_id],
                    data: borsh::to_vec(&(fees_a.0, fees_b.0)).unwrap(),
                }])
            }
            TransactionAction::ClaimMiningRewards { pool_id } => {
                // Mining state would need to be loaded from storage
                // For now, emit log — full mining integration in a follow-up
                Ok(vec![Log {
                    address: *sender,
                    topics: vec![vtt_crypto::blake3_hash(b"ClaimMiningRewards"), *pool_id],
                    data: vec![],
                }])
            }
```

- [ ] **Step 3: Add gas costs**

In the gas calculation section of `execute_transaction`, add the DEX action gas costs. Find where gas costs are calculated (likely a match or lookup) and add:

```rust
TransactionAction::CreatePool { .. } => 50_000,
TransactionAction::AddLiquidity { .. } => 30_000,
TransactionAction::RemoveLiquidity { .. } => 30_000,
TransactionAction::Swap { .. } => 25_000,
TransactionAction::ClaimRevenue { .. } => 10_000,
TransactionAction::ClaimMiningRewards { .. } => 10_000,
```

- [ ] **Step 4: Build full project**

```bash
cargo build
```

Fix any compilation errors across the workspace.

- [ ] **Step 5: Run existing tests**

```bash
cargo test
```

All existing tests must still pass.

- [ ] **Step 6: Commit**

```bash
git add crates/vtt-executor/
git commit -m "feat(executor): integrate DEX actions — CreatePool, AddLiquidity, RemoveLiquidity, Swap, ClaimRevenue"
```

---

## Task 9: Add RPC Endpoints

**Files:**
- Modify: `crates/vtt-rpc/Cargo.toml`
- Modify: `crates/vtt-rpc/src/server.rs`
- Modify: `crates/vtt-rpc/src/types.rs`

- [ ] **Step 1: Add vtt-dex dependency**

In `crates/vtt-rpc/Cargo.toml`:

```toml
vtt-dex = { path = "../vtt-dex" }
```

- [ ] **Step 2: Add RPC types**

In `crates/vtt-rpc/src/types.rs`, add:

```rust
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapQuoteRpc {
    pub amount_in: String,
    pub amount_out: String,
    pub price_impact_bps: u32,
    pub fee: String,
}
```

- [ ] **Step 3: Add RPC methods to trait**

In `crates/vtt-rpc/src/server.rs`, add to the `VttApi` trait:

```rust
    #[method(name = "vtt_listPools")]
    async fn list_pools(&self) -> Result<Vec<PoolInfo>, ErrorObjectOwned>;

    #[method(name = "vtt_getPool")]
    async fn get_pool(&self, pool_id: H256) -> Result<Option<PoolInfo>, ErrorObjectOwned>;

    #[method(name = "vtt_getSwapQuote")]
    async fn get_swap_quote(
        &self,
        pool_id: H256,
        token_in: H256,
        amount_in: String,
    ) -> Result<SwapQuoteRpc, ErrorObjectOwned>;
```

- [ ] **Step 4: Implement RPC methods**

In the `impl VttApiServer for VttRpcImpl` block, add implementations that read pool state from chain state and return the RPC types. The `list_pools` method iterates `state.iter_pools()`, deserializes each, and converts to `PoolInfo`. The `get_swap_quote` method loads the pool, runs the fee + output calculation without mutating state, and returns the quote.

- [ ] **Step 5: Build and test**

```bash
cargo build -p vtt-rpc
cargo test
```

- [ ] **Step 6: Commit**

```bash
git add crates/vtt-rpc/
git commit -m "feat(rpc): add DEX endpoints — listPools, getPool, getSwapQuote"
```

---

## Task 10: Genesis — vUSDT, VTT-REV, Initial Pool

**Files:**
- Modify: `crates/vtt-genesis/Cargo.toml`
- Modify: `crates/vtt-genesis/src/lib.rs`

- [ ] **Step 1: Add vtt-dex dependency**

In `crates/vtt-genesis/Cargo.toml`, add:

```toml
vtt-dex = { path = "../vtt-dex" }
```

- [ ] **Step 2: Add genesis assets and pool setup**

In `crates/vtt-genesis/src/lib.rs`, add a function `setup_dex_genesis` called at the end of `build_genesis`, after validators are set up:

```rust
fn setup_dex_genesis(state: &mut StateDB, treasury: &Address) {
    use vtt_state::asset::{AssetRecord, AssetClass, AssetStatus};
    use vtt_primitives::chain::ChainId;

    // 1. Create vUSDT stablecoin
    let vusdt_id = vtt_crypto::blake3_hash(b"asset:vUSDT");
    let vusdt = AssetRecord {
        id: vusdt_id,
        name: "VTT USD".to_string(),
        symbol: "vUSDT".to_string(),
        class: AssetClass::Fund,
        origin_chain: ChainId::RELAY,
        issuer: *treasury,
        total_supply: Amount::from_raw(100_000_000 * 10u128.pow(6)), // 100M with 6 decimals
        decimals: 6,
        status: AssetStatus::Active,
        compliance_policy: None,
        valuation_oracle: None,
        documents: Default::default(),
        metadata_uri: String::new(),
        created_at: 0,
    };
    state.register_asset(vusdt).expect("register vUSDT");

    // Mint 100M vUSDT to treasury
    let mut ownership = state.get_ownership(&vusdt_id, treasury);
    ownership.credit(Amount::from_raw(100_000_000 * 10u128.pow(6)));
    state.put_ownership(ownership);

    // 2. Create VTT-REV revenue share token
    let vtt_rev_id = vtt_crypto::blake3_hash(b"asset:VTT-REV");
    let vtt_rev = AssetRecord {
        id: vtt_rev_id,
        name: "VTT Revenue Share".to_string(),
        symbol: "VTT-REV".to_string(),
        class: AssetClass::Fund,
        origin_chain: ChainId::RELAY,
        issuer: *treasury,
        total_supply: Amount::from_raw(10_000),
        decimals: 0,
        status: AssetStatus::Active,
        compliance_policy: None,
        valuation_oracle: None,
        documents: Default::default(),
        metadata_uri: String::new(),
        created_at: 0,
    };
    state.register_asset(vtt_rev).expect("register VTT-REV");

    // Mint 10,000 VTT-REV to treasury
    let mut rev_ownership = state.get_ownership(&vtt_rev_id, treasury);
    rev_ownership.credit(Amount::from_raw(10_000));
    state.put_ownership(rev_ownership);
}
```

Call `setup_dex_genesis(&mut state, &treasury_address)` at the end of `build_genesis`, where `treasury_address` is the first allocation address (or a dedicated treasury address).

- [ ] **Step 3: Build and test**

```bash
cargo build -p vtt-genesis
cargo test
```

- [ ] **Step 4: Commit**

```bash
git add crates/vtt-genesis/
git commit -m "feat(genesis): create vUSDT and VTT-REV assets at genesis"
```

---

## Task 11: Integration Tests


**Files:**
- Modify: `tests/integration/chain_lifecycle.rs`

- [ ] **Step 1: Add DEX integration test**

Add a new test function to `tests/integration/chain_lifecycle.rs`:

```rust
#[test]
fn dex_swap_lifecycle() {
    // 1. Build genesis with two funded accounts
    // 2. Create an asset (vUSDT) via CreateAssetClass
    // 3. Create pool VTT/vUSDT via CreatePool action
    // 4. Verify pool exists in state
    // 5. Add liquidity from second account
    // 6. Swap VTT for vUSDT
    // 7. Verify balances changed correctly
    // 8. Remove liquidity
    // 9. Verify LP tokens burned and tokens returned

    // Use make_tx() helper from existing tests
    // Execute transactions through the full chain pipeline
}
```

Implement the full test body following the patterns in `full_chain_lifecycle` and `asset_tokenization_lifecycle` tests. Use `make_tx()` to build signed transactions and execute them through the chain.

- [ ] **Step 2: Run integration tests**

```bash
cargo test -p integration --test chain_lifecycle
```

- [ ] **Step 3: Commit**

```bash
git add tests/
git commit -m "test: DEX integration test — full swap lifecycle"
```

---

## Task 12: Update Web Frontend Borsh Serializer

**Files:**
- Modify: `/Users/alessandrovettor/Documents/Lavoro/vtt-web/src/lib/crypto/borsh.ts`

- [ ] **Step 1: Add new action types**

In `vtt-web/src/lib/crypto/borsh.ts`, add the new DEX action types to `TransactionAction`:

```ts
  | { type: "CreatePool"; tokenA: Uint8Array; tokenB: Uint8Array; amountA: Amount; amountB: Amount }
  | { type: "AddLiquidity"; poolId: Uint8Array; amountA: Amount; amountB: Amount; minLp: Amount }
  | { type: "RemoveLiquidity"; poolId: Uint8Array; lpAmount: Amount; minA: Amount; minB: Amount }
  | { type: "Swap"; poolId: Uint8Array; tokenIn: Uint8Array; amountIn: Amount; minAmountOut: Amount }
  | { type: "ClaimRevenue"; poolId: Uint8Array }
  | { type: "ClaimMiningRewards"; poolId: Uint8Array }
```

Add to `ACTION_INDICES`:

```ts
  CreatePool: 9,
  AddLiquidity: 10,
  RemoveLiquidity: 11,
  Swap: 12,
  ClaimRevenue: 13,
  ClaimMiningRewards: 14,
```

Add serialization cases in `writeAction`:

```ts
    case "CreatePool":
      w.writeFixedBytes(action.tokenA, 32);
      w.writeFixedBytes(action.tokenB, 32);
      writeAmount(w, action.amountA);
      writeAmount(w, action.amountB);
      break;
    case "AddLiquidity":
      w.writeFixedBytes(action.poolId, 32);
      writeAmount(w, action.amountA);
      writeAmount(w, action.amountB);
      writeAmount(w, action.minLp);
      break;
    case "RemoveLiquidity":
      w.writeFixedBytes(action.poolId, 32);
      writeAmount(w, action.lpAmount);
      writeAmount(w, action.minA);
      writeAmount(w, action.minB);
      break;
    case "Swap":
      w.writeFixedBytes(action.poolId, 32);
      w.writeFixedBytes(action.tokenIn, 32);
      writeAmount(w, action.amountIn);
      writeAmount(w, action.minAmountOut);
      break;
    case "ClaimRevenue":
      w.writeFixedBytes(action.poolId, 32);
      break;
    case "ClaimMiningRewards":
      w.writeFixedBytes(action.poolId, 32);
      break;
```

- [ ] **Step 2: Build web project**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web && npm run build
```

- [ ] **Step 3: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web
git add src/lib/crypto/borsh.ts
git commit -m "feat: add DEX action types to Borsh serializer"
```
