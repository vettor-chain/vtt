use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};
use std::time::Instant;

use jsonrpsee::core::async_trait;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::server::Server;
use jsonrpsee::types::ErrorObjectOwned;
use tower::{Layer, Service};
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{debug, info};

tokio::task_local! {
    /// Client IP of the currently-executing RPC request, populated by
    /// `ClientIpLayer` from `X-Forwarded-For` / `Forwarded` / the peer
    /// socket address before the jsonrpsee handler runs. Handlers read
    /// this via `current_client_ip()` to rate-limit per-IP instead of
    /// sharing one bucket across all clients.
    static CLIENT_IP: IpAddr;
}

fn current_client_ip() -> IpAddr {
    CLIENT_IP
        .try_with(|ip| *ip)
        .unwrap_or_else(|_| "127.0.0.1".parse().unwrap())
}

/// Tower layer that extracts the real client IP from `X-Forwarded-For`
/// (first hop) or `Forwarded: for=...` and scopes the downstream future
/// inside `CLIENT_IP.scope(...)`. Falls back to `127.0.0.1` when neither
/// header is present — which is the right behaviour for loopback traffic
/// (developer tools hitting the node directly).
#[derive(Clone)]
struct ClientIpLayer;

impl<S> Layer<S> for ClientIpLayer {
    type Service = ClientIpService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        ClientIpService { inner }
    }
}

#[derive(Clone)]
struct ClientIpService<S> {
    inner: S,
}

fn extract_client_ip<B>(req: &hyper::Request<B>) -> IpAddr {
    // X-Forwarded-For: "client, proxy1, proxy2" — first value is the real client.
    if let Some(v) = req.headers().get("x-forwarded-for") {
        if let Ok(s) = v.to_str() {
            if let Some(first) = s.split(',').next() {
                if let Ok(ip) = first.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }
    // Forwarded: for=1.2.3.4;proto=https;by=...  — RFC 7239.
    if let Some(v) = req.headers().get("forwarded") {
        if let Ok(s) = v.to_str() {
            for part in s.split(';') {
                let part = part.trim();
                if let Some(rest) = part.strip_prefix("for=") {
                    let trimmed = rest.trim_matches(|c: char| c == '"' || c.is_whitespace());
                    // IPv6 addrs are bracketed inside square brackets: for="[::1]:12345"
                    let bare = trimmed.trim_start_matches('[').trim_end_matches(']');
                    let no_port = bare.rsplit_once(':').map(|(h, _)| h).unwrap_or(bare);
                    if let Ok(ip) = no_port.parse::<IpAddr>() {
                        return ip;
                    }
                }
            }
        }
    }
    "127.0.0.1".parse().unwrap()
}

impl<S, B> Service<hyper::Request<B>> for ClientIpService<S>
where
    S: Service<hyper::Request<B>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: hyper::Request<B>) -> Self::Future {
        let ip = extract_client_ip(&req);
        // Clone so we don't move the in-progress service reference; see the
        // tower docs on Clone-before-call for the canonical pattern.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            CLIENT_IP
                .scope(ip, async move { inner.call(req).await })
                .await
        })
    }
}

use borsh::BorshDeserialize;
use vtt_chain::Chain;
use vtt_crypto::blake3_hash;
use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, BlockNumber, H256};
use vtt_telemetry::NodeMetrics;
use vtt_txpool::TxPool;

use crate::types::{
    AccountInfo, AssetBalanceInfo, AssetInfo, AssetProposalInfo, BlockInfo, BridgeWithdrawalInfo,
    ChainStatus, ConsensusParamsRpc, DelegationInfo, GasConfigRpc, LogInfo, NodeMetricsInfo,
    OracleFeedInfo, PaginatedResult, PoolInfo, PoolPriceRpc, ProposalInfo, ReceiptInfo,
    RegisteredChainInfo, SlashRecordInfo, StakingInfo, SwapQuoteRpc, TokenPriceRpc,
    TransactionInfo, ValidatorInfoRpc,
};

/// JSON-RPC API definition for VTT.
#[rpc(server)]
pub trait VttApi {
    /// Get the balance of an address.
    #[method(name = "vtt_getBalance")]
    async fn get_balance(&self, address: Address) -> Result<Amount, ErrorObjectOwned>;

    /// Get account info (balance, nonce, is_contract).
    #[method(name = "vtt_getAccount")]
    async fn get_account(&self, address: Address) -> Result<AccountInfo, ErrorObjectOwned>;

    /// Get block info by hash.
    #[method(name = "vtt_getBlock")]
    async fn get_block(&self, hash: H256) -> Result<Option<BlockInfo>, ErrorObjectOwned>;

    /// Get block info by number.
    #[method(name = "vtt_getBlockByNumber")]
    async fn get_block_by_number(
        &self,
        number: BlockNumber,
    ) -> Result<Option<BlockInfo>, ErrorObjectOwned>;

    /// Get the current chain height.
    #[method(name = "vtt_chainHeight")]
    async fn chain_height(&self) -> Result<BlockNumber, ErrorObjectOwned>;

    /// Get chain status.
    #[method(name = "vtt_chainStatus")]
    async fn chain_status(&self) -> Result<ChainStatus, ErrorObjectOwned>;

    /// Get consensus parameters.
    #[method(name = "vtt_getConsensusParams")]
    async fn get_consensus_params(&self) -> Result<ConsensusParamsRpc, ErrorObjectOwned>;

    /// Get gas configuration.
    #[method(name = "vtt_getGasConfig")]
    async fn get_gas_config(&self) -> Result<GasConfigRpc, ErrorObjectOwned>;

    /// Get the active validator set.
    #[method(name = "vtt_getValidators")]
    async fn get_validators(&self) -> Result<Vec<ValidatorInfoRpc>, ErrorObjectOwned>;

    /// Get the transaction pool size.
    #[method(name = "vtt_txPoolSize")]
    async fn tx_pool_size(&self) -> Result<usize, ErrorObjectOwned>;

    /// Get asset info by ID.
    #[method(name = "vtt_getAsset")]
    async fn get_asset(&self, asset_id: H256) -> Result<Option<AssetInfo>, ErrorObjectOwned>;

    /// Get asset balance for an address.
    #[method(name = "vtt_getAssetBalance")]
    async fn get_asset_balance(
        &self,
        asset_id: H256,
        address: Address,
    ) -> Result<AssetBalanceInfo, ErrorObjectOwned>;

    /// List all registered assets.
    #[method(name = "vtt_listAssets")]
    async fn list_assets(&self) -> Result<Vec<AssetInfo>, ErrorObjectOwned>;

    /// Get oracle feed info.
    #[method(name = "vtt_getOracle")]
    async fn get_oracle(&self, feed_id: H256) -> Result<Option<OracleFeedInfo>, ErrorObjectOwned>;

    /// List all registered oracle feeds.
    #[method(name = "vtt_listOracles")]
    async fn list_oracles(&self) -> Result<Vec<OracleFeedInfo>, ErrorObjectOwned>;

    /// Submit a signed transaction. Returns the transaction hash.
    #[method(name = "vtt_sendTransaction")]
    async fn send_transaction(&self, tx_hex: String) -> Result<H256, ErrorObjectOwned>;

    /// Get staking info for an address.
    #[method(name = "vtt_getStakingInfo")]
    async fn get_staking_info(
        &self,
        address: Address,
    ) -> Result<Option<StakingInfo>, ErrorObjectOwned>;

    /// List all DEX liquidity pools.
    #[method(name = "vtt_listPools")]
    async fn list_pools(&self) -> Result<Vec<PoolInfo>, ErrorObjectOwned>;

    /// Get a single DEX pool by its H256 ID.
    #[method(name = "vtt_getPool")]
    async fn get_pool(&self, pool_id: H256) -> Result<Option<PoolInfo>, ErrorObjectOwned>;

