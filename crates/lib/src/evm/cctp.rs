//! Circle CCTP v2 helpers: `mintRecipient` encoding, forwarding-service
//! `hookData`, and burn-fee math.
//!
//! Mirrors boltz-web-app `boltz-swaps` `cctp/evm.ts` + the fee math in
//! `CctpBridgeDriver`. The Arbitrum -> destination burn is driven by the
//! Router's `claimERC20ExecuteCctp`; these helpers build the `CctpData`
//! field values for it.

use std::collections::HashMap;

use alloy_primitives::FixedBytes;
use serde::Deserialize;

use platform_utils::http::HttpClient;

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

/// Invert the burn fee: the USDC amount the DEX must produce so that, after
/// the CCTP fee is deducted from the burn, at least `target` arrives on the
/// destination. Solves `burn * (denom - bps_units) / denom - forward_fee >=
/// target` for `burn`, rounding up. Used at prepare time to size the swap.
#[must_use]
pub fn cctp_required_burn(target: u128, fee: &CctpFee) -> u128 {
    let net = CCTP_FEE_BPS_DENOMINATOR.saturating_sub(fee.bps_units);
    if net == 0 {
        return u128::MAX;
    }
    let numerator = target
        .saturating_add(fee.forward_fee)
        .saturating_mul(CCTP_FEE_BPS_DENOMINATOR);
    // Ceiling division so we never under-size the burn.
    numerator
        .saturating_add(net.saturating_sub(1))
        .checked_div(net)
        .unwrap_or(u128::MAX)
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

/// Circle CCTP burn fee for one route/finality, resolved from the Iris API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CctpFee {
    /// Protocol fee in basis points, scaled by [`CCTP_FEE_SCALE`].
    pub bps_units: u128,
    /// Flat forwarding-service fee, in the burn token's smallest units.
    pub forward_fee: u128,
}

/// Parse a decimal value (number or numeric string) into an integer scaled by
/// `10^scale_digits`, truncating any fraction beyond `scale_digits`. Used for
/// Circle's `minimumFee` (bps, scaled by 9) and `forwardFee` (scale 0).
fn parse_scaled(value: &serde_json::Value, scale_digits: u32) -> Result<u128, BoltzError> {
    let s = match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        other => {
            return Err(BoltzError::Generic(format!(
                "invalid CCTP fee value: {other}"
            )));
        }
    };

    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s.as_str(), ""),
    };

    let parse_digits = |d: &str| -> Result<u128, BoltzError> {
        if d.is_empty() {
            return Ok(0);
        }
        d.parse::<u128>()
            .map_err(|_| BoltzError::Generic(format!("invalid CCTP fee number '{s}'")))
    };

    let scale = scale_digits as usize;
    // Truncate or right-pad the fractional part to exactly `scale` digits.
    let mut frac = String::with_capacity(scale);
    frac.push_str(frac_part.get(..scale.min(frac_part.len())).unwrap_or(""));
    while frac.len() < scale {
        frac.push('0');
    }

    let int_scaled = parse_digits(int_part)?
        .checked_mul(10u128.pow(scale_digits))
        .ok_or_else(|| BoltzError::Generic("CCTP fee overflow".into()))?;
    let frac_scaled = if frac.is_empty() {
        0
    } else {
        parse_digits(&frac)?
    };

    int_scaled
        .checked_add(frac_scaled)
        .ok_or_else(|| BoltzError::Generic("CCTP fee overflow".into()))
}

/// Client for Circle's Iris CCTP fee API.
pub struct CctpFeeClient {
    http: Box<dyn HttpClient>,
    api_url: String,
}

impl CctpFeeClient {
    pub fn new(http: Box<dyn HttpClient>, api_url: String) -> Self {
        // Normalize: drop a single trailing slash so path joins are clean.
        let api_url = match api_url.strip_suffix('/') {
            Some(trimmed) => trimmed.to_string(),
            None => api_url,
        };
        Self { http, api_url }
    }

    /// Fetch the burn fee for `source_domain -> dest_domain` at the given
    /// `finality_threshold` (Fast=1000 / Standard=2000), in Forwarded mode.
    pub async fn get_fee(
        &self,
        source_domain: u32,
        dest_domain: u32,
        finality_threshold: u32,
    ) -> Result<CctpFee, BoltzError> {
        let url = format!(
            "{}/v2/burn/USDC/fees/{source_domain}/{dest_domain}?forward=true",
            self.api_url
        );
        let mut headers = HashMap::new();
        headers.insert("Accept".to_string(), "application/json".to_string());

        let response = self
            .http
            .get(url, Some(headers))
            .await
            .map_err(|e| BoltzError::Generic(format!("CCTP fee request failed: {e}")))?;

        if !response.is_success() {
            return Err(BoltzError::Generic(format!(
                "CCTP fee HTTP error {}: {}",
                response.status, response.body
            )));
        }

        Self::parse_fee_response(&response.body, finality_threshold)
    }

