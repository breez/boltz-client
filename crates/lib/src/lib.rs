pub mod api;
pub mod config;
pub mod error;
pub mod events;
pub mod evm;
pub mod keys;
pub mod models;
pub mod solana;
pub mod store;
pub mod swap;

use std::sync::Arc;

use platform_utils::DefaultHttpClient;
use platform_utils::tokio::sync::{Mutex, mpsc};

pub use config::*;
pub use error::BoltzError;
pub use events::{BoltzEventListener, BoltzSwapEvent, EventEmitter};
pub use evm::cctp::CctpMessageStatus;
pub use evm::recipient::is_valid_destination_address;
pub use keys::EvmKeyManager;
pub use models::*;
pub use store::BoltzStorage;

use api::BoltzApiClient;
use api::ws::SwapStatusSubscriber;
use evm::alchemy::AlchemyGasClient;
use evm::oft::fetch_chain_registry;
use evm::provider::EvmProvider;
use evm::signing::EvmSigner;
use solana::rpc::SolanaRpcClient;
use swap::locks::SwapLocks;
use swap::manager::SwapManager;
use swap::reverse::{ReverseSwapExecutor, current_unix_timestamp, resolve_slippage_bps};

/// `User-Agent` sent on every outbound HTTP request. Some upstreams reject
/// header-less requests.
const USER_AGENT: &str = concat!("boltz-client/", env!("CARGO_PKG_VERSION"));

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
    chain_registry: Arc<DestinationRegistry>,
    /// Per-swap serialization, shared with the background [`SwapManager`] loop.
    /// The caller-facing mutators (`accept_degraded_quote`,
    /// `update_swap_slippage`, `refresh_pending_deliveries`) acquire the swap's
    /// lock so they never race the loop's handler for the same swap.
    swap_locks: Arc<SwapLocks>,
    /// Serializes HD key-index issuance in-process so concurrent
    /// `create_reverse_swap` calls can't draw the same index from a
    /// non-transactional store (which would reuse a preimage). See
    /// [`issue_key_index`].
    key_index_lock: Mutex<()>,
}

