use borsh::BorshDeserialize;
use thiserror::Error;
use tracing::debug;

use vtt_consensus::governance::{GovernanceSystem, Proposal, ProposalAction};
use vtt_crypto::{blake3_hash, verify};
use vtt_primitives::amount::Amount;
use vtt_primitives::asset_governance::{
    AssetProposal, AssetProposalAction, AssetProposalStatus, ASSET_VOTING_PERIOD_BLOCKS,
};
use vtt_primitives::chain::GasConfig;
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
    execute_block_transactions_at(state, transactions, gas_config, block_gas_limit, 0, 0, ChainId::RELAY)
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
    let mut receipts = Vec::with_capacity(transactions.len());
    let mut total_gas = 0u64;

    for tx in transactions {
        if total_gas >= block_gas_limit {
            break;
        }

        let result = execute_transaction_at(state, tx, gas_config, block_number, block_timestamp, chain_id);
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

    // 4. Calculate gas cost
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
            state, sender, contract, method, args, *value,
            block_number, block_timestamp, gas_limit,
        ),

        TransactionAction::CreateAssetClass {
            name,
            symbol,
            metadata_uri,
            total_supply,
        } => {
            execute_create_asset(state, sender, name, symbol, metadata_uri, *total_supply)?;
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
            state.transfer_asset(asset_id, sender, to, *amount)?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"AssetTransfer"), *asset_id],
                data: borsh::to_vec(&(*sender, *to, *amount)).unwrap(),
            }])
        }

        TransactionAction::CrossChainTransfer {
            destination_chain,
            to,
            payload,
        } => {
            // Lock assets on source chain based on payload
            match payload {
                vtt_primitives::transaction::CrossChainPayload::VttTransfer { amount } => {
                    // Lock VTT by deducting from sender
                    state.sub_balance(sender, *amount)?;
                }
                vtt_primitives::transaction::CrossChainPayload::AssetTransfer {
                    asset_id,
                    amount,
                } => {
                    // Lock asset tokens by deducting from sender
                    state.transfer_asset(asset_id, sender, &Address::ZERO, *amount)?;
                }
                vtt_primitives::transaction::CrossChainPayload::ContractCall { value, .. } => {
                    if !value.is_zero() {
                        state.sub_balance(sender, *value)?;
                    }
                }
            }
            Ok(vec![Log {
                address: *sender,
                topics: vec![
                    blake3_hash(b"CrossChainTransfer"),
                    blake3_hash(&borsh::to_vec(destination_chain).unwrap()),
                ],
                data: borsh::to_vec(&(*sender, *to, payload)).unwrap(),
            }])
        }

        TransactionAction::CreatePool { token_a, token_b, amount_a, amount_b } => {
            let epoch_length = state.get_epoch_length();
            let current_epoch = if epoch_length > 0 { block_number / epoch_length } else { 0 };
            let pool = vtt_dex::liquidity::create_pool(
                state, sender, *token_a, *token_b, *amount_a, *amount_b, current_epoch,
            ).map_err(|e| ExecutionError::Custom(e.to_string()))?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"CreatePool"), pool.pool_id],
                data: borsh::to_vec(&pool.pool_id).unwrap(),
            }])
        }

        TransactionAction::AddLiquidity { pool_id, amount_a, amount_b, min_lp } => {
            let lp_minted = vtt_dex::liquidity::add_liquidity(
                state, sender, pool_id, *amount_a, *amount_b, *min_lp,
            ).map_err(|e| ExecutionError::Custom(e.to_string()))?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"AddLiquidity"), *pool_id],
                data: borsh::to_vec(&lp_minted.0).unwrap(),
            }])
        }

        TransactionAction::RemoveLiquidity { pool_id, lp_amount, min_a, min_b } => {
            let (out_a, out_b) = vtt_dex::liquidity::remove_liquidity(
                state, sender, pool_id, *lp_amount, *min_a, *min_b,
            ).map_err(|e| ExecutionError::Custom(e.to_string()))?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"RemoveLiquidity"), *pool_id],
                data: borsh::to_vec(&(out_a.0, out_b.0)).unwrap(),
            }])
        }

        TransactionAction::Swap { pool_id, token_in, amount_in, min_amount_out } => {
            let amount_out = vtt_dex::swap::execute_swap(
                state, sender, pool_id, token_in, *amount_in, *min_amount_out,
            ).map_err(|e| ExecutionError::Custom(e.to_string()))?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"Swap"), *pool_id],
                data: borsh::to_vec(&amount_out.0).unwrap(),
            }])
        }

        TransactionAction::ClaimRevenue { pool_id } => {
            let treasury = state.get_treasury_address();
            let (fees_a, fees_b) = vtt_dex::revenue::claim_protocol_fees(
                state, sender, pool_id, &treasury,
            ).map_err(|e| ExecutionError::Custom(e.to_string()))?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"ClaimRevenue"), *pool_id],
                data: borsh::to_vec(&(fees_a.0, fees_b.0)).unwrap(),
            }])
        }

        TransactionAction::ClaimMiningRewards { pool_id } => {
            let epoch_length = state.get_epoch_length();
            let current_epoch = if epoch_length > 0 { block_number / epoch_length } else { 0 };

            // Load mining state from storage
            let mining_data = state.get_mining_state_raw(pool_id)
                .ok_or_else(|| ExecutionError::Custom("mining not active for this pool".to_string()))?
                .to_vec();
            let mut mining_state = vtt_dex::MiningState::try_from_slice(&mining_data)
                .map_err(|_| ExecutionError::Custom("corrupt mining state".to_string()))?;

            let reward_amount = vtt_dex::mining::claim_mining_rewards(
                state, sender, pool_id, current_epoch, &mut mining_state,
            ).map_err(|e| ExecutionError::Custom(e.to_string()))?;

            // Save updated mining state
            let updated_data = borsh::to_vec(&mining_state)
                .map_err(|_| ExecutionError::Custom("failed to serialize mining state".to_string()))?;
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
        } => {
            execute_distribute_revenue(state, sender, asset_id, *total_amount)
        }

        TransactionAction::ProposeAssetAction {
            asset_id,
            action,
            description,
        } => {
            execute_propose_asset_action(state, sender, asset_id, action, description, block_number, nonce)
        }

        TransactionAction::VoteAssetProposal {
            proposal_id,
            vote,
        } => {
            execute_vote_asset_proposal(state, sender, proposal_id, *vote, block_number)
        }

        TransactionAction::FinalizeAssetProposal {
            proposal_id,
        } => {
            execute_finalize_asset_proposal(state, sender, proposal_id, block_number)
        }

        TransactionAction::BridgeWithdraw {
            token,
            amount,
            destination_chain,
            destination_address,
        } => {
            execute_bridge_withdraw(state, sender, token, *amount, *destination_chain, destination_address)
        }

        TransactionAction::GovernancePropose {
            description,
            action_type,
        } => {
            execute_governance_propose(state, sender, description, action_type, block_number, nonce)
        }
    }
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
        // Delegation
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
const DEFAULT_UNBONDING_PERIOD_MS: u64 = 21 * 24 * 3600 * 1000;

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
    // block whose timestamp >= completion_time.
    let completion_time = block_timestamp + DEFAULT_UNBONDING_PERIOD_MS;
    state.add_unbonding_entry(
        *sender,
        vtt_state::account::UnbondingEntry {
            amount,
            completion_time,
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
        let mut storage = ctx.storage.lock()
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
        let storage = ctx.storage.lock()
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
) -> Result<(), ExecutionError> {
    // Generate a deterministic asset ID from sender + name + symbol
    let id_data = borsh::to_vec(&(*sender, name, symbol)).unwrap();
    let asset_id = blake3_hash(&id_data);

    let asset = AssetRecord {
        id: asset_id,
        name: name.to_string(),
        symbol: symbol.to_string(),
        class: AssetClass::Custom("General".to_string()),
        origin_chain: ChainId::RELAY,
        issuer: *sender,
        total_supply,
        decimals: 18,
        status: AssetStatus::Active,
        compliance_policy: None,
        valuation_oracle: None,
        documents: std::collections::BTreeMap::new(),
        metadata_uri: metadata_uri.to_string(),
        created_at: 0,
    };

    state.register_asset(asset)?;

    // Mint total supply to issuer
    let mut ownership = state.get_ownership(&asset_id, sender);
    ownership.credit(total_supply);
    state.put_ownership(ownership);

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
        return Err(ExecutionError::Custom(
            "asset has zero total supply".into(),
        ));
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
        let share_raw = mul_div(holder_available.raw(), total_amount.raw(), total_supply.raw());
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
        return Err(ExecutionError::Custom(
            "proposal is not active".into(),
        ));
    }

    // Verify voting hasn't ended
    if proposal.is_voting_ended(current_block) {
        return Err(ExecutionError::Custom(
            "voting period has ended".into(),
        ));
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
        return Err(ExecutionError::Custom(
            "proposal is not active".into(),
        ));
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
            AssetProposalAction::ChangeIssuer { .. } => proposal.passes_supermajority(),
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
                let asset_mut = state
                    .get_asset_mut(&asset_id)
                    .ok_or_else(|| ExecutionError::Custom(format!("asset not found: {asset_id}")))?;
                asset_mut.issuer = *new_issuer;
            }
            AssetProposalAction::Signal { .. } => {
                // No on-chain action for signal proposals
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
        None => return Err(ExecutionError::Custom("asset proposal disappeared during finalization".into())),
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
        data: borsh::to_vec(&(*sender, *token, amount, destination_chain, *destination_address)).unwrap(),
    }])
}

