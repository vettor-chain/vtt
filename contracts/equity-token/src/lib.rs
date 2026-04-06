//! Equity Token Contract for VTT
//!
//! A full ERC20-like contract for tokenizing company equity (shares).
//! Compile with: `cargo build --target wasm32-unknown-unknown --release`
//!
//! Exported functions:
//!   call()              — dispatch based on args written to memory at offset 0
//!   get_info()          — return total supply
//!
//! Entry protocol:
//!   The VM writes args to WASM memory at offset 0:
//!     [0..8)   = args_len (u64 LE)
//!     [8..8+N) = args payload (CBOR-like binary encoding, see below)
//!
//!   Return protocol:
//!     On success, write return data at offset 0 and return the byte length.
//!     On error, return a negative i32.
//!
//! Method dispatch (first byte of args payload):
//!   0x01 = init(name, symbol, total_supply, issuer)
//!   0x02 = transfer(to, amount)
//!   0x03 = balance_of(owner)
//!   0x04 = approve(spender, amount)
//!   0x05 = transfer_from(from, to, amount)
//!   0x06 = total_supply()
//!   0x07 = distribute_revenue(total_amount)
//!   0x08 = set_compliance(compliance_addr) — issuer only, enables whitelist enforcement
//!   0x09 = add_to_whitelist(address) — issuer only
//!   0x0A = remove_from_whitelist(address) — issuer only
//!   0x0B = is_whitelisted(address) — returns 1 byte: 0x01 = yes, 0x00 = no
//!
//! Host functions available via "env" import:
//!   host_storage_read(key_ptr, key_len, val_ptr, val_max) -> i32
//!   host_storage_write(key_ptr, key_len, val_ptr, val_len)
//!   host_caller_address(out_ptr) -> i32
//!   host_emit_log(data_ptr, data_len)
//!   host_consume_gas(amount) -> i32
//!   host_block_number() -> i64
//!   host_block_timestamp() -> i64
//!   host_caller() -> i64

// ---- Host function imports ----

extern "C" {
    fn host_storage_read(key_ptr: i32, key_len: i32, val_ptr: i32, val_max: i32) -> i32;
    fn host_storage_write(key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32);
    fn host_caller_address(out_ptr: i32) -> i32;
    fn host_caller() -> i64;
    fn host_emit_log(data_ptr: i32, data_len: i32);
    fn host_consume_gas(amount: i64) -> i32;
    fn host_block_number() -> i64;
    fn host_block_timestamp() -> i64;
}

// ---- Constants ----

const ADDR_LEN: usize = 20;
const U128_LEN: usize = 16;
const MAX_NAME_LEN: usize = 64;
const MAX_SYMBOL_LEN: usize = 16;
/// We reserve the first 1024 bytes of memory for argument I/O.
const ARGS_OFFSET: usize = 0;
/// Scratch space for storage operations starts at 1024.
const SCRATCH: usize = 1024;
/// Secondary scratch space at 2048.
const SCRATCH2: usize = 2048;
/// Output buffer at 4096.
const OUTPUT: usize = 4096;

// Storage key prefixes
const KEY_INITIALIZED: &[u8] = b"init";
const KEY_NAME: &[u8] = b"name";
const KEY_SYMBOL: &[u8] = b"sym";
const KEY_TOTAL_SUPPLY: &[u8] = b"tsup";
const KEY_ISSUER: &[u8] = b"issr";
const KEY_COMPLIANCE: &[u8] = b"comp"; // compliance contract address (optional)
const KEY_DECIMALS: &[u8] = b"dec";
const KEY_HOLDER_COUNT: &[u8] = b"hcnt";

// Method selectors
const METHOD_INIT: u8 = 0x01;
const METHOD_TRANSFER: u8 = 0x02;
const METHOD_BALANCE_OF: u8 = 0x03;
const METHOD_APPROVE: u8 = 0x04;
const METHOD_TRANSFER_FROM: u8 = 0x05;
const METHOD_TOTAL_SUPPLY: u8 = 0x06;
const METHOD_DISTRIBUTE_REVENUE: u8 = 0x07;
const METHOD_SET_COMPLIANCE: u8 = 0x08;
const METHOD_ADD_TO_WHITELIST: u8 = 0x09;
const METHOD_REMOVE_FROM_WHITELIST: u8 = 0x0A;
const METHOD_IS_WHITELISTED: u8 = 0x0B;

