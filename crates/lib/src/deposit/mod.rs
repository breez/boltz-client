//! Inbound stablecoin deposits: a reusable EVM deposit address receives USDC
//! on a source chain, the funds are CCTP-bridged to Arbitrum, locked as an
//! `ERC20Swap` commitment, bound to a Boltz submarine swap, and paid out over
//! Lightning to an integrator-supplied invoice.

pub mod models;
