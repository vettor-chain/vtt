use std::env;

use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::rpc_params;

use vtt_crypto::Keypair;
use vtt_genesis::GenesisConfig;
use vtt_primitives::amount::Amount;
use vtt_primitives::transaction::{SignedTransaction, TransactionAction, TransactionPayload};
use vtt_primitives::{Address, BlockNumber, H256};
use vtt_rpc::types::{AccountInfo, BlockInfo, ChainStatus, GasConfigRpc, ValidatorInfoRpc};

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
    eprintln!("  genesis                 Export default genesis config to stdout");
    eprintln!("  genesis --out <file>    Export default genesis config to file");
    eprintln!("  stake --validator <addr> --amount <vtt> --seed <hex>");
    eprintln!("                          Stake VTT to a validator");
    eprintln!("  unstake --validator <addr> --amount <vtt> --seed <hex>");
    eprintln!("                          Unstake VTT from a validator");
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
        "genesis" => cmd_genesis(&cmd_args[1..]),
        "balance" | "account" | "block" | "status" | "validators" => {
            if let Err(e) = cmd_rpc(command, &cmd_args[1..], &rpc_url).await {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        "stake" | "unstake" => {
            if let Err(e) = cmd_stake(command, &cmd_args[1..], &rpc_url).await {
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

fn cmd_genesis(args: &[String]) {
    let is_testnet = args.iter().any(|a| a == "--testnet");
    let config = if is_testnet {
        GenesisConfig::testnet_default()
    } else {
        GenesisConfig::dev_default()
    };
    let json = serde_json::to_string_pretty(&config).expect("failed to serialize genesis");

    let out_pos = args.iter().position(|a| a == "--out");
    if let Some(pos) = out_pos {
        if let Some(path) = args.get(pos + 1) {
            std::fs::write(path, &json).expect("failed to write genesis file");
            eprintln!("Genesis config written to {path}");
        } else {
            eprintln!("Error: --out requires a file path");
            std::process::exit(1);
        }
    } else {
        println!("{json}");
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

/// Parse a named flag value from args: --flag <value>
fn parse_flag<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

async fn cmd_stake(
    command: &str,
    args: &[String],
    rpc_url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let validator_hex = parse_flag(args, "--validator").ok_or("missing --validator <address>")?;
    let amount_str = parse_flag(args, "--amount").ok_or("missing --amount <vtt>")?;
    let seed_hex = parse_flag(args, "--seed").ok_or("missing --seed <hex>")?;

    let validator_addr =
        parse_address(validator_hex).map_err(|e| format!("invalid validator address: {e}"))?;
    let amount_vtt: u64 = amount_str
        .parse()
        .map_err(|_| "invalid amount: expected integer VTT")?;
    let amount = Amount::from_vtt(amount_vtt);

    let seed_bytes = hex::decode(seed_hex).map_err(|e| format!("invalid seed hex: {e}"))?;
    if seed_bytes.len() != 32 {
        return Err("seed must be exactly 32 bytes (64 hex chars)".into());
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&seed_bytes);
    let keypair = Keypair::from_seed(&seed);
    let sender_addr = keypair.address();

    let client = HttpClientBuilder::default().build(rpc_url)?;

    // 1. Get account nonce
    let account: AccountInfo = client
        .request("vtt_getAccount", rpc_params![sender_addr])
        .await?;
    let nonce = account.nonce;

    // 2. Get gas config
    let gas_config: GasConfigRpc = client.request("vtt_getGasConfig", rpc_params![]).await?;

    // 3. Get chain status for chain_id
    let status: ChainStatus = client.request("vtt_chainStatus", rpc_params![]).await?;

    // 4. Build transaction payload
    let action = if command == "stake" {
        TransactionAction::Stake {
            validator: validator_addr,
            amount,
        }
    } else {
        TransactionAction::Unstake {
            validator: validator_addr,
            amount,
        }
    };

    let payload = TransactionPayload {
        chain_id: status.chain_id,
        nonce,
        gas_price: gas_config.min_gas_price,
        gas_limit: 100_000,
        action,
    };

    // 5. Sign
    let payload_bytes = borsh::to_vec(&payload)?;
    let signature = keypair.sign(&payload_bytes);
    let public_key = keypair.public_key();

    let signed_tx = SignedTransaction {
        payload,
        signature,
        public_key,
    };

    let tx_bytes = borsh::to_vec(&signed_tx)?;
    let tx_hex = hex::encode(&tx_bytes);

    // 6. Submit
    let tx_hash: H256 = client
        .request("vtt_sendTransaction", rpc_params![tx_hex])
        .await?;

    println!("Transaction submitted");
    println!("  Hash: 0x{}", hex::encode(tx_hash.as_bytes()));
    println!("  Action: {command}");
    println!("  Validator: 0x{}", hex::encode(validator_addr.as_bytes()));
    println!("  Amount: {amount_vtt} VTT");

    Ok(())
}