    /// Get spot price of a token in VTT, derived from DEX pool reserves.
    #[method(name = "vtt_getTokenPrice")]
    async fn get_token_price(
        &self,
        token_id: H256,
    ) -> Result<Option<TokenPriceRpc>, ErrorObjectOwned>;

    /// Get spot prices for all DEX pools.
    #[method(name = "vtt_getPoolPrices")]
    async fn get_pool_prices(&self) -> Result<Vec<PoolPriceRpc>, ErrorObjectOwned>;

    /// Get a swap quote (read-only, no state mutation).
    #[method(name = "vtt_getSwapQuote")]
    async fn get_swap_quote(
        &self,
        pool_id: H256,
        amount_in: String,
        a_to_b: bool,
    ) -> Result<SwapQuoteRpc, ErrorObjectOwned>;

    /// List transactions (paginated, most recent first).
    #[method(name = "vtt_listTransactions")]
    async fn list_transactions(
        &self,
        page: usize,
        limit: usize,
    ) -> Result<PaginatedResult<TransactionInfo>, ErrorObjectOwned>;

    /// Get a single transaction by hash.
    #[method(name = "vtt_getTransaction")]
    async fn get_transaction(
        &self,
        hash: H256,
    ) -> Result<Option<TransactionInfo>, ErrorObjectOwned>;

    /// Get transactions by address (paginated, most recent first).
    #[method(name = "vtt_getTransactionsByAddress")]
    async fn get_transactions_by_address(
        &self,
        address: Address,
        page: usize,
        limit: usize,
    ) -> Result<PaginatedResult<TransactionInfo>, ErrorObjectOwned>;

    /// Get all asset governance proposals for an asset.
    #[method(name = "vtt_getAssetProposals")]
    async fn get_asset_proposals(
        &self,
        asset_id: H256,
    ) -> Result<Vec<AssetProposalInfo>, ErrorObjectOwned>;

    /// Get a single asset governance proposal by ID.
    #[method(name = "vtt_getAssetProposal")]
    async fn get_asset_proposal(
        &self,
        proposal_id: H256,
    ) -> Result<Option<AssetProposalInfo>, ErrorObjectOwned>;

    /// Get all bridge withdrawal events (for relayer monitoring).
    #[method(name = "vtt_getBridgeWithdrawals")]
    async fn get_bridge_withdrawals(&self) -> Result<Vec<BridgeWithdrawalInfo>, ErrorObjectOwned>;

    /// Get node metrics for monitoring.
    #[method(name = "vtt_getNodeMetrics")]
    async fn get_node_metrics(&self) -> Result<NodeMetricsInfo, ErrorObjectOwned>;

    /// List all protocol governance proposals.
    #[method(name = "vtt_listProposals")]
    async fn list_proposals(&self) -> Result<Vec<ProposalInfo>, ErrorObjectOwned>;

    /// Get a single protocol governance proposal by ID.
    #[method(name = "vtt_getProposal")]
    async fn get_proposal(&self, id: H256) -> Result<Option<ProposalInfo>, ErrorObjectOwned>;

    /// Get a range of blocks starting from a given number.
    #[method(name = "vtt_getBlockRange")]
    async fn get_block_range(
        &self,
        from: u64,
        count: u64,
    ) -> Result<Vec<BlockInfo>, ErrorObjectOwned>;

    /// Get balances for multiple assets at once.
    #[method(name = "vtt_getAssetBalances")]
    async fn get_asset_balances(
        &self,
        address: Address,
        asset_ids: Vec<H256>,
    ) -> Result<Vec<AssetBalanceInfo>, ErrorObjectOwned>;

    /// Get the receipt (logs, gas_used, success) for a transaction by hash.
    /// Returns None when the tx never executed or was produced before receipt
    /// persistence was introduced.
    #[method(name = "vtt_getTransactionReceipt")]
    async fn get_transaction_receipt(
        &self,
        tx_hash: H256,
    ) -> Result<Option<ReceiptInfo>, ErrorObjectOwned>;

    /// Check whether an address is KYC-approved for regulated asset transfers.
    #[method(name = "vtt_isKycApproved")]
    async fn is_kyc_approved(&self, address: Address) -> Result<bool, ErrorObjectOwned>;

    /// Get the currently configured bridge relayer address (or zero if unset).
    #[method(name = "vtt_getBridgeRelayer")]
    async fn get_bridge_relayer(&self) -> Result<Address, ErrorObjectOwned>;

    /// Get slashing history for a validator address.
    #[method(name = "vtt_getSlashingHistory")]
    async fn get_slashing_history(
        &self,
        validator: Address,
    ) -> Result<Vec<SlashRecordInfo>, ErrorObjectOwned>;

    /// List every app-chain registered via governance. Relay chain is not
    /// included (it is implicit and always active).
    #[method(name = "vtt_listRegisteredChains")]
    async fn list_registered_chains(&self) -> Result<Vec<RegisteredChainInfo>, ErrorObjectOwned>;

    /// Look up a single registered app-chain by id.
    #[method(name = "vtt_getRegisteredChain")]
    async fn get_registered_chain(
        &self,
        chain_id: u32,
    ) -> Result<Option<RegisteredChainInfo>, ErrorObjectOwned>;
}

/// Per-IP rate limiter for sendTransaction -- sliding window counter.
///
/// Each IP address gets its own call counter that resets every second.
/// Stale entries (no requests for 60s) are cleaned up periodically.
///
/// NOTE: When running behind a reverse proxy, the proxy should set
/// X-Forwarded-For so the application can extract the real client IP.
/// Currently, if the real IP is not available from the jsonrpsee
/// connection context, it falls back to 127.0.0.1.
pub struct PerIpRateLimiter {
    /// Maximum calls per 1-second window per IP.
    max_calls_per_second: u64,
    /// Per-IP counters: (call_count, window_start).
    clients: Mutex<HashMap<IpAddr, (u64, Instant)>>,
}