// ---- Helper functions ----

unsafe fn storage_read(key: &[u8], buf_offset: usize, buf_max: usize) -> i32 {
    host_storage_read(
        key.as_ptr() as i32,
        key.len() as i32,
        buf_offset as i32,
        buf_max as i32,
    )
}

unsafe fn storage_write(key: &[u8], val: &[u8]) {
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

/// Read a u128 from storage. Returns 0 if key not found.
unsafe fn read_u128(key: &[u8]) -> u128 {
    let mut buf = [0u8; U128_LEN];
    let read = storage_read(key, buf.as_mut_ptr() as usize, U128_LEN);
    if read == U128_LEN as i32 {
        u128::from_le_bytes(buf)
    } else {
        0
    }
}

/// Write a u128 to storage.
unsafe fn write_u128(key: &[u8], val: u128) {
    let bytes = val.to_le_bytes();
    storage_write(key, &bytes);
}

/// Build a balance storage key: "bal:" + 20-byte address
fn balance_key(addr: &[u8; ADDR_LEN]) -> [u8; 4 + ADDR_LEN] {
    let mut key = [0u8; 4 + ADDR_LEN];
    key[..4].copy_from_slice(b"bal:");
    key[4..].copy_from_slice(addr);
    key
}

/// Build a whitelist storage key: "wl:" + 20-byte address
fn whitelist_key(addr: &[u8; ADDR_LEN]) -> [u8; 3 + ADDR_LEN] {
    let mut key = [0u8; 3 + ADDR_LEN];
    key[..3].copy_from_slice(b"wl:");
    key[3..].copy_from_slice(addr);
    key
}

/// Build an allowance storage key: "alw:" + 20-byte owner + 20-byte spender
fn allowance_key(owner: &[u8; ADDR_LEN], spender: &[u8; ADDR_LEN]) -> [u8; 4 + ADDR_LEN * 2] {
    let mut key = [0u8; 4 + ADDR_LEN * 2];
    key[..4].copy_from_slice(b"alw:");
    key[4..4 + ADDR_LEN].copy_from_slice(owner);
    key[4 + ADDR_LEN..].copy_from_slice(spender);
    key
}

/// Build a holder index key: "hld:" + u32 LE index
fn holder_index_key(index: u32) -> [u8; 4 + 4] {
    let mut key = [0u8; 8];
    key[..4].copy_from_slice(b"hld:");
    key[4..].copy_from_slice(&index.to_le_bytes());
    key
}

/// Read args payload from WASM memory (written by the VM at offset 0).
/// Returns a static reference to the args bytes.
unsafe fn read_args() -> &'static [u8] {
    let args_ptr = ARGS_OFFSET as *const u8;
    let len_bytes: [u8; 8] = core::ptr::read(args_ptr as *const [u8; 8]);
    let len = u64::from_le_bytes(len_bytes) as usize;
    if len == 0 {
        return &[];
    }
    core::slice::from_raw_parts(args_ptr.add(8), len)
}

/// Write return data to memory at offset 0 and return its length.
unsafe fn write_return(data: &[u8]) -> i32 {
    let out_ptr = OUTPUT as *mut u8;
    core::ptr::copy_nonoverlapping(data.as_ptr(), out_ptr, data.len());
    // Copy to offset 0 for the VM to read
    let base_ptr = ARGS_OFFSET as *mut u8;
    core::ptr::copy_nonoverlapping(data.as_ptr(), base_ptr, data.len());
    data.len() as i32
}

// ---- Contract methods ----

