pub mod api;
pub mod config;
pub mod error;
pub mod events;
pub mod evm;
pub mod keys;
pub mod models;
pub mod recover;
pub mod store;
pub mod swap;

use std::sync::Arc;

use platform_utils::DefaultHttpClient;
use platform_utils::tokio::sync::mpsc;

pub use config::*;
pub use error::BoltzError;
pub use events::{BoltzEventListener, BoltzSwapEvent, EventEmitter};
pub use keys::EvmKeyManager;
pub use models::*;
pub use store::{BoltzStorage, MemoryBoltzStorage};

use api::BoltzApiClient;
use api::ws::SwapStatusSubscriber;
use evm::alchemy::AlchemyGasClient;
use evm::oft::OftDeployments;
use evm::provider::EvmProvider;
use evm::signing::EvmSigner;
use swap::manager::SwapManager;
use swap::reverse::{ReverseSwapExecutor, current_unix_timestamp};

/// Top-level Boltz service facade.
///
/// Two-step swap flow:
/// - `prepare_reverse_swap` — pure quote, no side effects
/// - `create_reverse_swap` — commit to swap, get invoice; the swap is
///   automatically monitored and progressed to completion in the background
///
/// Call `start()` after construction to resume any active swaps from storage.
/// Register a `BoltzEventListener` to receive swap status updates.
pub struct BoltzService {
    executor: Arc<ReverseSwapExecutor>,
    store: Arc<dyn BoltzStorage>,
    swap_manager: SwapManager,
    event_emitter: Arc<EventEmitter>,
    ws_subscriber: Arc<SwapStatusSubscriber>,
}

impl BoltzService {
    /// Construct from config, seed bytes, and a store implementation.
    pub async fn new(
        config: BoltzConfig,
        seed: &[u8],
        store: Arc<dyn BoltzStorage>,
    ) -> Result<Self, BoltzError> {
        let key_manager = EvmKeyManager::from_seed(seed)?;

        // Derive gas signer for Alchemy
        let chain_id_u32: u32 = config
            .chain_id
            .try_into()
            .map_err(|_| BoltzError::Generic("Chain ID overflow".to_string()))?;
        let gas_key_pair = key_manager.derive_gas_signer(chain_id_u32)?;
        let gas_signer = EvmSigner::new(&gas_key_pair, config.chain_id);

        // Each component gets its own DefaultHttpClient. Instances are cheap
        // (no shared connection pool), so sharing via Arc is not worth the
        // signature churn.
        let api_client = BoltzApiClient::new(&config, Box::new(DefaultHttpClient::new(None)));

        // Create the global WS channel and subscriber.
        let (ws_tx, ws_rx) = mpsc::channel(256);
        let ws_subscriber = Arc::new(SwapStatusSubscriber::connect(&config.ws_url(), ws_tx).await?);

        let alchemy_client = AlchemyGasClient::new(
            &config.alchemy_config,
            Box::new(DefaultHttpClient::new(None)),
            gas_signer,
        );

        let evm_provider = EvmProvider::new(
            config.arbitrum_rpc_url.clone(),
            Box::new(DefaultHttpClient::new(None)),
        );

        // OFT deployments are fetched once and cached for the service lifetime.
        // They change rarely; a service restart picks up any updates.
        let oft_deployments =
            OftDeployments::fetch(&DefaultHttpClient::new(None), &config.oft_deployments_url)
                .await?;

        // Fetch contract addresses from the Boltz API, matching by chain ID
        let contracts = api_client.get_contracts().await?;
        let erc20swap_address = contracts
            .0
            .values()
            .find(|c| c.network.chain_id == config.chain_id)
            .map(|c| c.swap_contracts.erc20_swap.clone())
            .ok_or_else(|| BoltzError::Api {
                reason: format!(
                    "Chain ID {} not found in contracts response",
                    config.chain_id,
                ),
                code: None,
            })?;

        let executor = Arc::new(ReverseSwapExecutor::new(
            api_client,
            key_manager,
            alchemy_client,
            evm_provider,
            oft_deployments,
            config,
            erc20swap_address,
        ));

        let event_emitter = Arc::new(EventEmitter::new());

        let swap_manager = SwapManager::start(
            executor.clone(),
            store.clone(),
            event_emitter.clone(),
            ws_subscriber.clone(),
            ws_rx,
        );

        Ok(Self {
            executor,
            store,
            swap_manager,
            event_emitter,
            ws_subscriber,
        })
    }

