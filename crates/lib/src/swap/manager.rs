use std::collections::HashSet;
use std::sync::Arc;

use platform_utils::tokio;
use tokio::sync::{Mutex, mpsc, watch};

use crate::api::ws::{SwapStatusSubscriber, SwapStatusUpdate};
use crate::error::BoltzError;
use crate::events::{BoltzSwapEvent, EventEmitter};
use crate::models::{BoltzSwap, BoltzSwapStatus};
use crate::recover;
use crate::store::BoltzStorage;
use crate::swap::reverse::{ReverseSwapExecutor, current_unix_timestamp};

/// Maximum number of receipt-poll attempts for a `Claiming` swap (5s * 60 = 5min).
/// If the receipt is still not found after this, the loop iteration exits and
/// relies on the WS `transaction.claimed` message. On process restart,
/// `resume_all` re-triggers the poll, so this is self-healing across restarts.
const RECEIPT_POLL_MAX_ATTEMPTS: u32 = 60;
/// Interval between receipt-poll attempts.
const RECEIPT_POLL_INTERVAL_SECS: u64 = 5;

/// Background swap manager.
///
/// Owns a single event loop that:
/// - Receives WebSocket status updates for all tracked swaps.
/// - Progresses each swap through its state machine.
/// - Runs claim/receipt-poll operations inline (blocking the loop).
///
/// NOTE: All reactions (claiming, receipt polling, on-chain checks) run inline
/// in the event loop. This keeps the code simple and race-free but means a slow
/// operation blocks processing of other swap updates. If this is ever used as a
/// backend relay serving many concurrent swaps, consider spawning these
/// operations into a `JoinSet` so they run in parallel while still being owned
/// by the loop for proper cancellation and error propagation.
pub(crate) struct SwapManager {
    store: Arc<dyn BoltzStorage>,
    /// Channel for sending swap IDs to track.
    cmd_tx: mpsc::Sender<String>,
    /// Shutdown signal — dropping the sender stops the event loop.
    shutdown_tx: watch::Sender<()>,
    task_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Sync-safe handle used by `Drop` to abort the task if `shutdown()` was
    /// never called.
    abort_handle: tokio::task::AbortHandle,
}

impl SwapManager {
    /// Create the manager and spawn its central event loop.
    ///
    /// `ws_rx` is the global receiver for all WebSocket status updates.
    pub fn start(
        executor: Arc<ReverseSwapExecutor>,
        store: Arc<dyn BoltzStorage>,
        event_emitter: Arc<EventEmitter>,
        ws_subscriber: Arc<SwapStatusSubscriber>,
        ws_rx: mpsc::Receiver<SwapStatusUpdate>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = watch::channel(());

        let handle = tokio::spawn(Self::run_loop(
            executor,
            store.clone(),
            event_emitter,
            ws_subscriber,
            ws_rx,
            cmd_rx,
            shutdown_rx,
        ));

        let abort_handle = handle.abort_handle();

        Self {
            store,
            cmd_tx,
            shutdown_tx,
            task_handle: Mutex::new(Some(handle)),
            abort_handle,
        }
    }

    /// Begin tracking a swap. The manager will subscribe to WS updates for it
    /// and progress it through the state machine.
    pub async fn track_swap(&self, swap_id: &str) {
        let _ = self.cmd_tx.send(swap_id.to_string()).await;
    }

    /// Resume all non-terminal swaps from the store.
    pub async fn resume_all(&self) -> Result<Vec<String>, BoltzError> {
        let active = self.store.list_active_swaps().await?;
        let mut ids = Vec::with_capacity(active.len());
        for swap in &active {
            tracing::info!(swap_id = swap.id, status = ?swap.status, "Resuming swap");
            self.track_swap(&swap.id).await;
            ids.push(swap.id.clone());
        }
        Ok(ids)
    }

