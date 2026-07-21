//! Source-chain deposit detection: scan USDC `Transfer` logs to the deposit
//! address between the stored watermark and the confirmed tip.
//!
//! Correctness rides on inflow identity (`chain:txHash:logIndex` dedup), not
//! on the watermark — a lost or stale watermark only causes a harmless
//! re-scan. The watermark is therefore a per-instance cursor and is advanced
//! only after every found inflow is persisted.

use alloy_sol_types::SolValue;

use crate::config::{ARBITRUM_CHAIN_ID, DepositChainSpec};
use crate::deposit::models::{Deposit, DepositStatus, ParkReason};
use crate::error::BoltzError;
use crate::evm::contracts::{address_to_topic, parse_address, transfer_event_topic0};
use crate::evm::provider::{EvmProvider, LogEntry};
use crate::store::DepositStorage;
use crate::swap::reverse::current_unix_timestamp;

/// `eth_getLogs` range size per request. Mirrors boltz-web-app's 2 000-block
/// scan interval — a safe ceiling for public RPC providers' range limits.
pub(crate) const DEPOSIT_SCAN_RANGE_BLOCKS: u64 = 2_000;

/// One scan pass over a single source chain. Returns the newly recorded
/// inflows (already persisted).
///
/// A fresh chain (no watermark) starts at the confirmed tip: pre-existing
/// history is deliberately not scanned — the address is handed out by this
/// service, so anything older predates its first run.
///
/// `erc20swap_address` is passed only for the Arbitrum-local chain, where an
/// inflow *from* the swap contract is a cooperative-refund return and must be
/// parked (never auto-retried) instead of processed as a fresh deposit.
pub(crate) async fn scan_chain_once<S>(
    provider: &EvmProvider,
    spec: &DepositChainSpec,
    confirmations: u64,
    deposit_address: &str,
    erc20swap_address: Option<&str>,
    store: &S,
) -> Result<Vec<Deposit>, BoltzError>
where
    S: DepositStorage + ?Sized,
{
    let latest = provider.eth_block_number().await?;
    let confirmed_tip = latest.saturating_sub(confirmations);
    if confirmed_tip == 0 {
        return Ok(Vec::new());
    }

    let Some(watermark) = store.get_deposit_watermark(spec.chain_id).await? else {
        // First run for this chain: anchor the cursor, scan nothing.
        store
            .set_deposit_watermark(spec.chain_id, confirmed_tip)
            .await?;
        return Ok(Vec::new());
    };
    let from = watermark.saturating_add(1);
    if from > confirmed_tip {
        return Ok(Vec::new());
    }

    let deposit_addr = parse_address(deposit_address)?;
    let to_topic = address_to_topic(&deposit_addr.into_array());
    let topic0 = transfer_event_topic0();

    let mut new_deposits = Vec::new();
    let mut range_start = from;
    while range_start <= confirmed_tip {
        let range_end = confirmed_tip.min(
            range_start
                .saturating_add(DEPOSIT_SCAN_RANGE_BLOCKS)
                .saturating_sub(1),
        );
        let logs = provider
            .eth_get_logs(
                spec.usdc_address,
                &[Some(&topic0), None, Some(&to_topic)],
                range_start,
                range_end,
            )
            .await?;

        for log in &logs {
            let Some(inflow) = parse_inbound_transfer(log, spec.usdc_address, &to_topic) else {
                continue;
            };
            let id = Deposit::make_id(spec.chain_id, &inflow.tx_hash, inflow.log_index);
            if store.get_deposit(&id).await?.is_some() {
                continue;
            }
            let deposit = build_deposit(id, spec, deposit_address, erc20swap_address, &inflow);
            store.upsert_deposit(&deposit).await?;
            new_deposits.push(deposit);
        }

        range_start = range_end.saturating_add(1);
    }

    // Only after every inflow above is durably recorded — a crash before
    // this line re-scans the same range and dedups by identity.
    store
        .set_deposit_watermark(spec.chain_id, confirmed_tip)
        .await?;

    Ok(new_deposits)
}

/// A decoded inbound USDC transfer.
struct InboundTransfer {
    tx_hash: String,
    log_index: u64,
    block_number: u64,
    amount: u64,
    /// Transfer sender (topic 1) — identifies refund returns on Arbitrum.
    from_address: String,
}

