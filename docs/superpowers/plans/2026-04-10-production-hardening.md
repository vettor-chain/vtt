# VTT Platform Production Hardening Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden VTT platform (blockchain, web, infra) for mainnet production launch.

**Architecture:** Three codebases — `vtt/` (Rust blockchain), `vtt-web/` (Next.js), parent `Lavoro/` (Docker Compose, infra). Changes are isolated per subsystem and can be parallelized. Bridge EVM contracts at `vtt/bridge-evm/`.

**Tech Stack:** Rust (blockchain), TypeScript/Next.js (web), Solidity/Foundry (bridge), Docker Compose (infra), PostgreSQL, RocksDB, libp2p, Wasmer.

---

## Phase 1: Critical Security (Blockchain)

### Task 1: WASM VM Limits

**Files:**
- Modify: `vtt/crates/vtt-vm/src/engine.rs:42` (execute method)
- Modify: `vtt/crates/vtt-vm/src/gas.rs:42` (add constants)
- Modify: `vtt/crates/vtt-executor/src/lib.rs:600` (deploy_contract)

- [ ] **Step 1: Add VM limit constants to gas.rs**

In `vtt/crates/vtt-vm/src/gas.rs`, after line 55 (ORACLE_READ):

```rust
    pub const MAX_CONTRACT_SIZE: usize = 512 * 1024; // 512 KB
    pub const MAX_WASM_MEMORY_PAGES: u32 = 256;      // 16 MB (256 * 64KB)
    pub const MAX_CALL_STACK_DEPTH: u32 = 64;
```

- [ ] **Step 2: Enforce contract size limit in executor**

In `vtt/crates/vtt-executor/src/lib.rs`, in `execute_deploy_contract` (around line 600), add before WASM compilation:

```rust
if bytecode.len() > GasCosts::MAX_CONTRACT_SIZE {
    return Err(ExecutionError::ContractTooLarge {
        size: bytecode.len(),
        max: GasCosts::MAX_CONTRACT_SIZE,
    });
}
```

Add variant to `ExecutionError` enum (line 21):

```rust
ContractTooLarge { size: usize, max: usize },
```

- [ ] **Step 3: Enforce WASM memory limits in engine.rs**

In `vtt/crates/vtt-vm/src/engine.rs`, in the `execute` method (line 42), when creating the Wasmer instance, set memory limits:

```rust
let mut store = Store::default();
// Limit memory to MAX_WASM_MEMORY_PAGES
let memory_type = MemoryType::new(1, Some(GasCosts::MAX_WASM_MEMORY_PAGES));
```

If using `wasmer::Module::new`, add a middleware or tunables that cap memory. The exact API depends on the wasmer version — check `Cargo.toml` for the version and adjust accordingly.

- [ ] **Step 4: Add call stack depth tracking**

In `vtt/crates/vtt-vm/src/context.rs`, add to `ExecutionContext`:

```rust
pub call_depth: u32,
pub max_call_depth: u32,
```

Initialize `max_call_depth` to `GasCosts::MAX_CALL_STACK_DEPTH`. In any host function that triggers nested execution, check:

```rust
if context.call_depth >= context.max_call_depth {
    return Err(VmError::CallStackOverflow);
}
```

Add `CallStackOverflow` to `VmError` in `error.rs`.

- [ ] **Step 5: Run tests**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test -p vtt-vm -p vtt-executor
```

- [ ] **Step 6: Commit**

```bash
git add crates/vtt-vm/ crates/vtt-executor/src/lib.rs
git commit -m "security: add WASM VM limits — contract size, memory, call depth"
```

---

### Task 2: RPC CORS Support

**Files:**
- Modify: `vtt/crates/vtt-rpc/src/server.rs:1105-1296` (RpcServer)
- Modify: `vtt/Cargo.toml` (add tower-http if needed)

- [ ] **Step 1: Add CORS to RPC server builder**

In `vtt/crates/vtt-rpc/src/server.rs`, find where the jsonrpsee server is built (around line 1149). Add CORS configuration:

```rust
use jsonrpsee::server::ServerBuilder;

let server = ServerBuilder::default()
    .set_http_middleware(
        tower::ServiceBuilder::new()
            .layer(
                tower_http::cors::CorsLayer::new()
                    .allow_origin(tower_http::cors::Any)
                    .allow_methods([hyper::Method::POST])
                    .allow_headers([hyper::header::CONTENT_TYPE])
            )
    )
    .build(&addr)
    .await?;
```

Add `tower-http` with `cors` feature to `vtt-rpc/Cargo.toml`:

```toml
tower-http = { version = "0.5", features = ["cors"] }
tower = "0.4"
hyper = "1"
```

- [ ] **Step 2: Run tests**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test -p vtt-rpc
```

- [ ] **Step 3: Commit**

```bash
git add crates/vtt-rpc/
git commit -m "security: add CORS support to RPC server"
```

---

### Task 3: RPC Per-IP Rate Limiting and Request Size Limits

**Files:**
- Modify: `vtt/crates/vtt-rpc/src/server.rs:203-246` (RateLimiter)

- [ ] **Step 1: Replace global RateLimiter with per-IP**

In `vtt/crates/vtt-rpc/src/server.rs`, replace the `RateLimiter` struct (lines 203-246):

```rust
use std::collections::HashMap;
use std::net::IpAddr;

pub struct PerIpRateLimiter {
    max_calls_per_second: u64,
    clients: Mutex<HashMap<IpAddr, (u64, Instant)>>,
}

impl PerIpRateLimiter {
    pub fn new(max_calls_per_second: u64) -> Self {
        Self {
            max_calls_per_second,
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub fn check(&self, ip: IpAddr) -> bool {
        let mut clients = self.clients.lock().unwrap();
        let now = Instant::now();
        let entry = clients.entry(ip).or_insert((0, now));

        if now.duration_since(entry.1).as_secs() >= 1 {
            *entry = (1, now);
            return true;
        }

        if entry.0 >= self.max_calls_per_second {
            return false;
        }

        entry.0 += 1;
        true
    }

    /// Periodically clean stale entries (call from a background task)
    pub fn cleanup(&self) {
        let mut clients = self.clients.lock().unwrap();
        let now = Instant::now();
        clients.retain(|_, (_, last)| now.duration_since(*last).as_secs() < 60);
    }
}
```

- [ ] **Step 2: Add request body size limit**

In the RPC server builder, add a body size limit layer:

```rust
.set_http_middleware(
    tower::ServiceBuilder::new()
        .layer(tower_http::limit::RequestBodyLimitLayer::new(1024 * 1024)) // 1MB max
        .layer(cors_layer)
)
```

Add `limit` feature to tower-http in Cargo.toml:

```toml
tower-http = { version = "0.5", features = ["cors", "limit"] }
```

- [ ] **Step 3: Add response pagination to dangerous methods**

In `server.rs`, find `list_transactions` (around line 1033). Add `limit` and `offset` parameters, cap `limit` to 100:

```rust
async fn list_transactions(&self, limit: Option<u64>, offset: Option<u64>) -> RpcResult<Vec<RpcTransaction>> {
    let limit = limit.unwrap_or(50).min(100);
    let offset = offset.unwrap_or(0);
    // ... existing logic with limit/offset applied
}
```

Do the same for `get_block_range` — cap `count` to 100.

- [ ] **Step 4: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test -p vtt-rpc
git add crates/vtt-rpc/
git commit -m "security: per-IP rate limiting, request size limits, response pagination"
```

---

### Task 4: Slashing Execution

**Files:**
- Modify: `vtt/crates/vtt-executor/src/lib.rs` (add slashing logic)
- Modify: `vtt/crates/vtt-consensus/src/slashing.rs:48` (SlashRecord persistence)
- Modify: `vtt/crates/vtt-state/src/statedb.rs` (slash state methods)

- [ ] **Step 1: Add slashing state methods to StateDB**

In the StateDB (find exact file with `grep -r "pub struct StateDB"`), add methods to track slashing:

```rust
pub fn apply_slash(&mut self, validator: &Address, amount: Amount) -> Result<(), StateError> {
    let mut account = self.get_account(validator)?;
    let slash = amount.min(account.staked);
    account.staked = account.staked.checked_sub(slash).unwrap_or(Amount::zero());
    self.set_account(validator, account)?;
    Ok(())
}

