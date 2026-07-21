//! The deposit engine: one serialized `tick` drives every inflow and lock
//! unit forward by (at most) one step each, persisting at every boundary.
//!
//! Tick order: resolve in-flight sends -> scan chains -> adopt/derive burn
//! schedules -> advance mints -> promote minted inflows to lock units ->
//! adopt/derive the lock schedule -> advance each lock unit's phase. Every
//! money-moving send goes through the `deposit::sends` nonce discipline, and
//! "is this send still needed" is always re-derived from chain logs
//! (`deposit::schedule`), never from the possibly-stale store.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, U256};

use crate::api::BoltzApiClient;
use crate::api::types::{
    BindCommitmentRequest, CommitmentRefundRequest, CreateSubmarineSwapRequest, SubmarinePairInfo,
};
use crate::config::{
    ARBITRUM_CHAIN_ID, ARBITRUM_USDC_ADDRESS, CCTP_ARBITRUM_DOMAIN, CCTP_FINALITY_FAST,
    CCTP_FORWARD_HOOK_DATA_HEX, CCTP_MESSAGE_TRANSMITTER_V2, CCTP_TOKEN_MESSENGER_V2,
    DepositChainConfig, DepositChainSpec, DepositConfig, deposit_chain_spec,
};
use crate::deposit::models::{
    Deposit, DepositStatus, DepositSwap, DepositSwapStatus, ParkReason, PendingSend,
};
use crate::deposit::schedule::{
    ObservedBurn, ObservedLock, derive_burn_schedule, derive_lock_schedule,
};
use crate::deposit::sends::{SendOutcome, read_deposit_nonce, send_nonce_guarded};
use crate::deposit::{DepositInvoiceResolver, InvoiceRequest};
use crate::error::BoltzError;
use crate::events::{BoltzSwapEvent, EventEmitter};
use crate::evm::alchemy::{AlchemyGasClient, CallStatus, EvmCall};
use crate::evm::cctp::{CctpFeeClient, add_fee_buffer, compute_total_fee};
use crate::evm::contracts::{
    COMMITMENT_PREIMAGE_HASH, address_to_topic, bytes32_to_topic, decode_allowance_return,
    decode_balance_of, decode_cctp_delivered_from_message, decode_cctp_nonce_from_message,
    decode_deposit_for_burn_event, decode_lockup_event, decode_used_nonces_return,
    decode_version_return, deposit_for_burn_event_topic0, encode_allowance, encode_approve,
    encode_balance_of, encode_deposit_for_burn_with_hook, encode_lock, encode_receive_message,
    encode_refund_cooperative, encode_used_nonces, encode_version_call, lockup_event_topic0,
    parse_address,
};
use crate::evm::provider::EvmProvider;
use crate::evm::signing::{EvmSignature, EvmSigner};
use crate::keys::EvmKeyPair;
use crate::store::DepositStorage;
use crate::swap::reverse::current_unix_timestamp;

/// Boltz currency of the locked token — commitment endpoints are keyed by
/// CURRENCY, not chain: `/v2/commitment/ARB/...` serves native-ETH
/// (`EtherSwap`) commitments, `/v2/commitment/USDC/...` the `ERC20Swap` ones we
/// lock. Matches PR #1550's `commitmentAsset = "USDC"`. Not live in
/// production yet (details 404s until Boltz lists the currency) — the engine
/// just retries.
pub(crate) const COMMITMENT_CURRENCY: &str = "USDC";
/// Boltz asset symbol of the bridged token (submarine pair "from").
pub(crate) const DEPOSIT_BRIDGE_ASSET: &str = "USDC";
/// Submarine pair "to" (Lightning BTC).
const SUBMARINE_TO: &str = "BTC";
/// Wait for Circle's forwarder this long after the burn before self-minting
/// (mirrors boltz-web-app's 5-minute default).
const MINT_DEADLINE_SECS: u64 = 5 * 60;
/// Minimum remaining invoice lifetime at resolve time: must comfortably
/// cover lock + create + bind + the server's pay attempt.
const MIN_INVOICE_EXPIRY_SECS: u64 = 600;
/// `maxOverpaymentPercentage` sent on bind — the backend cap.
const BIND_MAX_OVERPAYMENT_PCT: f64 = 10.0;

/// Everything the engine needs, wired by the service at construction.
pub(crate) struct DepositEngineDeps {
    pub api: BoltzApiClient,
    pub store: Arc<dyn DepositStorage>,
    pub alchemy: AlchemyGasClient,
    pub cctp_fee: CctpFeeClient,
    /// One provider per configured source chain, Arbitrum included.
    pub providers: HashMap<u64, EvmProvider>,
    pub config: DepositConfig,
    pub deposit_key: EvmKeyPair,
    pub resolver: Arc<dyn DepositInvoiceResolver>,
    pub events: Arc<EventEmitter>,
    pub referral_id: String,
}

pub(crate) struct DepositEngine {
    deps: DepositEngineDeps,
    deposit_address: String,
    /// Commitment contract details cache (contract + claim address are
    /// stable; the timelock is re-fetched fresh for every lock).
    commitment_contract: platform_utils::tokio::sync::Mutex<Option<(String, String)>>,
}

impl DepositEngine {
    pub(crate) fn new(deps: DepositEngineDeps) -> Self {
        let deposit_address = deps.deposit_key.address_hex();
        Self {
            deps,
            deposit_address,
            commitment_contract: platform_utils::tokio::sync::Mutex::new(None),
        }
    }

    pub(crate) fn deposit_address(&self) -> &str {
        &self.deposit_address
    }

    /// One full pass. Errors are contained per stage/record: a failing chain
    /// or record logs and skips — it never aborts the tick for the others.
    pub(crate) async fn tick(&self) {
        if let Err(e) = self.resolve_pending_sends().await {
            tracing::warn!(error = %e, "deposit tick: pending-send resolution failed");
        }
        self.scan_all_chains().await;
        self.drive_burns().await;
        self.drive_mints().await;
        if let Err(e) = self.promote_minted().await {
            tracing::warn!(error = %e, "deposit tick: promotion failed");
        }
        self.drive_lock_schedule().await;
        self.drive_swaps().await;
    }

    // ─── Pending sends ───────────────────────────────────────────────────

    /// Resolve every persisted in-flight send to a terminal answer where
    /// possible: a confirmed tx hash advances the record; a terminal failure
    /// or a pre-submit crash clears the anchor (the schedule re-derives what
    /// is still needed from chain truth).
    async fn resolve_pending_sends(&self) -> Result<(), BoltzError> {
        for mut deposit in self.deps.store.list_open_deposits().await? {
            let Some(pending) = deposit.pending_send.clone() else {
                continue;
            };
            match self.check_pending(&pending).await {
                PendingResolution::Confirmed(tx_hash) => {
                    // A deposit-record send is an approval, the burn, or a
                    // manual mint — only a receipt carrying our
                    // DepositForBurn event is the burn. Everything else just
                    // clears the anchor (mint completion is detected via
                    // usedNonces; approvals need no record).
                    deposit.pending_send = None;
                    if self
                        .receipt_has_our_burn(pending.chain_id, &tx_hash)
                        .await
                        .unwrap_or(false)
                    {
                        deposit.burn_tx_hash = Some(tx_hash);
                        deposit.status = DepositStatus::AwaitingMint;
                        deposit.mint_deadline =
                            Some(current_unix_timestamp().saturating_add(MINT_DEADLINE_SECS));
                    }
                    self.persist_deposit(deposit).await?;
                }
                PendingResolution::Gone => {
                    deposit.pending_send = None;
                    self.persist_deposit(deposit).await?;
                }
                PendingResolution::StillPending => {}
            }
        }

        for mut swap in self.deps.store.list_active_deposit_swaps().await? {
            let Some(pending) = swap.pending_send.clone() else {
                continue;
            };
            match self.check_pending(&pending).await {
                PendingResolution::Confirmed(tx_hash) => {
                    swap.pending_send = None;
                    match swap.status {
                        DepositSwapStatus::Locking => {
                            if let Err(e) = self.absorb_lock_receipt(&mut swap, &tx_hash).await {
                                tracing::warn!(id = %swap.id, error = %e, "lock receipt absorb failed");
                                self.deps.store.upsert_deposit_swap(&swap).await?;
                                continue;
                            }
                        }
                        DepositSwapStatus::Refunding => {
                            swap.refund_tx_hash = Some(tx_hash);
                            swap.status = DepositSwapStatus::Failed {
                                reason: "refunded".to_string(),
                            };
                        }
                        _ => {}
                    }
                    self.persist_deposit_swap(swap).await?;
                }
                PendingResolution::Gone => {
                    swap.pending_send = None;
                    self.persist_deposit_swap(swap).await?;
                }
                PendingResolution::StillPending => {}
            }
        }
        Ok(())
    }

