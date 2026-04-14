use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::amount::Amount;
use crate::asset_governance::AssetProposalAction;
use crate::{Address, ChainId, PublicKey, Signature, Vote, H256};

/// The payload of a transaction (everything that gets signed).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct TransactionPayload {
    /// Source chain.
    pub chain_id: ChainId,
    /// Sender's sequential nonce (replay protection).
    pub nonce: u64,
    /// Gas price in VTT (Amount per gas unit).
    pub gas_price: Amount,
    /// Maximum gas this transaction can consume.
    pub gas_limit: u64,
    /// The actual operation.
    pub action: TransactionAction,
}

/// All possible transaction types in VTT.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum TransactionAction {
    /// Simple VTT transfer.
    Transfer {
        to: Address,
        amount: Amount,
    },

    /// Deploy a smart contract.
    DeployContract {
        code: Vec<u8>,
        init_data: Vec<u8>,
    },

    /// Call a deployed contract.
    CallContract {
        contract: Address,
        method: String,
        args: Vec<u8>,
        value: Amount,
    },

    /// Stake VTT to a validator (or self-stake as a validator).
    Stake {
        validator: Address,
        amount: Amount,
    },

    /// Unstake VTT (begins unbonding period).
    Unstake {
        validator: Address,
        amount: Amount,
    },

    /// Cast governance vote.
    GovernanceVote {
        proposal_id: H256,
        vote: Vote,
    },

    /// Create a new asset class on this chain (RWA native).
    CreateAssetClass {
        name: String,
        symbol: String,
        metadata_uri: String,
        total_supply: Amount,
    },

    /// Transfer tokenized asset (RWA native).
    AssetTransfer {
        asset_id: H256,
        to: Address,
        amount: Amount,
    },

    /// Cross-chain transfer of VTT or assets.
    CrossChainTransfer {
        destination_chain: ChainId,
        to: Address,
        payload: CrossChainPayload,
    },

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

    /// Distribute revenue to all holders of an asset, proportional to their holdings.
    DistributeRevenue {
        asset_id: H256,
        /// Total VTT amount to distribute (taken from sender's balance).
        total_amount: Amount,
    },

    /// Propose an action on an asset (only token holders can propose).
    ProposeAssetAction {
        asset_id: H256,
        action: AssetProposalAction,
        description: String,
    },

    /// Vote on an asset proposal (weight = token holdings).
    VoteAssetProposal {
        proposal_id: H256,
        vote: Vote,
    },

    /// Finalize an asset proposal after the voting period ends.
    FinalizeAssetProposal {
        proposal_id: H256,
    },

    /// Bridge withdraw: burn tokens on VTT chain for release on external chain.
    /// For native VTT: burns VTT, to be minted as wVTT on Ethereum.
    /// For assets (e.g. vUSDT): burns asset tokens, to be released as real tokens on Ethereum.
    BridgeWithdraw {
        /// H256::ZERO for native VTT, or asset ID for tokens like vUSDT
        token: H256,
        /// Amount to withdraw (burned on VTT chain)
        amount: Amount,
        /// Destination chain identifier (e.g. 1 for Ethereum mainnet, 11155111 for Sepolia)
        destination_chain: u32,
        /// Destination address on the external chain (20 bytes for EVM)
        destination_address: Address,
    },

    /// Create a governance proposal (requires staked VTT).
    GovernancePropose {
        /// Human-readable description of the proposal.
        description: String,
        /// Type of proposal: "parameter_change", "treasury_spend", "signal", "dex_pause", "dex_unpause".
        action_type: String,
        /// Parameter key (for parameter_change proposals).
        param_key: Option<String>,
        /// Parameter value (for parameter_change proposals).
        param_value: Option<String>,
        /// Recipient address (for treasury_spend proposals).
        recipient: Option<Address>,
        /// Amount (for treasury_spend proposals).
        amount: Option<Amount>,
    },
}

/// Payload for cross-chain transfers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum CrossChainPayload {
    /// Transfer VTT from one chain to another.
    VttTransfer { amount: Amount },
    /// Transfer a tokenized asset cross-chain.
    AssetTransfer { asset_id: H256, amount: Amount },
    /// Arbitrary contract call on destination chain.
    ContractCall {
        contract: Address,
        method: String,
        args: Vec<u8>,
        value: Amount,
    },
}

/// A signed transaction ready for broadcast.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct SignedTransaction {
    pub payload: TransactionPayload,
    pub signature: Signature,
    pub public_key: PublicKey,
}

impl SignedTransaction {
    /// Get the serialized payload bytes for signing/verification.
    pub fn payload_bytes(&self) -> Vec<u8> {
        borsh::to_vec(&self.payload).expect("payload serialization should not fail")
    }

    /// Get the sender address (derived externally via vtt-crypto).
    /// This is a placeholder — actual derivation requires BLAKE3 from vtt-crypto.
    pub fn sender_public_key(&self) -> &PublicKey {
        &self.public_key
    }
}

/// Transaction receipt after execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct TransactionReceipt {
    /// Hash of the transaction.
    pub tx_hash: H256,
    /// Whether execution succeeded.
    pub success: bool,
    /// Gas actually consumed.
    pub gas_used: u64,
    /// Logs emitted during execution.
    pub logs: Vec<Log>,
}