pub fn record_slash(&mut self, record: SlashRecord) -> Result<(), StateError> {
    let key = format!("slash:{}:{}", record.validator, record.epoch);
    let encoded = borsh::to_vec(&record)?;
    self.put_raw(Column::ChainMeta, key.as_bytes(), &encoded)?;
    Ok(())
}

pub fn is_slashed_in_epoch(&self, validator: &Address, epoch: Epoch) -> Result<bool, StateError> {
    let key = format!("slash:{}:{}", validator, epoch);
    self.contains_raw(Column::ChainMeta, key.as_bytes())
}
```

- [ ] **Step 2: Execute slashing in block processing**

In `vtt/crates/vtt-executor/src/lib.rs`, add a function called after block execution:

```rust
pub fn process_slashing_evidence(
    chain: &mut Chain,
    evidence: &[DoubleSignEvidence],
    consensus_params: &ConsensusParams,
) -> Result<Vec<SlashRecord>, ExecutionError> {
    let mut records = Vec::new();
    let state = chain.state_mut();

    for ev in evidence {
        if !ev.is_valid() {
            continue;
        }
        let offender = ev.offender();
        let account = state.get_account(&offender)?;
        let slash_amount = calculate_double_sign_slash(
            account.staked,
            consensus_params.double_sign_slash_bps,
        );

        state.apply_slash(&offender, slash_amount)?;

        let record = SlashRecord {
            validator: offender,
            reason: SlashingReason::DoubleSigning,
            amount: slash_amount,
            epoch: chain.current_epoch(),
            block_number: chain.best_block_number(),
        };
        state.record_slash(record.clone())?;
        records.push(record);
    }

    Ok(records)
}
```

- [ ] **Step 3: Integrate into block import**

In `vtt-node/src/main.rs`, find the block import logic and call `process_slashing_evidence` after block execution.

- [ ] **Step 4: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test
git add crates/vtt-executor/ crates/vtt-consensus/ crates/vtt-state/ bin/vtt-node/
git commit -m "security: implement slashing execution — double-sign evidence now reduces validator stake"
```

---

### Task 5: Transaction TTL in TxPool

**Files:**
- Modify: `vtt/crates/vtt-txpool/src/lib.rs:27-44` (TxPoolConfig)

- [ ] **Step 1: Add TTL to TxPoolConfig and pool entries**

In `vtt/crates/vtt-txpool/src/lib.rs`, add to `TxPoolConfig` (around line 27):

```rust
pub tx_ttl_secs: u64, // default: 3600 (1 hour)
```

Add default:

```rust
impl Default for TxPoolConfig {
    fn default() -> Self {
        Self {
            max_size: 10_000,
            max_per_account: 100,
            min_gas_price: Amount::from_raw(1_000_000_000),
            tx_ttl_secs: 3600,
        }
    }
}
```

- [ ] **Step 2: Track insertion time per transaction**

Add a wrapper around stored transactions:

```rust
struct PoolEntry {
    tx: SignedTransaction,
    sender: Address,
    inserted_at: Instant,
}
```

Update the internal storage from `HashMap<H256, SignedTransaction>` to `HashMap<H256, PoolEntry>`.

- [ ] **Step 3: Add eviction method**

```rust
pub fn evict_expired(&mut self) {
    let now = Instant::now();
    let ttl = Duration::from_secs(self.config.tx_ttl_secs);
    let expired: Vec<H256> = self.entries.iter()
        .filter(|(_, e)| now.duration_since(e.inserted_at) > ttl)
        .map(|(h, _)| *h)
        .collect();
    for hash in expired {
        self.remove(&hash);
    }
}
```

Call `evict_expired()` periodically from the node's main loop (e.g., every 60 seconds).

- [ ] **Step 4: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test -p vtt-txpool
git add crates/vtt-txpool/ bin/vtt-node/
git commit -m "feat: add transaction TTL to mempool — evict stale transactions after 1 hour"
```

---

### Task 6: P2P Peer Banning and Connection Limits

**Files:**
- Modify: `vtt/crates/vtt-network/src/service.rs:36-41` (NetworkService)
- Modify: `vtt/crates/vtt-network/src/config.rs`

- [ ] **Step 1: Add connection limits to network config**

In `vtt/crates/vtt-network/src/config.rs`, add:

```rust
pub max_peers: u32,          // default: 50
pub max_peers_per_ip: u32,   // default: 3
pub ban_duration_secs: u64,  // default: 3600
```

- [ ] **Step 2: Add peer reputation tracking**

In `vtt/crates/vtt-network/src/service.rs`, add a reputation system:

```rust
use std::collections::HashMap;
use std::net::IpAddr;

struct PeerReputation {
    score: i32,
    banned_until: Option<Instant>,
}

impl NetworkService {
    fn is_banned(&self, peer_id: &PeerId) -> bool {
        self.reputations.get(peer_id)
            .map(|r| r.banned_until.map_or(false, |t| Instant::now() < t))
            .unwrap_or(false)
    }

    fn penalize(&mut self, peer_id: &PeerId, penalty: i32) {
        let rep = self.reputations.entry(*peer_id).or_insert(PeerReputation {
            score: 100,
            banned_until: None,
        });
        rep.score -= penalty;
        if rep.score <= 0 {
            rep.banned_until = Some(Instant::now() + Duration::from_secs(self.config.ban_duration_secs));
            let _ = self.swarm.disconnect_peer_id(*peer_id);
        }
    }
}
```

- [ ] **Step 3: Enforce max peers in swarm config**

In the swarm builder (service.rs, around line 91-102), add connection limits:

```rust
.with_swarm_config(|cfg| {
    cfg.with_idle_connection_timeout(Duration::from_secs(60))
       .with_max_negotiating_inbound_streams(128)
})
```

Use libp2p's `ConnectionLimits` behaviour to cap total connections.

- [ ] **Step 4: Add message size validation**

In the gossipsub message handler, reject messages over a size limit:

```rust
const MAX_GOSSIP_MESSAGE_SIZE: usize = 4 * 1024 * 1024; // 4 MB

// In message handler:
if message.data.len() > MAX_GOSSIP_MESSAGE_SIZE {
    self.penalize(&source, 50);
    return;
}
```

- [ ] **Step 5: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test -p vtt-network
git add crates/vtt-network/
git commit -m "security: add peer banning, connection limits, message size validation"
```

---

## Phase 2: Critical Security (Web)

### Task 7: Fix CSP — Remove unsafe-inline

**Files:**
- Modify: `vtt-web/next.config.ts:17-28`

- [ ] **Step 1: Update CSP directives**

In `vtt-web/next.config.ts`, replace the CSP string (lines 17-28). Remove `'unsafe-inline'` from `script-src` and `'unsafe-eval'`. Keep `'unsafe-inline'` for `style-src` only (Next.js requires it for styled-jsx):

```typescript
const csp = [
  "default-src 'self'",
  "script-src 'self' https://challenges.cloudflare.com https://js.stripe.com https://static.cloudflareinsights.com",
  "style-src 'self' 'unsafe-inline' https://fonts.googleapis.com",
  "font-src 'self' https://fonts.gstatic.com",
  "img-src 'self' data: blob: https:",
  "connect-src 'self' https://api.stripe.com https://challenges.cloudflare.com https://*.r2.cloudflarestorage.com https://cloudflareinsights.com",
  "frame-src https://challenges.cloudflare.com https://js.stripe.com",
  "object-src 'none'",
  "base-uri 'self'",
  "form-action 'self'",
].join("; ");
```

Note: If removing `'unsafe-inline'` from `script-src` breaks Stripe or Turnstile widgets, add nonce-based CSP instead. Test thoroughly.

- [ ] **Step 2: Test locally**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web && npm run build && npm run start
```

Verify in browser: Stripe checkout, Turnstile CAPTCHA, and all pages load without CSP errors in console.

- [ ] **Step 3: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web
git add next.config.ts
git commit -m "security: remove unsafe-inline from CSP script-src"
```

