#[cfg(test)]
use platform_utils::tokio;

use crate::error::BoltzError;
use crate::models::BoltzSwap;

/// Persistence interface for Boltz swap state.
///
/// The boltz crate defines the trait; the caller provides the implementation.
/// (A volatile `MemoryBoltzStorage` exists for the crate's own unit tests only,
/// gated behind `#[cfg(test)]`, so it is never reachable by an embedder.)
///
/// # Key index durability
///
/// `increment_key_index` must be durable: the new index must be persisted
/// before the method returns. This is the sole defense against preimage
/// reuse — if the counter regresses after a crash, a previously-used
/// preimage hash could be sent to Boltz, enabling fund theft.
#[macros::async_trait]
pub trait BoltzStorage: Send + Sync {
    async fn insert_swap(&self, swap: &BoltzSwap) -> Result<(), BoltzError>;
    async fn update_swap(&self, swap: &BoltzSwap) -> Result<(), BoltzError>;
    async fn get_swap(&self, id: &str) -> Result<Option<BoltzSwap>, BoltzError>;
    /// Return all swaps with non-terminal status.
    async fn list_active_swaps(&self) -> Result<Vec<BoltzSwap>, BoltzError>;
    /// Atomically reserve the next key index and return it.
    async fn increment_key_index(&self) -> Result<u32, BoltzError>;
}

/// In-memory store for the crate's own unit tests, gated behind `#[cfg(test)]`
/// so it is compiled only when testing this crate — never in a build where the
/// crate is a dependency, so an embedder can't reach it.
///
/// Its `key_index` lives in a volatile `Mutex<u32>` that resets to 0 on every
/// process restart, which would violate the [`BoltzStorage`] durability
/// invariant above (index regression re-derives used preimages — a fund-theft
/// vector). That is exactly why it must stay test-only.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryBoltzStorage {
    swaps: tokio::sync::Mutex<std::collections::HashMap<String, BoltzSwap>>,
    key_index: tokio::sync::Mutex<u32>,
}

#[cfg(test)]
impl MemoryBoltzStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
#[macros::async_trait]
impl BoltzStorage for MemoryBoltzStorage {
    async fn insert_swap(&self, swap: &BoltzSwap) -> Result<(), BoltzError> {
        self.swaps
            .lock()
            .await
            .insert(swap.id.clone(), swap.clone());
        Ok(())
    }

    async fn update_swap(&self, swap: &BoltzSwap) -> Result<(), BoltzError> {
        let mut swaps = self.swaps.lock().await;
        if swaps.contains_key(&swap.id) {
            swaps.insert(swap.id.clone(), swap.clone());
            Ok(())
        } else {
            Err(BoltzError::Store(format!("Swap not found: {}", swap.id)))
        }
    }

    async fn get_swap(&self, id: &str) -> Result<Option<BoltzSwap>, BoltzError> {
        Ok(self.swaps.lock().await.get(id).cloned())
    }

    async fn list_active_swaps(&self) -> Result<Vec<BoltzSwap>, BoltzError> {
        Ok(self
            .swaps
            .lock()
            .await
            .values()
            .filter(|s| !s.status.is_terminal())
            .cloned()
            .collect())
    }