    /// Parse the Iris fee response body and extract the fee for
    /// `finality_threshold`. Split out for testing against recorded JSON.
    fn parse_fee_response(body: &str, finality_threshold: u32) -> Result<CctpFee, BoltzError> {
        let entries: Vec<CctpFeeEntry> = serde_json::from_str(body).map_err(|e| {
            BoltzError::Generic(format!(
                "failed to parse CCTP fee response: {e} (body: {body})"
            ))
        })?;

        let entry = entries
            .into_iter()
            .find(|e| e.finality_threshold == finality_threshold)
            .ok_or_else(|| {
                BoltzError::Generic(format!(
                    "missing CCTP fee for finality threshold {finality_threshold}"
                ))
            })?;

        let bps_units = parse_scaled(&entry.minimum_fee, CCTP_FEE_SCALE_DIGITS)?;
        let forward_fee = match entry.forward_fee {
            Some(f) => parse_scaled(&f.med, 0)?,
            None => {
                return Err(BoltzError::Generic(format!(
                    "missing CCTP forward fee for finality threshold {finality_threshold}"
                )));
            }
        };

        Ok(CctpFee {
            bps_units,
            forward_fee,
        })
    }
}

/// Scale (in decimal digits) Circle applies to `minimumFee`. `CCTP_FEE_SCALE`
/// is `10^CCTP_FEE_SCALE_DIGITS`.
const CCTP_FEE_SCALE_DIGITS: u32 = 9;

#[derive(Deserialize)]
struct CctpFeeEntry {
    #[serde(rename = "finalityThreshold")]
    finality_threshold: u32,
    #[serde(rename = "minimumFee")]
    minimum_fee: serde_json::Value,
    #[serde(rename = "forwardFee")]
    forward_fee: Option<CctpForwardFee>,
}

#[derive(Deserialize)]
struct CctpForwardFee {
    med: serde_json::Value,
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;
    use crate::config::{CCTP_FEE_SCALE, CCTP_FORWARD_HOOK_DATA_HEX};

    // Sanity: the scale constant and digit count agree.
    #[macros::test_all]
    fn fee_scale_matches_digits() {
        assert_eq!(CCTP_FEE_SCALE, 10u128.pow(CCTP_FEE_SCALE_DIGITS));
    }

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

    #[macros::test_all]
    fn cctp_required_burn_covers_target_plus_fee() {
        // 1 bps protocol + flat forward fee of 50, target 1_000_000.
        let fee = CctpFee {
            bps_units: 1_000_000_000, // 1 bps scaled
            forward_fee: 50,
        };
        let burn = cctp_required_burn(1_000_000, &fee);
        // Burning `burn` and deducting the fee must leave >= target.
        let actual_fee = compute_total_fee(burn, fee.bps_units, fee.forward_fee);
        assert!(burn.saturating_sub(actual_fee) >= 1_000_000);
        // ...and not be wildly over-sized (within fee + 1 of target).
        assert!(burn <= 1_000_000 + actual_fee + 1);
    }

    #[macros::test_all]
    fn cctp_required_burn_zero_fee_is_target() {
        let fee = CctpFee {
            bps_units: 0,
            forward_fee: 0,
        };
        assert_eq!(cctp_required_burn(1_000_000, &fee), 1_000_000);
    }

    #[macros::test_all]
    fn parse_scaled_handles_integers_and_decimals() {
        use serde_json::json;
        // Integer bps "1" scaled by 9.
        assert_eq!(parse_scaled(&json!("1"), 9).unwrap(), 1_000_000_000);
        // Numeric (not string) integer.
        assert_eq!(parse_scaled(&json!(2), 0).unwrap(), 2);
        // Decimal bps "0.5" scaled by 9.
        assert_eq!(parse_scaled(&json!("0.5"), 9).unwrap(), 500_000_000);
        // Fraction longer than the scale is truncated.
        assert_eq!(
            parse_scaled(&json!("1.2345678915"), 9).unwrap(),
            1_234_567_891
        );
        // Zero.
        assert_eq!(parse_scaled(&json!("0"), 9).unwrap(), 0);
    }

    #[macros::test_all]
    fn parse_fee_response_picks_finality_and_tier() {
        // Recorded-shape Iris response: two finality tiers, each with a
        // minimumFee (bps) and low/med/high forwardFee.
        let body = r#"[
            {"finalityThreshold":1000,"minimumFee":"1","forwardFee":{"low":"100","med":"150","high":"200"}},
            {"finalityThreshold":2000,"minimumFee":"0","forwardFee":{"low":"50","med":"75","high":"100"}}
        ]"#;

        let fast = CctpFeeClient::parse_fee_response(body, 1000).unwrap();
        assert_eq!(fast.bps_units, 1_000_000_000); // 1 bps scaled by 10^9
        assert_eq!(fast.forward_fee, 150); // med tier

        let standard = CctpFeeClient::parse_fee_response(body, 2000).unwrap();
        assert_eq!(standard.bps_units, 0);
        assert_eq!(standard.forward_fee, 75);
    }

    #[macros::test_all]
    fn parse_fee_response_missing_finality_errors() {
        let body = r#"[{"finalityThreshold":2000,"minimumFee":"0","forwardFee":{"low":"1","med":"2","high":"3"}}]"#;
        assert!(CctpFeeClient::parse_fee_response(body, 1000).is_err());
    }

    #[macros::test_all]
    fn parse_fee_response_forwarded_fee_with_numeric_values() {
        // minimumFee / forwardFee can arrive as JSON numbers, not strings.
        let body = r#"[{"finalityThreshold":1000,"minimumFee":0.5,"forwardFee":{"low":10,"med":20,"high":30}}]"#;
        let fee = CctpFeeClient::parse_fee_response(body, 1000).unwrap();
        assert_eq!(fee.bps_units, 500_000_000);
        assert_eq!(fee.forward_fee, 20);
    }
}