    /// Whether a confirmed tx carries a `DepositForBurn` from our address.
    async fn receipt_has_our_burn(&self, chain_id: u64, tx_hash: &str) -> Result<bool, BoltzError> {
        let provider = self.provider(chain_id)?;
        let Some(receipt) = provider.eth_get_transaction_receipt(tx_hash).await? else {
            return Ok(false);
        };
        Ok(receipt.logs.iter().any(|log| {
            decode_deposit_for_burn_event(log).is_some_and(|ev| {
                ev.depositor
                    .to_string()
                    .eq_ignore_ascii_case(&self.deposit_address)
            })
        }))
    }

    async fn check_pending(&self, pending: &PendingSend) -> PendingResolution {
        let Some(call_id) = &pending.call_id else {
            // Crashed between persisting the anchor and submitting: nothing
            // was signed, so nothing can be in flight.
            return PendingResolution::Gone;
        };
        match self.deps.alchemy.check_call_status_once(call_id).await {
            Ok(CallStatus::Confirmed(tx_hash)) => PendingResolution::Confirmed(tx_hash),
            Ok(CallStatus::Failed { reason, .. }) => {
                tracing::warn!(call_id, reason, "sponsored deposit send failed terminally");
                PendingResolution::Gone
            }
            Ok(CallStatus::Pending) => PendingResolution::StillPending,
            Err(e) => {
                tracing::debug!(call_id, error = %e, "call status check failed; retrying next tick");
                PendingResolution::StillPending
            }
        }
    }

    /// Whether any in-flight send exists on `chain_id` — new sends on that
    /// chain wait until it resolves (one send at a time per account+chain).
    async fn chain_has_pending_send(&self, chain_id: u64) -> Result<bool, BoltzError> {
        for d in self.deps.store.list_open_deposits().await? {
            if let Some(p) = &d.pending_send
                && p.chain_id == chain_id
            {
                return Ok(true);
            }
        }
        for s in self.deps.store.list_active_deposit_swaps().await? {
            if let Some(p) = &s.pending_send
                && p.chain_id == chain_id
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ─── Detection ───────────────────────────────────────────────────────

    async fn scan_all_chains(&self) {
        if !self.deps.config.watch {
            return;
        }
        let erc20swap = self.commitment_contract_address().await.ok();
        for (spec, chain_cfg) in self.configured_chains() {
            let Some(provider) = self.deps.providers.get(&spec.chain_id) else {
                continue;
            };
            let swap_addr = if spec.chain_id == ARBITRUM_CHAIN_ID {
                erc20swap.as_deref()
            } else {
                None
            };
            match crate::deposit::detect::scan_chain_once(
                provider,
                spec,
                chain_cfg.confirmations,
                &self.deposit_address,
                swap_addr,
                self.deps.store.as_ref(),
            )
            .await
            {
                Ok(new_deposits) => {
                    for deposit in new_deposits {
                        self.deps
                            .events
                            .emit(&BoltzSwapEvent::DepositUpdated { deposit })
                            .await;
                    }
                }
                Err(e) => {
                    tracing::warn!(chain = spec.label, error = %e, "deposit scan failed");
                }
            }
        }
    }

    // ─── Bridge (burn) phase ─────────────────────────────────────────────

    async fn drive_burns(&self) {
        for (spec, _cfg) in self.configured_chains() {
            if spec.chain_id == ARBITRUM_CHAIN_ID {
                continue; // local inflows never burn
            }
            if let Err(e) = self.drive_chain_burns(spec).await {
                tracing::warn!(chain = spec.label, error = %e, "burn drive failed");
            }
        }
    }

    #[expect(clippy::too_many_lines)]
    async fn drive_chain_burns(&self, spec: &DepositChainSpec) -> Result<(), BoltzError> {
        let provider = self.provider(spec.chain_id)?;

        // Nonce FIRST, then log scan — the ordering the send guard relies on.
        let nonce = read_deposit_nonce(provider, &self.deposit_address).await?;

        let all = self.deps.store.list_chain_deposits(spec.chain_id).await?;
        let recorded_burns: Vec<String> = all
            .iter()
            .filter_map(|d| d.burn_tx_hash.clone())
            .map(|h| h.to_lowercase())
            .collect();
        let unrecorded: Vec<Deposit> = all
            .into_iter()
            .filter(|d| d.burn_tx_hash.is_none())
            .collect();
        if unrecorded.is_empty() {
            return Ok(());
        }

        // Observed burns from our address since the oldest candidate inflow,
        // minus the ones already attributed by records.
        let from_block = unrecorded
            .iter()
            .map(|d| d.block_number)
            .min()
            .unwrap_or_default();
        let burns = self
            .fetch_observed_burns(provider, spec, from_block)
            .await?
            .into_iter()
            .filter(|b| !recorded_burns.contains(&b.tx_hash.to_lowercase()))
            .collect::<Vec<_>>();

        let schedule = match derive_burn_schedule(&unrecorded, &burns) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    chain = spec.label,
                    ?e,
                    "burn schedule inconsistent; stalling chain"
                );
                return Ok(());
            }
        };

        // Adopt whatever already burned — whoever sent it.
        for (deposit_id, burn_tx) in &schedule.adopted {
            if let Some(mut d) = self.deps.store.get_deposit(deposit_id).await? {
                d.burn_tx_hash = Some(burn_tx.clone());
                d.pending_send = None;
                if d.mint_deadline.is_none() {
                    d.mint_deadline =
                        Some(current_unix_timestamp().saturating_add(MINT_DEADLINE_SECS));
                }
                if matches!(
                    d.status,
                    DepositStatus::Detected
                        | DepositStatus::Parked { .. }
                        | DepositStatus::Bridging
                ) {
                    d.status = DepositStatus::AwaitingMint;
                }
                self.persist_deposit(d).await?;
            }
        }

        let Some(next_id) = schedule.next else {
            return Ok(());
        };
        if self.chain_has_pending_send(spec.chain_id).await? {
            return Ok(());
        }
        let Some(mut deposit) = self.deps.store.get_deposit(&next_id).await? else {
            return Ok(());
        };

