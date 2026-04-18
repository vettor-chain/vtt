//! VTT bridge relayer.
//!
//! Watches the Ethereum bridge contract for `Deposit` events and mirrors each
//! one as an on-chain `BridgeDeposit` transaction on the VTT chain.
//!
//! Design:
//! - The relayer's Ed25519 keypair address must be configured on-chain via
//!   the `bridge_relayer` ParameterChange governance proposal.
//! - Source-tx replay protection is enforced on VTT side via
//!   `StateDB::bridge_deposit_processed`; submitting the same event twice is
//!   a no-op that returns an error.
//! - This binary exposes two modes:
//!     1. One-shot `submit` subcommand to test the wiring end-to-end or to
//!        manually replay a stuck deposit.
//!     2. Continuous `watch` mode (stub — requires an Ethereum client library
//!        integration such as alloy to decode logs). The polling loop and
//!        transaction-submission path are fully implemented; filling in the
//!        Ethereum event-scanning call is the remaining TODO.

use std::env;
use std::time::Duration;

use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use anyhow::{anyhow, Context, Result};
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClient;
use jsonrpsee::rpc_params;

use vtt_crypto::{blake3_hash, Keypair};
use vtt_primitives::amount::Amount;
use vtt_primitives::transaction::{SignedTransaction, TransactionAction, TransactionPayload};
use vtt_primitives::{Address, ChainId, H256};

fn usage() -> ! {
    eprintln!(
        "vtt-relayer\n\
         \n\
         Usage:\n  \
           vtt-relayer watch            --eth-rpc URL --bridge-addr 0x… --vtt-rpc URL [--poll-ms 15000]\n  \
           vtt-relayer submit           --vtt-rpc URL --source-tx-hash 0x… --source-chain N \\\n                                        --recipient 0x… --token 0x… --amount <raw-u128>\n  \
           vtt-relayer list-withdrawals --vtt-rpc URL\n\
         \n\
         Common flags:\n  \
           --relayer-seed HEX      32-byte seed (hex). Alt: env RELAYER_SEED.\n  \
           --chain-id N            VTT chain id (default 0).\n  \
           --gas-price RAW         VTT gas price raw (default 1_000_000_000).\n\
        "
    );
    std::process::exit(1)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse_hex20(s: &str) -> Result<Address> {
    let raw = s.trim_start_matches("0x");
    let bytes = hex::decode(raw).with_context(|| format!("invalid hex: {s}"))?;
    if bytes.len() != 20 {
        return Err(anyhow!("expected 20-byte address, got {}", bytes.len()));
    }
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&bytes);
    Ok(Address::from(arr))
}

