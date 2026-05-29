//! Circle CCTP v2 helpers: `mintRecipient` encoding, forwarding-service
//! `hookData`, and burn-fee math.
//!
//! Mirrors boltz-web-app `boltz-swaps` `cctp/evm.ts` + the fee math in
//! `CctpBridgeDriver`. The Arbitrum -> destination burn is driven by the
//! Router's `claimERC20ExecuteCctp`; these helpers build the `CctpData`
//! field values for it.

use alloy_primitives::FixedBytes;

use crate::config::{CCTP_FEE_BPS_DENOMINATOR, CCTP_MAX_FEE_BUFFER_BPS, SOLANA_USDC_MINT};
use crate::error::BoltzError;
use crate::evm::contracts::{address_to_bytes32, parse_address};
use crate::solana::ata::derive_ata;

/// Basis-points denominator for the `maxFee` buffer.
const BPS_DENOMINATOR: u128 = 10_000;

/// Length of the EVM forwarding-service prefix reused by the Solana hookData:
/// the first 29 bytes of [`CCTP_FORWARD_HOOK_DATA_HEX`] (the `"cctp-forward"`
/// tag plus padding), matching the web app's `cctpForwardHookData.slice(0,58)`.
const SOLANA_FORWARD_PREFIX_LEN: usize = 29;

/// Decode a base58 Solana pubkey into its 32-byte form.
fn decode_solana_pubkey(s: &str) -> Result<[u8; 32], BoltzError> {
    let decoded = bs58::decode(s)
        .into_vec()
        .map_err(|e| BoltzError::Generic(format!("Invalid Solana pubkey '{s}': {e}")))?;
    let arr: [u8; 32] = decoded.try_into().map_err(|v: Vec<u8>| {
        BoltzError::Generic(format!(
            "Solana pubkey '{s}' must decode to 32 bytes, got {}",
            v.len()
        ))
    })?;
    Ok(arr)
}

/// CCTP `mintRecipient` for an EVM destination: the 20-byte recipient address
/// left-padded to `bytes32`.
pub fn evm_mint_recipient(address: &str) -> Result<FixedBytes<32>, BoltzError> {
    Ok(address_to_bytes32(parse_address(address)?))
}

/// CCTP `mintRecipient` for a Solana destination: the recipient's USDC
/// Associated Token Account (32-byte pubkey). CCTP mints into a token account,
/// so the recipient must be the ATA, not the wallet; the wallet rides in the
/// Solana forwarding `hookData` so Circle's forwarder can create the ATA.
pub fn solana_mint_recipient(recipient_wallet: &str) -> Result<FixedBytes<32>, BoltzError> {
    let owner = decode_solana_pubkey(recipient_wallet)?;
    let mint = decode_solana_pubkey(SOLANA_USDC_MINT)?;
    let ata = derive_ata(&owner, &mint)?;
    Ok(FixedBytes::from(ata))
}

/// EVM forwarding-service `hookData`: the ASCII tag `"cctp-forward"` (12 bytes)
/// right-padded to 32 bytes. Built directly (rather than decoding the hex
/// const) to avoid a fallible decode in a hot path; a unit test pins it equal
/// to [`CCTP_FORWARD_HOOK_DATA_HEX`].
#[must_use]
pub fn evm_forward_hook_data() -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..12].copy_from_slice(b"cctp-forward");
    out
}

/// Solana forwarding-service `hookData`: the EVM forward prefix (29 bytes) +
/// payload length `0x00000021` (33) + ATA-creation flag `0x01` + the 32-byte
/// recipient wallet pubkey. Mirrors the web app's
/// `createCctpSolanaForwardHookData`. Used when the recipient's USDC ATA does
/// not yet exist so Circle's forwarder creates it on delivery.
pub fn solana_forward_hook_data(recipient_wallet: &str) -> Result<Vec<u8>, BoltzError> {
    let wallet = decode_solana_pubkey(recipient_wallet)?;
    let prefix = evm_forward_hook_data();

    let mut out = Vec::with_capacity(SOLANA_FORWARD_PREFIX_LEN + 4 + 1 + 32);
    out.extend_from_slice(&prefix[..SOLANA_FORWARD_PREFIX_LEN]);
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x21]); // length = 0x21 = 33
    out.push(0x01); // ATA-creation flag
    out.extend_from_slice(&wallet);
    Ok(out)
}

/// Total CCTP burn fee deducted from the burned amount: the protocol fee
/// (`amount * bpsUnits / CCTP_FEE_BPS_DENOMINATOR`) plus the flat forwarding
/// fee. `bps_units` is Circle's `minimumFee` scaled by `CCTP_FEE_SCALE`.
#[must_use]
pub fn compute_total_fee(amount: u128, bps_units: u128, forward_fee: u128) -> u128 {
    amount
        .saturating_mul(bps_units)
        .checked_div(CCTP_FEE_BPS_DENOMINATOR)
        .unwrap_or(0)
        .saturating_add(forward_fee)
}