    /// Load and resume all active (non-terminal) swaps from storage.
    /// Call once after construction to pick up swaps from previous runs.
    pub async fn resume_swaps(&self) -> Result<Vec<String>, BoltzError> {
        self.swap_manager.resume_all().await
    }

    /// Register an event listener. Returns a unique ID for removal.
    pub async fn add_event_listener(&self, listener: Box<dyn BoltzEventListener>) -> String {
        self.event_emitter.add_listener(listener).await
    }

    /// Remove a previously registered event listener.
    pub async fn remove_event_listener(&self, id: &str) -> bool {
        self.event_emitter.remove_listener(id).await
    }

    /// Get a swap by its internal ID.
    pub async fn get_swap(&self, swap_id: &str) -> Result<Option<BoltzSwap>, BoltzError> {
        self.store.get_swap(swap_id).await
    }

    /// Shut down the swap manager and close the WebSocket connection.
    pub async fn shutdown(&self) {
        self.swap_manager.shutdown().await;
        self.ws_subscriber.close().await;
    }

    /// Get a quote for converting sats to USDT.
    /// Pure quote — no side effects, no swap created.
    pub async fn prepare_reverse_swap(
        &self,
        destination: &str,
        chain: Chain,
        usdt_amount: u64,
    ) -> Result<PreparedSwap, BoltzError> {
        self.executor.prepare(destination, chain, usdt_amount).await
    }

    /// Get a quote starting from input sats (computes expected USDT output).
    /// Pure quote — no side effects, no swap created.
    pub async fn prepare_reverse_swap_from_sats(
        &self,
        destination: &str,
        chain: Chain,
        invoice_amount_sats: u64,
    ) -> Result<PreparedSwap, BoltzError> {
        self.executor
            .prepare_from_sats(destination, chain, invoice_amount_sats)
            .await
    }

    /// Maximum retries when Boltz rejects a preimage hash as already used.
    const MAX_DUPLICATE_RETRIES: u32 = 10;

