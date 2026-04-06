//! Real Estate Token Contract for VTT
//!
//! Extends the equity-token pattern with real-estate-specific metadata:
//! property address, valuation, rental yield, and document hashes.
//!
//! Compile with: `cargo build --target wasm32-unknown-unknown --release`
//!
//! Method dispatch (first byte of args payload):
//!   0x01 = init(name, symbol, total_supply, issuer, property_uri_len, property_uri)
//!   0x02 = transfer(to, amount)
//!   0x03 = balance_of(owner)
//!   0x04 = approve(spender, amount)
//!   0x05 = transfer_from(from, to, amount)
//!   0x06 = total_supply()
//!   0x07 = distribute_rent(total_amount) — issuer distributes rent pro-rata
//!   0x08 = set_compliance(compliance_addr) — issuer only, enables whitelist enforcement
//!   0x09 = update_valuation(new_valuation_u128) — issuer only
//!   0x0A = property_info() — returns property URI + valuation
//!   0x0B = add_to_whitelist(address) — issuer only
//!   0x0C = remove_from_whitelist(address) — issuer only
//!   0x0D = is_whitelisted(address) — returns 1 byte: 0x01 = yes, 0x00 = no

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

const ADDR_LEN: usize = 20;
const U128_LEN: usize = 16;
const MAX_NAME_LEN: usize = 64;
const MAX_SYMBOL_LEN: usize = 16;
const MAX_URI_LEN: usize = 256;
const ARGS_OFFSET: usize = 0;

// Storage keys
const KEY_INITIALIZED: &[u8] = b"init";
const KEY_NAME: &[u8] = b"name";
const KEY_SYMBOL: &[u8] = b"sym";
const KEY_TOTAL_SUPPLY: &[u8] = b"tsup";
const KEY_ISSUER: &[u8] = b"issr";
const KEY_COMPLIANCE: &[u8] = b"comp";
const KEY_HOLDER_COUNT: &[u8] = b"hcnt";
const KEY_PROPERTY_URI: &[u8] = b"puri";
const KEY_VALUATION: &[u8] = b"pval";
const KEY_TOTAL_RENT_DISTRIBUTED: &[u8] = b"tren";

// Methods
const METHOD_INIT: u8 = 0x01;
const METHOD_TRANSFER: u8 = 0x02;
const METHOD_BALANCE_OF: u8 = 0x03;
const METHOD_APPROVE: u8 = 0x04;
const METHOD_TRANSFER_FROM: u8 = 0x05;
const METHOD_TOTAL_SUPPLY: u8 = 0x06;
const METHOD_DISTRIBUTE_RENT: u8 = 0x07;
const METHOD_SET_COMPLIANCE: u8 = 0x08;
const METHOD_UPDATE_VALUATION: u8 = 0x09;
const METHOD_PROPERTY_INFO: u8 = 0x0A;
const METHOD_ADD_TO_WHITELIST: u8 = 0x0B;
const METHOD_REMOVE_FROM_WHITELIST: u8 = 0x0C;
const METHOD_IS_WHITELISTED: u8 = 0x0D;

// ---- Helpers (same as equity-token) ----