fn build_deposit(
    id: String,
    spec: &DepositChainSpec,
    deposit_address: &str,
    erc20swap_address: Option<&str>,
    inflow: &InboundTransfer,
) -> Deposit {
    // Arbitrum-local inflows skip the bridge: funds are already on the lock
    // chain, so they enter as `Minted` — except refund returns, which park.
    let (status, minted_amount) = if spec.chain_id == ARBITRUM_CHAIN_ID {
        let is_refund_return = erc20swap_address
            .is_some_and(|swap_addr| swap_addr.eq_ignore_ascii_case(&inflow.from_address));
        if is_refund_return {
            (
                DepositStatus::Parked {
                    reason: ParkReason::RefundReturned,
                },
                Some(inflow.amount),
            )
        } else {
            (DepositStatus::Minted, Some(inflow.amount))
        }
    } else {
        (DepositStatus::Detected, None)
    };

    let now = current_unix_timestamp();
    Deposit {
        id,
        status,
        chain_id: spec.chain_id,
        tx_hash: inflow.tx_hash.clone(),
        log_index: inflow.log_index,
        block_number: inflow.block_number,
        amount: inflow.amount,
        deposit_address: deposit_address.to_string(),
        pending_send: None,
        burn_tx_hash: None,
        cctp_nonce: None,
        mint_deadline: None,
        minted_amount,
        deposit_swap_id: None,
        created_at: now,
        updated_at: now,
    }
}

/// Decode a log as a USDC `Transfer` to the deposit address. Defensive: the
/// node already filtered by contract + topics, but a buggy/malicious RPC
/// response must not fabricate an inflow with the wrong token or recipient.
/// Logs without a `logIndex` are skipped — identity is impossible without it.
fn parse_inbound_transfer(
    log: &LogEntry,
    usdc_address: &str,
    to_topic: &str,
) -> Option<InboundTransfer> {
    if !log.address.eq_ignore_ascii_case(usdc_address) {
        return None;
    }
    if log.topics.len() < 3
        || !topics_eq(&log.topics[0], &transfer_event_topic0())
        || !topics_eq(&log.topics[2], to_topic)
    {
        return None;
    }

    let from_address = topic_address_hex(&log.topics[1])?;
    let data = hex::decode(log.data.strip_prefix("0x").unwrap_or(&log.data)).ok()?;
    let value = <alloy_primitives::U256>::abi_decode(&data).ok()?;
    let amount = u64::try_from(value).ok()?;

    Some(InboundTransfer {
        tx_hash: log.transaction_hash.to_lowercase(),
        log_index: parse_hex_u64(log.log_index.as_deref()?)?,
        block_number: parse_hex_u64(&log.block_number)?,
        amount,
        from_address,
    })
}

fn topics_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Last 20 bytes of a 32-byte topic, as `0x…` hex.
fn topic_address_hex(topic: &str) -> Option<String> {
    let hex_part = topic.strip_prefix("0x").unwrap_or(topic);
    if hex_part.len() != 64 {
        return None;
    }
    Some(format!("0x{}", &hex_part[24..].to_lowercase()))
}