    async fn increment_key_index(&self) -> Result<u32, BoltzError> {
        let mut idx = self.key_index.lock().await;
        let current = *idx;
        *idx = current
            .checked_add(1)
            .ok_or_else(|| BoltzError::Store("Key index overflow".to_string()))?;
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;
    use crate::models::{Asset, BoltzSwapStatus, BridgeKind};

    fn test_swap(id: &str, status: BoltzSwapStatus) -> BoltzSwap {
        BoltzSwap {
            id: id.to_string(),
            status,
            bridge_kind: BridgeKind::Oft,
            claim_key_index: 0,
            chain_id: 42161,
            claim_address: "0xabc".to_string(),
            destination_address: "0xdef".to_string(),
            destination_chain: "Arbitrum One".to_string(),
            asset: Asset::Usdt,
            refund_address: "0x123".to_string(),
            erc20swap_address: "0xswap".to_string(),
            router_address: "0xrouter".to_string(),
            invoice: "lnbc...".to_string(),
            invoice_amount_sats: 100_000,
            onchain_amount: 99_500,
            expected_output_amount: 71_000_000,
            slippage_bps: 100,
            timeout_block_height: 123_456,
            lockup_tx_id: None,
            claim_tx_hash: None,
            pending_call_id: None,
            delivered_amount: None,
            bridge_ref: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    #[macros::async_test_all]
    async fn test_insert_and_get() {
        let store = MemoryBoltzStorage::new();
        let swap = test_swap("1", BoltzSwapStatus::Created);
        store.insert_swap(&swap).await.unwrap();

        let retrieved = store.get_swap("1").await.unwrap().unwrap();
        assert_eq!(retrieved.id, "1");
    }

    #[macros::async_test_all]
    async fn test_update_swap() {
        let store = MemoryBoltzStorage::new();
        let mut swap = test_swap("1", BoltzSwapStatus::Created);
        store.insert_swap(&swap).await.unwrap();

        swap.status = BoltzSwapStatus::TbtcLocked;
        store.update_swap(&swap).await.unwrap();

        let retrieved = store.get_swap("1").await.unwrap().unwrap();
        assert_eq!(retrieved.status, BoltzSwapStatus::TbtcLocked);
    }

    #[macros::async_test_all]
    async fn test_update_nonexistent_fails() {
        let store = MemoryBoltzStorage::new();
        let swap = test_swap("1", BoltzSwapStatus::Created);
        assert!(store.update_swap(&swap).await.is_err());
    }

    #[macros::async_test_all]
    async fn test_list_active_swaps() {
        let store = MemoryBoltzStorage::new();
        store
            .insert_swap(&test_swap("1", BoltzSwapStatus::Created))
            .await
            .unwrap();
        store
            .insert_swap(&test_swap("2", BoltzSwapStatus::Completed))
            .await
            .unwrap();
        store
            .insert_swap(&test_swap("3", BoltzSwapStatus::TbtcLocked))
            .await
            .unwrap();

        let active = store.list_active_swaps().await.unwrap();
        assert_eq!(active.len(), 2);
    }

    #[macros::async_test_all]
    async fn test_key_index_management() {
        let store = MemoryBoltzStorage::new();

        let idx0 = store.increment_key_index().await.unwrap();
        assert_eq!(idx0, 0);

        let idx1 = store.increment_key_index().await.unwrap();
        assert_eq!(idx1, 1);
    }

    #[macros::async_test_all]
    async fn test_get_nonexistent_returns_none() {
        let store = MemoryBoltzStorage::new();
        assert!(store.get_swap("nonexistent").await.unwrap().is_none());
    }

    #[macros::async_test_all]
    async fn test_pending_call_id_round_trips() {
        let store = MemoryBoltzStorage::new();
        let mut swap = test_swap("1", BoltzSwapStatus::Claiming);
        assert!(swap.pending_call_id.is_none());
        store.insert_swap(&swap).await.unwrap();

        // Mid-claim: record the in-flight call_id.
        swap.pending_call_id = Some("call_abc".to_string());
        store.update_swap(&swap).await.unwrap();
        assert_eq!(
            store.get_swap("1").await.unwrap().unwrap().pending_call_id,
            Some("call_abc".to_string())
        );

        // Confirmed: tx hash recorded, pending marker cleared.
        swap.claim_tx_hash = Some("0xdeadbeef".to_string());
        swap.pending_call_id = None;
        store.update_swap(&swap).await.unwrap();
        let final_swap = store.get_swap("1").await.unwrap().unwrap();
        assert_eq!(final_swap.claim_tx_hash, Some("0xdeadbeef".to_string()));
        assert!(final_swap.pending_call_id.is_none());
    }
}
