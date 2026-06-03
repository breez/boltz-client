use std::collections::HashSet;
use std::sync::Arc;

use platform_utils::tokio;
use tokio::sync::{Mutex, mpsc, watch};

use crate::api::ws::{SwapStatusSubscriber, SwapStatusUpdate};
use crate::config::CCTP_MESSAGE_TRANSMITTER_V2;
use crate::error::BoltzError;
use crate::events::{BoltzSwapEvent, EventEmitter};
use crate::evm::contracts::{
    DeliveredAmount, DeliveredAmountSource, decode_delivered_from_logs, parse_address,
};
use crate::evm::lockup::is_swap_still_locked_by_swap;
use crate::evm::provider::TxReceipt;
use crate::models::{BoltzSwap, BoltzSwapStatus, Bridge, BridgeKind};
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
        delivery_poll_interval_secs: Option<u64>,
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
            delivery_poll_interval_secs,
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

    #[expect(clippy::too_many_arguments)]
    async fn run_loop(
        executor: Arc<ReverseSwapExecutor>,
        store: Arc<dyn BoltzStorage>,
        event_emitter: Arc<EventEmitter>,
        ws_subscriber: Arc<SwapStatusSubscriber>,
        mut ws_rx: mpsc::Receiver<SwapStatusUpdate>,
        mut cmd_rx: mpsc::Receiver<String>,
        mut shutdown_rx: watch::Receiver<()>,
        delivery_poll_interval_secs: Option<u64>,
    ) {
        // Swap IDs currently being tracked (for WS dispatch filtering).
        let mut tracked_ids: HashSet<String> = HashSet::new();

        // Background delivery-confirmation ticker. `None` disables it (callers
        // drive confirmation via `refresh_pending_deliveries`). The first tick
        // fires immediately, re-arming any swaps resumed as `Settling`. Missed
        // ticks (if a branch handler ran long) just coalesce into idempotent
        // catch-up polls, so the default missed-tick behavior is fine — and
        // `set_missed_tick_behavior` isn't available on the WASM tokio shim.
        let mut delivery_ticker = delivery_poll_interval_secs.map(|secs| {
            tokio::time::interval(platform_utils::time::Duration::from_secs(secs.max(1)))
        });

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                () = async {
                    match delivery_ticker.as_mut() {
                        Some(t) => { t.tick().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    poll_settling_swaps(&executor, &store, &event_emitter).await;
                }
                should_break = async {
                    let Some(update) = ws_rx.recv().await else { return true };
                    if !tracked_ids.contains(&update.swap_id) {
                        tracing::warn!(boltz_id = update.swap_id, "WS update for untracked swap");
                        return false;
                    }
                    Self::handle_ws_update(
                        &executor,
                        &store,
                        &event_emitter,
                        &ws_subscriber,
                        &mut tracked_ids,
                        &update,
                    ).await;
                    false
                } => {
                    if should_break { break; }
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

        // Settling = past the claim: delivery is confirmed by the background
        // poll, not the WS. This MUST run before the status match below — else a
        // late `swap.expired` / `invoice.expired` (re-delivered after
        // `resume_all` re-subscribes) would wrongly drive an already-claimed
        // swap to `Expired`. A Settling swap only leaves Settling via confirmed
        // delivery, by design.
        if swap.status == BoltzSwapStatus::Settling {
            tracing::debug!(
                swap_id,
                "Swap settling; delivery confirmation is poll-driven"
            );
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
            // `invoice.settled`: reverse swap success. Boltz can only settle the
            //   hold invoice once it holds the preimage, and the preimage only
            //   reaches the chain inside our atomic claim tx (claim + DEX +
            //   bridge-send, all-or-nothing). So a *genuine* settled event is
            //   itself proof the source-side claim + send succeeded.
            // `transaction.claimed`: submarine/chain swap success (included
            //   for completeness, not expected for reverse swaps).
            //
            // We don't act on the WS event alone — a spoofed/buggy one mustn't
            // finalize a swap. With a tx hash we verify the receipt; with a
            // call_id we recover then verify it; with neither we fall back to the
            // ERC20Swap lock state as a claim-retry guard (see below).
            "invoice.settled" | "transaction.claimed" => {
                if let Some(ref tx_hash) = swap.claim_tx_hash {
                    let reached_terminal =
                        Self::poll_receipt(executor, store, event_emitter, swap_id, tx_hash).await;
                    if reached_terminal {
                        Self::cleanup_terminal(ws_subscriber, tracked_ids, swap_id).await;
                    }
                } else if let Some(call_id) = swap.pending_call_id.clone() {
                    // No tx hash recorded, but we have the gas-sponsor call_id:
                    // recover the tx hash and verify the receipt on-chain
                    // rather than trusting the WS event blindly.
                    let reached_terminal =
                        Self::resume_pending_call(executor, store, event_emitter, &swap, &call_id)
                            .await;
                    if reached_terminal {
                        Self::cleanup_terminal(ws_subscriber, tracked_ids, swap_id).await;
                    }
                } else {
                    // No tx hash and no call_id, so we can't fetch the receipt.
                    // The lock state here is a CLAIM-RETRY GUARD, not a delivery
                    // (or claim-vs-refund) check: it only tells us whether the
                    // swap is still claimable, so a spoofed/premature settled
                    // event can't make us abandon recoverable tBTC. Delivery is
                    // gated separately (`post_claim_status` -> `Settling` poll).
                    match is_swap_still_locked_by_swap(
                        &executor.evm_provider,
                        &swap,
                        &executor.key_manager,
                    )
                    .await
                    {
                        Ok(true) => {
                            // Still claimable: the claim hasn't happened (the
                            // settled event was premature or spoofed). Retry it
                            // rather than finalizing on a WS message alone — else
                            // the tBTC would sit until it refunds back to Boltz.
                            tracing::warn!(
                                swap_id,
                                "WS reports settled but swap still locked on-chain; retrying claim"
                            );
                            Self::check_on_chain_and_retry(executor, store, event_emitter, &swap)
                                .await;
                        }
                        Ok(false) => {
                            // Spent. Combined with the settled event — which
                            // requires the preimage from our atomic claim — this
                            // means the source-side claim + send succeeded (a
                            // refund couldn't have produced a preimage to settle).
                            // Gate delivery: `post_claim_status` completes `Direct`
                            // (atomically delivered in that same tx) and holds
                            // `Oft`/`Cctp` in `Settling` for the delivery poll.
                            let next = post_claim_status(&swap);
                            update_swap_status(&**store, event_emitter, &mut swap, next).await;
                            Self::cleanup_terminal(ws_subscriber, tracked_ids, swap_id).await;
                        }
                        Err(e) => {
                            // Couldn't read the lock state — leave the swap
                            // tracked; the next WS update or `resume_all` retries.
                            tracing::warn!(
                                swap_id,
                                error = %e,
                                "On-chain lock check failed; leaving swap for retry"
                            );
                        }
                    }
                }
            }
            "invoice.expired" | "swap.expired" => {
                Self::handle_terminal_ws_event(
                    executor,
                    store,
                    event_emitter,
                    ws_subscriber,
                    tracked_ids,
                    &mut swap,
                    BoltzSwapStatus::Expired,
                )
                .await;
            }
            "invoice.failedToPay"
            | "transaction.lockupFailed"
            | "transaction.refunded"
            | "swap.refunded" => {
                let reason = update
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| update.status.clone());
                Self::handle_terminal_ws_event(
                    executor,
                    store,
                    event_emitter,
                    ws_subscriber,
                    tracked_ids,
                    &mut swap,
                    BoltzSwapStatus::Failed { reason },
                )
                .await;
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
                expected_usd,
                quoted_usd,
            }) => {
                tracing::warn!(
                    swap_id,
                    expected_usd,
                    quoted_usd,
                    "Claim-time quote degraded beyond slippage tolerance"
                );
                // The claim was NOT attempted on-chain — the drift check
                // short-circuits before any signing/submit, so the tBTC is
                // still locked and the preimage was never revealed. Revert the
                // status set above back to `TbtcLocked` so the persisted (and
                // emitted) state matches the documented degraded-quote contract:
                // the consumer accepts the new rate via `accept_degraded_quote`
                // (which also tolerates `Claiming`) and the next claim retries.
                swap.status = BoltzSwapStatus::TbtcLocked;
                swap.updated_at = current_unix_timestamp();
                if let Err(e) = store.update_swap(swap).await {
                    tracing::error!(swap_id, error = %e, "Failed to persist TbtcLocked after degraded quote");
                }
                event_emitter
                    .emit(&BoltzSwapEvent::QuoteDegraded {
                        swap: swap.clone(),
                        expected_usd,
                        quoted_usd,
                    })
                    .await;
            }
            Err(e) => {
                tracing::error!(swap_id, error = %e, "Claim failed, staying in Claiming for retry");
            }
        }
    }

    /// Handle a terminal Boltz WS event (`*.expired` / refund / `failedToPay`)
    /// for a tracked swap.
    ///
    /// The naive action — mark the swap `Expired`/`Failed` and stop — is unsafe
    /// when the swap is mid-claim: once `do_claim` records progress the swap
    /// sits in `Claiming` until the receipt poll promotes it, and in that window
    /// the atomic claim may have ALREADY revealed the preimage and committed the
    /// bridge-send on-chain. Finalizing such a swap on a WS event alone would
    /// strand an already-successful bridged swap (delivery never confirmed,
    /// `delivered_amount` never recorded, dropped from tracking).
    ///
    /// So when `Claiming`, re-check the on-chain lock first: finalize only if the
    /// tBTC is provably still locked (the claim never happened); if it is spent,
    /// advance through the post-claim path instead. A failed lock read leaves the
    /// swap for a later retry rather than finalizing on the event alone. For all
    /// pre-claim states (`Created`/`InvoicePaid`/`TbtcLocked`) the event is
    /// authoritative and finalizes directly.
    async fn handle_terminal_ws_event(
        executor: &ReverseSwapExecutor,
        store: &Arc<dyn BoltzStorage>,
        event_emitter: &EventEmitter,
        ws_subscriber: &SwapStatusSubscriber,
        tracked_ids: &mut HashSet<String>,
        swap: &mut BoltzSwap,
        terminal_status: BoltzSwapStatus,
    ) {
        let swap_id = swap.id.clone();

        if swap.status == BoltzSwapStatus::Claiming {
            match is_swap_still_locked_by_swap(&executor.evm_provider, swap, &executor.key_manager)
                .await
            {
                Ok(true) => {
                    // Claim never happened — the tBTC is still locked and will
                    // refund to Boltz. The terminal event is legitimate.
                }
                Ok(false) => {
                    // Already claimed on-chain. Do NOT finalize on the WS event;
                    // advance through the post-claim path so a successful bridged
                    // swap completes/settles instead of being wrongly failed.
                    tracing::warn!(
                        swap_id,
                        ws_terminal = ?terminal_status,
                        "Terminal WS event for an already-claimed swap; advancing post-claim instead of finalizing"
                    );
                    let resolved =
                        Self::advance_claimed_swap(executor, store, event_emitter, swap).await;
                    if resolved {
                        Self::cleanup_terminal(ws_subscriber, tracked_ids, &swap_id).await;
                    }
                    return;
                }
                Err(e) => {
                    // Couldn't verify the lock — don't finalize blindly. Leave
                    // the swap tracked; the next WS update or `resume_all` retries.
                    tracing::warn!(
                        swap_id,
                        error = %e,
                        "Could not verify lock state on terminal WS event; leaving swap for retry"
                    );
                    return;
                }
            }
        }

        update_swap_status(&**store, event_emitter, swap, terminal_status).await;
        Self::cleanup_terminal(ws_subscriber, tracked_ids, &swap_id).await;
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
        } else if let Some(ref call_id) = swap.pending_call_id {
            // Crash between gas-sponsor submission and confirmation: the claim
            // was handed to the sponsor (and likely mined) but we never
            // recorded a tx hash. Recover it directly from the call_id.
            Self::resume_pending_call(executor, store, event_emitter, swap, call_id).await;
        } else {
            // Crash during Alchemy call: we set Claiming but never got a tx
            // hash back. Check on-chain if the claim went through anyway.
            Self::check_on_chain_and_retry(executor, store, event_emitter, swap).await;
        }
    }

    /// Recover a mid-claim swap from its persisted gas-sponsor `call_id`:
    /// re-poll for the tx hash, persist it (clearing `pending_call_id`), then
    /// poll the receipt to reach a terminal state. Falls back to the on-chain
    /// rescan if the `call_id` can't be resolved (e.g. the sponsor no longer
    /// knows it). Returns `true` if a terminal state was reached.
    async fn resume_pending_call(
        executor: &ReverseSwapExecutor,
        store: &Arc<dyn BoltzStorage>,
        event_emitter: &EventEmitter,
        swap: &BoltzSwap,
        call_id: &str,
    ) -> bool {
        let swap_id = &swap.id;
        match executor.poll_pending_call(call_id).await {
            Ok(tx_hash) => {
                tracing::info!(
                    swap_id,
                    tx_hash,
                    "Recovered claim tx hash from pending call_id"
                );
                if let Ok(Some(mut s)) = store.get_swap(swap_id).await {
                    s.claim_tx_hash = Some(tx_hash.clone());
                    s.pending_call_id = None;
                    s.updated_at = current_unix_timestamp();
                    if let Err(e) = store.update_swap(&s).await {
                        tracing::error!(swap_id, error = %e, "Failed to persist recovered tx hash");
                    }
                }
                Self::poll_receipt(executor, store, event_emitter, swap_id, &tx_hash).await
            }
            Err(e) => {
                tracing::warn!(
                    swap_id,
                    error = %e,
                    "Could not recover tx hash from pending call_id, falling back to on-chain check"
                );
                Self::check_on_chain_and_retry(executor, store, event_emitter, swap).await;
                false
            }
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
                Ok(Some(receipt)) if receipt.is_success() => {
                    tracing::info!(swap_id, tx_hash, "Claim receipt confirmed");
                    if let Ok(Some(mut swap)) = store.get_swap(swap_id).await {
                        apply_delivered_amount(executor, &mut swap, &receipt, tx_hash);
                        let next = post_claim_status(&swap);
                        update_swap_status(&**store, event_emitter, &mut swap, next).await;
                    }
                    return true;
                }
                Ok(Some(receipt)) if receipt.is_reverted() => {
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
                    return true;
                }
                // Mined but status neither success nor revert (absent/unknown):
                // never infer a terminal state from an ambiguous receipt — keep
                // polling, same as not-yet-mined.
                Ok(_) => {
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

        match is_swap_still_locked_by_swap(&executor.evm_provider, swap, &executor.key_manager)
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
                // Already claimed on-chain: the preimage was revealed and the
                // atomic claim + DEX + bridge-send committed. We must NOT leave
                // the swap stranded in `Claiming` — advance it through the
                // post-claim path (recovering the receipt-derived delivered
                // amount when possible) so `Direct` completes and `Oft`/`Cctp`
                // enter `Settling` for the delivery poll. Mirrors the inline
                // `invoice.settled` already-claimed branch.
                tracing::info!(
                    swap_id,
                    "Swap already claimed on-chain; advancing through post-claim status"
                );
                Self::advance_claimed_swap(executor, store, event_emitter, swap).await;
            }
            Err(e) => {
                tracing::error!(swap_id, error = %e, "On-chain check failed");
            }
        }
    }

    /// Advance a swap whose lockup is provably spent (the claim already happened
    /// on-chain) to its post-claim status. The caller must have confirmed the
    /// lock is spent — this routine does NOT re-check it.
    ///
    /// Resolution order, best to worst:
    /// 1. Known `claim_tx_hash` → poll the receipt (records `delivered_amount`
    ///    and `bridge_ref`, then sets the post-claim status).
    /// 2. Persisted gas-sponsor `call_id` → recover the tx hash, persist it, and
    ///    poll the receipt.
    /// 3. Neither recoverable → set the post-claim status directly so the swap
    ///    is not stranded in `Claiming`. `Direct` completes; `Oft`/`Cctp` enter
    ///    `Settling` (a missing `bridge_ref` then holds in `Settling`, the safe
    ///    failure mode — never a falsely `Completed` swap).
    ///
    /// Returns `true` if the swap reached a resolved state (caller may clean up
    /// WS tracking), `false` if a receipt poll timed out and it is still
    /// `Claiming` (keep tracking; the next event/resume retries).
    async fn advance_claimed_swap(
        executor: &ReverseSwapExecutor,
        store: &Arc<dyn BoltzStorage>,
        event_emitter: &EventEmitter,
        swap: &BoltzSwap,
    ) -> bool {
        let swap_id = &swap.id;

        if let Some(ref tx_hash) = swap.claim_tx_hash {
            return Self::poll_receipt(executor, store, event_emitter, swap_id, tx_hash).await;
        }

        if let Some(ref call_id) = swap.pending_call_id {
            match executor.poll_pending_call(call_id).await {
                Ok(tx_hash) => {
                    tracing::info!(
                        swap_id,
                        tx_hash,
                        "Recovered claim tx hash from pending call_id"
                    );
                    if let Ok(Some(mut s)) = store.get_swap(swap_id).await {
                        s.claim_tx_hash = Some(tx_hash.clone());
                        s.pending_call_id = None;
                        s.updated_at = current_unix_timestamp();
                        if let Err(e) = store.update_swap(&s).await {
                            tracing::error!(swap_id, error = %e, "Failed to persist recovered tx hash");
                        }
                    }
                    return Self::poll_receipt(executor, store, event_emitter, swap_id, &tx_hash)
                        .await;
                }
                Err(e) => {
                    tracing::warn!(
                        swap_id,
                        error = %e,
                        "Could not recover tx hash for already-claimed swap; advancing on post-claim status without delivered amount"
                    );
                    // Fall through to step 3.
                }
            }
        }

        // No receipt recoverable. Advance anyway so the swap isn't stranded in
        // `Claiming`; re-read first to avoid clobbering concurrent updates.
        if let Ok(Some(mut s)) = store.get_swap(swap_id).await {
            if s.status != BoltzSwapStatus::Claiming {
                // Another path already advanced it.
                return s.status.is_terminal() || s.status == BoltzSwapStatus::Settling;
            }
            let next = post_claim_status(&s);
            update_swap_status(&**store, event_emitter, &mut s, next).await;
            return true;
        }
        false
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

/// Decode the claim receipt's logs and write the bridge tracking handle (and,
/// where it's already authoritative, the delivered amount) onto `swap` in
/// memory. The caller persists `swap` afterwards.
///
/// Per bridge:
/// - **OFT/Direct**: the decoded amount (`amountReceivedLD` / the ERC20
///   transfer) is final, so `delivered_amount` is set now. For OFT, `bridge_ref`
///   is the `LayerZero` GUID; Direct has no bridge.
/// - **CCTP**: the on-chain figure is the *burn* amount (source `feeExecuted` is
///   0), which overstates delivery by the fast-transfer fee, so `delivered_amount`
///   is left unset until Circle attests the real `feeExecuted`. `bridge_ref` is
///   synthesized as `"<source_domain>:<burn_tx_hash>"` (the tx hash isn't
///   available to the log decoder) — the key Circle Iris indexes by.
fn apply_delivered_amount(
    executor: &ReverseSwapExecutor,
    swap: &mut BoltzSwap,
    receipt: &TxReceipt,
    tx_hash: &str,
) {
    let Some(source) = delivered_source_for(executor, swap) else {
        tracing::warn!(
            swap_id = swap.id,
            tx_hash,
            dest = %swap.destination_chain,
            "No delivered-amount source resolvable for destination"
        );
        return;
    };

    match decode_delivered_from_logs(&receipt.logs, &source) {
        Some(decoded) => {
            let (delivered, bridge_ref) = delivered_and_ref(decoded, tx_hash);
            swap.delivered_amount = delivered;
            swap.bridge_ref = bridge_ref;
        }
        None => {
            tracing::warn!(
                swap_id = swap.id,
                tx_hash,
                dest = %swap.destination_chain,
                "No matching log in claim receipt; delivered_amount left unset"
            );
        }
    }
}

/// Map a decoded claim receipt to `(delivered_amount, bridge_ref)`.
///
/// CCTP defers the amount — the source `feeExecuted` is 0, so the decoded burn
/// amount overstates delivery by the fast-transfer fee — and keys Circle Iris
/// by `"<source_domain>:<burn_tx_hash>"`. OFT/Direct amounts are final at claim
/// (`amountReceivedLD` / the ERC20 transfer); OFT carries the `LayerZero` GUID
/// as its `bridge_ref`, Direct has none.
fn delivered_and_ref(decoded: DeliveredAmount, tx_hash: &str) -> (Option<u64>, Option<String>) {
    match decoded.cctp_source_domain {
        Some(domain) => (None, Some(format!("{domain}:{tx_hash}"))),
        None => (Some(decoded.amount), decoded.lz_guid),
    }
}

/// Choose the post-claim status for a swap whose claim receipt just confirmed
/// successfully. `Direct` delivery is complete on Arbitrum; `Oft`/`Cctp` enter
/// `Settling` so the background poll can confirm cross-chain delivery before
/// `Completed`.
///
/// A bridged swap is *never* marked `Completed` here: completion only happens
/// once delivery is confirmed (`confirm_delivery`). If a `bridge_ref` couldn't
/// be recovered from the receipt (anomalous — a successful bridged claim should
/// emit one), the swap still holds in `Settling` rather than falsely completing;
/// `confirm_delivery` will log that it can't track it. This is the safe failure
/// mode: a stuck-but-honest `Settling` beats a "Completed" we never verified.
fn post_claim_status(swap: &BoltzSwap) -> BoltzSwapStatus {
    match swap.bridge_kind {
        BridgeKind::Direct => BoltzSwapStatus::Completed,
        BridgeKind::Oft | BridgeKind::Cctp => {
            if swap.bridge_ref.is_none() {
                tracing::warn!(
                    swap_id = swap.id,
                    "Bridged swap missing bridge_ref after claim; holding in Settling (delivery unconfirmable)"
                );
            }
            BoltzSwapStatus::Settling
        }
    }
}

/// Poll every `Settling` swap once and finalize any whose cross-chain delivery
/// has confirmed. Store-driven (not tied to WS tracking), so it covers swaps
/// resumed after a restart. Used by both the background tick and the on-demand
/// [`crate::BoltzService::refresh_pending_deliveries`].
pub(crate) async fn poll_settling_swaps(
    executor: &ReverseSwapExecutor,
    store: &Arc<dyn BoltzStorage>,
    event_emitter: &EventEmitter,
) {
    let swaps = match store.list_active_swaps().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to list active swaps for delivery poll");
            return;
        }
    };
    for swap in swaps
        .into_iter()
        .filter(|s| s.status == BoltzSwapStatus::Settling)
    {
        confirm_delivery(executor, store, event_emitter, &swap).await;
    }
}

