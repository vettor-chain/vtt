use wasmer::{imports, Function, FunctionEnv, FunctionEnvMut, Instance, Module, Store, Value};

use vtt_primitives::transaction::Log;
use vtt_primitives::H256;

use crate::context::ExecutionContext;
use crate::error::VmError;
use crate::gas::GasCosts;

/// Result of executing a contract.
#[derive(Debug)]
pub struct ExecutionResult {
    /// Return value (0 = success, non-zero = revert).
    pub status: i32,
    /// Gas used.
    pub gas_used: u64,
    /// Return data from the contract.
    pub return_data: Vec<u8>,
}

/// The VTT WASM virtual machine engine.
pub struct VmEngine {
    store: Store,
}

impl VmEngine {
    pub fn new() -> Self {
        Self {
            store: Store::default(),
        }
    }

    /// Compile WASM bytecode into a module.
    pub fn compile(&self, bytecode: &[u8]) -> Result<Vec<u8>, VmError> {
        // Validate the bytecode by attempting compilation
        Module::new(&self.store, bytecode).map_err(|e| VmError::Compilation(e.to_string()))?;
        // Return the raw bytecode (wasmer can cache compiled modules internally)
        Ok(bytecode.to_vec())
    }

    /// Execute a contract method.
    pub fn execute(
        &mut self,
        bytecode: &[u8],
        method: &str,
        args: &[u8],
        context: ExecutionContext,
    ) -> Result<ExecutionResult, VmError> {
        let module =
            Module::new(&self.store, bytecode).map_err(|e| VmError::Compilation(e.to_string()))?;

        let env = FunctionEnv::new(&mut self.store, context.clone());

        // Define host functions available to the contract
        let import_object = imports! {
            "env" => {
                "host_storage_read" => Function::new_typed_with_env(
                    &mut self.store, &env, host_storage_read
                ),
                "host_storage_write" => Function::new_typed_with_env(
                    &mut self.store, &env, host_storage_write
                ),
                "host_caller" => Function::new_typed_with_env(
                    &mut self.store, &env, host_caller
                ),
                "host_caller_address" => Function::new_typed_with_env(
                    &mut self.store, &env, host_caller_address
                ),
                "host_block_number" => Function::new_typed_with_env(
                    &mut self.store, &env, host_block_number
                ),
                "host_block_timestamp" => Function::new_typed_with_env(
                    &mut self.store, &env, host_block_timestamp
                ),
                "host_chain_id" => Function::new_typed_with_env(
                    &mut self.store, &env, host_chain_id
                ),
                "host_emit_log" => Function::new_typed_with_env(
                    &mut self.store, &env, host_emit_log
                ),
                "host_gas_remaining" => Function::new_typed_with_env(
                    &mut self.store, &env, host_gas_remaining
                ),
                "host_consume_gas" => Function::new_typed_with_env(
                    &mut self.store, &env, host_consume_gas
                ),
            },
        };

        let instance = Instance::new(&mut self.store, &module, &import_object)
            .map_err(|e| VmError::Instantiation(e.to_string()))?;

        // Set WASM memory on the context so host functions can access it
        if let Ok(memory) = instance.exports.get_memory("memory") {
            let ctx = env.as_mut(&mut self.store);
            ctx.set_memory(memory.clone());
            // Also set on the original context clone for post-execution reads
            context.set_memory(memory.clone());
        }

        // Write args to contract memory if the contract exports memory
        if let Ok(memory) = instance.exports.get_memory("memory") {
            if !args.is_empty() {
                let view = memory.view(&self.store);
                // Write args length at offset 0, then args at offset 8
                let len_bytes = (args.len() as u64).to_le_bytes();
                view.write(0, &len_bytes)
                    .map_err(|e| VmError::MemoryAccess(e.to_string()))?;
                view.write(8, args)
                    .map_err(|e| VmError::MemoryAccess(e.to_string()))?;
            }
        }

        // Call the method
        let func = instance
            .exports
            .get_function(method)
            .map_err(|_| VmError::ExportNotFound(method.to_string()))?;

        let result = func
            .call(&mut self.store, &[])
            .map_err(|e| VmError::Runtime(e.to_string()))?;

        let status = match result.first() {
            Some(Value::I32(v)) => *v,
            _ => 0,
        };

        let gas_used = context.gas_used();

        // Read return data from WASM memory if the contract returned a positive length
        let return_data = if status > 0 {
            if let Ok(memory) = instance.exports.get_memory("memory") {
                let view = memory.view(&self.store);
                let len = status as usize;
                let mut buf = vec![0u8; len];
                // Convention: return data is written at offset 0 in memory
                if view.read(0, &mut buf).is_ok() {
                    buf
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        Ok(ExecutionResult {
            status,
            gas_used,
            return_data,
        })
    }

    /// Compute the code hash of a contract's bytecode.
    pub fn code_hash(bytecode: &[u8]) -> H256 {
        vtt_crypto::blake3_hash(bytecode)
    }
}

impl Default for VmEngine {
    fn default() -> Self {
        Self::new()
    }
}

// --- Helper: read bytes from WASM linear memory ---

/// Read `len` bytes from WASM memory starting at `offset`.
fn read_wasm_memory(
    env: &FunctionEnvMut<ExecutionContext>,
    offset: u32,
    len: u32,
) -> Result<Vec<u8>, ()> {
    let ctx = env.data();
    let mem_guard = ctx.wasm_memory.lock().unwrap();
    let memory = mem_guard.as_ref().ok_or(())?;
    let view = memory.view(&env);
    let mut buf = vec![0u8; len as usize];
    view.read(offset as u64, &mut buf).map_err(|_| ())?;
    Ok(buf)
}

/// Write bytes to WASM memory at `offset`.
fn write_wasm_memory(
    env: &FunctionEnvMut<ExecutionContext>,
    offset: u32,
    data: &[u8],
) -> Result<(), ()> {
    let ctx = env.data();
    let mem_guard = ctx.wasm_memory.lock().unwrap();
    let memory = mem_guard.as_ref().ok_or(())?;
    let view = memory.view(&env);
    view.write(offset as u64, data).map_err(|_| ())?;
    Ok(())
}

// --- Host Functions ---
// These are callable from within WASM contracts via the `env` import namespace.

/// Read a value from contract storage.
///
/// Signature: `host_storage_read(key_ptr, key_len, val_ptr, val_max) -> i32`
/// - Reads the key from WASM memory at `[key_ptr..key_ptr+key_len]`
/// - Looks up the key in contract storage
/// - If found, writes the value to `[val_ptr..val_ptr+val_max]`
/// - Returns: bytes written on success, 0 if not found, -1 on error (buffer too small / OOG)
fn host_storage_read(
    env: FunctionEnvMut<ExecutionContext>,
    key_ptr: i32,
    key_len: i32,
    val_ptr: i32,
    val_max: i32,
) -> i32 {
    let ctx = env.data().clone();
    if !ctx.gas.consume(GasCosts::STORAGE_READ) {
        return -1; // out of gas
    }

    // Read key from WASM memory
    let key = match read_wasm_memory(&env, key_ptr as u32, key_len as u32) {
        Ok(k) => k,
        Err(()) => return -1,
    };

    // Look up in context storage
    let storage = ctx.storage.lock().unwrap();
    match storage.get(&key) {
        None => 0, // not found
        Some(value) => {
            let val_max = val_max as usize;
            if value.len() > val_max {
                return -1; // buffer too small
            }
            // Write value to WASM memory
            match write_wasm_memory(&env, val_ptr as u32, value) {
                Ok(()) => value.len() as i32,
                Err(()) => -1,
            }
        }
    }
}

/// Write a key-value pair to contract storage.
///
/// Reads key from `[key_ptr..key_ptr+key_len]` and value from `[val_ptr..val_ptr+val_len]`.
/// Charges STORAGE_WRITE_NEW if the key is new, STORAGE_WRITE if existing.
fn host_storage_write(
    env: FunctionEnvMut<ExecutionContext>,
    key_ptr: i32,
    key_len: i32,
    val_ptr: i32,
    val_len: i32,
) {
    let ctx = env.data().clone();

    // Read key from WASM memory
    let key = match read_wasm_memory(&env, key_ptr as u32, key_len as u32) {
        Ok(k) => k,
        Err(()) => return,
    };

    // Read value from WASM memory
    let value = match read_wasm_memory(&env, val_ptr as u32, val_len as u32) {
        Ok(v) => v,
        Err(()) => return,
    };

    // Charge gas: more for new keys, less for overwrites
    let is_new = {
        let storage = ctx.storage.lock().unwrap();
        !storage.contains_key(&key)
    };

    let gas_cost = if is_new {
        GasCosts::STORAGE_WRITE_NEW
    } else {
        GasCosts::STORAGE_WRITE
    };

    if !ctx.gas.consume(gas_cost) {
        return; // out of gas
    }

    // Write to context storage
    ctx.storage_write(key, value);
}

/// Emit a log event from the contract.
///
/// Reads log data from `[data_ptr..data_ptr+data_len]`.
fn host_emit_log(env: FunctionEnvMut<ExecutionContext>, data_ptr: i32, data_len: i32) {
    let ctx = env.data().clone();
    if !ctx.gas.consume(GasCosts::LOG_BASE + GasCosts::LOG_PER_BYTE * data_len as u64) {
        return; // out of gas
    }

    // Read log data from WASM memory
    let data = match read_wasm_memory(&env, data_ptr as u32, data_len as u32) {
        Ok(d) => d,
        Err(()) => return,
    };

    // Emit a log with the contract address, no topics, and the raw data
    ctx.emit_log(Log {
        address: ctx.contract_address,
        topics: vec![],
        data,
    });
}

/// Return first 8 bytes of caller address as i64 (kept for backward compatibility).
fn host_caller(env: FunctionEnvMut<ExecutionContext>) -> i64 {
    let ctx = env.data();
    let _ = ctx.gas.consume(GasCosts::HOST_CALL_BASE);
    // Return first 8 bytes of caller address as i64
    let bytes = &ctx.caller.0[..8];
    i64::from_le_bytes(bytes.try_into().unwrap())
}

/// Write the full 20-byte caller address to WASM memory at `out_ptr`.
///
/// Returns 0 on success, -1 on error.
fn host_caller_address(env: FunctionEnvMut<ExecutionContext>, out_ptr: i32) -> i32 {
    let ctx = env.data().clone();
    if !ctx.gas.consume(GasCosts::HOST_CALL_BASE) {
        return -1;
    }

    match write_wasm_memory(&env, out_ptr as u32, &ctx.caller.0) {
        Ok(()) => 0,
        Err(()) => -1,
    }
}

fn host_block_number(env: FunctionEnvMut<ExecutionContext>) -> i64 {
    let ctx = env.data();
    let _ = ctx.gas.consume(GasCosts::HOST_CALL_BASE);
    ctx.block_number as i64
}

fn host_block_timestamp(env: FunctionEnvMut<ExecutionContext>) -> i64 {
    let ctx = env.data();
    let _ = ctx.gas.consume(GasCosts::HOST_CALL_BASE);
    ctx.block_timestamp as i64
}

fn host_chain_id(env: FunctionEnvMut<ExecutionContext>) -> i32 {
    let ctx = env.data();
    let _ = ctx.gas.consume(GasCosts::HOST_CALL_BASE);
    ctx.chain_id.0 as i32
}

fn host_gas_remaining(env: FunctionEnvMut<ExecutionContext>) -> i64 {
    env.data().gas.remaining() as i64
}

fn host_consume_gas(env: FunctionEnvMut<ExecutionContext>, amount: i64) -> i32 {
    if env.data().gas.consume(amount as u64) {
        0 // ok
    } else {
        -1 // out of gas
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtt_primitives::amount::Amount;
    use vtt_primitives::{Address, ChainId};

    fn test_context() -> ExecutionContext {
        ExecutionContext::new(crate::context::ExecutionParams {
            contract_address: Address::from([0x10; 20]),
            caller: Address::from([0x01; 20]),
            origin: Address::from([0x01; 20]),
            value: Amount::ZERO,
            block_number: 42,
            block_timestamp: 1_700_000_000_000,
            chain_id: ChainId::RELAY,
            gas_limit: 1_000_000,
        })
    }

    // Minimal WASM module that exports a function returning 0 (success)
    fn minimal_wasm() -> Vec<u8> {
        // WAT: (module (func (export "call") (result i32) i32.const 0))
        wat::parse_str(r#"(module (func (export "call") (result i32) i32.const 0))"#)
            .expect("WAT should be valid")
    }

    // WASM that returns 42
    fn wasm_return_42() -> Vec<u8> {
        wat::parse_str(r#"(module (func (export "call") (result i32) i32.const 42))"#)
            .expect("WAT should be valid")
    }

    // WASM that calls host_block_number
    fn wasm_with_host_call() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "host_block_number" (func $host_block_number (result i64)))
                (func (export "call") (result i32)
                    call $host_block_number
                    drop
                    i32.const 0
                )
            )"#,
        )
        .expect("WAT should be valid")
    }

    /// WASM module that writes a key-value pair to storage, then reads it back.
    ///
    /// Memory layout:
    ///   [64..68)   = key "test" (4 bytes)
    ///   [72..79)   = value "hello!!" (7 bytes)
    ///   [128..256) = read-back output buffer (128 bytes)
    ///
    /// Steps:
    ///   1. Call host_storage_write(key_ptr=64, key_len=4, val_ptr=72, val_len=7)
    ///   2. Call host_storage_read(key_ptr=64, key_len=4, out_ptr=128, out_max=128)
    ///   3. Return the result of host_storage_read (should be 7 = bytes written)
    fn wasm_storage_roundtrip() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "host_storage_write"
                    (func $host_storage_write (param i32 i32 i32 i32)))
                (import "env" "host_storage_read"
                    (func $host_storage_read (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)

                ;; key = "test" at offset 64
                (data (i32.const 64) "test")
                ;; value = "hello!!" at offset 72
                (data (i32.const 72) "hello!!")

                (func (export "call") (result i32)
                    ;; write key="test" (4 bytes at 64), value="hello!!" (7 bytes at 72)
                    (call $host_storage_write (i32.const 64) (i32.const 4) (i32.const 72) (i32.const 7))
                    ;; read key="test" into buffer at 128 (max 128 bytes)
                    (call $host_storage_read (i32.const 64) (i32.const 4) (i32.const 128) (i32.const 128))
                    ;; return value is bytes written (should be 7)
                )
            )"#,
        )
        .expect("WAT should be valid")
    }