---

### Task 8: KYC PII Encryption at Rest

**Files:**
- Create: `vtt-web/src/lib/crypto/field-encryption.ts`
- Modify: `vtt-web/src/app/api/kyc/submit/route.ts`
- Modify: `vtt-web/src/app/api/admin/kyc/[id]/route.ts`

- [ ] **Step 1: Create field-level encryption utility**

Create `vtt-web/src/lib/crypto/field-encryption.ts`:

```typescript
import { createCipheriv, createDecipheriv, randomBytes, scryptSync } from "crypto";

const ALGORITHM = "aes-256-gcm";
const KEY = (() => {
  const secret = process.env.ENCRYPTION_KEY;
  if (!secret || secret.length < 32) {
    throw new Error("ENCRYPTION_KEY must be at least 32 characters");
  }
  return scryptSync(secret, "vtt-kyc-salt", 32);
})();

export function encrypt(plaintext: string): string {
  const iv = randomBytes(16);
  const cipher = createCipheriv(ALGORITHM, KEY, iv);
  const encrypted = Buffer.concat([cipher.update(plaintext, "utf8"), cipher.final()]);
  const tag = cipher.getAuthTag();
  return `${iv.toString("hex")}:${tag.toString("hex")}:${encrypted.toString("hex")}`;
}

export function decrypt(ciphertext: string): string {
  const [ivHex, tagHex, dataHex] = ciphertext.split(":");
  const iv = Buffer.from(ivHex, "hex");
  const tag = Buffer.from(tagHex, "hex");
  const data = Buffer.from(dataHex, "hex");
  const decipher = createDecipheriv(ALGORITHM, KEY, iv);
  decipher.setAuthTag(tag);
  return decipher.update(data).toString("utf8") + decipher.final("utf8");
}

const PII_FIELDS = ["fullName", "email", "dateOfBirth", "nationality", "residentialAddress"] as const;

export function encryptPii(data: Record<string, string>): Record<string, string> {
  const result = { ...data };
  for (const field of PII_FIELDS) {
    if (result[field]) {
      result[field] = encrypt(result[field]);
    }
  }
  return result;
}

export function decryptPii(data: Record<string, string | null>): Record<string, string | null> {
  const result = { ...data };
  for (const field of PII_FIELDS) {
    if (result[field]) {
      try {
        result[field] = decrypt(result[field]);
      } catch {
        // Field may not be encrypted (legacy data)
      }
    }
  }
  return result;
}
```

- [ ] **Step 2: Encrypt PII on KYC submission**

In `vtt-web/src/app/api/kyc/submit/route.ts`, before the Prisma `create` call, encrypt PII fields:

```typescript
import { encryptPii } from "@/lib/crypto/field-encryption";

// Before prisma.kycSubmission.create:
const encrypted = encryptPii({ fullName, email, dateOfBirth, nationality, residentialAddress });
```

Use `encrypted.fullName`, `encrypted.email`, etc. in the create call.

- [ ] **Step 3: Decrypt PII on admin read**

In `vtt-web/src/app/api/admin/kyc/[id]/route.ts`, after fetching the submission, decrypt:

```typescript
import { decryptPii } from "@/lib/crypto/field-encryption";

// After prisma.kycSubmission.findUnique:
const decrypted = decryptPii(submission);
return Response.json(decrypted);
```

- [ ] **Step 4: Add ENCRYPTION_KEY to env files**

Add to all `.env.*` files in `/Users/alessandrovettor/Documents/Lavoro/`:

```
ENCRYPTION_KEY=<generate-64-char-random-hex>
```

Add to `vtt-web/.env.example`:

```
ENCRYPTION_KEY=change-me-at-least-32-characters-long
```

- [ ] **Step 5: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web
git add src/lib/crypto/ src/app/api/kyc/ src/app/api/admin/kyc/ .env.example
git commit -m "security: encrypt KYC PII fields at rest with AES-256-GCM"
```

---

### Task 9: Environment Validation at Startup

**Files:**
- Create: `vtt-web/src/lib/env.ts`
- Modify: `vtt-web/src/app/layout.tsx` (import for side-effect)

- [ ] **Step 1: Create env validation module**

Create `vtt-web/src/lib/env.ts`:

```typescript
function requireEnv(name: string, minLength = 1): string {
  const value = process.env[name];
  if (!value || value.length < minLength) {
    throw new Error(`Missing or invalid env var: ${name} (min length: ${minLength})`);
  }
  return value;
}

function optionalEnv(name: string): string | undefined {
  return process.env[name] || undefined;
}

// Validate on import (server-side only)
if (typeof window === "undefined") {
  const required = [
    "DATABASE_URL",
    "JWT_SECRET",
    "STRIPE_SECRET_KEY",
    "STRIPE_WEBHOOK_SECRET",
    "VTT_RPC_URL",
    "HD_WALLET_SEED",
    "TREASURY_SEED",
    "R2_ACCOUNT_ID",
    "R2_ACCESS_KEY",
    "R2_SECRET_KEY",
    "R2_BUCKET",
    "CRON_SECRET",
    "ENCRYPTION_KEY",
  ];

  const missing = required.filter((name) => !process.env[name]);
  if (missing.length > 0) {
    throw new Error(`Missing required environment variables:\n  ${missing.join("\n  ")}`);
  }

  if ((process.env.JWT_SECRET?.length ?? 0) < 32) {
    throw new Error("JWT_SECRET must be at least 32 characters");
  }

  if ((process.env.ENCRYPTION_KEY?.length ?? 0) < 32) {
    throw new Error("ENCRYPTION_KEY must be at least 32 characters");
  }
}

export const env = {
  DATABASE_URL: process.env.DATABASE_URL!,
  JWT_SECRET: process.env.JWT_SECRET!,
  NODE_ENV: process.env.NODE_ENV ?? "development",
  VTT_RPC_URL: process.env.VTT_RPC_URL ?? "http://localhost:9944",
  NEXT_PUBLIC_NETWORK: process.env.NEXT_PUBLIC_NETWORK ?? "testnet",
} as const;
```

- [ ] **Step 2: Import in root layout for fail-fast**

In `vtt-web/src/app/layout.tsx`, add at top:

```typescript
import "@/lib/env";
```

- [ ] **Step 3: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web
git add src/lib/env.ts src/app/layout.tsx
git commit -m "security: fail-fast environment validation at startup"
```

---

### Task 10: React Error Boundaries and 404/500 Pages

**Files:**
- Create: `vtt-web/src/app/not-found.tsx`
- Create: `vtt-web/src/app/error.tsx`
- Create: `vtt-web/src/app/global-error.tsx`

- [ ] **Step 1: Create 404 page**

Create `vtt-web/src/app/not-found.tsx`:

```tsx
import Link from "next/link";

export default function NotFound() {
  return (
    <div className="flex min-h-screen items-center justify-center">
      <div className="text-center">
        <h1 className="text-6xl font-bold text-white">404</h1>
        <p className="mt-4 text-lg text-white/60">Page not found</p>
        <Link href="/" className="mt-8 inline-block rounded-lg bg-white/10 px-6 py-3 text-white hover:bg-white/20">
          Go home
        </Link>
      </div>
    </div>
  );
}
```

- [ ] **Step 2: Create error boundary**

Create `vtt-web/src/app/error.tsx`:

```tsx
"use client";

export default function Error({ error, reset }: { error: Error; reset: () => void }) {
  return (
    <div className="flex min-h-screen items-center justify-center">
      <div className="text-center">
        <h1 className="text-4xl font-bold text-white">Something went wrong</h1>
        <p className="mt-4 text-white/60">{error.message || "An unexpected error occurred"}</p>
        <button
          onClick={reset}
          className="mt-8 rounded-lg bg-white/10 px-6 py-3 text-white hover:bg-white/20"
        >
          Try again
        </button>
      </div>
    </div>
  );
}
```

- [ ] **Step 3: Create global error boundary**

Create `vtt-web/src/app/global-error.tsx`:

