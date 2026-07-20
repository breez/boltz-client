use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A single on-chain USDC inflow to the deposit address.
///
/// Identity is the transfer log's `{chain_id}:{tx_hash}:{log_index}` — never
/// the address balance, because the reusable address commingles inflows. An
/// inflow's lifecycle ends at [`DepositStatus::Consumed`], when a
/// [`DepositSwap`] takes ownership of its minted funds; a cooperative refund
/// later produces a brand-new inflow (the refund transfer has its own
/// identity), never resurrects this one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deposit {
    /// `"{chain_id}:{tx_hash}:{log_index}"`, lowercased.
    pub id: String,
    pub status: DepositStatus,
    /// Source chain (a `DEPOSIT_SOURCE_CHAINS` entry). Arbitrum marks a
    /// local inflow (refund return / direct deposit) whose bridge leg is
    /// skipped entirely.
    pub chain_id: u64,
    pub tx_hash: String,
    pub log_index: u64,
    /// Block the transfer mined in — the confirmation-depth anchor and the
    /// lower bound for burn-schedule rescans.
    pub block_number: u64,
    /// USDC received on the source chain (6 decimals).
    pub amount: u64,
    /// The deposit address the funds landed on.
    pub deposit_address: String,

    // Bridge leg (source chain -> Arbitrum). All `None` for Arbitrum-local
    // inflows.
    /// In-flight sponsored burn anchor — persisted BEFORE broadcast.
    pub pending_send: Option<PendingSend>,
    pub burn_tx_hash: Option<String>,
    /// Circle message nonce (`0x…` bytes32 hex) from the burn's
    /// `MessageSent`, once attested — the `usedNonces` idempotency key for a
    /// manual mint.
    pub cctp_nonce: Option<String>,
    /// Unix-seconds deadline to wait for Circle's forwarder before
    /// self-submitting `receiveMessage`. Persisted once so a resume never
    /// resets the clock.
    pub mint_deadline: Option<u64>,
    /// USDC actually minted on Arbitrum, net of bridge fees (6 decimals).
    /// For Arbitrum-local inflows this equals [`amount`](Self::amount).
    pub minted_amount: Option<u64>,
    /// The [`DepositSwap`] that consumed this inflow.
    pub deposit_swap_id: Option<String>,

    // Timestamps (unix seconds)
    pub created_at: u64,
    pub updated_at: u64,
}

impl Deposit {
    /// Canonical inflow identity.
    pub fn make_id(chain_id: u64, tx_hash: &str, log_index: u64) -> String {
        format!("{chain_id}:{}:{log_index}", tx_hash.to_lowercase())
    }

    /// Whether this inflow needs no further driving.
    pub fn is_terminal(&self) -> bool {
        matches!(self.status, DepositStatus::Consumed)
    }
}

/// Inflow lifecycle. The scanner only records transfers already at
/// confirmation depth, so there is no pre-confirmation state.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum DepositStatus {
    /// Confirmed on the source chain, not yet acted on.
    Detected,
    /// Deliberately not progressed; funds sit at the deposit address.
    /// Re-enters via an explicit retry (`retry_parked`).
    Parked { reason: ParkReason },
    /// Sponsored CCTP burn submitted.
    Bridging,
    /// Burn mined; waiting for the Arbitrum mint (forwarder or manual).
    AwaitingMint,
    /// Funds on Arbitrum with the exact minted amount known — eligible for
    /// a lock.
    Minted,
    /// Owned by a [`DepositSwap`] (terminal for the inflow).
    Consumed,
}

/// Why an inflow was parked instead of progressed.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ParkReason {
    /// Too small to cover the buffered CCTP burn fee — bridging would
    /// revert on-chain (`maxFee >= amount`).
    BelowBridgeFee,
    /// Below the Boltz pair minimum; waiting to be aggregated with other
    /// parked inflows into one lock.
    BelowPairLimit,
    /// A cooperative-refund return landing back at the deposit address on
    /// Arbitrum. Never auto-retried — a failed flow re-entering by itself
    /// would loop (fail -> refund -> detect -> retry); it waits for an
    /// explicit `retry_parked`.
    RefundReturned,
}

/// Anchor for an in-flight sponsored send, persisted BEFORE broadcast so a
/// crash can never lose track of a possibly-landed send. Recovery re-polls
/// `call_id` for the tx hash and/or re-derives the schedule from chain logs
/// starting at `from_block`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingSend {
    pub chain_id: u64,
    /// Chain height just before submission — the rescan lower bound.
    pub from_block: u64,
    /// Gas-sponsor call id, set right after submission returns (the
    /// `pending_call_id` resume pattern).
    pub call_id: Option<String>,
    pub created_at: u64,
}

