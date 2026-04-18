# VTT Validator Guide

## 1. System Requirements

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| CPU | 4 cores | 8+ cores |
| RAM | 8 GB | 16 GB |
| Disk | 100 GB SSD | 500 GB NVMe SSD |
| OS | Ubuntu 22.04+ / Debian 12+ | Ubuntu 24.04 LTS |
| Network | 100 Mbps | 1 Gbps |
| Rust | 1.75+ | Latest stable |

## 2. Building from Source

```bash
git clone https://github.com/alessandrovettor/vtt.git
cd vtt
cargo build --release --bin vtt-validator
cargo build --release --bin vtt-node
cargo build --release --bin vtt-cli
```

Binaries are output to `target/release/`.

## 3. Generating a Keypair

```bash
# Generate a random keypair
vtt-cli keygen

# Generate from a specific seed (32 bytes hex)
vtt-cli keygen --seed <64-char-hex>
```

Output:

```
Public Key: <hex>
Address:    <hex>
```

Store the seed securely. It is your validator signing key. Loss of the seed means loss of the validator identity.

## 4. Genesis File Format

Export the default dev genesis:

```bash
vtt-cli genesis --out genesis.json
```

The genesis file is a JSON object with the following top-level fields:

- `chain` -- chain parameters (chain_id, consensus config, gas config)
- `timestamp` -- genesis block timestamp (ISO 8601 or Unix seconds)
- `validators` -- initial validator set with addresses, stakes, and commission
- `allocations` -- initial account balances

For testnet/mainnet, you will receive the genesis file from the network coordinator. All nodes on the same network must use the same genesis file.

## 5. Running a Validator

```bash
vtt-validator \
  --seed <64-char-hex-seed> \
  --genesis genesis.json \
  --port 30333 \
  --rpc-port 9944 \
  --metrics-port 9615 \
  --data-dir /var/lib/vtt \
  --bootnodes /ip4/<IP>/tcp/30333/p2p/<PEER_ID>
```

### CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--seed <hex>` | (required in production) | 32-byte hex validator signing seed. Also accepts `VALIDATOR_SEED` env var. |
| `--genesis <path>` | dev default | Path to genesis JSON file. |
| `--port <port>` | 30333 | P2P listening port. |
| `--rpc-port <port>` | 9944 | JSON-RPC server port. |
| `--metrics-port <port>` | 9615 | Prometheus metrics HTTP port. |
| `--data-dir <path>` | in-memory | Directory for persistent RocksDB storage. |
| `--bootnodes <addrs>` | none | Comma-separated libp2p multiaddrs of boot nodes. |
| `--testnet` | off | Use the built-in testnet genesis and bootnodes. Accepted by both `vtt-validator` and `vtt-node`. |

### Environment Variables

| Variable | Description |
|----------|-------------|
| `VALIDATOR_SEED` | Alternative to `--seed` flag. |
| `RUST_LOG` | Log level filter (e.g., `info`, `debug`, `vtt_rpc=debug`). |

## 6. Running a Non-Validator Node

A full node syncs the chain but does not produce blocks.

> **Note:** `vtt-node` does NOT start an RPC server. Only `vtt-validator` exposes the JSON-RPC API on port 9944. If you need RPC access, run `vtt-validator` without a seed (it will sync but not produce blocks) or point your RPC clients at an existing validator.

```bash
vtt-node \
  --genesis genesis.json \
  --port 30334 \
  --metrics-port 9616 \
  --data-dir /var/lib/vtt-node \
  --bootnodes /ip4/<VALIDATOR_IP>/tcp/30333/p2p/<PEER_ID>
```

Use `--dev` for local development mode (uses built-in dev genesis).

## 7. Staking to Register as a Validator

To register as a validator, you must self-stake at least the minimum self-stake amount (configured in genesis consensus params, default 100,000 VTT).

```bash
# Self-stake (validator address = your own address)
vtt-cli stake \
  --validator <your-address> \
  --amount 10000 \
  --seed <your-seed-hex> \
  --rpc http://127.0.0.1:9944

# Delegate to another validator
vtt-cli stake \
  --validator <validator-address> \
  --amount 5000 \
  --seed <your-seed-hex> \
  --rpc http://127.0.0.1:9944

# Unstake (begins unbonding period)
vtt-cli unstake \
  --validator <validator-address> \
  --amount 5000 \
  --seed <your-seed-hex> \
  --rpc http://127.0.0.1:9944
```

Unstaking initiates an unbonding period (configured in consensus params). Tokens are not available until the unbonding period completes.

## 8. Monitoring

### Prometheus Metrics

The metrics endpoint is available at `http://<host>:<metrics-port>/` and returns Prometheus text format.

Available metrics:

