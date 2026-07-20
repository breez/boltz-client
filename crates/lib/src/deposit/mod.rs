//! Inbound stablecoin deposits: a reusable EVM deposit address receives USDC
//! on a source chain, the funds are CCTP-bridged to Arbitrum, locked as an
//! `ERC20Swap` commitment, bound to a Boltz submarine swap, and paid out over
//! Lightning to an integrator-supplied invoice.

// dead_code: consumed by the deposit manager (in progress on this branch).
#[cfg_attr(not(test), expect(dead_code))]
pub(crate) mod detect;
pub mod models;
// dead_code: consumed by the deposit manager (in progress on this branch).
#[cfg_attr(not(test), expect(dead_code))]
pub(crate) mod schedule;
#[cfg_attr(not(test), expect(dead_code))]
pub(crate) mod sends;