```tsx
"use client";

export default function GlobalError({ error, reset }: { error: Error; reset: () => void }) {
  return (
    <html>
      <body className="bg-black">
        <div className="flex min-h-screen items-center justify-center">
          <div className="text-center">
            <h1 className="text-4xl font-bold text-white">Critical error</h1>
            <p className="mt-4 text-white/60">{error.message}</p>
            <button
              onClick={reset}
              className="mt-8 rounded-lg bg-white/10 px-6 py-3 text-white hover:bg-white/20"
            >
              Try again
            </button>
          </div>
        </div>
      </body>
    </html>
  );
}
```

- [ ] **Step 4: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web
git add src/app/not-found.tsx src/app/error.tsx src/app/global-error.tsx
git commit -m "feat: add 404, error boundary, and global error pages"
```

---

### Task 11: Web API Global Rate Limiting

**Files:**
- Create: `vtt-web/src/middleware.ts` (or modify if exists)

- [ ] **Step 1: Add rate limiting middleware**

Create or modify `vtt-web/src/middleware.ts`:

```typescript
import { NextRequest, NextResponse } from "next/server";

const rateLimit = new Map<string, { count: number; resetAt: number }>();

const RATE_LIMIT = 60;      // requests per window
const WINDOW_MS = 60_000;   // 1 minute

function getRateLimitKey(req: NextRequest): string {
  return req.headers.get("x-forwarded-for")?.split(",")[0]?.trim()
    ?? req.headers.get("cf-connecting-ip")
    ?? "unknown";
}

function checkRateLimit(key: string): { allowed: boolean; remaining: number } {
  const now = Date.now();
  const entry = rateLimit.get(key);

  if (!entry || now > entry.resetAt) {
    rateLimit.set(key, { count: 1, resetAt: now + WINDOW_MS });
    return { allowed: true, remaining: RATE_LIMIT - 1 };
  }

  entry.count++;
  if (entry.count > RATE_LIMIT) {
    return { allowed: false, remaining: 0 };
  }

  return { allowed: true, remaining: RATE_LIMIT - entry.count };
}

// Cleanup stale entries every 5 minutes
setInterval(() => {
  const now = Date.now();
  for (const [key, entry] of rateLimit) {
    if (now > entry.resetAt) rateLimit.delete(key);
  }
}, 300_000);

export function middleware(req: NextRequest) {
  // Only rate-limit API routes
  if (!req.nextUrl.pathname.startsWith("/api/")) {
    return NextResponse.next();
  }

  // Skip webhook routes (they have their own auth)
  if (req.nextUrl.pathname.startsWith("/api/webhooks/")) {
    return NextResponse.next();
  }

  const ip = getRateLimitKey(req);
  const { allowed, remaining } = checkRateLimit(ip);

  if (!allowed) {
    return NextResponse.json(
      { error: "Too many requests" },
      {
        status: 429,
        headers: { "Retry-After": "60" },
      }
    );
  }

  const response = NextResponse.next();
  response.headers.set("X-RateLimit-Remaining", String(remaining));
  return response;
}

export const config = {
  matcher: "/api/:path*",
};
```

- [ ] **Step 2: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web
git add src/middleware.ts
git commit -m "security: add global per-IP rate limiting middleware for API routes"
```

---

### Task 12: Sentry Error Tracking

**Files:**
- Modify: `vtt-web/package.json`
- Create: `vtt-web/sentry.client.config.ts`
- Create: `vtt-web/sentry.server.config.ts`
- Modify: `vtt-web/next.config.ts`

- [ ] **Step 1: Install Sentry SDK**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web && npm install @sentry/nextjs
```

- [ ] **Step 2: Create Sentry client config**

Create `vtt-web/sentry.client.config.ts`:

```typescript
import * as Sentry from "@sentry/nextjs";

Sentry.init({
  dsn: process.env.NEXT_PUBLIC_SENTRY_DSN,
  environment: process.env.NEXT_PUBLIC_NETWORK ?? "development",
  tracesSampleRate: 0.1,
  replaysSessionSampleRate: 0,
  replaysOnErrorSampleRate: 1.0,
  enabled: process.env.NODE_ENV === "production",
});
```

- [ ] **Step 3: Create Sentry server config**

Create `vtt-web/sentry.server.config.ts`:

```typescript
import * as Sentry from "@sentry/nextjs";

Sentry.init({
  dsn: process.env.NEXT_PUBLIC_SENTRY_DSN,
  environment: process.env.NEXT_PUBLIC_NETWORK ?? "development",
  tracesSampleRate: 0.1,
  enabled: process.env.NODE_ENV === "production",
});
```

- [ ] **Step 4: Wrap next.config.ts**

In `vtt-web/next.config.ts`, wrap with Sentry:

```typescript
import { withSentryConfig } from "@sentry/nextjs";

// ... existing config ...

export default withSentryConfig(nextConfig, {
  silent: true,
  org: "vtt",
  project: "vtt-web",
});
```

- [ ] **Step 5: Add env vars**

Add to `.env.*` files:

```
NEXT_PUBLIC_SENTRY_DSN=https://xxx@sentry.io/xxx
```

- [ ] **Step 6: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web
git add sentry.client.config.ts sentry.server.config.ts next.config.ts package.json package-lock.json .env.example
git commit -m "feat: integrate Sentry error tracking for production"
```

---

## Phase 3: Bridge Hardening (Solidity)

### Task 13: Bridge Pause Mechanism

**Files:**
- Modify: `vtt/bridge-evm/src/VTTBridge.sol`
- Modify: `vtt/bridge-evm/test/Bridge.t.sol`

- [ ] **Step 1: Add pause state and modifier**

In `vtt/bridge-evm/src/VTTBridge.sol`, add after storage declarations (around line 33):

```solidity
bool public paused;

event Paused(address indexed by);
event Unpaused(address indexed by);

modifier whenNotPaused() {
    require(!paused, "Bridge: paused");
    _;
}

function pause() external onlyOwner {
    paused = true;
    emit Paused(msg.sender);
}

function unpause() external onlyOwner {
    paused = false;
    emit Unpaused(msg.sender);
}
```

- [ ] **Step 2: Add whenNotPaused to deposit and release functions**

Add `whenNotPaused` modifier to:
- `depositWVTT` (line 79)
- `depositUSDT` (line 100)
- `releaseWVTT` (line 123)
- `releaseUSDT` (line 138)

Example: `function depositWVTT(uint256 amount, bytes32 vttDestination) external whenNotPaused {`

- [ ] **Step 3: Add pause tests**

In `vtt/bridge-evm/test/Bridge.t.sol`, add:

```solidity
function test_pause_blocks_deposit() public {
    bridge.pause();
    vm.startPrank(alice);
    usdt.approve(address(bridge), 1000e6);
    vm.expectRevert("Bridge: paused");
    bridge.depositUSDT(1000e6, bytes32(uint256(1)));
    vm.stopPrank();
}

function test_unpause_allows_deposit() public {
    bridge.pause();
    bridge.unpause();
    vm.startPrank(alice);
    usdt.approve(address(bridge), 1000e6);
    bridge.depositUSDT(1000e6, bytes32(uint256(1)));
    vm.stopPrank();
}

function test_pause_only_owner() public {
    vm.prank(alice);
    vm.expectRevert("Bridge: not owner");
    bridge.pause();
}
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt/bridge-evm && forge test -v
```

- [ ] **Step 5: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt
git add bridge-evm/src/VTTBridge.sol bridge-evm/test/Bridge.t.sol
git commit -m "security: add pause mechanism to bridge contract"
```

---

### Task 14: Bridge Timelock on Admin Operations

**Files:**
- Modify: `vtt/bridge-evm/src/VTTBridge.sol`
- Modify: `vtt/bridge-evm/test/Bridge.t.sol`

- [ ] **Step 1: Add timelock storage and logic**

In `vtt/bridge-evm/src/VTTBridge.sol`, add after pause declarations:

```solidity
uint256 public constant TIMELOCK_DELAY = 2 days;

struct TimelockAction {
    bytes32 actionHash;
    uint256 executeAfter;
    bool executed;
}

mapping(bytes32 => TimelockAction) public timelockActions;

event TimelockQueued(bytes32 indexed actionHash, uint256 executeAfter);
event TimelockExecuted(bytes32 indexed actionHash);