fn parse_hex_u64(value: &str) -> Option<u64> {
    let hex_part = value.strip_prefix("0x").unwrap_or(value);
    u64::from_str_radix(hex_part, 16).ok()
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;
    use crate::config::deposit_chain_spec;
    use crate::store::MemoryBoltzStorage;
    use alloy_primitives::U256;
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

    const DEPOSIT_ADDR: &str = "0x9858EfFD232B4033E47d90003D41EC34EcaEda94";
    const SENDER: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    fn transfer_log(
        spec: &DepositChainSpec,
        from: &str,
        tx_hash: &str,
        log_index: u64,
        block: u64,
        amount: u64,
    ) -> serde_json::Value {
        let from_addr = parse_address(from).unwrap();
        let to_addr = parse_address(DEPOSIT_ADDR).unwrap();
        serde_json::json!({
            "address": spec.usdc_address,
            "topics": [
                transfer_event_topic0(),
                address_to_topic(&from_addr.into_array()),
                address_to_topic(&to_addr.into_array()),
            ],
            "data": format!("0x{}", hex::encode(U256::from(amount).abi_encode())),
            "blockNumber": format!("0x{block:x}"),
            "transactionHash": tx_hash,
            "logIndex": format!("0x{log_index:x}"),
        })
    }

    fn provider_with(responses: Vec<HttpResponse>) -> EvmProvider {
        EvmProvider::new(
            "http://mock".to_string(),
            Box::new(MockHttpClient::new(responses)),
        )
    }

    #[macros::async_test_all]
    async fn fresh_chain_anchors_watermark_and_scans_nothing() {
        let store = MemoryBoltzStorage::new();
        let spec = deposit_chain_spec(8453).unwrap();
        // latest = 1000, confirmations 12 -> confirmed tip 988.
        let provider = provider_with(vec![rpc_success(&serde_json::json!("0x3e8"))]);

        let found = scan_chain_once(&provider, spec, 12, DEPOSIT_ADDR, None, &store)
            .await
            .unwrap();
        assert!(found.is_empty());
        assert_eq!(store.get_deposit_watermark(8453).await.unwrap(), Some(988));
    }

    #[macros::async_test_all]
    async fn detects_dedups_and_advances_watermark() {
        let store = MemoryBoltzStorage::new();
        let spec = deposit_chain_spec(8453).unwrap();
        store.set_deposit_watermark(8453, 988).await.unwrap();

        let log = transfer_log(spec, SENDER, "0xAAAA", 3, 990, 50_000_000);
        let provider = provider_with(vec![
            rpc_success(&serde_json::json!("0x3f2")), // latest 1010 -> tip 998
            rpc_success(&serde_json::json!([log])),
        ]);

        let found = scan_chain_once(&provider, spec, 12, DEPOSIT_ADDR, None, &store)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        let d = &found[0];
        assert_eq!(d.id, "8453:0xaaaa:3");
        assert_eq!(d.status, DepositStatus::Detected);
        assert_eq!(d.amount, 50_000_000);
        assert_eq!(d.block_number, 990);
        assert!(d.minted_amount.is_none());
        assert_eq!(store.get_deposit_watermark(8453).await.unwrap(), Some(998));

        // Second pass over an overlapping range: same log, no new record.
        store.set_deposit_watermark(8453, 989).await.unwrap();
        let log2 = transfer_log(spec, SENDER, "0xAAAA", 3, 990, 50_000_000);
        let provider = provider_with(vec![
            rpc_success(&serde_json::json!("0x3f2")),
            rpc_success(&serde_json::json!([log2])),
        ]);
        let found = scan_chain_once(&provider, spec, 12, DEPOSIT_ADDR, None, &store)
            .await
            .unwrap();
        assert!(found.is_empty());
    }

    #[macros::async_test_all]
    async fn arbitrum_local_inflows_enter_minted_or_parked() {
        let store = MemoryBoltzStorage::new();
        let spec = deposit_chain_spec(ARBITRUM_CHAIN_ID).unwrap();
        store
            .set_deposit_watermark(ARBITRUM_CHAIN_ID, 100)
            .await
            .unwrap();

        let erc20swap = "0xc09247F837A205BDdE43960Ca01BDea426F1370e";
        let direct = transfer_log(spec, SENDER, "0xD1", 0, 105, 30_000_000);
        let refund_return = transfer_log(spec, erc20swap, "0xD2", 1, 106, 20_000_000);
        let provider = provider_with(vec![
            rpc_success(&serde_json::json!("0x7d")), // latest 125 -> tip 113
            rpc_success(&serde_json::json!([direct, refund_return])),
        ]);

        let found = scan_chain_once(&provider, spec, 12, DEPOSIT_ADDR, Some(erc20swap), &store)
            .await
            .unwrap();
        assert_eq!(found.len(), 2);

        let direct_dep = found.iter().find(|d| d.tx_hash == "0xd1").unwrap();
        assert_eq!(direct_dep.status, DepositStatus::Minted);
        assert_eq!(direct_dep.minted_amount, Some(30_000_000));

        let parked = found.iter().find(|d| d.tx_hash == "0xd2").unwrap();
        assert_eq!(
            parked.status,
            DepositStatus::Parked {
                reason: ParkReason::RefundReturned
            }
        );
        assert_eq!(parked.minted_amount, Some(20_000_000));
    }

    #[macros::async_test_all]
    async fn skips_logs_with_wrong_token_or_missing_log_index() {
        let store = MemoryBoltzStorage::new();
        let spec = deposit_chain_spec(8453).unwrap();
        store.set_deposit_watermark(8453, 988).await.unwrap();

        let mut wrong_token = transfer_log(spec, SENDER, "0xE1", 0, 990, 10);
        wrong_token["address"] = serde_json::json!("0x0000000000000000000000000000000000000bad");
        let mut no_index = transfer_log(spec, SENDER, "0xE2", 0, 990, 10);
        no_index.as_object_mut().unwrap().remove("logIndex");

        let provider = provider_with(vec![
            rpc_success(&serde_json::json!("0x3f2")),
            rpc_success(&serde_json::json!([wrong_token, no_index])),
        ]);
        let found = scan_chain_once(&provider, spec, 12, DEPOSIT_ADDR, None, &store)
            .await
            .unwrap();
        assert!(found.is_empty());
        // Watermark still advances: the range was fully scanned.
        assert_eq!(store.get_deposit_watermark(8453).await.unwrap(), Some(998));
    }
}
