use wasmer::{imports, Function, FunctionEnv, FunctionEnvMut, Instance, Module, Store, Value};

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

        // Write args to contract memory if the contract has an `args` export
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

        Ok(ExecutionResult {
            status,
            gas_used,
            return_data: Vec::new(),
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

// --- Host Functions ---
// These are callable from within WASM contracts via the `env` import namespace.

fn host_storage_read(env: FunctionEnvMut<ExecutionContext>, key_ptr: i32, key_len: i32) -> i32 {
    let ctx = env.data().clone();
    if !ctx.gas.consume(GasCosts::STORAGE_READ) {
        return -1;
    }

    let memory = env.data().storage.lock().unwrap();
    let key = vec![0u8; key_len as usize]; // simplified: in production, read from wasm memory
    let _ = (key_ptr, key); // placeholder
    if memory.is_empty() {
        0 // not found
    } else {
        1 // found
    }
}

fn host_storage_write(
    env: FunctionEnvMut<ExecutionContext>,
    _key_ptr: i32,
    _key_len: i32,
    _val_ptr: i32,
    _val_len: i32,
) {
    let ctx = env.data().clone();
    let _ = ctx.gas.consume(GasCosts::STORAGE_WRITE);
}

fn host_caller(env: FunctionEnvMut<ExecutionContext>) -> i64 {
    let ctx = env.data();
    let _ = ctx.gas.consume(GasCosts::HOST_CALL_BASE);
    // Return first 8 bytes of caller address as i64 (simplified)
    let bytes = &ctx.caller.0[..8];
    i64::from_le_bytes(bytes.try_into().unwrap())
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

fn host_emit_log(env: FunctionEnvMut<ExecutionContext>, _data_ptr: i32, _data_len: i32) {
    let ctx = env.data().clone();
    let _ = ctx.gas.consume(GasCosts::LOG_BASE);
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
}