function queueSetRelayer(address _relayer) external onlyOwner {
    bytes32 hash = keccak256(abi.encode("setRelayer", _relayer));
    timelockActions[hash] = TimelockAction(hash, block.timestamp + TIMELOCK_DELAY, false);
    emit TimelockQueued(hash, block.timestamp + TIMELOCK_DELAY);
}

function executeSetRelayer(address _relayer) external onlyOwner {
    bytes32 hash = keccak256(abi.encode("setRelayer", _relayer));
    TimelockAction storage action = timelockActions[hash];
    require(action.executeAfter > 0, "Bridge: not queued");
    require(block.timestamp >= action.executeAfter, "Bridge: timelock active");
    require(!action.executed, "Bridge: already executed");
    action.executed = true;
    relayer = _relayer;
    emit RelayerUpdated(_relayer);
    emit TimelockExecuted(hash);
}
```

- [ ] **Step 2: Replace direct setRelayer with timelocked version**

Remove or restrict the old `setRelayer` function. Keep `setFee` direct (fee changes are less critical and capped at 5%).

- [ ] **Step 3: Add timelock tests**

```solidity
function test_timelock_setRelayer() public {
    bridge.queueSetRelayer(alice);
    vm.expectRevert("Bridge: timelock active");
    bridge.executeSetRelayer(alice);

    vm.warp(block.timestamp + 2 days);
    bridge.executeSetRelayer(alice);
    assertEq(bridge.relayer(), alice);
}

function test_timelock_not_queued_reverts() public {
    vm.expectRevert("Bridge: not queued");
    bridge.executeSetRelayer(alice);
}
```

- [ ] **Step 4: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt/bridge-evm && forge test -v
git add bridge-evm/
git commit -m "security: add 2-day timelock on bridge admin operations"
```

---

### Task 15: Fix Bridge Fee Withdrawal Bug

**Files:**
- Modify: `vtt/bridge-evm/src/VTTBridge.sol:164-173` (withdrawFees)
- Modify: `vtt/bridge-evm/test/Bridge.t.sol`

- [ ] **Step 1: Fix withdrawFees to handle wVTT fees separately**

Replace `withdrawFees` in `VTTBridge.sol` (lines 164-173):

```solidity
uint256 public collectedFeesWVTT;
uint256 public collectedFeesUSDT;

function withdrawFees(address to) external onlyOwner {
    require(to != address(0), "Bridge: zero address");

    uint256 wvttFees = collectedFeesWVTT;
    uint256 usdtFees = collectedFeesUSDT;

    if (wvttFees > 0) {
        collectedFeesWVTT = 0;
        wvtt.mint(to, wvttFees);
    }

    if (usdtFees > 0) {
        collectedFeesUSDT = 0;
        require(usdt.transfer(to, usdtFees), "Bridge: USDT transfer failed");
    }

    emit FeesWithdrawn(to, wvttFees, usdtFees);
}
```

Update `depositWVTT` to increment `collectedFeesWVTT` instead of `collectedFees`.
Update `depositUSDT` to increment `collectedFeesUSDT` instead of `collectedFees`.

- [ ] **Step 2: Update FeesWithdrawn event**

```solidity
event FeesWithdrawn(address indexed to, uint256 wvttAmount, uint256 usdtAmount);
```

- [ ] **Step 3: Add test for wVTT fee withdrawal**

```solidity
function test_withdraw_wvtt_fees() public {
    // Deposit wVTT to generate fees
    vm.startPrank(address(bridge));
    wvtt.mint(alice, 10000e18);
    vm.stopPrank();

    vm.startPrank(alice);
    wvtt.approve(address(bridge), 10000e18);
    bridge.depositWVTT(10000e18, bytes32(uint256(1)));
    vm.stopPrank();

    uint256 expectedFee = (10000e18 * 10) / 10000; // 0.1%
    assertEq(bridge.collectedFeesWVTT(), expectedFee);

    bridge.withdrawFees(owner);
    assertEq(wvtt.balanceOf(owner), expectedFee);
    assertEq(bridge.collectedFeesWVTT(), 0);
}
```

- [ ] **Step 4: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt/bridge-evm && forge test -v
git add bridge-evm/
git commit -m "fix: separate wVTT and USDT fee tracking in bridge withdrawFees"
```

---

## Phase 4: Governance and DEX

### Task 16: Governance Timelock

**Files:**
- Modify: `vtt/crates/vtt-consensus/src/governance.rs`
- Modify: `vtt/crates/vtt-executor/src/lib.rs` (finalize_governance_proposals)

- [ ] **Step 1: Add timelock constant and state**

In `vtt/crates/vtt-consensus/src/governance.rs`, add after existing constants (line 14):

```rust
pub const EXECUTION_DELAY_BLOCKS: u64 = 28_800; // ~24 hours at 3s/block
```

Add to `ProposalStatus` enum:

```rust
Queued { execute_after: BlockNumber },
```

- [ ] **Step 2: Modify finalization to queue instead of execute**

In `vtt/crates/vtt-executor/src/lib.rs`, in `finalize_governance_proposals` (line 1329), change passed proposals to `Queued` instead of `Executed`:

```rust
if proposal.has_quorum(total_staked) && proposal.passes_threshold() {
    proposal.status = ProposalStatus::Queued {
        execute_after: current_block + EXECUTION_DELAY_BLOCKS,
    };
} else {
    proposal.status = ProposalStatus::Rejected;
}
```

- [ ] **Step 3: Add separate execution step**

Add a new function to execute queued proposals after timelock:

```rust
pub fn execute_queued_proposals(
    chain: &mut Chain,
    current_block: BlockNumber,
) -> Result<Vec<u64>, ExecutionError> {
    let mut executed = Vec::new();
    // Iterate queued proposals where current_block >= execute_after
    // Execute the proposal action
    // Set status to Executed
    Ok(executed)
}
```

Call this from the block processing loop in vtt-node.

- [ ] **Step 4: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test -p vtt-consensus -p vtt-executor
git add crates/vtt-consensus/ crates/vtt-executor/ bin/vtt-node/
git commit -m "feat: add 24-hour timelock for governance proposal execution"
```

---

### Task 17: Governance Vote Snapshot

**Files:**
- Modify: `vtt/crates/vtt-consensus/src/governance.rs:44` (Proposal struct)
- Modify: `vtt/crates/vtt-executor/src/lib.rs:1204` (execute_governance_vote)

- [ ] **Step 1: Store snapshot block in Proposal**

In `governance.rs`, add to `Proposal` struct:

```rust
pub snapshot_block: BlockNumber, // Block at which vote weights are calculated
```

Set `snapshot_block = current_block` when proposal is created.

- [ ] **Step 2: Use snapshot for vote weight**

In `execute_governance_vote` (executor lib.rs:1204), get the voter's stake at `proposal.snapshot_block` instead of current block:

```rust
let vote_weight = chain.state_at(proposal.snapshot_block)?
    .get_account(&voter)?
    .staked;
```

If `state_at` is not available (no historical state), use the stake recorded at proposal creation time. Alternative: store a `total_staked_at_creation` and use current stake capped to what was staked at creation.

- [ ] **Step 3: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test -p vtt-consensus -p vtt-executor
git add crates/vtt-consensus/ crates/vtt-executor/
git commit -m "feat: snapshot vote weights at proposal creation to prevent vote buying"
```

---

### Task 18: DEX Pause Mechanism

**Files:**
- Modify: `vtt/crates/vtt-dex/src/swap.rs:10`
- Modify: `vtt/crates/vtt-dex/src/liquidity.rs:12`
- Modify: `vtt/crates/vtt-dex/src/error.rs`

- [ ] **Step 1: Add DEX paused check**

In `vtt/crates/vtt-dex/src/error.rs`, add variant:

```rust
DexPaused,
```

- [ ] **Step 2: Store pause state in chain config**

Use a well-known key in StateDB (e.g., `dex:paused` in ChainMeta column). Add helpers:

```rust
pub fn is_dex_paused(state: &StateDB) -> bool {
    state.get_raw(Column::ChainMeta, b"dex:paused")
        .ok()
        .flatten()
        .map(|v| v == [1u8])
        .unwrap_or(false)
}