unsafe fn storage_read_buf(key: &[u8], buf: &mut [u8]) -> i32 {
    host_storage_read(
        key.as_ptr() as i32,
        key.len() as i32,
        buf.as_mut_ptr() as i32,
        buf.len() as i32,
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

unsafe fn read_u128(key: &[u8]) -> u128 {
    let mut buf = [0u8; U128_LEN];
    let read = storage_read_buf(key, &mut buf);
    if read == U128_LEN as i32 {
        u128::from_le_bytes(buf)
    } else {
        0
    }
}

unsafe fn write_u128(key: &[u8], val: u128) {
    storage_write(key, &val.to_le_bytes());
}

fn balance_key(addr: &[u8; ADDR_LEN]) -> [u8; 4 + ADDR_LEN] {
    let mut key = [0u8; 4 + ADDR_LEN];
    key[..4].copy_from_slice(b"bal:");
    key[4..].copy_from_slice(addr);
    key
}

fn whitelist_key(addr: &[u8; ADDR_LEN]) -> [u8; 3 + ADDR_LEN] {
    let mut key = [0u8; 3 + ADDR_LEN];
    key[..3].copy_from_slice(b"wl:");
    key[3..].copy_from_slice(addr);
    key
}

fn allowance_key(owner: &[u8; ADDR_LEN], spender: &[u8; ADDR_LEN]) -> [u8; 4 + ADDR_LEN * 2] {
    let mut key = [0u8; 4 + ADDR_LEN * 2];
    key[..4].copy_from_slice(b"alw:");
    key[4..4 + ADDR_LEN].copy_from_slice(owner);
    key[4 + ADDR_LEN..].copy_from_slice(spender);
    key
}

fn holder_index_key(index: u32) -> [u8; 8] {
    let mut key = [0u8; 8];
    key[..4].copy_from_slice(b"hld:");
    key[4..].copy_from_slice(&index.to_le_bytes());
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

unsafe fn is_issuer(caller: &[u8; ADDR_LEN]) -> bool {
    let mut issuer = [0u8; ADDR_LEN];
    let read = storage_read_buf(KEY_ISSUER, &mut issuer);
    read == ADDR_LEN as i32 && *caller == issuer
}

unsafe fn do_transfer(from: &[u8; ADDR_LEN], to: &[u8; ADDR_LEN], amount: u128) -> i32 {
    // If compliance is enabled (a compliance address is set), enforce whitelist
    let mut compliance_addr = [0u8; ADDR_LEN];
    let comp_read = storage_read_buf(KEY_COMPLIANCE, &mut compliance_addr);
    if comp_read == ADDR_LEN as i32 {
        // Compliance is enabled -- both sender and receiver must be whitelisted
        let from_wl = whitelist_key(from);
        let mut from_status = [0u8; 1];
        let fr = storage_read_buf(&from_wl, &mut from_status);
        if fr != 1 || from_status[0] != 1 {
            return -13; // sender not whitelisted
        }

        let to_wl = whitelist_key(to);
        let mut to_status = [0u8; 1];
        let tr = storage_read_buf(&to_wl, &mut to_status);
        if tr != 1 || to_status[0] != 1 {
            return -14; // receiver not whitelisted
        }
    }

    let from_key = balance_key(from);
    let from_balance = read_u128(&from_key);
    if from_balance < amount {
        return -5;
    }

    write_u128(&from_key, from_balance - amount);

    let to_key = balance_key(to);
    let to_balance = read_u128(&to_key);
    let was_zero = to_balance == 0;
    write_u128(&to_key, to_balance + amount);

    if was_zero {
        let holder_count = read_u128(KEY_HOLDER_COUNT) as u32;
        let hkey = holder_index_key(holder_count);
        storage_write(&hkey, to);
        write_u128(KEY_HOLDER_COUNT, (holder_count + 1) as u128);
    }

    let mut event = [0u8; 1 + ADDR_LEN * 2 + U128_LEN];
    event[0] = METHOD_TRANSFER;
    event[1..1 + ADDR_LEN].copy_from_slice(from);
    event[1 + ADDR_LEN..1 + ADDR_LEN * 2].copy_from_slice(to);
    event[1 + ADDR_LEN * 2..].copy_from_slice(&amount.to_le_bytes());
    emit_event(&event);

    0
}

// ---- Contract methods ----

/// Initialize the real estate token.
/// Args: name_len(1) + name + symbol_len(1) + symbol + total_supply(16) + issuer(20) + uri_len(1) + uri
unsafe fn method_init(args: &[u8]) -> i32 {
    if host_consume_gas(6000) != 0 {
        return -1;
    }

    let mut init_buf = [0u8; 1];
    let init_read = storage_read_buf(KEY_INITIALIZED, &mut init_buf);
    if init_read > 0 && init_buf[0] == 1 {
        return -2;
    }

    let mut offset = 0;

    // name
    if args.len() < 1 { return -3; }
    let name_len = args[offset] as usize;
    offset += 1;
    if offset + name_len > args.len() || name_len > MAX_NAME_LEN { return -3; }
    let name = &args[offset..offset + name_len];
    offset += name_len;

    // symbol
    if offset >= args.len() { return -3; }
    let symbol_len = args[offset] as usize;
    offset += 1;
    if offset + symbol_len > args.len() || symbol_len > MAX_SYMBOL_LEN { return -3; }
    let symbol = &args[offset..offset + symbol_len];
    offset += symbol_len;

    // total_supply
    if offset + U128_LEN > args.len() { return -3; }
    let mut supply_bytes = [0u8; U128_LEN];
    supply_bytes.copy_from_slice(&args[offset..offset + U128_LEN]);
    let total_supply = u128::from_le_bytes(supply_bytes);
    offset += U128_LEN;

    // issuer
    if offset + ADDR_LEN > args.len() { return -3; }
    let mut issuer = [0u8; ADDR_LEN];
    issuer.copy_from_slice(&args[offset..offset + ADDR_LEN]);
    offset += ADDR_LEN;

    // property URI (optional but expected for real estate)
    let mut property_uri: &[u8] = b"";
    if offset < args.len() {
        let uri_len = args[offset] as usize;
        offset += 1;
        if offset + uri_len <= args.len() && uri_len <= MAX_URI_LEN {
            property_uri = &args[offset..offset + uri_len];
        }
    }

    // Store metadata
    storage_write(KEY_NAME, name);
    storage_write(KEY_SYMBOL, symbol);
    write_u128(KEY_TOTAL_SUPPLY, total_supply);
    storage_write(KEY_ISSUER, &issuer);
    if !property_uri.is_empty() {
        storage_write(KEY_PROPERTY_URI, property_uri);
    }

    // Mint total supply to issuer
    let bkey = balance_key(&issuer);
    write_u128(&bkey, total_supply);

    // Track issuer as holder #0
    let hkey = holder_index_key(0);
    storage_write(&hkey, &issuer);
    write_u128(KEY_HOLDER_COUNT, 1);

    storage_write(KEY_INITIALIZED, &[1]);

    let mut event = [0u8; 1 + ADDR_LEN + U128_LEN];
    event[0] = METHOD_INIT;
    event[1..1 + ADDR_LEN].copy_from_slice(&issuer);
    event[1 + ADDR_LEN..].copy_from_slice(&total_supply.to_le_bytes());
    emit_event(&event);

    0
}

unsafe fn method_transfer(args: &[u8]) -> i32 {
    if host_consume_gas(3000) != 0 { return -1; }
    if args.len() < ADDR_LEN + U128_LEN { return -3; }

    let mut to = [0u8; ADDR_LEN];
    to.copy_from_slice(&args[..ADDR_LEN]);
    let mut amount_bytes = [0u8; U128_LEN];
    amount_bytes.copy_from_slice(&args[ADDR_LEN..ADDR_LEN + U128_LEN]);
    let amount = u128::from_le_bytes(amount_bytes);
    if amount == 0 { return -4; }

    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);
    do_transfer(&caller, &to, amount)
}

unsafe fn method_balance_of(args: &[u8]) -> i32 {
    if host_consume_gas(500) != 0 { return -1; }
    if args.len() < ADDR_LEN { return -3; }

    let mut owner = [0u8; ADDR_LEN];
    owner.copy_from_slice(&args[..ADDR_LEN]);
    let bkey = balance_key(&owner);
    let balance = read_u128(&bkey);
    write_return(&balance.to_le_bytes())
}

unsafe fn method_approve(args: &[u8]) -> i32 {
    if host_consume_gas(2000) != 0 { return -1; }
    if args.len() < ADDR_LEN + U128_LEN { return -3; }

    let mut spender = [0u8; ADDR_LEN];
    spender.copy_from_slice(&args[..ADDR_LEN]);
    let mut amount_bytes = [0u8; U128_LEN];
    amount_bytes.copy_from_slice(&args[ADDR_LEN..ADDR_LEN + U128_LEN]);
    let amount = u128::from_le_bytes(amount_bytes);

    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);
    let akey = allowance_key(&caller, &spender);
    write_u128(&akey, amount);

    0
}

unsafe fn method_transfer_from(args: &[u8]) -> i32 {
    if host_consume_gas(4000) != 0 { return -1; }
    if args.len() < ADDR_LEN * 2 + U128_LEN { return -3; }

    let mut from = [0u8; ADDR_LEN];
    from.copy_from_slice(&args[..ADDR_LEN]);
    let mut to = [0u8; ADDR_LEN];
    to.copy_from_slice(&args[ADDR_LEN..ADDR_LEN * 2]);
    let mut amount_bytes = [0u8; U128_LEN];
    amount_bytes.copy_from_slice(&args[ADDR_LEN * 2..ADDR_LEN * 2 + U128_LEN]);
    let amount = u128::from_le_bytes(amount_bytes);
    if amount == 0 { return -4; }

    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);
    let akey = allowance_key(&from, &caller);
    let allowance = read_u128(&akey);
    if allowance < amount { return -6; }
    write_u128(&akey, allowance - amount);

    do_transfer(&from, &to, amount)
}