/// Initialize the token.
/// Args: name_len(1) + name + symbol_len(1) + symbol + total_supply(16) + issuer(20)
unsafe fn method_init(args: &[u8]) -> i32 {
    if host_consume_gas(5000) != 0 {
        return -1;
    }

    // Check not already initialized
    let mut init_buf = [0u8; 1];
    let init_read = storage_read(KEY_INITIALIZED, init_buf.as_mut_ptr() as usize, 1);
    if init_read > 0 && init_buf[0] == 1 {
        return -2; // already initialized
    }

    // Parse args
    let mut offset = 0;
    if args.len() < 1 {
        return -3;
    }

    // name
    let name_len = args[offset] as usize;
    offset += 1;
    if offset + name_len > args.len() || name_len > MAX_NAME_LEN {
        return -3;
    }
    let name = &args[offset..offset + name_len];
    offset += name_len;

    // symbol
    if offset >= args.len() {
        return -3;
    }
    let symbol_len = args[offset] as usize;
    offset += 1;
    if offset + symbol_len > args.len() || symbol_len > MAX_SYMBOL_LEN {
        return -3;
    }
    let symbol = &args[offset..offset + symbol_len];
    offset += symbol_len;

    // total_supply (u128 LE)
    if offset + U128_LEN > args.len() {
        return -3;
    }
    let mut supply_bytes = [0u8; U128_LEN];
    supply_bytes.copy_from_slice(&args[offset..offset + U128_LEN]);
    let total_supply = u128::from_le_bytes(supply_bytes);
    offset += U128_LEN;

    // issuer address (20 bytes)
    if offset + ADDR_LEN > args.len() {
        return -3;
    }
    let mut issuer = [0u8; ADDR_LEN];
    issuer.copy_from_slice(&args[offset..offset + ADDR_LEN]);

    // Store metadata
    storage_write(KEY_NAME, name);
    storage_write(KEY_SYMBOL, symbol);
    write_u128(KEY_TOTAL_SUPPLY, total_supply);
    storage_write(KEY_ISSUER, &issuer);
    storage_write(KEY_DECIMALS, &[18]); // 18 decimals by default

    // Mint total supply to issuer
    let bkey = balance_key(&issuer);
    write_u128(&bkey, total_supply);

    // Track issuer as holder #0
    let hkey = holder_index_key(0);
    storage_write(&hkey, &issuer);
    write_u128(KEY_HOLDER_COUNT, 1);

    // Mark as initialized
    storage_write(KEY_INITIALIZED, &[1]);

    // Emit init event
    let mut event = [0u8; 1 + ADDR_LEN + U128_LEN];
    event[0] = METHOD_INIT;
    event[1..1 + ADDR_LEN].copy_from_slice(&issuer);
    event[1 + ADDR_LEN..].copy_from_slice(&total_supply.to_le_bytes());
    emit_event(&event);

    0 // success
}

/// Transfer tokens to another address.
/// Args: to(20) + amount(16)
unsafe fn method_transfer(args: &[u8]) -> i32 {
    if host_consume_gas(3000) != 0 {
        return -1;
    }

    if args.len() < ADDR_LEN + U128_LEN {
        return -3;
    }

    let mut to = [0u8; ADDR_LEN];
    to.copy_from_slice(&args[..ADDR_LEN]);

    let mut amount_bytes = [0u8; U128_LEN];
    amount_bytes.copy_from_slice(&args[ADDR_LEN..ADDR_LEN + U128_LEN]);
    let amount = u128::from_le_bytes(amount_bytes);

    if amount == 0 {
        return -4; // zero transfer
    }

    // Get caller
    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);

    // Execute the transfer
    do_transfer(&caller, &to, amount)
}

