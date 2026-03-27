use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use jsonrpsee::core::async_trait;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::server::Server;
use jsonrpsee::types::ErrorObjectOwned;
use tracing::info;

use vtt_chain::Chain;
use vtt_crypto::blake3_hash;
use vtt_primitives::amount::Amount;
use vtt_primitives::{Address, BlockNumber, H256};
use vtt_txpool::TxPool;

use crate::types::{
    AccountInfo, AssetBalanceInfo, AssetInfo, BlockInfo, ChainStatus, DelegationInfo,
    OracleFeedInfo, StakingInfo, ValidatorInfoRpc,
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

    /// Submit a signed transaction. Returns the transaction hash.
    #[method(name = "vtt_sendTransaction")]
    async fn send_transaction(&self, tx_hex: String) -> Result<H256, ErrorObjectOwned>;

    /// Get staking info for an address.
    #[method(name = "vtt_getStakingInfo")]
    async fn get_staking_info(
        &self,
        address: Address,
    ) -> Result<Option<StakingInfo>, ErrorObjectOwned>;
}

/// Shared state accessible by RPC handlers.
pub struct RpcState {
    pub chain: Arc<RwLock<Chain>>,
    pub txpool: Arc<RwLock<TxPool>>,
}

/// Implementation of the VTT JSON-RPC API.
struct VttRpcImpl {
    state: Arc<RpcState>,
}

#[async_trait]
impl VttApiServer for VttRpcImpl {
    async fn get_balance(&self, address: Address) -> Result<Amount, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
        Ok(chain.get_balance_of(&address))
    }

    async fn get_account(&self, address: Address) -> Result<AccountInfo, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
        let account = chain.state().get_account(&address);
        Ok(AccountInfo {
            address,
            balance: account.balance,
            nonce: account.nonce,
            is_contract: account.is_contract(),
        })
    }

    async fn get_block(&self, hash: H256) -> Result<Option<BlockInfo>, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
        Ok(chain
            .get_block(&hash)
            .map(|block| BlockInfo::from_header(&block.header, hash, block.tx_count())))
    }

    async fn get_block_by_number(
        &self,
        number: BlockNumber,
    ) -> Result<Option<BlockInfo>, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
        Ok(chain.get_block_by_number(number).map(|block| {
            let hash = blake3_hash(&block.header.signable_bytes());
            BlockInfo::from_header(&block.header, hash, block.tx_count())
        }))
    }

    async fn chain_height(&self) -> Result<BlockNumber, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
        Ok(chain.height().unwrap_or(0))
    }

    async fn chain_status(&self) -> Result<ChainStatus, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
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
        })
    }

    async fn get_validators(&self) -> Result<Vec<ValidatorInfoRpc>, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
        let vs = chain.validator_set();
        Ok(vs
            .validators
            .iter()
            .map(|v| ValidatorInfoRpc {
                address: v.address,
                total_stake: v.total_stake,
                self_stake: v.self_stake,
                commission_bps: v.commission_bps,
            })
            .collect())
    }

    async fn tx_pool_size(&self) -> Result<usize, ErrorObjectOwned> {
        let pool = self.state.txpool.read().unwrap();
        Ok(pool.len())
    }

    async fn get_asset(&self, asset_id: H256) -> Result<Option<AssetInfo>, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
        Ok(chain.state().get_asset(&asset_id).map(|a| AssetInfo {
            id: a.id,
            name: a.name.clone(),
            symbol: a.symbol.clone(),
            issuer: a.issuer,
            total_supply: a.total_supply,
            status: a.status_str().to_string(),
            decimals: a.decimals,
        }))
    }

    async fn get_asset_balance(
        &self,
        asset_id: H256,
        address: Address,
    ) -> Result<AssetBalanceInfo, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
        let record = chain.state().get_ownership(&asset_id, &address);
        Ok(AssetBalanceInfo {
            asset_id,
            owner: address,
            available: record.available,
            locked: record.locked,
        })
    }

    async fn list_assets(&self) -> Result<Vec<AssetInfo>, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
        Ok(chain
            .state()
            .iter_assets()
            .map(|(_, a)| AssetInfo {
                id: a.id,
                name: a.name.clone(),
                symbol: a.symbol.clone(),
                issuer: a.issuer,
                total_supply: a.total_supply,
                status: a.status_str().to_string(),
                decimals: a.decimals,
            })
            .collect())
    }

    async fn get_oracle(&self, feed_id: H256) -> Result<Option<OracleFeedInfo>, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
        Ok(chain.state().get_oracle(&feed_id).map(|f| OracleFeedInfo {
            feed_id: f.feed_id,
            name: f.name.clone(),
            latest_value: f.latest_value,
            updated_at: f.updated_at,
            quorum: f.quorum,
            sources: f.authorized_sources.len(),
        }))
    }

    async fn send_transaction(&self, tx_hex: String) -> Result<H256, ErrorObjectOwned> {
        // Decode the hex-encoded signed transaction
        let tx_bytes = hex::decode(&tx_hex).map_err(|e| {
            ErrorObjectOwned::owned(-32602, format!("invalid hex: {e}"), None::<()>)
        })?;
        let tx: vtt_primitives::transaction::SignedTransaction = borsh::from_slice(&tx_bytes)
            .map_err(|e| {
                ErrorObjectOwned::owned(-32602, format!("invalid transaction: {e}"), None::<()>)
            })?;

        let tx_hash = blake3_hash(&tx.payload_bytes());

        // Add to transaction pool
        let sender = vtt_crypto::address_from_public_key(&tx.public_key);
        let chain = self.state.chain.read().unwrap();
        let account_nonce = chain.state().get_nonce(&sender);
        drop(chain);

        let mut pool = self.state.txpool.write().unwrap();
        pool.add(tx, sender, account_nonce)
            .map_err(|e| ErrorObjectOwned::owned(-32603, format!("pool error: {e}"), None::<()>))?;

        Ok(tx_hash)
    }

    async fn get_staking_info(
        &self,
        address: Address,
    ) -> Result<Option<StakingInfo>, ErrorObjectOwned> {
        let chain = self.state.chain.read().unwrap();
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
}

/// The RPC server wrapper.
pub struct RpcServer {
    state: Arc<RpcState>,
}

impl RpcServer {
    /// Create a new RPC server with shared state.
    pub fn new(chain: Arc<RwLock<Chain>>, txpool: Arc<RwLock<TxPool>>) -> Self {
        Self {
            state: Arc::new(RpcState { chain, txpool }),
        }
    }

    /// Start the JSON-RPC server on the given address.
    pub async fn start(self, addr: SocketAddr) -> Result<SocketAddr, Box<dyn std::error::Error>> {
        let server = Server::builder().build(addr).await?;
        let local_addr = server.local_addr()?;

        let rpc_impl = VttRpcImpl { state: self.state };

        let handle = server.start(rpc_impl.into_rpc());

        info!(%local_addr, "JSON-RPC server started");

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