impl PerIpRateLimiter {
    pub fn new(max_calls_per_second: u64) -> Self {
        Self {
            max_calls_per_second,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Returns true if the request from `ip` is allowed, false if rate-limited.
    pub fn check(&self, ip: IpAddr) -> bool {
        let mut clients = match self.clients.lock() {
            Ok(c) => c,
            Err(_) => return false, // poisoned lock -- deny request
        };
        let now = Instant::now();
        let entry = clients.entry(ip).or_insert((0, now));

        // If the window has elapsed, reset the counter.
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

    /// Remove entries that have not been seen for 60 seconds.
    pub fn cleanup(&self) {
        let mut clients = match self.clients.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let now = Instant::now();
        clients.retain(|_, (_, last)| now.duration_since(*last).as_secs() < 60);
    }
}

/// Helper: acquire a read lock on the chain, returning a JSON-RPC internal error on lock poisoning.
fn read_chain(
    chain: &Arc<RwLock<Chain>>,
) -> Result<std::sync::RwLockReadGuard<'_, Chain>, ErrorObjectOwned> {
    chain.read().map_err(|_| {
        ErrorObjectOwned::owned(-32603, "internal error: chain lock poisoned", None::<()>)
    })
}

/// Log the full details of an internal error and return a redacted
/// JSON-RPC error to the caller. Keeps infrastructure internals out of
/// responses while preserving operability via server logs.
fn internal_err<E: std::fmt::Display>(ctx: &str, err: E) -> ErrorObjectOwned {
    tracing::warn!(context = ctx, error = %err, "RPC internal error");
    ErrorObjectOwned::owned(-32603, format!("{ctx}: internal error"), None::<()>)
}

/// Maximum hex length accepted by sendTransaction. Anything larger is rejected
/// before allocating the decoded buffer. 2 MiB of hex decodes to ~1 MiB of
/// bytes — enough headroom for the largest reasonable DeployContract WASM
/// payloads while still being well under the 1 MB HTTP body limit (the hex
/// itself arrives inside a JSON-RPC envelope, so the body limit effectively
/// caps hex_len at ~1.8 MiB regardless of this constant).
const MAX_TX_HEX_LEN: usize = 2 * 1024 * 1024;

/// Upper bound on the number of recent blocks scanned by RPC methods that
/// need to walk the chain (e.g. get_transaction, get_bridge_withdrawals).
/// Callers are expected to use paginated, index-backed queries once the
/// chain grows; until then this cap prevents a single request from pinning
/// the read lock on a long scan.
const MAX_BLOCK_SCAN_DEPTH: u64 = 20_000;

/// Gate expensive read RPCs behind a dedicated per-IP token bucket. Cheap
/// reads (getBalance, chainStatus, getBlockByNumber) skip this check.
fn check_heavy_read(state: &RpcState) -> Result<(), ErrorObjectOwned> {
    if !state.heavy_read_limiter.check(current_client_ip()) {
        return Err(ErrorObjectOwned::owned(
            -32005,
            "Rate limit exceeded",
            None::<()>,
        ));
    }
    Ok(())
}

/// Helper: acquire a write lock on the chain, returning a JSON-RPC internal error on lock poisoning.
#[allow(dead_code)]
fn write_chain(
    chain: &Arc<RwLock<Chain>>,
) -> Result<std::sync::RwLockWriteGuard<'_, Chain>, ErrorObjectOwned> {
    chain.write().map_err(|_| {
        ErrorObjectOwned::owned(-32603, "internal error: chain lock poisoned", None::<()>)
    })
}

/// Helper: acquire a read lock on the tx pool, returning a JSON-RPC internal error on lock poisoning.
fn read_txpool(
    txpool: &Arc<RwLock<TxPool>>,
) -> Result<std::sync::RwLockReadGuard<'_, TxPool>, ErrorObjectOwned> {
    txpool.read().map_err(|_| {
        ErrorObjectOwned::owned(-32603, "internal error: txpool lock poisoned", None::<()>)
    })
}

/// Helper: acquire a write lock on the tx pool, returning a JSON-RPC internal error on lock poisoning.
fn write_txpool(
    txpool: &Arc<RwLock<TxPool>>,
) -> Result<std::sync::RwLockWriteGuard<'_, TxPool>, ErrorObjectOwned> {
    txpool.write().map_err(|_| {
        ErrorObjectOwned::owned(-32603, "internal error: txpool lock poisoned", None::<()>)
    })
}

/// Shared state accessible by RPC handlers.
pub struct RpcState {
    pub chain: Arc<RwLock<Chain>>,
    pub txpool: Arc<RwLock<TxPool>>,
    /// Cumulative VTT burned from gas fees (raw, stored as high 64 bits lost — in whole VTT units × 1000 for milli-VTT precision).
    pub total_burned_milli: AtomicU64,
    /// Cumulative VTT minted as block rewards (whole VTT units × 1000).
    pub total_minted_milli: AtomicU64,
    /// Optional node metrics for monitoring.
    pub metrics: Option<Arc<NodeMetrics>>,
    /// Per-IP rate limiter for sendTransaction.
    pub send_tx_limiter: PerIpRateLimiter,
    /// Per-IP rate limiter for expensive read RPCs that walk the chain.
    pub heavy_read_limiter: PerIpRateLimiter,
}

/// Implementation of the VTT JSON-RPC API.
struct VttRpcImpl {
    state: Arc<RpcState>,
}

