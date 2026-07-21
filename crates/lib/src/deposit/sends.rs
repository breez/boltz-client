//! Nonce-guarded sponsored sends — the chain-enforced at-most-once for
//! deposit burns, locks, refunds, and manual mints.
//!
//! Every deposit send follows one discipline:
//! 1. read `EntryPoint.getNonce(deposit_address, DEPOSIT_NONCE_KEY)`;
//! 2. derive the send schedule from chain logs (see `deposit::schedule`);
//! 3. persist the pending-send anchor;
//! 4. `wallet_prepareCalls`, then REQUIRE the prepared `UserOp`'s nonce to
//!    equal the step-1 read — a moved nonce means another send (possibly
//!    another instance's) slipped between derivation and prepare, so abort
//!    and re-derive rather than sign;
//! 5. sign and submit; two instances that both pass step 4 hold
//!    identical-nonce `UserOp`s, of which the chain executes at most one.
//!
//! The signature covers the `UserOp` hash (nonce included), so nothing
//! downstream can re-nonce a signed op.

use alloy_primitives::U256;

use crate::config::{DEPOSIT_NONCE_KEY, ENTRYPOINT_V07_ADDRESS};
use crate::error::BoltzError;
use crate::evm::alchemy::{AlchemyGasClient, EvmCall};
use crate::evm::contracts::{decode_get_nonce_return, encode_get_nonce, parse_address};
use crate::evm::provider::EvmProvider;
use crate::evm::signing::EvmSigner;

/// Outcome of a guarded send attempt.
#[derive(Debug)]
pub(crate) enum SendOutcome {
    /// Submitted; poll the gas sponsor by `call_id` for the tx hash.
    Sent { call_id: String },
    /// The prepared nonce differs from the pre-derivation read — another
    /// send landed (or is pending) in between. Nothing was signed or sent;
    /// re-derive the schedule and retry.
    NonceMoved,
}

/// Read the deposit account's 4337 nonce — step 1 of the discipline. Must
/// happen BEFORE the schedule derivation's log scan: any send consuming an
/// earlier sequence is then either already visible in the scan or still
/// holds the nonce we read (and will collide at step 4/5).
pub(crate) async fn read_deposit_nonce(
    provider: &EvmProvider,
    deposit_address: &str,
) -> Result<U256, BoltzError> {
    let sender = parse_address(deposit_address)?;
    let calldata = encode_get_nonce(sender, DEPOSIT_NONCE_KEY);
    let ret = provider.eth_call(ENTRYPOINT_V07_ADDRESS, &calldata).await?;
    decode_get_nonce_return(&ret)
}

/// Prepare, nonce-check, sign, and submit one sponsored send.
///
/// Fails closed: a prepared payload with no extractable `UserOp` nonce is an
/// error (never signed) — if the sponsor's wallet stack ever changes shape
/// or nonce-key semantics, deposit sends stall loudly instead of silently
/// losing the collision guarantee.
pub(crate) async fn send_nonce_guarded(
    alchemy: &AlchemyGasClient,
    signer: &EvmSigner,
    calls: Vec<EvmCall>,
    chain_id: u64,
    expected_nonce: U256,
) -> Result<SendOutcome, BoltzError> {
    let prepared = alchemy.prepare_calls(&calls, chain_id, signer).await?;

    let Some(prepared_nonce) = extract_user_op_nonce(&prepared) else {
        return Err(BoltzError::Evm {
            reason: "prepared calls carry no user-operation nonce; refusing to sign".to_string(),
            tx_hash: None,
        });
    };
    if prepared_nonce != expected_nonce {
        tracing::warn!(
            expected = %expected_nonce,
            prepared = %prepared_nonce,
            "deposit send nonce moved between schedule derivation and prepare; aborting"
        );
        return Ok(SendOutcome::NonceMoved);
    }

    let call_id = alchemy.sign_and_send(prepared, signer).await?;
    Ok(SendOutcome::Sent { call_id })
}