    /// Create the swap on Boltz and begin background monitoring.
    /// Returns the hold invoice to pay.
    pub async fn create_reverse_swap(
        &self,
        prepared: &PreparedSwap,
    ) -> Result<CreatedSwap, BoltzError> {
        let mut last_err = None;
        for _ in 0..Self::MAX_DUPLICATE_RETRIES {
            let key_index = self
                .store
                .increment_key_index(self.executor.config.chain_id)
                .await?;

            match self.executor.create(prepared, key_index).await {
                Ok(swap) => {
                    let created = CreatedSwap {
                        swap_id: swap.id.clone(),
                        invoice: swap.invoice.clone(),
                        invoice_amount_sats: swap.invoice_amount_sats,
                        timeout_block_height: swap.timeout_block_height,
                    };
                    self.store.insert_swap(&swap).await?;
                    self.swap_manager.track_swap(&created.swap_id).await;
                    return Ok(created);
                }
                Err(e) if e.is_duplicate_preimage() => {
                    tracing::warn!(
                        key_index,
                        "Preimage hash already used, bumping key index and retrying"
                    );
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_err.unwrap_or_else(|| {
            BoltzError::Generic("Exhausted duplicate preimage retries".into())
        }))
    }

    /// Get supported destination chains.
    pub fn supported_chains(&self) -> Vec<Chain> {
        vec![
            Chain::Arbitrum,
            Chain::Berachain,
            Chain::Conflux,
            Chain::Corn,
            Chain::Ethereum,
            Chain::Flare,
            Chain::Hedera,
            Chain::HyperEvm,
            Chain::Ink,
            Chain::Mantle,
            Chain::MegaEth,
            Chain::Monad,
            Chain::Morph,
            Chain::Optimism,
            Chain::Plasma,
            Chain::Polygon,
            Chain::Rootstock,
            Chain::Sei,
            Chain::Stable,
            Chain::Unichain,
            Chain::XLayer,
        ]
    }

    /// Get current Boltz swap limits (min/max sats).
    pub async fn get_limits(&self) -> Result<SwapLimits, BoltzError> {
        self.executor.get_limits().await
    }

    /// Accept a degraded DEX quote and proceed with claiming.
    ///
    /// Call this after receiving a [`BoltzSwapEvent::QuoteDegraded`] event.
    /// The swap must be in `TbtcLocked` or `Claiming` status. The claim will
    /// proceed with the current DEX quote (with on-chain slippage protection
    /// still applied).
    pub async fn accept_degraded_quote(&self, swap_id: &str) -> Result<BoltzSwap, BoltzError> {
        let mut swap = self
            .store
            .get_swap(swap_id)
            .await?
            .ok_or_else(|| BoltzError::Store(format!("Swap not found: {swap_id}")))?;

        if !matches!(
            swap.status,
            BoltzSwapStatus::TbtcLocked | BoltzSwapStatus::Claiming
        ) {
            return Err(BoltzError::Generic(format!(
                "Cannot accept degraded quote: swap {} is {:?}, expected TbtcLocked or Claiming",
                swap_id, swap.status
            )));
        }

        swap::manager::update_swap_status(
            &*self.store,
            &self.event_emitter,
            &mut swap,
            BoltzSwapStatus::Claiming,
        )
        .await;

        match self.executor.claim_and_swap(&swap, true).await {
            Ok(tx_hash) => {
                swap.claim_tx_hash = Some(tx_hash);
                swap.updated_at = current_unix_timestamp();
                self.store.update_swap(&swap).await?;
                Ok(swap)
            }
            Err(e) => {
                tracing::error!(swap_id, error = %e, "Forced claim after accept_degraded_quote failed, staying in Claiming for retry");
                Err(e)
            }
        }
    }

    /// Recover unclaimed swaps by scanning the blockchain.
    pub async fn recover(&self, destination_address: &str) -> Result<RecoveryResult, BoltzError> {
        let (recoverable, stats) = self.executor.scan_recoverable().await?;

        if let Some(highest) = stats.highest_key_index {
            self.store
                .set_key_index_if_higher(
                    self.executor.config.chain_id,
                    highest.saturating_add(1),
                )
                .await?;
        }

        let mut claimed = Vec::new();
        for r in &recoverable {
            let swap = self
                .executor
                .build_recovery_swap(r, destination_address)?;
            if let Err(e) = self.store.insert_swap(&swap).await {
                tracing::error!(
                    key_index = r.key_index,
                    error = %e,
                    "Failed to persist recovery swap"
                );
                continue;
            }
            let mut swap = swap;
            swap::manager::update_swap_status(
                &*self.store,
                &self.event_emitter,
                &mut swap,
                BoltzSwapStatus::Claiming,
            )
            .await;
            match self.executor.claim_and_swap(&swap, true).await {
                Ok(tx_hash) => {
                    swap.claim_tx_hash = Some(tx_hash.clone());
                    swap.updated_at = current_unix_timestamp();
                    if let Err(e) = self.store.update_swap(&swap).await {
                        tracing::error!(key_index = r.key_index, error = %e, "Failed to persist claim tx hash");
                    }
                    claimed.push(ClaimedRecovery {
                        key_index: r.key_index,
                        preimage_hash: r.preimage_hash,
                        claim_tx_hash: tx_hash,
                    });
                }
                Err(e) => {
                    tracing::error!(
                        key_index = r.key_index,
                        tx = r.lockup_tx_hash,
                        error = %e,
                        "Failed to claim recovered swap"
                    );
                }
            }
        }

        Ok(RecoveryResult {
            claimed,
            already_settled: stats.already_settled,
            total_events_scanned: stats.total_events,
            highest_key_index: stats.highest_key_index,
        })
    }
}
