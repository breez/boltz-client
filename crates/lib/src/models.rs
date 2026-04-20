use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Persisted state for a single Boltz reverse swap.
///
/// Preimage and `preimage_hash` are NOT stored — they are deterministically
/// derived from `seed + claim_key_index + chain_id`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoltzSwap {
    /// Swap ID — the Boltz backend ID for normal swaps, or a `recovery-*` ID for recovered swaps.
    pub id: String,
    pub status: BoltzSwapStatus,
    /// HD derivation index for the per-swap preimage key.
    pub claim_key_index: u32,
    /// EVM chain ID (42161 for Arbitrum).
    pub chain_id: u64,

    // Addresses
    /// Gas signer address (used as claimAddress with Boltz).
    pub claim_address: String,
    /// User's final USDT destination.
    pub destination_address: String,
    /// Target chain for delivery.
    pub destination_chain: ChainId,
    /// Boltz's refund address (from swap response).
    pub refund_address: String,

    // Contract addresses (snapshot at creation time)
    pub erc20swap_address: String,
    pub router_address: String,

    // Invoice
    pub invoice: String,
    pub invoice_amount_sats: u64,

    // Amounts
    /// tBTC amount locked on-chain (sats, from swap response `onchainAmount`).
    pub onchain_amount: u64,
    /// Expected USDT output (6 decimals).
    pub expected_usdt_amount: u64,
    /// DEX slippage tolerance (basis points) snapshot at `prepare` time.
    /// Used for the claim-time quote drift check and on-chain `minOut`
    /// values so per-swap overrides survive across service restarts.
    pub slippage_bps: u32,

    // Timing
    pub timeout_block_height: u64,

    // Results
    pub lockup_tx_id: Option<String>,
    pub claim_tx_hash: Option<String>,
    /// Actual USDT amount delivered on the destination chain (6 decimals).
    /// `None` until the claim receipt is processed. For bridged destinations
    /// this is the OFT `amountReceivedLD`; for Arbitrum delivery it's the
    /// final ERC20 `Transfer` value to the user.
    pub delivered_amount: Option<u64>,
    /// `LayerZero` message GUID (`0x`-prefixed hex) for bridged swaps.
    /// `None` for Arbitrum-destination swaps (no bridge).
    pub lz_guid: Option<String>,

    // Timestamps (unix seconds)
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum BoltzSwapStatus {
    /// Swap created on Boltz, invoice ready to pay.
    Created,
    /// Hold invoice paid, waiting for Boltz to lock tBTC.
    InvoicePaid,
    /// tBTC locked on Arbitrum, ready to claim.
    TbtcLocked,
    /// Claim tx submitted, waiting for confirmation.
    Claiming,
    /// USDT delivered to destination.
    Completed,
    /// Swap failed.
    Failed { reason: String },
    /// Swap expired (Boltz timeout reached).
    Expired,
}

impl BoltzSwapStatus {
    /// Whether this status is terminal (no further transitions expected).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed { .. } | Self::Expired)
    }
}

/// Underlying transport for a chain. Determines recipient encoding, RPC
/// dispatch, and OFT source-contract selection.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum NetworkTransport {
    Evm,
    Solana,
    Tron,
}

/// Which USDT0 mesh a destination belongs to. Native-mesh and legacy-mesh
/// deployments live on distinct source-side OFT contracts with different
/// fee models, so the destination's mesh determines which source contract
/// the claim path quotes and bridges through.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Usdt0Kind {
    Native,
    Legacy,
}

/// Stable identifier for a destination chain. Holds the USDT0 chain name
/// lowercased (e.g. `"arbitrum one"`, `"solana"`, `"tempo"`). Construct via
/// [`ChainId::new`] to guarantee the canonical lowercased form.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChainId(String);