/// A log entry emitted during transaction execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Log {
    /// Contract address that emitted the log.
    pub address: Address,
    /// Indexed topics for filtering.
    pub topics: Vec<H256>,
    /// Raw log data.
    pub data: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_transfer_payload() -> TransactionPayload {
        TransactionPayload {
            chain_id: ChainId::RELAY,
            nonce: 0,
            gas_price: Amount::from_raw(1_000_000_000), // 1 gwei equivalent
            gas_limit: 21_000,
            action: TransactionAction::Transfer {
                to: Address::from([0x01; 20]),
                amount: Amount::from_vtt(100),
            },
        }
    }

    #[test]
    fn transaction_payload_borsh_roundtrip() {
        let payload = test_transfer_payload();
        let bytes = borsh::to_vec(&payload).unwrap();
        let payload2 = TransactionPayload::try_from_slice(&bytes).unwrap();
        assert_eq!(payload, payload2);
    }

    #[test]
    fn signed_transaction_borsh_roundtrip() {
        let tx = SignedTransaction {
            payload: test_transfer_payload(),
            signature: Signature::ZERO,
            public_key: PublicKey::from([0xAA; 32]),
        };
        let bytes = borsh::to_vec(&tx).unwrap();
        let tx2 = SignedTransaction::try_from_slice(&bytes).unwrap();
        assert_eq!(tx, tx2);
    }

    #[test]
    fn cross_chain_payload_serialization() {
        let payload = CrossChainPayload::VttTransfer {
            amount: Amount::from_vtt(50),
        };
        let bytes = borsh::to_vec(&payload).unwrap();
        let payload2 = CrossChainPayload::try_from_slice(&bytes).unwrap();
        assert_eq!(payload, payload2);
    }

    #[test]
    fn all_action_variants_serialize() {
        let actions = vec![
            TransactionAction::Transfer {
                to: Address::ZERO,
                amount: Amount::from_vtt(1),
            },
            TransactionAction::DeployContract {
                code: vec![0x00, 0x61, 0x73, 0x6D],
                init_data: vec![],
            },
            TransactionAction::CallContract {
                contract: Address::ZERO,
                method: "transfer".to_string(),
                args: vec![1, 2, 3],
                value: Amount::ZERO,
            },
            TransactionAction::Stake {
                validator: Address::ZERO,
                amount: Amount::from_vtt(100_000),
            },
            TransactionAction::Unstake {
                validator: Address::ZERO,
                amount: Amount::from_vtt(50_000),
            },
            TransactionAction::GovernanceVote {
                proposal_id: H256::ZERO,
                vote: Vote::Yes,
            },
            TransactionAction::CreateAssetClass {
                name: "Real Estate Fund".to_string(),
                symbol: "REF".to_string(),
                metadata_uri: "ipfs://Qm...".to_string(),
                total_supply: Amount::from_vtt(1_000_000),
            },
            TransactionAction::AssetTransfer {
                asset_id: H256::ZERO,
                to: Address::ZERO,
                amount: Amount::from_vtt(100),
            },
            TransactionAction::CrossChainTransfer {
                destination_chain: ChainId::new(1),
                to: Address::ZERO,
                payload: CrossChainPayload::VttTransfer {
                    amount: Amount::from_vtt(10),
                },
            },
            TransactionAction::DistributeRevenue {
                asset_id: H256::ZERO,
                total_amount: Amount::from_vtt(1_000),
            },
            TransactionAction::ProposeAssetAction {
                asset_id: H256::ZERO,
                action: crate::asset_governance::AssetProposalAction::Signal {
                    description: "test proposal".to_string(),
                },
                description: "test".to_string(),
            },
            TransactionAction::VoteAssetProposal {
                proposal_id: H256::ZERO,
                vote: Vote::Yes,
            },
            TransactionAction::FinalizeAssetProposal {
                proposal_id: H256::ZERO,
            },
            TransactionAction::BridgeWithdraw {
                token: H256::ZERO,
                amount: Amount::from_vtt(100),
                destination_chain: 1,
                destination_address: Address::from([0xAA; 20]),
            },
            TransactionAction::GovernancePropose {
                description: "Reduce block time".to_string(),
                action_type: "parameter_change".to_string(),
                param_key: Some("block_time_ms".to_string()),
                param_value: Some("2000".to_string()),
                recipient: None,
                amount: None,
            },
        ];

        for action in actions {
            let bytes = borsh::to_vec(&action).unwrap();
            let action2 = TransactionAction::try_from_slice(&bytes).unwrap();
            assert_eq!(action, action2);
        }
    }

    #[test]
    fn receipt_serialization() {
        let receipt = TransactionReceipt {
            tx_hash: H256::ZERO,
            success: true,
            gas_used: 21000,
            logs: vec![Log {
                address: Address::ZERO,
                topics: vec![H256::ZERO],
                data: vec![1, 2, 3],
            }],
        };
        let bytes = borsh::to_vec(&receipt).unwrap();
        let receipt2 = TransactionReceipt::try_from_slice(&bytes).unwrap();
        assert_eq!(receipt, receipt2);
    }
}