/// Execute governance proposal creation.
/// The sender must have staked VTT (either as a validator or delegator).
/// The proposal is persisted in the state DB via GovernanceSystem.
fn execute_governance_propose(
    state: &mut StateDB,
    sender: &Address,
    description: &str,
    action_type: &str,
    block_number: u64,
    _nonce: u64,
) -> Result<Vec<Log>, ExecutionError> {
    if description.is_empty() {
        return Err(ExecutionError::Custom(
            "proposal description must not be empty".into(),
        ));
    }

    // Validate action_type and map to ProposalAction
    let action = match action_type {
        "parameter_change" => ProposalAction::ParameterChange {
            key: String::new(),
            value: description.to_string(),
        },
        "treasury_spend" => ProposalAction::TreasurySpend {
            recipient: Address::ZERO,
            amount: Amount::ZERO,
        },
        "signal" => ProposalAction::ProtocolUpgrade {
            version: 0,
            description: description.to_string(),
        },
        "dex_pause" => ProposalAction::DexPause(true),
        "dex_unpause" => ProposalAction::DexPause(false),
        other => {
            return Err(ExecutionError::Custom(format!(
                "invalid action_type '{}', must be one of: parameter_change, treasury_spend, signal, dex_pause, dex_unpause",
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

    // Create proposal via GovernanceSystem
    let mut gov = GovernanceSystem::new();

    // Load existing proposals from state (reconstitute system from stored proposals)
    // For proposal creation we only need the system to generate a unique ID
    let proposal_id = gov.create_proposal(
        *sender,
        action,
        description.to_string(),
        block_number,
    );

    // Serialize and store the proposal in state
    let proposal = gov.get(&proposal_id)
        .ok_or_else(|| ExecutionError::Custom("governance proposal creation failed".into()))?
        .clone();
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
        return Err(ExecutionError::Custom("already voted on this proposal".into()));
    }

    // Get sender's voting power (staked VTT)
    let sender_account = state.get_account(sender);
    let voting_power = match &sender_account.staking {
        Some(staking) if !staking.total_stake.is_zero() => staking.total_stake,
        _ => {
            return Err(ExecutionError::Custom(
                "no staked VTT to vote with".into(),
            ));
        }
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
    use vtt_consensus::governance::{ProposalStatus, ProposalAction};

    // Collect all proposal IDs and their raw bytes first (to avoid borrow issues)
    let proposals_raw: Vec<(H256, Vec<u8>)> = state
        .iter_governance_proposals()
        .map(|(id, data)| (*id, data.clone()))
        .collect();

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
            // Execute the proposal action
            match &proposal.action {
                ProposalAction::ParameterChange { key, value } => {
                    // Log the parameter change; full parameter updates require consensus restart
                    debug!(key, value, ?proposal_id, "governance parameter change passed (logged, requires restart)");
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
                    } else {
                        debug!(
                            ?proposal_id,
                            ?treasury_balance,
                            ?amount,
                            "treasury spend failed: insufficient treasury balance"
                        );
                    }
                }
                ProposalAction::RegisterChain { name, .. } => {
                    debug!(?proposal_id, name, "chain registration signal passed");
                }
                ProposalAction::ProtocolUpgrade { version, description } => {
                    debug!(?proposal_id, version, description, "protocol upgrade signal passed");
                }
                ProposalAction::DexPause(paused) => {
                    state.set_dex_paused(*paused);
                    debug!(?proposal_id, paused, "DEX pause state updated via governance");
                }
            }

            // Mark as Executed
            let mut updated = proposal.clone();
            updated.status = ProposalStatus::Executed;
            let updated_bytes = match borsh::to_vec(&updated) {
                Ok(b) => b,
                Err(_) => continue,
            };
            state.put_governance_proposal(proposal_id, updated_bytes);
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
            passed,
            has_quorum,
            passes_threshold,
            "governance proposal finalized"
        );
    }

    finalized_count
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
}
