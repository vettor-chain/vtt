#![allow(clippy::too_many_arguments)]

use borsh::BorshDeserialize;
use thiserror::Error;
use tracing::debug;

use vtt_consensus::governance::{Proposal, ProposalAction};
use vtt_consensus::rewards::{
    calculate_epoch_reward, split_block_reward, split_gas_fees, split_producer_reward,
};
use vtt_crypto::{blake3_hash, verify};
use vtt_primitives::amount::Amount;
use vtt_primitives::asset_governance::{
    AssetProposal, AssetProposalAction, AssetProposalStatus, ASSET_VOTING_PERIOD_BLOCKS,
};
use vtt_primitives::chain::{ConsensusParams, GasConfig};
use vtt_primitives::transaction::{Log, SignedTransaction, TransactionAction, TransactionReceipt};
use vtt_primitives::{Address, ChainId, Vote, H256};
use vtt_state::account::AccountState;
use vtt_state::asset::{AssetClass, AssetRecord, AssetStatus};
use vtt_state::StateDB;
use vtt_vm::context::{ExecutionContext, ExecutionParams};
use vtt_vm::VmEngine;

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("invalid signature")]
    InvalidSignature,
    #[error("nonce mismatch: expected {expected}, got {got}")]
    NonceMismatch { expected: u64, got: u64 },
    #[error("insufficient balance for gas: have {have}, need {need}")]
    InsufficientGas { have: Amount, need: Amount },
    #[error("insufficient balance for transfer: have {have}, need {need}")]
    InsufficientBalance { have: Amount, need: Amount },
    #[error("gas limit exceeded")]
    GasLimitExceeded,
    #[error("state error: {0}")]
    State(#[from] vtt_state::statedb::StateError),
    #[error("contract execution not yet supported")]
    ContractNotSupported,
    #[error("self-stake below minimum: have {have}, need {need}")]
    StakeBelowMinimum { have: Amount, need: Amount },
    #[error("cannot unstake more than staked: staked {staked}, requested {requested}")]
    UnstakeExceedsStake { staked: Amount, requested: Amount },
    #[error("contract too large: {size} bytes (max {max})")]
    ContractTooLarge { size: usize, max: usize },
    #[error("bridge is paused")]
    BridgePaused,
    #[error("asset not found")]
    AssetNotFound,
    #[error("sender is not the asset issuer")]
    NotIssuer,
    #[error("{0}")]
    Custom(String),
}

/// Result of executing a single transaction.
pub struct ExecutionResult {
    pub receipt: TransactionReceipt,
    pub gas_used: u64,
}

/// Execute a batch of signed transactions against a state database.
/// Returns receipts for each transaction and total gas used.
pub fn execute_block_transactions(
    state: &mut StateDB,
    transactions: &[SignedTransaction],
    gas_config: &GasConfig,
    block_gas_limit: u64,
) -> (Vec<TransactionReceipt>, u64) {
    execute_block_transactions_at(
        state,
        transactions,
        gas_config,
        block_gas_limit,
        0,
        0,
        ChainId::RELAY,
    )
}

/// Execute a batch of signed transactions at a given block height and timestamp.
/// Validates that each transaction's chain_id matches the expected `chain_id`.
/// Returns receipts for each transaction and total gas used.
pub fn execute_block_transactions_at(
    state: &mut StateDB,
    transactions: &[SignedTransaction],
    gas_config: &GasConfig,
    block_gas_limit: u64,
    block_number: u64,
    block_timestamp: u64,
    chain_id: ChainId,
) -> (Vec<TransactionReceipt>, u64) {
    // Process matured unbonding entries deterministically at the start of the
    // block so producer and importer reach the same state root. The block
    // timestamp (not wall clock) drives maturation.
    let _ = state.process_unbonding(block_timestamp);

    let mut receipts = Vec::with_capacity(transactions.len());
    let mut total_gas = 0u64;

    for tx in transactions {
        if total_gas >= block_gas_limit {
            break;
        }

        let result = execute_transaction_at(
            state,
            tx,
            gas_config,
            block_number,
            block_timestamp,
            chain_id,
        );
        total_gas += result.gas_used;
        receipts.push(result.receipt);
    }

    (receipts, total_gas)
}

/// Execute a single signed transaction (block_number and block_timestamp default to 0).
pub fn execute_transaction(
    state: &mut StateDB,
    tx: &SignedTransaction,
    gas_config: &GasConfig,
) -> ExecutionResult {
    execute_transaction_at(state, tx, gas_config, 0, 0, ChainId::RELAY)
}

/// Execute a single signed transaction at a given block height and timestamp.
/// Rejects transactions whose chain_id does not match the expected `chain_id`.
pub fn execute_transaction_at(
    state: &mut StateDB,
    tx: &SignedTransaction,
    gas_config: &GasConfig,
    block_number: u64,
    block_timestamp: u64,
    chain_id: ChainId,
) -> ExecutionResult {
    let tx_hash = blake3_hash(&match borsh::to_vec(&tx.payload) {
        Ok(b) => b,
        Err(_) => return fail_receipt(H256::ZERO, 0),
    });

    // 0. Validate chain_id — reject cross-chain replay attempts
    if tx.payload.chain_id != chain_id {
        debug!(
            ?tx_hash,
            expected = %chain_id,
            got = %tx.payload.chain_id,
            "chain_id mismatch"
        );
        return fail_receipt(tx_hash, 0);
    }

    // 1. Verify signature
    if let Err(_e) = verify(&tx.payload_bytes(), &tx.signature, &tx.public_key) {
        debug!(?tx_hash, "invalid signature");
        return fail_receipt(tx_hash, 0);
    }

    // 2. Derive sender address
    let sender = vtt_crypto::address_from_public_key(&tx.public_key);

    // 3. Check nonce
    let expected_nonce = state.get_nonce(&sender);
    if tx.payload.nonce != expected_nonce {
        debug!(
            ?tx_hash,
            expected_nonce,
            got = tx.payload.nonce,
            "nonce mismatch"
        );
        return fail_receipt(tx_hash, 0);
    }

    // 4. Enforce the effective minimum gas price. The txpool also gates
    // this on submission, but a malicious producer could assemble a block
    // that bypasses their own pool — import_block re-checks here so every
    // node rejects the same sub-minimum-fee tx deterministically.
    let effective_min_gas_price = state
        .get_min_gas_price_override()
        .unwrap_or(gas_config.min_gas_price);
    if tx.payload.gas_price < effective_min_gas_price {
        debug!(
            ?tx_hash,
            got = %tx.payload.gas_price,
            min = %effective_min_gas_price,
            "gas_price below network minimum"
        );
        return fail_receipt(tx_hash, 0);
    }

    // 5. Calculate gas cost
    let gas_cost = calculate_gas_cost(&tx.payload.action, gas_config);
    let gas_to_use = gas_cost.min(tx.payload.gas_limit);
    let gas_fee = tx
        .payload
        .gas_price
        .checked_mul(gas_to_use as u128)
        .unwrap_or(Amount::ZERO);

    // 5. Check sender can pay gas
    let sender_balance = state.get_balance(&sender);
    let total_value = match &tx.payload.action {
        TransactionAction::Transfer { amount, .. } => gas_fee.checked_add(*amount),
        TransactionAction::Stake { amount, .. } => gas_fee.checked_add(*amount),
        TransactionAction::CallContract { value, .. } => gas_fee.checked_add(*value),
        TransactionAction::DistributeRevenue { total_amount, .. } => {
            gas_fee.checked_add(*total_amount)
        }
        TransactionAction::BridgeWithdraw { token, amount, .. } if *token == H256::ZERO => {
            gas_fee.checked_add(*amount)
        }
        _ => Some(gas_fee),
    };

    let total_needed = match total_value {
        Some(v) => v,
        None => return fail_receipt(tx_hash, gas_to_use),
    };

    if sender_balance < total_needed {
        debug!(
            ?tx_hash,
            ?sender_balance,
            ?total_needed,
            "insufficient balance"
        );
        // Still deduct gas if possible
        let _ = state.sub_balance(&sender, gas_fee.min(sender_balance));
        state.increment_nonce(&sender);
        return fail_receipt(tx_hash, gas_to_use);
    }

    // 6. Take snapshot for rollback on failure
    let snapshot = state.snapshot();

    // 7. Deduct gas fee and increment nonce
    let _ = state.sub_balance(&sender, gas_fee);
    state.increment_nonce(&sender);

    // 8. Execute the action
    let exec_result = execute_action(
        state,
        &sender,
        &tx.payload.action,
        block_number,
        block_timestamp,
        tx.payload.nonce,
        tx.payload.gas_limit,
    );

    match exec_result {
        Ok(logs) => ExecutionResult {
            receipt: TransactionReceipt {
                tx_hash,
                success: true,
                gas_used: gas_to_use,
                logs,
            },
            gas_used: gas_to_use,
        },
        Err(e) => {
            debug!(?tx_hash, error = %e, "execution failed, rolling back");
            // Rollback state changes but keep gas deduction and nonce increment
            state.restore(snapshot);
            let _ = state.sub_balance(&sender, gas_fee);
            state.increment_nonce(&sender);
            fail_receipt(tx_hash, gas_to_use)
        }
    }
}

/// Execute the specific action of a transaction.
fn execute_action(
    state: &mut StateDB,
    sender: &Address,
    action: &TransactionAction,
    block_number: u64,
    block_timestamp: u64,
    nonce: u64,
    gas_limit: u64,
) -> Result<Vec<Log>, ExecutionError> {
    match action {
        TransactionAction::Transfer { to, amount } => {
            state.transfer(sender, to, *amount)?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"Transfer")],
                data: borsh::to_vec(&(*sender, *to, *amount)).unwrap(),
            }])
        }

        TransactionAction::Stake { validator, amount } => {
            execute_stake(state, sender, validator, *amount)?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"Stake")],
                data: borsh::to_vec(&(*sender, *validator, *amount)).unwrap(),
            }])
        }

        TransactionAction::Unstake { validator, amount } => {
            execute_unstake(state, sender, validator, *amount, block_timestamp)?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"Unstake")],
                data: borsh::to_vec(&(*sender, *validator, *amount)).unwrap(),
            }])
        }

        TransactionAction::GovernanceVote { proposal_id, vote } => {
            execute_governance_vote(state, sender, proposal_id, *vote, block_number)
        }

        TransactionAction::DeployContract { code, init_data: _ } => {
            execute_deploy_contract(state, sender, code)
        }

        TransactionAction::CallContract {
            contract,
            method,
            args,
            value,
        } => execute_call_contract(
            state,
            sender,
            contract,
            method,
            args,
            *value,
            block_number,
            block_timestamp,
            gas_limit,
        ),

        TransactionAction::CreateAssetClass {
            name,
            symbol,
            metadata_uri,
            total_supply,
            decimals,
            asset_class,
            jurisdiction,
            legal_entity,
        } => {
            execute_create_asset(
                state,
                sender,
                name,
                symbol,
                metadata_uri,
                *total_supply,
                *decimals,
                asset_class,
                jurisdiction,
                legal_entity,
                block_number,
            )?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"CreateAssetClass")],
                data: borsh::to_vec(&(sender, name, symbol)).unwrap(),
            }])
        }

        TransactionAction::AssetTransfer {
            asset_id,
            to,
            amount,
        } => {
            // Enforce transfer_mode: registrar-mediated transfers must be
            // originated by the configured registrar address. This anchors
            // the on-chain ledger to the off-chain legal registry.
            let asset = state
                .get_asset(asset_id)
                .ok_or(ExecutionError::AssetNotFound)?;
            let requires_kyc = asset.requires_kyc;
            let transfer_mode = asset.transfer_mode.clone();
            let registrar = asset.registrar;
            if matches!(
                transfer_mode,
                vtt_state::asset::TransferMode::RegistrarMediated
            ) {
                let registrar = registrar.ok_or_else(|| {
                    ExecutionError::Custom(
                        "asset configured as RegistrarMediated but has no registrar".into(),
                    )
                })?;
                if *sender != registrar {
                    return Err(ExecutionError::Custom(
                        "registrar-mediated asset: only the registrar can originate transfers"
                            .into(),
                    ));
                }
            }
            if requires_kyc {
                if !state.is_kyc_approved(sender) {
                    return Err(ExecutionError::Custom(
                        "sender is not KYC-approved for this regulated asset".into(),
                    ));
                }
                if !state.is_kyc_approved(to) {
                    return Err(ExecutionError::Custom(
                        "recipient is not KYC-approved for this regulated asset".into(),
                    ));
                }
            }
            check_jurisdiction_policy(state, sender)?;
            check_jurisdiction_policy(state, to)?;
            check_max_holders(state, asset_id, sender, to, *amount)?;
            state.transfer_asset(asset_id, sender, to, *amount)?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"AssetTransfer"), *asset_id],
                data: borsh::to_vec(&(*sender, *to, *amount)).unwrap(),
            }])
        }

        TransactionAction::CrossChainTransfer {
            destination_chain,
            to: _to,
            payload: _payload,
        } => {
            // Cross-chain routing is not yet live: the chain registry exists
            // (registered chains are visible via vtt_listRegisteredChains)
            // but no relayer moves messages from outbox to inbox. Rather
            // than silently locking funds in an outbox that never drains —
            // which would be a money-loss bug for any user who tried it —
            // reject the transaction with a clear error. The tx-level
            // snapshot/rollback guarantees no state mutation persists.
            Err(ExecutionError::Custom(format!(
                "cross-chain routing to {destination_chain} is not yet live; the app-chain is registered but the relayer is not active",
            )))
        }

        TransactionAction::CreatePool {
            token_a,
            token_b,
            amount_a,
            amount_b,
        } => {
            let epoch_length = state.get_epoch_length();
            let current_epoch = block_number.checked_div(epoch_length).unwrap_or(0);
            let pool = vtt_dex::liquidity::create_pool(
                state,
                sender,
                *token_a,
                *token_b,
                *amount_a,
                *amount_b,
                current_epoch,
            )
            .map_err(|e| ExecutionError::Custom(e.to_string()))?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"CreatePool"), pool.pool_id],
                data: borsh::to_vec(&pool.pool_id).unwrap(),
            }])
        }

        TransactionAction::AddLiquidity {
            pool_id,
            amount_a,
            amount_b,
            min_lp,
        } => {
            let lp_minted = vtt_dex::liquidity::add_liquidity(
                state, sender, pool_id, *amount_a, *amount_b, *min_lp,
            )
            .map_err(|e| ExecutionError::Custom(e.to_string()))?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"AddLiquidity"), *pool_id],
                data: borsh::to_vec(&lp_minted.0).unwrap(),
            }])
        }

        TransactionAction::RemoveLiquidity {
            pool_id,
            lp_amount,
            min_a,
            min_b,
        } => {
            let (out_a, out_b) = vtt_dex::liquidity::remove_liquidity(
                state, sender, pool_id, *lp_amount, *min_a, *min_b,
            )
            .map_err(|e| ExecutionError::Custom(e.to_string()))?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"RemoveLiquidity"), *pool_id],
                data: borsh::to_vec(&(out_a.0, out_b.0)).unwrap(),
            }])
        }

        TransactionAction::Swap {
            pool_id,
            token_in,
            amount_in,
            min_amount_out,
        } => {
            let amount_out = vtt_dex::swap::execute_swap(
                state,
                sender,
                pool_id,
                token_in,
                *amount_in,
                *min_amount_out,
            )
            .map_err(|e| ExecutionError::Custom(e.to_string()))?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"Swap"), *pool_id],
                data: borsh::to_vec(&amount_out.0).unwrap(),
            }])
        }

        TransactionAction::ClaimRevenue { pool_id } => {
            let treasury = state.get_treasury_address();
            let (fees_a, fees_b) =
                vtt_dex::revenue::claim_protocol_fees(state, sender, pool_id, &treasury)
                    .map_err(|e| ExecutionError::Custom(e.to_string()))?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"ClaimRevenue"), *pool_id],
                data: borsh::to_vec(&(fees_a.0, fees_b.0)).unwrap(),
            }])
        }

        TransactionAction::ClaimMiningRewards { pool_id } => {
            let epoch_length = state.get_epoch_length();
            let current_epoch = block_number.checked_div(epoch_length).unwrap_or(0);

            // Load mining state from storage
            let mining_data = state
                .get_mining_state_raw(pool_id)
                .ok_or_else(|| {
                    ExecutionError::Custom("mining not active for this pool".to_string())
                })?
                .to_vec();
            let mut mining_state = vtt_dex::MiningState::try_from_slice(&mining_data)
                .map_err(|_| ExecutionError::Custom("corrupt mining state".to_string()))?;

            let reward_amount = vtt_dex::mining::claim_mining_rewards(
                state,
                sender,
                pool_id,
                current_epoch,
                &mut mining_state,
            )
            .map_err(|e| ExecutionError::Custom(e.to_string()))?;

            // Save updated mining state
            let updated_data = borsh::to_vec(&mining_state).map_err(|_| {
                ExecutionError::Custom("failed to serialize mining state".to_string())
            })?;
            state.put_mining_state_raw(*pool_id, updated_data);

            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"ClaimMiningRewards"), *pool_id],
                data: borsh::to_vec(&reward_amount.raw()).unwrap(),
            }])
        }

        TransactionAction::DistributeRevenue {
            asset_id,
            total_amount,
        } => execute_distribute_revenue(state, sender, asset_id, *total_amount),

        TransactionAction::ProposeAssetAction {
            asset_id,
            action,
            description,
        } => execute_propose_asset_action(
            state,
            sender,
            asset_id,
            action,
            description,
            block_number,
            nonce,
        ),

        TransactionAction::VoteAssetProposal { proposal_id, vote } => {
            execute_vote_asset_proposal(state, sender, proposal_id, *vote, block_number)
        }

        TransactionAction::FinalizeAssetProposal { proposal_id } => {
            execute_finalize_asset_proposal(state, sender, proposal_id, block_number)
        }

        TransactionAction::BridgeWithdraw {
            token,
            amount,
            destination_chain,
            destination_address,
        } => execute_bridge_withdraw(
            state,
            sender,
            token,
            *amount,
            *destination_chain,
            destination_address,
        ),

        TransactionAction::GovernancePropose {
            description,
            action_type,
            param_key,
            param_value,
            recipient,
            amount,
        } => execute_governance_propose(
            state,
            sender,
            description,
            action_type,
            param_key.as_deref(),
            param_value.as_deref(),
            recipient.as_ref(),
            amount.as_ref(),
            block_number,
            nonce,
        ),

        TransactionAction::FreezeAsset { asset_id } => {
            execute_freeze_asset(state, sender, asset_id)
        }

        TransactionAction::UnfreezeAsset { asset_id } => {
            execute_unfreeze_asset(state, sender, asset_id)
        }

        TransactionAction::SubmitSlashingEvidence { evidence } => {
            execute_submit_slashing_evidence(state, sender, evidence, block_number)
        }

        TransactionAction::FundRedemptionPool { asset_id, amount } => {
            execute_fund_redemption_pool(state, sender, asset_id, *amount)
        }

        TransactionAction::ClaimRedemption { asset_id } => {
            execute_claim_redemption(state, sender, asset_id)
        }

        TransactionAction::SetKycApproval { address, approved } => {
            execute_set_kyc_approval(state, sender, address, *approved)
        }

        TransactionAction::BridgeDeposit {
            source_tx_hash,
            source_chain,
            recipient,
            token,
            amount,
        } => execute_bridge_deposit(
            state,
            sender,
            source_tx_hash,
            *source_chain,
            recipient,
            token,
            *amount,
        ),

        TransactionAction::SetAddressJurisdiction { address, country } => {
            execute_set_address_jurisdiction(state, sender, address, country)
        }

        TransactionAction::CreateOracleFeed {
            feed_id,
            name,
            feed_type,
            authorized_sources,
            quorum,
            max_staleness_ms,
            decimals,
        } => execute_create_oracle_feed(
            state,
            sender,
            *feed_id,
            name,
            feed_type,
            authorized_sources,
            *quorum,
            *max_staleness_ms,
            *decimals,
            block_number,
        ),

        TransactionAction::SubmitOracleValue { feed_id, value } => {
            execute_submit_oracle_value(state, sender, feed_id, *value, block_timestamp)
        }
    }
}

