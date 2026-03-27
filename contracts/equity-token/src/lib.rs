//! Equity Token Contract Template for VTT
//!
//! A standard contract for tokenizing company equity (shares).
//! Compile with: `cargo build --target wasm32-unknown-unknown --release`
//!
//! Exports:
//! - `call`: Main entry point, returns 0 on success.
//!
//! Host functions available via "env" import:
//! - host_storage_read(key_ptr, key_len) -> i32
//! - host_storage_write(key_ptr, key_len, val_ptr, val_len)
//! - host_caller() -> i64
//! - host_block_number() -> i64
//! - host_block_timestamp() -> i64
//! - host_chain_id() -> i32
//! - host_emit_log(data_ptr, data_len)
//! - host_gas_remaining() -> i64
//! - host_consume_gas(amount) -> i32

extern "C" {
    fn host_caller() -> i64;
    fn host_block_number() -> i64;
    fn host_emit_log(data_ptr: i32, data_len: i32);
    fn host_consume_gas(amount: i64) -> i32;
}

/// Main entry point for the equity token contract.
/// Returns 0 on success, non-zero on error.
#[no_mangle]
pub extern "C" fn call() -> i32 {
    // Consume base gas
    unsafe {
        if host_consume_gas(1000) != 0 {
            return -1; // out of gas
        }
    }

    // Emit a log event
    let event = b"equity_token_called";
    unsafe {
        host_emit_log(event.as_ptr() as i32, event.len() as i32);
    }

    0 // success
}

/// Query function: returns the block number as the "balance" (placeholder).
#[no_mangle]
pub extern "C" fn get_info() -> i32 {
    unsafe {
        let _block = host_block_number();
        let _caller = host_caller();
    }
    0
}