unsafe fn method_total_supply() -> i32 {
    if host_consume_gas(200) != 0 { return -1; }
    let supply = read_u128(KEY_TOTAL_SUPPLY);
    write_return(&supply.to_le_bytes())
}

/// Distribute rent pro-rata to all holders (issuer only).
/// Args: total_amount(16)
unsafe fn method_distribute_rent(args: &[u8]) -> i32 {
    if host_consume_gas(5000) != 0 { return -1; }
    if args.len() < U128_LEN { return -3; }

    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);
    if !is_issuer(&caller) { return -8; }

    let mut amount_bytes = [0u8; U128_LEN];
    amount_bytes.copy_from_slice(&args[..U128_LEN]);
    let total_amount = u128::from_le_bytes(amount_bytes);
    if total_amount == 0 { return 0; }

    let total_supply = read_u128(KEY_TOTAL_SUPPLY);
    if total_supply == 0 { return -9; }

    let holder_count = read_u128(KEY_HOLDER_COUNT) as u32;
    if host_consume_gas(holder_count as i64 * 500) != 0 { return -1; }

    let mut distributed: u128 = 0;

    for i in 0..holder_count {
        let hkey = holder_index_key(i);
        let mut holder_addr = [0u8; ADDR_LEN];
        let read = storage_read_buf(&hkey, &mut holder_addr);
        if read != ADDR_LEN as i32 { continue; }

        let bkey = balance_key(&holder_addr);
        let holder_balance = read_u128(&bkey);
        if holder_balance == 0 { continue; }

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

    // Increase total supply by distributed amount
    write_u128(KEY_TOTAL_SUPPLY, total_supply + distributed);

    // Track cumulative rent
    let prev_rent = read_u128(KEY_TOTAL_RENT_DISTRIBUTED);
    write_u128(KEY_TOTAL_RENT_DISTRIBUTED, prev_rent + distributed);

    let mut event = [0u8; 1 + U128_LEN * 2];
    event[0] = METHOD_DISTRIBUTE_RENT;
    event[1..1 + U128_LEN].copy_from_slice(&total_amount.to_le_bytes());
    event[1 + U128_LEN..].copy_from_slice(&distributed.to_le_bytes());
    emit_event(&event);

    0
}

