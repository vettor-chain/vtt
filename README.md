# VTT Blockchain

[![CI](https://github.com/vettor-chain/vtt/actions/workflows/ci.yml/badge.svg)](https://github.com/vettor-chain/vtt/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Layer 1 blockchain for tokenizing real-world assets. Built in Rust with DPoS consensus, WASM smart contracts, built-in DEX, cross-chain bridge, and on-chain governance.

[Website](https://testnet.vettor.org) | [Whitepaper](https://testnet.vettor.org/whitepaper) | [Explorer](https://testnet.vettor.org/explorer) | [SDK](https://www.npmjs.com/package/@vettor/sdk)

## Architecture

```
vtt/
  bin/
    vtt-node/        # Full node (sync, no block production)
    vtt-validator/   # Validator node (block production + RPC)
    vtt-cli/         # Command-line wallet and tools
  crates/
    vtt-primitives/  # Core types (transactions, blocks, amounts)
    vtt-crypto/      # Ed25519 signatures, BLAKE3 hashing
    vtt-consensus/   # DPoS engine, finality, slashing, governance
    vtt-executor/    # Transaction execution, staking, contracts
    vtt-vm/          # WASM virtual machine (Wasmer 5)
    vtt-dex/         # AMM DEX (constant product, multi-pool)
    vtt-multichain/  # Cross-chain messaging
    vtt-network/     # libp2p P2P networking
    vtt-txpool/      # Transaction mempool with TTL
    vtt-storage/     # RocksDB storage with pruning
    vtt-state/       # Account and contract state
    vtt-chain/       # Chain management and fork choice
    vtt-genesis/     # Genesis configuration and validation
    vtt-telemetry/   # Prometheus metrics
    vtt-rpc/         # JSON-RPC server
  bridge-evm/        # Solidity bridge contracts (Foundry)
```

## Quick Start

```bash
# Build
cargo build --release

# Run dev validator
./target/release/vtt-validator --dev

# Run testnet
./target/release/vtt-validator --testnet --seed <hex-seed> --bootnodes <addr>

# Generate keypair
./target/release/vtt-cli keygen
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
| Governance voting | 7 days |
| Governance quorum | 33% |
| Governance timelock | 24 hours |
| Double-sign slash | 5% of stake |
| Downtime slash | 0.1% of stake |
| Max contract size | 512 KB |
| Max WASM memory | 16 MB |
| Transaction TTL | 1 hour |

## Ports

| Port | Service |
|------|---------|
| 9944 | JSON-RPC (validator only) |
| 30333 | P2P |
| 9615 | Prometheus metrics |

## Bridge (EVM)

Solidity contracts for the Ethereum/Base bridge. See `bridge-evm/`.

```bash
cd bridge-evm
forge build
forge test -v
```

## Tests

```bash
cargo test          # All Rust tests
cd bridge-evm && forge test  # Solidity tests
```

## License

Proprietary.