/// Fund the redemption pool. Only the asset issuer can call this, and only
/// when the asset is in RedemptionPending. VTT is debited from the sender.
fn execute_fund_redemption_pool(
    state: &mut StateDB,
    sender: &Address,
    asset_id: &H256,
    amount: Amount,
) -> Result<Vec<Log>, ExecutionError> {
    if amount.is_zero() {
        return Err(ExecutionError::Custom("amount is zero".into()));
    }
    let asset = state
        .get_asset(asset_id)
        .ok_or(ExecutionError::AssetNotFound)?;
    if asset.status != AssetStatus::RedemptionPending {
        return Err(ExecutionError::Custom(
            "asset is not in RedemptionPending state".into(),
        ));
    }
    if asset.issuer != *sender {
        return Err(ExecutionError::Custom(
            "only the asset issuer can fund the redemption pool".into(),
        ));
    }

    state.sub_balance(sender, amount)?;
    let asset_mut = state
        .get_asset_mut(asset_id)
        .ok_or(ExecutionError::AssetNotFound)?;
    asset_mut.redemption_pool =
        asset_mut
            .redemption_pool
            .checked_add(amount)
            .ok_or(ExecutionError::State(
                vtt_state::statedb::StateError::Serialization("redemption_pool overflow".into()),
            ))?;

    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"FundRedemptionPool"), *asset_id],
        data: borsh::to_vec(&(*sender, amount)).unwrap_or_default(),
    }])
}

/// Claim pro-rata redemption for a single holder. The holder's available +
/// locked token balance is burned and the corresponding share of the
/// redemption_pool is credited as VTT. When the last holder claims, the asset
/// transitions to Redeemed.
fn execute_claim_redemption(
    state: &mut StateDB,
    sender: &Address,
    asset_id: &H256,
) -> Result<Vec<Log>, ExecutionError> {
    let asset = state
        .get_asset(asset_id)
        .ok_or(ExecutionError::AssetNotFound)?;
    if asset.status != AssetStatus::RedemptionPending {
        return Err(ExecutionError::Custom(
            "asset is not in RedemptionPending state".into(),
        ));
    }
    let total_supply = asset.total_supply;
    let pool = asset.redemption_pool;
    if total_supply.is_zero() {
        return Err(ExecutionError::Custom("asset has zero total supply".into()));
    }
    if pool.is_zero() {
        return Err(ExecutionError::Custom(
            "redemption pool is empty, no proceeds to claim".into(),
        ));
    }

    let ownership = state.get_ownership(asset_id, sender);
    let holder_total = ownership.total();
    if holder_total.is_zero() {
        return Err(ExecutionError::Custom(
            "caller holds no tokens for this asset".into(),
        ));
    }

    // Pro-rata share: (holder_total * pool) / total_supply
    let share_raw = holder_total
        .raw()
        .checked_mul(pool.raw())
        .ok_or(ExecutionError::State(
            vtt_state::statedb::StateError::Serialization("redemption share overflow".into()),
        ))?
        / total_supply.raw();
    let share = Amount::from_raw(share_raw);

    // Burn the holder's tokens
    let mut zeroed = ownership.clone();
    zeroed.available = Amount::ZERO;
    zeroed.locked = Amount::ZERO;
    state.put_ownership(zeroed);

    // Credit the share in VTT
    state
        .add_balance(sender, share)
        .map_err(|e| ExecutionError::Custom(format!("credit failed: {e}")))?;

    // Update asset: reduce total_supply by holder_total, reduce pool by share
    let asset_mut = state
        .get_asset_mut(asset_id)
        .ok_or(ExecutionError::AssetNotFound)?;
    asset_mut.total_supply = asset_mut.total_supply - holder_total;
    asset_mut.redemption_pool = asset_mut.redemption_pool - share;
    if asset_mut.total_supply.is_zero() {
        asset_mut.status = AssetStatus::Redeemed;
    }

    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"ClaimRedemption"), *asset_id],
        data: borsh::to_vec(&(*sender, holder_total, share)).unwrap_or_default(),
    }])
}

/// Set or clear KYC approval for an address. Only callable by the treasury
/// address (the on-chain admin). Governance can change the treasury address
/// via ParameterChange if needed.
fn execute_set_kyc_approval(
    state: &mut StateDB,
    sender: &Address,
    address: &Address,
    approved: bool,
) -> Result<Vec<Log>, ExecutionError> {
    let treasury = state.get_treasury_address();
    if *sender != treasury {
        return Err(ExecutionError::Custom(
            "only the treasury / admin can set KYC approval".into(),
        ));
    }
    state.set_kyc_approved(address, approved);
    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"SetKycApproval")],
        data: borsh::to_vec(&(*address, approved)).unwrap_or_default(),
    }])
}

/// Register a new oracle feed. Treasury-gated so the set of trusted sources
/// is controlled. Duplicate feed_ids are rejected.
#[allow(clippy::too_many_arguments)]
fn execute_create_oracle_feed(
    state: &mut StateDB,
    sender: &Address,
    feed_id: H256,
    name: &str,
    feed_type: &str,
    authorized_sources: &[Address],
    quorum: u8,
    max_staleness_ms: u64,
    decimals: u8,
    block_number: u64,
) -> Result<Vec<Log>, ExecutionError> {
    if decimals > 30 {
        return Err(ExecutionError::Custom(
            "oracle decimals must be <= 30".into(),
        ));
    }
    let treasury = state.get_treasury_address();
    if *sender != treasury {
        return Err(ExecutionError::Custom(
            "only the treasury / admin can create oracle feeds".into(),
        ));
    }
    if name.is_empty() || name.len() > 128 {
        return Err(ExecutionError::Custom(
            "oracle feed name must be 1..=128 chars".into(),
        ));
    }
    if feed_type.is_empty() || feed_type.len() > 64 {
        return Err(ExecutionError::Custom(
            "oracle feed_type must be 1..=64 chars".into(),
        ));
    }
    if authorized_sources.is_empty() || authorized_sources.len() > 32 {
        return Err(ExecutionError::Custom(
            "oracle feed must have between 1 and 32 authorized sources".into(),
        ));
    }
    // Reject duplicate sources: the submit path dedupes by source, so duplicates
    // would just waste space and could mislead callers into picking a quorum
    // the feed can never actually reach.
    {
        let mut seen = std::collections::HashSet::with_capacity(authorized_sources.len());
        for a in authorized_sources {
            if !seen.insert(*a) {
                return Err(ExecutionError::Custom(
                    "oracle authorized_sources contains duplicates".into(),
                ));
            }
        }
    }
    if quorum == 0 || (quorum as usize) > authorized_sources.len() {
        return Err(ExecutionError::Custom(
            "oracle quorum must be between 1 and the number of authorized sources".into(),
        ));
    }

    let parsed_type = match feed_type {
        s if s.starts_with("price:") => vtt_state::oracle::OracleFeedType::MarketPrice(
            s.trim_start_matches("price:").to_string(),
        ),
        s if s.starts_with("rate:") => vtt_state::oracle::OracleFeedType::InterestRate(
            s.trim_start_matches("rate:").to_string(),
        ),
        s if s.starts_with("asset:") => {
            let tail = s.trim_start_matches("asset:");
            let hex_str = tail.strip_prefix("0x").unwrap_or(tail);
            if hex_str.len() != 64 {
                return Err(ExecutionError::Custom(
                    "oracle feed_type 'asset:<hex>' expects 32-byte asset id".into(),
                ));
            }
            let mut bytes = [0u8; 32];
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16).map_err(|_| {
                    ExecutionError::Custom("oracle feed_type 'asset:<hex>': invalid hex".into())
                })?;
            }
            vtt_state::oracle::OracleFeedType::AssetValuation(H256::from(bytes))
        }
        s => vtt_state::oracle::OracleFeedType::Custom(s.to_string()),
    };

    let mut feed = vtt_state::oracle::OracleFeed::new_with_decimals(
        feed_id,
        name.to_string(),
        parsed_type,
        authorized_sources.to_vec(),
        quorum,
        max_staleness_ms,
        decimals,
    );
    feed.created_at = block_number;
    state.register_oracle(feed).map_err(|e| match e {
        vtt_state::statedb::StateError::Serialization(s) if s.contains("already exists") => {
            ExecutionError::Custom(format!("oracle feed_id already registered: {feed_id}"))
        }
        other => ExecutionError::Custom(other.to_string()),
    })?;

    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"CreateOracleFeed"), feed_id],
        data: borsh::to_vec(&(
            feed_id,
            name.to_string(),
            authorized_sources.to_vec(),
            quorum,
        ))
        .unwrap_or_default(),
    }])
}

/// Submit a new value to an existing oracle feed. Sender must be one of the
/// feed's authorized sources. Quorum aggregation and persistence happen
/// inside `StateDB::submit_oracle`.
fn execute_submit_oracle_value(
    state: &mut StateDB,
    sender: &Address,
    feed_id: &H256,
    value: Amount,
    block_timestamp: u64,
) -> Result<Vec<Log>, ExecutionError> {
    let reached = state
        .submit_oracle(feed_id, *sender, value, block_timestamp)
        .map_err(|e| ExecutionError::Custom(e.to_string()))?;
    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"SubmitOracleValue"), *feed_id],
        data: borsh::to_vec(&(*sender, value, block_timestamp, reached)).unwrap_or_default(),
    }])
}

/// Record the jurisdiction (ISO 3166-1 alpha-2 country code) for an address.
/// Treasury-gated, same pattern as `SetKycApproval`. Empty string clears the
/// mapping. Used by the jurisdiction_whitelist / jurisdiction_blacklist gates
/// enforced at AssetTransfer time.
fn execute_set_address_jurisdiction(
    state: &mut StateDB,
    sender: &Address,
    address: &Address,
    country: &str,
) -> Result<Vec<Log>, ExecutionError> {
    let treasury = state.get_treasury_address();
    if *sender != treasury {
        return Err(ExecutionError::Custom(
            "only the treasury / admin can set address jurisdiction".into(),
        ));
    }
    let country_upper = country.trim().to_ascii_uppercase();
    if !country_upper.is_empty() && country_upper.len() != 2 {
        return Err(ExecutionError::Custom(
            "jurisdiction must be an ISO 3166-1 alpha-2 code (2 letters) or empty".into(),
        ));
    }
    if !country_upper.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(ExecutionError::Custom(
            "jurisdiction must contain only ASCII letters".into(),
        ));
    }
    state.set_address_jurisdiction(address, &country_upper);
    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"SetAddressJurisdiction")],
        data: borsh::to_vec(&(*address, country_upper)).unwrap_or_default(),
    }])
}