/// Pull the `UserOp` nonce out of a `wallet_prepareCalls` response. Handles
/// both response shapes: `{type: "array", data: [...]}` (first-time EIP-7702
/// flows carry an authorization element alongside the `UserOp`) and a single
/// user-operation object.
pub(crate) fn extract_user_op_nonce(prepared: &serde_json::Value) -> Option<U256> {
    let elements: Vec<&serde_json::Value> =
        if prepared.get("type").and_then(serde_json::Value::as_str) == Some("array") {
            prepared.get("data")?.as_array()?.iter().collect()
        } else {
            vec![prepared]
        };

    for element in elements {
        let is_user_op = element
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|t| t.starts_with("user-operation"));
        if !is_user_op {
            continue;
        }
        let nonce_hex = element.get("data")?.get("nonce")?.as_str()?;
        let hex_part = nonce_hex.strip_prefix("0x").unwrap_or(nonce_hex);
        return U256::from_str_radix(hex_part, 16).ok();
    }
    None
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;
    use crate::config::AlchemyConfig;
    use crate::keys::EvmKeyManager;
    use alloy_sol_types::SolValue;
    use platform_utils::http::{HttpClient, HttpError, HttpResponse};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct MockHttpClient {
        responses: Arc<Mutex<Vec<HttpResponse>>>,
    }

    impl MockHttpClient {
        fn new(responses: Vec<HttpResponse>) -> Self {
            let mut r = responses;
            r.reverse();
            Self {
                responses: Arc::new(Mutex::new(r)),
            }
        }
    }

    #[macros::async_trait]
    impl HttpClient for MockHttpClient {
        async fn get(
            &self,
            _url: String,
            _headers: Option<HashMap<String, String>>,
        ) -> Result<HttpResponse, HttpError> {
            unimplemented!()
        }

        async fn post(
            &self,
            _url: String,
            _headers: Option<HashMap<String, String>>,
            _body: Option<String>,
        ) -> Result<HttpResponse, HttpError> {
            let mut responses = self.responses.lock().unwrap();
            Ok(responses.pop().expect("no more mock responses"))
        }

        async fn delete(
            &self,
            _url: String,
            _headers: Option<HashMap<String, String>>,
            _body: Option<String>,
        ) -> Result<HttpResponse, HttpError> {
            unimplemented!()
        }
    }

    fn rpc_success(result: &serde_json::Value) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string(),
        }
    }

    /// Shape of the live sponsor's response for a fresh (undelegated)
    /// account, captured 2026-07-20: authorization element + v0.70 `UserOp`
    /// with 2D nonce key=1, seq=0.
    fn live_prepared_fixture() -> serde_json::Value {
        serde_json::json!({
            "type": "array",
            "data": [
                {
                    "type": "authorization",
                    "data": {"address": "0x69007702764179f14F51cdce752f4f775d74E139", "nonce": "0x0"},
                    "chainId": "0xa4b1",
                    "signatureRequest": {"type": "eip7702Auth", "rawPayload": "0x8c6f"}
                },
                {
                    "type": "user-operation-v070",
                    "data": {
                        "sender": "0xd2c9c2c3BB140717E2d24b45442Fe9CBd49DD750",
                        "nonce": "0x10000000000000000",
                        "callData": "0xb61d27f6"
                    },
                    "chainId": "0xa4b1",
                    "signatureRequest": {"type": "personal_sign", "data": {"raw": "0x8912"}}
                }
            ]
        })
    }

    fn expected_key1_nonce(seq: u64) -> U256 {
        U256::from(0x1_0000_0000_0000_0000_u128) | U256::from(seq)
    }

    #[macros::test_all]
    fn extracts_nonce_from_live_array_shape() {
        let nonce = extract_user_op_nonce(&live_prepared_fixture()).unwrap();
        assert_eq!(nonce, expected_key1_nonce(0));
    }

    #[macros::test_all]
    fn extracts_nonce_from_single_op_shape() {
        let prepared = serde_json::json!({
            "type": "user-operation-v070",
            "data": {"nonce": "0x1000000000000000a"}
        });
        assert_eq!(
            extract_user_op_nonce(&prepared).unwrap(),
            expected_key1_nonce(10)
        );
    }

    #[macros::test_all]
    fn missing_nonce_yields_none() {
        assert!(extract_user_op_nonce(&serde_json::json!({"type": "array", "data": []})).is_none());
        assert!(
            extract_user_op_nonce(&serde_json::json!({
                "type": "array",
                "data": [{"type": "authorization", "data": {"nonce": "0x0"}}]
            }))
            .is_none()
        );
    }

    fn test_signer() -> EvmSigner {
        let seed = hex::decode(
            "5eb00bbddcf069084889a8ab9155568165f5c453ccb85e70811aaed6f6da5fc19a5ac40b389cd370d086206dec8aa6c43daea6690f20ad3d8d48b2d2ce9e38e4",
        )
        .unwrap();
        let manager = EvmKeyManager::from_seed(&seed).unwrap();
        EvmSigner::new(&manager.derive_deposit_key(0).unwrap(), 42161)
    }

    fn alchemy_with(responses: Vec<HttpResponse>) -> AlchemyGasClient {
        AlchemyGasClient::new(
            &AlchemyConfig {
                gas_sponsor_url: "http://mock".to_string(),
            },
            Box::new(MockHttpClient::new(responses)),
        )
    }

    #[macros::async_test_all]
    async fn nonce_move_aborts_without_signing() {
        // Prepared op carries seq=5; we expected seq=4 -> NonceMoved, and the
        // single mock response proves no send request followed the prepare.
        let prepared = serde_json::json!({
            "type": "user-operation-v070",
            "data": {"nonce": "0x10000000000000005"}
        });
        let alchemy = alchemy_with(vec![rpc_success(&prepared)]);

        let outcome = send_nonce_guarded(
            &alchemy,
            &test_signer(),
            vec![EvmCall {
                to: "0x0000000000000000000000000000000000000001".to_string(),
                value: None,
                data: None,
            }],
            42161,
            expected_key1_nonce(4),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, SendOutcome::NonceMoved));
    }

    #[macros::async_test_all]
    async fn missing_nonce_is_an_error_not_a_send() {
        let prepared = serde_json::json!({"type": "array", "data": []});
        let alchemy = alchemy_with(vec![rpc_success(&prepared)]);

        let result = send_nonce_guarded(
            &alchemy,
            &test_signer(),
            vec![EvmCall {
                to: "0x0000000000000000000000000000000000000001".to_string(),
                value: None,
                data: None,
            }],
            42161,
            expected_key1_nonce(0),
        )
        .await;
        assert!(result.is_err());
    }

    #[macros::async_test_all]
    async fn read_deposit_nonce_decodes_entrypoint_return() {
        let nonce = expected_key1_nonce(7);
        let encoded = format!("0x{}", hex::encode(nonce.abi_encode()));
        let provider = EvmProvider::new(
            "http://mock".to_string(),
            Box::new(MockHttpClient::new(vec![rpc_success(&serde_json::json!(
                encoded
            ))])),
        );

        let read = read_deposit_nonce(&provider, "0xd2c9c2c3BB140717E2d24b45442Fe9CBd49DD750")
            .await
            .unwrap();
        assert_eq!(read, nonce);
    }
}