unsafe fn method_set_compliance(args: &[u8]) -> i32 {
    if host_consume_gas(2000) != 0 { return -1; }
    if args.len() < ADDR_LEN { return -3; }

    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);
    if !is_issuer(&caller) { return -8; }

    let mut compliance_addr = [0u8; ADDR_LEN];
    compliance_addr.copy_from_slice(&args[..ADDR_LEN]);
    storage_write(KEY_COMPLIANCE, &compliance_addr);

    0
}

/// Update property valuation (issuer only).
/// Args: new_valuation(16)
unsafe fn method_update_valuation(args: &[u8]) -> i32 {
    if host_consume_gas(2000) != 0 { return -1; }
    if args.len() < U128_LEN { return -3; }

    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);
    if !is_issuer(&caller) { return -8; }

    let mut val_bytes = [0u8; U128_LEN];
    val_bytes.copy_from_slice(&args[..U128_LEN]);
    let valuation = u128::from_le_bytes(val_bytes);
    write_u128(KEY_VALUATION, valuation);

    let mut event = [0u8; 1 + U128_LEN];
    event[0] = METHOD_UPDATE_VALUATION;
    event[1..].copy_from_slice(&valuation.to_le_bytes());
    emit_event(&event);

    0
}

/// Returns property URI length + URI bytes + valuation(16).
unsafe fn method_property_info() -> i32 {
    if host_consume_gas(500) != 0 { return -1; }

    let mut uri_buf = [0u8; MAX_URI_LEN];
    let uri_len = storage_read_buf(KEY_PROPERTY_URI, &mut uri_buf);
    let uri_len = if uri_len > 0 { uri_len as usize } else { 0 };

    let valuation = read_u128(KEY_VALUATION);
    let total_rent = read_u128(KEY_TOTAL_RENT_DISTRIBUTED);

    // Return: uri_len(1) + uri + valuation(16) + total_rent(16)
    let total_len = 1 + uri_len + U128_LEN * 2;
    let mut out = [0u8; 1 + MAX_URI_LEN + U128_LEN * 2];
    out[0] = uri_len as u8;
    out[1..1 + uri_len].copy_from_slice(&uri_buf[..uri_len]);
    out[1 + uri_len..1 + uri_len + U128_LEN].copy_from_slice(&valuation.to_le_bytes());
    out[1 + uri_len + U128_LEN..1 + uri_len + U128_LEN * 2].copy_from_slice(&total_rent.to_le_bytes());

    write_return(&out[..total_len])
}

