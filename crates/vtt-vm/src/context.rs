use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use vtt_primitives::amount::Amount;
use vtt_primitives::transaction::Log;
use vtt_primitives::{Address, BlockNumber, ChainId, Timestamp};

use crate::gas::GasMeter;

/// Execution context shared between the VM and host functions.
/// Provides contract storage, caller info, and block context.
#[derive(Clone)]
pub struct ExecutionContext {
    /// The address of the contract being executed.
    pub contract_address: Address,
    /// The address that called this contract (msg.sender).
    pub caller: Address,
    /// The original transaction sender (tx.origin).
    pub origin: Address,
    /// VTT value sent with the call.
    pub value: Amount,
    /// Current block number.
    pub block_number: BlockNumber,
    /// Current block timestamp (ms).
    pub block_timestamp: Timestamp,
    /// Chain ID.
    pub chain_id: ChainId,
    /// Gas meter.
    pub gas: GasMeter,
    /// Contract storage (key -> value).
    pub storage: Arc<Mutex<HashMap<Vec<u8>, Vec<u8>>>>,
    /// Logs emitted during execution.
    pub logs: Arc<Mutex<Vec<Log>>>,
    /// Balance changes to apply after execution (address -> delta).
    pub balance_changes: Arc<Mutex<Vec<BalanceChange>>>,
    /// WASM linear memory reference, set after instantiation.
    pub wasm_memory: Arc<Mutex<Option<wasmer::Memory>>>,
}

/// A pending balance change from contract execution.
#[derive(Clone, Debug)]
pub struct BalanceChange {
    pub address: Address,
    pub amount: Amount,
    pub is_credit: bool,
}

/// Parameters for creating an execution context.
pub struct ExecutionParams {
    pub contract_address: Address,
    pub caller: Address,
    pub origin: Address,
    pub value: Amount,
    pub block_number: BlockNumber,
    pub block_timestamp: Timestamp,
    pub chain_id: ChainId,
    pub gas_limit: u64,
}

impl ExecutionContext {
    pub fn new(params: ExecutionParams) -> Self {
        Self {
            contract_address: params.contract_address,
            caller: params.caller,
            origin: params.origin,
            value: params.value,
            block_number: params.block_number,
            block_timestamp: params.block_timestamp,
            chain_id: params.chain_id,
            gas: GasMeter::new(params.gas_limit),
            storage: Arc::new(Mutex::new(HashMap::new())),
            logs: Arc::new(Mutex::new(Vec::new())),
            balance_changes: Arc::new(Mutex::new(Vec::new())),
            wasm_memory: Arc::new(Mutex::new(None)),
        }
    }

    /// Read from contract storage.
    pub fn storage_read(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.storage.lock().unwrap().get(key).cloned()
    }

    /// Write to contract storage.
    pub fn storage_write(&self, key: Vec<u8>, value: Vec<u8>) {
        self.storage.lock().unwrap().insert(key, value);
    }

    /// Delete from contract storage.
    pub fn storage_delete(&self, key: &[u8]) {
        self.storage.lock().unwrap().remove(key);
    }

    /// Emit a log.
    pub fn emit_log(&self, log: Log) {
        self.logs.lock().unwrap().push(log);
    }

    /// Get all emitted logs.
    pub fn take_logs(&self) -> Vec<Log> {
        std::mem::take(&mut *self.logs.lock().unwrap())
    }

    /// Record a balance change.
    pub fn record_transfer(&self, to: Address, amount: Amount) {
        let mut changes = self.balance_changes.lock().unwrap();
        // Debit from contract
        changes.push(BalanceChange {
            address: self.contract_address,
            amount,
            is_credit: false,
        });
        // Credit to recipient
        changes.push(BalanceChange {
            address: to,
            amount,
            is_credit: true,
        });
    }

    /// Get all pending balance changes.
    pub fn take_balance_changes(&self) -> Vec<BalanceChange> {
        std::mem::take(&mut *self.balance_changes.lock().unwrap())
    }

    /// Set the WASM linear memory reference (called after instantiation).
    pub fn set_memory(&self, memory: wasmer::Memory) {
        *self.wasm_memory.lock().unwrap() = Some(memory);
    }

    /// Get gas used.
    pub fn gas_used(&self) -> u64 {
        self.gas.used()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> ExecutionContext {
        ExecutionContext::new(ExecutionParams {
            contract_address: Address::from([0x10; 20]),
            caller: Address::from([0x01; 20]),
            origin: Address::from([0x01; 20]),
            value: Amount::ZERO,
            block_number: 100,
            block_timestamp: 1_700_000_300_000,
            chain_id: ChainId::RELAY,
            gas_limit: 1_000_000,
        })
    }

    #[test]
    fn storage_read_write() {
        let ctx = test_context();
        assert!(ctx.storage_read(b"key").is_none());

        ctx.storage_write(b"key".to_vec(), b"value".to_vec());
        assert_eq!(ctx.storage_read(b"key"), Some(b"value".to_vec()));

        ctx.storage_delete(b"key");
        assert!(ctx.storage_read(b"key").is_none());
    }

    #[test]
    fn emit_and_take_logs() {
        let ctx = test_context();
        ctx.emit_log(Log {
            address: ctx.contract_address,
            topics: vec![],
            data: b"test".to_vec(),
        });

        let logs = ctx.take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].data, b"test");

        // Logs are consumed
        assert!(ctx.take_logs().is_empty());
    }

    #[test]
    fn record_transfer() {
        let ctx = test_context();
        let recipient = Address::from([0x02; 20]);
        ctx.record_transfer(recipient, Amount::from_vtt(100));

        let changes = ctx.take_balance_changes();
        assert_eq!(changes.len(), 2);
        assert!(!changes[0].is_credit); // debit from contract
        assert!(changes[1].is_credit); // credit to recipient
    }
}