impl ChainId {
    /// Build a `ChainId` from any string, lowercasing to the canonical form.
    pub fn new(name: impl AsRef<str>) -> Self {
        Self(name.as_ref().to_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ChainId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ChainId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Runtime metadata for a single destination chain. Built from the USDT0
/// deployments API at service init and joined with the `NetworkTransport`
/// inferred from the USDT0 entry.
#[derive(Clone, Debug)]
pub struct ChainSpec {
    /// Canonical ID (lowercased USDT0 name). Stable join key.
    pub id: ChainId,
    /// `true` when this spec represents the USDT0 mesh's source chain
    /// (same-chain delivery: no OFT bridging, no `LayerZero` message).
    /// Set by the registry builder at init time.
    pub is_source: bool,
    /// Raw USDT0 name (`"Arbitrum One"`, `"Solana"`) — display-only.
    pub display_name: String,
    pub transport: NetworkTransport,
    /// EVM chain ID. `None` for non-EVM transports (Solana, Tron), which
    /// USDT0 returns with `chainId: null`.
    pub evm_chain_id: Option<u64>,
    /// `LayerZero` endpoint ID for this destination.
    pub lz_eid: u32,
    /// Destination-side OFT contract address (`0x…` for EVM, base58 for
    /// Solana/Tron). Informational only — the claim path uses the
    /// source-side OFT picked from [`SourceSpec::oft_for`].
    pub oft_address: String,
    /// USDT0 token contract address when the deployments registry publishes
    /// one. `None` for adapter-only deployments (Ethereum mainnet, where the
    /// adapter wraps the canonical USDT).
    pub token_address: Option<String>,
    /// Which mesh this entry came from.
    pub mesh: Usdt0Kind,
}

impl ChainSpec {
    /// Ticker of the asset the user receives on this destination chain.
    ///
    /// Returns `"USDT"` when the delivered token is canonical Tether:
    ///   - Source chain (same-chain delivery; no OFT bridging).
    ///   - Adapter-only deployments (`token_address.is_none()`) where the
    ///     OFT adapter unwraps the canonical underlying USDT (e.g.
    ///     Ethereum mainnet, and legacy-mesh chains like Tron/Solana/Celo
    ///     that bridge into the pre-existing canonical USDT on that chain).
    ///
    /// Returns `"USDT0"` everywhere else — any native-mesh destination
    /// that publishes its own `Token` entry receives the distinct USDT0
    /// ERC20/SPL, not canonical Tether, even when other clients label it
    /// plain "USDT" (they do so because USDT0 is the only USDT-branded
    /// token they surface on that chain). Labeling it accurately here
    /// prevents users from conflating a USDT0 balance with any canonical
    /// Tether deployment they may also hold.
    pub fn asset_symbol(&self) -> &'static str {
        if self.is_source || self.token_address.is_none() {
            return "USDT";
        }
        "USDT0"
    }
}

/// Runtime metadata for the source chain (Arbitrum). Aggregates the native-
/// and legacy-mesh OFT contracts on the same chain so the claim path can
/// pick the one matching the destination's mesh.
#[derive(Clone, Debug)]
pub struct SourceSpec {
    pub id: ChainId,
    pub evm_chain_id: u64,
    /// Source OFT contract on the native mesh. `None` if the source chain
    /// doesn't participate in the native mesh.
    pub native_oft_address: Option<String>,
    /// Source OFT contract on the legacy mesh. `None` if the source chain
    /// doesn't participate in the legacy mesh.
    pub legacy_oft_address: Option<String>,
}

impl SourceSpec {
    /// Pick the source OFT contract address for a destination on the given
    /// mesh. Returns `None` if the source doesn't participate in that mesh.
    pub fn oft_for(&self, mesh: Usdt0Kind) -> Option<&str> {
        match mesh {
            Usdt0Kind::Native => self.native_oft_address.as_deref(),
            Usdt0Kind::Legacy => self.legacy_oft_address.as_deref(),
        }
    }
}

/// Runtime registry of the source chain and all supported destinations.
/// Built once at service init from the USDT0 deployments API; stable for
/// the process lifetime.
#[derive(Clone, Debug)]
pub struct ChainRegistry {
    pub source: SourceSpec,
    pub destinations: HashMap<ChainId, ChainSpec>,
}

impl ChainRegistry {
    pub fn get(&self, id: &ChainId) -> Option<&ChainSpec> {
        self.destinations.get(id)
    }

    /// Whether `id` refers to the source chain (i.e. same-chain delivery,
    /// no OFT bridging needed).
    pub fn is_source(&self, id: &ChainId) -> bool {
        *id == self.source.id
    }

    /// All destination IDs, in arbitrary order.
    pub fn supported_chains(&self) -> Vec<ChainId> {
        self.destinations.keys().cloned().collect()
    }
}

/// Quote result returned to caller before committing to a swap.
#[derive(Clone, Debug, Serialize)]
pub struct PreparedSwap {
    pub destination_address: String,
    pub destination_chain: ChainId,
    /// Requested USDT output (6 decimals).
    pub usdt_amount: u64,
    /// Total sats to pay (includes all fees).
    pub invoice_amount_sats: u64,
    /// Boltz service fee in sats.
    pub boltz_fee_sats: u64,
    /// tBTC amount after Boltz fee (sats).
    pub estimated_onchain_amount: u64,
    pub slippage_bps: u32,
    /// Pins fee/rate snapshot for `POST /swap/reverse`.
    pub pair_hash: String,
    /// Quote expiry (unix timestamp seconds).
    pub expires_at: u64,
}

/// Result of creating a swap on Boltz.
#[derive(Clone, Debug, Serialize)]
pub struct CreatedSwap {
    /// Swap ID (Boltz backend ID).
    pub swap_id: String,
    /// Hold invoice to pay.
    pub invoice: String,
    pub invoice_amount_sats: u64,
    pub timeout_block_height: u64,
}

/// Result of a successfully completed swap.
#[derive(Clone, Debug, Serialize)]
pub struct CompletedSwap {
    pub swap_id: String,
    pub claim_tx_hash: String,
    /// Actual USDT amount delivered (6 decimals).
    pub usdt_delivered: u64,
    pub destination_address: String,
    pub destination_chain: ChainId,
}

/// Min/max swap limits from the Boltz pairs endpoint.
#[derive(Clone, Debug, Serialize)]
pub struct SwapLimits {
    pub min_sats: u64,
    pub max_sats: u64,
}

/// Summary of a recovery operation.
#[derive(Clone, Debug, Serialize)]
pub struct RecoveryResult {
    /// Swaps that were found and claimed.
    pub claimed: Vec<ClaimedRecovery>,
    /// Swaps found on-chain but already claimed/refunded.
    pub already_settled: u32,
    /// Total Lockup events scanned matching our claim address.
    pub total_events_scanned: u32,
    /// Highest key index found (for syncing the counter).
    pub highest_key_index: Option<u32>,
}

/// A single successfully claimed recovery.
#[derive(Clone, Debug, Serialize)]
pub struct ClaimedRecovery {
    pub key_index: u32,
    #[serde(serialize_with = "serialize_hex")]
    pub preimage_hash: [u8; 32],
    pub claim_tx_hash: String,
}

fn serialize_hex<S: serde::Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("0x{}", hex::encode(bytes)))
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;