        // Fee floor: a burn with maxFee >= amount reverts on-chain; park
        // instead of ever attempting it (avoids the reference impl's
        // revert-retry loop).
        let fee = self
            .deps
            .cctp_fee
            .get_fee(
                spec.cctp_domain,
                CCTP_ARBITRUM_DOMAIN,
                CCTP_FINALITY_FAST,
                false,
            )
            .await?;
        let max_fee = add_fee_buffer(compute_total_fee(
            u128::from(deposit.amount),
            fee.bps_units,
            fee.forward_fee,
        ));
        if max_fee >= u128::from(deposit.amount) {
            deposit.status = DepositStatus::Parked {
                reason: ParkReason::BelowBridgeFee,
            };
            self.persist_deposit(deposit).await?;
            return Ok(());
        }

        // Conservation floor: the address balance must cover every inflow we
        // believe is un-burned; anything less means a send we haven't seen.
        let balance = self
            .erc20_balance(provider, spec.usdc_address, &self.deposit_address)
            .await?;
        if balance < U256::from(schedule.unburned_total) {
            tracing::warn!(
                chain = spec.label,
                %balance,
                expected = schedule.unburned_total,
                "deposit address balance below un-burned total; stalling burns"
            );
            return Ok(());
        }

        // Approval is its own send (never batched — the sponsor returns one
        // receipt per send); the burn follows on a later tick.
        let token_messenger = parse_address(CCTP_TOKEN_MESSENGER_V2)?;
        let allowance = self
            .erc20_allowance(provider, spec.usdc_address, token_messenger)
            .await?;
        if allowance < U256::from(deposit.amount) {
            self.submit_guarded(
                spec.chain_id,
                &mut RecordRef::Deposit(&mut deposit),
                nonce,
                vec![EvmCall {
                    to: spec.usdc_address.to_string(),
                    value: None,
                    data: Some(hex_calldata(&encode_approve(token_messenger, U256::MAX))),
                }],
            )
            .await?;
            return Ok(());
        }

