use thiserror::Error;
use tracing::debug;

use vtt_crypto::{blake3_hash, verify};
use vtt_primitives::amount::Amount;
use vtt_primitives::chain::GasConfig;
use vtt_primitives::transaction::{Log, SignedTransaction, TransactionAction, TransactionReceipt};
use vtt_primitives::{Address, ChainId, H256};
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
    let mut receipts = Vec::with_capacity(transactions.len());
    let mut total_gas = 0u64;

    for tx in transactions {
        if total_gas >= block_gas_limit {
            break;
        }

        let result = execute_transaction(state, tx, gas_config);
        total_gas += result.gas_used;
        receipts.push(result.receipt);
    }

    (receipts, total_gas)
}

/// Execute a single signed transaction.
pub fn execute_transaction(
    state: &mut StateDB,
    tx: &SignedTransaction,
    gas_config: &GasConfig,
) -> ExecutionResult {
    let tx_hash = blake3_hash(&borsh::to_vec(&tx.payload).unwrap());

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
    let exec_result = execute_action(state, &sender, &tx.payload.action);

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
            execute_unstake(state, sender, validator, *amount)?;
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"Unstake")],
                data: borsh::to_vec(&(*sender, *validator, *amount)).unwrap(),
            }])
        }

        TransactionAction::GovernanceVote { proposal_id, vote } => {
            // Governance votes are recorded as logs. Full governance logic
            // will be implemented in a later phase.
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"GovernanceVote"), *proposal_id],
                data: borsh::to_vec(vote).unwrap(),
            }])
        }

        TransactionAction::DeployContract { code, init_data: _ } => {
            execute_deploy_contract(state, sender, code)
        }

        TransactionAction::CallContract {
            contract,
            method,
            args,
            value,
        } => execute_call_contract(state, sender, contract, method, args, *value),

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
            let pool = vtt_dex::liquidity::create_pool(
                state, sender, *token_a, *token_b, *amount_a, *amount_b, 0, // TODO: pass current epoch
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
            // Treasury address hardcoded for now — should come from chain config
            let treasury = Address::ZERO; // TODO: configure via genesis
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
            // Mining state would need to be loaded from storage
            // For now, emit log — full mining integration in a follow-up
            Ok(vec![Log {
                address: *sender,
                topics: vec![blake3_hash(b"ClaimMiningRewards"), *pool_id],
                data: vec![],
            }])
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

/// Execute an unstaking operation.
fn execute_unstake(
    state: &mut StateDB,
    sender: &Address,
    validator: &Address,
    amount: Amount,
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

    // Return VTT to sender (in production this goes through unbonding period)
    state.add_balance(sender, amount)?;

    Ok(())
}

/// Execute contract deployment.
fn execute_deploy_contract(
    state: &mut StateDB,
    sender: &Address,
    code: &[u8],
) -> Result<Vec<Log>, ExecutionError> {
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

    let mut engine = VmEngine::new();
    let ctx = ExecutionContext::new(ExecutionParams {
        contract_address: *contract,
        caller: *sender,
        origin: *sender,
        value,
        block_number: 0, // TODO: pass actual block number
        block_timestamp: 0,
        chain_id: vtt_primitives::ChainId::RELAY,
        gas_limit: 1_000_000,
    });

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
    }
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