pub fn set_dex_paused(state: &mut StateDB, paused: bool) {
    let val = if paused { vec![1u8] } else { vec![0u8] };
    let _ = state.put_raw(Column::ChainMeta, b"dex:paused", &val);
}
```

- [ ] **Step 3: Check pause in swap and liquidity operations**

In `execute_swap` (swap.rs:10), add at the top:

```rust
if is_dex_paused(state) {
    return Err(DexError::DexPaused);
}
```

Same in `create_pool`, `add_liquidity`, `remove_liquidity`.

- [ ] **Step 4: Add governance action to toggle DEX pause**

Add `DexPause(bool)` to `ProposalAction` enum in governance.rs and handle execution.

- [ ] **Step 5: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test -p vtt-dex -p vtt-consensus
git add crates/vtt-dex/ crates/vtt-consensus/
git commit -m "feat: add DEX pause mechanism, controllable via governance"
```

---

## Phase 5: Infrastructure

### Task 19: Docker HEALTHCHECK

**Files:**
- Modify: `vtt/Dockerfile`
- Modify: `vtt-web/Dockerfile`
- Modify: `vtt-web/cron/Dockerfile`

- [ ] **Step 1: Add HEALTHCHECK to blockchain Dockerfile**

In `vtt/Dockerfile`, before the ENTRYPOINT, add:

```dockerfile
HEALTHCHECK --interval=10s --timeout=5s --retries=10 --start-period=15s \
  CMD curl -sf -X POST -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"vtt_chainStatus","params":[]}' \
  http://localhost:9944 || exit 1
```

- [ ] **Step 2: Add HEALTHCHECK to web Dockerfile**

In `vtt-web/Dockerfile`, before the ENTRYPOINT, add:

```dockerfile
HEALTHCHECK --interval=10s --timeout=5s --retries=5 --start-period=30s \
  CMD wget --no-verbose --tries=1 --spider http://127.0.0.1:3000/api/status || exit 1
```

- [ ] **Step 3: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && git add Dockerfile
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web && git add Dockerfile
git commit -m "ops: add HEALTHCHECK to Dockerfiles"
```

---

### Task 20: CI/CD Pipeline

**Files:**
- Create: `vtt/.github/workflows/ci.yml`
- Create: `vtt-web/.github/workflows/ci.yml`

- [ ] **Step 1: Create blockchain CI**

Create `vtt/.github/workflows/ci.yml`:

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

env:
  CARGO_TERM_COLOR: always
  RUSTFLAGS: -D warnings

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - uses: Swatinem/rust-cache@v2

      - name: Format
        run: cargo fmt --all -- --check

      - name: Clippy
        run: cargo clippy --all-targets --all-features

      - name: Test
        run: cargo test --all

      - name: Audit
        run: |
          cargo install cargo-audit
          cargo audit

  bridge:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          submodules: recursive
      - uses: foundry-rs/foundry-toolchain@v1

      - name: Forge test
        working-directory: bridge-evm
        run: forge test -v

      - name: Forge snapshot
        working-directory: bridge-evm
        run: forge snapshot
```

- [ ] **Step 2: Create web CI**

Create `vtt-web/.github/workflows/ci.yml`:

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 22
          cache: npm

      - name: Install
        run: npm ci

      - name: Lint
        run: npm run lint

      - name: Build
        run: npm run build
        env:
          NEXT_PUBLIC_NETWORK: testnet
          NEXT_PUBLIC_STRIPE_PUBLISHABLE_KEY: pk_test_dummy
          NEXT_PUBLIC_TURNSTILE_SITE_KEY: dummy
          NEXT_PUBLIC_DOMAIN: test.example.com
          NEXT_PUBLIC_CONTACT_EMAIL: test@example.com

      - name: Audit
        run: npm audit --production
```

- [ ] **Step 3: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt
git add .github/workflows/ci.yml
git commit -m "ops: add CI pipeline — format, clippy, test, audit, bridge tests"

cd /Users/alessandrovettor/Documents/Lavoro/vtt-web
git add .github/workflows/ci.yml
git commit -m "ops: add CI pipeline — lint, build, audit"
```

---

### Task 21: Nginx Reverse Proxy

**Files:**
- Create: `/Users/alessandrovettor/Documents/Lavoro/nginx.conf`
- Modify: `/Users/alessandrovettor/Documents/Lavoro/docker-compose.mainnet.yml`

- [ ] **Step 1: Create nginx config**

Create `/Users/alessandrovettor/Documents/Lavoro/nginx.conf`:

```nginx
worker_processes auto;
events {
    worker_connections 2048;
}

http {
    limit_req_zone $binary_remote_addr zone=api:10m rate=30r/s;
    limit_req_zone $binary_remote_addr zone=rpc:10m rate=10r/s;
    limit_conn_zone $binary_remote_addr zone=addr:10m;

    client_max_body_size 10m;
    client_body_timeout 10s;
    client_header_timeout 10s;

    upstream web {
        server web:3000;
    }

    upstream validator_rpc {
        server validator:9944;
    }

    server {
        listen 80;
        server_name _;

        # API routes — rate limited
        location /api/ {
            limit_req zone=api burst=20 nodelay;
            limit_conn addr 20;
            proxy_pass http://web;
            proxy_set_header Host $host;
            proxy_set_header X-Real-IP $remote_addr;
            proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
            proxy_set_header X-Forwarded-Proto $scheme;
        }

        # RPC endpoint — stricter rate limit
        location /rpc {
            limit_req zone=rpc burst=5 nodelay;
            limit_conn addr 5;
            proxy_pass http://validator_rpc;
            proxy_set_header Host $host;
            proxy_set_header X-Real-IP $remote_addr;
        }

        # Static assets and pages
        location / {
            limit_conn addr 30;
            proxy_pass http://web;
            proxy_set_header Host $host;
            proxy_set_header X-Real-IP $remote_addr;
            proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
            proxy_set_header X-Forwarded-Proto $scheme;
        }

        # Block direct access to internal endpoints
        location /api/cron/ {
            deny all;
            return 403;
        }
    }
}
```

- [ ] **Step 2: Add nginx to mainnet docker-compose**

In `/Users/alessandrovettor/Documents/Lavoro/docker-compose.mainnet.yml`, add service before cloudflared:

```yaml
  nginx:
    image: nginx:alpine
    container_name: vtt-nginx
    restart: always
    volumes:
      - ./nginx.conf:/etc/nginx/nginx.conf:ro
    depends_on:
      web:
        condition: service_healthy
    networks:
      - vtt-network
    deploy:
      resources:
        limits:
          cpus: "0.5"
          memory: 256M
```

Update cloudflared to depend on nginx instead of web, and point to `http://nginx:80`.

- [ ] **Step 3: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro
git add nginx.conf docker-compose.mainnet.yml
git commit -m "ops: add nginx reverse proxy with rate limiting"
```

---

### Task 22: Monitoring Stack

**Files:**
- Create: `/Users/alessandrovettor/Documents/Lavoro/monitoring/prometheus.yml`
- Create: `/Users/alessandrovettor/Documents/Lavoro/monitoring/alertmanager.yml`
- Modify: `/Users/alessandrovettor/Documents/Lavoro/docker-compose.mainnet.yml`

- [ ] **Step 1: Create Prometheus config**

Create `/Users/alessandrovettor/Documents/Lavoro/monitoring/prometheus.yml`:

```yaml
global:
  scrape_interval: 15s
  evaluation_interval: 15s

rule_files:
  - /etc/prometheus/alerts.yml

alerting:
  alertmanagers:
    - static_configs:
        - targets: ["alertmanager:9093"]

scrape_configs:
  - job_name: vtt-validator
    static_configs:
      - targets: ["validator:9615"]

  - job_name: vtt-web
    metrics_path: /api/status
    static_configs:
      - targets: ["web:3000"]