/// Confirm cross-chain delivery for a single `Settling` swap and finalize it if
/// delivered. CCTP queries Circle Iris (and persists the authoritative
/// `feeExecuted`-adjusted amount); OFT queries `LayerZero` Scan (amount already
/// recorded at claim). Leaves the swap `Settling` on any not-yet-delivered or
/// transient-error outcome — the next poll retries.
async fn confirm_delivery(
    executor: &ReverseSwapExecutor,
    store: &Arc<dyn BoltzStorage>,
    event_emitter: &EventEmitter,
    swap: &BoltzSwap,
) {
    let Some(bridge_ref) = swap.bridge_ref.clone() else {
        tracing::warn!(
            swap_id = swap.id,
            "Settling swap has no bridge_ref; cannot confirm delivery"
        );
        return;
    };

    match swap.bridge_kind {
        BridgeKind::Cctp => match executor.cctp_delivery_status(&bridge_ref).await {
            Ok(status) => {
                if let Some(delivered) = cctp_completion_amount(&status) {
                    finalize_completed(store, event_emitter, &swap.id, Some(delivered)).await;
                }
            }
            Err(e) => {
                tracing::warn!(swap_id = swap.id, error = %e, "CCTP delivery status query failed");
            }
        },
        BridgeKind::Oft => match executor.oft_delivery_status(&bridge_ref).await {
            Ok(status) => {
                if status.is_delivered() {
                    finalize_completed(store, event_emitter, &swap.id, None).await;
                }
            }
            Err(e) => {
                tracing::warn!(swap_id = swap.id, error = %e, "OFT delivery status query failed");
            }
        },
        // Direct swaps never enter Settling; defensively finalize.
        BridgeKind::Direct => {
            finalize_completed(store, event_emitter, &swap.id, None).await;
        }
    }
}

