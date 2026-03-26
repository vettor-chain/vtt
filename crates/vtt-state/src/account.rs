use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, Timestamp, H256};

/// The state of a single account in the VTT blockchain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct AccountState {
    /// Sequential nonce for replay protection.
    pub nonce: u64,
    /// VTT balance.
    pub balance: Amount,
    /// For contract accounts: hash of the deployed contract code.
    pub code_hash: Option<H256>,
    /// Root hash of the account's private storage trie (for contracts).
    pub storage_root: H256,
    /// Staking information (if this account is a validator or delegator).
    pub staking: Option<StakingState>,
}

impl Default for AccountState {
    fn default() -> Self {
        Self {
            nonce: 0,
            balance: Amount::ZERO,
            code_hash: None,
            storage_root: H256::ZERO,
            staking: None,
        }
    }
}

impl AccountState {
    /// Create a new account with an initial balance.
    pub fn with_balance(balance: Amount) -> Self {
        Self {
            balance,
            ..Default::default()
        }
    }

    /// Returns true if this is an empty/uninitialized account.
    pub fn is_empty(&self) -> bool {
        self.nonce == 0 && self.balance.is_zero() && self.code_hash.is_none()
    }

    /// Returns true if this is a contract account.
    pub fn is_contract(&self) -> bool {
        self.code_hash.is_some()
    }
}

/// Staking state for validators and delegators.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct StakingState {
    /// Total VTT staked to this validator (self + delegated).
    pub total_stake: Amount,
    /// Self-bonded amount (only for validators).
    pub self_stake: Amount,
    /// Commission rate in basis points (e.g., 500 = 5%).
    pub commission_bps: u16,
    /// Whether this validator is currently in the active set.
    pub active: bool,
    /// Delegations received: (delegator_address, amount).
    pub delegations: Vec<Delegation>,
    /// Pending unbonding entries.
    pub unbonding: Vec<UnbondingEntry>,
}

impl Default for StakingState {
    fn default() -> Self {
        Self {
            total_stake: Amount::ZERO,
            self_stake: Amount::ZERO,
            commission_bps: 0,
            active: false,
            delegations: Vec::new(),
            unbonding: Vec::new(),
        }
    }
}

/// A delegation from a delegator to a validator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Delegation {
    pub delegator: Address,
    pub amount: Amount,
}

/// A pending unbonding entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct UnbondingEntry {
    pub amount: Amount,
    /// Timestamp (ms) when unbonding completes.
    pub completion_time: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_account_is_empty() {
        let acc = AccountState::default();
        assert!(acc.is_empty());
        assert!(!acc.is_contract());
    }

    #[test]
    fn account_with_balance() {
        let acc = AccountState::with_balance(Amount::from_vtt(100));
        assert!(!acc.is_empty());
        assert_eq!(acc.balance, Amount::from_vtt(100));
        assert_eq!(acc.nonce, 0);
    }

    #[test]
    fn account_borsh_roundtrip() {
        let acc = AccountState {
            nonce: 5,
            balance: Amount::from_vtt(1000),
            code_hash: Some(H256::from([0xAB; 32])),
            storage_root: H256::from([0xCD; 32]),
            staking: Some(StakingState {
                total_stake: Amount::from_vtt(200_000),
                self_stake: Amount::from_vtt(100_000),
                commission_bps: 500,
                active: true,
                delegations: vec![Delegation {
                    delegator: Address::from([0x01; 20]),
                    amount: Amount::from_vtt(100_000),
                }],
                unbonding: vec![],
            }),
        };

        let bytes = borsh::to_vec(&acc).unwrap();
        let acc2 = AccountState::try_from_slice(&bytes).unwrap();
        assert_eq!(acc, acc2);
    }

    #[test]
    fn contract_account() {
        let acc = AccountState {
            code_hash: Some(H256::from([0xFF; 32])),
            ..Default::default()
        };
        assert!(acc.is_contract());
    }
}
