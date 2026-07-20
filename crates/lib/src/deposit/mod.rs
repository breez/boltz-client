//! Inbound stablecoin deposits: a reusable EVM deposit address receives USDC
//! on a source chain, the funds are CCTP-bridged to Arbitrum, locked as an
//! `ERC20Swap` commitment, bound to a Boltz submarine swap, and paid out over
//! Lightning to an integrator-supplied invoice.

use crate::error::BoltzError;

pub(crate) mod detect;
// dead_code: wired into BoltzService by the service-integration change (in
// progress on this branch).
#[expect(dead_code)]
pub(crate) mod engine;
#[expect(dead_code)]
pub(crate) mod manager;
pub mod models;
pub(crate) mod schedule;
pub(crate) mod sends;

/// Everything the engine knows when it asks for an invoice.
#[derive(Debug, Clone)]
pub struct InvoiceRequest {
    /// The lock unit this invoice will settle.
    pub deposit_swap_id: String,
    /// EXACT invoice amount required — a returned invoice with any other
    /// amount is rejected and re-requested.
    pub amount_sats: u64,
    /// USDC that will be locked for the swap (6 decimals) — what the swap
    /// spends; `amount_sats` is what the recipient receives after Boltz fees.
    pub lock_amount: u64,
}

/// Integrator-supplied invoice source. A **mechanical fetch**, not a decision
/// point: the crate sizes the amount and auto-proceeds on its own guards.
///
/// Errors always mean "transient" — the engine retries on its tick cadence
/// while the funds sit minted (and safe) at the deposit address; there is no
/// rejection semantic. The callback may fire more than once for the same
/// deposit (retries, or concurrent service instances racing to bind — only
/// one resulting swap settles, the losers' invoices simply expire unpaid), so
/// implementations must tolerate repeat calls.
#[macros::async_trait]
pub trait DepositInvoiceResolver: Send + Sync {
    /// Return a BOLT11 invoice for exactly `request.amount_sats`.
    async fn resolve_invoice(&self, request: &InvoiceRequest) -> Result<String, BoltzError>;
}