    /// Signal the event loop to shut down and wait for it to exit.
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
        if let Some(handle) = self.task_handle.lock().await.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for SwapManager {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

impl SwapManager {
    // ─── Central event loop ─────────────────────────────────────────

    async fn run_loop(
        executor: Arc<ReverseSwapExecutor>,
        store: Arc<dyn BoltzStorage>,
        event_emitter: Arc<EventEmitter>,
        ws_subscriber: Arc<SwapStatusSubscriber>,
        mut ws_rx: mpsc::Receiver<SwapStatusUpdate>,
        mut cmd_rx: mpsc::Receiver<String>,
        mut shutdown_rx: watch::Receiver<()>,
    ) {
        // Swap IDs currently being tracked (for WS dispatch filtering).
        let mut tracked_ids: HashSet<String> = HashSet::new();

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                update = ws_rx.recv() => {
                    let Some(update) = update else { break };
                    if !tracked_ids.contains(&update.swap_id) {
                        tracing::warn!(boltz_id = update.swap_id, "WS update for untracked swap");
                        continue;
                    }
                    Self::handle_ws_update(
                        &executor,
                        &store,
                        &event_emitter,
                        &ws_subscriber,
                        &mut tracked_ids,
                        &update,
                    ).await;
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(swap_id) => {
                            if let Err(e) = Self::start_tracking(
                                &ws_subscriber,
                                &mut tracked_ids,
                                &swap_id,
                            ).await {
                                tracing::error!(swap_id, error = %e, "Failed to start tracking swap");
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        tracing::info!("SwapManager event loop exiting");
    }

    /// Begin tracking a specific swap: subscribe to WS and wait for the
    /// backend to send the current status. The WS update will drive any
    /// needed action via `handle_ws_update` — we don't act on local state
    /// here because another instance may have progressed the swap.
    async fn start_tracking(
        ws_subscriber: &Arc<SwapStatusSubscriber>,
        tracked_ids: &mut HashSet<String>,
        swap_id: &str,
    ) -> Result<(), BoltzError> {
        tracked_ids.insert(swap_id.to_string());
        ws_subscriber.subscribe(swap_id).await?;
        Ok(())
    }

    /// Process a WS status update for a tracked swap.
    #[expect(clippy::too_many_lines)]
    async fn handle_ws_update(
        executor: &Arc<ReverseSwapExecutor>,
        store: &Arc<dyn BoltzStorage>,
        event_emitter: &Arc<EventEmitter>,
        ws_subscriber: &Arc<SwapStatusSubscriber>,
        tracked_ids: &mut HashSet<String>,
        update: &SwapStatusUpdate,
    ) {
        let swap_id = &update.swap_id;
        let mut swap = match store.get_swap(swap_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                tracing::warn!(swap_id, "WS update for unknown swap");
                return;
            }
            Err(e) => {
                tracing::error!(swap_id, error = %e, "Failed to load swap for WS update");
                return;
            }
        };

        if swap.status.is_terminal() {
            tracing::debug!(swap_id, status = ?swap.status, "Swap already terminal, cleaning up");
            Self::cleanup_terminal(ws_subscriber, tracked_ids, swap_id).await;
            return;
        }

        tracing::info!(
            swap_id,
            local_status = ?swap.status,
            ws_status = update.status,
            "Processing WS update"
        );

        match update.status.as_str() {
            "swap.created" | "invoice.set" | "invoice.pending" => {}
            "invoice.paid" => {
                update_swap_status(
                    &**store,
                    event_emitter,
                    &mut swap,
                    BoltzSwapStatus::InvoicePaid,
                )
                .await;
            }
            "transaction.mempool" => {
                if let Some(tx) = &update.transaction {
                    let mut s = swap;
                    s.lockup_tx_id = Some(tx.id.clone());
                    s.updated_at = current_unix_timestamp();
                    if let Err(e) = store.update_swap(&s).await {
                        tracing::error!(swap_id, error = %e, "Failed to persist lockup_tx_id");
                    }
                    event_emitter
                        .emit(&BoltzSwapEvent::SwapUpdated { swap: s })
                        .await;
                }
            }
            "transaction.confirmed" => {
                if matches!(swap.status, BoltzSwapStatus::Claiming) {
                    Self::handle_claiming_resume(executor, store, event_emitter, &swap).await;
                } else {
                    // tBTC locked on-chain. Update local status, then claim.
                    let mut s = swap.clone();
                    if let Some(tx) = &update.transaction {
                        s.lockup_tx_id = Some(tx.id.clone());
                    }
                    s.status = BoltzSwapStatus::TbtcLocked;
                    s.updated_at = current_unix_timestamp();
                    if let Err(e) = store.update_swap(&s).await {
                        tracing::error!(swap_id, error = %e, "Failed to persist TbtcLocked status");
                    }
                    event_emitter
                        .emit(&BoltzSwapEvent::SwapUpdated { swap: s.clone() })
                        .await;
                    Self::do_claim(executor, store, event_emitter, &mut s, false).await;
                }
            }
            // `invoice.settled`: reverse swap success (Boltz settled the hold
            //   invoice after detecting our on-chain claim).
            // `transaction.claimed`: submarine/chain swap success (included
            //   for completeness, not expected for reverse swaps).
            //
            // If we have a claim tx hash, verify the receipt on-chain before
            // marking Completed. Without a tx hash we can't meaningfully verify
            // (the on-chain lock check can't distinguish our claim from a Boltz
            // refund), so trust the WS event and log a warning.
            //
            // TODO: Parse Transfer event logs from the receipt to record the
            // actual USDT amount delivered (may differ from estimate due to
            // slippage).
            "invoice.settled" | "transaction.claimed" => {
                if let Some(ref tx_hash) = swap.claim_tx_hash {
                    let reached_terminal =
                        Self::poll_receipt(executor, store, event_emitter, swap_id, tx_hash).await;
                    if reached_terminal {
                        Self::cleanup_terminal(ws_subscriber, tracked_ids, swap_id).await;
                    }
                } else {
                    tracing::warn!(
                        swap_id,
                        "No claim tx hash — cannot verify on-chain, trusting WS event"
                    );
                    update_swap_status(
                        &**store,
                        event_emitter,
                        &mut swap,
                        BoltzSwapStatus::Completed,
                    )
                    .await;
                    Self::cleanup_terminal(ws_subscriber, tracked_ids, swap_id).await;
                }
            }
            "invoice.expired" | "swap.expired" => {
                update_swap_status(&**store, event_emitter, &mut swap, BoltzSwapStatus::Expired)
                    .await;
                Self::cleanup_terminal(ws_subscriber, tracked_ids, swap_id).await;
            }
            "invoice.failedToPay"
            | "transaction.lockupFailed"
            | "transaction.refunded"
            | "swap.refunded" => {
                let reason = update
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| update.status.clone());
                update_swap_status(
                    &**store,
                    event_emitter,
                    &mut swap,
                    BoltzSwapStatus::Failed { reason },
                )
                .await;
                Self::cleanup_terminal(ws_subscriber, tracked_ids, swap_id).await;
            }
            _ => {
                tracing::debug!(
                    swap_id,
                    ws_status = update.status,
                    "Unknown WS status, ignoring"
                );
            }
        }
    }

    /// Execute the claim flow for a swap, handling all outcomes inline.
    async fn do_claim(
        executor: &ReverseSwapExecutor,
        store: &Arc<dyn BoltzStorage>,
        event_emitter: &EventEmitter,
        swap: &mut BoltzSwap,
        skip_drift_check: bool,
    ) {
        let swap_id = swap.id.clone();

        update_swap_status(&**store, event_emitter, swap, BoltzSwapStatus::Claiming).await;

        match executor.claim_and_swap(swap, skip_drift_check).await {
            Ok(tx_hash) => {
                swap.claim_tx_hash = Some(tx_hash);
                swap.updated_at = current_unix_timestamp();
                if let Err(e) = store.update_swap(swap).await {
                    tracing::error!(swap_id, error = %e, "Failed to persist claim tx hash");
                }
            }
            Err(BoltzError::QuoteDegradedBeyondSlippage {
                expected_usdt,
                quoted_usdt,
            }) => {
                tracing::warn!(
                    swap_id,
                    expected_usdt,
                    quoted_usdt,
                    "Claim-time quote degraded beyond slippage tolerance"
                );
                event_emitter
                    .emit(&BoltzSwapEvent::QuoteDegraded {
                        swap: swap.clone(),
                        expected_usdt,
                        quoted_usdt,
                    })
                    .await;
            }
            Err(e) => {
                tracing::error!(swap_id, error = %e, "Claim failed, staying in Claiming for retry");
            }
        }
    }

    /// Handle resuming a swap stuck in `Claiming` status. Either the tx hash
    /// is known (poll chain for receipt) or unknown (check on-chain if preimage
    /// was revealed).
    async fn handle_claiming_resume(
        executor: &ReverseSwapExecutor,
        store: &Arc<dyn BoltzStorage>,
        event_emitter: &EventEmitter,
        swap: &BoltzSwap,
    ) {
        if let Some(ref tx_hash) = swap.claim_tx_hash {
            let _ = Self::poll_receipt(executor, store, event_emitter, &swap.id, tx_hash).await;
        } else {
            // Crash during Alchemy call: we set Claiming but never got a tx
            // hash back. Check on-chain if the claim went through anyway.
            Self::check_on_chain_and_retry(executor, store, event_emitter, swap).await;
        }
    }

    /// Poll `eth_get_transaction_receipt` for a known tx hash. If the receipt
    /// shows success, mark `Completed`. If reverted, mark `Failed`.
    /// Returns `true` if a terminal state was reached.
    async fn poll_receipt(
        executor: &ReverseSwapExecutor,
        store: &Arc<dyn BoltzStorage>,
        event_emitter: &EventEmitter,
        swap_id: &str,
        tx_hash: &str,
    ) -> bool {
        for attempt in 0..RECEIPT_POLL_MAX_ATTEMPTS {
            match executor
                .evm_provider
                .eth_get_transaction_receipt(tx_hash)
                .await
            {
                Ok(Some(receipt)) => {
                    if receipt.is_success() {
                        tracing::info!(swap_id, tx_hash, "Claim receipt confirmed");
                        if let Ok(Some(mut swap)) = store.get_swap(swap_id).await {
                            update_swap_status(
                                &**store,
                                event_emitter,
                                &mut swap,
                                BoltzSwapStatus::Completed,
                            )
                            .await;
                        }
                    } else {
                        tracing::error!(swap_id, tx_hash, "Claim tx reverted");
                        if let Ok(Some(mut swap)) = store.get_swap(swap_id).await {
                            update_swap_status(
                                &**store,
                                event_emitter,
                                &mut swap,
                                BoltzSwapStatus::Failed {
                                    reason: "Claim transaction reverted".to_string(),
                                },
                            )
                            .await;
                        }
                    }
                    return true;
                }
                Ok(None) => {
                    // Not mined yet.
                    if attempt < RECEIPT_POLL_MAX_ATTEMPTS.saturating_sub(1) {
                        platform_utils::tokio::time::sleep(
                            platform_utils::time::Duration::from_secs(RECEIPT_POLL_INTERVAL_SECS),
                        )
                        .await;
                    }
                }
                Err(e) => {
                    tracing::warn!(swap_id, attempt, error = %e, "Receipt poll failed");
                    platform_utils::tokio::time::sleep(platform_utils::time::Duration::from_secs(
                        RECEIPT_POLL_INTERVAL_SECS,
                    ))
                    .await;
                }
            }
        }

        // Timed out — rely on WS `transaction.claimed` to complete.
        // On process restart, `resume_all` re-triggers the poll.
        tracing::warn!(swap_id, tx_hash, "Receipt poll timed out, waiting for WS");
        false
    }

    /// Check on-chain whether the preimage was already revealed. If still
    /// locked, retry the claim. If already claimed, wait for WS
    /// `transaction.claimed`.
    async fn check_on_chain_and_retry(
        executor: &ReverseSwapExecutor,
        store: &Arc<dyn BoltzStorage>,
        event_emitter: &EventEmitter,
        swap: &BoltzSwap,
    ) {
        let swap_id = &swap.id;

        match recover::is_swap_still_locked_by_swap(
            &executor.evm_provider,
            swap,
            &executor.key_manager,
        )
        .await
        {
            Ok(true) => {
                // Still locked — safe to retry claim.
                tracing::info!(swap_id, "Swap still locked on-chain, retrying claim");
                let mut s = swap.clone();
                s.status = BoltzSwapStatus::TbtcLocked;
                s.updated_at = current_unix_timestamp();
                if let Err(e) = store.update_swap(&s).await {
                    tracing::error!(swap_id, error = %e, "Failed to persist TbtcLocked reset");
                }
                Self::do_claim(executor, store, event_emitter, &mut s, false).await;
            }
            Ok(false) => {
                // Already claimed — just wait for WS `transaction.claimed`.
                tracing::info!(
                    swap_id,
                    "Swap already claimed on-chain, waiting for WS confirmation"
                );
            }
            Err(e) => {
                tracing::error!(swap_id, error = %e, "On-chain check failed");
            }
        }
    }

    /// Unsubscribe from WS and remove from tracking set after a swap
    /// reaches a terminal state.
    async fn cleanup_terminal(
        ws_subscriber: &SwapStatusSubscriber,
        tracked_ids: &mut HashSet<String>,
        swap_id: &str,
    ) {
        ws_subscriber.unsubscribe(swap_id).await;
        tracked_ids.remove(swap_id);
    }
}

pub(crate) async fn update_swap_status(
    store: &dyn BoltzStorage,
    emitter: &EventEmitter,
    swap: &mut BoltzSwap,
    new_status: BoltzSwapStatus,
) {
    swap.status = new_status;
    swap.updated_at = current_unix_timestamp();
    if let Err(e) = store.update_swap(swap).await {
        tracing::error!(swap_id = swap.id, error = %e, "Failed to update swap status");
    }
    emitter
        .emit(&BoltzSwapEvent::SwapUpdated { swap: swap.clone() })
        .await;
}