#[async_trait]
impl VttApiServer for VttRpcImpl {
    async fn get_balance(&self, address: Address) -> Result<Amount, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain.get_balance_of(&address))
    }

    async fn get_account(&self, address: Address) -> Result<AccountInfo, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        let account = chain.state().get_account(&address);
        Ok(AccountInfo {
            address,
            balance: account.balance,
            nonce: account.nonce,
            is_contract: account.is_contract(),
        })
    }

    async fn get_block(&self, hash: H256) -> Result<Option<BlockInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain
            .get_block(&hash)
            .map(|block| BlockInfo::from_header(&block.header, hash, block.tx_count())))
    }

    async fn get_block_by_number(
        &self,
        number: BlockNumber,
    ) -> Result<Option<BlockInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain.get_block_by_number(number).map(|block| {
            let hash = blake3_hash(&block.header.signable_bytes());
            BlockInfo::from_header(&block.header, hash, block.tx_count())
        }))
    }

    async fn chain_height(&self) -> Result<BlockNumber, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain.height().unwrap_or(0))
    }

    async fn chain_status(&self) -> Result<ChainStatus, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        let vs = chain.validator_set();
        Ok(ChainStatus {
            chain_id: chain
                .head()
                .map(|h| h.chain_id)
                .unwrap_or(vtt_primitives::ChainId::RELAY),
            height: chain.height().unwrap_or(0),
            head_hash: chain.head_hash().unwrap_or(H256::ZERO),
            validator_count: vs.len(),
            total_stake: vs.total_stake(),
            total_burned: Amount::from_raw(
                self.state.total_burned_milli.load(Ordering::Relaxed) as u128 * 10u128.pow(15),
            ),
            total_minted: Amount::from_raw(
                self.state.total_minted_milli.load(Ordering::Relaxed) as u128 * 10u128.pow(15),
            ),
        })
    }

    async fn get_consensus_params(&self) -> Result<ConsensusParamsRpc, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        let p = chain.consensus().params();
        Ok(ConsensusParamsRpc {
            epoch_length: p.epoch_length,
            block_time_ms: p.block_time_ms,
            active_validators: p.active_validators,
            min_self_stake: p.min_self_stake,
            unbonding_period_secs: p.unbonding_period_secs,
            slash_double_sign_bps: p.slash_double_sign_bps,
            slash_downtime_bps: p.slash_downtime_bps,
            downtime_threshold_pct: p.downtime_threshold_pct,
        })
    }

    async fn get_gas_config(&self) -> Result<GasConfigRpc, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        let g = chain.gas_config();
        Ok(GasConfigRpc {
            min_gas_price: g.min_gas_price,
            base_transfer_cost: g.base_transfer_cost,
            cost_per_byte: g.cost_per_byte,
        })
    }

    async fn get_validators(&self) -> Result<Vec<ValidatorInfoRpc>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        let vs = chain.validator_set();
        Ok(vs
            .validators
            .iter()
            .map(|v| ValidatorInfoRpc {
                address: v.address,
                total_stake: v.total_stake,
                self_stake: v.self_stake,
                commission_bps: v.commission_bps,
                is_active: true, // all validators in the set are active by definition
            })
            .collect())
    }

    async fn tx_pool_size(&self) -> Result<usize, ErrorObjectOwned> {
        let pool = read_txpool(&self.state.txpool)?;
        Ok(pool.len())
    }

    async fn get_asset(&self, asset_id: H256) -> Result<Option<AssetInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain.state().get_asset(&asset_id).map(asset_to_info))
    }

    async fn get_asset_balance(
        &self,
        asset_id: H256,
        address: Address,
    ) -> Result<AssetBalanceInfo, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        let record = chain.state().get_ownership(&asset_id, &address);
        Ok(AssetBalanceInfo {
            asset_id,
            owner: address,
            available: record.available,
            locked: record.locked,
        })
    }

    async fn list_assets(&self) -> Result<Vec<AssetInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain
            .state()
            .iter_assets()
            .map(|(_, a)| asset_to_info(a))
            .collect())
    }

    async fn get_oracle(&self, feed_id: H256) -> Result<Option<OracleFeedInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain.state().get_oracle(&feed_id).map(|f| OracleFeedInfo {
            feed_id: f.feed_id,
            name: f.name.clone(),
            latest_value: f.latest_value,
            updated_at: f.updated_at,
            quorum: f.quorum,
            sources: f.authorized_sources.len(),
            decimals: f.decimals,
        }))
    }

    async fn list_oracles(&self) -> Result<Vec<OracleFeedInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain
            .state()
            .iter_oracles()
            .map(|(_, f)| OracleFeedInfo {
                feed_id: f.feed_id,
                name: f.name.clone(),
                latest_value: f.latest_value,
                updated_at: f.updated_at,
                quorum: f.quorum,
                sources: f.authorized_sources.len(),
                decimals: f.decimals,
            })
            .collect())
    }

    async fn send_transaction(&self, tx_hex: String) -> Result<H256, ErrorObjectOwned> {
        // Per-IP rate limit check. `current_client_ip()` reads the task-local
        // populated by `ClientIpLayer` from X-Forwarded-For / Forwarded.
        if !self.state.send_tx_limiter.check(current_client_ip()) {
            return Err(ErrorObjectOwned::owned(
                -32005,
                "Rate limit exceeded",
                None::<()>,
            ));
        }

        // Reject oversized inputs before allocating the decoded buffer.
        if tx_hex.len() > MAX_TX_HEX_LEN {
            return Err(ErrorObjectOwned::owned(
                -32602,
                "tx_hex too large",
                None::<()>,
            ));
        }

        // Decode the hex-encoded signed transaction
        let tx_bytes = hex::decode(&tx_hex)
            .map_err(|_| ErrorObjectOwned::owned(-32602, "invalid hex encoding", None::<()>))?;
        let tx: vtt_primitives::transaction::SignedTransaction = borsh::from_slice(&tx_bytes)
            .map_err(|e| {
                debug!("transaction deserialization failed: {e}");
                ErrorObjectOwned::owned(-32602, "Invalid transaction", None::<()>)
            })?;

        let tx_hash = blake3_hash(&tx.payload_bytes());

        // Add to transaction pool
        let sender = vtt_crypto::address_from_public_key(&tx.public_key);
        let chain = read_chain(&self.state.chain)?;
        let account_nonce = chain.state().get_nonce(&sender);
        drop(chain);

        let mut pool = write_txpool(&self.state.txpool)?;
        pool.add(tx, sender, account_nonce).map_err(|e| {
            debug!("pool add failed for {sender}: {e}");
            ErrorObjectOwned::owned(-32603, "Pool operation failed", None::<()>)
        })?;

        Ok(tx_hash)
    }

    async fn get_staking_info(
        &self,
        address: Address,
    ) -> Result<Option<StakingInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        let account = chain.state().get_account(&address);
        Ok(account.staking.map(|s| StakingInfo {
            address,
            self_stake: s.self_stake,
            total_stake: s.total_stake,
            commission_bps: s.commission_bps,
            active: s.active,
            delegations: s
                .delegations
                .iter()
                .map(|d| DelegationInfo {
                    delegator: d.delegator,
                    amount: d.amount,
                })
                .collect(),
        }))
    }

    async fn list_pools(&self) -> Result<Vec<PoolInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        let mut pools = Vec::new();
        for (_id, data) in chain.state().iter_pools() {
            let pool = vtt_dex::PoolState::try_from_slice(data)
                .map_err(|e| internal_err("pool deserialize", e))?;
            pools.push(pool_state_to_info(&pool));
        }
        Ok(pools)
    }

    async fn get_pool(&self, pool_id: H256) -> Result<Option<PoolInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        match chain.state().get_pool_raw(&pool_id) {
            None => Ok(None),
            Some(data) => {
                let pool = vtt_dex::PoolState::try_from_slice(data)
                    .map_err(|e| internal_err("pool deserialize", e))?;
                Ok(Some(pool_state_to_info(&pool)))
            }
        }
    }

    async fn get_token_price(
        &self,
        token_id: H256,
    ) -> Result<Option<TokenPriceRpc>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        // Iterate all pools, find one where this token is paired with native VTT (H256::ZERO)
        for (_id, data) in chain.state().iter_pools() {
            let pool = vtt_dex::PoolState::try_from_slice(data)
                .map_err(|e| internal_err("pool deserialize", e))?;

            let ra = pool.reserve_a.raw();
            let rb = pool.reserve_b.raw();

            if pool.token_a == token_id && vtt_dex::PoolState::is_native(&pool.token_b) && ra > 0 {
                // price = reserve_b / reserve_a, scaled to 18 decimals
                let price = rb.saturating_mul(10u128.pow(18)) / ra;
                return Ok(Some(TokenPriceRpc {
                    token_id,
                    price_in_vtt: price.to_string(),
                    pool_id: pool.pool_id,
                }));
            }
            if pool.token_b == token_id && vtt_dex::PoolState::is_native(&pool.token_a) && rb > 0 {
                // price = reserve_a / reserve_b, scaled to 18 decimals
                let price = ra.saturating_mul(10u128.pow(18)) / rb;
                return Ok(Some(TokenPriceRpc {
                    token_id,
                    price_in_vtt: price.to_string(),
                    pool_id: pool.pool_id,
                }));
            }
        }
        Ok(None)
    }

    async fn get_pool_prices(&self) -> Result<Vec<PoolPriceRpc>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        let mut prices = Vec::new();
        for (_id, data) in chain.state().iter_pools() {
            let pool = vtt_dex::PoolState::try_from_slice(data)
                .map_err(|e| internal_err("pool deserialize", e))?;

            let ra = pool.reserve_a.raw();
            let rb = pool.reserve_b.raw();

            // price_a_in_b = reserve_b / reserve_a, scaled to 18 decimals
            let price_a_in_b = rb
                .saturating_mul(10u128.pow(18))
                .checked_div(ra)
                .unwrap_or(0);

            // price_b_in_a = reserve_a / reserve_b, scaled to 18 decimals
            let price_b_in_a = ra
                .saturating_mul(10u128.pow(18))
                .checked_div(rb)
                .unwrap_or(0);

            prices.push(PoolPriceRpc {
                pool_id: pool.pool_id,
                token_a: pool.token_a,
                token_b: pool.token_b,
                price_a_in_b: price_a_in_b.to_string(),
                price_b_in_a: price_b_in_a.to_string(),
                tvl_a: ra.to_string(),
                tvl_b: rb.to_string(),
            });
        }
        Ok(prices)
    }

    async fn get_swap_quote(
        &self,
        pool_id: H256,
        amount_in: String,
        a_to_b: bool,
    ) -> Result<SwapQuoteRpc, ErrorObjectOwned> {
        let amount_in_u128: u128 = amount_in.parse().map_err(|_| {
            ErrorObjectOwned::owned(
                -32602,
                "invalid amount_in: expected decimal u128",
                None::<()>,
            )
        })?;

        let chain = read_chain(&self.state.chain)?;
        let data = chain
            .state()
            .get_pool_raw(&pool_id)
            .ok_or_else(|| ErrorObjectOwned::owned(-32602, "pool not found", None::<()>))?;
        let pool = vtt_dex::PoolState::try_from_slice(data)
            .map_err(|e| internal_err("pool deserialize", e))?;

        let (reserve_in, reserve_out) = if a_to_b {
            (pool.reserve_a.raw(), pool.reserve_b.raw())
        } else {
            (pool.reserve_b.raw(), pool.reserve_a.raw())
        };

        let (amount_in_net, _lp_fee, _protocol_fee) =
            vtt_dex::math::calculate_fees(amount_in_u128, pool.fee_bps, pool.protocol_fee_bps)
                .map_err(|e| internal_err("fee calculation", e))?;

        // total_fee = lp_fee + protocol_fee, already captured as amount_in - amount_in_net
        let total_fee = amount_in_u128.saturating_sub(amount_in_net);

        let amount_out = vtt_dex::math::get_amount_out(amount_in_net, reserve_in, reserve_out)
            .map_err(|e| internal_err("swap quote", e))?;

        // Price impact in bps: (amount_in_net / reserve_in) * 10000
        let price_impact_bps = if reserve_in > 0 {
            ((amount_in_net as u64).saturating_mul(10_000)
                / reserve_in.min(u64::MAX as u128) as u64) as u32
        } else {
            0
        };

        Ok(SwapQuoteRpc {
            amount_in: amount_in_u128.to_string(),
            amount_out: amount_out.to_string(),
            price_impact_bps,
            fee: total_fee.to_string(),
        })
    }

    async fn list_transactions(
        &self,
        page: usize,
        limit: usize,
    ) -> Result<PaginatedResult<TransactionInfo>, ErrorObjectOwned> {
        check_heavy_read(&self.state)?;
        let limit = limit.min(100); // cap to prevent resource exhaustion
        let chain = read_chain(&self.state.chain)?;
        // Bound the scan to just what this page needs instead of materialising
        // every tx from the last 20k blocks on every request.
        let need = (page + 1).saturating_mul(limit);
        let collected = collect_txs_until(&chain, |_| true, need);
        let total = collected.len();
        let start = page * limit;
        let items = collected.into_iter().skip(start).take(limit).collect();
        Ok(PaginatedResult {
            items,
            total,
            page,
            page_size: limit,
        })
    }

    async fn get_transaction(
        &self,
        hash: H256,
    ) -> Result<Option<TransactionInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        // O(1) via the tx_hash -> (block_number, tx_index) index populated at
        // import time. Falls back to a bounded linear scan for transactions
        // imported before the index was introduced (legacy testnet blocks).
        if let Some((n, idx)) = chain.get_tx_location(&hash) {
            if let Some(block) = chain.get_block_by_number(n) {
                if let Some(tx) = block.transactions.get(idx as usize) {
                    return Ok(Some(tx_to_info(
                        tx,
                        block.header.number,
                        block.header.timestamp,
                    )));
                }
            }
        }
        let height = chain.height().unwrap_or(0);
        let min = height.saturating_sub(MAX_BLOCK_SCAN_DEPTH);
        for n in (min..=height).rev() {
            if let Some(block) = chain.get_block_by_number(n) {
                for tx in &block.transactions {
                    let tx_hash = blake3_hash(&tx.payload_bytes());
                    if tx_hash == hash {
                        return Ok(Some(tx_to_info(
                            tx,
                            block.header.number,
                            block.header.timestamp,
                        )));
                    }
                }
            }
        }
        Ok(None)
    }

    async fn get_transactions_by_address(
        &self,
        address: Address,
        page: usize,
        limit: usize,
    ) -> Result<PaginatedResult<TransactionInfo>, ErrorObjectOwned> {
        check_heavy_read(&self.state)?;
        let limit = limit.min(100); // cap to prevent resource exhaustion
        let chain = read_chain(&self.state.chain)?;
        let need = (page + 1).saturating_mul(limit);
        let all = collect_txs_until(
            &chain,
            |tx| tx.from == address || tx.to == Some(address),
            need,
        );
        let total = all.len();
        let start = page * limit;
        let items = all.into_iter().skip(start).take(limit).collect();
        Ok(PaginatedResult {
            items,
            total,
            page,
            page_size: limit,
        })
    }

    async fn get_asset_proposals(
        &self,
        asset_id: H256,
    ) -> Result<Vec<AssetProposalInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        let proposals = chain.state().iter_asset_proposals_for_asset(&asset_id);
        Ok(proposals.into_iter().map(proposal_to_info).collect())
    }

    async fn get_asset_proposal(
        &self,
        proposal_id: H256,
    ) -> Result<Option<AssetProposalInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain
            .state()
            .get_asset_proposal(&proposal_id)
            .map(proposal_to_info))
    }

    async fn get_bridge_withdrawals(&self) -> Result<Vec<BridgeWithdrawalInfo>, ErrorObjectOwned> {
        use vtt_primitives::transaction::TransactionAction;

        check_heavy_read(&self.state)?;
        let chain = read_chain(&self.state.chain)?;
        let height = chain.height().unwrap_or(0);
        let min = height.saturating_sub(MAX_BLOCK_SCAN_DEPTH);
        let mut withdrawals = Vec::new();

        for n in (min..=height).rev() {
            if let Some(block) = chain.get_block_by_number(n) {
                let ts = block.header.timestamp;
                let bn = block.header.number;
                for tx in &block.transactions {
                    if let TransactionAction::BridgeWithdraw {
                        token,
                        amount,
                        destination_chain,
                        destination_address,
                    } = &tx.payload.action
                    {
                        let tx_hash = blake3_hash(&tx.payload_bytes());
                        let sender = vtt_crypto::address_from_public_key(&tx.public_key);
                        withdrawals.push(BridgeWithdrawalInfo {
                            tx_hash,
                            block_number: bn,
                            sender,
                            token: *token,
                            amount: *amount,
                            destination_chain: *destination_chain,
                            destination_address: *destination_address,
                            timestamp: ts,
                        });
                    }
                }
            }
        }

        Ok(withdrawals)
    }

    async fn get_node_metrics(&self) -> Result<NodeMetricsInfo, ErrorObjectOwned> {
        match &self.state.metrics {
            Some(m) => Ok(NodeMetricsInfo {
                block_height: m.block_height.get(),
                connected_peers: m.connected_peers.get(),
                txpool_size: m.txpool_size.get(),
                blocks_imported: m.blocks_imported.get(),
                transactions_executed: m.transactions_executed.get(),
                current_epoch: m.current_epoch.get(),
                active_validators: m.active_validators.get(),
            }),
            None => {
                // Fallback: derive basic metrics from chain state
                let chain = read_chain(&self.state.chain)?;
                let pool = read_txpool(&self.state.txpool)?;
                Ok(NodeMetricsInfo {
                    block_height: chain.height().unwrap_or(0) as i64,
                    connected_peers: 0,
                    txpool_size: pool.len() as i64,
                    blocks_imported: 0,
                    transactions_executed: 0,
                    current_epoch: 0,
                    active_validators: chain.validator_set().validators.len() as i64,
                })
            }
        }
    }

    async fn list_proposals(&self) -> Result<Vec<ProposalInfo>, ErrorObjectOwned> {
        use vtt_consensus::governance::Proposal;

        let chain = read_chain(&self.state.chain)?;
        let mut proposals = Vec::new();
        for (_id, data) in chain.state().iter_governance_proposals() {
            if let Ok(p) = borsh::from_slice::<Proposal>(data) {
                proposals.push(gov_proposal_to_info(&p));
            }
        }
        Ok(proposals)
    }

    async fn get_proposal(&self, id: H256) -> Result<Option<ProposalInfo>, ErrorObjectOwned> {
        use vtt_consensus::governance::Proposal;

        let chain = read_chain(&self.state.chain)?;
        match chain.state().get_governance_proposal(&id) {
            None => Ok(None),
            Some(data) => {
                let p = borsh::from_slice::<Proposal>(data).map_err(|e| {
                    ErrorObjectOwned::owned(
                        -32603,
                        format!("proposal deserialize: {e}"),
                        None::<()>,
                    )
                })?;
                Ok(Some(gov_proposal_to_info(&p)))
            }
        }
    }

    async fn get_block_range(
        &self,
        from: u64,
        count: u64,
    ) -> Result<Vec<BlockInfo>, ErrorObjectOwned> {
        let count = count.min(100);
        let chain = read_chain(&self.state.chain)?;
        let mut blocks = Vec::with_capacity(count as usize);
        for n in from..from.saturating_add(count) {
            if let Some(block) = chain.get_block_by_number(n) {
                let hash = blake3_hash(&block.header.signable_bytes());
                blocks.push(BlockInfo::from_header(
                    &block.header,
                    hash,
                    block.tx_count(),
                ));
            }
        }
        Ok(blocks)
    }

    async fn get_asset_balances(
        &self,
        address: Address,
        asset_ids: Vec<H256>,
    ) -> Result<Vec<AssetBalanceInfo>, ErrorObjectOwned> {
        if asset_ids.len() > 100 {
            return Err(ErrorObjectOwned::owned(
                -32602,
                "too many asset_ids (max 100)",
                None::<()>,
            ));
        }
        let chain = read_chain(&self.state.chain)?;
        let mut balances = Vec::with_capacity(asset_ids.len());
        for asset_id in &asset_ids {
            let record = chain.state().get_ownership(asset_id, &address);
            balances.push(AssetBalanceInfo {
                asset_id: *asset_id,
                owner: address,
                available: record.available,
                locked: record.locked,
            });
        }
        Ok(balances)
    }

    async fn get_transaction_receipt(
        &self,
        tx_hash: H256,
    ) -> Result<Option<ReceiptInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain.get_receipt(&tx_hash).map(|r| ReceiptInfo {
            tx_hash: r.tx_hash,
            success: r.success,
            gas_used: r.gas_used,
            log_count: r.logs.len(),
            logs: r
                .logs
                .into_iter()
                .map(|l| LogInfo {
                    address: l.address,
                    topics: l.topics,
                    data: format!("0x{}", hex::encode(&l.data)),
                })
                .collect(),
        }))
    }

    async fn is_kyc_approved(&self, address: Address) -> Result<bool, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain.state().is_kyc_approved(&address))
    }

    async fn get_bridge_relayer(&self) -> Result<Address, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain.state().bridge_relayer())
    }

    async fn get_slashing_history(
        &self,
        validator: Address,
    ) -> Result<Vec<SlashRecordInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain
            .state()
            .slashing_history(&validator)
            .into_iter()
            .map(|r| SlashRecordInfo {
                validator: r.validator,
                epoch: r.epoch,
                reason: r.reason,
                amount: r.amount,
            })
            .collect())
    }

    async fn list_registered_chains(&self) -> Result<Vec<RegisteredChainInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        let mut out = Vec::new();
        for (chain_id, bytes) in chain.state().iter_registered_chains() {
            if let Some(info) = decode_registered_chain(chain_id, &bytes) {
                out.push(info);
            }
        }
        out.sort_by_key(|c| c.chain_id);
        Ok(out)
    }

    async fn get_registered_chain(
        &self,
        chain_id: u32,
    ) -> Result<Option<RegisteredChainInfo>, ErrorObjectOwned> {
        let chain = read_chain(&self.state.chain)?;
        Ok(chain
            .state()
            .get_registered_chain(chain_id)
            .and_then(|b| decode_registered_chain(chain_id, &b)))
    }
}

