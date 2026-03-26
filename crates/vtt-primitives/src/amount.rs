use std::fmt;
use std::ops::{Add, Sub};

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Fixed-point token amount with 18 decimal places.
/// Stored as u128 to avoid floating point entirely.
/// Max representable: ~340 * 10^18 VTT (more than enough).
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
)]
pub struct Amount(pub u128);

impl Amount {
    pub const ZERO: Self = Self(0);
    pub const DECIMALS: u32 = 18;
    pub const ONE_VTT: Self = Self(10u128.pow(Self::DECIMALS));

    /// Create an Amount from a whole number of VTT (e.g., 100 VTT).
    pub fn from_vtt(vtt: u64) -> Self {
        Self(vtt as u128 * 10u128.pow(Self::DECIMALS))
    }

    /// Create an Amount from the smallest unit (like wei in Ethereum).
    pub fn from_raw(raw: u128) -> Self {
        Self(raw)
    }

    /// Get the raw u128 value.
    pub fn raw(&self) -> u128 {
        self.0
    }

    /// Get the whole VTT part (truncating decimals).
    pub fn whole_vtt(&self) -> u64 {
        (self.0 / 10u128.pow(Self::DECIMALS)) as u64
    }

    /// Checked addition. Returns None on overflow.
    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        self.0.checked_add(rhs.0).map(Self)
    }

    /// Checked subtraction. Returns None on underflow.
    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        self.0.checked_sub(rhs.0).map(Self)
    }

    /// Checked multiplication. Returns None on overflow.
    pub fn checked_mul(self, rhs: u128) -> Option<Self> {
        self.0.checked_mul(rhs).map(Self)
    }

    /// Returns true if this amount is zero.
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

impl Add for Amount {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self(self.0.checked_add(rhs.0).expect("Amount overflow"))
    }
}

impl Sub for Amount {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self(self.0.checked_sub(rhs.0).expect("Amount underflow"))
    }
}

impl fmt::Debug for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Amount({})", self)
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / 10u128.pow(Self::DECIMALS);
        let frac = self.0 % 10u128.pow(Self::DECIMALS);
        if frac == 0 {
            write!(f, "{whole} VTT")
        } else {
            let frac_str = format!("{:018}", frac);
            let trimmed = frac_str.trim_end_matches('0');
            write!(f, "{whole}.{trimmed} VTT")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_from_vtt() {
        let a = Amount::from_vtt(100);
        assert_eq!(a.whole_vtt(), 100);
        assert_eq!(a.0, 100 * 10u128.pow(18));
    }

    #[test]
    fn amount_zero() {
        let a = Amount::ZERO;
        assert!(a.is_zero());
        assert_eq!(a.whole_vtt(), 0);
    }

    #[test]
    fn amount_one_vtt() {
        let a = Amount::ONE_VTT;
        assert_eq!(a.whole_vtt(), 1);
    }

    #[test]
    fn amount_addition() {
        let a = Amount::from_vtt(50);
        let b = Amount::from_vtt(30);
        let c = a + b;
        assert_eq!(c.whole_vtt(), 80);
    }

    #[test]
    fn amount_subtraction() {
        let a = Amount::from_vtt(50);
        let b = Amount::from_vtt(30);
        let c = a - b;
        assert_eq!(c.whole_vtt(), 20);
    }

    #[test]
    #[should_panic(expected = "underflow")]
    fn amount_underflow_panics() {
        let a = Amount::from_vtt(10);
        let b = Amount::from_vtt(20);
        let _ = a - b;
    }

    #[test]
    fn amount_checked_ops() {
        let a = Amount::from_vtt(10);
        let b = Amount::from_vtt(20);
        assert!(a.checked_sub(b).is_none());
        assert_eq!(a.checked_add(b), Some(Amount::from_vtt(30)));
    }

    #[test]
    fn amount_display_whole() {
        let a = Amount::from_vtt(42);
        assert_eq!(format!("{a}"), "42 VTT");
    }

    #[test]
    fn amount_display_fractional() {
        let a = Amount::from_raw(1_500_000_000_000_000_000); // 1.5 VTT
        assert_eq!(format!("{a}"), "1.5 VTT");
    }

    #[test]
    fn amount_borsh_roundtrip() {
        let a = Amount::from_vtt(1000);
        let bytes = borsh::to_vec(&a).unwrap();
        let a2 = Amount::try_from_slice(&bytes).unwrap();
        assert_eq!(a, a2);
    }

    #[test]
    fn amount_ordering() {
        let a = Amount::from_vtt(10);
        let b = Amount::from_vtt(20);
        assert!(a < b);
        assert!(b > a);
    }
}