        // The burn itself: forwarded receive mode (Circle auto-mints on
        // Arbitrum; manual receiveMessage is the fallback).
        let mint_recipient = evm_address_bytes32(&self.deposit_address)?;
        let hook_data = hex::decode(CCTP_FORWARD_HOOK_DATA_HEX)
            .map_err(|e| BoltzError::Generic(format!("invalid forward hook data const: {e}")))?;
        let calldata = encode_deposit_for_burn_with_hook(
            U256::from(deposit.amount),
            CCTP_ARBITRUM_DOMAIN,
            mint_recipient,
            parse_address(spec.usdc_address)?,
            [0u8; 32],
            U256::from(max_fee),
            CCTP_FINALITY_FAST,
            hook_data,
        );
        deposit.status = DepositStatus::Bridging;
        self.submit_guarded(
            spec.chain_id,
            &mut RecordRef::Deposit(&mut deposit),
            nonce,
            vec![EvmCall {
                to: CCTP_TOKEN_MESSENGER_V2.to_string(),
                value: None,
                data: Some(hex_calldata(&calldata)),
            }],
        )
        .await
    }

    async fn fetch_observed_burns(
        &self,
        provider: &EvmProvider,
        spec: &DepositChainSpec,
        from_block: u64,
    ) -> Result<Vec<ObservedBurn>, BoltzError> {
        let latest = provider.eth_block_number().await?;
        let depositor = parse_address(&self.deposit_address)?;
        let topic0 = deposit_for_burn_event_topic0();
        let depositor_topic = address_to_topic(&depositor.into_array());

        let mut burns = Vec::new();
        let mut start = from_block;
        while start <= latest {
            let end = latest.min(
                start
                    .saturating_add(crate::deposit::detect::DEPOSIT_SCAN_RANGE_BLOCKS)
                    .saturating_sub(1),
            );
            let logs = provider
                .eth_get_logs(
                    CCTP_TOKEN_MESSENGER_V2,
                    &[Some(&topic0), None, Some(&depositor_topic)],
                    start,
                    end,
                )
                .await?;
            for log in &logs {
                if let Some(ev) = decode_deposit_for_burn_event(log)
                    && let Ok(amount) = u64::try_from(ev.amount)
                    && ev.destination_domain == CCTP_ARBITRUM_DOMAIN
                    && ev
                        .burn_token
                        .to_string()
                        .eq_ignore_ascii_case(spec.usdc_address)
                {
                    burns.push(ObservedBurn {
                        amount,
                        tx_hash: log.transaction_hash.to_lowercase(),
                    });
                }
            }
            start = end.saturating_add(1);
        }
        Ok(burns)
    }

    // ─── Mint phase ──────────────────────────────────────────────────────

    async fn drive_mints(&self) {
        let deposits = match self.deps.store.list_open_deposits().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "mint drive: store list failed");
                return;
            }
        };
        for deposit in deposits {
            if deposit.status != DepositStatus::AwaitingMint {
                continue;
            }
            if let Err(e) = self.drive_mint(deposit).await {
                tracing::warn!(error = %e, "mint drive failed");
            }
        }
    }

    async fn drive_mint(&self, mut deposit: Deposit) -> Result<(), BoltzError> {
        let Some(burn_tx) = deposit.burn_tx_hash.clone() else {
            return Ok(());
        };
        let Some(spec) = deposit_chain_spec(deposit.chain_id) else {
            return Ok(());
        };

        let status = self
            .deps
            .cctp_fee
            .get_message_status(spec.cctp_domain, &burn_tx)
            .await?;

        if deposit.cctp_nonce.is_none()
            && let Some(message) = &status.message
            && let Some(nonce) = decode_cctp_nonce_from_message(message)
        {
            deposit.cctp_nonce = Some(format!("0x{}", hex::encode(nonce)));
            self.deps.store.upsert_deposit(&deposit).await?;
        }

        // Happy path: Circle's forwarder minted; Iris reports the
        // fee-adjusted delivered amount once forwarded AND attested.
        if status.is_forwarded()
            && let Some(delivered) = status.delivered_amount
        {
            deposit.minted_amount = Some(delivered);
            deposit.status = DepositStatus::Minted;
            self.persist_deposit(deposit).await?;
            return Ok(());
        }

        // Fallback: self-submit receiveMessage on Arbitrum once the deadline
        // passed and the attestation is available.
        let deadline_passed = deposit
            .mint_deadline
            .is_some_and(|d| current_unix_timestamp() > d);
        if !deadline_passed {
            return Ok(());
        }
        let (Some(message), Some(attestation)) = (&status.message, &status.attestation) else {
            return Ok(());
        };
        let Some(nonce_hex) = deposit.cctp_nonce.clone() else {
            return Ok(());
        };
        let nonce_bytes = parse_bytes32(&nonce_hex)?;

        let arb = self.provider(ARBITRUM_CHAIN_ID)?;
        // Idempotency: a consumed nonce means the mint already landed
        // (whoever submitted it) — never a second receiveMessage.
        let used = arb
            .eth_call(
                CCTP_MESSAGE_TRANSMITTER_V2,
                &encode_used_nonces(nonce_bytes),
            )
            .await
            .and_then(|ret| decode_used_nonces_return(&ret))?;
        if used {
            let delivered = decode_cctp_delivered_from_message(message).ok_or_else(|| {
                BoltzError::Generic("attested CCTP message has no decodable amount".to_string())
            })?;
            deposit.minted_amount = Some(delivered);
            deposit.status = DepositStatus::Minted;
            self.persist_deposit(deposit).await?;
            return Ok(());
        }

        if self.chain_has_pending_send(ARBITRUM_CHAIN_ID).await? {
            return Ok(());
        }
        let nonce = read_deposit_nonce(arb, &self.deposit_address).await?;
        let message_bytes = hex::decode(message.strip_prefix("0x").unwrap_or(message))
            .map_err(|e| BoltzError::Generic(format!("invalid CCTP message hex: {e}")))?;
        let attestation_bytes = hex::decode(attestation.strip_prefix("0x").unwrap_or(attestation))
            .map_err(|e| BoltzError::Generic(format!("invalid attestation hex: {e}")))?;
        let calldata = encode_receive_message(&message_bytes, &attestation_bytes);
        self.submit_guarded(
            ARBITRUM_CHAIN_ID,
            &mut RecordRef::Deposit(&mut deposit),
            nonce,
            vec![EvmCall {
                to: CCTP_MESSAGE_TRANSMITTER_V2.to_string(),
                value: None,
                data: Some(hex_calldata(&calldata)),
            }],
        )
        .await
        // Completion is observed via the usedNonces probe on a later tick;
        // the pending-send resolution only clears the anchor (a manual-mint
        // receipt carries no DepositForBurn, so it is never mistaken for one).
    }

    // ─── Promotion (Minted inflow -> lock unit) ──────────────────────────

    async fn promote_minted(&self) -> Result<(), BoltzError> {
        let deposits = self.deps.store.list_open_deposits().await?;
        let arb_head = self.provider(ARBITRUM_CHAIN_ID)?.eth_block_number().await?;
        for mut deposit in deposits {
            if deposit.status != DepositStatus::Minted {
                continue;
            }
            let Some(minted) = deposit.minted_amount else {
                continue;
            };
            let ids = vec![deposit.id.clone()];
            let ds_id = DepositSwap::derive_id(&ids);
            if self.deps.store.get_deposit_swap(&ds_id).await?.is_none() {
                let now = current_unix_timestamp();
                let swap = DepositSwap {
                    id: ds_id.clone(),
                    status: DepositSwapStatus::Resolving,
                    deposit_ids: ids,
                    amount: minted,
                    deposit_address: self.deposit_address.clone(),
                    created_at_block: arb_head,
                    erc20swap_address: None,
                    claim_address: None,
                    timelock: None,
                    pending_send: None,
                    commitment_tx_hash: None,
                    commitment_log_index: None,
                    invoice: None,
                    invoice_amount_sats: None,
                    swap_id: None,
                    expected_amount: None,
                    bound: false,
                    refund_tx_hash: None,
                    created_at: now,
                    updated_at: now,
                };
                self.persist_deposit_swap(swap).await?;
            }
            deposit.status = DepositStatus::Consumed;
            deposit.deposit_swap_id = Some(ds_id);
            self.persist_deposit(deposit).await?;
        }
        Ok(())
    }

    // ─── Lock schedule (adoption + one lock at a time) ───────────────────

    async fn drive_lock_schedule(&self) {
        if let Err(e) = self.drive_lock_schedule_inner().await {
            tracing::warn!(error = %e, "lock schedule drive failed");
        }
    }

    #[expect(clippy::too_many_lines)]
    async fn drive_lock_schedule_inner(&self) -> Result<(), BoltzError> {
        let swaps = self.deps.store.list_active_deposit_swaps().await?;
        let candidates: Vec<DepositSwap> = swaps
            .into_iter()
            .filter(|s| {
                matches!(
                    s.status,
                    DepositSwapStatus::Locking | DepositSwapStatus::Resolving
                ) || s.commitment_tx_hash.is_some()
            })
            .collect();
        let locking: Vec<DepositSwap> = candidates
            .iter()
            .filter(|s| matches!(s.status, DepositSwapStatus::Locking))
            .cloned()
            .collect();
        if locking.is_empty() {
            return Ok(());
        }

        let arb = self.provider(ARBITRUM_CHAIN_ID)?;
        let nonce = read_deposit_nonce(arb, &self.deposit_address).await?;
        let (contract, boltz_claim) = self.commitment_details_cached().await?;

        let from_block = candidates
            .iter()
            .map(|s| s.created_at_block)
            .min()
            .unwrap_or_default();
        let locks = self
            .fetch_observed_locks(arb, &contract, from_block)
            .await?;

        let schedule = match derive_lock_schedule(&candidates, &locks) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, "lock schedule inconsistent; stalling locks");
                return Ok(());
            }
        };

        for (ds_id, lock) in &schedule.adopted {
            if let Some(mut ds) = self.deps.store.get_deposit_swap(ds_id).await?
                && ds.commitment_tx_hash.is_none()
            {
                ds.commitment_tx_hash = Some(lock.tx_hash.clone());
                ds.commitment_log_index = Some(lock.log_index);
                ds.timelock = Some(lock.timelock);
                ds.erc20swap_address = Some(contract.clone());
                ds.claim_address = Some(boltz_claim.clone());
                ds.pending_send = None;
                if matches!(ds.status, DepositSwapStatus::Locking) {
                    ds.status = DepositSwapStatus::Creating;
                }
                self.persist_deposit_swap(ds).await?;
            }
        }

        let Some(next_id) = schedule.next else {
            return Ok(());
        };
        // Only lock units that finished resolving are sendable.
        let Some(mut ds) = self.deps.store.get_deposit_swap(&next_id).await? else {
            return Ok(());
        };
        if !matches!(ds.status, DepositSwapStatus::Locking) || ds.invoice.is_none() {
            return Ok(());
        }
        if self.chain_has_pending_send(ARBITRUM_CHAIN_ID).await? {
            return Ok(());
        }

        // Allowance for the swap contract (lock pulls via transferFrom).
        let contract_addr = parse_address(&contract)?;
        let allowance = self
            .erc20_allowance(arb, ARBITRUM_USDC_ADDRESS, contract_addr)
            .await?;
        if allowance < U256::from(ds.amount) {
            self.submit_guarded(
                ARBITRUM_CHAIN_ID,
                &mut RecordRef::Swap(&mut ds),
                nonce,
                vec![EvmCall {
                    to: ARBITRUM_USDC_ADDRESS.to_string(),
                    value: None,
                    data: Some(hex_calldata(&encode_approve(contract_addr, U256::MAX))),
                }],
            )
            .await?;
            return Ok(());
        }

        // Fresh timelock for this lock attempt.
        let details = self
            .deps
            .api
            .get_commitment_details(COMMITMENT_CURRENCY)
            .await?;
        let calldata = encode_lock(
            COMMITMENT_PREIMAGE_HASH,
            U256::from(ds.amount),
            parse_address(ARBITRUM_USDC_ADDRESS)?,
            parse_address(&details.claim_address)?,
            parse_address(&self.deposit_address)?,
            U256::from(details.timelock),
        );
        ds.erc20swap_address = Some(details.contract.clone());
        ds.claim_address = Some(details.claim_address.clone());
        ds.timelock = Some(details.timelock);
        self.submit_guarded(
            ARBITRUM_CHAIN_ID,
            &mut RecordRef::Swap(&mut ds),
            nonce,
            vec![EvmCall {
                to: details.contract,
                value: None,
                data: Some(hex_calldata(&calldata)),
            }],
        )
        .await
    }

    async fn fetch_observed_locks(
        &self,
        arb: &EvmProvider,
        contract: &str,
        from_block: u64,
    ) -> Result<Vec<ObservedLock>, BoltzError> {
        let latest = arb.eth_block_number().await?;
        let refund_topic = address_to_topic(&parse_address(&self.deposit_address)?.into_array());
        let zero_topic = bytes32_to_topic(&COMMITMENT_PREIMAGE_HASH);
        let topic0 = lockup_event_topic0();

        let mut locks = Vec::new();
        let mut start = from_block;
        while start <= latest {
            let end = latest.min(
                start
                    .saturating_add(crate::deposit::detect::DEPOSIT_SCAN_RANGE_BLOCKS)
                    .saturating_sub(1),
            );
            let logs = arb
                .eth_get_logs(
                    contract,
                    &[Some(&topic0), Some(&zero_topic), None, Some(&refund_topic)],
                    start,
                    end,
                )
                .await?;
            for log in &logs {
                if let Some(ev) = decode_lockup_event(log)
                    && ev.preimage_hash == COMMITMENT_PREIMAGE_HASH
                    && let Ok(amount) = u64::try_from(ev.amount)
                    && let Ok(timelock) = u64::try_from(ev.timelock)
                    && let Some(log_index) = log
                        .log_index
                        .as_deref()
                        .and_then(|i| u64::from_str_radix(i.trim_start_matches("0x"), 16).ok())
                {
                    locks.push(ObservedLock {
                        amount,
                        tx_hash: log.transaction_hash.to_lowercase(),
                        log_index,
                        timelock,
                    });
                }
            }
            start = end.saturating_add(1);
        }
        Ok(locks)
    }

    // ─── Lock-unit phases ────────────────────────────────────────────────

    async fn drive_swaps(&self) {
        let swaps = match self.deps.store.list_active_deposit_swaps().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "swap drive: store list failed");
                return;
            }
        };
        for swap in swaps {
            if swap.pending_send.is_some() {
                continue; // resolved at the top of the tick
            }
            let id = swap.id.clone();
            let result = match swap.status.clone() {
                DepositSwapStatus::Resolving => self.phase_resolve(swap).await,
                DepositSwapStatus::Creating => self.phase_create(swap).await,
                DepositSwapStatus::Binding => self.phase_bind(swap).await,
                DepositSwapStatus::Settling => self.phase_settle(swap).await,
                DepositSwapStatus::Refunding => self.phase_refund(swap).await,
                // Locking is driven by the lock schedule; terminal do nothing.
                _ => Ok(()),
            };
            if let Err(e) = result {
                tracing::warn!(id = %id, error = %e, "deposit swap phase failed; retrying next tick");
            }
        }
    }

    /// Size the invoice from live pair data and fetch it from the resolver.
    /// Everything here is pre-lock: persistent failure parks nothing and
    /// costs nothing — funds sit minted at the deposit address.
    async fn phase_resolve(&self, mut ds: DepositSwap) -> Result<(), BoltzError> {
        let pair = self.submarine_pair().await?;

        if ds.amount < pair.limits.minimal || ds.amount > pair.limits.maximal {
            return self
                .fail_and_repark(
                    ds,
                    ParkReason::BelowPairLimit,
                    format!(
                        "amount outside {DEPOSIT_BRIDGE_ASSET}->{SUBMARINE_TO} limits [{}, {}]",
                        pair.limits.minimal, pair.limits.maximal
                    ),
                )
                .await;
        }

        let request_sats = estimate_submarine_receive_sats(
            ds.amount,
            pair.rate,
            pair.fees.percentage,
            pair.fees.miner_fees,
        );
        if request_sats == 0 {
            return self
                .fail_and_repark(
                    ds,
                    ParkReason::BelowPairLimit,
                    "too small to swap out over Lightning after fees".to_string(),
                )
                .await;
        }

        let request = InvoiceRequest {
            deposit_swap_id: ds.id.clone(),
            amount_sats: request_sats,
            lock_amount: ds.amount,
        };
        let invoice = self.deps.resolver.resolve_invoice(&request).await?;
        verify_invoice(&invoice, request_sats)?;

        ds.invoice = Some(invoice);
        ds.invoice_amount_sats = Some(request_sats);
        ds.status = DepositSwapStatus::Locking;
        self.persist_deposit_swap(ds).await
    }

    async fn phase_create(&self, mut ds: DepositSwap) -> Result<(), BoltzError> {
        if ds.swap_id.is_some() {
            ds.status = DepositSwapStatus::Binding;
            return self.persist_deposit_swap(ds).await;
        }
        let Some(invoice) = ds.invoice.clone() else {
            // Should be unreachable (lock requires an invoice); self-heal.
            ds.status = DepositSwapStatus::Resolving;
            return self.persist_deposit_swap(ds).await;
        };
        let pair = self.submarine_pair().await?;
        let created = self
            .deps
            .api
            .create_submarine_swap(&CreateSubmarineSwapRequest {
                from: DEPOSIT_BRIDGE_ASSET.to_string(),
                to: SUBMARINE_TO.to_string(),
                invoice,
                referral_id: Some(self.deps.referral_id.clone()),
                pair_hash: Some(pair.hash),
            })
            .await?;

        // THE money guard: the server may claim the whole lock, so the swap
        // must never require more than we locked. Oversize (rate moved) is
        // recoverable: drop the invoice and re-size at current numbers.
        if created.expected_amount > ds.amount {
            tracing::warn!(
                id = %ds.id,
                expected = created.expected_amount,
                locked = ds.amount,
                "submarine swap outgrew the lock; re-resolving invoice"
            );
            ds.invoice = None;
            ds.invoice_amount_sats = None;
            ds.status = DepositSwapStatus::Resolving;
            return self.persist_deposit_swap(ds).await;
        }

        ds.swap_id = Some(created.id);
        ds.expected_amount = Some(created.expected_amount);
        ds.status = DepositSwapStatus::Binding;
        self.persist_deposit_swap(ds).await
    }

    async fn phase_bind(&self, mut ds: DepositSwap) -> Result<(), BoltzError> {
        if ds.bound {
            ds.status = DepositSwapStatus::Settling;
            return self.persist_deposit_swap(ds).await;
        }
        let (Some(swap_id), Some(invoice), Some(tx_hash), Some(log_index)) = (
            ds.swap_id.clone(),
            ds.invoice.clone(),
            ds.commitment_tx_hash.clone(),
            ds.commitment_log_index,
        ) else {
            return Ok(());
        };
        let (Some(contract), Some(claim), Some(timelock)) = (
            ds.erc20swap_address.clone(),
            ds.claim_address.clone(),
            ds.timelock,
        ) else {
            return Ok(());
        };

        // Commit binds the lock to the SWAP's preimage hash = the invoice's
        // payment hash (the server learns the preimage by paying it).
        let payment_hash = invoice_payment_hash(&invoice)?;
        let arb = self.provider(ARBITRUM_CHAIN_ID)?;
        let version = arb
            .eth_call(&contract, &encode_version_call())
            .await
            .and_then(|ret| decode_version_return(&ret))?;

        let signer = EvmSigner::new(&self.deps.deposit_key, ARBITRUM_CHAIN_ID);
        let sig = signer.sign_eip712_erc20swap_commit(
            parse_address(&contract)?,
            &version.to_string(),
            &payment_hash,
            U256::from(ds.amount),
            parse_address(ARBITRUM_USDC_ADDRESS)?,
            parse_address(&claim)?,
            parse_address(&self.deposit_address)?,
            U256::from(timelock),
        )?;

        let result = self
            .deps
            .api
            .bind_commitment(
                COMMITMENT_CURRENCY,
                &BindCommitmentRequest {
                    swap_id,
                    signature: signature_hex(&sig),
                    transaction_hash: tx_hash,
                    log_index: u32::try_from(log_index).ok(),
                    max_overpayment_percentage: Some(BIND_MAX_OVERPAYMENT_PCT),
                },
            )
            .await;

        match result {
            Ok(()) => {
                ds.bound = true;
                ds.status = DepositSwapStatus::Settling;
                self.persist_deposit_swap(ds).await
            }
            // "commitment exists already" = bound — by our own earlier
            // attempt (crash before persist) or by another instance whose
            // swap id will arrive via record sync. Either way the record's
            // swap_id is what settle polls, and sync converges it to the
            // winner's; treat as bound and move on.
            Err(BoltzError::Api {
                reason,
                code: Some(400),
            }) if reason.to_lowercase().contains("exists") => {
                tracing::info!(id = %ds.id, "commitment already bound; adopting");
                ds.bound = true;
                ds.status = DepositSwapStatus::Settling;
                self.persist_deposit_swap(ds).await
            }
            Err(e) => Err(e),
        }
    }

    async fn phase_settle(&self, mut ds: DepositSwap) -> Result<(), BoltzError> {
        let Some(swap_id) = ds.swap_id.clone() else {
            return Ok(());
        };
        let status = self.deps.api.get_swap_status(&swap_id).await?;
        if is_success_status(&status.status) {
            ds.status = DepositSwapStatus::Done;
            return self.persist_deposit_swap(ds).await;
        }
        if is_failure_status(&status.status) {
            tracing::warn!(id = %ds.id, status = %status.status, "deposit swap failed; refunding");
            ds.status = DepositSwapStatus::Refunding;
            return self.persist_deposit_swap(ds).await;
        }
        Ok(())
    }

    async fn phase_refund(&self, mut ds: DepositSwap) -> Result<(), BoltzError> {
        if ds.refund_tx_hash.is_some() {
            ds.status = DepositSwapStatus::Failed {
                reason: "refunded".to_string(),
            };
            return self.persist_deposit_swap(ds).await;
        }
        let (Some(tx_hash), Some(log_index)) =
            (ds.commitment_tx_hash.clone(), ds.commitment_log_index)
        else {
            // Nothing was ever locked: fail without an on-chain refund.
            ds.status = DepositSwapStatus::Failed {
                reason: "failed before lock".to_string(),
            };
            return self.persist_deposit_swap(ds).await;
        };
        let (Some(claim), Some(timelock)) = (ds.claim_address.clone(), ds.timelock) else {
            return Ok(());
        };
        if self.chain_has_pending_send(ARBITRUM_CHAIN_ID).await? {
            return Ok(());
        }

        let arb = self.provider(ARBITRUM_CHAIN_ID)?;
        let nonce = read_deposit_nonce(arb, &self.deposit_address).await?;

        // Ownership proof over the exact backend message format.
        let message = format!(
            "Boltz commitment refund authorization\nchain: {COMMITMENT_CURRENCY}\ntransactionHash: {tx_hash}\nlogIndex: {log_index}"
        );
        let signer = EvmSigner::new(&self.deps.deposit_key, ARBITRUM_CHAIN_ID);
        let auth_sig = signer.sign_message(message.as_bytes())?;

        let refund = self
            .deps
            .api
            .get_commitment_refund_signature(
                COMMITMENT_CURRENCY,
                &CommitmentRefundRequest {
                    transaction_hash: tx_hash,
                    refund_address_signature: signature_hex(&auth_sig),
                    log_index: u32::try_from(log_index).ok(),
                },
            )
            .await?;
        let (v, r, s) = parse_signature_hex(&refund.signature)?;

        let calldata = encode_refund_cooperative(
            COMMITMENT_PREIMAGE_HASH,
            U256::from(ds.amount),
            parse_address(ARBITRUM_USDC_ADDRESS)?,
            parse_address(&claim)?,
            parse_address(&self.deposit_address)?,
            U256::from(timelock),
            v,
            r,
            s,
        );
        let contract = ds.erc20swap_address.clone().ok_or_else(|| {
            BoltzError::Generic("refunding without a recorded contract".to_string())
        })?;
        self.submit_guarded(
            ARBITRUM_CHAIN_ID,
            &mut RecordRef::Swap(&mut ds),
            nonce,
            vec![EvmCall {
                to: contract,
                value: None,
                data: Some(hex_calldata(&calldata)),
            }],
        )
        .await
    }

    // ─── Parked recovery (integrator-triggered) ──────────────────────────

    /// Re-evaluate parked inflows and aggregate every Arbitrum-side parked
    /// balance into ONE new lock unit. Returns its id, if one was created.
    pub(crate) async fn retry_parked(&self) -> Result<Option<String>, BoltzError> {
        let deposits = self.deps.store.list_open_deposits().await?;

        // Source-chain dust: back to Detected — the burn drive re-checks the
        // fee floor at current quotes.
        let mut aggregate: Vec<Deposit> = Vec::new();
        for mut d in deposits {
            match &d.status {
                DepositStatus::Parked {
                    reason: ParkReason::BelowBridgeFee,
                } => {
                    d.status = DepositStatus::Detected;
                    self.persist_deposit(d).await?;
                }
                DepositStatus::Parked { .. } if d.minted_amount.is_some() => {
                    aggregate.push(d);
                }
                _ => {}
            }
        }
        if aggregate.is_empty() {
            return Ok(None);
        }

        let ids: Vec<String> = aggregate.iter().map(|d| d.id.clone()).collect();
        let ds_id = DepositSwap::derive_id(&ids);
        if self.deps.store.get_deposit_swap(&ds_id).await?.is_some() {
            return Ok(Some(ds_id));
        }
        let amount = aggregate
            .iter()
            .filter_map(|d| d.minted_amount)
            .fold(0u64, u64::saturating_add);
        let arb_head = self.provider(ARBITRUM_CHAIN_ID)?.eth_block_number().await?;
        let now = current_unix_timestamp();
        let swap = DepositSwap {
            id: ds_id.clone(),
            status: DepositSwapStatus::Resolving,
            deposit_ids: ids,
            amount,
            deposit_address: self.deposit_address.clone(),
            created_at_block: arb_head,
            erc20swap_address: None,
            claim_address: None,
            timelock: None,
            pending_send: None,
            commitment_tx_hash: None,
            commitment_log_index: None,
            invoice: None,
            invoice_amount_sats: None,
            swap_id: None,
            expected_amount: None,
            bound: false,
            refund_tx_hash: None,
            created_at: now,
            updated_at: now,
        };
        self.persist_deposit_swap(swap).await?;
        for mut d in aggregate {
            d.status = DepositStatus::Consumed;
            d.deposit_swap_id = Some(ds_id.clone());
            self.persist_deposit(d).await?;
        }
        Ok(Some(ds_id))
    }

    // ─── Shared helpers ──────────────────────────────────────────────────

    /// Persist the pending-send anchor, then prepare/nonce-check/sign/send.
    /// On `NonceMoved` the anchor is cleared (nothing was signed) and the
    /// next tick re-derives.
    async fn submit_guarded(
        &self,
        chain_id: u64,
        record: &mut RecordRef<'_>,
        expected_nonce: U256,
        calls: Vec<EvmCall>,
    ) -> Result<(), BoltzError> {
        let provider = self.provider(chain_id)?;
        let from_block = provider.eth_block_number().await?;
        record.set_pending(Some(PendingSend {
            chain_id,
            from_block,
            call_id: None,
            created_at: current_unix_timestamp(),
        }));
        record.persist(self).await?;

        let signer = EvmSigner::new(&self.deps.deposit_key, chain_id);
        match send_nonce_guarded(&self.deps.alchemy, &signer, calls, chain_id, expected_nonce)
            .await?
        {
            SendOutcome::Sent { call_id } => {
                record.update_pending_call_id(call_id);
                record.persist(self).await
            }
            SendOutcome::NonceMoved => {
                record.set_pending(None);
                record.persist(self).await
            }
        }
    }

    /// Absorb a confirmed lock tx: find and validate our commitment Lockup
    /// event in the receipt before the unit may ever be bound.
    async fn absorb_lock_receipt(
        &self,
        ds: &mut DepositSwap,
        tx_hash: &str,
    ) -> Result<(), BoltzError> {
        let arb = self.provider(ARBITRUM_CHAIN_ID)?;
        let receipt = arb
            .eth_get_transaction_receipt(tx_hash)
            .await?
            .ok_or_else(|| BoltzError::Generic("lock receipt not found".to_string()))?;
        // An approval send also lands here; it has no Lockup event and just
        // clears the pending anchor (the schedule sends the lock next tick).
        let contract = ds.erc20swap_address.clone().unwrap_or_default();
        for log in &receipt.logs {
            if !log.address.eq_ignore_ascii_case(&contract) {
                continue;
            }
            let Some(ev) = decode_lockup_event(log) else {
                continue;
            };
            // P0-3: validate before this lock can ever be bound.
            if ev.preimage_hash != COMMITMENT_PREIMAGE_HASH
                || ev.amount != U256::from(ds.amount)
                || !ev
                    .token_address
                    .to_string()
                    .eq_ignore_ascii_case(ARBITRUM_USDC_ADDRESS)
                || !ev
                    .refund_address
                    .to_string()
                    .eq_ignore_ascii_case(&self.deposit_address)
            {
                return Err(BoltzError::Generic(
                    "lockup event does not match the intended commitment".to_string(),
                ));
            }
            ds.commitment_tx_hash = Some(receipt.transaction_hash.to_lowercase());
            ds.commitment_log_index = log
                .log_index
                .as_deref()
                .and_then(|i| u64::from_str_radix(i.trim_start_matches("0x"), 16).ok());
            ds.timelock = u64::try_from(ev.timelock).ok();
            ds.status = DepositSwapStatus::Creating;
            return Ok(());
        }
        Ok(())
    }

    async fn commitment_details_cached(&self) -> Result<(String, String), BoltzError> {
        let mut cache = self.commitment_contract.lock().await;
        if let Some(cached) = cache.as_ref() {
            return Ok(cached.clone());
        }
        let details = self
            .deps
            .api
            .get_commitment_details(COMMITMENT_CURRENCY)
            .await?;
        let value = (details.contract, details.claim_address);
        *cache = Some(value.clone());
        Ok(value)
    }

    async fn commitment_contract_address(&self) -> Result<String, BoltzError> {
        Ok(self.commitment_details_cached().await?.0)
    }

    async fn submarine_pair(&self) -> Result<SubmarinePairInfo, BoltzError> {
        let pairs = self.deps.api.get_submarine_pairs().await?;
        pairs
            .0
            .get(DEPOSIT_BRIDGE_ASSET)
            .and_then(|to| to.get(SUBMARINE_TO))
            .cloned()
            .ok_or_else(|| BoltzError::Api {
                reason: format!("no {DEPOSIT_BRIDGE_ASSET} -> {SUBMARINE_TO} submarine pair"),
                code: None,
            })
    }

    async fn fail_and_repark(
        &self,
        mut ds: DepositSwap,
        reason: ParkReason,
        why: String,
    ) -> Result<(), BoltzError> {
        for id in ds.deposit_ids.clone() {
            if let Some(mut d) = self.deps.store.get_deposit(&id).await? {
                d.status = DepositStatus::Parked { reason };
                d.deposit_swap_id = None;
                self.persist_deposit(d).await?;
            }
        }
        ds.status = DepositSwapStatus::Failed { reason: why };
        self.persist_deposit_swap(ds).await
    }

    async fn erc20_balance(
        &self,
        provider: &EvmProvider,
        token: &str,
        owner: &str,
    ) -> Result<U256, BoltzError> {
        let ret = provider
            .eth_call(token, &encode_balance_of(parse_address(owner)?))
            .await?;
        decode_balance_of(&ret)
    }

    async fn erc20_allowance(
        &self,
        provider: &EvmProvider,
        token: &str,
        spender: Address,
    ) -> Result<U256, BoltzError> {
        let owner = parse_address(&self.deposit_address)?;
        let ret = provider
            .eth_call(token, &encode_allowance(owner, spender))
            .await?;
        decode_allowance_return(&ret)
    }

    fn provider(&self, chain_id: u64) -> Result<&EvmProvider, BoltzError> {
        self.deps.providers.get(&chain_id).ok_or_else(|| {
            BoltzError::Generic(format!("no provider configured for chain {chain_id}"))
        })
    }

    fn configured_chains(&self) -> Vec<(&'static DepositChainSpec, &DepositChainConfig)> {
        self.deps
            .config
            .source_chains
            .iter()
            .filter_map(|c| deposit_chain_spec(c.chain_id).map(|s| (s, c)))
            .collect()
    }

    async fn persist_deposit(&self, mut deposit: Deposit) -> Result<(), BoltzError> {
        deposit.updated_at = current_unix_timestamp();
        self.deps.store.upsert_deposit(&deposit).await?;
        self.deps
            .events
            .emit(&BoltzSwapEvent::DepositUpdated { deposit })
            .await;
        Ok(())
    }

    async fn persist_deposit_swap(&self, mut swap: DepositSwap) -> Result<(), BoltzError> {
        swap.updated_at = current_unix_timestamp();
        self.deps.store.upsert_deposit_swap(&swap).await?;
        self.deps
            .events
            .emit(&BoltzSwapEvent::DepositSwapUpdated { swap })
            .await;
        Ok(())
    }
}

