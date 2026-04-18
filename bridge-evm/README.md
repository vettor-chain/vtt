# VTT Bridge — EVM side

Solidity contracts and deployment workflow for the VTT ↔ Ethereum/Base
bridge. The VTT-side relayer (`bin/vtt-relayer`) complements these
contracts.

## Contracts

| Contract | Purpose |
|---|---|
| `WVTT.sol` | ERC-20 wrapped VTT on the EVM chain. Only the configured bridge can `mint` / `burn` |
| `VTTBridge.sol` | User-facing deposit/release entrypoint. Holds USDT reserves for USDT deposits, mints wVTT on release, burns wVTT on deposit |

Key safety properties already in place:

- `onlyRelayer` gate on `releaseWVTT` / `releaseUSDT` — only the configured relayer address can release funds
- `processedWithdrawals[vttTxHash]` replay protection — each VTT-chain burn hash is consumable at most once
- `whenNotPaused` modifier with 2-day admin timelock on pause/unpause
- Admin rotation goes through `requestAdmin` + `applyAdmin` with the same timelock, preventing flash-rotation attacks
- Solidity ≥ 0.8 overflow reverts by default; no `unchecked` blocks on money-moving paths

## Build + test

```bash
forge build
forge test -vv
```

All tests must pass before deploying.

## Testnet deploy (Base Sepolia)

### Prerequisites

- [Foundry](https://book.getfoundry.sh/getting-started/installation) installed
- A funded deployer EOA on Base Sepolia (≥ 0.05 ETH for deploys + admin actions)
- A separate relayer EOA (the private key lives on the VTT relayer host, NOT on the deployer)
- USDT mock or real USDT contract address on Base Sepolia (use the Base Sepolia USDC as a stand-in for testing: `0x036CbD53842c5426634e7929541eC2318f3dCF7e`)

### Environment

```bash
export BASE_SEPOLIA_RPC=https://sepolia.base.org
export DEPLOYER_PRIVATE_KEY=0x<deployer-seed-hex>
export USDT_ADDRESS=0x036CbD53842c5426634e7929541eC2318f3dCF7e   # USDC as stand-in on testnet
export RELAYER_ADDRESS=0x<relayer-eoa-address>
export BRIDGE_FEE_BPS=10   # 0.10%
```

### Deploy

```bash
forge script script/Deploy.s.sol:DeployBridge \
  --rpc-url $BASE_SEPOLIA_RPC \
  --private-key $DEPLOYER_PRIVATE_KEY \
  --broadcast \
  --verify \
  --etherscan-api-key $BASESCAN_API_KEY
```

Record both addresses from the console output. They go into the VTT
relayer config (see below) and into `vtt-web`'s env vars.

### Post-deploy smoke test

1. From the relayer EOA, call `releaseWVTT(<some-vtt-tx-hash>, <recipient>, <amount>)` against the bridge. If the call is accepted and emits `Release(...)`, the relayer auth path is functional.
2. As a user, approve wVTT to the bridge and call `depositWVTT(amount, vttRecipient)` to burn wVTT. The `DepositWVTT` event is what the VTT relayer will observe on the VTT side to mint.

## Wiring the VTT relayer

After deploying the contracts, tell the VTT relayer where to find them:

```bash
export VTT_RPC_URL=https://testnet.vettor.org
export EVM_RPC_URL=https://sepolia.base.org
export EVM_BRIDGE_ADDRESS=0x<VTTBridge-address>
export EVM_WVTT_ADDRESS=0x<WVTT-address>
export EVM_RELAYER_PRIVATE_KEY=0x<relayer-seed-hex>
export VTT_BRIDGE_SIGNER_SEED=0x<vtt-side-relayer-seed-hex>   # must match the address set via ParameterChange(bridge_relayer)

cargo run -p vtt-relayer
```

On the VTT side, the relayer EOA's address must match the
`bridge_relayer` ParameterChange set via governance. Submitting a
`BridgeDeposit` tx from any other address is rejected by the executor.

## Pause / admin rotation

Pausing the bridge (emergency stop):

```bash
cast send $BRIDGE_ADDRESS "requestPause()" --rpc-url $BASE_SEPOLIA_RPC --private-key $ADMIN_KEY
# wait 2 days (timelock)
cast send $BRIDGE_ADDRESS "applyPause()" --rpc-url $BASE_SEPOLIA_RPC --private-key $ADMIN_KEY
```

Unpause and admin rotation follow the same request→wait→apply pattern.
Do not skip the timelock — it is the depositors' protection against a
compromised admin key.

## Mainnet checklist

Before deploying to mainnet:

- [ ] Two independent audits on `VTTBridge.sol` + `WVTT.sol`
- [ ] Relayer key in an HSM or MPC wallet, not a file on disk
- [ ] Admin key rotated to a multi-sig (Safe / Gnosis) with ≥ 3-of-5 threshold
- [ ] Paused by default on mainnet; pause can be lifted only after a dry-run on mainnet with small amounts
- [ ] Monitor set up: every `Deposit` / `Release` emits to a log sink, plus balance-drift alert on the USDT reserve and the wVTT supply

Do not deploy to mainnet without the full checklist.
