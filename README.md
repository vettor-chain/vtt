# VTT Blockchain

[![CI](https://github.com/vettor-chain/vtt/actions/workflows/ci.yml/badge.svg)](https://github.com/vettor-chain/vtt/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Layer 1 blockchain for tokenizing real-world assets. Built in Rust with DPoS consensus, WASM smart contracts, built-in DEX, cross-chain bridge, and on-chain governance.

> Currently in **testnet**. Mainnet launch planned for Q2 2026.

[Testnet Website](https://testnet.vettor.org) | [Whitepaper](https://testnet.vettor.org/whitepaper) | [Testnet Explorer](https://testnet.vettor.org/explorer) | [SDK](https://www.npmjs.com/package/@vettor/sdk)

## Features

- **DPoS Consensus** -- 3-second block times, up to 21 validators, BFT finality (2/3+1)
- **WASM Smart Contracts** -- Write contracts in Rust, compile to WASM, deterministic gas metering
- **Built-in DEX** -- Constant product AMM, multi-pool, LP tokens, auto-liquidity
- **Cross-chain Bridge** -- Custodial bridge to Ethereum and Base, wVTT (ERC-20)
- **On-chain Governance** -- Proposal system with 7-day voting, 33% quorum, 24h execution timelock, whitelisted `ParameterChange` keys
- **Multichain Registry** -- Governance-managed registry of app-chains (relay + future parachains). RegisterChain proposal persists a RegisteredChain record; cross-chain message routing is scaffolded but not yet live — CrossChainTransfer transactions are rejected explicitly until a relayer ships
- **RWA Tokenization** -- Fractional ownership, revenue distribution, asset governance, RedemptionPending lifecycle
- **Compliance** -- On-chain KYC flag, per-address jurisdiction, chain-wide whitelist/blacklist, `max_holders_per_asset` cap
- **Oracles** -- Treasury-gated feed registration, M-of-N authorised sources with median quorum aggregation
- **Slashing** -- Double-sign (5%) and downtime (0.1%) penalties, automatic double-sign detection
- **Deflationary** -- 70% of gas fees burned
- **Schema versioning** -- RocksDB stamped with a schema version on open; incompatible binaries refuse to start

## Architecture

```
vtt/
  bin/
    vtt-validator/   # Validator node (block production + RPC)
    vtt-node/        # Full node (sync only, no RPC)
    vtt-cli/         # Command-line wallet and tools
  crates/
    vtt-primitives/  # Core types (transactions, blocks, amounts)
    vtt-crypto/      # Ed25519 signatures, BLAKE3 hashing
    vtt-consensus/   # DPoS engine, finality, slashing, governance
    vtt-executor/    # Transaction execution, staking, contracts
    vtt-vm/          # WASM virtual machine (Wasmer 5)
    vtt-dex/         # AMM DEX (constant product, multi-pool)
    vtt-multichain/  # Cross-chain messaging
    vtt-network/     # libp2p P2P networking (gossipsub + kademlia)
    vtt-txpool/      # Transaction mempool with TTL
    vtt-storage/     # RocksDB storage with pruning
    vtt-state/       # Account and contract state
    vtt-chain/       # Chain management and fork choice
    vtt-genesis/     # Genesis configuration and validation
    vtt-telemetry/   # Prometheus metrics
    vtt-rpc/         # JSON-RPC server with CORS
  bridge-evm/        # Solidity bridge contracts (Foundry)
```

## Quick Start

```bash
# Build
cargo build --release

# Run dev validator (single node, in-memory)
./target/release/vtt-validator --dev

# Run testnet validator
./target/release/vtt-validator \
  --testnet \
  --seed <64-char-hex-seed> \
  --bootnodes /ip4/<ip>/tcp/30333/p2p/<peer-id> \
  --data-dir /data/vtt

# Generate a new keypair
./target/release/vtt-cli keygen

# Import from existing seed
./target/release/vtt-cli keygen --seed <hex>
```

## Key Parameters

| Parameter | Value |
|-----------|-------|
| Block time | 3 seconds |
| Max validators | 21 |
| Min self-stake | 100,000 VTT |
| Epoch length | 1,200 blocks (~1 hour) |
| Unbonding period | 21 days |
| Gas burn rate | 70% |
| Block reward split | 80% producer / 20% treasury |
| Annual inflation target | 5% (adjusted by staking ratio) |
| Governance voting period | 7 days (201,600 blocks) |
| Governance quorum | 33% |
| Governance pass threshold | >50% |
| Governance execution delay | 24 hours (28,800 blocks) |
| Double-sign slash | 5% of total stake |
| Downtime slash | 0.1% of total stake |
| Downtime threshold | >50% missed slots |
| Max contract size | 512 KB |
| Max WASM memory | 16 MB |
| Max call stack depth | 64 |
| Transaction TTL | 1 hour |
| Max tx pool size | 10,000 |
| RocksDB pruning | Keep last 100,000 blocks |

## Ports

| Port | Service |
|------|---------|
| 9944 | JSON-RPC (validator only) |
| 30333 | P2P networking |
| 9615 | Prometheus metrics |

## RPC API

The validator exposes a JSON-RPC 2.0 API on port 9944. Key methods:

| Method | Description |
|--------|-------------|
| `vtt_chainStatus` | Chain height, validator count, epoch |
| `vtt_getBlock` | Get block by hash |
| `vtt_getBlockByNumber` | Get block by number |
| `vtt_getAccount` | Get account info (balance, nonce, staking) |
| `vtt_getBalance` | Get VTT balance |
| `vtt_sendTransaction` | Submit signed transaction |
| `vtt_getValidators` | List active validators |
| `vtt_listPools` | List DEX liquidity pools |
| `vtt_getSwapQuote` | Get DEX swap quote |
| `vtt_listProposals` | List governance proposals |
| `vtt_getNodeMetrics` | Prometheus metrics (JSON) |
| `vtt_listOracles` | List registered oracle feeds |
| `vtt_getOracle` | Latest aggregated value for a feed |
| `vtt_isKycApproved` | On-chain KYC flag for an address |
| `vtt_getBridgeRelayer` | Address authorised to submit BridgeDeposit |
| `vtt_getSlashingHistory` | Slashing events recorded for a validator |
| `vtt_getTransactionReceipt` | Receipt (logs, gas used) for a tx hash |
| `vtt_listRegisteredChains` | List app-chains registered via governance |
| `vtt_getRegisteredChain` | Lookup a registered app-chain by chain_id |

Full API documentation: [docs](https://testnet.vettor.org/docs)

## TypeScript SDK

```bash
npm install @vettor/sdk
```

```typescript
import { VttClient, Wallet } from "@vettor/sdk";

// Connect to testnet
const client = new VttClient("https://testnet.vettor.org/api/rpc");
const wallet = await Wallet.fromSeed("your-hex-seed");

// Check balance
const account = await client.getAccount(wallet.address);
console.log(account.balance);

// Transfer VTT
const txHash = await client.transfer(wallet, "0xrecipient...", "1000000000000000000");
```

## Bridge (EVM)

Solidity contracts for the Ethereum/Base bridge:
- **VTTBridge.sol** -- Deposit/release with pause mechanism, 2-day admin timelock, replay protection
- **WVTT.sol** -- ERC-20 wrapped VTT token

```bash
cd bridge-evm
forge build
forge test -v
```

## Tests

```bash
# Rust unit tests (all crates)
cargo test --all

# Bridge contract tests
cd bridge-evm && forge test -v

# Format check
cargo fmt --all -- --check

# Lint
cargo clippy --all-targets --all-features
```

## Docker

```bash
# Build the validator image
docker build -t vtt-validator .

# Run
docker run -v /data/vtt:/data/vtt vtt-validator \
  vtt-validator --seed <hex> --genesis /etc/vtt/genesis.json --data-dir /data/vtt
```

Pre-built images available on [GitHub Container Registry](https://github.com/orgs/vettor-chain/packages).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Security

See [SECURITY.md](SECURITY.md) for vulnerability reporting.

## License

Licensed under the [Apache License 2.0](LICENSE).