```

- [ ] **Step 2: Create alert rules**

Create `/Users/alessandrovettor/Documents/Lavoro/monitoring/alerts.yml`:

```yaml
groups:
  - name: vtt
    rules:
      - alert: BlockProductionStalled
        expr: increase(vtt_blocks_imported_total[5m]) == 0
        for: 5m
        labels:
          severity: critical
        annotations:
          summary: "No new blocks in 5 minutes"

      - alert: NoPeers
        expr: vtt_connected_peers == 0
        for: 2m
        labels:
          severity: critical
        annotations:
          summary: "Validator has no peers"

      - alert: TxPoolFull
        expr: vtt_txpool_size > 8000
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Transaction pool is near capacity"

      - alert: HighBlockImportLatency
        expr: histogram_quantile(0.95, vtt_block_import_duration_seconds_bucket) > 2
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Block import latency p95 > 2s"
```

- [ ] **Step 3: Create AlertManager config**

Create `/Users/alessandrovettor/Documents/Lavoro/monitoring/alertmanager.yml`:

```yaml
global:
  resolve_timeout: 5m

route:
  receiver: telegram
  group_by: [alertname]
  group_wait: 30s
  group_interval: 5m
  repeat_interval: 4h

receivers:
  - name: telegram
    webhook_configs:
      - url: "http://web:3000/api/webhooks/alerts"
        send_resolved: true
```

- [ ] **Step 4: Add monitoring services to mainnet docker-compose**

Add to `/Users/alessandrovettor/Documents/Lavoro/docker-compose.mainnet.yml`:

```yaml
  prometheus:
    image: prom/prometheus:latest
    container_name: vtt-prometheus
    restart: always
    volumes:
      - ./monitoring/prometheus.yml:/etc/prometheus/prometheus.yml:ro
      - ./monitoring/alerts.yml:/etc/prometheus/alerts.yml:ro
      - prometheus-data:/prometheus
    networks:
      - vtt-network
    deploy:
      resources:
        limits:
          cpus: "0.5"
          memory: 512M

  alertmanager:
    image: prom/alertmanager:latest
    container_name: vtt-alertmanager
    restart: always
    volumes:
      - ./monitoring/alertmanager.yml:/etc/alertmanager/alertmanager.yml:ro
    networks:
      - vtt-network
    deploy:
      resources:
        limits:
          cpus: "0.25"
          memory: 128M

  grafana:
    image: grafana/grafana:latest
    container_name: vtt-grafana
    restart: always
    environment:
      GF_SECURITY_ADMIN_PASSWORD: ${GRAFANA_PASSWORD}
    volumes:
      - grafana-data:/var/lib/grafana
    networks:
      - vtt-network
    deploy:
      resources:
        limits:
          cpus: "0.5"
          memory: 512M
```

Add volumes:

```yaml
  prometheus-data:
  grafana-data:
```

- [ ] **Step 5: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro
mkdir -p monitoring
git add monitoring/ docker-compose.mainnet.yml
git commit -m "ops: add Prometheus + Grafana + AlertManager monitoring stack"
```

---

### Task 23: Automated Database Backups

**Files:**
- Create: `/Users/alessandrovettor/Documents/Lavoro/scripts/backup-db.sh`
- Modify: `/Users/alessandrovettor/Documents/Lavoro/docker-compose.mainnet.yml`

- [ ] **Step 1: Create backup script**

Create `/Users/alessandrovettor/Documents/Lavoro/scripts/backup-db.sh`:

```bash
#!/bin/bash
set -euo pipefail

BACKUP_DIR="/backups"
RETENTION_DAYS=30
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
BACKUP_FILE="${BACKUP_DIR}/vtt_${TIMESTAMP}.sql.gz"

mkdir -p "${BACKUP_DIR}"

# Dump and compress
pg_dump -h postgres -U vtt -d vtt | gzip > "${BACKUP_FILE}"

# Verify backup is not empty
if [ ! -s "${BACKUP_FILE}" ]; then
    echo "ERROR: Backup file is empty"
    exit 1
fi

# Clean old backups
find "${BACKUP_DIR}" -name "vtt_*.sql.gz" -mtime +"${RETENTION_DAYS}" -delete

echo "Backup complete: ${BACKUP_FILE} ($(du -h "${BACKUP_FILE}" | cut -f1))"
```

- [ ] **Step 2: Add backup service to docker-compose**

Add to mainnet docker-compose:

```yaml
  backup:
    image: postgres:16-alpine
    container_name: vtt-backup
    restart: "no"
    entrypoint: /scripts/backup-db.sh
    environment:
      PGPASSWORD: ${POSTGRES_PASSWORD}
    volumes:
      - ./scripts/backup-db.sh:/scripts/backup-db.sh:ro
      - backup-data:/backups
    depends_on:
      postgres:
        condition: service_healthy
    networks:
      - vtt-network
    profiles:
      - backup
```

Add volume: `backup-data:`

Run manually or via cron: `docker compose --profile backup run --rm backup`

- [ ] **Step 3: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro
chmod +x scripts/backup-db.sh
git add scripts/ docker-compose.mainnet.yml
git commit -m "ops: add automated PostgreSQL backup with 30-day retention"
```

---

## Phase 6: Compliance and User-Facing

### Task 24: GDPR Data Deletion Endpoint

**Files:**
- Create: `vtt-web/src/app/api/kyc/delete-data/route.ts`

- [ ] **Step 1: Create data deletion endpoint**

Create `vtt-web/src/app/api/kyc/delete-data/route.ts`:

```typescript
import { NextRequest } from "next/server";
import { prisma } from "@/lib/db/prisma";
import { verifyWalletSignature } from "@/lib/auth/verify-signature";

export async function POST(req: NextRequest) {
  try {
    const { address, signature, publicKey, timestamp } = await req.json();

    if (!address || !signature || !publicKey || !timestamp) {
      return Response.json({ error: "Missing required fields" }, { status: 400 });
    }

    // Verify timestamp within 5 minutes
    const now = Date.now();
    if (Math.abs(now - timestamp) > 5 * 60 * 1000) {
      return Response.json({ error: "Timestamp expired" }, { status: 400 });
    }

    // Verify wallet signature
    const message = `DELETE_MY_DATA:${address}:${timestamp}`;
    const valid = await verifyWalletSignature(message, signature, publicKey, address);
    if (!valid) {
      return Response.json({ error: "Invalid signature" }, { status: 401 });
    }

    // Delete KYC data
    const submission = await prisma.kycSubmission.findUnique({
      where: { address },
    });

    if (!submission) {
      return Response.json({ error: "No data found" }, { status: 404 });
    }

    // Delete KYC submission (PII removed)
    await prisma.kycSubmission.delete({ where: { address } });

    // Anonymize audit logs (keep logs but remove PII)
    await prisma.auditLog.updateMany({
      where: { address },
      data: { address: "DELETED", details: {} },
    });

    // Delete associated launchpad orders PII
    await prisma.launchpadOrder.updateMany({
      where: { address },
      data: { address: "DELETED" },
    });

    return Response.json({ success: true, message: "Data deleted per GDPR request" });
  } catch (error) {
    console.error("GDPR deletion error:", error);
    return Response.json({ error: "Internal server error" }, { status: 500 });
  }
}
```

- [ ] **Step 2: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web
git add src/app/api/kyc/delete-data/
git commit -m "feat: add GDPR data deletion endpoint for KYC data"
```

---

### Task 25: Stripe Refund Flow

**Files:**
- Create: `vtt-web/src/app/api/admin/launchpad/orders/[id]/refund/route.ts`
- Modify: `vtt-web/src/lib/stripe/client.ts`

- [ ] **Step 1: Add refund helper to Stripe client**

In `vtt-web/src/lib/stripe/client.ts`, add:

```typescript
export async function createRefund(paymentIntentId: string, reason?: string): Promise<Stripe.Refund> {
  return stripe.refunds.create({
    payment_intent: paymentIntentId,
    reason: "requested_by_customer",
    metadata: { reason: reason ?? "admin_initiated" },
  });
}
```

- [ ] **Step 2: Create admin refund endpoint**

Create `vtt-web/src/app/api/admin/launchpad/orders/[id]/refund/route.ts`:

```typescript
import { NextRequest } from "next/server";
import { prisma } from "@/lib/db/prisma";
import { requireAdmin } from "@/lib/auth/require-admin";
import { createRefund } from "@/lib/stripe/client";

export async function POST(req: NextRequest, { params }: { params: Promise<{ id: string }> }) {
  const adminAddress = await requireAdmin();
  const { id } = await params;
  const { reason } = await req.json().catch(() => ({ reason: "" }));

  const order = await prisma.launchpadOrder.findUnique({ where: { id } });
  if (!order) {
    return Response.json({ error: "Order not found" }, { status: 404 });
  }

  if (order.paymentStatus !== "paid") {
    return Response.json({ error: "Order not eligible for refund" }, { status: 400 });
  }

  if (!order.stripeSessionId) {
    return Response.json({ error: "No Stripe payment found" }, { status: 400 });
  }

  const refund = await createRefund(order.stripeSessionId, reason);

  await prisma.launchpadOrder.update({
    where: { id },
    data: { paymentStatus: "refunded" },
  });

  await prisma.auditLog.create({
    data: {
      address: adminAddress,
      action: "refund_order",
      details: { orderId: id, refundId: refund.id, reason },
    },
  });

  return Response.json({ success: true, refundId: refund.id });
}
```

- [ ] **Step 3: Commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt-web
git add src/app/api/admin/launchpad/orders/ src/lib/stripe/client.ts
git commit -m "feat: add admin refund flow for Stripe payments"
```

---

## Phase 7: Storage and Chain Robustness

### Task 26: RocksDB Pruning Strategy

**Files:**
- Modify: `vtt/crates/vtt-storage/src/rocks.rs`
- Modify: `vtt/bin/vtt-node/src/main.rs`

- [ ] **Step 1: Add pruning method to RocksDB backend**

In `vtt/crates/vtt-storage/src/rocks.rs`, add:

```rust
pub fn prune_old_blocks(&self, keep_recent: u64, current_height: u64) -> Result<u64, StorageError> {
    if current_height <= keep_recent {
        return Ok(0);
    }

    let prune_below = current_height - keep_recent;
    let mut pruned = 0;

    // Prune block bodies and receipts (keep headers for chain verification)
    for column in [Column::BlockBodies, Column::Receipts] {
        for height in 0..prune_below {
            let key = height.to_be_bytes();
            if self.contains(column, &key)? {
                self.delete(column, &key)?;
                pruned += 1;
            }
        }
    }

    Ok(pruned)
}

pub fn compact(&self) {
    if let Some(db) = &self.db {
        for cf in Column::ALL {
            if let Some(handle) = db.cf_handle(cf.name()) {
                db.compact_range_cf(handle, None::<&[u8]>, None::<&[u8]>);
            }
        }
    }
}
```

- [ ] **Step 2: Schedule periodic pruning in node**

In `vtt/bin/vtt-node/src/main.rs`, add a periodic task (e.g., every 10,000 blocks):

```rust
const PRUNE_INTERVAL_BLOCKS: u64 = 10_000;
const KEEP_RECENT_BLOCKS: u64 = 100_000; // ~3.5 days at 3s/block

if chain.best_block_number() % PRUNE_INTERVAL_BLOCKS == 0 {
    if let Ok(pruned) = storage.prune_old_blocks(KEEP_RECENT_BLOCKS, chain.best_block_number()) {
        tracing::info!(pruned, "Pruned old block data");
        storage.compact();
    }
}
```

- [ ] **Step 3: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test -p vtt-storage
git add crates/vtt-storage/ bin/vtt-node/
git commit -m "ops: add RocksDB pruning — keep last 100k blocks, periodic compaction"
```

---

### Task 27: Genesis Validation

**Files:**
- Modify: `vtt/crates/vtt-genesis/src/lib.rs`

- [ ] **Step 1: Add validation to build_genesis**

In `vtt/crates/vtt-genesis/src/lib.rs`, in `build_genesis` (around line 157), add validation before building:

```rust
pub fn validate_genesis(config: &GenesisConfig) -> Result<(), String> {
    // Minimum validators
    if config.validators.is_empty() {
        return Err("Genesis must have at least 1 validator".into());
    }

    // Validate chain_id
    if config.chain_config.chain_id == 0 {
        return Err("chain_id cannot be 0".into());
    }

    // Validate total supply consistency
    let total_allocated: u128 = config.allocations.iter()
        .map(|a| a.amount.raw())
        .sum();
    let total_staked: u128 = config.validators.iter()
        .map(|v| v.stake.raw())
        .sum();

    if total_allocated == 0 && total_staked == 0 {
        return Err("Genesis has no tokens allocated".into());
    }

    // Validate min self-stake
    let min_stake = config.chain_config.consensus_params.min_self_stake;
    for validator in &config.validators {
        if validator.stake < min_stake {
            return Err(format!(
                "Validator {} stake {} below minimum {}",
                validator.address, validator.stake, min_stake
            ));
        }
    }

    // Validate epoch length
    if config.chain_config.consensus_params.epoch_length == 0 {
        return Err("epoch_length cannot be 0".into());
    }

    Ok(())
}
```

Call `validate_genesis(&config)?` at the start of `build_genesis`.

- [ ] **Step 2: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test -p vtt-genesis
git add crates/vtt-genesis/
git commit -m "security: add genesis config validation — supply, validators, params"
```

---

## Phase 8: Finality Enforcement

### Task 28: Integrate Finality into Fork Choice

**Files:**
- Modify: `vtt/crates/vtt-consensus/src/finality.rs`
- Modify: `vtt/bin/vtt-node/src/main.rs` (block import)

- [ ] **Step 1: Add finalized block tracking to chain**

The FinalityTracker already tracks finalized block number. Ensure the node stores it persistently:

```rust
// In chain or storage, persist the finalized block number
pub fn set_finalized_block(&mut self, number: BlockNumber) {
    self.storage.put(Column::ChainMeta, b"finalized_block", &number.to_be_bytes()).ok();
    self.finalized_number = number;
}

pub fn finalized_block(&self) -> BlockNumber {
    self.finalized_number
}
```

- [ ] **Step 2: Enforce finality in fork choice**

In the block import logic (vtt-node main.rs), when choosing between forks:

```rust
// Never revert past finalized block
if fork_point < chain.finalized_block() {
    tracing::warn!(fork_point, finalized = chain.finalized_block(), "Rejecting fork below finalized block");
    continue; // Skip this fork
}
```

- [ ] **Step 3: Process finality votes from network**

Ensure `FinalityVote` messages from the network are fed into the `FinalityTracker`. When `submit_vote` returns `true` (new block finalized), call `set_finalized_block`.

- [ ] **Step 4: Run tests and commit**

```bash
cd /Users/alessandrovettor/Documents/Lavoro/vtt && cargo test
git add crates/vtt-consensus/ bin/vtt-node/
git commit -m "consensus: enforce finality in fork choice — reject forks below finalized block"
```

---

## Dependency Order

Tasks can be parallelized within groups:

**Group A (independent — run in parallel):**
- Task 1 (WASM limits)
- Task 2 (CORS)
- Task 3 (Rate limiting)
- Task 5 (TX TTL)
- Task 6 (Peer banning)
- Task 7 (CSP fix)
- Task 8 (KYC encryption)
- Task 9 (Env validation)
- Task 10 (Error boundaries)
- Task 11 (API rate limiting)
- Task 13 (Bridge pause)
- Task 15 (Fee bug fix)
- Task 19 (HEALTHCHECK)
- Task 20 (CI/CD)
- Task 24 (GDPR deletion)
- Task 25 (Stripe refund)
- Task 27 (Genesis validation)

**Group B (depends on Group A completing):**
- Task 4 (Slashing — needs state methods)
- Task 12 (Sentry — needs npm install)
- Task 14 (Bridge timelock — needs pause first)
- Task 16 (Governance timelock)
- Task 17 (Vote snapshot)
- Task 18 (DEX pause)
- Task 21 (Nginx — needs docker-compose)
- Task 22 (Monitoring — needs docker-compose)
- Task 23 (Backups — needs docker-compose)
- Task 26 (RocksDB pruning)
- Task 28 (Finality enforcement)