/// Add an address to the compliance whitelist (issuer only).
/// Args: address(20)
unsafe fn method_add_to_whitelist(args: &[u8]) -> i32 {
    if host_consume_gas(2000) != 0 { return -1; }
    if args.len() < ADDR_LEN { return -3; }

    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);
    if !is_issuer(&caller) { return -8; }

    let mut addr = [0u8; ADDR_LEN];
    addr.copy_from_slice(&args[..ADDR_LEN]);
    let wkey = whitelist_key(&addr);
    storage_write(&wkey, &[1]);

    0
}

/// Remove an address from the compliance whitelist (issuer only).
/// Args: address(20)
unsafe fn method_remove_from_whitelist(args: &[u8]) -> i32 {
    if host_consume_gas(2000) != 0 { return -1; }
    if args.len() < ADDR_LEN { return -3; }

    let mut caller = [0u8; ADDR_LEN];
    get_caller(&mut caller);
    if !is_issuer(&caller) { return -8; }

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
    if host_consume_gas(500) != 0 { return -1; }
    if args.len() < ADDR_LEN { return -3; }

    let mut addr = [0u8; ADDR_LEN];
    addr.copy_from_slice(&args[..ADDR_LEN]);
    let wkey = whitelist_key(&addr);
    let mut status = [0u8; 1];
    let read = storage_read_buf(&wkey, &mut status);
    let whitelisted: u8 = if read == 1 && status[0] == 1 { 1 } else { 0 };
    write_return(&[whitelisted])
}

// ---- Entry points ----

#[no_mangle]
pub extern "C" fn call() -> i32 {
    unsafe {
        if host_consume_gas(1000) != 0 { return -1; }

        let args = read_args();
        if args.is_empty() {
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
            METHOD_DISTRIBUTE_RENT => method_distribute_rent(payload),
            METHOD_SET_COMPLIANCE => method_set_compliance(payload),
            METHOD_UPDATE_VALUATION => method_update_valuation(payload),
            METHOD_PROPERTY_INFO => method_property_info(),
            METHOD_ADD_TO_WHITELIST => method_add_to_whitelist(payload),
            METHOD_REMOVE_FROM_WHITELIST => method_remove_from_whitelist(payload),
            METHOD_IS_WHITELISTED => method_is_whitelisted(payload),
            _ => -10,
        }
    }
}

#[no_mangle]
pub extern "C" fn get_info() -> i32 {
    unsafe {
        let _block = host_block_number();
        let _caller = host_caller();
    }
    0
}