/// Add the `maxFee` cushion (`CCTP_MAX_FEE_BUFFER_BPS`) on top of the quoted
/// fee, rounding up, so fee movement between prepare and claim doesn't make the
/// burn revert. This is a fee cap, NOT user slippage.
#[must_use]
pub fn add_fee_buffer(amount: u128) -> u128 {
    let numerator =
        amount.saturating_mul(BPS_DENOMINATOR.saturating_add(u128::from(CCTP_MAX_FEE_BUFFER_BPS)));
    // Ceiling division: (numerator + denom - 1) / denom.
    numerator
        .saturating_add(BPS_DENOMINATOR.saturating_sub(1))
        .checked_div(BPS_DENOMINATOR)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;
    use crate::config::CCTP_FORWARD_HOOK_DATA_HEX;

    #[macros::test_all]
    fn evm_forward_hook_matches_config_hex() {
        let expected = hex::decode(CCTP_FORWARD_HOOK_DATA_HEX).unwrap();
        assert_eq!(evm_forward_hook_data().to_vec(), expected);
    }

    #[macros::test_all]
    fn evm_mint_recipient_left_pads() {
        let r = evm_mint_recipient("0x1234567890AbCdEf1234567890aBcDeF12345678").unwrap();
        assert_eq!(&r[..12], &[0u8; 12]);
        assert_eq!(
            &r[12..],
            &hex::decode("1234567890abcdef1234567890abcdef12345678").unwrap()[..]
        );
    }

    #[macros::test_all]
    fn evm_mint_recipient_rejects_garbage() {
        assert!(evm_mint_recipient("not-an-address").is_err());
    }

    #[macros::test_all]
    fn solana_mint_recipient_is_recipient_ata() {
        // Arbitrary valid Solana wallet pubkey.
        let wallet = "BZkwksSEeHrCVS3HeewBJKEBTEEuwnEqpkHqEg1dRpuE";
        let r = solana_mint_recipient(wallet).unwrap();
        // The ATA is derived (off-curve), so it must differ from the wallet itself.
        let wallet_bytes = bs58::decode(wallet).into_vec().unwrap();
        assert_ne!(r.as_slice(), &wallet_bytes[..]);
        // Deterministic.
        assert_eq!(r, solana_mint_recipient(wallet).unwrap());
    }

    #[macros::test_all]
    fn solana_forward_hook_structure() {
        let wallet = "BZkwksSEeHrCVS3HeewBJKEBTEEuwnEqpkHqEg1dRpuE";
        let hook = solana_forward_hook_data(wallet).unwrap();

        assert_eq!(hook.len(), 29 + 4 + 1 + 32);
        // Prefix is "cctp-forward" + padding (first 29 bytes of the EVM hook).
        assert_eq!(&hook[..12], b"cctp-forward");
        assert_eq!(&hook[12..29], &[0u8; 17]);
        // length = 0x00000021, flag = 0x01.
        assert_eq!(&hook[29..33], &[0x00, 0x00, 0x00, 0x21]);
        assert_eq!(hook[33], 0x01);
        // Trailing 32 bytes = the wallet pubkey.
        let wallet_bytes = bs58::decode(wallet).into_vec().unwrap();
        assert_eq!(&hook[34..], &wallet_bytes[..]);
    }

    #[macros::test_all]
    fn solana_forward_hook_rejects_bad_wallet() {
        assert!(solana_forward_hook_data("0xnot-base58").is_err());
    }

    #[macros::test_all]
    fn compute_total_fee_protocol_plus_forward() {
        // bps_units = minimumFee (in bps) scaled by CCTP_FEE_SCALE (10^9).
        // So `1` bps -> 1 * 10^9. Protocol fee = amount * bps_units / denom
        // = amount * bps / 10_000. On 1_000_000 units at 1 bps that is 100.
        // Plus a flat forward fee of 7 -> 107.
        let one_bps_scaled = 1_000_000_000u128;
        let fee = compute_total_fee(1_000_000, one_bps_scaled, 7);
        assert_eq!(fee, 107);
    }

    #[macros::test_all]
    fn compute_total_fee_zero_protocol() {
        assert_eq!(compute_total_fee(0, 1_000_000_000, 5), 5);
    }

    #[macros::test_all]
    fn add_fee_buffer_rounds_up() {
        // 2 bps buffer on 10_000 = exactly 10_002.
        assert_eq!(add_fee_buffer(10_000), 10_002);
        // On a value where the buffer is fractional, it rounds up.
        // 1 * (10002) / 10000 = 1.0002 -> ceil -> 2.
        assert_eq!(add_fee_buffer(1), 2);
        assert_eq!(add_fee_buffer(0), 0);
    }
}