fn decode_registered_chain(chain_id: u32, bytes: &[u8]) -> Option<RegisteredChainInfo> {
    use vtt_multichain::RegisteredChain;
    let record: RegisteredChain = borsh::from_slice(bytes).ok()?;
    let compliance_mode = if record.compliance.requires_identity {
        "permissioned".to_string()
    } else {
        "permissionless".to_string()
    };
    Some(RegisteredChainInfo {
        chain_id,
        name: record.name,
        description: record.description,
        validator_count: record.validator_count,
        compliance_mode,
        active: record.active,
        registered_at: record.registered_at,
        proposer: record.proposer,
        // Routing stays `false` until a relayer is wired up — this flag is
        // what the UI / clients check before building CrossChainTransfer.
        routable: false,
    })
}

/// Convert an internal AssetRecord to the RPC-facing AssetInfo, including
/// the new fields (transfer_mode, registrar, requires_kyc, redemption_pool,
/// asset_class).
fn asset_to_info(a: &vtt_state::AssetRecord) -> AssetInfo {
    use vtt_state::asset::{AssetClass, TransferMode};
    let asset_class = match &a.class {
        AssetClass::Equity => "equity".to_string(),
        AssetClass::Debt => "debt".to_string(),
        AssetClass::RealEstate => "real_estate".to_string(),
        AssetClass::Commodity => "commodity".to_string(),
        AssetClass::Fund => "fund".to_string(),
        AssetClass::IntellectualProperty => "ip".to_string(),
        AssetClass::CarbonCredit => "carbon".to_string(),
        AssetClass::Invoice => "invoice".to_string(),
        AssetClass::Custom(s) => format!("custom:{s}"),
    };
    let transfer_mode = match a.transfer_mode {
        TransferMode::PeerToPeer => "PeerToPeer".to_string(),
        TransferMode::RegistrarMediated => "RegistrarMediated".to_string(),
    };
    AssetInfo {
        id: a.id,
        name: a.name.clone(),
        symbol: a.symbol.clone(),
        issuer: a.issuer,
        total_supply: a.total_supply,
        status: a.status_str().to_string(),
        decimals: a.decimals,
        jurisdiction: a.jurisdiction.clone(),
        legal_entity: a.legal_entity.clone(),
        transfer_mode,
        registrar: a.registrar,
        requires_kyc: a.requires_kyc,
        redemption_pool: a.redemption_pool,
        asset_class,
    }
}