/// Internal transfer logic with compliance check.
unsafe fn do_transfer(from: &[u8; ADDR_LEN], to: &[u8; ADDR_LEN], amount: u128) -> i32 {
    // If compliance is enabled (a compliance address is set), enforce whitelist
    let mut compliance_addr = [0u8; ADDR_LEN];
    let comp_read = storage_read(KEY_COMPLIANCE, compliance_addr.as_mut_ptr() as usize, ADDR_LEN);
    if comp_read == ADDR_LEN as i32 {
        // Compliance is enabled -- both sender and receiver must be whitelisted
        let from_wl = whitelist_key(from);
        let mut from_status = [0u8; 1];
        let fr = storage_read(&from_wl, from_status.as_mut_ptr() as usize, 1);
        if fr != 1 || from_status[0] != 1 {
            return -13; // sender not whitelisted
        }

        let to_wl = whitelist_key(to);
        let mut to_status = [0u8; 1];
        let tr = storage_read(&to_wl, to_status.as_mut_ptr() as usize, 1);
        if tr != 1 || to_status[0] != 1 {
            return -14; // receiver not whitelisted
        }
    }

    // Check balance
    let from_key = balance_key(from);
    let from_balance = read_u128(&from_key);
    if from_balance < amount {
        return -5; // insufficient balance
    }

    // Debit sender
    write_u128(&from_key, from_balance - amount);

    // Credit recipient
    let to_key = balance_key(to);
    let to_balance = read_u128(&to_key);
    let was_zero = to_balance == 0;
    write_u128(&to_key, to_balance + amount);

    // Track new holder if balance was zero
    if was_zero {
        let holder_count = read_u128(KEY_HOLDER_COUNT) as u32;
        let hkey = holder_index_key(holder_count);
        storage_write(&hkey, to);
        write_u128(KEY_HOLDER_COUNT, (holder_count + 1) as u128);
    }

    // Emit transfer event: 0x02 + from(20) + to(20) + amount(16)
    let mut event = [0u8; 1 + ADDR_LEN * 2 + U128_LEN];
    event[0] = METHOD_TRANSFER;
    event[1..1 + ADDR_LEN].copy_from_slice(from);
    event[1 + ADDR_LEN..1 + ADDR_LEN * 2].copy_from_slice(to);
    event[1 + ADDR_LEN * 2..].copy_from_slice(&amount.to_le_bytes());
    emit_event(&event);

    0 // success
}

/// Query balance of an address.
/// Args: owner(20)
/// Returns: u128 LE (16 bytes) written to memory offset 0.
unsafe fn method_balance_of(args: &[u8]) -> i32 {
    if host_consume_gas(500) != 0 {
        return -1;
    }

    if args.len() < ADDR_LEN {
        return -3;
    }

    let mut owner = [0u8; ADDR_LEN];
    owner.copy_from_slice(&args[..ADDR_LEN]);

    let bkey = balance_key(&owner);
    let balance = read_u128(&bkey);

    write_return(&balance.to_le_bytes())
}

/// Approve a spender.
/// Args: spender(20) + amount(16)
unsafe fn method_approve(args: &[u8]) -> i32 {
    if host_consume_gas(2000) != 0 {
        return -1;
    }

    if args.len() < ADDR_LEN + U128_LEN {
        return -3;
    }

    let mut spender = [0u8; ADDR_LEN];
    spender.copy_from_slice(&args[..ADDR_LEN]);

    let mut amount_bytes = [0u8; U128_LEN];
    amount_bytes.copy_from_slice(&args[ADDR_LEN..ADDR_LEN + U128_LEN]);
    let amount = u128::from_le_bytes(amount_bytes);

    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);

    let akey = allowance_key(&caller, &spender);
    write_u128(&akey, amount);

    // Emit approval event: 0x04 + owner(20) + spender(20) + amount(16)
    let mut event = [0u8; 1 + ADDR_LEN * 2 + U128_LEN];
    event[0] = METHOD_APPROVE;
    event[1..1 + ADDR_LEN].copy_from_slice(&caller);
    event[1 + ADDR_LEN..1 + ADDR_LEN * 2].copy_from_slice(&spender);
    event[1 + ADDR_LEN * 2..].copy_from_slice(&amount.to_le_bytes());
    emit_event(&event);

    0
}

/// Transfer tokens from one address to another using allowance.
/// Args: from(20) + to(20) + amount(16)
unsafe fn method_transfer_from(args: &[u8]) -> i32 {
    if host_consume_gas(4000) != 0 {
        return -1;
    }

    if args.len() < ADDR_LEN * 2 + U128_LEN {
        return -3;
    }

    let mut from = [0u8; ADDR_LEN];
    from.copy_from_slice(&args[..ADDR_LEN]);

    let mut to = [0u8; ADDR_LEN];
    to.copy_from_slice(&args[ADDR_LEN..ADDR_LEN * 2]);

    let mut amount_bytes = [0u8; U128_LEN];
    amount_bytes.copy_from_slice(&args[ADDR_LEN * 2..ADDR_LEN * 2 + U128_LEN]);
    let amount = u128::from_le_bytes(amount_bytes);

    if amount == 0 {
        return -4;
    }

    // Check allowance
    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);

    let akey = allowance_key(&from, &caller);
    let allowance = read_u128(&akey);
    if allowance < amount {
        return -6; // insufficient allowance
    }

    // Decrease allowance
    write_u128(&akey, allowance - amount);

    // Execute the transfer
    do_transfer(&from, &to, amount)
}

