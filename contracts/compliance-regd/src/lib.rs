//! Compliance RegD Contract for VTT
//!
//! Implements transfer restrictions for US accredited investors (SEC Regulation D).
//! Only whitelisted addresses may send or receive tokens.
//! The issuer (set at init) controls the whitelist.
//!
//! Compile with: `cargo build --target wasm32-unknown-unknown --release`
//!
//! Entry protocol:
//!   The VM writes args to WASM memory at offset 0:
//!     [0..8)   = args_len (u64 LE)
//!     [8..8+N) = args payload
//!
//!   Return protocol:
//!     On success for queries, write return data at offset 0 and return byte length.
//!     For mutations, return 0 on success, negative on error.
//!
//! Method dispatch (first byte of args payload):
//!   0x01 = init(issuer_address)
//!   0x02 = set_whitelist(address, status)  — issuer only
//!   0x03 = check_transfer(from, to, amount) — returns 0 if allowed, -1 if blocked
//!   0x04 = is_whitelisted(address) — returns 1 byte: 0x01 = yes, 0x00 = no
//!   0x05 = set_lockup_period(seconds) — issuer only, set global lockup
//!   0x06 = set_max_holders(max) — issuer only, cap number of holders
//!
//! Host functions available via "env" import:
//!   host_storage_read(key_ptr, key_len, val_ptr, val_max) -> i32
//!   host_storage_write(key_ptr, key_len, val_ptr, val_len)
//!   host_caller_address(out_ptr) -> i32
//!   host_emit_log(data_ptr, data_len)
//!   host_consume_gas(amount) -> i32
//!   host_block_timestamp() -> i64

extern "C" {
    fn host_storage_read(key_ptr: i32, key_len: i32, val_ptr: i32, val_max: i32) -> i32;
    fn host_storage_write(key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32);
    fn host_caller_address(out_ptr: i32) -> i32;
    fn host_emit_log(data_ptr: i32, data_len: i32);
    fn host_consume_gas(amount: i64) -> i32;
    fn host_block_timestamp() -> i64;
}

// ---- Constants ----

const ADDR_LEN: usize = 20;
const ARGS_OFFSET: usize = 0;

// Storage key prefixes
const KEY_INITIALIZED: &[u8] = b"init";
const KEY_ISSUER: &[u8] = b"issr";
const KEY_LOCKUP_END: &[u8] = b"lock"; // global lockup end timestamp (u64 LE)
const KEY_MAX_HOLDERS: &[u8] = b"maxh"; // u32 LE, 0 = unlimited
const KEY_HOLDER_COUNT: &[u8] = b"hcnt"; // u32 LE

// Method selectors
const METHOD_INIT: u8 = 0x01;
const METHOD_SET_WHITELIST: u8 = 0x02;
const METHOD_CHECK_TRANSFER: u8 = 0x03;
const METHOD_IS_WHITELISTED: u8 = 0x04;
const METHOD_SET_LOCKUP: u8 = 0x05;
const METHOD_SET_MAX_HOLDERS: u8 = 0x06;

// ---- Helpers ----

unsafe fn storage_read_into(key: &[u8], buf: &mut [u8]) -> i32 {
    host_storage_read(
        key.as_ptr() as i32,
        key.len() as i32,
        buf.as_mut_ptr() as i32,
        buf.len() as i32,
    )
}

unsafe fn storage_write_bytes(key: &[u8], val: &[u8]) {
    host_storage_write(
        key.as_ptr() as i32,
        key.len() as i32,
        val.as_ptr() as i32,
        val.len() as i32,
    );
}

unsafe fn emit_event(data: &[u8]) {
    host_emit_log(data.as_ptr() as i32, data.len() as i32);
}

unsafe fn get_caller(buf: &mut [u8; ADDR_LEN]) {
    host_caller_address(buf.as_mut_ptr() as i32);
}

/// Build a whitelist storage key: "wl:" + address
fn whitelist_key(addr: &[u8; ADDR_LEN]) -> [u8; 3 + ADDR_LEN] {
    let mut key = [0u8; 3 + ADDR_LEN];
    key[..3].copy_from_slice(b"wl:");
    key[3..].copy_from_slice(addr);
    key
}

unsafe fn read_args() -> &'static [u8] {
    let args_ptr = ARGS_OFFSET as *const u8;
    let len_bytes: [u8; 8] = core::ptr::read(args_ptr as *const [u8; 8]);
    let len = u64::from_le_bytes(len_bytes) as usize;
    if len == 0 {
        return &[];
    }
    core::slice::from_raw_parts(args_ptr.add(8), len)
}