/// Mark a `Settling` swap `Completed`, idempotently.
///
/// `poll_settling_swaps` runs on two tasks — the background delivery ticker
/// (event loop) and the on-demand [`crate::BoltzService::refresh_pending_deliveries`]
/// (caller task) — each operating on its own snapshot. Re-reading here and
/// bailing unless the swap is still `Settling` prevents a double `Completed`
/// emission (and a redundant store write) when both observe the same swap as
/// delivered. The delivered amount is identical on either path, so the only
/// thing being deduplicated is the event/write, not the value.
async fn finalize_completed(
    store: &Arc<dyn BoltzStorage>,
    event_emitter: &EventEmitter,
    swap_id: &str,
    delivered_amount: Option<u64>,
) {
    match store.get_swap(swap_id).await {
        Ok(Some(mut fresh)) => {
            if fresh.status != BoltzSwapStatus::Settling {
                // Already finalized by a concurrent poll — don't double-emit.
                return;
            }
            if let Some(amount) = delivered_amount {
                fresh.delivered_amount = Some(amount);
            }
            update_swap_status(
                &**store,
                event_emitter,
                &mut fresh,
                BoltzSwapStatus::Completed,
            )
            .await;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(swap_id, error = %e, "Failed to re-read swap before completion");
        }
    }
}