enum PendingResolution {
    Confirmed(String),
    Gone,
    StillPending,
}

/// Uniform pending-send handling over both record types.
enum RecordRef<'a> {
    Deposit(&'a mut Deposit),
    Swap(&'a mut DepositSwap),
}

impl RecordRef<'_> {
    fn set_pending(&mut self, pending: Option<PendingSend>) {
        match self {
            Self::Deposit(d) => d.pending_send = pending,
            Self::Swap(s) => s.pending_send = pending,
        }
    }

    fn update_pending_call_id(&mut self, call_id: String) {
        match self {
            Self::Deposit(d) => {
                if let Some(p) = &mut d.pending_send {
                    p.call_id = Some(call_id);
                }
            }
            Self::Swap(s) => {
                if let Some(p) = &mut s.pending_send {
                    p.call_id = Some(call_id);
                }
            }
        }
    }

    async fn persist(&mut self, engine: &DepositEngine) -> Result<(), BoltzError> {
        match self {
            Self::Deposit(d) => engine.persist_deposit((*d).clone()).await,
            Self::Swap(s) => engine.persist_deposit_swap((*s).clone()).await,
        }
    }
}

// ─── Pure helpers ────────────────────────────────────────────────────────

/// Conservative LN sats receivable for a USDC lock budget — the inverse
/// submarine fee model with a 0.5% undershoot, ported verbatim from
/// boltz-web-app deposits:
/// `floor((budget - minerFees) / (rate * (1 + pct)) * 0.995)`.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(crate) fn estimate_submarine_receive_sats(
    lock_budget: u64,
    rate: f64,
    fee_percentage: f64,
    miner_fees: u64,
) -> u64 {
    let rate = if rate > 0.0 { rate } else { 1.0 };
    let pct = fee_percentage / 100.0;
    let Some(net) = lock_budget.checked_sub(miner_fees) else {
        return 0;
    };
    if net == 0 {
        return 0;
    }
    let estimate = (net as f64 / (rate * (1.0 + pct))) * 0.995;
    if estimate <= 0.0 {
        0
    } else {
        estimate.floor() as u64
    }
}

