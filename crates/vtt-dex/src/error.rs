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
    DexPaused,
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
            Self::DexPaused => write!(f, "DEX is paused"),
        }
    }
}

impl std::error::Error for DexError {}