/// Credit a bridge deposit on VTT chain. Signed by the configured relayer.
fn execute_bridge_deposit(
    state: &mut StateDB,
    sender: &Address,
    source_tx_hash: &H256,
    source_chain: u32,
    recipient: &Address,
    token: &H256,
    amount: Amount,
) -> Result<Vec<Log>, ExecutionError> {
    // 1. Bridge must not be paused
    if state.is_bridge_paused() {
        return Err(ExecutionError::Custom("bridge is paused".into()));
    }

    // 2. Only the configured relayer can submit
    let relayer = state.bridge_relayer();
    if relayer == Address::ZERO {
        return Err(ExecutionError::Custom(
            "bridge relayer not configured".into(),
        ));
    }
    if *sender != relayer {
        return Err(ExecutionError::Custom(
            "only the configured bridge relayer can submit BridgeDeposit".into(),
        ));
    }

    // 3. Replay protection
    if state.bridge_deposit_processed(source_chain, source_tx_hash) {
        return Err(ExecutionError::Custom(
            "bridge deposit already processed".into(),
        ));
    }

    if amount.is_zero() {
        return Err(ExecutionError::Custom(
            "bridge deposit amount is zero".into(),
        ));
    }

    // 4. Credit recipient: either native VTT or an asset
    if *token == H256::ZERO {
        state
            .add_balance(recipient, amount)
            .map_err(|e| ExecutionError::Custom(format!("credit recipient failed: {e}")))?;
    } else {
        // Asset: must exist and be active
        let asset = state
            .get_asset(token)
            .ok_or_else(|| ExecutionError::Custom(format!("bridge asset not found: {token}")))?;
        if !asset.is_tradeable() {
            return Err(ExecutionError::Custom("bridge asset is not active".into()));
        }
        let mut ownership = state.get_ownership(token, recipient);
        ownership.credit(amount);
        state.put_ownership(ownership);
    }

    state.mark_bridge_deposit_processed(source_chain, source_tx_hash);

    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"BridgeDeposit"), *source_tx_hash, *token],
        data: borsh::to_vec(&(*recipient, *token, amount)).unwrap_or_default(),
    }])
}

/// Execute a submit slashing evidence transaction. Verifies the evidence, and
/// if valid and not already processed, applies a double-sign slash to the
/// offender.
fn execute_submit_slashing_evidence(
    state: &mut StateDB,
    sender: &Address,
    evidence_bytes: &[u8],
    block_number: u64,
) -> Result<Vec<Log>, ExecutionError> {
    use vtt_consensus::slashing::{calculate_double_sign_slash, DoubleSignEvidence};

    let evidence = DoubleSignEvidence::try_from_slice(evidence_bytes)
        .map_err(|_| ExecutionError::Custom("malformed slashing evidence".into()))?;

    if !evidence.is_valid() {
        return Err(ExecutionError::Custom("invalid slashing evidence".into()));
    }

    let offender = evidence.offender();

    // Idempotency: dedup by (offender, epoch, slot) in state
    if state.slashing_evidence_seen(&offender, evidence.header_a.epoch, evidence.header_a.slot) {
        return Err(ExecutionError::Custom(
            "slashing evidence already processed".into(),
        ));
    }

    let account = state.get_account(&offender);
    let total_stake = account
        .staking
        .as_ref()
        .map(|s| s.total_stake)
        .unwrap_or(Amount::ZERO);
    if total_stake.is_zero() {
        return Err(ExecutionError::Custom(
            "offender has no stake to slash".into(),
        ));
    }

    // 5% slash for double-sign
    let slash_amount = calculate_double_sign_slash(total_stake, 500);
    let actual = state.apply_slash(&offender, slash_amount);
    let epoch = evidence.header_a.epoch;
    state.record_slash(&offender, epoch, "double_sign", actual);
    state.mark_slashing_evidence(offender, evidence.header_a.epoch, evidence.header_a.slot);

    let mut topic_bytes = [0u8; 32];
    topic_bytes[..20].copy_from_slice(offender.as_bytes());
    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"SlashValidator"), H256::from(topic_bytes)],
        data: borsh::to_vec(&(offender, actual, epoch, block_number)).unwrap_or_default(),
    }])
}

/// Execute a staking operation.
fn execute_stake(
    state: &mut StateDB,
    sender: &Address,
    validator: &Address,
    amount: Amount,
) -> Result<(), ExecutionError> {
    // Deduct from sender balance
    state.sub_balance(sender, amount)?;

    // Update validator's staking state
    let mut val_account = state.get_account(validator);
    let mut staking = val_account.staking.unwrap_or_default();

    if sender == validator {
        // Self-stake
        staking.self_stake =
            staking
                .self_stake
                .checked_add(amount)
                .ok_or(ExecutionError::State(
                    vtt_state::statedb::StateError::Serialization("overflow".into()),
                ))?;
    } else {
        // Delegation — reject delegating to addresses that have never
        // self-staked ("ghost validators"). A validator must have registered
        // itself with at least one self-stake tx before delegators can join.
        if staking.self_stake.is_zero() {
            return Err(ExecutionError::Custom(
                "cannot delegate to an address with no self-stake".into(),
            ));
        }
        if let Some(delegation) = staking
            .delegations
            .iter_mut()
            .find(|d| d.delegator == *sender)
        {
            delegation.amount =
                delegation
                    .amount
                    .checked_add(amount)
                    .ok_or(ExecutionError::State(
                        vtt_state::statedb::StateError::Serialization("overflow".into()),
                    ))?;
        } else {
            staking.delegations.push(vtt_state::account::Delegation {
                delegator: *sender,
                amount,
            });
        }
    }

    staking.total_stake = staking
        .total_stake
        .checked_add(amount)
        .ok_or(ExecutionError::State(
            vtt_state::statedb::StateError::Serialization("overflow".into()),
        ))?;

    val_account.staking = Some(staking);
    state.put_account(*validator, val_account);

    Ok(())
}

/// Default unbonding period: 21 days in milliseconds.
const DEFAULT_UNBONDING_PERIOD_SECS: u64 = 21 * 24 * 3600;

/// Execute an unstaking operation.
/// Tokens are not returned immediately; instead an unbonding entry is created
/// that matures after the unbonding period.
fn execute_unstake(
    state: &mut StateDB,
    sender: &Address,
    validator: &Address,
    amount: Amount,
    block_timestamp: u64,
) -> Result<(), ExecutionError> {
    let mut val_account = state.get_account(validator);
    let mut staking = val_account.staking.clone().unwrap_or_default();

    if sender == validator {
        // Self-unstake
        if staking.self_stake < amount {
            return Err(ExecutionError::UnstakeExceedsStake {
                staked: staking.self_stake,
                requested: amount,
            });
        }
        staking.self_stake = staking.self_stake - amount;
    } else {
        // Undelegation
        let delegation = staking
            .delegations
            .iter_mut()
            .find(|d| d.delegator == *sender)
            .ok_or(ExecutionError::UnstakeExceedsStake {
                staked: Amount::ZERO,
                requested: amount,
            })?;

        if delegation.amount < amount {
            return Err(ExecutionError::UnstakeExceedsStake {
                staked: delegation.amount,
                requested: amount,
            });
        }
        delegation.amount = delegation.amount - amount;

        // Remove delegation entry if zero
        staking.delegations.retain(|d| !d.amount.is_zero());
    }

    staking.total_stake = staking.total_stake - amount;
    val_account.staking = Some(staking);
    state.put_account(*validator, val_account);

    // Create an unbonding entry instead of returning VTT immediately.
    // The funds will be released when process_unbonding() is called at a
    // block whose timestamp >= completion_time. The effective period honours
    // any governance-set `unbonding_period_secs` override.
    let completion_time =
        block_timestamp + state.effective_unbonding_period_ms(DEFAULT_UNBONDING_PERIOD_SECS);
    state.add_unbonding_entry(
        *sender,
        vtt_state::account::UnbondingEntry {
            amount,
            completion_time,
            validator: *validator,
        },
    );

    Ok(())
}

/// Execute contract deployment.
fn execute_deploy_contract(
    state: &mut StateDB,
    sender: &Address,
    code: &[u8],
) -> Result<Vec<Log>, ExecutionError> {
    use vtt_vm::gas::GasCosts;

    // Reject oversized contracts before attempting compilation
    if code.len() > GasCosts::MAX_CONTRACT_SIZE {
        return Err(ExecutionError::ContractTooLarge {
            size: code.len(),
            max: GasCosts::MAX_CONTRACT_SIZE,
        });
    }

    let engine = VmEngine::new();

    // Validate the WASM bytecode compiles
    engine
        .compile(code)
        .map_err(|_| ExecutionError::ContractNotSupported)?;

    // Store the code and compute the code hash
    let code_hash = state.store_code(code.to_vec());

    // Derive contract address from sender + nonce
    let nonce = state.get_nonce(sender);
    let addr_data = borsh::to_vec(&(*sender, nonce)).unwrap();
    let contract_addr_hash = blake3_hash(&addr_data);
    let contract_addr = Address::from_slice(&contract_addr_hash.as_bytes()[12..32]);

    // Create contract account
    let contract_account = AccountState {
        nonce: 0,
        balance: Amount::ZERO,
        code_hash: Some(code_hash),
        storage_root: vtt_primitives::H256::ZERO,
        staking: None,
    };
    state.put_account(contract_addr, contract_account);

    debug!(
        ?contract_addr,
        ?code_hash,
        code_size = code.len(),
        "contract deployed"
    );

    Ok(vec![Log {
        address: contract_addr,
        topics: vec![blake3_hash(b"ContractDeployed")],
        data: borsh::to_vec(&(contract_addr, code_hash)).unwrap(),
    }])
}

/// Execute a contract call.
#[allow(clippy::too_many_arguments)]
fn execute_call_contract(
    state: &mut StateDB,
    sender: &Address,
    contract: &Address,
    method: &str,
    args: &[u8],
    value: Amount,
    block_number: u64,
    block_timestamp: u64,
    gas_limit: u64,
) -> Result<Vec<Log>, ExecutionError> {
    let contract_account = state.get_account(contract);
    let code_hash = contract_account
        .code_hash
        .ok_or(ExecutionError::ContractNotSupported)?;

    let code = state
        .get_code(&code_hash)
        .ok_or(ExecutionError::ContractNotSupported)?
        .clone();

    // Transfer value to contract if any
    if !value.is_zero() {
        state.transfer(sender, contract, value)?;
    }

    // Load existing contract storage from StateDB into the execution context
    let existing_storage = state.load_contract_storage(contract);

    let mut engine = VmEngine::new();
    let ctx = ExecutionContext::new(ExecutionParams {
        contract_address: *contract,
        caller: *sender,
        origin: *sender,
        value,
        block_number,
        block_timestamp,
        chain_id: vtt_primitives::ChainId::RELAY,
        gas_limit,
    });

    // Pre-populate the execution context with existing storage
    {
        let mut storage = ctx
            .storage
            .lock()
            .map_err(|_| ExecutionError::Custom("contract storage lock poisoned".into()))?;
        for (key, val) in existing_storage {
            storage.insert(key, val);
        }
    }

    let result = engine
        .execute(&code, method, args, ctx.clone())
        .map_err(|e| {
            ExecutionError::State(vtt_state::statedb::StateError::Serialization(e.to_string()))
        })?;

    if result.status != 0 {
        return Err(ExecutionError::State(
            vtt_state::statedb::StateError::Serialization(format!(
                "contract reverted with status {}",
                result.status
            )),
        ));
    }

    // Persist storage changes back to StateDB
    {
        let storage = ctx
            .storage
            .lock()
            .map_err(|_| ExecutionError::Custom("contract storage lock poisoned".into()))?;
        for (key, val) in storage.iter() {
            state.put_contract_storage(*contract, key.clone(), val.clone());
        }
    }

    // Process balance changes from the execution context
    for change in ctx.take_balance_changes() {
        if change.is_credit {
            state.add_balance(&change.address, change.amount)?;
        } else {
            state.sub_balance(&change.address, change.amount)?;
        }
    }

    // Collect logs from execution context
    let mut logs = ctx.take_logs();
    logs.push(Log {
        address: *contract,
        topics: vec![blake3_hash(b"ContractCall")],
        data: borsh::to_vec(&(*sender, *contract, method)).unwrap(),
    });

    Ok(logs)
}

/// Execute asset creation.
fn execute_create_asset(
    state: &mut StateDB,
    sender: &Address,
    name: &str,
    symbol: &str,
    metadata_uri: &str,
    total_supply: Amount,
    decimals: u8,
    asset_class: &str,
    jurisdiction: &str,
    legal_entity: &str,
    block_number: u64,
) -> Result<(), ExecutionError> {
    // Generate a deterministic asset ID from sender + name + symbol
    let id_data = borsh::to_vec(&(*sender, name, symbol)).unwrap();
    let asset_id = blake3_hash(&id_data);

    let class = match asset_class {
        "equity" => AssetClass::Equity,
        "debt" => AssetClass::Debt,
        "real_estate" => AssetClass::RealEstate,
        "commodity" => AssetClass::Commodity,
        "fund" => AssetClass::Fund,
        "ip" => AssetClass::IntellectualProperty,
        "carbon" => AssetClass::CarbonCredit,
        "invoice" => AssetClass::Invoice,
        _ => AssetClass::Custom(asset_class.to_string()),
    };

    // Regulated asset classes require jurisdiction (ISO 3166-1 alpha-2) and
    // a non-empty legal_entity. Commodity/CarbonCredit/Invoice/Custom are
    // currently treated as unregulated from the chain's perspective; stricter
    // policy can be enforced at the chain compliance layer.
    let is_regulated = matches!(
        class,
        AssetClass::Equity | AssetClass::Debt | AssetClass::RealEstate | AssetClass::Fund
    );
    if is_regulated {
        if jurisdiction.len() != 2 || !jurisdiction.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(ExecutionError::Custom(
                "jurisdiction must be a 2-letter ISO 3166-1 alpha-2 code for regulated asset classes".into(),
            ));
        }
        if legal_entity.trim().is_empty() {
            return Err(ExecutionError::Custom(
                "legal_entity is required for regulated asset classes".into(),
            ));
        }
    }

    let asset = AssetRecord {
        id: asset_id,
        name: name.to_string(),
        symbol: symbol.to_string(),
        class,
        origin_chain: ChainId::RELAY,
        issuer: *sender,
        total_supply,
        decimals,
        status: AssetStatus::Active,
        compliance_policy: None,
        valuation_oracle: None,
        documents: std::collections::BTreeMap::new(),
        metadata_uri: metadata_uri.to_string(),
        jurisdiction: jurisdiction.to_string(),
        legal_entity: legal_entity.to_string(),
        transfer_mode: vtt_state::asset::TransferMode::PeerToPeer,
        registrar: None,
        redemption_pool: Amount::ZERO,
        // Regulated classes default to requiring KYC; admin can flip later via
        // governance if the asset is deployed on a permissionless app chain.
        requires_kyc: is_regulated,
        created_at: block_number,
    };

    state.register_asset(asset)?;

    // Mint total supply to issuer
    let mut ownership = state.get_ownership(&asset_id, sender);
    ownership.credit(total_supply);
    state.put_ownership(ownership);

    Ok(())
}

/// Force-close a RedemptionPending asset via governance. Asset must currently
/// be in `RedemptionPending`. Any unclaimed balance in the redemption pool is
/// swept to the treasury so it is not locked forever. Transitions the asset
/// to `Redeemed`.
fn execute_finalize_redemption(state: &mut StateDB, asset_id: &H256) -> Result<(), ExecutionError> {
    let asset = state
        .get_asset(asset_id)
        .ok_or_else(|| ExecutionError::Custom(format!("asset not found: {asset_id}")))?;
    if asset.status != AssetStatus::RedemptionPending {
        return Err(ExecutionError::Custom(
            "FinalizeRedemption only applies to assets in RedemptionPending".into(),
        ));
    }
    let remaining = asset.redemption_pool;
    let treasury = state.get_treasury_address();

    let asset_mut = state
        .get_asset_mut(asset_id)
        .ok_or_else(|| ExecutionError::Custom(format!("asset not found: {asset_id}")))?;
    asset_mut.status = AssetStatus::Redeemed;
    asset_mut.redemption_pool = Amount::ZERO;

    if !remaining.is_zero() {
        state
            .add_balance(&treasury, remaining)
            .map_err(|e| ExecutionError::Custom(e.to_string()))?;
    }
    Ok(())
}

