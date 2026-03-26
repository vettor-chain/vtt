use std::env;

use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::rpc_params;

use vtt_crypto::Keypair;
use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, BlockNumber};
use vtt_rpc::types::{AccountInfo, BlockInfo, ChainStatus, ValidatorInfoRpc};

const DEFAULT_RPC: &str = "http://127.0.0.1:9944";

fn print_usage() {
    eprintln!("VTT CLI v{}", env!("CARGO_PKG_VERSION"));
    eprintln!();
    eprintln!("Usage: vtt-cli [--rpc URL] <command> [args...]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  keygen                  Generate a new keypair");
    eprintln!("  keygen --seed <hex>     Generate keypair from 32-byte hex seed");
    eprintln!("  balance <address>       Get balance of an address");
    eprintln!("  account <address>       Get account info");
    eprintln!("  block <number>          Get block by number");
    eprintln!("  status                  Get chain status");
    eprintln!("  validators              List active validators");
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    // Parse --rpc flag
    let mut rpc_url = DEFAULT_RPC.to_string();
    let mut cmd_args = Vec::new();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--rpc" {
            if let Some(url) = args.get(i + 1) {
                rpc_url = url.clone();
                i += 2;
                continue;
            }
        }
        cmd_args.push(args[i].clone());
        i += 1;
    }

    if cmd_args.is_empty() {
        print_usage();
        std::process::exit(1);
    }

    let command = cmd_args[0].as_str();

    match command {
        "keygen" => cmd_keygen(&cmd_args[1..]),
        "balance" | "account" | "block" | "status" | "validators" => {
            if let Err(e) = cmd_rpc(command, &cmd_args[1..], &rpc_url).await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("Unknown command: {command}");
            print_usage();
            std::process::exit(1);
        }
    }
}

fn cmd_keygen(args: &[String]) {
    let keypair = if args.len() >= 2 && args[0] == "--seed" {
        let seed_hex = &args[1];
        let seed_bytes = hex::decode(seed_hex).expect("invalid hex seed");
        if seed_bytes.len() != 32 {
            eprintln!("Error: seed must be exactly 32 bytes (64 hex chars)");
            std::process::exit(1);
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&seed_bytes);
        Keypair::from_seed(&seed)
    } else {
        Keypair::generate()
    };

    let pubkey = keypair.public_key();
    let address = keypair.address();

    println!("Public Key: {pubkey}");
    println!("Address:    {address}");
}

async fn cmd_rpc(
    command: &str,
    args: &[String],
    rpc_url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = HttpClientBuilder::default().build(rpc_url)?;

    match command {
        "balance" => {
            let addr = parse_address(args.first().ok_or("missing address argument")?)?;
            let balance: Amount = client.request("vtt_getBalance", rpc_params![addr]).await?;
            println!("{balance}");
        }
        "account" => {
            let addr = parse_address(args.first().ok_or("missing address argument")?)?;
            let info: AccountInfo = client.request("vtt_getAccount", rpc_params![addr]).await?;
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
        "block" => {
            let num: BlockNumber = args
                .first()
                .ok_or("missing block number")?
                .parse()
                .map_err(|_| "invalid block number")?;
            let block: Option<BlockInfo> = client
                .request("vtt_getBlockByNumber", rpc_params![num])
                .await?;
            match block {
                Some(b) => println!("{}", serde_json::to_string_pretty(&b)?),
                None => println!("Block {num} not found"),
            }
        }
        "status" => {
            let status: ChainStatus = client.request("vtt_chainStatus", rpc_params![]).await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        "validators" => {
            let validators: Vec<ValidatorInfoRpc> =
                client.request("vtt_getValidators", rpc_params![]).await?;
            println!("{}", serde_json::to_string_pretty(&validators)?);
        }
        _ => unreachable!(),
    }

    Ok(())
}

fn parse_address(s: &str) -> Result<Address, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).map_err(|e| format!("invalid hex address: {e}"))?;
    if bytes.len() != 20 {
        return Err(format!("address must be 20 bytes, got {}", bytes.len()));
    }
    Ok(Address::from_slice(&bytes))
}
