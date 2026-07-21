//! Inbound stablecoin deposits: a reusable EVM deposit address receives USDC
//! on a source chain, the funds are CCTP-bridged to Arbitrum, locked as an
//! `ERC20Swap` commitment, bound to a Boltz submarine swap, and paid out over
//! Lightning to an integrator-supplied invoice.

use crate::error::BoltzError;

pub(crate) mod detect;
pub(crate) mod engine;
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

/// Integrator's answer to an invoice request.
#[derive(Debug, Clone)]
pub enum InvoiceResolution {
    /// A BOLT11 invoice for exactly the requested amount — proceed.
    Invoice(String),
    /// The receiver does not accept the current terms. The lock unit parks
    /// (nothing is locked yet, so declining is free — no refund round-trip)
    /// and re-enters, re-sized at then-current numbers, on the next
    /// `retry_parked`.
    Decline,
}

/// Integrator-supplied invoice source — and the receiver's one decision
/// point. Deposits arrive unsolicited, so this fires with the exact terms
/// (`amount_sats` receivable for `lock_amount` locked): the crate has no
/// market oracle and does NOT judge the implied rate, so an integrator that
/// cares should compare against its own price feed here and [`Decline`]
/// (park until an explicit retry) or simply error (safe stall, retried next
/// tick — funds sit minted at the deposit address either way).
///
/// [`Decline`]: InvoiceResolution::Decline
///
/// Errors always mean "transient". The callback may fire more than once for
/// the same deposit (retries, park/resume cycles, or concurrent service
/// instances racing to bind — only one resulting swap settles, the losers'
/// invoices simply expire unpaid), so implementations must tolerate repeat
/// calls.
#[macros::async_trait]
pub trait DepositInvoiceResolver: Send + Sync {
    /// Answer a request for an invoice of exactly `request.amount_sats`.
    async fn resolve_invoice(
        &self,
        request: &InvoiceRequest,
    ) -> Result<InvoiceResolution, BoltzError>;
}