/// Convert a protocol governance `Proposal` to RPC-friendly `ProposalInfo`.
fn gov_proposal_to_info(p: &vtt_consensus::governance::Proposal) -> ProposalInfo {
    use vtt_consensus::governance::{ProposalAction, ProposalStatus};

    let (action_type, action_detail) = match &p.action {
        ProposalAction::ParameterChange { key, value } => {
            ("ParameterChange", Some(format!("{} = {}", key, value)))
        }
        ProposalAction::RegisterChain { name, .. } => {
            ("RegisterChain", Some(format!("chain: {}", name)))
        }
        ProposalAction::TreasurySpend { recipient, amount } => (
            "TreasurySpend",
            Some(format!("{} VTT to {}", amount, recipient)),
        ),
        ProposalAction::ProtocolUpgrade {
            version,
            description,
        } => (
            "ProtocolUpgrade",
            Some(format!("v{}: {}", version, description)),
        ),
        ProposalAction::DexPause(true) => ("DexPause", Some("paused: true".to_string())),
        ProposalAction::DexPause(false) => ("DexUnpause", Some("paused: false".to_string())),
        ProposalAction::BridgePause(true) => ("BridgePause", Some("paused: true".to_string())),
        ProposalAction::BridgePause(false) => ("BridgeUnpause", Some("paused: false".to_string())),
    };

    let status = match &p.status {
        ProposalStatus::Active => "active",
        ProposalStatus::Passed => "passed",
        ProposalStatus::Queued { .. } => "queued",
        ProposalStatus::Rejected => "rejected",
        ProposalStatus::Executed => "executed",
    }
    .to_string();

    ProposalInfo {
        id: p.id,
        proposer: p.proposer,
        description: p.description.clone(),
        action_type: action_type.to_string(),
        status,
        votes_yes: p.votes_yes,
        votes_no: p.votes_no,
        votes_abstain: p.votes_abstain,
        created_at: p.created_at,
        voting_end: p.voting_end,
        action_detail,
    }
}

