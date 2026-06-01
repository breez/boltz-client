//! On-chain liveness checks for `ERC20Swap` lockups.
//!
//! These are used by the live claim path to verify funds are actually locked
//! before revealing the preimage (anti-fraud guard), and by the resume/retry
//! logic to decide whether a claim is still worth attempting.

use alloy_primitives::U256;

use crate::config::{ARBITRUM_TBTC_ADDRESS, SATS_TO_TBTC_FACTOR};
use crate::error::BoltzError;
use crate::evm::contracts::{
    DecodedLockupEvent, decode_hash_values_return, decode_swaps_check_return, encode_hash_values,
    encode_swaps_check, parse_address,
};
use crate::evm::provider::EvmProvider;
use crate::keys::EvmKeyManager;
use crate::models::BoltzSwap;

/// Check whether a swap is still locked on-chain (not yet claimed/refunded).
pub async fn is_swap_still_locked(
    evm_provider: &EvmProvider,
    erc20swap_address: &str,
    event: &DecodedLockupEvent,
) -> Result<bool, BoltzError> {
    let hash_calldata = encode_hash_values(
        event.preimage_hash,
        event.amount,
        event.token_address,
        event.claim_address,
        event.refund_address,
        event.timelock,
    );
    let hash_result = evm_provider
        .eth_call(erc20swap_address, &hash_calldata)
        .await?;
    let swap_hash = decode_hash_values_return(&hash_result)?;

    let check_calldata = encode_swaps_check(swap_hash);
    let check_result = evm_provider
        .eth_call(erc20swap_address, &check_calldata)
        .await?;
    decode_swaps_check_return(&check_result)
}

/// Convenience wrapper: check whether a persisted swap's funds are still
/// locked on the `ERC20Swap` contract. Returns `true` if claimable, `false`
/// if already claimed or refunded.
pub async fn is_swap_still_locked_by_swap(
    evm_provider: &EvmProvider,
    swap: &BoltzSwap,
    key_manager: &EvmKeyManager,
) -> Result<bool, BoltzError> {
    let chain_id_u32: u32 = swap
        .chain_id
        .try_into()
        .map_err(|_| BoltzError::Generic("Chain ID overflow".into()))?;
    let preimage_hash = key_manager.derive_preimage_hash(chain_id_u32, swap.claim_key_index)?;
    let tbtc_evm_amount = U256::from(swap.onchain_amount)
        .checked_mul(U256::from(SATS_TO_TBTC_FACTOR))
        .ok_or_else(|| BoltzError::Generic("tBTC EVM amount overflow".into()))?;

    let event = DecodedLockupEvent {
        preimage_hash,
        amount: tbtc_evm_amount,
        token_address: parse_address(ARBITRUM_TBTC_ADDRESS)?,
        claim_address: parse_address(&swap.claim_address)?,
        refund_address: parse_address(&swap.refund_address)?,
        timelock: U256::from(swap.timeout_block_height),
    };

    is_swap_still_locked(evm_provider, &swap.erc20swap_address, &event).await
}