| Metric | Type | Description |
|--------|------|-------------|
| `vtt_block_height` | Gauge | Current block height |
| `vtt_connected_peers` | Gauge | Number of connected peers |
| `vtt_txpool_size` | Gauge | Transaction pool size |
| `vtt_blocks_imported_total` | Counter | Total blocks imported |
| `vtt_transactions_executed_total` | Counter | Total transactions executed |
| `vtt_block_import_duration_seconds` | Histogram | Block import duration |
| `vtt_current_epoch` | Gauge | Current DPoS epoch |
| `vtt_active_validators` | Gauge | Number of active validators |

### Prometheus Configuration

```yaml
scrape_configs:
  - job_name: 'vtt-validator'
    static_configs:
      - targets: ['localhost:9615']
```

### JSON-RPC Metrics

```bash
curl -X POST http://localhost:9944 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"vtt_getNodeMetrics","params":[]}'
```

## 9. Backup and Recovery

### Data Directory

All persistent state is stored in the `--data-dir` directory (RocksDB). To back up:

```bash
# Stop the node first
systemctl stop vtt-validator

# Copy the data directory
cp -r /var/lib/vtt /var/lib/vtt-backup-$(date +%Y%m%d)

# Restart
systemctl start vtt-validator
```

### Key Backup

Back up your validator seed securely (offline, encrypted). The seed is the only way to recover your validator identity.

### Recovery

To recover from backup:

1. Stop the node.
2. Replace the data directory with the backup.
3. Start the node with the same genesis file and seed.

The node will catch up to the current chain head from peers.

To start fresh (resync from genesis):

1. Stop the node.
2. Delete the data directory.
3. Start the node. It will rebuild state from genesis and sync from peers.

## 10. Network Ports

| Port | Protocol | Service | Access |
|------|----------|---------|--------|
| 30333 | TCP | P2P networking (libp2p) | Open to other nodes |
| 9944 | TCP | JSON-RPC API | Restrict to trusted clients |
| 9615 | TCP | Prometheus metrics | Restrict to monitoring infra |

### Firewall Configuration (ufw)

```bash
# P2P -- open to all
sudo ufw allow 30333/tcp

# RPC -- restrict to localhost or specific IPs
sudo ufw allow from 127.0.0.1 to any port 9944

# Metrics -- restrict to monitoring server
sudo ufw allow from <PROMETHEUS_IP> to any port 9615
```

## 11. Storage Pruning

RocksDB automatically prunes old block bodies and receipts every 10,000 blocks. The node keeps the most recent 100,000 blocks (~3.5 days at 3s/block). Manual compaction runs after each pruning cycle to reclaim disk space.

## 12. Transaction Pool

- Transactions expire after 1 hour (configurable via `tx_ttl_secs`)
- Expired transactions are evicted every 60 seconds
- Max pool size: 10,000 transactions
- Max per account: 100 pending transactions

## 13. Peer Management

- Maximum 50 connected peers (configurable)
- Maximum 3 connections per IP address
- Peer reputation system with automatic banning (1 hour) for misbehavior
- Messages over 4 MB are rejected and the sender is penalized

## 14. Finality

- BFT finality with 2/3+1 validator threshold
- Finalized blocks are persisted and cannot be reverted
- Fork choice rejects any chain that would revert past the finalized block

## 15. RPC Security

- CORS enabled (configurable)
- Per-IP rate limiting on transaction submission (10/sec default)
- Per-IP rate limiting on heavy reads — `listTransactions`, `getTransactionsByAddress`, `getBridgeWithdrawals` (60/sec default)
- Request body size limit: 4 MiB (fits DeployContract with up to ~1 MiB of WASM)
- `sendTransaction` hex payload capped at 2 MiB before decode
- Chain-walking reads bounded to the most recent 20,000 blocks per request
- Internal errors are logged server-side and redacted in RPC responses
- Response pagination: max 100 items per query

## 16. Deterministic `peer_id`

Every validator derives its libp2p identity from its signing seed via a
domain-separated blake3 hash (`blake3("vtt:libp2p:" ++ seed)`). This
means:

- Restarting a node does not change its `peer_id`; bootnode multiaddrs
  configured on peers stay valid.
- Leaking a `peer_id` does not reveal the consensus signing key, and
  vice-versa — the tag domain-separates the two uses of the seed.

An operator publishing a bootnode multiaddr for others to dial can
therefore compute it once (off the seed) and keep it stable.

## 17. DB Schema Versioning

On first open, RocksDB gets a `schema:version` stamp under ChainMeta. On every
subsequent start the validator verifies the stamp against the binary's
`DB_SCHEMA_VERSION`:

- Match → proceed normally.
- Absent → stamp current version (fresh or legacy DB) and proceed.
- Mismatch → panic on startup; a binary mismatched with the on-disk schema
  refuses to run rather than silently corrupting state.

When upgrading across a schema bump, wipe the data directory (testnet) or
follow the migration documented in the release notes.
