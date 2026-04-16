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
           vtt-relayer watch        --eth-rpc URL --bridge-addr 0x… --vtt-rpc URL [--poll-ms 15000]\n  \
           vtt-relayer submit       --vtt-rpc URL --source-tx-hash 0x… --source-chain N \\\n                                    --recipient 0x… --token 0x… --amount <raw-u128>\n\
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

async fn run_watch(args: &[String]) -> Result<()> {
    let vtt_rpc = flag(args, "--vtt-rpc").ok_or_else(|| anyhow!("--vtt-rpc required"))?;
    let eth_rpc = flag(args, "--eth-rpc").ok_or_else(|| anyhow!("--eth-rpc required"))?;
    let bridge_addr =
        flag(args, "--bridge-addr").ok_or_else(|| anyhow!("--bridge-addr required"))?;
    let poll_ms: u64 = flag(args, "--poll-ms")
        .unwrap_or_else(|| "15000".to_string())
        .parse()
        .context("--poll-ms must be u64")?;

    let seed = load_relayer_seed(args)?;
    let keypair = Keypair::from_seed(&seed);
    let relayer_addr = keypair.address();
    info!(%relayer_addr, eth_rpc, bridge_addr, poll_ms, "relayer starting in watch mode");

    let client = vtt_client(&vtt_rpc).await?;

    // Minimal liveness loop. The Ethereum event-scanning integration is
    // intentionally left to a dedicated task that the operator can fill in
    // using `alloy` (preferred) or raw `eth_getLogs` JSON-RPC calls. The
    // tx-submission path above is ready: once each Deposit event is decoded
    // to (source_tx_hash, source_chain, recipient, token, amount), feed it
    // into `submit_bridge_deposit` with a fresh nonce.
    let mut ticker = tokio::time::interval(Duration::from_millis(poll_ms));
    loop {
        ticker.tick().await;
        match fetch_nonce(&client, &relayer_addr).await {
            Ok(n) => debug!(nonce = n, "relayer alive, polling Ethereum (stub)"),
            Err(e) => warn!(%e, "VTT RPC unreachable"),
        }
        // TODO: query Ethereum logs and submit new deposits. Signature
        // Deposit(uint256,address,address,uint256,uint256,bytes32) topic0
        // = keccak256("Deposit(uint256,address,address,uint256,uint256,bytes32)")
        // Decode args, then:
        //   submit_bridge_deposit(&client, &keypair, chain_id, gas_price,
        //                         fetch_nonce(&client, &relayer_addr).await?,
        //                         tx_hash_b32, source_chain, recipient,
        //                         token, amount).await?;
    }
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
