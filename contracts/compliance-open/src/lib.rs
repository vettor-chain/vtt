//! Open Compliance Policy Contract for VTT
//!
//! A permissionless compliance policy that allows all transfers.
//! This is the default compliance module for non-regulated assets.
//!
//! All check functions return 0 (allowed) unconditionally.
//!
//! Compile with: `cargo build --target wasm32-unknown-unknown --release`
//!
//! Exports:
//! - `check_transfer`: Check if a transfer is allowed (always 0 = allowed)
//! - `can_hold`: Check if an address can hold assets (always 0 = allowed)
//! - `can_issue`: Check if an address can issue assets (always 0 = allowed)
//! - `call`: Main entry point (always 0 = success)

extern "C" {
    fn host_consume_gas(amount: i64) -> i32;
    fn host_emit_log(data_ptr: i32, data_len: i32);
}

/// Check if a transfer from `from` to `to` of `amount` is allowed.
/// Open compliance: always returns 0 (allowed) with no restrictions.
#[no_mangle]
pub extern "C" fn check_transfer() -> i32 {
    unsafe {
        if host_consume_gas(100) != 0 {
            return -1; // out of gas
        }
    }
    0 // always allowed — no restrictions
}

/// Check if an address can hold assets.
/// Open compliance: always returns 0 (allowed).
#[no_mangle]
pub extern "C" fn can_hold() -> i32 {
    unsafe {
        if host_consume_gas(100) != 0 {
            return -1;
        }
    }
    0 // always allowed
}

/// Check if an address can issue new assets.
/// Open compliance: always returns 0 (allowed).
#[no_mangle]
pub extern "C" fn can_issue() -> i32 {
    unsafe {
        if host_consume_gas(100) != 0 {
            return -1;
        }
    }
    0 // always allowed
}

/// Main entry point. Dispatches to check_transfer by default.
#[no_mangle]
pub extern "C" fn call() -> i32 {
    unsafe {
        if host_consume_gas(100) != 0 {
            return -1;
        }
        let event = b"compliance_open:allowed";
        host_emit_log(event.as_ptr() as i32, event.len() as i32);
    }
    0 // always succeeds
}