/// Build a `TransactionInfo` from a `SignedTransaction` within a block.
fn tx_to_info(
    tx: &vtt_primitives::transaction::SignedTransaction,
    block_number: BlockNumber,
    timestamp: u64,
) -> TransactionInfo {
    use vtt_primitives::transaction::TransactionAction;

    let hash = blake3_hash(&tx.payload_bytes());
    let from = vtt_crypto::address_from_public_key(&tx.public_key);

    let (action_type, to, amount, swap_pool_id, swap_token_in, swap_min_out) =
        match &tx.payload.action {
            TransactionAction::Transfer { to, amount } => {
                ("Transfer".to_string(), Some(*to), *amount, None, None, None)
            }
            TransactionAction::Stake { validator, amount } => (
                "Stake".to_string(),
                Some(*validator),
                *amount,
                None,
                None,
                None,
            ),
            TransactionAction::Unstake { validator, amount } => (
                "Unstake".to_string(),
                Some(*validator),
                *amount,
                None,
                None,
                None,
            ),
            TransactionAction::AssetTransfer { to, amount, .. } => (
                "AssetTransfer".to_string(),
                Some(*to),
                *amount,
                None,
                None,
                None,
            ),
            TransactionAction::DeployContract { .. } => (
                "DeployContract".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::CallContract {
                contract, value, ..
            } => (
                "CallContract".to_string(),
                Some(*contract),
                *value,
                None,
                None,
                None,
            ),
            TransactionAction::GovernanceVote { .. } => (
                "GovernanceVote".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::CreateAssetClass { total_supply, .. } => (
                "CreateAssetClass".to_string(),
                None,
                *total_supply,
                None,
                None,
                None,
            ),
            TransactionAction::CrossChainTransfer { to, .. } => (
                "CrossChainTransfer".to_string(),
                Some(*to),
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::CreatePool { amount_a, .. } => {
                ("CreatePool".to_string(), None, *amount_a, None, None, None)
            }
            TransactionAction::AddLiquidity { amount_a, .. } => (
                "AddLiquidity".to_string(),
                None,
                *amount_a,
                None,
                None,
                None,
            ),
            TransactionAction::RemoveLiquidity { lp_amount, .. } => (
                "RemoveLiquidity".to_string(),
                None,
                *lp_amount,
                None,
                None,
                None,
            ),
            TransactionAction::Swap {
                pool_id,
                token_in,
                amount_in,
                min_amount_out,
            } => (
                "Swap".to_string(),
                None,
                *amount_in,
                Some(hex::encode(pool_id.as_bytes())),
                Some(hex::encode(token_in.as_bytes())),
                Some(*min_amount_out),
            ),
            TransactionAction::ClaimRevenue { .. } => (
                "ClaimRevenue".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::ClaimMiningRewards { .. } => (
                "ClaimMiningRewards".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::DistributeRevenue { total_amount, .. } => (
                "DistributeRevenue".to_string(),
                None,
                *total_amount,
                None,
                None,
                None,
            ),
            TransactionAction::ProposeAssetAction { .. } => (
                "ProposeAssetAction".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::VoteAssetProposal { .. } => (
                "VoteAssetProposal".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::FinalizeAssetProposal { .. } => (
                "FinalizeAssetProposal".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::BridgeWithdraw {
                destination_address,
                amount,
                ..
            } => (
                "BridgeWithdraw".to_string(),
                Some(*destination_address),
                *amount,
                None,
                None,
                None,
            ),
            TransactionAction::GovernancePropose { .. } => (
                "GovernancePropose".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::FreezeAsset { .. } => (
                "FreezeAsset".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::UnfreezeAsset { .. } => (
                "UnfreezeAsset".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::SubmitSlashingEvidence { .. } => (
                "SubmitSlashingEvidence".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::BridgeDeposit {
                recipient, amount, ..
            } => (
                "BridgeDeposit".to_string(),
                Some(*recipient),
                *amount,
                None,
                None,
                None,
            ),
            TransactionAction::FundRedemptionPool { amount, .. } => (
                "FundRedemptionPool".to_string(),
                None,
                *amount,
                None,
                None,
                None,
            ),
            TransactionAction::ClaimRedemption { .. } => (
                "ClaimRedemption".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::SetKycApproval { address, .. } => (
                "SetKycApproval".to_string(),
                Some(*address),
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::SetAddressJurisdiction { address, .. } => (
                "SetAddressJurisdiction".to_string(),
                Some(*address),
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::CreateOracleFeed { .. } => (
                "CreateOracleFeed".to_string(),
                None,
                Amount::ZERO,
                None,
                None,
                None,
            ),
            TransactionAction::SubmitOracleValue { value, .. } => (
                "SubmitOracleValue".to_string(),
                None,
                *value,
                None,
                None,
                None,
            ),
        };

    TransactionInfo {
        hash,
        block_number,
        from,
        to,
        action_type,
        amount,
        nonce: tx.payload.nonce,
        gas_price: tx.payload.gas_price,
        gas_limit: tx.payload.gas_limit,
        timestamp,
        swap_pool_id,
        swap_token_in,
        swap_min_out,
    }
}

/// Walk recent blocks (newest first), collecting transactions that pass
/// `accept`, and stop as soon as `cap` entries are gathered. Bounded by
/// `MAX_BLOCK_SCAN_DEPTH`. Callers now pass a `cap` of
/// `(page + 1) * limit` so the scan short-circuits after enough matches,
/// which supersedes the earlier full-chain sweep.
fn collect_txs_until<F: Fn(&TransactionInfo) -> bool>(
    chain: &Chain,
    accept: F,
    cap: usize,
) -> Vec<TransactionInfo> {
    let height = chain.height().unwrap_or(0);
    let min = height.saturating_sub(MAX_BLOCK_SCAN_DEPTH);
    let mut txs = Vec::new();
    for n in (min..=height).rev() {
        if txs.len() >= cap {
            break;
        }
        if let Some(block) = chain.get_block_by_number(n) {
            let ts = block.header.timestamp;
            let bn = block.header.number;
            for tx in &block.transactions {
                let info = tx_to_info(tx, bn, ts);
                if accept(&info) {
                    txs.push(info);
                    if txs.len() >= cap {
                        break;
                    }
                }
            }
        }
    }
    txs
}

/// Convert a `PoolState` into the RPC-friendly `PoolInfo`.
fn pool_state_to_info(pool: &vtt_dex::PoolState) -> PoolInfo {
    PoolInfo {
        pool_id: hex::encode(pool.pool_id.as_bytes()),
        token_a: hex::encode(pool.token_a.as_bytes()),
        token_b: hex::encode(pool.token_b.as_bytes()),
        reserve_a: pool.reserve_a.raw().to_string(),
        reserve_b: pool.reserve_b.raw().to_string(),
        lp_token_id: hex::encode(pool.lp_token_id.as_bytes()),
        lp_total_supply: pool.lp_total_supply.raw().to_string(),
        fee_bps: pool.fee_bps,
        protocol_fee_bps: pool.protocol_fee_bps,
        protocol_fees_a: pool.protocol_fees_a.raw().to_string(),
        protocol_fees_b: pool.protocol_fees_b.raw().to_string(),
    }
}

/// Convert an `AssetProposal` into the RPC-friendly `AssetProposalInfo`.
fn proposal_to_info(p: &vtt_primitives::asset_governance::AssetProposal) -> AssetProposalInfo {
    use vtt_primitives::asset_governance::{AssetProposalAction, AssetProposalStatus};

    let action_type = match &p.action {
        AssetProposalAction::DistributeRevenue { .. } => "DistributeRevenue",
        AssetProposalAction::ChangeIssuer { .. } => "ChangeIssuer",
        AssetProposalAction::Signal { .. } => "Signal",
        AssetProposalAction::DisposeAsset { .. } => "DisposeAsset",
        AssetProposalAction::FinalizeRedemption { .. } => "FinalizeRedemption",
    }
    .to_string();

    let status = match &p.status {
        AssetProposalStatus::Active => "Active",
        AssetProposalStatus::Passed => "Passed",
        AssetProposalStatus::Rejected => "Rejected",
        AssetProposalStatus::Executed => "Executed",
    }
    .to_string();

    AssetProposalInfo {
        id: p.id,
        asset_id: p.asset_id,
        proposer: p.proposer,
        action_type,
        description: p.description.clone(),
        status,
        votes_yes: p.votes_yes,
        votes_no: p.votes_no,
        votes_abstain: p.votes_abstain,
        voting_end: p.voting_end,
        created_at: p.created_at,
    }
}

/// The RPC server wrapper.
pub struct RpcServer {
    state: Arc<RpcState>,
}

impl RpcServer {
    /// Create a new RPC server with shared state.
    pub fn new(chain: Arc<RwLock<Chain>>, txpool: Arc<RwLock<TxPool>>) -> Self {
        Self {
            state: Arc::new(RpcState {
                chain,
                txpool,
                total_burned_milli: AtomicU64::new(0),
                total_minted_milli: AtomicU64::new(0),
                metrics: None,
                send_tx_limiter: PerIpRateLimiter::new(10),
                heavy_read_limiter: PerIpRateLimiter::new(60),
            }),
        }
    }

    /// Create a new RPC server with shared state and node metrics.
    pub fn with_metrics(
        chain: Arc<RwLock<Chain>>,
        txpool: Arc<RwLock<TxPool>>,
        metrics: Arc<NodeMetrics>,
    ) -> Self {
        Self {
            state: Arc::new(RpcState {
                chain,
                txpool,
                total_burned_milli: AtomicU64::new(0),
                total_minted_milli: AtomicU64::new(0),
                metrics: Some(metrics),
                send_tx_limiter: PerIpRateLimiter::new(10),
                heavy_read_limiter: PerIpRateLimiter::new(60),
            }),
        }
    }

    /// Get a reference to the shared state (for the validator to update counters).
    pub fn shared_state(&self) -> Arc<RpcState> {
        self.state.clone()
    }

    /// Start the JSON-RPC server on the given address.
    ///
    /// The server is configured with:
    /// - CORS headers allowing any origin (POST with Content-Type)
    /// - 1 MB request body size limit
    /// - Per-IP rate limiting on sendTransaction
    pub async fn start(self, addr: SocketAddr) -> Result<SocketAddr, Box<dyn std::error::Error>> {
        // CORS: allow any origin, POST method, Content-Type header.
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([hyper::Method::POST])
            .allow_headers([hyper::header::CONTENT_TYPE]);

        // Request body size limit: 4 MiB. Sized to accommodate the largest
        // reasonable DeployContract tx (up to ~2 MiB of hex-encoded WASM)
        // plus JSON-RPC envelope overhead.
        let body_limit = RequestBodyLimitLayer::new(4 * 1024 * 1024);

        // Order: body_limit is innermost (applied first to the service),
        // CORS is outermost (applied last, intercepts preflight OPTIONS before body limit).
        // ClientIpLayer sits between them so the scoped task-local is set on
        // every request before jsonrpsee's handler runs.
        let middleware = tower::ServiceBuilder::new()
            .layer(body_limit)
            .layer(ClientIpLayer)
            .layer(cors);

        let server = Server::builder()
            .set_http_middleware(middleware)
            .build(addr)
            .await?;
        let local_addr = server.local_addr()?;

        let state_for_cleanup = self.state.clone();
        let rpc_impl = VttRpcImpl { state: self.state };

        let handle = server.start(rpc_impl.into_rpc());

        info!(%local_addr, "JSON-RPC server started");

        // Periodic rate-limiter cleanup: without this the per-IP client
        // HashMap grows unbounded over the lifetime of the node since
        // every new IP pins a (count, window_start) entry. Every minute
        // we prune entries idle for 60+ seconds.
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                ticker.tick().await;
                state_for_cleanup.send_tx_limiter.cleanup();
                state_for_cleanup.heavy_read_limiter.cleanup();
            }
        });

        // Keep the server running in the background
        tokio::spawn(async move {
            handle.stopped().await;
        });

        Ok(local_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::core::client::ClientT;
    use jsonrpsee::rpc_params;
    use vtt_consensus::ConsensusEngine;
    use vtt_genesis::{build_genesis, GenesisConfig};
    use vtt_primitives::chain::GasConfig;
    use vtt_txpool::TxPoolConfig;

    async fn setup_rpc() -> (SocketAddr, Arc<RwLock<Chain>>) {
        let genesis_config = GenesisConfig::dev_default();
        let genesis_result = build_genesis(&genesis_config);

        let consensus = ConsensusEngine::new(genesis_config.chain.consensus.clone());
        let mut chain = Chain::new(consensus, GasConfig::default());
        chain
            .init_genesis(genesis_result.block, genesis_result.state)
            .unwrap();

        let chain = Arc::new(RwLock::new(chain));
        let txpool = Arc::new(RwLock::new(TxPool::new(TxPoolConfig::default())));

        let rpc = RpcServer::new(chain.clone(), txpool);
        let addr = rpc.start("127.0.0.1:0".parse().unwrap()).await.unwrap();

        (addr, chain)
    }

    #[tokio::test]
    async fn rpc_chain_height() {
        let (addr, _chain) = setup_rpc().await;

        let client = jsonrpsee::http_client::HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();

        let height: BlockNumber = client
            .request("vtt_chainHeight", rpc_params![])
            .await
            .unwrap();
        assert_eq!(height, 0);
    }

    #[tokio::test]
    async fn rpc_get_balance() {
        let (addr, _chain) = setup_rpc().await;

        let client = jsonrpsee::http_client::HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();

        let dev_addr = vtt_crypto::Keypair::from_seed(&[0x01; 32]).address();
        let balance: Amount = client
            .request("vtt_getBalance", rpc_params![dev_addr])
            .await
            .unwrap();
        assert_eq!(balance, Amount::from_vtt(1_000_000));
    }

    #[tokio::test]
    async fn rpc_chain_status() {
        let (addr, _chain) = setup_rpc().await;

        let client = jsonrpsee::http_client::HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();

        let status: ChainStatus = client
            .request("vtt_chainStatus", rpc_params![])
            .await
            .unwrap();
        assert_eq!(status.height, 0);
        assert_eq!(status.validator_count, 1);
    }

    #[tokio::test]
    async fn rpc_get_validators() {
        let (addr, _chain) = setup_rpc().await;

        let client = jsonrpsee::http_client::HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();

        let validators: Vec<ValidatorInfoRpc> = client
            .request("vtt_getValidators", rpc_params![])
            .await
            .unwrap();
        assert_eq!(validators.len(), 1);
        assert_eq!(validators[0].commission_bps, 500);
    }

    #[tokio::test]
    async fn rpc_get_block_by_number() {
        let (addr, _chain) = setup_rpc().await;

        let client = jsonrpsee::http_client::HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();

        let block: Option<BlockInfo> = client
            .request("vtt_getBlockByNumber", rpc_params![0u64])
            .await
            .unwrap();
        assert!(block.is_some());
        assert_eq!(block.unwrap().number, 0);

        // Non-existent block
        let block2: Option<BlockInfo> = client
            .request("vtt_getBlockByNumber", rpc_params![999u64])
            .await
            .unwrap();
        assert!(block2.is_none());
    }

    #[tokio::test]
    async fn rpc_tx_pool_size() {
        let (addr, _chain) = setup_rpc().await;

        let client = jsonrpsee::http_client::HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .unwrap();

        let size: usize = client
            .request("vtt_txPoolSize", rpc_params![])
            .await
            .unwrap();
        assert_eq!(size, 0);
    }
}