    /// WASM module that emits a log event with data "EVENT".
    ///
    /// Memory layout:
    ///   [64..69) = "EVENT" (5 bytes)
    fn wasm_emit_log() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "host_emit_log"
                    (func $host_emit_log (param i32 i32)))
                (memory (export "memory") 1)

                ;; log data = "EVENT" at offset 64
                (data (i32.const 64) "EVENT")

                (func (export "call") (result i32)
                    (call $host_emit_log (i32.const 64) (i32.const 5))
                    i32.const 0
                )
            )"#,
        )
        .expect("WAT should be valid")
    }

    /// WASM module that calls host_caller_address and returns the first byte of the address.
    ///
    /// Memory layout:
    ///   [128..148) = 20-byte output buffer for caller address
    fn wasm_caller_address() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "host_caller_address"
                    (func $host_caller_address (param i32) (result i32)))
                (memory (export "memory") 1)

                (func (export "call") (result i32)
                    ;; write caller address to offset 128
                    (call $host_caller_address (i32.const 128))
                    drop
                    ;; load the first byte of the address and return it
                    (i32.load8_u (i32.const 128))
                )
            )"#,
        )
        .expect("WAT should be valid")
    }

    /// WASM module that reads from a key that does not exist in storage.
    /// Should return 0 (not found).
    fn wasm_storage_read_missing() -> Vec<u8> {
        wat::parse_str(
            r#"(module
                (import "env" "host_storage_read"
                    (func $host_storage_read (param i32 i32 i32 i32) (result i32)))
                (memory (export "memory") 1)

                ;; key = "nope" at offset 64
                (data (i32.const 64) "nope")

                (func (export "call") (result i32)
                    ;; try to read key="nope", should return 0
                    (call $host_storage_read (i32.const 64) (i32.const 4) (i32.const 128) (i32.const 128))
                )
            )"#,
        )
        .expect("WAT should be valid")
    }

    #[test]
    fn compile_valid_wasm() {
        let engine = VmEngine::new();
        let bytecode = minimal_wasm();
        assert!(engine.compile(&bytecode).is_ok());
    }

    #[test]
    fn compile_invalid_wasm() {
        let engine = VmEngine::new();
        assert!(engine.compile(b"not wasm").is_err());
    }

    #[test]
    fn execute_minimal_contract() {
        let mut engine = VmEngine::new();
        let bytecode = minimal_wasm();
        let ctx = test_context();

        let result = engine.execute(&bytecode, "call", &[], ctx).unwrap();
        assert_eq!(result.status, 0);
    }

    #[test]
    fn execute_returns_value() {
        let mut engine = VmEngine::new();
        let bytecode = wasm_return_42();
        let ctx = test_context();

        let result = engine.execute(&bytecode, "call", &[], ctx).unwrap();
        assert_eq!(result.status, 42);
    }

    #[test]
    fn execute_with_host_call() {
        let mut engine = VmEngine::new();
        let bytecode = wasm_with_host_call();
        let ctx = test_context();

        let result = engine.execute(&bytecode, "call", &[], ctx).unwrap();
        assert_eq!(result.status, 0);
        assert!(result.gas_used > 0); // host call consumed gas
    }

    #[test]
    fn execute_nonexistent_method() {
        let mut engine = VmEngine::new();
        let bytecode = minimal_wasm();
        let ctx = test_context();

        let result = engine.execute(&bytecode, "nonexistent", &[], ctx);
        assert!(matches!(result, Err(VmError::ExportNotFound(_))));
    }

    #[test]
    fn code_hash_deterministic() {
        let bytecode = minimal_wasm();
        let h1 = VmEngine::code_hash(&bytecode);
        let h2 = VmEngine::code_hash(&bytecode);
        assert_eq!(h1, h2);
        assert_ne!(h1, H256::ZERO);
    }

    #[test]
    fn storage_write_and_read_roundtrip() {
        let mut engine = VmEngine::new();
        let bytecode = wasm_storage_roundtrip();
        let ctx = test_context();

        let result = engine.execute(&bytecode, "call", &[], ctx.clone()).unwrap();
        // host_storage_read should return 7 (bytes written for "hello!!")
        assert_eq!(result.status, 7);

        // Verify the value is actually in context storage
        let stored = ctx.storage_read(b"test");
        assert_eq!(stored, Some(b"hello!!".to_vec()));
    }

    #[test]
    fn storage_read_missing_key_returns_zero() {
        let mut engine = VmEngine::new();
        let bytecode = wasm_storage_read_missing();
        let ctx = test_context();

        let result = engine.execute(&bytecode, "call", &[], ctx).unwrap();
        assert_eq!(result.status, 0); // not found
    }

    #[test]
    fn emit_log_writes_to_context() {
        let mut engine = VmEngine::new();
        let bytecode = wasm_emit_log();
        let ctx = test_context();

        let result = engine.execute(&bytecode, "call", &[], ctx.clone()).unwrap();
        assert_eq!(result.status, 0);

        let logs = ctx.take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].data, b"EVENT");
        assert_eq!(logs[0].address, Address::from([0x10; 20]));
        assert!(logs[0].topics.is_empty());
    }

    #[test]
    fn caller_address_writes_full_address() {
        let mut engine = VmEngine::new();
        let bytecode = wasm_caller_address();
        let ctx = test_context();

        let result = engine.execute(&bytecode, "call", &[], ctx).unwrap();
        // The caller is Address([0x01; 20]), first byte is 0x01
        assert_eq!(result.status, 0x01);
    }

    #[test]
    fn storage_write_charges_correct_gas() {
        let mut engine = VmEngine::new();
        let bytecode = wasm_storage_roundtrip();
        let ctx = test_context();

        let result = engine.execute(&bytecode, "call", &[], ctx).unwrap();
        // Should have charged: STORAGE_WRITE_NEW (20000) for first write + STORAGE_READ (200) for read
        assert!(result.gas_used >= GasCosts::STORAGE_WRITE_NEW + GasCosts::STORAGE_READ);
    }
}