/// Execute on-chain revenue distribution: debit VTT from sender (the asset issuer)
/// and credit each holder proportionally to their available holdings.
fn execute_distribute_revenue(
    state: &mut StateDB,
    sender: &Address,
    asset_id: &H256,
    total_amount: Amount,
) -> Result<Vec<Log>, ExecutionError> {
    if total_amount.is_zero() {
        return Err(ExecutionError::Custom(
            "distribution amount must be non-zero".into(),
        ));
    }

    // Verify asset exists and sender is the issuer
    let asset = state
        .get_asset(asset_id)
        .ok_or_else(|| ExecutionError::Custom(format!("asset not found: {asset_id}")))?;
    if asset.issuer != *sender {
        return Err(ExecutionError::Custom(
            "only the asset issuer can distribute revenue".into(),
        ));
    }
    let total_supply = asset.total_supply;
    if total_supply.is_zero() {
        return Err(ExecutionError::Custom("asset has zero total supply".into()));
    }

    // Collect holders snapshot (we need to iterate first, then mutate state)
    let holders: Vec<(Address, Amount)> = state
        .iter_ownership_for_asset(asset_id)
        .filter(|r| !r.available.is_zero())
        .map(|r| (r.owner, r.available))
        .collect();

    if holders.is_empty() {
        return Err(ExecutionError::Custom(
            "no holders with available balance".into(),
        ));
    }

    // Debit total_amount from sender's VTT balance
    state.sub_balance(sender, total_amount)?;

    // Distribute pro-rata: share = holder_available * total_amount / total_supply
    // Use u256-style math via u128 with careful ordering to avoid overflow:
    // share = (holder_available.raw() as u128) * (total_amount.raw() as u128) / (total_supply.raw() as u128)
    // We use checked arithmetic with intermediate widening to u128 (already u128, so no overflow issue
    // for realistic amounts).
    let mut distributed = Amount::ZERO;
    let mut num_recipients = 0u64;
    for (holder_addr, holder_available) in &holders {
        // Use u128 multiplication; the product could overflow u128 for very large amounts,
        // so we use a simple safe helper: a * b / c with u128.
        let share_raw = mul_div(
            holder_available.raw(),
            total_amount.raw(),
            total_supply.raw(),
        );
        if share_raw > 0 {
            let share = Amount::from_raw(share_raw);
            state.add_balance(holder_addr, share)?;
            distributed = distributed + share;
            num_recipients += 1;
        }
    }

    // Remainder (due to rounding) goes back to sender
    if let Some(remainder) = total_amount.checked_sub(distributed) {
        if !remainder.is_zero() {
            state.add_balance(sender, remainder)?;
        }
    }

    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"DistributeRevenue"), *asset_id],
        data: borsh::to_vec(&(*sender, *asset_id, total_amount.raw(), num_recipients)).unwrap(),
    }])
}

/// Execute a ProposeAssetAction transaction.
fn execute_propose_asset_action(
    state: &mut StateDB,
    sender: &Address,
    asset_id: &H256,
    action: &AssetProposalAction,
    description: &str,
    block_number: u64,
    nonce: u64,
) -> Result<Vec<Log>, ExecutionError> {
    // Verify the asset exists
    let asset = state
        .get_asset(asset_id)
        .ok_or_else(|| ExecutionError::Custom(format!("asset not found: {asset_id}")))?;

    // Verify sender holds > 0 tokens of this asset
    let ownership = state.get_ownership(asset_id, sender);
    if ownership.available.is_zero() {
        return Err(ExecutionError::Custom(
            "only token holders can propose asset actions".into(),
        ));
    }

    // If action is DistributeRevenue, verify sender has enough VTT balance
    // (don't debit yet - debit on execution)
    if let AssetProposalAction::DistributeRevenue { total_amount } = action {
        let sender_balance = state.get_balance(sender);
        if sender_balance < *total_amount {
            return Err(ExecutionError::InsufficientBalance {
                have: sender_balance,
                need: *total_amount,
            });
        }
    }

    // FinalizeRedemption only makes sense once the asset has been moved to
    // RedemptionPending by a prior DisposeAsset vote — reject the proposal
    // upfront rather than wasting a voting period on an impossible action.
    if matches!(action, AssetProposalAction::FinalizeRedemption { .. })
        && asset.status != AssetStatus::RedemptionPending
    {
        return Err(ExecutionError::Custom(
            "FinalizeRedemption can only be proposed on an asset in RedemptionPending".into(),
        ));
    }

    // Create proposal with unique ID (blake3 hash of asset_id + proposer + block_number + nonce)
    let id_data = borsh::to_vec(&(*asset_id, *sender, block_number, nonce)).unwrap();
    let proposal_id = blake3_hash(&id_data);

    let _ = asset; // used above for existence check

    let proposal = AssetProposal {
        id: proposal_id,
        asset_id: *asset_id,
        proposer: *sender,
        action: action.clone(),
        description: description.to_string(),
        created_at: block_number,
        voting_end: block_number + ASSET_VOTING_PERIOD_BLOCKS,
        status: AssetProposalStatus::Active,
        votes_yes: Amount::ZERO,
        votes_no: Amount::ZERO,
        votes_abstain: Amount::ZERO,
        voters: Vec::new(),
    };

    state.put_asset_proposal(proposal);

    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"ProposeAssetAction"), proposal_id],
        data: borsh::to_vec(&(*sender, *asset_id, proposal_id)).unwrap(),
    }])
}

/// Execute a VoteAssetProposal transaction.
fn execute_vote_asset_proposal(
    state: &mut StateDB,
    sender: &Address,
    proposal_id: &H256,
    vote: Vote,
    current_block: u64,
) -> Result<Vec<Log>, ExecutionError> {
    // Load proposal
    let proposal = state
        .get_asset_proposal(proposal_id)
        .ok_or_else(|| ExecutionError::Custom("asset proposal not found".into()))?;

    // Verify it's Active
    if proposal.status != AssetProposalStatus::Active {
        return Err(ExecutionError::Custom("proposal is not active".into()));
    }

    // Verify voting hasn't ended
    if proposal.is_voting_ended(current_block) {
        return Err(ExecutionError::Custom("voting period has ended".into()));
    }

    // Verify sender hasn't already voted
    if proposal.has_voted(sender) {
        return Err(ExecutionError::Custom(
            "already voted on this proposal".into(),
        ));
    }

    // Get sender's token balance for the proposal's asset_id
    let asset_id = proposal.asset_id;
    let ownership = state.get_ownership(&asset_id, sender);
    let voting_power = ownership.available;

    // Verify balance > 0
    if voting_power.is_zero() {
        return Err(ExecutionError::Custom(
            "no token holdings to vote with".into(),
        ));
    }

    // Add vote weight
    let proposal_mut = state
        .get_asset_proposal_mut(proposal_id)
        .ok_or_else(|| ExecutionError::Custom("asset proposal not found".into()))?;

    match vote {
        Vote::Yes => proposal_mut.votes_yes = proposal_mut.votes_yes + voting_power,
        Vote::No => proposal_mut.votes_no = proposal_mut.votes_no + voting_power,
        Vote::Abstain => proposal_mut.votes_abstain = proposal_mut.votes_abstain + voting_power,
    }

    proposal_mut.voters.push(*sender);

    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"VoteAssetProposal"), *proposal_id],
        data: borsh::to_vec(&(*sender, vote as u8, voting_power.raw())).unwrap(),
    }])
}

/// Execute a FinalizeAssetProposal transaction.
fn execute_finalize_asset_proposal(
    state: &mut StateDB,
    sender: &Address,
    proposal_id: &H256,
    current_block: u64,
) -> Result<Vec<Log>, ExecutionError> {
    // Load proposal and clone needed fields to avoid borrow issues
    let proposal = state
        .get_asset_proposal(proposal_id)
        .ok_or_else(|| ExecutionError::Custom("asset proposal not found".into()))?;

    // Verify it's Active
    if proposal.status != AssetProposalStatus::Active {
        return Err(ExecutionError::Custom("proposal is not active".into()));
    }

    // Verify voting period has ended
    if !proposal.is_voting_ended(current_block) {
        return Err(ExecutionError::Custom(
            "voting period has not ended yet".into(),
        ));
    }

    // Get the asset's total supply for quorum calculation
    let asset_id = proposal.asset_id;
    let asset = state
        .get_asset(&asset_id)
        .ok_or_else(|| ExecutionError::Custom(format!("asset not found: {asset_id}")))?;
    let total_supply = asset.total_supply;

    // Clone action and proposer before mutating state
    let action = proposal.action.clone();
    let proposer = proposal.proposer;

    // Check quorum: total votes >= ASSET_QUORUM_BPS of total_supply
    let has_quorum = proposal.has_quorum(total_supply);

    // Check threshold based on action type
    let passes = if has_quorum {
        match &action {
            AssetProposalAction::ChangeIssuer { .. }
            | AssetProposalAction::DisposeAsset { .. }
            | AssetProposalAction::FinalizeRedemption { .. } => proposal.passes_supermajority(),
            _ => proposal.passes_threshold(),
        }
    } else {
        false
    };

    if passes {
        // Execute the action
        match &action {
            AssetProposalAction::DistributeRevenue { total_amount } => {
                // Debit VTT from proposer, distribute pro-rata to all holders
                execute_distribute_revenue(state, &proposer, &asset_id, *total_amount)?;
            }
            AssetProposalAction::ChangeIssuer { new_issuer } => {
                // Update the asset's issuer field
                let asset_mut = state.get_asset_mut(&asset_id).ok_or_else(|| {
                    ExecutionError::Custom(format!("asset not found: {asset_id}"))
                })?;
                asset_mut.issuer = *new_issuer;
            }
            AssetProposalAction::Signal { .. } => {
                // No on-chain action for signal proposals
            }
            AssetProposalAction::DisposeAsset { .. } => {
                // Move the asset to RedemptionPending. Transfers are blocked
                // but holders can still call ClaimRedemption to burn their
                // tokens and receive their pro-rata share of the proceeds.
                // The proceeds amount is credited off-chain after the legal
                // sale closes; for now the redemption_pool is initialized to
                // zero and incremented via a separate funding action.
                let asset_mut = state.get_asset_mut(&asset_id).ok_or_else(|| {
                    ExecutionError::Custom(format!("asset not found: {asset_id}"))
                })?;
                asset_mut.status = AssetStatus::RedemptionPending;
            }
            AssetProposalAction::FinalizeRedemption { .. } => {
                execute_finalize_redemption(state, &asset_id)?;
            }
        }

        // Mark as Executed
        let proposal_mut = state
            .get_asset_proposal_mut(proposal_id)
            .ok_or_else(|| ExecutionError::Custom("asset proposal not found".into()))?;
        proposal_mut.status = AssetProposalStatus::Executed;
    } else {
        // Mark as Rejected
        let proposal_mut = state
            .get_asset_proposal_mut(proposal_id)
            .ok_or_else(|| ExecutionError::Custom("asset proposal not found".into()))?;
        proposal_mut.status = AssetProposalStatus::Rejected;
    }

    let final_status = match state.get_asset_proposal(proposal_id) {
        Some(p) => p.status.clone(),
        None => {
            return Err(ExecutionError::Custom(
                "asset proposal disappeared during finalization".into(),
            ))
        }
    };
    let status_str = match &final_status {
        AssetProposalStatus::Executed => "Executed",
        AssetProposalStatus::Rejected => "Rejected",
        _ => "Unknown",
    };

    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"FinalizeAssetProposal"), *proposal_id],
        data: borsh::to_vec(&(*sender, *proposal_id, status_str)).unwrap(),
    }])
}

/// Execute an asset freeze (only the issuer can freeze).
fn execute_freeze_asset(
    state: &mut StateDB,
    sender: &Address,
    asset_id: &H256,
) -> Result<Vec<Log>, ExecutionError> {
    let mut asset = state
        .get_asset_owned(asset_id)
        .ok_or(ExecutionError::AssetNotFound)?;
    if asset.issuer != *sender {
        return Err(ExecutionError::NotIssuer);
    }
    asset.freeze();
    state.put_asset(asset_id, &asset);
    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"FreezeAsset"), *asset_id],
        data: borsh::to_vec(&(*sender, *asset_id)).unwrap(),
    }])
}

/// Execute an asset unfreeze (only the issuer can unfreeze).
fn execute_unfreeze_asset(
    state: &mut StateDB,
    sender: &Address,
    asset_id: &H256,
) -> Result<Vec<Log>, ExecutionError> {
    let mut asset = state
        .get_asset_owned(asset_id)
        .ok_or(ExecutionError::AssetNotFound)?;
    if asset.issuer != *sender {
        return Err(ExecutionError::NotIssuer);
    }
    asset.unfreeze();
    state.put_asset(asset_id, &asset);
    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"UnfreezeAsset"), *asset_id],
        data: borsh::to_vec(&(*sender, *asset_id)).unwrap(),
    }])
}

/// Execute a bridge withdrawal: burn tokens on VTT chain.
/// A backend relayer watches for these logs and releases tokens on the destination chain.
fn execute_bridge_withdraw(
    state: &mut StateDB,
    sender: &Address,
    token: &H256,
    amount: Amount,
    destination_chain: u32,
    destination_address: &Address,
) -> Result<Vec<Log>, ExecutionError> {
    if state.is_bridge_paused() {
        return Err(ExecutionError::BridgePaused);
    }

    if amount.is_zero() {
        return Err(ExecutionError::Custom(
            "bridge withdraw amount must be non-zero".into(),
        ));
    }

    if *token == H256::ZERO {
        // Native VTT: burn by debiting sender balance
        state.sub_balance(sender, amount)?;
    } else {
        // Asset token: verify asset exists, then burn by transferring to Address::ZERO
        if state.get_asset(token).is_none() {
            return Err(ExecutionError::Custom(format!("asset not found: {token}")));
        }
        state.transfer_asset(token, sender, &Address::ZERO, amount)?;
    }

    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"BridgeWithdraw"), *token],
        data: borsh::to_vec(&(
            *sender,
            *token,
            amount,
            destination_chain,
            *destination_address,
        ))
        .unwrap(),
    }])
}

/// Execute governance proposal creation.
/// The sender must have staked VTT (either as a validator or delegator).
/// The proposal is persisted directly in the state DB.
#[allow(clippy::too_many_arguments)]
fn execute_governance_propose(
    state: &mut StateDB,
    sender: &Address,
    description: &str,
    action_type: &str,
    param_key: Option<&str>,
    param_value: Option<&str>,
    recipient: Option<&Address>,
    amount: Option<&Amount>,
    block_number: u64,
    _nonce: u64,
) -> Result<Vec<Log>, ExecutionError> {
    if description.is_empty() {
        return Err(ExecutionError::Custom(
            "proposal description must not be empty".into(),
        ));
    }

    // Validate action_type and map to ProposalAction using the real parameters
    let action = match action_type {
        "parameter_change" => {
            let key = param_key.unwrap_or_default().to_string();
            let value = param_value.unwrap_or(description).to_string();
            validate_parameter_change(&key, &value)?;
            ProposalAction::ParameterChange { key, value }
        }
        "treasury_spend" => {
            let recv = recipient.copied().unwrap_or(Address::ZERO);
            let amt = amount.copied().unwrap_or(Amount::ZERO);
            ProposalAction::TreasurySpend {
                recipient: recv,
                amount: amt,
            }
        }
        "signal" => ProposalAction::ProtocolUpgrade {
            version: 0,
            description: description.to_string(),
        },
        "dex_pause" => ProposalAction::DexPause(true),
        "dex_unpause" => ProposalAction::DexPause(false),
        "bridge_pause" => ProposalAction::BridgePause(true),
        "bridge_unpause" => ProposalAction::BridgePause(false),
        "register_chain" => {
            let name = param_key.unwrap_or("").to_string();
            let config_json = param_value.unwrap_or("{}").to_string();
            ProposalAction::RegisterChain { name, config_json }
        }
        other => {
            return Err(ExecutionError::Custom(format!(
                "invalid action_type '{}', must be one of: parameter_change, treasury_spend, signal, dex_pause, dex_unpause, bridge_pause, bridge_unpause, register_chain",
                other
            )));
        }
    };

    // Check the sender has stake (validator self-stake or delegation)
    let sender_account = state.get_account(sender);
    let has_stake = match &sender_account.staking {
        Some(staking) => !staking.total_stake.is_zero(),
        None => false,
    };

    if !has_stake {
        return Err(ExecutionError::Custom(
            "sender must have staked VTT to create proposals".into(),
        ));
    }

    // Compute total staked for vote weight snapshot
    let total_staked: Amount = state
        .iter_accounts()
        .filter_map(|(_, acc)| acc.staking.as_ref())
        .fold(Amount::ZERO, |sum, s| sum + s.total_stake);

    // Generate a unique, persistent proposal ID using the governance counter
    let gov_seq = state.next_governance_id();
    let id_data = borsh::to_vec(&(gov_seq, sender, block_number)).unwrap();
    let proposal_id = blake3_hash(&id_data);

    let proposal = Proposal {
        id: proposal_id,
        proposer: *sender,
        action,
        description: description.to_string(),
        created_at: block_number,
        voting_end: block_number + vtt_consensus::governance::voting_period_blocks(),
        status: vtt_consensus::governance::ProposalStatus::Active,
        votes_yes: Amount::ZERO,
        votes_no: Amount::ZERO,
        votes_abstain: Amount::ZERO,
        voters: Vec::new(),
        snapshot_block: block_number,
        total_staked_at_creation: total_staked,
    };
    let proposal_bytes = borsh::to_vec(&proposal)
        .map_err(|e| ExecutionError::Custom(format!("proposal serialization failed: {e}")))?;
    state.put_governance_proposal(proposal_id, proposal_bytes);

    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"GovernancePropose"), proposal_id],
        data: borsh::to_vec(&(*sender, description, action_type)).unwrap(),
    }])
}

