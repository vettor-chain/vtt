//! Open Compliance Policy Contract for VTT
//!
//! A permissionless compliance policy that allows all transfers.
//! Used for chains that don't require KYC/AML.

extern "C" {
    fn host_consume_gas(amount: i64) -> i32;
}

/// Check if a transfer is allowed. Always returns 0 (allowed).
#[no_mangle]
pub extern "C" fn check_transfer() -> i32 {
    unsafe {
        host_consume_gas(100);
    }
    0 // always allowed
}

/// Check if an address can hold assets. Always returns 0 (allowed).
#[no_mangle]
pub extern "C" fn can_hold() -> i32 {
    0 // always allowed
}

/// Main entry point.
#[no_mangle]
pub extern "C" fn call() -> i32 {
    0
}