/// Verify a resolver-returned invoice: exact amount, mainnet, and enough
/// remaining lifetime to survive lock -> create -> bind -> pay.
fn verify_invoice(invoice: &str, expected_sats: u64) -> Result<(), BoltzError> {
    let parsed: lightning_invoice::Bolt11Invoice = invoice
        .parse()
        .map_err(|e| BoltzError::Generic(format!("resolver returned unparseable invoice: {e}")))?;
    let msats = parsed
        .amount_milli_satoshis()
        .ok_or_else(|| BoltzError::Generic("resolver invoice has no amount".to_string()))?;
    if msats != expected_sats.saturating_mul(1000) {
        return Err(BoltzError::Generic(format!(
            "resolver invoice amount {msats} msat != requested {expected_sats} sats"
        )));
    }
    if parsed.currency() != lightning_invoice::Currency::Bitcoin {
        return Err(BoltzError::Generic(
            "resolver invoice is not for mainnet Bitcoin".to_string(),
        ));
    }
    let expires_at = parsed
        .duration_since_epoch()
        .saturating_add(parsed.expiry_time())
        .as_secs();
    let min_alive_until = current_unix_timestamp().saturating_add(MIN_INVOICE_EXPIRY_SECS);
    if expires_at < min_alive_until {
        return Err(BoltzError::Generic(format!(
            "resolver invoice expires too soon (at {expires_at}, need >= {min_alive_until})"
        )));
    }
    Ok(())
}