/// Execute a governance vote on a protocol proposal.
/// Loads the proposal from state, records the vote, and saves back.
fn execute_governance_vote(
    state: &mut StateDB,
    sender: &Address,
    proposal_id: &H256,
    vote: Vote,
    block_number: u64,
) -> Result<Vec<Log>, ExecutionError> {
    // Load proposal from state
    let proposal_bytes = state
        .get_governance_proposal_owned(proposal_id)
        .ok_or_else(|| ExecutionError::Custom("governance proposal not found".into()))?;

    let mut proposal: Proposal = borsh::from_slice(&proposal_bytes)
        .map_err(|e| ExecutionError::Custom(format!("corrupt governance proposal: {e}")))?;

    // Verify it's Active
    if proposal.status != vtt_consensus::governance::ProposalStatus::Active {
        return Err(ExecutionError::Custom("proposal is not active".into()));
    }

    // Verify voting hasn't ended
    if proposal.is_voting_ended(block_number) {
        return Err(ExecutionError::Custom("voting period has ended".into()));
    }

    // Verify sender hasn't already voted
    if proposal.has_voted(sender) {
        return Err(ExecutionError::Custom(
            "already voted on this proposal".into(),
        ));
    }

    // Get sender's voting power (staked VTT)
    let sender_account = state.get_account(sender);
    let voter_stake = match &sender_account.staking {
        Some(staking) if !staking.total_stake.is_zero() => staking.total_stake,
        _ => {
            return Err(ExecutionError::Custom("no staked VTT to vote with".into()));
        }
    };

    // Cap vote weight: can't vote with more than existed at proposal creation.
    // This prevents vote manipulation by buying stake after a proposal is created.
    let voting_power = if !proposal.total_staked_at_creation.is_zero() {
        voter_stake.min(proposal.total_staked_at_creation)
    } else {
        voter_stake // backwards compat for proposals without snapshot
    };

    // Apply vote
    match vote {
        Vote::Yes => proposal.votes_yes = proposal.votes_yes + voting_power,
        Vote::No => proposal.votes_no = proposal.votes_no + voting_power,
        Vote::Abstain => proposal.votes_abstain = proposal.votes_abstain + voting_power,
    }
    proposal.voters.push(*sender);

    // Save updated proposal
    let updated_bytes = borsh::to_vec(&proposal)
        .map_err(|e| ExecutionError::Custom(format!("proposal serialization failed: {e}")))?;
    state.put_governance_proposal(*proposal_id, updated_bytes);

    Ok(vec![Log {
        address: *sender,
        topics: vec![blake3_hash(b"GovernanceVote"), *proposal_id],
        data: borsh::to_vec(&vote).unwrap(),
    }])
}

/// Safe integer math: a * b / c without overflow using u128.
/// For amounts up to ~3.4e38 (u128 max), the product a*b can overflow.
/// We widen to (u128, u128) pair representing a 256-bit value when needed.
fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    // Use u128 directly when possible; otherwise fall back to widening.
    if let Some(product) = a.checked_mul(b) {
        product / c
    } else {
        // a * b overflows u128. Use decomposition: a = q*c + r, so
        // a*b/c = q*b + r*b/c, which avoids the full product.
        let q1 = (a / c) * b;
        let r1 = a % c;
        // r1 < c, so r1 * b may still overflow — handle recursively
        let q2 = if let Some(prod) = r1.checked_mul(b) {
            prod / c
        } else {
            // Both factors large; decompose again
            (r1 / c) * b + (r1 % c) * (b / c)
        };
        q1 + q2
    }
}

/// Calculate gas cost for an action.
fn calculate_gas_cost(action: &TransactionAction, config: &GasConfig) -> u64 {
    match action {
        TransactionAction::Transfer { .. } => config.base_transfer_cost,
        TransactionAction::Stake { .. } => config.base_transfer_cost * 2,
        TransactionAction::Unstake { .. } => config.base_transfer_cost * 2,
        TransactionAction::GovernanceVote { .. } => config.base_transfer_cost,
        TransactionAction::DeployContract { code, .. } => {
            config.base_transfer_cost + (code.len() as u64 * config.cost_per_byte)
        }
        TransactionAction::CallContract { args, .. } => {
            config.base_transfer_cost + (args.len() as u64 * config.cost_per_byte)
        }
        TransactionAction::CreateAssetClass { .. } => config.base_transfer_cost * 5,
        TransactionAction::AssetTransfer { .. } => config.base_transfer_cost * 2,
        TransactionAction::CrossChainTransfer { .. } => config.base_transfer_cost * 3,
        TransactionAction::CreatePool { .. } => 50_000,
        TransactionAction::AddLiquidity { .. } => 30_000,
        TransactionAction::RemoveLiquidity { .. } => 30_000,
        TransactionAction::Swap { .. } => 25_000,
        TransactionAction::ClaimRevenue { .. } => 10_000,
        TransactionAction::ClaimMiningRewards { .. } => 10_000,
        TransactionAction::DistributeRevenue { .. } => 50_000,
        TransactionAction::ProposeAssetAction { .. } => 100_000,
        TransactionAction::VoteAssetProposal { .. } => 30_000,
        TransactionAction::FinalizeAssetProposal { .. } => 100_000,
        TransactionAction::BridgeWithdraw { .. } => 50_000,
        TransactionAction::GovernancePropose { .. } => 100_000,
        TransactionAction::FreezeAsset { .. } => 30_000,
        TransactionAction::UnfreezeAsset { .. } => 30_000,
        TransactionAction::SubmitSlashingEvidence { .. } => 150_000,
        TransactionAction::BridgeDeposit { .. } => 80_000,
        TransactionAction::FundRedemptionPool { .. } => 50_000,
        TransactionAction::ClaimRedemption { .. } => 80_000,
        TransactionAction::SetKycApproval { .. } => 30_000,
        TransactionAction::SetAddressJurisdiction { .. } => 30_000,
        TransactionAction::CreateOracleFeed { .. } => 80_000,
        TransactionAction::SubmitOracleValue { .. } => 40_000,
    }
}

/// Auto-finalize governance proposals whose voting period has ended.
///
/// For each protocol governance proposal stored in state:
///   - Skip if not Active or voting period not ended
///   - Check quorum (33% of total staked VTT must have voted)
///   - Check threshold (>50% yes votes of yes+no)
///   - If passed: execute the proposal action, then mark as Executed
///   - If failed: mark as Rejected
///
/// Returns the number of proposals finalized.
pub fn finalize_governance_proposals(
    state: &mut StateDB,
    current_block: u64,
    total_staked: Amount,
) -> u64 {
    use vtt_consensus::governance::{execution_delay_blocks, ProposalStatus};

    // Collect all proposal IDs and their raw bytes first (to avoid borrow
    // issues). Sorted by id so iteration order is deterministic across
    // nodes — HashMap order otherwise differs between replicas and would
    // cause consensus drift when multiple proposals mutate the same
    // state (e.g. a ParameterChange that rotates treasury_address followed
    // by a TreasurySpend in the same finalisation pass).
    let mut proposals_raw: Vec<(H256, Vec<u8>)> = state
        .iter_governance_proposals()
        .map(|(id, data)| (*id, data.clone()))
        .collect();
    proposals_raw.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    let mut finalized_count = 0u64;

    for (proposal_id, proposal_bytes) in proposals_raw {
        let proposal: Proposal = match borsh::from_slice(&proposal_bytes) {
            Ok(p) => p,
            Err(e) => {
                debug!(?proposal_id, error = %e, "skipping corrupt governance proposal");
                continue;
            }
        };

        // Only process Active proposals whose voting period has ended
        if proposal.status != ProposalStatus::Active {
            continue;
        }
        if !proposal.is_voting_ended(current_block) {
            continue;
        }

        // Check quorum and threshold
        let has_quorum = proposal.has_quorum(total_staked);
        let passes_threshold = proposal.passes_threshold();
        let passed = has_quorum && passes_threshold;

        if passed {
            // Queue the proposal for execution after timelock delay
            let mut updated = proposal.clone();
            let execute_after = current_block + execution_delay_blocks();
            updated.status = ProposalStatus::Queued { execute_after };
            let updated_bytes = match borsh::to_vec(&updated) {
                Ok(b) => b,
                Err(_) => continue,
            };
            state.put_governance_proposal(proposal_id, updated_bytes);
            debug!(
                ?proposal_id,
                execute_after, "governance proposal queued for execution after timelock"
            );
        } else {
            // Mark as Rejected
            let mut updated = proposal.clone();
            updated.status = ProposalStatus::Rejected;
            let updated_bytes = match borsh::to_vec(&updated) {
                Ok(b) => b,
                Err(_) => continue,
            };
            state.put_governance_proposal(proposal_id, updated_bytes);
        }

        finalized_count += 1;
        debug!(
            ?proposal_id,
            passed, has_quorum, passes_threshold, "governance proposal finalized"
        );
    }

    finalized_count
}