/// Query total supply.
/// Returns: u128 LE (16 bytes).
unsafe fn method_total_supply() -> i32 {
    if host_consume_gas(200) != 0 {
        return -1;
    }

    let supply = read_u128(KEY_TOTAL_SUPPLY);
    write_return(&supply.to_le_bytes())
}

/// Distribute revenue pro-rata to all holders.
/// Can only be called by the issuer.
/// Args: total_amount(16) — the total VTT revenue to distribute.
///
/// Distribution logic:
///   For each holder: share = (holder_balance * total_amount) / total_supply
///   The holder's balance is credited with `share` new tokens.
///   Total supply increases by total_amount (new minting).
unsafe fn method_distribute_revenue(args: &[u8]) -> i32 {
    if host_consume_gas(5000) != 0 {
        return -1;
    }

    if args.len() < U128_LEN {
        return -3;
    }

    // Only issuer can distribute
    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);

    let mut issuer_buf = [0u8; ADDR_LEN];
    let issuer_read = storage_read(KEY_ISSUER, issuer_buf.as_mut_ptr() as usize, ADDR_LEN);
    if issuer_read != ADDR_LEN as i32 {
        return -7; // not initialized
    }
    if caller != issuer_buf {
        return -8; // not issuer
    }

    let mut amount_bytes = [0u8; U128_LEN];
    amount_bytes.copy_from_slice(&args[..U128_LEN]);
    let total_amount = u128::from_le_bytes(amount_bytes);

    if total_amount == 0 {
        return 0; // nothing to distribute
    }

    let total_supply = read_u128(KEY_TOTAL_SUPPLY);
    if total_supply == 0 {
        return -9; // no supply
    }

    let holder_count = read_u128(KEY_HOLDER_COUNT) as u32;

    // Charge gas proportional to holder count
    if host_consume_gas(holder_count as i64 * 500) != 0 {
        return -1;
    }

    let mut distributed: u128 = 0;

    for i in 0..holder_count {
        let hkey = holder_index_key(i);
        let mut holder_addr = [0u8; ADDR_LEN];
        let read = storage_read(&hkey, holder_addr.as_mut_ptr() as usize, ADDR_LEN);
        if read != ADDR_LEN as i32 {
            continue;
        }

        let bkey = balance_key(&holder_addr);
        let holder_balance = read_u128(&bkey);
        if holder_balance == 0 {
            continue;
        }

        // Pro-rata share: (holder_balance * total_amount) / total_supply
        // Use u128 arithmetic (max ~3.4e38, sufficient for token math)
        let share = (holder_balance as u128)
            .checked_mul(total_amount)
            .and_then(|v| v.checked_div(total_supply));

        if let Some(share) = share {
            if share > 0 {
                write_u128(&bkey, holder_balance + share);
                distributed += share;
            }
        }
    }

    // Increase total supply by the amount actually distributed
    write_u128(KEY_TOTAL_SUPPLY, total_supply + distributed);

    // Emit revenue distribution event: 0x07 + total_amount(16) + distributed(16)
    let mut event = [0u8; 1 + U128_LEN * 2];
    event[0] = METHOD_DISTRIBUTE_REVENUE;
    event[1..1 + U128_LEN].copy_from_slice(&total_amount.to_le_bytes());
    event[1 + U128_LEN..].copy_from_slice(&distributed.to_le_bytes());
    emit_event(&event);

    0
}