/// The invoice's payment hash — the preimage hash the Commit signature binds.
fn invoice_payment_hash(invoice: &str) -> Result<[u8; 32], BoltzError> {
    let parsed: lightning_invoice::Bolt11Invoice = invoice
        .parse()
        .map_err(|e| BoltzError::Generic(format!("stored invoice unparseable: {e}")))?;
    Ok(*parsed.payment_hash().as_ref())
}

/// Ethereum-standard 65-byte signature hex: `0x || r || s || v`.
fn signature_hex(sig: &EvmSignature) -> String {
    format!(
        "0x{}{}{:02x}",
        hex::encode(sig.r),
        hex::encode(sig.s),
        sig.v
    )
}

/// Parse a 65-byte `0x || r || s || v` signature hex into `(v, r, s)`.
fn parse_signature_hex(signature: &str) -> Result<(u8, [u8; 32], [u8; 32]), BoltzError> {
    let raw = hex::decode(signature.strip_prefix("0x").unwrap_or(signature))
        .map_err(|e| BoltzError::Generic(format!("invalid refund signature hex: {e}")))?;
    if raw.len() != 65 {
        return Err(BoltzError::Generic(format!(
            "refund signature must be 65 bytes, got {}",
            raw.len()
        )));
    }
    let mut r = [0u8; 32];
    let mut s = [0u8; 32];
    r.copy_from_slice(&raw[..32]);
    s.copy_from_slice(&raw[32..64]);
    let mut v = raw[64];
    if v < 27 {
        v = v.saturating_add(27);
    }
    Ok((v, r, s))
}