/// Execute governance proposals whose timelock has expired.
pub fn execute_queued_proposals(state: &mut StateDB, current_block: u64) -> u64 {
    use vtt_consensus::governance::{ProposalAction, ProposalStatus};

    // Sort by id so execution order is deterministic across replicas.
    let mut proposals_raw: Vec<(H256, Vec<u8>)> = state
        .iter_governance_proposals()
        .map(|(id, data)| (*id, data.clone()))
        .collect();
    proposals_raw.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    let mut executed_count = 0u64;

    for (proposal_id, proposal_bytes) in proposals_raw {
        let proposal: Proposal = match borsh::from_slice(&proposal_bytes) {
            Ok(p) => p,
            Err(e) => {
                debug!(?proposal_id, error = %e, "skipping corrupt governance proposal");
                continue;
            }
        };

        let execute_after = match &proposal.status {
            ProposalStatus::Queued { execute_after } => *execute_after,
            _ => continue,
        };

        if current_block < execute_after {
            continue;
        }

        match &proposal.action {
            ProposalAction::ParameterChange { key, value } => {
                // Whitelist + validation happens at proposal creation time
                // (`validate_parameter_change`), so by the time we reach here
                // the key is known and the value is well-formed. Re-validate
                // defensively in case state was migrated from an older schema.
                apply_parameter_change(state, key, value, &proposal_id);
            }
            ProposalAction::TreasurySpend { recipient, amount } => {
                let treasury_addr = state.get_treasury_address();
                let treasury_balance = state.get_balance(&treasury_addr);
                if treasury_balance >= *amount {
                    if let Err(e) = state.transfer(&treasury_addr, recipient, *amount) {
                        debug!(?proposal_id, error = %e, "treasury spend transfer failed");
                    } else {
                        debug!(?proposal_id, %recipient, %amount, "treasury spend executed");
                    }
                }
            }
            ProposalAction::RegisterChain { name, config_json } => {
                match register_app_chain(state, name, config_json, &proposal, current_block) {
                    Ok(chain_id) => {
                        tracing::info!(
                            ?proposal_id,
                            name,
                            %chain_id,
                            event = "AppChainRegistered",
                            "app chain registered"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(?proposal_id, name, error = %e, "RegisterChain failed");
                    }
                }
            }
            ProposalAction::ProtocolUpgrade {
                version,
                description,
            } => {
                debug!(
                    ?proposal_id,
                    version, description, "protocol upgrade signal executed"
                );
            }
            ProposalAction::DexPause(paused) => {
                state.set_dex_paused(*paused);
                debug!(
                    ?proposal_id,
                    paused, "DEX pause state updated via governance"
                );
            }
            ProposalAction::BridgePause(paused) => {
                state.set_bridge_paused(*paused);
                debug!(
                    ?proposal_id,
                    paused, "bridge pause state updated via governance"
                );
            }
        }

        let mut updated = proposal.clone();
        updated.status = ProposalStatus::Executed;
        let updated_bytes = match borsh::to_vec(&updated) {
            Ok(b) => b,
            Err(_) => continue,
        };
        state.put_governance_proposal(proposal_id, updated_bytes);
        executed_count += 1;
        debug!(
            ?proposal_id,
            "queued governance proposal executed after timelock"
        );
    }

    executed_count
}

/// Process double-sign slashing evidence against the state database.
pub fn process_slashing_evidence(
    state: &mut StateDB,
    evidence: &[vtt_consensus::slashing::DoubleSignEvidence],
    double_sign_slash_bps: u16,
    current_epoch: u64,
) -> Vec<(Address, Amount)> {
    use vtt_consensus::slashing::calculate_double_sign_slash;

    let mut slashed = Vec::new();
    for ev in evidence {
        if !ev.is_valid() {
            continue;
        }
        let offender = ev.offender();
        // Dedup by (offender, epoch, slot) so a validator can't be slashed
        // twice for the same evidence via the in-block auto-detect path and
        // a follow-up SubmitSlashingEvidence transaction.
        let ev_epoch = ev.header_a.epoch;
        let ev_slot = ev.header_a.slot;
        if state.slashing_evidence_seen(&offender, ev_epoch, ev_slot) {
            continue;
        }
        let account = state.get_account(&offender);
        let total_stake = account
            .staking
            .as_ref()
            .map(|s| s.total_stake)
            .unwrap_or(Amount::ZERO);

        if total_stake.is_zero() {
            continue;
        }

        let slash_amount = calculate_double_sign_slash(total_stake, double_sign_slash_bps);
        let actual = state.apply_slash(&offender, slash_amount);
        if !actual.is_zero() {
            state.record_slash(&offender, current_epoch, "double_sign", actual);
            state.mark_slashing_evidence(offender, ev_epoch, ev_slot);
            slashed.push((offender, actual));
        }
    }
    slashed
}

/// Parse a hex-encoded address string (with or without 0x prefix, 40 hex chars).
fn parse_address(s: &str) -> Option<Address> {
    let trimmed = s.trim().strip_prefix("0x").unwrap_or(s.trim());
    if trimmed.len() != 40 {
        return None;
    }
    let mut bytes = [0u8; 20];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&trimmed[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(Address::from(bytes))
}

/// Whitelist of governance-controlled protocol parameters and their acceptable
/// value formats. Called at proposal creation time so unknown keys or malformed
/// values never reach the execution path.
/// Parse a `RegisterChain` proposal and persist the resulting
/// `RegisteredChain` blob in state. Returns the allocated chain_id.
///
/// `config_json` is expected to be a small JSON object:
/// ```json
/// {
///   "description": "...",
///   "validator_count": 11,
///   "compliance": "permissioned" | "permissionless",
///   "trusted_issuers": ["0x...", ...]
/// }
/// ```
/// Missing fields get sensible defaults. The consensus and gas configs
/// are inherited from the relay's defaults — a future governance action
/// can tune them per-chain once multichain routing is live.
fn register_app_chain(
    state: &mut StateDB,
    name: &str,
    config_json: &str,
    proposal: &vtt_consensus::governance::Proposal,
    current_block: u64,
) -> std::result::Result<vtt_primitives::ChainId, String> {
    use vtt_compliance::ChainComplianceConfig;
    use vtt_multichain::RegisteredChain;
    use vtt_primitives::chain::{ConsensusParams, GasConfig};
    use vtt_primitives::ChainId;

    #[derive(serde::Deserialize, Default)]
    struct Config {
        #[serde(default)]
        description: String,
        #[serde(default = "default_validator_count")]
        validator_count: u32,
        #[serde(default)]
        compliance: String,
        #[serde(default)]
        trusted_issuers: Vec<String>,
    }
    fn default_validator_count() -> u32 {
        11
    }

    let cfg: Config =
        serde_json::from_str(config_json).map_err(|e| format!("invalid config_json: {e}"))?;

    if cfg.validator_count == 0 {
        return Err("validator_count must be > 0".into());
    }

    let compliance = match cfg.compliance.to_ascii_lowercase().as_str() {
        "" | "permissionless" => ChainComplianceConfig::permissionless(),
        "permissioned" => {
            let issuers: Vec<H256> = cfg
                .trusted_issuers
                .iter()
                .filter_map(|s| {
                    let trimmed = s.trim().trim_start_matches("0x");
                    if trimmed.len() != 64 {
                        return None;
                    }
                    let mut bytes = [0u8; 32];
                    for (i, b) in bytes.iter_mut().enumerate() {
                        *b = u8::from_str_radix(&trimmed[i * 2..i * 2 + 2], 16).ok()?;
                    }
                    Some(H256::from(bytes))
                })
                .collect();
            ChainComplianceConfig::permissioned(issuers)
        }
        other => return Err(format!("unknown compliance mode '{other}'")),
    };

    let chain_id_raw = state.next_registered_chain_id();
    let chain_id = ChainId::new(chain_id_raw);

    let record = RegisteredChain {
        chain_id,
        name: name.to_string(),
        description: cfg.description,
        validator_count: cfg.validator_count,
        consensus: ConsensusParams::default(),
        gas: GasConfig::default(),
        compliance,
        genesis_hash: H256::ZERO,
        active: true,
        registered_at: current_block,
        proposer: proposal.proposer,
    };
    let bytes = borsh::to_vec(&record).map_err(|e| format!("encode failed: {e}"))?;
    state.put_registered_chain(chain_id_raw, &bytes);
    Ok(chain_id)
}

fn validate_parameter_change(key: &str, value: &str) -> Result<(), ExecutionError> {
    match key {
        "bridge_relayer" | "treasury_address" => {
            parse_address(value).ok_or_else(|| {
                ExecutionError::Custom(format!(
                    "ParameterChange '{key}': value must be a 20-byte hex address",
                ))
            })?;
        }
        "min_gas_price" => {
            value.trim().parse::<u128>().map_err(|_| {
                ExecutionError::Custom(format!(
                    "ParameterChange '{key}': value must be a u128 (raw Amount)",
                ))
            })?;
        }
        "base_transfer_cost" | "cost_per_byte" | "unbonding_period_secs" => {
            value.trim().parse::<u64>().map_err(|_| {
                ExecutionError::Custom(format!("ParameterChange '{key}': value must be a u64"))
            })?;
        }
        "slash_double_sign_bps" | "slash_downtime_bps" => {
            let v: u16 = value.trim().parse().map_err(|_| {
                ExecutionError::Custom(format!(
                    "ParameterChange '{key}': value must be a u16 (basis points 0..=10000)",
                ))
            })?;
            if v > 10_000 {
                return Err(ExecutionError::Custom(format!(
                    "ParameterChange '{key}': basis points must be <= 10000 (got {v})",
                )));
            }
        }
        "downtime_threshold_pct" => {
            let v: u8 = value.trim().parse().map_err(|_| {
                ExecutionError::Custom(format!(
                    "ParameterChange '{key}': value must be a u8 percentage 0..=100",
                ))
            })?;
            if v > 100 {
                return Err(ExecutionError::Custom(format!(
                    "ParameterChange '{key}': percentage must be <= 100 (got {v})",
                )));
            }
        }
        "max_holders_per_asset" => {
            value.trim().parse::<u32>().map_err(|_| {
                ExecutionError::Custom(format!(
                    "ParameterChange '{key}': value must be a u32 (0 = unlimited)",
                ))
            })?;
        }
        "jurisdiction_whitelist" | "jurisdiction_blacklist" => {
            for code in value.split(',').map(|c| c.trim()) {
                if code.is_empty() {
                    continue;
                }
                if code.len() != 2 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
                    return Err(ExecutionError::Custom(format!(
                        "ParameterChange '{key}': '{code}' is not a valid ISO 3166-1 alpha-2 code",
                    )));
                }
            }
        }
        other => {
            return Err(ExecutionError::Custom(format!(
                "ParameterChange: unknown parameter '{other}'. Allowed: bridge_relayer, treasury_address, min_gas_price, base_transfer_cost, cost_per_byte, slash_double_sign_bps, slash_downtime_bps, downtime_threshold_pct, unbonding_period_secs, max_holders_per_asset, jurisdiction_whitelist, jurisdiction_blacklist",
            )));
        }
    }
    Ok(())
}

/// Apply a validated ParameterChange to state. Values already passed
/// `validate_parameter_change` at propose time; a parse failure here would only
/// come from a corrupt persisted proposal, in which case we log and skip.
/// All successful mutations are emitted at INFO level (not debug) so node
/// operators can audit governance-driven parameter drift from logs.
fn apply_parameter_change(state: &mut StateDB, key: &str, value: &str, proposal_id: &H256) {
    use tracing::info;
    match key {
        "bridge_relayer" => {
            if let Some(addr) = parse_address(value) {
                state.set_bridge_relayer(addr);
                info!(?proposal_id, %addr, event="ParameterChanged", key="bridge_relayer", "bridge_relayer updated");
            } else {
                debug!(
                    ?proposal_id,
                    value, "invalid bridge_relayer address (skipped)"
                );
            }
        }
        "treasury_address" => {
            if let Some(addr) = parse_address(value) {
                state.set_treasury_address(addr);
                info!(?proposal_id, %addr, event="ParameterChanged", key="treasury_address", "treasury_address updated");
            }
        }
        "min_gas_price" => {
            if let Ok(v) = value.trim().parse::<u128>() {
                state.set_min_gas_price(Amount::from_raw(v));
                info!(
                    ?proposal_id,
                    v,
                    event = "ParameterChanged",
                    key = "min_gas_price",
                    "min_gas_price updated"
                );
            }
        }
        "base_transfer_cost" => {
            if let Ok(v) = value.trim().parse::<u64>() {
                state.set_base_transfer_cost(v);
                info!(
                    ?proposal_id,
                    v,
                    event = "ParameterChanged",
                    key = "base_transfer_cost",
                    "base_transfer_cost updated"
                );
            }
        }
        "cost_per_byte" => {
            if let Ok(v) = value.trim().parse::<u64>() {
                state.set_cost_per_byte(v);
                info!(
                    ?proposal_id,
                    v,
                    event = "ParameterChanged",
                    key = "cost_per_byte",
                    "cost_per_byte updated"
                );
            }
        }
        "slash_double_sign_bps" => {
            if let Ok(v) = value.trim().parse::<u16>() {
                if v <= 10_000 {
                    state.set_slash_double_sign_bps(v);
                    info!(
                        ?proposal_id,
                        v,
                        event = "ParameterChanged",
                        key = "slash_double_sign_bps",
                        "slash_double_sign_bps updated"
                    );
                }
            }
        }
        "slash_downtime_bps" => {
            if let Ok(v) = value.trim().parse::<u16>() {
                if v <= 10_000 {
                    state.set_slash_downtime_bps(v);
                    info!(
                        ?proposal_id,
                        v,
                        event = "ParameterChanged",
                        key = "slash_downtime_bps",
                        "slash_downtime_bps updated"
                    );
                }
            }
        }
        "downtime_threshold_pct" => {
            if let Ok(v) = value.trim().parse::<u8>() {
                if v <= 100 {
                    state.set_downtime_threshold_pct(v);
                    info!(
                        ?proposal_id,
                        v,
                        event = "ParameterChanged",
                        key = "downtime_threshold_pct",
                        "downtime_threshold_pct updated"
                    );
                }
            }
        }
        "unbonding_period_secs" => {
            if let Ok(v) = value.trim().parse::<u64>() {
                state.set_unbonding_period_secs(v);
                info!(
                    ?proposal_id,
                    v,
                    event = "ParameterChanged",
                    key = "unbonding_period_secs",
                    "unbonding_period_secs updated"
                );
            }
        }
        "max_holders_per_asset" => {
            if let Ok(v) = value.trim().parse::<u32>() {
                state.set_max_holders_per_asset(v);
                info!(
                    ?proposal_id,
                    v,
                    event = "ParameterChanged",
                    key = "max_holders_per_asset",
                    "max_holders_per_asset updated"
                );
            }
        }
        "jurisdiction_whitelist" => {
            state.set_jurisdiction_whitelist(value);
            info!(
                ?proposal_id,
                value,
                event = "ParameterChanged",
                key = "jurisdiction_whitelist",
                "jurisdiction_whitelist updated"
            );
        }
        "jurisdiction_blacklist" => {
            state.set_jurisdiction_blacklist(value);
            info!(
                ?proposal_id,
                value,
                event = "ParameterChanged",
                key = "jurisdiction_blacklist",
                "jurisdiction_blacklist updated"
            );
        }
        other => {
            debug!(
                key = other,
                ?proposal_id,
                "unknown ParameterChange key on execute (corrupt state?) — skipping"
            );
        }
    }
}

/// Enforce chain-wide jurisdiction whitelist / blacklist against an address'
/// recorded country code.
/// - Whitelist empty → no whitelist check.
/// - Whitelist non-empty → address must have a jurisdiction set and it must
///   be in the whitelist.
/// - Blacklist is always checked if the address has a jurisdiction.
fn check_jurisdiction_policy(state: &StateDB, addr: &Address) -> Result<(), ExecutionError> {
    let whitelist = state.get_jurisdiction_whitelist();
    let blacklist = state.get_jurisdiction_blacklist();
    if whitelist.is_empty() && blacklist.is_empty() {
        return Ok(());
    }
    let jur = state
        .get_address_jurisdiction(addr)
        .map(|c| c.to_ascii_uppercase());
    if !whitelist.is_empty() {
        let Some(ref code) = jur else {
            return Err(ExecutionError::Custom(format!(
                "address {addr} has no jurisdiction set; chain requires whitelisted jurisdiction",
            )));
        };
        if !whitelist.iter().any(|c| c.eq_ignore_ascii_case(code)) {
            return Err(ExecutionError::Custom(format!(
                "jurisdiction '{code}' is not in the chain whitelist",
            )));
        }
    }
    if let Some(code) = jur {
        if blacklist.iter().any(|c| c.eq_ignore_ascii_case(&code)) {
            return Err(ExecutionError::Custom(format!(
                "jurisdiction '{code}' is blacklisted on this chain",
            )));
        }
    }
    Ok(())
}

/// Enforce chain-wide `max_holders_per_asset`. Skipped when recipient already
/// holds a non-zero balance of the asset (existing holder, transfer doesn't
/// grow the holder set).
fn check_max_holders(
    state: &StateDB,
    asset_id: &H256,
    sender: &Address,
    recipient: &Address,
    amount: Amount,
) -> Result<(), ExecutionError> {
    let max = state.get_max_holders_per_asset();
    if max == 0 {
        return Ok(());
    }
    let recipient_holding = state.get_ownership(asset_id, recipient);
    if !recipient_holding.total().is_zero() {
        // Recipient is already a holder — transfer can only change balances,
        // never the holder set.
        return Ok(());
    }
    // Recipient is a new holder. Allow when the sender is moving their full
    // balance across (sender -> 0 offsets recipient -> non-zero → net = 0).
    let sender_holding = state.get_ownership(asset_id, sender);
    let sender_total_after = sender_holding.total().raw().saturating_sub(amount.raw());
    if sender_total_after == 0 && !sender_holding.total().is_zero() {
        return Ok(());
    }
    let current_holders = state.asset_holder_count(asset_id);
    if current_holders >= max {
        return Err(ExecutionError::Custom(format!(
            "asset has reached max_holders_per_asset ({max}); recipient is a new holder",
        )));
    }
    Ok(())
}

fn fail_receipt(tx_hash: H256, gas_used: u64) -> ExecutionResult {
    ExecutionResult {
        receipt: TransactionReceipt {
            tx_hash,
            success: false,
            gas_used,
            logs: vec![],
        },
        gas_used,
    }
}

/// Initial supply used in the inflation formula. Must match the genesis
/// total across all validator/delegator/treasury balances so the circulating
/// supply calculation is the same on every node.
const INITIAL_SUPPLY_VTT: u64 = 1_000_000_000;

/// Summary of state mutations applied by `apply_block_rewards_and_governance`.
/// Consumers may use this for telemetry — it has no consensus significance.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlockSettlementSummary {
    pub minted: Amount,
    pub burned: Amount,
    pub treasury: Amount,
    pub producer_total: Amount,
    pub governance_finalized: u64,
    pub governance_executed: u64,
}

/// Pay a producer reward according to the validator's declared commission:
/// commission_bps goes to the validator, the remainder is split pro-rata
/// between self-stake and all active delegators. Falls back to crediting
/// the validator if staking state is missing.
fn distribute_producer_reward(
    state: &mut StateDB,
    validator_addr: &Address,
    producer_reward: Amount,
) {
    if producer_reward.raw() == 0 {
        return;
    }
    let account = state.get_account(validator_addr);
    let staking = match account.staking.as_ref() {
        Some(s) if s.total_stake.raw() > 0 => s.clone(),
        _ => {
            let _ = state.add_balance(validator_addr, producer_reward);
            return;
        }
    };

    let split = split_producer_reward(producer_reward, staking.commission_bps);
    if split.validator_commission.raw() > 0 {
        let _ = state.add_balance(validator_addr, split.validator_commission);
    }

    let total_stake = staking.total_stake.raw();
    let stakers_pool = split.staker_rewards.raw();
    if stakers_pool == 0 || total_stake == 0 {
        return;
    }

    let self_share = staking
        .self_stake
        .raw()
        .saturating_mul(stakers_pool)
        .checked_div(total_stake)
        .unwrap_or(0);
    if self_share > 0 {
        let _ = state.add_balance(validator_addr, Amount::from_raw(self_share));
    }

    let mut distributed = self_share;
    for delegation in &staking.delegations {
        let share = delegation
            .amount
            .raw()
            .saturating_mul(stakers_pool)
            .checked_div(total_stake)
            .unwrap_or(0);
        if share == 0 {
            continue;
        }
        let _ = state.add_balance(&delegation.delegator, Amount::from_raw(share));
        distributed = distributed.saturating_add(share);
    }

    if distributed < stakers_pool {
        let dust = stakers_pool - distributed;
        let _ = state.add_balance(validator_addr, Amount::from_raw(dust));
    }
}