/// Set the compliance contract address (issuer only).
/// Args: compliance_address(20)
unsafe fn method_set_compliance(args: &[u8]) -> i32 {
    if host_consume_gas(2000) != 0 {
        return -1;
    }

    if args.len() < ADDR_LEN {
        return -3;
    }

    // Only issuer
    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);

    let mut issuer_buf = [0u8; ADDR_LEN];
    let issuer_read = storage_read(KEY_ISSUER, issuer_buf.as_mut_ptr() as usize, ADDR_LEN);
    if issuer_read != ADDR_LEN as i32 {
        return -7;
    }
    if caller != issuer_buf {
        return -8;
    }

    let mut compliance_addr = [0u8; ADDR_LEN];
    compliance_addr.copy_from_slice(&args[..ADDR_LEN]);
    storage_write(KEY_COMPLIANCE, &compliance_addr);

    // Emit event: 0x08 + address(20)
    let mut event = [0u8; 1 + ADDR_LEN];
    event[0] = METHOD_SET_COMPLIANCE;
    event[1..].copy_from_slice(&compliance_addr);
    emit_event(&event);

    0
}

/// Add an address to the compliance whitelist (issuer only).
/// Args: address(20)
unsafe fn method_add_to_whitelist(args: &[u8]) -> i32 {
    if host_consume_gas(2000) != 0 {
        return -1;
    }

    if args.len() < ADDR_LEN {
        return -3;
    }

    // Only issuer
    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);

    let mut issuer_buf = [0u8; ADDR_LEN];
    let issuer_read = storage_read(KEY_ISSUER, issuer_buf.as_mut_ptr() as usize, ADDR_LEN);
    if issuer_read != ADDR_LEN as i32 {
        return -7;
    }
    if caller != issuer_buf {
        return -8;
    }

    let mut addr = [0u8; ADDR_LEN];
    addr.copy_from_slice(&args[..ADDR_LEN]);

    let wkey = whitelist_key(&addr);
    storage_write(&wkey, &[1]);

    0
}

/// Remove an address from the compliance whitelist (issuer only).
/// Args: address(20)
unsafe fn method_remove_from_whitelist(args: &[u8]) -> i32 {
    if host_consume_gas(2000) != 0 {
        return -1;
    }

    if args.len() < ADDR_LEN {
        return -3;
    }

    // Only issuer
    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);

    let mut issuer_buf = [0u8; ADDR_LEN];
    let issuer_read = storage_read(KEY_ISSUER, issuer_buf.as_mut_ptr() as usize, ADDR_LEN);
    if issuer_read != ADDR_LEN as i32 {
        return -7;
    }
    if caller != issuer_buf {
        return -8;
    }

    let mut addr = [0u8; ADDR_LEN];
    addr.copy_from_slice(&args[..ADDR_LEN]);

    let wkey = whitelist_key(&addr);
    storage_write(&wkey, &[0]);

    0
}

/// Check if an address is whitelisted.
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

    let wkey = whitelist_key(&addr);
    let mut status = [0u8; 1];
    let read = storage_read(&wkey, status.as_mut_ptr() as usize, 1);
    let whitelisted: u8 = if read == 1 && status[0] == 1 { 1 } else { 0 };

    write_return(&[whitelisted])
}

// ---- Entry points ----

/// Main entry point. Dispatches based on the method selector in args.
#[no_mangle]
pub extern "C" fn call() -> i32 {
    unsafe {
        if host_consume_gas(1000) != 0 {
            return -1;
        }

        let args = read_args();

        if args.is_empty() {
            // No args — return total supply as a query
            return method_total_supply();
        }

        let method = args[0];
        let payload = &args[1..];

        match method {
            METHOD_INIT => method_init(payload),
            METHOD_TRANSFER => method_transfer(payload),
            METHOD_BALANCE_OF => method_balance_of(payload),
            METHOD_APPROVE => method_approve(payload),
            METHOD_TRANSFER_FROM => method_transfer_from(payload),
            METHOD_TOTAL_SUPPLY => method_total_supply(),
            METHOD_DISTRIBUTE_REVENUE => method_distribute_revenue(payload),
            METHOD_SET_COMPLIANCE => method_set_compliance(payload),
            METHOD_ADD_TO_WHITELIST => method_add_to_whitelist(payload),
            METHOD_REMOVE_FROM_WHITELIST => method_remove_from_whitelist(payload),
            METHOD_IS_WHITELISTED => method_is_whitelisted(payload),
            _ => -10, // unknown method
        }
    }
}

/// Legacy query entry point.
#[no_mangle]
pub extern "C" fn get_info() -> i32 {
    unsafe {
        let _block = host_block_number();
        let _caller = host_caller();
    }
    0
}