/// One lock unit: an `ERC20Swap` commitment plus the Boltz submarine swap it
/// gets bound to, consuming one or more minted inflows. N:1 (`deposit_ids`)
/// exists for aggregated recovery of parked funds; the normal path is 1:1.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DepositSwap {
    /// Deterministic: derived from the sorted consumed inflow ids (see
    /// [`Self::derive_id`]), so concurrent instances create the SAME record
    /// for the same inflows and last-writer-wins sync collapses them
    /// instead of forking. Inflow ids are single-use (`Consumed`), so the
    /// id can never legitimately recur after a refund.
    pub id: String,
    pub status: DepositSwapStatus,
    pub deposit_ids: Vec<String>,
    /// USDC to lock (6 decimals) — the summed minted amounts.
    pub amount: u64,
    /// The deposit address: burn recipient, lock `refundAddress`, and
    /// `Commit`/refund-authorization signer.
    pub deposit_address: String,

    // Lock parameters, snapshotted from `GET /v2/commitment/{cur}/details`
    // at lock time — enough to rebuild `hashValues`, the bind signature,
    // and the cooperative-refund call after a restart.
    pub erc20swap_address: Option<String>,
    /// Boltz's claim address for commitments.
    pub claim_address: Option<String>,
    pub timelock: Option<u64>,
    /// In-flight sponsored lock/refund anchor — persisted BEFORE broadcast.
    pub pending_send: Option<PendingSend>,
    pub commitment_tx_hash: Option<String>,
    pub commitment_log_index: Option<u64>,

    // Out-swap (Boltz submarine, USDC -> BTC Lightning)
    /// Integrator-resolved BOLT11 (verified: amount, network, expiry).
    pub invoice: Option<String>,
    pub invoice_amount_sats: Option<u64>,
    /// Boltz swap id, once created.
    pub swap_id: Option<String>,
    /// Boltz's `expectedAmount` for the submarine swap (must be <= locked).
    pub expected_amount: Option<u64>,
    /// The bind signature was accepted by (or already existed at) the
    /// server — never re-post after this is set.
    pub bound: bool,
    pub refund_tx_hash: Option<String>,

    // Timestamps (unix seconds)
    pub created_at: u64,
    pub updated_at: u64,
}

impl DepositSwap {
    /// Deterministic id over the consumed inflow set: `"ds-"` + first 16
    /// bytes (hex) of `SHA256(sorted ids joined by '\n')`.
    pub fn derive_id(deposit_ids: &[String]) -> String {
        let mut sorted: Vec<&str> = deposit_ids.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        let digest = Sha256::digest(sorted.join("\n").as_bytes());
        format!("ds-{}", hex::encode(&digest[..16]))
    }
}

/// Lock-unit lifecycle. Poll-driven (no WS ordering hazard), each phase
/// guarded by its own output field, so declaration order carries no
/// load-bearing `Ord` like `BoltzSwapStatus` does.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum DepositSwapStatus {
    /// Resolving the integrator invoice (pre-lock; transient failures
    /// retry — funds sit minted and safe).
    Resolving,
    /// Sponsored `ERC20Swap.lock` in flight.
    Locking,
    /// Creating the Boltz submarine swap.
    Creating,
    /// Posting the EIP-712 `Commit` signature.
    Binding,
    /// Bound; Boltz pays the invoice — polling swap status.
    Settling,
    /// Invoice paid; swap complete.
    Done,
    /// Cooperative refund in flight (business failure after lock).
    Refunding,
    /// Terminal failure; refunded funds re-enter as a fresh parked inflow.
    Failed { reason: String },
}

impl DepositSwapStatus {
    /// Whether this status is terminal (no further transitions expected).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed { .. })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;

    #[macros::test_all]
    fn deposit_id_is_canonical() {
        assert_eq!(
            Deposit::make_id(137, "0xABCDEF", 3),
            "137:0xabcdef:3".to_string()
        );
    }

    #[macros::test_all]
    fn deposit_swap_id_is_order_independent_and_prefixed() {
        let a = ["1:0xa:0".to_string(), "137:0xb:2".to_string()];
        let b = ["137:0xb:2".to_string(), "1:0xa:0".to_string()];
        let id_a = DepositSwap::derive_id(&a);
        let id_b = DepositSwap::derive_id(&b);
        assert_eq!(id_a, id_b);
        assert!(id_a.starts_with("ds-"));
        assert_eq!(id_a.len(), 3 + 32);

        // A different set yields a different id.
        let c = ["1:0xa:0".to_string()];
        assert_ne!(DepositSwap::derive_id(&c), id_a);
    }

    #[macros::test_all]
    fn status_terminality() {
        assert!(DepositSwapStatus::Done.is_terminal());
        assert!(
            DepositSwapStatus::Failed {
                reason: "r".to_string()
            }
            .is_terminal()
        );
        assert!(!DepositSwapStatus::Refunding.is_terminal());
        assert!(!DepositSwapStatus::Resolving.is_terminal());
    }
}