/// Deterministic post-execution settlement for a block: governance
/// finalization, timelocked proposal execution, inflation-based block
/// reward, and gas-fee split/burn. Every node — producer and peer alike —
/// must run this in an identical way so the resulting `state_root` agrees.
///
/// The supply counters (`total_minted_milli`, `total_burned_milli`) live
/// in `StateDB` and are updated here, so cold-started peers compute the
/// same inflation from persisted state without needing the validator's
/// in-memory atomics.
///
/// `total_staked` must be the stake snapshot from BEFORE any epoch
/// rotation applied during this block, matching the producer path.
pub fn apply_block_rewards_and_governance(
    state: &mut StateDB,
    block_number: u64,
    gas_used: u64,
    gas_config: &GasConfig,
    producer: &Address,
    total_staked: Amount,
    consensus_params: &ConsensusParams,
) -> BlockSettlementSummary {
    // Governance: auto-finalize at the end of each voting period, then
    // run any proposal whose timelock has expired. The order matches
    // vtt-validator::try_produce_block's pre-refactor behavior so the
    // state_root stays byte-identical to what we produced before the
    // consolidation.
    let mut summary = BlockSettlementSummary {
        governance_finalized: finalize_governance_proposals(state, block_number, total_staked),
        governance_executed: execute_queued_proposals(state, block_number),
        ..Default::default()
    };

    // Block reward — inflation-based, 80% producer / 20% treasury.
    let treasury_addr = consensus_params.treasury_address;
    let epoch_length = consensus_params.epoch_length;

    let milli_to_raw = 10u128.pow(15);
    let initial_supply = Amount::from_vtt(INITIAL_SUPPLY_VTT);
    let minted_so_far = Amount::from_raw(state.total_minted_milli() as u128 * milli_to_raw);
    let burned_so_far = Amount::from_raw(state.total_burned_milli() as u128 * milli_to_raw);
    let total_supply = Amount::from_raw(
        initial_supply
            .raw()
            .saturating_add(minted_so_far.raw())
            .saturating_sub(burned_so_far.raw()),
    );
    let staking_ratio_pct = if total_supply.raw() > 0 {
        (total_staked.raw() * 100 / total_supply.raw()) as u64
    } else {
        0
    };
    let epoch_reward = calculate_epoch_reward(total_supply, staking_ratio_pct);
    let per_block_reward = if epoch_length > 0 {
        Amount::from_raw(epoch_reward.raw() / epoch_length as u128)
    } else {
        Amount::ZERO
    };

    if per_block_reward.raw() > 0 {
        let split = split_block_reward(per_block_reward);
        distribute_producer_reward(state, producer, split.producer);
        let _ = state.add_balance(&treasury_addr, split.treasury);
        let minted_milli = (per_block_reward.raw() / milli_to_raw) as u64;
        state.record_mint_milli(minted_milli);
        summary.minted = per_block_reward;
        summary.producer_total = split.producer;
        summary.treasury = split.treasury;
    }

    // Gas fees — 70% burned, 30% to producer (commission-aware).
    let total_gas_fees = Amount::from_raw(gas_used as u128 * gas_config.min_gas_price.raw());
    if total_gas_fees.raw() > 0 {
        let gas_split = split_gas_fees(total_gas_fees);
        distribute_producer_reward(state, producer, gas_split.producer);
        let burned_milli = (gas_split.burned.raw() / milli_to_raw) as u64;
        state.record_burn_milli(burned_milli);
        summary.burned = gas_split.burned;
        summary.producer_total = summary
            .producer_total
            .checked_add(gas_split.producer)
            .unwrap_or(summary.producer_total);
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtt_crypto::Keypair;
    use vtt_primitives::amount::Amount;
    use vtt_primitives::chain::GasConfig;
    use vtt_primitives::transaction::TransactionPayload;
    use vtt_primitives::ChainId;

    fn gas_config() -> GasConfig {
        GasConfig::default()
    }

    fn make_signed_tx(
        keypair: &Keypair,
        nonce: u64,
        action: TransactionAction,
    ) -> SignedTransaction {
        let payload = TransactionPayload {
            chain_id: ChainId::RELAY,
            nonce,
            gas_price: Amount::from_raw(1_000_000_000),
            gas_limit: 100_000,
            action,
        };
        let payload_bytes = borsh::to_vec(&payload).unwrap();
        let sig = keypair.sign(&payload_bytes);
        SignedTransaction {
            payload,
            signature: sig,
            public_key: keypair.public_key(),
        }
    }

    #[test]
    fn execute_transfer_success() {
        let alice_kp = Keypair::from_seed(&[1u8; 32]);
        let bob_addr = Address::from([0x02; 20]);
        let alice_addr = alice_kp.address();

        let mut state = StateDB::new();
        state
            .add_balance(&alice_addr, Amount::from_vtt(1000))
            .unwrap();

        let tx = make_signed_tx(
            &alice_kp,
            0,
            TransactionAction::Transfer {
                to: bob_addr,
                amount: Amount::from_vtt(100),
            },
        );

        let result = execute_transaction(&mut state, &tx, &gas_config());
        assert!(result.receipt.success);
        assert!(result.gas_used > 0);

        assert_eq!(state.get_balance(&bob_addr), Amount::from_vtt(100));
        assert!(state.get_balance(&alice_addr) < Amount::from_vtt(900)); // 1000 - 100 - gas
        assert_eq!(state.get_nonce(&alice_addr), 1);
    }

    #[test]
    fn execute_transfer_insufficient_balance() {
        let alice_kp = Keypair::from_seed(&[1u8; 32]);
        let bob_addr = Address::from([0x02; 20]);
        let alice_addr = alice_kp.address();

        let mut state = StateDB::new();
        state
            .add_balance(&alice_addr, Amount::from_vtt(10))
            .unwrap();

        let tx = make_signed_tx(
            &alice_kp,
            0,
            TransactionAction::Transfer {
                to: bob_addr,
                amount: Amount::from_vtt(100),
            },
        );

        let result = execute_transaction(&mut state, &tx, &gas_config());
        assert!(!result.receipt.success);
        assert_eq!(state.get_balance(&bob_addr), Amount::ZERO);
    }

    #[test]
    fn execute_wrong_nonce_fails() {
        let alice_kp = Keypair::from_seed(&[1u8; 32]);
        let alice_addr = alice_kp.address();

        let mut state = StateDB::new();
        state
            .add_balance(&alice_addr, Amount::from_vtt(1000))
            .unwrap();

        let tx = make_signed_tx(
            &alice_kp,
            5, // wrong nonce, should be 0
            TransactionAction::Transfer {
                to: Address::from([0x02; 20]),
                amount: Amount::from_vtt(10),
            },
        );

        let result = execute_transaction(&mut state, &tx, &gas_config());
        assert!(!result.receipt.success);
    }

    #[test]
    fn execute_invalid_signature_fails() {
        let alice_kp = Keypair::from_seed(&[1u8; 32]);
        let alice_addr = alice_kp.address();

        let mut state = StateDB::new();
        state
            .add_balance(&alice_addr, Amount::from_vtt(1000))
            .unwrap();

        let mut tx = make_signed_tx(
            &alice_kp,
            0,
            TransactionAction::Transfer {
                to: Address::from([0x02; 20]),
                amount: Amount::from_vtt(10),
            },
        );
        // Corrupt the signature
        tx.signature.0[0] ^= 0xFF;

        let result = execute_transaction(&mut state, &tx, &gas_config());
        assert!(!result.receipt.success);
    }

    #[test]
    fn delegation_rejected_for_address_without_self_stake() {
        let ghost_kp = Keypair::from_seed(&[0x99u8; 32]);
        let del_kp = Keypair::from_seed(&[0x02u8; 32]);
        let ghost_addr = ghost_kp.address();
        let del_addr = del_kp.address();

        let mut state = StateDB::new();
        state
            .add_balance(&del_addr, Amount::from_vtt(100_000))
            .unwrap();

        // Attempt to delegate to an address that has never self-staked.
        let tx = make_signed_tx(
            &del_kp,
            0,
            TransactionAction::Stake {
                validator: ghost_addr,
                amount: Amount::from_vtt(50_000),
            },
        );
        let result = execute_transaction(&mut state, &tx, &gas_config());
        assert!(
            !result.receipt.success,
            "delegation to an address without self-stake must fail"
        );
        // The ghost validator must NOT have been registered as a staker.
        assert!(
            state.get_account(&ghost_addr).staking.is_none()
                || state
                    .get_account(&ghost_addr)
                    .staking
                    .as_ref()
                    .map(|s| s.total_stake.is_zero())
                    .unwrap_or(true),
            "ghost validator must not appear in the staking set"
        );
    }

    #[test]
    fn asset_transfer_rejects_non_kyc_when_required() {
        use vtt_state::asset::{AssetClass, AssetRecord, AssetStatus, TransferMode};

        let issuer_kp = Keypair::from_seed(&[0x21u8; 32]);
        let recipient_kp = Keypair::from_seed(&[0x22u8; 32]);
        let issuer_addr = issuer_kp.address();
        let recipient_addr = recipient_kp.address();

        let mut state = StateDB::new();
        state
            .add_balance(&issuer_addr, Amount::from_vtt(10_000))
            .unwrap();

        let asset_id = H256::from([0x44; 32]);
        state
            .register_asset(AssetRecord {
                id: asset_id,
                name: "Regulated Real Estate".into(),
                symbol: "RRE".into(),
                class: AssetClass::RealEstate,
                origin_chain: vtt_primitives::ChainId::RELAY,
                issuer: issuer_addr,
                total_supply: Amount::from_vtt(1_000),
                decimals: 18,
                status: AssetStatus::Active,
                compliance_policy: None,
                valuation_oracle: None,
                documents: Default::default(),
                metadata_uri: String::new(),
                jurisdiction: "IT".into(),
                legal_entity: "Test SPV".into(),
                transfer_mode: TransferMode::PeerToPeer,
                registrar: None,
                redemption_pool: Amount::ZERO,
                requires_kyc: true,
                created_at: 0,
            })
            .unwrap();
        let mut issuer_ownership = state.get_ownership(&asset_id, &issuer_addr);
        issuer_ownership.credit(Amount::from_vtt(1_000));
        state.put_ownership(issuer_ownership);

        let tx = make_signed_tx(
            &issuer_kp,
            0,
            TransactionAction::AssetTransfer {
                asset_id,
                to: recipient_addr,
                amount: Amount::from_vtt(100),
            },
        );
        let result = execute_transaction(&mut state, &tx, &gas_config());
        assert!(
            !result.receipt.success,
            "transfer of KYC-required asset must fail without approvals"
        );

        // Approve both parties on-chain and retry.
        state.set_kyc_approved(&issuer_addr, true);
        state.set_kyc_approved(&recipient_addr, true);
        let tx2 = make_signed_tx(
            &issuer_kp,
            1,
            TransactionAction::AssetTransfer {
                asset_id,
                to: recipient_addr,
                amount: Amount::from_vtt(100),
            },
        );
        let result2 = execute_transaction(&mut state, &tx2, &gas_config());
        assert!(
            result2.receipt.success,
            "transfer must succeed after KYC approvals"
        );
        let recipient_ownership = state.get_ownership(&asset_id, &recipient_addr);
        assert_eq!(recipient_ownership.available, Amount::from_vtt(100));
    }

    #[test]
    fn execute_stake_and_unstake() {
        let val_kp = Keypair::from_seed(&[1u8; 32]);
        let val_addr = val_kp.address();

        let mut state = StateDB::new();
        state
            .add_balance(&val_addr, Amount::from_vtt(200_000))
            .unwrap();

        // Stake
        let tx = make_signed_tx(
            &val_kp,
            0,
            TransactionAction::Stake {
                validator: val_addr,
                amount: Amount::from_vtt(100_000),
            },
        );
        let result = execute_transaction(&mut state, &tx, &gas_config());
        assert!(result.receipt.success);

        let val_account = state.get_account(&val_addr);
        let staking = val_account.staking.unwrap();
        assert_eq!(staking.self_stake, Amount::from_vtt(100_000));
        assert_eq!(staking.total_stake, Amount::from_vtt(100_000));

        // Unstake half
        let tx2 = make_signed_tx(
            &val_kp,
            1,
            TransactionAction::Unstake {
                validator: val_addr,
                amount: Amount::from_vtt(50_000),
            },
        );
        let result2 = execute_transaction(&mut state, &tx2, &gas_config());
        assert!(result2.receipt.success);

        let val_account2 = state.get_account(&val_addr);
        let staking2 = val_account2.staking.unwrap();
        assert_eq!(staking2.self_stake, Amount::from_vtt(50_000));
        assert_eq!(staking2.total_stake, Amount::from_vtt(50_000));
    }

    #[test]
    fn execute_delegation() {
        let val_kp = Keypair::from_seed(&[1u8; 32]);
        let del_kp = Keypair::from_seed(&[2u8; 32]);
        let val_addr = val_kp.address();
        let del_addr = del_kp.address();

        let mut state = StateDB::new();
        state
            .add_balance(&val_addr, Amount::from_vtt(200_000))
            .unwrap();
        state
            .add_balance(&del_addr, Amount::from_vtt(100_000))
            .unwrap();

        // Validator self-stakes
        let tx1 = make_signed_tx(
            &val_kp,
            0,
            TransactionAction::Stake {
                validator: val_addr,
                amount: Amount::from_vtt(100_000),
            },
        );
        execute_transaction(&mut state, &tx1, &gas_config());

        // Delegator stakes to validator
        let tx2 = make_signed_tx(
            &del_kp,
            0,
            TransactionAction::Stake {
                validator: val_addr,
                amount: Amount::from_vtt(50_000),
            },
        );
        let result = execute_transaction(&mut state, &tx2, &gas_config());
        assert!(result.receipt.success);

        let val_account = state.get_account(&val_addr);
        let staking = val_account.staking.unwrap();
        assert_eq!(staking.total_stake, Amount::from_vtt(150_000));
        assert_eq!(staking.self_stake, Amount::from_vtt(100_000));
        assert_eq!(staking.delegations.len(), 1);
        assert_eq!(staking.delegations[0].delegator, del_addr);
        assert_eq!(staking.delegations[0].amount, Amount::from_vtt(50_000));
    }

    #[test]
    fn execute_multiple_transactions() {
        let alice_kp = Keypair::from_seed(&[1u8; 32]);
        let alice_addr = alice_kp.address();
        let bob_addr = Address::from([0x02; 20]);

        let mut state = StateDB::new();
        state
            .add_balance(&alice_addr, Amount::from_vtt(1000))
            .unwrap();

        let txs: Vec<SignedTransaction> = (0..3)
            .map(|i| {
                make_signed_tx(
                    &alice_kp,
                    i,
                    TransactionAction::Transfer {
                        to: bob_addr,
                        amount: Amount::from_vtt(10),
                    },
                )
            })
            .collect();

        let (receipts, total_gas) =
            execute_block_transactions(&mut state, &txs, &gas_config(), 1_000_000);

        assert_eq!(receipts.len(), 3);
        assert!(receipts.iter().all(|r| r.success));
        assert!(total_gas > 0);
        assert_eq!(state.get_balance(&bob_addr), Amount::from_vtt(30));
        assert_eq!(state.get_nonce(&alice_addr), 3);
    }

    #[test]
    fn process_slashing_evidence_reduces_stake() {
        use vtt_consensus::slashing::DoubleSignEvidence;
        use vtt_primitives::block::BlockHeader;
        use vtt_primitives::{Signature, H256 as PH256};
        use vtt_state::account::StakingState;

        let val = Address::from([0x10; 20]);
        let mut state = StateDB::new();
        let mut account = vtt_state::account::AccountState::with_balance(Amount::from_vtt(500_000));
        account.staking = Some(StakingState {
            total_stake: Amount::from_vtt(100_000),
            self_stake: Amount::from_vtt(100_000),
            commission_bps: 500,
            active: true,
            delegations: Vec::new(),
            unbonding: Vec::new(),
        });
        state.put_account(val, account);

        // Create valid double-sign evidence
        let header_a = BlockHeader {
            version: 1,
            chain_id: ChainId::RELAY,
            number: 1,
            parent_hash: PH256::from([1u8; 32]),
            transactions_root: PH256::ZERO,
            state_root: PH256::ZERO,
            receipts_root: PH256::ZERO,
            validator: val,
            epoch: 0,
            slot: 0,
            timestamp: 1_700_000_000_000,
            gas_limit: 10_000_000,
            gas_used: 0,
            cross_chain_root: None,
            signature: Signature::ZERO,
        };
        let mut header_b = header_a.clone();
        header_b.number = 2; // different block = different signable bytes
        header_b.parent_hash = PH256::from([2u8; 32]);

        let evidence = vec![DoubleSignEvidence { header_a, header_b }];

        let slashed = process_slashing_evidence(&mut state, &evidence, 500, 0);
        assert_eq!(slashed.len(), 1);
        assert_eq!(slashed[0].0, val);
        assert_eq!(slashed[0].1, Amount::from_vtt(5_000)); // 5% of 100k

        let after = state.get_account(&val);
        let staking = after.staking.unwrap();
        assert_eq!(staking.total_stake, Amount::from_vtt(95_000));
    }

    #[test]
    fn process_slashing_evidence_invalid_evidence_skipped() {
        use vtt_consensus::slashing::DoubleSignEvidence;
        use vtt_primitives::block::BlockHeader;
        use vtt_primitives::{Signature, H256 as PH256};

        let mut state = StateDB::new();

        // Same block = invalid evidence (identical signable bytes)
        let header = BlockHeader {
            version: 1,
            chain_id: ChainId::RELAY,
            number: 1,
            parent_hash: PH256::from([1u8; 32]),
            transactions_root: PH256::ZERO,
            state_root: PH256::ZERO,
            receipts_root: PH256::ZERO,
            validator: Address::from([0x10; 20]),
            epoch: 0,
            slot: 0,
            timestamp: 1_700_000_000_000,
            gas_limit: 10_000_000,
            gas_used: 0,
            cross_chain_root: None,
            signature: Signature::ZERO,
        };

        let evidence = vec![DoubleSignEvidence {
            header_a: header.clone(),
            header_b: header,
        }];

        let slashed = process_slashing_evidence(&mut state, &evidence, 500, 0);
        assert!(slashed.is_empty());
    }

    #[test]
    fn validate_parameter_change_unknown_key_rejected() {
        let err = validate_parameter_change("foo_bar", "1").unwrap_err();
        match err {
            ExecutionError::Custom(msg) => assert!(msg.contains("unknown parameter")),
            _ => panic!("expected Custom error for unknown key"),
        }
    }

    #[test]
    fn validate_parameter_change_bad_address_rejected() {
        let err = validate_parameter_change("bridge_relayer", "not-an-address").unwrap_err();
        match err {
            ExecutionError::Custom(msg) => assert!(msg.contains("hex address")),
            _ => panic!("expected Custom error for bad address"),
        }
    }

    #[test]
    fn validate_parameter_change_bps_out_of_range() {
        let err = validate_parameter_change("slash_double_sign_bps", "10001").unwrap_err();
        match err {
            ExecutionError::Custom(msg) => assert!(msg.contains("<= 10000")),
            _ => panic!("expected Custom error for out-of-range bps"),
        }
    }

    #[test]
    fn validate_parameter_change_threshold_pct_out_of_range() {
        let err = validate_parameter_change("downtime_threshold_pct", "101").unwrap_err();
        match err {
            ExecutionError::Custom(msg) => assert!(msg.contains("<= 100")),
            _ => panic!("expected Custom error for out-of-range pct"),
        }
    }

    #[test]
    fn validate_parameter_change_accepts_each_whitelisted_key() {
        validate_parameter_change(
            "bridge_relayer",
            "0x1111111111111111111111111111111111111111",
        )
        .unwrap();
        validate_parameter_change(
            "treasury_address",
            "2222222222222222222222222222222222222222",
        )
        .unwrap();
        validate_parameter_change("min_gas_price", "1000000000").unwrap();
        validate_parameter_change("base_transfer_cost", "21000").unwrap();
        validate_parameter_change("cost_per_byte", "16").unwrap();
        validate_parameter_change("slash_double_sign_bps", "500").unwrap();
        validate_parameter_change("slash_downtime_bps", "10").unwrap();
        validate_parameter_change("downtime_threshold_pct", "50").unwrap();
        validate_parameter_change("unbonding_period_secs", "1814400").unwrap();
    }

    #[test]
    fn apply_parameter_change_persists_override() {
        use vtt_storage::memory::InMemoryStore;
        let storage = std::sync::Arc::new(InMemoryStore::new());
        let mut state = StateDB::with_storage(storage);
        let pid = H256::ZERO;

        apply_parameter_change(&mut state, "min_gas_price", "2000000000", &pid);
        assert_eq!(
            state.get_min_gas_price_override(),
            Some(Amount::from_raw(2_000_000_000))
        );

        apply_parameter_change(&mut state, "slash_double_sign_bps", "750", &pid);
        assert_eq!(state.get_slash_double_sign_bps_override(), Some(750));

        apply_parameter_change(&mut state, "downtime_threshold_pct", "80", &pid);
        assert_eq!(state.get_downtime_threshold_pct_override(), Some(80));
    }

    #[test]
    fn validate_parameter_change_compliance_keys() {
        validate_parameter_change("max_holders_per_asset", "100").unwrap();
        validate_parameter_change("jurisdiction_whitelist", "IT,DE,FR").unwrap();
        validate_parameter_change("jurisdiction_blacklist", "KP,IR").unwrap();
        let err = validate_parameter_change("jurisdiction_whitelist", "ITA").unwrap_err();
        match err {
            ExecutionError::Custom(msg) => assert!(msg.contains("alpha-2")),
            _ => panic!("expected Custom error for bad country code"),
        }
    }

    #[test]
    fn check_jurisdiction_policy_without_policy_is_allowed() {
        use vtt_storage::memory::InMemoryStore;
        let storage = std::sync::Arc::new(InMemoryStore::new());
        let state = StateDB::with_storage(storage);
        let addr = Address::from([0x11; 20]);
        assert!(check_jurisdiction_policy(&state, &addr).is_ok());
    }

    #[test]
    fn check_jurisdiction_policy_whitelist_enforced() {
        use vtt_storage::memory::InMemoryStore;
        let storage = std::sync::Arc::new(InMemoryStore::new());
        let state = StateDB::with_storage(storage);
        state.set_jurisdiction_whitelist("IT,DE");

        let it_addr = Address::from([0x11; 20]);
        state.set_address_jurisdiction(&it_addr, "IT");
        assert!(check_jurisdiction_policy(&state, &it_addr).is_ok());

        let us_addr = Address::from([0x22; 20]);
        state.set_address_jurisdiction(&us_addr, "US");
        assert!(check_jurisdiction_policy(&state, &us_addr).is_err());

        // Address with no jurisdiction set, when whitelist is active, is rejected
        let unknown = Address::from([0x33; 20]);
        assert!(check_jurisdiction_policy(&state, &unknown).is_err());
    }

    #[test]
    fn check_jurisdiction_policy_blacklist_enforced() {
        use vtt_storage::memory::InMemoryStore;
        let storage = std::sync::Arc::new(InMemoryStore::new());
        let state = StateDB::with_storage(storage);
        state.set_jurisdiction_blacklist("KP,IR");

        let kp_addr = Address::from([0x44; 20]);
        state.set_address_jurisdiction(&kp_addr, "KP");
        assert!(check_jurisdiction_policy(&state, &kp_addr).is_err());

        // Address with no jurisdiction + only blacklist = allowed (unrestricted)
        let unknown = Address::from([0x55; 20]);
        assert!(check_jurisdiction_policy(&state, &unknown).is_ok());
    }

    #[test]
    fn set_address_jurisdiction_requires_treasury() {
        use vtt_storage::memory::InMemoryStore;
        let storage = std::sync::Arc::new(InMemoryStore::new());
        let mut state = StateDB::with_storage(storage);
        let treasury = Address::from([0xAA; 20]);
        state.set_treasury_address(treasury);

        let target = Address::from([0xBB; 20]);
        let impostor = Address::from([0xCC; 20]);

        let err = execute_set_address_jurisdiction(&mut state, &impostor, &target, "IT")
            .expect_err("non-treasury must fail");
        match err {
            ExecutionError::Custom(msg) => assert!(msg.contains("treasury")),
            _ => panic!("expected Custom error"),
        }

        execute_set_address_jurisdiction(&mut state, &treasury, &target, "IT").unwrap();
        assert_eq!(
            state.get_address_jurisdiction(&target).as_deref(),
            Some("IT")
        );

        // Clear by passing empty string
        execute_set_address_jurisdiction(&mut state, &treasury, &target, "").unwrap();
        assert_eq!(state.get_address_jurisdiction(&target), None);
    }

    #[test]
    fn finalize_redemption_sweeps_pool_to_treasury() {
        use std::collections::BTreeMap;
        use vtt_state::asset::{AssetClass, AssetRecord, AssetStatus, TransferMode};

        let mut state = StateDB::new();
        let treasury = Address::from([0xAA; 20]);
        state.set_treasury_address(treasury);

        let asset_id = H256::from([0x77; 32]);
        let issuer = Address::from([0x22; 20]);
        let asset = AssetRecord {
            id: asset_id,
            name: "Sellable".to_string(),
            symbol: "SELL".to_string(),
            class: AssetClass::RealEstate,
            origin_chain: vtt_primitives::ChainId::RELAY,
            issuer,
            total_supply: Amount::from_vtt(1_000_000),
            decimals: 0,
            status: AssetStatus::RedemptionPending,
            compliance_policy: None,
            valuation_oracle: None,
            documents: BTreeMap::new(),
            metadata_uri: String::new(),
            jurisdiction: "IT".to_string(),
            legal_entity: "SPV".to_string(),
            transfer_mode: TransferMode::PeerToPeer,
            registrar: None,
            redemption_pool: Amount::from_vtt(5_000),
            requires_kyc: false,
            created_at: 0,
        };
        state.register_asset(asset).unwrap();

        execute_finalize_redemption(&mut state, &asset_id).unwrap();

        let finalized = state.get_asset(&asset_id).cloned().unwrap();
        assert_eq!(finalized.status, AssetStatus::Redeemed);
        assert_eq!(finalized.redemption_pool, Amount::ZERO);
        assert_eq!(state.get_balance(&treasury), Amount::from_vtt(5_000));
    }

    #[test]
    fn finalize_redemption_rejects_non_pending_asset() {
        use std::collections::BTreeMap;
        use vtt_state::asset::{AssetClass, AssetRecord, AssetStatus, TransferMode};

        let mut state = StateDB::new();
        let asset_id = H256::from([0x88; 32]);
        let asset = AssetRecord {
            id: asset_id,
            name: "Active".to_string(),
            symbol: "ACT".to_string(),
            class: AssetClass::Equity,
            origin_chain: vtt_primitives::ChainId::RELAY,
            issuer: Address::from([0x22; 20]),
            total_supply: Amount::from_vtt(1_000),
            decimals: 0,
            status: AssetStatus::Active,
            compliance_policy: None,
            valuation_oracle: None,
            documents: BTreeMap::new(),
            metadata_uri: String::new(),
            jurisdiction: "IT".to_string(),
            legal_entity: "X".to_string(),
            transfer_mode: TransferMode::PeerToPeer,
            registrar: None,
            redemption_pool: Amount::ZERO,
            requires_kyc: false,
            created_at: 0,
        };
        state.register_asset(asset).unwrap();

        let err = execute_finalize_redemption(&mut state, &asset_id).expect_err("must fail");
        match err {
            ExecutionError::Custom(msg) => assert!(msg.contains("RedemptionPending")),
            _ => panic!("expected Custom error"),
        }
    }

    #[test]
    fn create_oracle_feed_requires_treasury() {
        use vtt_storage::memory::InMemoryStore;
        let storage = std::sync::Arc::new(InMemoryStore::new());
        let mut state = StateDB::with_storage(storage);
        let treasury = Address::from([0xAA; 20]);
        state.set_treasury_address(treasury);

        let feed_id = H256::from([0xDE; 32]);
        let sources = vec![Address::from([0x01; 20]), Address::from([0x02; 20])];
        let impostor = Address::from([0xCC; 20]);
        let err = execute_create_oracle_feed(
            &mut state,
            &impostor,
            feed_id,
            "BTC/USD",
            "price:BTC/USD",
            &sources,
            2,
            60_000,
            8,
            0,
        )
        .expect_err("non-treasury must fail");
        match err {
            ExecutionError::Custom(msg) => assert!(msg.contains("treasury")),
            _ => panic!("expected Custom error"),
        }

        execute_create_oracle_feed(
            &mut state,
            &treasury,
            feed_id,
            "BTC/USD",
            "price:BTC/USD",
            &sources,
            2,
            60_000,
            8,
            7,
        )
        .unwrap();
        let feed = state.get_oracle(&feed_id).unwrap();
        assert_eq!(feed.decimals, 8);
    }

    #[test]
    fn submit_oracle_value_reaches_quorum() {
        use vtt_storage::memory::InMemoryStore;
        let storage = std::sync::Arc::new(InMemoryStore::new());
        let mut state = StateDB::with_storage(storage);
        let treasury = Address::from([0xAA; 20]);
        state.set_treasury_address(treasury);

        let feed_id = H256::from([0xDE; 32]);
        let src_a = Address::from([0xA0; 20]);
        let src_b = Address::from([0xB0; 20]);
        let sources = vec![src_a, src_b];
        execute_create_oracle_feed(
            &mut state,
            &treasury,
            feed_id,
            "BTC/USD",
            "price:BTC/USD",
            &sources,
            2,
            60_000,
            18,
            0,
        )
        .unwrap();

        execute_submit_oracle_value(&mut state, &src_a, &feed_id, Amount::from_vtt(60_000), 1000)
            .unwrap();
        // Quorum not reached yet
        assert!(state.get_oracle(&feed_id).unwrap().latest_value.is_none());

        execute_submit_oracle_value(&mut state, &src_b, &feed_id, Amount::from_vtt(62_000), 2000)
            .unwrap();
        // Now quorum reached; median of [60k, 62k] = 62k
        assert_eq!(
            state.get_oracle(&feed_id).unwrap().latest_value,
            Some(Amount::from_vtt(62_000))
        );
    }

    #[test]
    fn submit_oracle_value_rejects_unauthorized() {
        use vtt_storage::memory::InMemoryStore;
        let storage = std::sync::Arc::new(InMemoryStore::new());
        let mut state = StateDB::with_storage(storage);
        let treasury = Address::from([0xAA; 20]);
        state.set_treasury_address(treasury);

        let feed_id = H256::from([0xDE; 32]);
        let sources = vec![Address::from([0xA0; 20])];
        execute_create_oracle_feed(
            &mut state, &treasury, feed_id, "Custom", "custom", &sources, 1, 0, 0, 0,
        )
        .unwrap();

        let impostor = Address::from([0xFF; 20]);
        let err = execute_submit_oracle_value(
            &mut state,
            &impostor,
            &feed_id,
            Amount::from_vtt(100),
            1000,
        )
        .expect_err("unauthorized source must fail");
        match err {
            ExecutionError::Custom(msg) => assert!(msg.contains("authorized")),
            _ => panic!("expected Custom error"),
        }
    }

    #[test]
    fn set_address_jurisdiction_rejects_bad_code() {
        use vtt_storage::memory::InMemoryStore;
        let storage = std::sync::Arc::new(InMemoryStore::new());
        let mut state = StateDB::with_storage(storage);
        let treasury = Address::from([0xAA; 20]);
        state.set_treasury_address(treasury);

        let target = Address::from([0xBB; 20]);
        let err = execute_set_address_jurisdiction(&mut state, &treasury, &target, "ITA")
            .expect_err("3-letter code must fail");
        match err {
            ExecutionError::Custom(msg) => assert!(msg.contains("alpha-2")),
            _ => panic!("expected Custom error"),
        }
    }
}