/// The authoritative CCTP delivered amount, but only once delivery is real:
/// the message must be *forwarded* (minted on the destination) and *attested*
/// (so the finalized `feeExecuted` — hence the amount — is known). Returns
/// `None` while either is still pending, keeping the swap `Settling`.
fn cctp_completion_amount(status: &crate::evm::cctp::CctpMessageStatus) -> Option<u64> {
    if status.is_forwarded() {
        status.delivered_amount
    } else {
        None
    }
}

fn delivered_source_for(
    executor: &ReverseSwapExecutor,
    swap: &BoltzSwap,
) -> Option<DeliveredAmountSource> {
    let registry = &executor.chain_registry;
    let dest = registry.find(&swap.destination_chain, swap.asset)?;
    match dest.bridge {
        // CCTP swaps emit a MessageSent log from the MessageTransmitter rather
        // than an OFTSent / Transfer log.
        Bridge::Cctp { .. } => {
            let message_transmitter = parse_address(CCTP_MESSAGE_TRANSMITTER_V2).ok()?;
            Some(DeliveredAmountSource::Cctp {
                message_transmitter,
            })
        }
        // Direct delivery: read the final ERC20 Transfer of the output token
        // (USDT or USDC) to the user on Arbitrum.
        Bridge::Direct => {
            let token = parse_address(dest.dex_output_token).ok()?;
            let user = parse_address(&swap.destination_address).ok()?;
            Some(DeliveredAmountSource::ArbitrumTransfer { token, user })
        }
        Bridge::Oft { mesh, .. } => {
            let oft_addr = registry.oft_for(mesh)?;
            let oft_contract = parse_address(oft_addr).ok()?;
            Some(DeliveredAmountSource::OftSent { oft_contract })
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;
    use crate::evm::cctp::CctpMessageStatus;
    use crate::models::Asset;

    fn swap_with(bridge_kind: BridgeKind, bridge_ref: Option<&str>) -> BoltzSwap {
        BoltzSwap {
            id: "s1".to_string(),
            status: BoltzSwapStatus::Claiming,
            bridge_kind,
            claim_key_index: 0,
            chain_id: 42161,
            claim_address: "0xabc".to_string(),
            destination_address: "0xdef".to_string(),
            destination_chain: "Arbitrum One".to_string(),
            asset: Asset::Usdt,
            refund_address: "0x123".to_string(),
            erc20swap_address: "0xswap".to_string(),
            router_address: "0xrouter".to_string(),
            invoice: "lnbc".to_string(),
            invoice_amount_sats: 100_000,
            onchain_amount: 99_500,
            expected_output_amount: 71_000_000,
            slippage_bps: 100,
            timeout_block_height: 123_456,
            lockup_tx_id: None,
            claim_tx_hash: None,
            pending_call_id: None,
            delivered_amount: None,
            bridge_ref: bridge_ref.map(str::to_string),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[macros::test_all]
    fn direct_completes_immediately() {
        let swap = swap_with(BridgeKind::Direct, None);
        assert_eq!(post_claim_status(&swap), BoltzSwapStatus::Completed);
    }

    #[macros::test_all]
    fn bridged_with_ref_settles() {
        let oft = swap_with(BridgeKind::Oft, Some("0xguid"));
        assert_eq!(post_claim_status(&oft), BoltzSwapStatus::Settling);
        let cctp = swap_with(BridgeKind::Cctp, Some("3:0xhash"));
        assert_eq!(post_claim_status(&cctp), BoltzSwapStatus::Settling);
    }

    #[macros::test_all]
    fn bridged_without_ref_holds_in_settling_not_completed() {
        // Anomalous: a successful bridged claim should yield a bridge_ref.
        // Without one we still must NOT mark the swap Completed unverified —
        // it holds in Settling (delivery unconfirmable) rather than lying.
        let oft = swap_with(BridgeKind::Oft, None);
        assert_eq!(post_claim_status(&oft), BoltzSwapStatus::Settling);
        let cctp = swap_with(BridgeKind::Cctp, None);
        assert_eq!(post_claim_status(&cctp), BoltzSwapStatus::Settling);
    }

    // ─── delivered_and_ref: per-bridge field mapping at claim time ──────

    #[macros::test_all]
    fn cctp_defers_amount_and_keys_iris_by_domain_and_tx() {
        let decoded = DeliveredAmount {
            amount: 1_000_000,
            lz_guid: None,
            cctp_source_domain: Some(3),
        };
        let (delivered, bridge_ref) = delivered_and_ref(decoded, "0xburn");
        // CCTP amount is not authoritative at claim — deferred to attestation.
        assert_eq!(delivered, None);
        assert_eq!(bridge_ref.as_deref(), Some("3:0xburn"));
    }

    #[macros::test_all]
    fn oft_sets_final_amount_and_carries_lz_guid() {
        let decoded = DeliveredAmount {
            amount: 990_000,
            lz_guid: Some("0xguid".to_string()),
            cctp_source_domain: None,
        };
        let (delivered, bridge_ref) = delivered_and_ref(decoded, "0xclaim");
        assert_eq!(delivered, Some(990_000));
        assert_eq!(bridge_ref.as_deref(), Some("0xguid"));
    }

    #[macros::test_all]
    fn direct_sets_final_amount_and_no_bridge_ref() {
        let decoded = DeliveredAmount {
            amount: 500_000,
            lz_guid: None,
            cctp_source_domain: None,
        };
        let (delivered, bridge_ref) = delivered_and_ref(decoded, "0xclaim");
        assert_eq!(delivered, Some(500_000));
        assert_eq!(bridge_ref, None);
    }

    // ─── cctp_completion_amount: only complete once forwarded + attested ──

    fn cctp_status(forward_tx: Option<&str>, delivered: Option<u64>) -> CctpMessageStatus {
        CctpMessageStatus {
            found: true,
            forward_tx_hash: forward_tx.map(str::to_string),
            delivered_amount: delivered,
            ..Default::default()
        }
    }

    #[macros::test_all]
    fn cctp_completes_only_when_forwarded_and_attested() {
        // Forwarded (minted) + attested (amount known) → complete with amount.
        assert_eq!(
            cctp_completion_amount(&cctp_status(Some("0xfwd"), Some(980_000))),
            Some(980_000)
        );
        // Attested but not yet forwarded → still settling.
        assert_eq!(
            cctp_completion_amount(&cctp_status(None, Some(980_000))),
            None
        );
        // Forwarded but no attested amount yet → still settling.
        assert_eq!(
            cctp_completion_amount(&cctp_status(Some("0xfwd"), None)),
            None
        );
        // Neither → still settling.
        assert_eq!(cctp_completion_amount(&cctp_status(None, None)), None);
    }
}
