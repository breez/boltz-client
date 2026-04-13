//! Thin Solana JSON-RPC client.
//!
//! Exists solely so the claim path can ask "does this base58-encoded Solana
//! account already exist on mainnet?" before deciding whether to emit the
//! `LayerZero` ATA-creation option. Wraps `platform_utils::http::HttpClient`
//! so the implementation works on both native and WASM without a Solana SDK.

use std::collections::HashMap;

use platform_utils::http::HttpClient;
use serde::Deserialize;
use serde_json::json;

use crate::error::BoltzError;

/// Minimal Solana JSON-RPC client. Only implements `getAccountInfo` because
/// that's all we need for the ATA existence check.
pub struct SolanaRpcClient {
    http: Box<dyn HttpClient>,
    rpc_url: String,
}

impl SolanaRpcClient {
    pub fn new(http: Box<dyn HttpClient>, rpc_url: String) -> Self {
        Self { http, rpc_url }
    }

    /// Query `getAccountInfo` and return whether the account exists.
    ///
    /// `account` is the base58 pubkey to look up. A `result.value: null`
    /// response means the account is missing; any object means it's present.
    pub async fn account_exists(&self, account: &str) -> Result<bool, BoltzError> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getAccountInfo",
            "params": [account, { "encoding": "base64" }],
        });
        let body = serde_json::to_string(&request).map_err(|e| {
            BoltzError::Generic(format!("Failed to serialize Solana RPC request: {e}"))
        })?;

        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        let response = self
            .http
            .post(self.rpc_url.clone(), Some(headers), Some(body))
            .await
            .map_err(|e| BoltzError::Generic(format!("Solana RPC request failed: {e}")))?;

        if !response.is_success() {
            return Err(BoltzError::Generic(format!(
                "Solana RPC HTTP error {}: {}",
                response.status, response.body
            )));
        }

        let parsed: JsonRpcResponse<AccountInfoResult> = serde_json::from_str(&response.body)
            .map_err(|e| {
                BoltzError::Generic(format!(
                    "Failed to parse Solana RPC response: {e} (body: {})",
                    response.body
                ))
            })?;

        if let Some(err) = parsed.error {
            return Err(BoltzError::Generic(format!(
                "Solana RPC error {}: {}",
                err.code, err.message
            )));
        }

        let result = parsed.result.ok_or_else(|| {
            BoltzError::Generic("Solana RPC response missing result field".into())
        })?;

        Ok(result.value.is_some())
    }
}

#[derive(Deserialize)]
struct JsonRpcResponse<T> {
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct AccountInfoResult {
    /// `null` when the account doesn't exist; otherwise an account object.
    /// We only care about presence, so keep it as an opaque `Value`.
    value: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    //! Test fixtures are captured verbatim from `api.mainnet.solana.com`
    //! `getAccountInfo` responses, not hand-written — so field naming and
    //! shape reflect what the real RPC returns.

    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;

    /// Real mainnet response for
    /// `Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB` (the Tether USDT mint
    /// account) — an existing account owned by the SPL Token program.
    #[macros::test_all]
    fn parses_real_existing_account() {
        let body = r#"{"jsonrpc":"2.0","result":{"context":{"apiVersion":"3.1.13","slot":412946078},"value":{"data":["AQAAAAXqnPFs5BGY8aSZN8iMNwqU1K//ibW6y470XmMku3j3oxlY8g1mDAAGAQEAAAAF6pzxbOQRmPGkmTfIjDcKlNSv/4m1usuO9F5jJLt49w==","base64"],"executable":false,"lamports":186394278422,"owner":"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA","rentEpoch":18446744073709551615,"space":82}},"id":1}"#;
        let parsed: JsonRpcResponse<AccountInfoResult> = serde_json::from_str(body).expect("parse");
        assert!(parsed.error.is_none());
        let result = parsed.result.expect("result");
        assert!(result.value.is_some());
    }

    /// Real mainnet response for a random base58 pubkey that was never
    /// initialised — the canonical "account doesn't exist" shape, with
    /// `result.value: null`.
    #[macros::test_all]
    fn parses_real_missing_account() {
        let body = r#"{"jsonrpc":"2.0","result":{"context":{"apiVersion":"3.1.13","slot":412946263},"value":null},"id":1}"#;
        let parsed: JsonRpcResponse<AccountInfoResult> = serde_json::from_str(body).expect("parse");
        assert!(parsed.error.is_none());
        let result = parsed.result.expect("result");
        assert!(result.value.is_none());
    }

    /// Real mainnet response for an invalid pubkey — the RPC returns a
    /// JSON-RPC error object (`-32602 Invalid params`).
    #[macros::test_all]
    fn parses_real_rpc_error() {
        let body = r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"Invalid param: Invalid"},"id":1}"#;
        let parsed: JsonRpcResponse<AccountInfoResult> = serde_json::from_str(body).expect("parse");
        assert!(parsed.result.is_none());
        let err = parsed.error.expect("error");
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "Invalid param: Invalid");
    }
}