fn is_success_status(status: &str) -> bool {
    matches!(status, "invoice.settled" | "transaction.claimed")
}

fn is_failure_status(status: &str) -> bool {
    matches!(
        status,
        "swap.expired"
            | "swap.refunded"
            | "swap.waitingForRefund"
            | "invoice.expired"
            | "invoice.failedToPay"
            | "transaction.failed"
            | "transaction.lockupFailed"
            | "transaction.refunded"
    )
}

/// Left-pad an EVM address into the CCTP `bytes32` recipient form.
fn evm_address_bytes32(address: &str) -> Result<[u8; 32], BoltzError> {
    let addr = parse_address(address)?;
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(addr.as_slice());
    Ok(out)
}

fn hex_calldata(data: &[u8]) -> String {
    format!("0x{}", hex::encode(data))
}

fn parse_bytes32(value: &str) -> Result<[u8; 32], BoltzError> {
    let raw = hex::decode(value.strip_prefix("0x").unwrap_or(value))
        .map_err(|e| BoltzError::Generic(format!("invalid bytes32 hex: {e}")))?;
    raw.try_into()
        .map_err(|_| BoltzError::Generic("bytes32 must be exactly 32 bytes".to_string()))
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;

    #[macros::test_all]
    fn sizing_matches_reference_formula() {
        // rate 1, 0.1% fee, 1000 sats miner fee, 1_000_000 budget:
        // floor((999000 / 1.001) * 0.995) = floor(993_011.988…) — matches
        // the JS reference Math.floor((999000/(1*(1+0.001)))*0.995).
        assert_eq!(
            estimate_submarine_receive_sats(1_000_000, 1.0, 0.1, 1000),
            993_011
        );
        // Budget below miner fees -> 0.
        assert_eq!(estimate_submarine_receive_sats(500, 1.0, 0.1, 1000), 0);
        // Zero/negative rate falls back to 1 (reference behavior).
        assert_eq!(
            estimate_submarine_receive_sats(1_000_000, 0.0, 0.1, 1000),
            estimate_submarine_receive_sats(1_000_000, 1.0, 0.1, 1000)
        );
        // Exact-zero net -> 0.
        assert_eq!(estimate_submarine_receive_sats(1000, 1.0, 0.1, 1000), 0);
    }

    #[macros::test_all]
    fn signature_hex_round_trips() {
        let sig = EvmSignature {
            v: 28,
            r: [0x11; 32],
            s: [0x22; 32],
        };
        let hex_sig = signature_hex(&sig);
        assert_eq!(hex_sig.len(), 2 + 130);
        let (v, r, s) = parse_signature_hex(&hex_sig).unwrap();
        assert_eq!((v, r, s), (28, [0x11; 32], [0x22; 32]));

        // EIP-2098-style v=1 normalizes to 28.
        let mut raw = vec![0x11; 32];
        raw.extend_from_slice(&[0x22; 32]);
        raw.push(1);
        let (v, _, _) = parse_signature_hex(&format!("0x{}", hex::encode(raw))).unwrap();
        assert_eq!(v, 28);

        assert!(parse_signature_hex("0xdeadbeef").is_err());
    }

    #[macros::test_all]
    fn status_classification_matches_reference_sets() {
        for s in ["invoice.settled", "transaction.claimed"] {
            assert!(is_success_status(s));
            assert!(!is_failure_status(s));
        }
        for s in [
            "swap.expired",
            "swap.refunded",
            "swap.waitingForRefund",
            "invoice.expired",
            "invoice.failedToPay",
            "transaction.failed",
            "transaction.lockupFailed",
            "transaction.refunded",
        ] {
            assert!(is_failure_status(s));
            assert!(!is_success_status(s));
        }
        for s in [
            "swap.created",
            "invoice.set",
            "invoice.pending",
            "transaction.mempool",
        ] {
            assert!(!is_success_status(s));
            assert!(!is_failure_status(s));
        }
    }

    #[macros::test_all]
    fn evm_address_bytes32_left_pads() {
        let out = evm_address_bytes32("0x9858EfFD232B4033E47d90003D41EC34EcaEda94").unwrap();
        assert_eq!(&out[..12], &[0u8; 12]);
        assert_eq!(
            hex::encode(&out[12..]),
            "9858effd232b4033e47d90003d41ec34ecaeda94"
        );
    }

    #[macros::test_all]
    fn invoice_verification_rejects_bad_amounts() {
        // A syntactically valid mainnet invoice with a KNOWN amount would
        // need a signed fixture; unparseable input must error cleanly.
        assert!(verify_invoice("lnbc1notaninvoice", 1000).is_err());
        assert!(invoice_payment_hash("garbage").is_err());
    }
}
