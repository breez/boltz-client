//! `LayerZero` v2 type-3 executor options encoder for OFT
//! `SendParam.extraOptions`.
//!
//! Only the "create Solana `Associated Token Account`" branch is supported —
//! the LZ executor reads an option-type-1 `lzReceive` entry with
//! `(gas=0, value=solanaAtaRentExemptLamports)` and creates the recipient's
//! SPL ATA on Solana before landing the cross-chain tokens. Without this
//! hint, sending USDT0 to a Solana wallet that has never held USDT fails
//! silently on the destination side.
//!
//! Native-drop (option type 2), gas top-up, and compose messages are not
//! emitted — the reverse-swap flow only needs ATA creation.

/// Lamports required to rent-exempt a 165-byte SPL Token account. Static on
/// Solana mainnet. If the runtime ever raises the rent-exempt floor this
/// becomes load-bearing and should move to a `getMinimumBalanceForRentExemption`
/// runtime query.
pub const SOLANA_ATA_RENT_EXEMPT_LAMPORTS: u128 = 2_039_280;

/// `LayerZero` v2 options header — type-3 options.
const TYPE3_HEADER: [u8; 2] = [0x00, 0x03];
/// Executor worker id (constant in LZ v2; only one executor worker exists).
const EXECUTOR_WORKER_ID: u8 = 1;
/// Option type: `lzReceive`.
const OPTION_TYPE_LZ_RECEIVE: u8 = 1;

/// Build the `extraOptions` bytes for an OFT `SendParam`.
///
/// When `create_solana_ata` is `false`, returns an empty byte vector — the
/// default when no executor directives are needed (all EVM destinations, Tron,
/// and Solana destinations where the recipient's ATA already exists).
///
/// When `true`, returns a 38-byte type-3 options blob with a single lzReceive
/// entry carrying `(gas=0, value=SOLANA_ATA_RENT_EXEMPT_LAMPORTS)`, instructing
/// the destination executor to create the recipient's ATA before delivering
/// the cross-chain tokens.
#[must_use]
pub fn build_extra_options(create_solana_ata: bool) -> Vec<u8> {
    if !create_solana_ata {
        return Vec::new();
    }

    // Option payload: two uint128s, big-endian. The first is the gas limit
    // (0 — ATA creation is a Solana native instruction). The second is the
    // lamports to pre-fund the account.
    let mut option_payload = [0u8; 32];
    option_payload[16..32].copy_from_slice(&SOLANA_ATA_RENT_EXEMPT_LAMPORTS.to_be_bytes());

    // `option_payload.len()` is 32, so `+ 1` cannot overflow `u16`.
    let option_size: u16 = 32 + 1;

    // 2 bytes type-3 header + 1 byte worker id + 2 bytes option size
    // + 1 byte option type + 32 bytes payload = 38 bytes.
    let mut out = Vec::with_capacity(38);
    out.extend_from_slice(&TYPE3_HEADER);
    out.push(EXECUTOR_WORKER_ID);
    out.extend_from_slice(&option_size.to_be_bytes());
    out.push(OPTION_TYPE_LZ_RECEIVE);
    out.extend_from_slice(&option_payload);
    out
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;

    #[macros::test_all]
    fn no_ata_creation_returns_empty() {
        assert!(build_extra_options(false).is_empty());
    }

    /// Structural assertions for the `LayerZero` v2 type-3 options blob
    /// carrying a single ATA-creation `lzReceive` entry:
    ///   - Type-3 header `0003`
    ///   - Executor worker id `01`
    ///   - Option size `0021` (= 33 = 32-byte payload + 1 for optionType)
    ///   - Option type `01` (lzReceive)
    ///   - Payload: 16 zero bytes gas + `2_039_280` as uint128 BE (0x1F1DF0)
    #[macros::test_all]
    fn ata_creation_matches_structural_layout() {
        let bytes = build_extra_options(true);

        assert_eq!(bytes.len(), 38);
        assert_eq!(&bytes[0..2], &[0x00, 0x03]);
        assert_eq!(bytes[2], 0x01);
        assert_eq!(&bytes[3..5], &[0x00, 0x21]);
        assert_eq!(bytes[5], 0x01);
        assert_eq!(&bytes[6..22], &[0u8; 16]);
        let mut expected_lamports = [0u8; 16];
        expected_lamports[13..16].copy_from_slice(&[0x1f, 0x1d, 0xf0]);
        assert_eq!(&bytes[22..38], &expected_lamports);
    }

    /// Byte-exact vector decoded from a literal hex string. Cross-checks the
    /// same bytes against an independent encoding path.
    #[macros::test_all]
    fn ata_creation_full_hex_vector() {
        let bytes = build_extra_options(true);
        let expected = hex::decode(
            "00030100210100000000000000000000000000000000000000000000000000000000001f1df0",
        )
        .expect("hex");
        assert_eq!(bytes, expected);
    }
}