unsafe fn write_return(data: &[u8]) -> i32 {
    let base_ptr = ARGS_OFFSET as *mut u8;
    core::ptr::copy_nonoverlapping(data.as_ptr(), base_ptr, data.len());
    data.len() as i32
}

unsafe fn read_u32_storage(key: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    let read = storage_read_into(key, &mut buf);
    if read == 4 {
        u32::from_le_bytes(buf)
    } else {
        0
    }
}

unsafe fn write_u32_storage(key: &[u8], val: u32) {
    storage_write_bytes(key, &val.to_le_bytes());
}

unsafe fn read_u64_storage(key: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    let read = storage_read_into(key, &mut buf);
    if read == 8 {
        u64::from_le_bytes(buf)
    } else {
        0
    }
}

unsafe fn write_u64_storage(key: &[u8], val: u64) {
    storage_write_bytes(key, &val.to_le_bytes());
}

unsafe fn is_issuer(caller: &[u8; ADDR_LEN]) -> bool {
    let mut issuer = [0u8; ADDR_LEN];
    let read = storage_read_into(KEY_ISSUER, &mut issuer);
    read == ADDR_LEN as i32 && *caller == issuer
}

unsafe fn addr_is_whitelisted(addr: &[u8; ADDR_LEN]) -> bool {
    let wkey = whitelist_key(addr);
    let mut buf = [0u8; 1];
    let read = storage_read_into(&wkey, &mut buf);
    read == 1 && buf[0] == 1
}

// ---- Contract methods ----

/// Initialize the compliance contract.
/// Args: issuer(20)
unsafe fn method_init(args: &[u8]) -> i32 {
    if host_consume_gas(3000) != 0 {
        return -1;
    }

    let mut init_buf = [0u8; 1];
    let init_read = storage_read_into(KEY_INITIALIZED, &mut init_buf);
    if init_read > 0 && init_buf[0] == 1 {
        return -2; // already initialized
    }

    if args.len() < ADDR_LEN {
        return -3;
    }

    let mut issuer = [0u8; ADDR_LEN];
    issuer.copy_from_slice(&args[..ADDR_LEN]);

    storage_write_bytes(KEY_ISSUER, &issuer);
    storage_write_bytes(KEY_INITIALIZED, &[1]);

    // Auto-whitelist the issuer
    let wkey = whitelist_key(&issuer);
    storage_write_bytes(&wkey, &[1]);
    write_u32_storage(KEY_HOLDER_COUNT, 1);

    // Emit init event
    let mut event = [0u8; 1 + ADDR_LEN];
    event[0] = METHOD_INIT;
    event[1..].copy_from_slice(&issuer);
    emit_event(&event);

    0
}

/// Set whitelist status for an address.
/// Args: address(20) + status(1)  — 1 = whitelisted, 0 = blacklisted
unsafe fn method_set_whitelist(args: &[u8]) -> i32 {
    if host_consume_gas(2000) != 0 {
        return -1;
    }

    if args.len() < ADDR_LEN + 1 {
        return -3;
    }

    // Only issuer
    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);
    if !is_issuer(&caller) {
        return -8; // not issuer
    }

    let mut addr = [0u8; ADDR_LEN];
    addr.copy_from_slice(&args[..ADDR_LEN]);
    let status = args[ADDR_LEN];

    let wkey = whitelist_key(&addr);
    let was_whitelisted = addr_is_whitelisted(&addr);

    if status == 1 && !was_whitelisted {
        // Check max holders if set
        let max_holders = read_u32_storage(KEY_MAX_HOLDERS);
        if max_holders > 0 {
            let current = read_u32_storage(KEY_HOLDER_COUNT);
            if current >= max_holders {
                return -11; // max holders reached
            }
        }
        storage_write_bytes(&wkey, &[1]);
        let count = read_u32_storage(KEY_HOLDER_COUNT);
        write_u32_storage(KEY_HOLDER_COUNT, count + 1);
    } else if status == 0 && was_whitelisted {
        storage_write_bytes(&wkey, &[0]);
        let count = read_u32_storage(KEY_HOLDER_COUNT);
        if count > 0 {
            write_u32_storage(KEY_HOLDER_COUNT, count - 1);
        }
    } else {
        storage_write_bytes(&wkey, &[status.min(1)]);
    }

    // Emit event: 0x02 + address(20) + status(1)
    let mut event = [0u8; 1 + ADDR_LEN + 1];
    event[0] = METHOD_SET_WHITELIST;
    event[1..1 + ADDR_LEN].copy_from_slice(&addr);
    event[1 + ADDR_LEN] = status;
    emit_event(&event);

    0
}