/// Issue the next HD key index, serialized in-process by `lock`. Concurrent
/// `create_reverse_swap` calls would otherwise both draw the same index from a
/// non-transactional store and reuse a preimage (fund-theft risk). The lock is
/// per-process: durability and cross-process atomicity remain the store's
/// contract (see [`BoltzStorage::increment_key_index`]).
async fn issue_key_index(lock: &Mutex<()>, store: &dyn BoltzStorage) -> Result<u32, BoltzError> {
    let _guard = lock.lock().await;
    store.increment_key_index().await
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

        // Each component gets its own DefaultHttpClient. They mostly hit
        // distinct hosts, so a shared reqwest connection pool would rarely be
        // reused; giving each its own keeps ownership simple (Box, not a
        // cloned Arc). All carry USER_AGENT — some upstreams 403 without it
        // (native only; the WASM client omits it, see ReqwestHttpClient::new).
        let api_client = BoltzApiClient::new(
            &config,
            Box::new(DefaultHttpClient::new(Some(USER_AGENT.to_string()))),
        );

        // Create the global WS channel and subscriber.
        let (ws_tx, ws_rx) = mpsc::channel(256);
        let ws_subscriber = Arc::new(SwapStatusSubscriber::connect(&config.ws_url(), ws_tx).await?);

        let alchemy_client = AlchemyGasClient::new(
            &config.alchemy_config,
            Box::new(DefaultHttpClient::new(Some(USER_AGENT.to_string()))),
            gas_signer,
        );

        let evm_provider = EvmProvider::new(
            config.arbitrum_rpc_url.clone(),
            Box::new(DefaultHttpClient::new(Some(USER_AGENT.to_string()))),
        );

        // Chain registry is fetched once from the USDT0 deployments API and
        // cached for the service lifetime. A service restart picks up any
        // upstream updates — including new destination chains without
        // shipping a new client.
        let chain_registry = Arc::new(
            fetch_chain_registry(
                &DefaultHttpClient::new(Some(USER_AGENT.to_string())),
                &config.oft_deployments_url,
                config.chain_id,
            )
            .await?,
        );

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

        let solana_rpc = SolanaRpcClient::new(
            Box::new(DefaultHttpClient::new(Some(USER_AGENT.to_string()))),
            config.solana_rpc_url.clone(),
        );

        let cctp_fee_client = crate::evm::cctp::CctpFeeClient::new(
            Box::new(DefaultHttpClient::new(Some(USER_AGENT.to_string()))),
            config.cctp_api_url.clone(),
        );

        let lz_scan_client = crate::evm::lz_scan::LzScanClient::new(
            Box::new(DefaultHttpClient::new(Some(USER_AGENT.to_string()))),
            config.lz_scan_api_url.clone(),
        );

        // Capture before `config` is moved into the executor.
        let delivery_poll_interval_secs = config.delivery_poll_interval_secs;

        let executor = Arc::new(ReverseSwapExecutor::new(
            api_client,
            key_manager,
            alchemy_client,
            evm_provider,
            chain_registry.clone(),
            config,
            store.clone(),
            cctp_fee_client,
            lz_scan_client,
            erc20swap_address,
            solana_rpc,
        ));

        let event_emitter = Arc::new(EventEmitter::new());
        let swap_locks = Arc::new(SwapLocks::new());

        let swap_manager = SwapManager::start(
            executor.clone(),
            store.clone(),
            event_emitter.clone(),
            ws_subscriber.clone(),
            swap_locks.clone(),
            ws_rx,
            delivery_poll_interval_secs,
        );

        Ok(Self {
            executor,
            store,
            swap_manager,
            event_emitter,
            ws_subscriber,
            chain_registry,
            swap_locks,
            key_index_lock: Mutex::new(()),
        })
    }

    /// Load and resume all active (non-terminal) swaps from storage, returning
    /// their ids. Call once after construction to pick up swaps from previous
    /// runs.
    ///
    /// When background polling is enabled this is an optional accelerator —
    /// the background loop periodically re-tracks any non-terminal swap on its
    /// own, so this just makes resumption immediate and returns the id list.
    /// When polling is disabled it is the only mechanism that resumes them.
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

    /// Advance every in-flight swap one step: confirm cross-chain delivery for
    /// swaps currently `Settling` and finalize any whose bridge has delivered
    /// (CCTP via Circle Iris, OFT via `LayerZero` Scan; CCTP also persists the
    /// authoritative `feeExecuted`-adjusted amount), and recover any stuck in
    /// `Claiming` (re-check the claim receipt; finalize a dropped claim once its
    /// lockup refunds). Completions/finalizations emit a
    /// [`BoltzSwapEvent::SwapUpdated`].
    ///
    /// This runs automatically on the background poll cadence
    /// ([`BoltzConfig::delivery_poll_interval_secs`]); call it directly only to
    /// drive it when background polling is disabled (`None`), or to force an
    /// immediate check.
    pub async fn refresh_pending_deliveries(&self) -> Result<(), BoltzError> {
        swap::manager::poll_pending_swaps(
            &self.executor,
            &self.store,
            &self.event_emitter,
            &self.swap_locks,
        )
        .await;
        Ok(())
    }

    /// Shut down the swap manager and close the WebSocket connection.
    pub async fn shutdown(&self) {
        self.swap_manager.shutdown().await;
        self.ws_subscriber.close().await;
    }

    /// Get a quote for converting sats to a stablecoin (USDT/USDT0/USDC,
    /// per the destination). Pure quote — no side effects, no swap created.
    ///
    /// `max_slippage_bps` overrides [`BoltzConfig::slippage_bps`] for this
    /// quote only. Must still fall within the `10..=MAX_SLIPPAGE_BPS` range.
    /// The resolved value is snapshotted onto the returned [`PreparedSwap`]
    /// and persisted with the swap so the claim-time DEX quote check
    /// honours the per-swap tolerance rather than the live config value.
    pub async fn prepare_reverse_swap(
        &self,
        destination: &str,
        chain: &str,
        asset: Asset,
        output_amount: u64,
        max_slippage_bps: Option<u32>,
    ) -> Result<PreparedSwap, BoltzError> {
        self.executor
            .prepare(destination, chain, asset, output_amount, max_slippage_bps)
            .await
    }

    /// Get a quote starting from input sats (computes expected stablecoin output).
    /// Pure quote — no side effects, no swap created.
    ///
    /// See [`prepare_reverse_swap`](Self::prepare_reverse_swap) for the
    /// `max_slippage_bps` override semantics.
    pub async fn prepare_reverse_swap_from_sats(
        &self,
        destination: &str,
        chain: &str,
        asset: Asset,
        invoice_amount_sats: u64,
        max_slippage_bps: Option<u32>,
    ) -> Result<PreparedSwap, BoltzError> {
        self.executor
            .prepare_from_sats(
                destination,
                chain,
                asset,
                invoice_amount_sats,
                max_slippage_bps,
            )
            .await
    }

    /// Create the swap on Boltz and begin background monitoring.
    /// Returns the hold invoice to pay.
    ///
    /// # Key index safety
    ///
    /// A duplicate HD index reuses a preimage (fund-theft risk), so issuance is
    /// serialized in-process here ([`issue_key_index`]). The caller's
    /// `BoltzStorage` must still guarantee `increment_key_index` is **durable**
    /// (persisted before returning) and **atomic across processes** if multiple
    /// instances share one store — neither is enforceable in-crate. This is the
    /// sole defense against preimage reuse: Boltz's duplicate-preimage detection
    /// (HTTP 409) must NOT be relied upon, as a malicious API could lie.
    pub async fn create_reverse_swap(
        &self,
        prepared: &PreparedSwap,
    ) -> Result<CreatedSwap, BoltzError> {
        let key_index = issue_key_index(&self.key_index_lock, &*self.store).await?;

        let swap = self.executor.create(prepared, key_index).await?;
        let created = CreatedSwap {
            swap_id: swap.id.clone(),
            invoice: swap.invoice.clone(),
            invoice_amount_sats: swap.invoice_amount_sats,
            timeout_block_height: swap.timeout_block_height,
        };
        self.store.upsert_swap(&swap).await?;
        self.swap_manager.track_swap(&created.swap_id).await;
        Ok(created)
    }

    /// Create a throwaway hold invoice for Lightning fee estimation.
    ///
    /// Returns the BOLT11 invoice string only. The invoice **must not be
    /// paid**: a fresh random preimage is used and discarded, so any
    /// payment to this invoice would lock funds with no way to claim.
    ///
    /// Useful when the caller needs an LN routing fee estimate against a
    /// real BOLT11 invoice without committing to a real swap — for
    /// example, when the final invoice's amount has to account for the
    /// routing fee, which itself can only be estimated against a real
    /// invoice. Pass the returned invoice to a fee-estimation API, then
    /// call [`create_reverse_swap`](Self::create_reverse_swap) for the
    /// real swap.
    ///
    /// Unlike [`create_reverse_swap`](Self::create_reverse_swap), this:
    /// - does not consume an HD key index (no `increment_key_index`),
    /// - does not write to local storage (no `upsert_swap`),
    /// - does not subscribe to swap WebSocket events,
    /// - sets a short `invoiceExpiry` so Boltz's server-side state
    ///   self-clears as quickly as the API allows.
    pub async fn create_probe_invoice(
        &self,
        prepared: &PreparedSwap,
    ) -> Result<String, BoltzError> {
        self.executor.create_probe_invoice(prepared).await
    }

    /// Every selectable destination across all bridges (OFT/USDT0, CCTP/USDC,
    /// and Arbitrum-direct USDT/USDC). Round-trip a destination's `(chain_label,
    /// asset)` back into `prepare_reverse_swap`.
    pub fn supported_destinations(&self) -> Vec<DestinationOption> {
        self.chain_registry
            .destinations
            .iter()
            .map(|dest| DestinationOption {
                chain_label: dest.chain_label.clone(),
                asset: dest.asset,
                transport: dest.transport,
                evm_chain_id: dest.evm_chain_id,
                dest_token_address: dest.dest_token_address.clone(),
                bridge_kind: dest.bridge.kind(),
            })
            .collect()
    }

    /// Destinations whose transport accepts `address` as a valid recipient.
    /// Drives UX flows that pick a destination from an address.
    pub fn destinations_accepting(&self, address: &str) -> Vec<DestinationOption> {
        self.supported_destinations()
            .into_iter()
            .filter(|d| is_valid_destination_address(d.transport, address))
            .collect()
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
        // Serialize against the manager loop's claim handler for this swap: the
        // guard is held across the read, the claim, and the persist, so this
        // forced claim cannot run concurrently with an in-flight `do_claim` and
        // submit a second, competing gas-sponsored claim tx (or clobber the
        // persisted `claim_tx_hash`).
        let _guard = self.swap_locks.lock(swap_id).await;

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
                self.store.upsert_swap(&swap).await?;
                Ok(swap)
            }
            Err(e) => {
                tracing::error!(swap_id, error = %e, "Forced claim after accept_degraded_quote failed, staying in Claiming for retry");
                Err(e)
            }
        }
    }

    /// Update the slippage tolerance for an existing swap.
    ///
    /// `slippage_bps` is purely client-side: it gates the claim-time DEX
    /// quote drift check and the on-chain `minOut` floor. Boltz never sees
    /// it, so adjusting it does not require any API interaction.
    ///
    /// Rejects swaps in terminal states (`Completed`/`Failed`/`Expired`),
    /// where no future claim attempt will read the value. For a swap in
    /// `Claiming` with a tx already broadcast, the on-chain `minOut` is
    /// already signed into that tx — the new value will only take effect
    /// on the next retry.
    ///
    /// `slippage_bps` is validated against the same `10..=MAX_SLIPPAGE_BPS`
    /// bounds enforced at `prepare` time.
    pub async fn update_swap_slippage(
        &self,
        swap_id: &str,
        slippage_bps: u32,
    ) -> Result<BoltzSwap, BoltzError> {
        let bps = resolve_slippage_bps(Some(slippage_bps), self.executor.config.slippage_bps)?;

        // Serialize the read-modify-write against the manager loop so this
        // whole-record update can't clobber a status the loop wrote in between.
        let _guard = self.swap_locks.lock(swap_id).await;

        let mut swap = self
            .store
            .get_swap(swap_id)
            .await?
            .ok_or_else(|| BoltzError::Store(format!("Swap not found: {swap_id}")))?;

        if swap.status.is_terminal() {
            return Err(BoltzError::Generic(format!(
                "Cannot update slippage: swap {} is in terminal state {:?}",
                swap_id, swap.status
            )));
        }

        swap.slippage_bps = bps;
        swap.updated_at = current_unix_timestamp();
        self.store.upsert_swap(&swap).await?;
        Ok(swap)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex as StdMutex;

    /// A deliberately non-transactional store: `increment_key_index` reads the
    /// counter, yields, then writes — so two concurrent calls collide unless the
    /// caller serializes them. Only `increment_key_index` is meaningful.
    #[derive(Default)]
    struct RacyStore {
        counter: StdMutex<u32>,
    }

    #[macros::async_trait]
    impl BoltzStorage for RacyStore {
        async fn upsert_swap(&self, _swap: &BoltzSwap) -> Result<(), BoltzError> {
            Ok(())
        }
        async fn get_swap(&self, _id: &str) -> Result<Option<BoltzSwap>, BoltzError> {
            Ok(None)
        }
        async fn list_active_swaps(&self) -> Result<Vec<BoltzSwap>, BoltzError> {
            Ok(vec![])
        }
        async fn increment_key_index(&self) -> Result<u32, BoltzError> {
            let current = *self.counter.lock().unwrap();
            platform_utils::tokio::task::yield_now().await;
            *self.counter.lock().unwrap() = current.saturating_add(1);
            Ok(current)
        }
    }

    #[macros::async_test_all]
    async fn issue_key_index_serializes_concurrent_calls() {
        use futures::future::join_all;
        const N: usize = 8;

        // Sanity: unguarded, the racy store really does hand out duplicates, so
        // the serialized assertion below is meaningful.
        let store = RacyStore::default();
        let racy: Vec<u32> = join_all((0..N).map(|_| store.increment_key_index()))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert!(
            racy.iter().collect::<HashSet<_>>().len() < N,
            "unguarded racy store should collide: {racy:?}"
        );

        // With in-process serialization every issued index is distinct, even
        // though the underlying store is non-atomic.
        let store = RacyStore::default();
        let lock = Mutex::new(());
        let issued: Vec<u32> = join_all((0..N).map(|_| issue_key_index(&lock, &store)))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            issued.iter().collect::<HashSet<_>>().len(),
            N,
            "serialized issuance must be unique: {issued:?}"
        );
    }
}