    #[macros::test_all]
    fn test_swap_status_terminal() {
        assert!(!BoltzSwapStatus::Created.is_terminal());
        assert!(!BoltzSwapStatus::InvoicePaid.is_terminal());
        assert!(!BoltzSwapStatus::TbtcLocked.is_terminal());
        assert!(!BoltzSwapStatus::Claiming.is_terminal());
        assert!(BoltzSwapStatus::Completed.is_terminal());
        assert!(BoltzSwapStatus::Expired.is_terminal());
        assert!(
            BoltzSwapStatus::Failed {
                reason: "test".to_string()
            }
            .is_terminal()
        );
    }

    #[macros::test_all]
    fn test_boltz_swap_serialization() {
        let swap = BoltzSwap {
            id: "boltz-1".to_string(),
            status: BoltzSwapStatus::Created,
            claim_key_index: 0,
            chain_id: 42161,
            claim_address: "0xabc".to_string(),
            destination_address: "0xdef".to_string(),
            destination_chain: ChainId::new("arbitrum one"),
            refund_address: "0x123".to_string(),
            erc20swap_address: "0xswap".to_string(),
            router_address: "0xrouter".to_string(),
            invoice: "lnbc1000n1...".to_string(),
            invoice_amount_sats: 100_000,
            onchain_amount: 99_500,
            expected_usdt_amount: 71_000_000,
            slippage_bps: 100,
            timeout_block_height: 123_456,
            lockup_tx_id: None,
            claim_tx_hash: None,
            delivered_amount: None,
            lz_guid: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        };

        let json = serde_json::to_string(&swap).unwrap();
        let deserialized: BoltzSwap = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "boltz-1");
        assert_eq!(deserialized.status, BoltzSwapStatus::Created);
        assert_eq!(deserialized.chain_id, 42161);
        assert_eq!(deserialized.destination_chain.as_str(), "arbitrum one");
    }

    #[macros::test_all]
    fn chain_id_lowercases_on_construction() {
        assert_eq!(ChainId::new("Arbitrum One").as_str(), "arbitrum one");
        assert_eq!(ChainId::new("SOLANA").as_str(), "solana");
        assert_eq!(ChainId::new("tempo").as_str(), "tempo");
    }

    #[macros::test_all]
    fn chain_id_round_trips_via_serde() {
        let id = ChainId::new("Polygon PoS");
        let json = serde_json::to_string(&id).unwrap();
        // `#[serde(transparent)]` serialises as a bare string.
        assert_eq!(json, r#""polygon pos""#);
        let back: ChainId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