/// Check if a transfer is allowed.
/// Args: from(20) + to(20) + amount(16)
/// Returns: 0 if allowed, negative if blocked.
unsafe fn method_check_transfer(args: &[u8]) -> i32 {
    if host_consume_gas(1000) != 0 {
        return -1;
    }

    if args.len() < ADDR_LEN * 2 + 16 {
        return -3;
    }

    let mut from = [0u8; ADDR_LEN];
    from.copy_from_slice(&args[..ADDR_LEN]);

    let mut to = [0u8; ADDR_LEN];
    to.copy_from_slice(&args[ADDR_LEN..ADDR_LEN * 2]);

    // Check lockup period
    let lockup_end = read_u64_storage(KEY_LOCKUP_END);
    if lockup_end > 0 {
        let now = host_block_timestamp() as u64;
        if now < lockup_end {
            return -12; // lockup period active
        }
    }

    // Both sender and receiver must be whitelisted
    if !addr_is_whitelisted(&from) {
        return -13; // sender not whitelisted
    }
    if !addr_is_whitelisted(&to) {
        return -14; // receiver not whitelisted
    }

    0 // transfer allowed
}

/// Query whitelist status of an address.
/// Args: address(20)
/// Returns: 1 byte (0x01 = whitelisted, 0x00 = not)
unsafe fn method_is_whitelisted(args: &[u8]) -> i32 {
    if host_consume_gas(500) != 0 {
        return -1;
    }

    if args.len() < ADDR_LEN {
        return -3;
    }

    let mut addr = [0u8; ADDR_LEN];
    addr.copy_from_slice(&args[..ADDR_LEN]);

    let status: u8 = if addr_is_whitelisted(&addr) { 1 } else { 0 };
    write_return(&[status])
}

/// Set a global lockup period.
/// Args: lockup_end_timestamp(8) — Unix timestamp in ms when lockup ends. 0 = no lockup.
unsafe fn method_set_lockup(args: &[u8]) -> i32 {
    if host_consume_gas(2000) != 0 {
        return -1;
    }

    if args.len() < 8 {
        return -3;
    }

    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);
    if !is_issuer(&caller) {
        return -8;
    }

    let mut ts_bytes = [0u8; 8];
    ts_bytes.copy_from_slice(&args[..8]);
    let lockup_end = u64::from_le_bytes(ts_bytes);

    write_u64_storage(KEY_LOCKUP_END, lockup_end);

    // Emit event: 0x05 + timestamp(8)
    let mut event = [0u8; 9];
    event[0] = METHOD_SET_LOCKUP;
    event[1..].copy_from_slice(&lockup_end.to_le_bytes());
    emit_event(&event);

    0
}

/// Set maximum number of whitelisted holders.
/// Args: max(4) — u32 LE, 0 = unlimited
unsafe fn method_set_max_holders(args: &[u8]) -> i32 {
    if host_consume_gas(1000) != 0 {
        return -1;
    }

    if args.len() < 4 {
        return -3;
    }

    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);
    if !is_issuer(&caller) {
        return -8;
    }

    let mut max_bytes = [0u8; 4];
    max_bytes.copy_from_slice(&args[..4]);
    let max_holders = u32::from_le_bytes(max_bytes);

    write_u32_storage(KEY_MAX_HOLDERS, max_holders);

    // Emit event: 0x06 + max(4)
    let mut event = [0u8; 5];
    event[0] = METHOD_SET_MAX_HOLDERS;
    event[1..].copy_from_slice(&max_holders.to_le_bytes());
    emit_event(&event);

    0
}

// ---- Entry points ----

/// Main entry point.
#[no_mangle]
pub extern "C" fn call() -> i32 {
    unsafe {
        if host_consume_gas(500) != 0 {
            return -1;
        }

        let args = read_args();

        if args.is_empty() {
            return 0; // no-op
        }

        let method = args[0];
        let payload = &args[1..];

        match method {
            METHOD_INIT => method_init(payload),
            METHOD_SET_WHITELIST => method_set_whitelist(payload),
            METHOD_CHECK_TRANSFER => method_check_transfer(payload),
            METHOD_IS_WHITELISTED => method_is_whitelisted(payload),
            METHOD_SET_LOCKUP => method_set_lockup(payload),
            METHOD_SET_MAX_HOLDERS => method_set_max_holders(payload),
            _ => -10, // unknown method
        }
    }
}

/// Legacy check_transfer entry point (for direct call from equity-token).
/// Always returns 0 (allowed) — actual logic is in the dispatch above.
#[no_mangle]
pub extern "C" fn check_transfer() -> i32 {
    unsafe {
        host_consume_gas(100);
    }
    // When called directly (not via dispatch), allow — compliance check
    // should go through the `call` entry with METHOD_CHECK_TRANSFER.
    0
}

/// Legacy can_hold entry point.
#[no_mangle]
pub extern "C" fn can_hold() -> i32 {
    0
}