fn parse_hex32(s: &str) -> Result<H256> {
    let raw = s.trim_start_matches("0x");
    let bytes = hex::decode(raw).with_context(|| format!("invalid hex: {s}"))?;
    if bytes.len() != 32 {
        return Err(anyhow!("expected 32-byte hash, got {}", bytes.len()));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(H256::from(arr))
}

fn load_relayer_seed(args: &[String]) -> Result<[u8; 32]> {
    let hex_str = flag(args, "--relayer-seed")
        .or_else(|| env::var("RELAYER_SEED").ok())
        .ok_or_else(|| anyhow!("--relayer-seed or RELAYER_SEED env var required"))?;
    let raw = hex_str.trim_start_matches("0x");
    let bytes = hex::decode(raw).context("relayer seed is not valid hex")?;
    if bytes.len() != 32 {
        return Err(anyhow!(
            "relayer seed must be 32 bytes (got {})",
            bytes.len()
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

async fn vtt_client(url: &str) -> Result<HttpClient> {
    jsonrpsee::http_client::HttpClientBuilder::default()
        .build(url)
        .context("failed to build VTT RPC client")
}

async fn fetch_nonce(client: &HttpClient, address: &Address) -> Result<u64> {
    let hex = format!("0x{}", hex::encode(address.as_bytes()));
    let res: serde_json::Value = client
        .request("vtt_getAccount", rpc_params![hex])
        .await
        .context("vtt_getAccount failed")?;
    let nonce = res
        .get("nonce")
        .and_then(|n| n.as_u64())
        .ok_or_else(|| anyhow!("RPC response missing nonce field"))?;
    Ok(nonce)
}

#[allow(clippy::too_many_arguments)]
async fn submit_bridge_deposit(
    client: &HttpClient,
    keypair: &Keypair,
    chain_id: ChainId,
    gas_price_raw: u128,
    nonce: u64,
    source_tx_hash: H256,
    source_chain: u32,
    recipient: Address,
    token: H256,
    amount: Amount,
) -> Result<H256> {
    let payload = TransactionPayload {
        chain_id,
        nonce,
        gas_price: Amount::from_raw(gas_price_raw),
        gas_limit: 80_000,
        action: TransactionAction::BridgeDeposit {
            source_tx_hash,
            source_chain,
            recipient,
            token,
            amount,
        },
    };
    let bytes = borsh::to_vec(&payload).context("borsh serialization")?;
    let signature = keypair.sign(&bytes);
    let signed = SignedTransaction {
        payload,
        signature,
        public_key: keypair.public_key(),
    };
    let wire = borsh::to_vec(&signed).context("borsh serialization")?;
    let hex_wire = format!("0x{}", hex::encode(&wire));
    let tx_hash_hex: String = client
        .request("vtt_sendTransaction", rpc_params![hex_wire])
        .await
        .context("vtt_sendTransaction failed")?;
    let clean = tx_hash_hex.trim_start_matches("0x");
    let hash = parse_hex32(clean)?;
    Ok(hash)
}

async fn run_submit(args: &[String]) -> Result<()> {
    let vtt_rpc = flag(args, "--vtt-rpc").ok_or_else(|| anyhow!("--vtt-rpc required"))?;
    let source_tx_hash = parse_hex32(
        &flag(args, "--source-tx-hash").ok_or_else(|| anyhow!("--source-tx-hash required"))?,
    )?;
    let source_chain: u32 = flag(args, "--source-chain")
        .ok_or_else(|| anyhow!("--source-chain required"))?
        .parse()
        .context("--source-chain must be u32")?;
    let recipient =
        parse_hex20(&flag(args, "--recipient").ok_or_else(|| anyhow!("--recipient required"))?)?;
    let token =
        parse_hex32(&flag(args, "--token").unwrap_or_else(|| "0x".to_string() + &"0".repeat(64)))?;
    let amount_raw: u128 = flag(args, "--amount")
        .ok_or_else(|| anyhow!("--amount required (raw u128)"))?
        .parse()
        .context("--amount must be u128")?;
    let chain_id = ChainId::new(
        flag(args, "--chain-id")
            .unwrap_or_else(|| "0".to_string())
            .parse()
            .context("--chain-id must be u32")?,
    );
    let gas_price: u128 = flag(args, "--gas-price")
        .unwrap_or_else(|| "1000000000".to_string())
        .parse()
        .context("--gas-price must be u128")?;

    let seed = load_relayer_seed(args)?;
    let keypair = Keypair::from_seed(&seed);
    let relayer_addr = keypair.address();
    info!(%relayer_addr, "relayer identity");

    let client = vtt_client(&vtt_rpc).await?;
    let nonce = fetch_nonce(&client, &relayer_addr).await?;
    debug!(nonce, "fetched relayer account nonce");

    let tx_hash = submit_bridge_deposit(
        &client,
        &keypair,
        chain_id,
        gas_price,
        nonce,
        source_tx_hash,
        source_chain,
        recipient,
        token,
        Amount::from_raw(amount_raw),
    )
    .await?;
    println!("0x{}", hex::encode(tx_hash.as_bytes()));
    Ok(())
}

/// List pending BridgeWithdraw events on the VTT chain. Used as the first
/// half of the VTT -> Ethereum relay flow: the operator (or a downstream
/// EVM integration layered on alloy) takes the JSON and calls `release()`
/// on the Solidity bridge contract. Idempotency and replay protection live
/// in the Solidity contract (processedWithdrawals mapping).
async fn run_list_withdrawals(args: &[String]) -> Result<()> {
    let vtt_rpc = flag(args, "--vtt-rpc").ok_or_else(|| anyhow!("--vtt-rpc required"))?;
    let client = vtt_client(&vtt_rpc).await?;
    let withdrawals: serde_json::Value = client
        .request("vtt_getBridgeWithdrawals", rpc_params![])
        .await
        .context("vtt_getBridgeWithdrawals failed")?;
    println!("{}", serde_json::to_string_pretty(&withdrawals)?);
    Ok(())
}

// ─── EVM-side helpers ──────────────────────────────────────────────────────
//
// We talk to the Ethereum/Base RPC over plain JSON-RPC via jsonrpsee
// (reusing the http-client dep already in the workspace). This avoids
// pulling in alloy/ethers just for two RPC calls and one log-decoding
// pass; the trade-off is we decode the `Deposit` event by byte offset
// rather than via an ABI. That's fine because the event layout is fixed
// by VTTBridge.sol and re-encoded by every well-formed deposit.

/// `keccak256("Deposit(uint256,address,address,uint256,uint256,bytes32)")`.
/// This is the topic0 the Solidity bridge stamps on every Deposit log.
/// Hardcoded so a wrong RPC / wrong contract never accidentally routes
/// unrelated events through the relayer.
const DEPOSIT_TOPIC0: &str = "0xa1a227e2d79d31ead6b6a29c4b0158d07ef3e3bf2cbd44c9731bbd9a46858e93";

async fn eth_block_number(client: &HttpClient) -> Result<u64> {
    let res: String = client
        .request("eth_blockNumber", rpc_params![])
        .await
        .context("eth_blockNumber failed")?;
    let clean = res.trim_start_matches("0x");
    u64::from_str_radix(clean, 16).context("bad hex in eth_blockNumber")
}

async fn eth_get_deposit_logs(
    client: &HttpClient,
    bridge_addr: &str,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<serde_json::Value>> {
    let filter = serde_json::json!({
        "fromBlock": format!("0x{:x}", from_block),
        "toBlock":   format!("0x{:x}", to_block),
        "address":   bridge_addr,
        "topics":    [DEPOSIT_TOPIC0],
    });
    let res: serde_json::Value = client
        .request("eth_getLogs", rpc_params![filter])
        .await
        .context("eth_getLogs failed")?;
    Ok(res.as_array().cloned().unwrap_or_default())
}

/// Decoded Deposit event, in VTT-side types so it can be fed straight
/// into `submit_bridge_deposit`.
#[derive(Debug)]
struct DepositEvent {
    tx_hash: H256,
    /// `address(0)` in the event means the user burned wVTT; we map it
    /// to native VTT by feeding `H256::ZERO` to the chain.
    token_evm: Address,
    amount: Amount,
    vtt_recipient: Address,
}

fn parse_deposit_log(log: &serde_json::Value) -> Result<DepositEvent> {
    let tx_hash_hex = log
        .get("transactionHash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("log missing transactionHash"))?;
    let tx_hash = parse_hex32(tx_hash_hex.trim_start_matches("0x"))?;

    // topics: [topic0, indexed nonce (ignored), indexed sender (ignored)]
    // data:   abi.encode(address token, uint256 amount, uint256 fee, bytes32 vttDestination)
    //         → 4 words of 32 bytes = 128 bytes total
    let data_hex = log
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("log missing data"))?;
    let data =
        hex::decode(data_hex.trim_start_matches("0x")).context("log data is not valid hex")?;
    if data.len() != 128 {
        return Err(anyhow!(
            "unexpected Deposit data length {} (want 128)",
            data.len()
        ));
    }

    // word 0: token address, right-aligned in 32 bytes
    let mut token_bytes = [0u8; 20];
    token_bytes.copy_from_slice(&data[12..32]);
    let token_evm = Address::from(token_bytes);

    // word 1: amount (uint256 — we treat as u128 since VTT Amount is u128;
    // Solidity uint256 > u128::MAX would overflow but we cap).
    let amount_raw = u256_as_u128(&data[32..64])?;
    let amount = Amount::from_raw(amount_raw);

    // word 2: fee — we ignore here, the EVM side already deducted it.
    // word 3: vttDestination (bytes32 — first 20 bytes are the VTT address).
    let mut recip_bytes = [0u8; 20];
    recip_bytes.copy_from_slice(&data[96..116]);
    let vtt_recipient = Address::from(recip_bytes);

    Ok(DepositEvent {
        tx_hash,
        token_evm,
        amount,
        vtt_recipient,
    })
}

fn u256_as_u128(bytes: &[u8]) -> Result<u128> {
    if bytes.len() != 32 {
        return Err(anyhow!("u256 slice must be 32 bytes"));
    }
    // High 128 bits must be zero, else the amount doesn't fit u128.
    if bytes[0..16].iter().any(|b| *b != 0) {
        return Err(anyhow!("uint256 value exceeds u128"));
    }
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&bytes[16..32]);
    Ok(u128::from_be_bytes(buf))
}

/// Persistent cursor so a restart doesn't re-submit every historical
/// deposit. Lives at `$VTT_RELAYER_STATE` (default: ~/.vtt-relayer/cursor).
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct WatchCursor {
    last_scanned_block: u64,
}

fn cursor_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("VTT_RELAYER_STATE") {
        return std::path::PathBuf::from(p);
    }
    let mut p = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    p.push(".vtt-relayer");
    p.push("cursor.json");
    p
}

fn load_cursor() -> WatchCursor {
    let path = cursor_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<WatchCursor>(&s).ok())
        .unwrap_or_default()
}

fn save_cursor(cursor: &WatchCursor) -> Result<()> {
    let path = cursor_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(cursor)?;
    std::fs::write(&path, json).with_context(|| format!("writing cursor to {path:?}"))?;
    Ok(())
}

/// Map an EVM token address to the VTT-side asset id. `address(0)` in the
/// Deposit event means the user burned wVTT and should get native VTT
/// credited (sentinel `H256::ZERO`). Any non-zero token is mapped via
/// `TOKEN_MAP_JSON` (env var) which the operator configures per deployment.
fn map_token(token_evm: &Address, token_map: &TokenMap) -> Result<H256> {
    if *token_evm == Address::ZERO {
        return Ok(H256::ZERO);
    }
    let key = format!("0x{}", hex::encode(token_evm.as_bytes())).to_lowercase();
    let asset_hex = token_map
        .inner
        .get(&key)
        .ok_or_else(|| anyhow!("no VTT asset id configured for EVM token {key}"))?;
    let clean = asset_hex.trim_start_matches("0x");
    parse_hex32(clean)
}

struct TokenMap {
    /// EVM address (lowercase 0x-prefixed) → VTT asset_id hex.
    inner: std::collections::HashMap<String, String>,
}

fn load_token_map() -> TokenMap {
    let raw = std::env::var("TOKEN_MAP_JSON").unwrap_or_default();
    if raw.trim().is_empty() {
        return TokenMap {
            inner: std::collections::HashMap::new(),
        };
    }
    match serde_json::from_str::<std::collections::HashMap<String, String>>(&raw) {
        Ok(m) => {
            let inner = m.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
            TokenMap { inner }
        }
        Err(e) => {
            warn!(%e, "TOKEN_MAP_JSON is not valid JSON; ignoring");
            TokenMap {
                inner: std::collections::HashMap::new(),
            }
        }
    }
}

async fn run_watch(args: &[String]) -> Result<()> {
    let vtt_rpc = flag(args, "--vtt-rpc").ok_or_else(|| anyhow!("--vtt-rpc required"))?;
    let eth_rpc = flag(args, "--eth-rpc").ok_or_else(|| anyhow!("--eth-rpc required"))?;
    let bridge_addr =
        flag(args, "--bridge-addr").ok_or_else(|| anyhow!("--bridge-addr required"))?;
    let source_chain: u32 = flag(args, "--source-chain")
        .unwrap_or_else(|| "84532".to_string())
        .parse()
        .context("--source-chain must be u32")?;
    let vtt_chain_id = ChainId::new(
        flag(args, "--chain-id")
            .unwrap_or_else(|| "0".to_string())
            .parse()
            .context("--chain-id must be u32")?,
    );
    let gas_price: u128 = flag(args, "--gas-price")
        .unwrap_or_else(|| "1000000000".to_string())
        .parse()
        .context("--gas-price must be u128")?;
    let poll_ms: u64 = flag(args, "--poll-ms")
        .unwrap_or_else(|| "15000".to_string())
        .parse()
        .context("--poll-ms must be u64")?;
    // Block depth under head we require before scanning — protects
    // against shallow reorgs on Base (which finalises within seconds
    // but still has 1-2 block soft reorgs occasionally).
    let confirmations: u64 = flag(args, "--confirmations")
        .unwrap_or_else(|| "3".to_string())
        .parse()
        .context("--confirmations must be u64")?;

    let seed = load_relayer_seed(args)?;
    let keypair = Keypair::from_seed(&seed);
    let relayer_addr = keypair.address();
    let token_map = load_token_map();
    info!(
        %relayer_addr,
        eth_rpc,
        bridge_addr,
        source_chain,
        poll_ms,
        confirmations,
        token_map_entries = token_map.inner.len(),
        "relayer starting in watch mode"
    );

    let vtt = vtt_client(&vtt_rpc).await?;
    let eth = jsonrpsee::http_client::HttpClientBuilder::default()
        .build(&eth_rpc)
        .context("failed to build ETH RPC client")?;

    let mut cursor = load_cursor();
    if cursor.last_scanned_block == 0 {
        // Fresh start: seed from current head so we don't replay every
        // historical deposit. Operators replaying from zero should
        // delete the cursor file manually.
        cursor.last_scanned_block = eth_block_number(&eth).await?;
        save_cursor(&cursor).ok();
        info!(seed_block = cursor.last_scanned_block, "initialised cursor");
    }

    let mut ticker = tokio::time::interval(Duration::from_millis(poll_ms));
    loop {
        ticker.tick().await;

        // ── ETH -> VTT: scan Deposit events, submit BridgeDeposit on VTT
        let head = match eth_block_number(&eth).await {
            Ok(h) => h,
            Err(e) => {
                warn!(%e, "ETH RPC unreachable");
                continue;
            }
        };
        let safe_head = head.saturating_sub(confirmations);
        if safe_head > cursor.last_scanned_block {
            let from = cursor.last_scanned_block + 1;
            // Cap scan range per tick so a cold cursor doesn't try to
            // download 100k blocks in one shot.
            let to = safe_head.min(from + 1_999);
            match eth_get_deposit_logs(&eth, &bridge_addr, from, to).await {
                Ok(logs) => {
                    debug!(from, to, count = logs.len(), "scanned ETH bridge logs");
                    for log in &logs {
                        match parse_deposit_log(log) {
                            Ok(ev) => match map_token(&ev.token_evm, &token_map) {
                                Ok(vtt_token) => {
                                    if let Err(e) = process_deposit_event(
                                        &vtt,
                                        &keypair,
                                        &relayer_addr,
                                        vtt_chain_id,
                                        gas_price,
                                        source_chain,
                                        vtt_token,
                                        &ev,
                                    )
                                    .await
                                    {
                                        warn!(?ev, %e, "failed to mirror deposit to VTT");
                                    }
                                }
                                Err(e) => warn!(?ev, %e, "skipping deposit: token not mapped"),
                            },
                            Err(e) => warn!(%e, ?log, "could not parse Deposit log"),
                        }
                    }
                    cursor.last_scanned_block = to;
                    if let Err(e) = save_cursor(&cursor) {
                        warn!(%e, "failed to persist cursor");
                    }
                }
                Err(e) => warn!(%e, from, to, "eth_getLogs failed"),
            }
        }

        // ── VTT -> ETH: surface pending withdrawals so the operator (or
        // a future eth-side signing path) can call releaseWVTT/USDT.
        match vtt
            .request::<serde_json::Value, _>("vtt_getBridgeWithdrawals", rpc_params![])
            .await
        {
            Ok(v) => {
                let count = v.as_array().map(|a| a.len()).unwrap_or(0);
                if count > 0 {
                    info!(
                        count,
                        "pending VTT -> ETH withdrawals; call release() on Solidity bridge"
                    );
                    debug!(withdrawals = %v, "withdrawal payload");
                }
            }
            Err(e) => warn!(%e, "failed to query bridge withdrawals"),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_deposit_event(
    vtt: &HttpClient,
    keypair: &Keypair,
    relayer_addr: &Address,
    vtt_chain_id: ChainId,
    gas_price: u128,
    source_chain: u32,
    vtt_token: H256,
    ev: &DepositEvent,
) -> Result<()> {
    let nonce = fetch_nonce(vtt, relayer_addr).await?;
    let tx_hash = submit_bridge_deposit(
        vtt,
        keypair,
        vtt_chain_id,
        gas_price,
        nonce,
        ev.tx_hash,
        source_chain,
        ev.vtt_recipient,
        vtt_token,
        ev.amount,
    )
    .await?;
    info!(
        eth_tx = %format!("0x{}", hex::encode(ev.tx_hash.as_bytes())),
        vtt_tx = %format!("0x{}", hex::encode(tx_hash.as_bytes())),
        recipient = %ev.vtt_recipient,
        amount = %ev.amount,
        "mirrored ETH deposit as VTT BridgeDeposit"
    );
    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let result = match args[1].as_str() {
        "watch" => run_watch(&args).await,
        "submit" => run_submit(&args).await,
        "list-withdrawals" => run_list_withdrawals(&args).await,
        _ => {
            usage();
        }
    };
    if let Err(e) = result {
        error!(%e, "relayer failed");
        std::process::exit(1);
    }
    // suppress unused warnings for bootstrap code
    let _ = blake3_hash(&[]);
}
