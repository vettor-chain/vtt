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
        let carry = if lo < ll { 1u128 } else { 0 } + if mid < lh { 1u128 << 64 } else { 0 };
        let hi = hh + (mid >> 64) + carry;

        Self { hi, lo }
    }

    /// Divide U256 by u128, returning u128 quotient (errors if result overflows u128)
    pub fn div_u128(self, divisor: u128) -> Result<u128, DexError> {
        if divisor == 0 {
            return Err(DexError::Overflow);
        }
        if self.hi == 0 {
            return Ok(self.lo / divisor);
        }
        // If hi >= divisor, quotient > 2^128 which doesn't fit in u128
        if self.hi >= divisor {
            return Err(DexError::Overflow);
        }

        // Bit-by-bit long division of the 256-bit number (hi:lo) by divisor.
        // After processing hi, rem = hi % divisor (hi < divisor, so quotient so far = 0).
        // Then process each bit of lo to build the result.
        let mut rem = 0u128;
        for i in (0..128).rev() {
            let bit = (self.hi >> i) & 1;
            let overflow = rem >> 127;
            rem = rem.wrapping_shl(1) | bit;
            if overflow != 0 || rem >= divisor {
                rem = rem.wrapping_sub(divisor);
            }
        }

        let mut result = 0u128;
        for i in (0..128).rev() {
            let bit = (self.lo >> i) & 1;
            let overflow = rem >> 127;
            rem = rem.wrapping_shl(1) | bit;
            if overflow != 0 || rem >= divisor {
                rem = rem.wrapping_sub(divisor);
                result |= 1u128 << i;
            }
        }

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
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Integer square root of U256, returning u128.
/// Uses Newton's method with u128 arithmetic by first estimating
/// from the highest non-zero bits.
pub fn sqrt_u256(n: U256) -> u128 {
    if n.hi == 0 {
        return sqrt_u128(n.lo);
    }

    // Initial estimate: sqrt(hi * 2^128 + lo) ≈ sqrt(hi) * 2^64
    // This gives us a good starting point for Newton's method
    let mut x = (sqrt_u128(n.hi) + 1) << 64;

    // Newton's method: x = (x + n/x) / 2
    // We need n/x which is U256/u128 = u128
    loop {
        let div = n.div_u128(x).unwrap_or(u128::MAX);
        let next = (x >> 1) + (div >> 1) + ((x & 1) + (div & 1)) / 2;
        if next >= x {
            break;
        }
        x = next;
    }

    // Verify: x*x <= n < (x+1)*(x+1)
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
        assert_eq!(protocol_fee, 5); // 10000 * 5 / 10000
        assert_eq!(lp_fee, 25); // 30 - 5
        assert_eq!(net, 9970); // 10000 - 30
    }

    #[test]
    fn test_zero_amount() {
        assert!(matches!(
            get_amount_out(0, 1000, 2000),
            Err(DexError::ZeroAmount)
        ));
        assert!(matches!(
            calculate_fees(0, 30, 5),
            Err(DexError::ZeroAmount)
        ));
    }

    #[test]
    fn test_zero_reserves() {
        assert!(matches!(
            get_amount_out(100, 0, 2000),
            Err(DexError::ZeroLiquidity)
        ));
        assert!(matches!(
            get_amount_out(100, 1000, 0),
            Err(DexError::ZeroLiquidity)
        ));
    }
}
