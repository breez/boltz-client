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
    /// Which bridge carries the Arbitrum -> destination leg. Defaults to `Oft`
    /// for swaps persisted before CCTP support existed.
    #[serde(default)]
    pub bridge_kind: BridgeKind,
    /// HD derivation index for the per-swap preimage key.
    pub claim_key_index: u32,
    /// EVM chain ID (42161 for Arbitrum).
    pub chain_id: u64,

    // Addresses
    /// Gas signer address (used as claimAddress with Boltz).
    pub claim_address: String,
    /// User's final destination address for the delivered stablecoin.
    pub destination_address: String,
    /// Target destination (asset-on-chain) for delivery.
    pub destination_chain: DestinationId,
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
    /// Expected stablecoin output (6 decimals).
    #[serde(alias = "expected_usdt_amount")]
    pub expected_output_amount: u64,
    /// DEX slippage tolerance (basis points) snapshot at `prepare` time.
    /// Used for the claim-time quote drift check and on-chain `minOut`
    /// values so per-swap overrides survive across service restarts.
    pub slippage_bps: u32,

    // Timing
    pub timeout_block_height: u64,

    // Results
    pub lockup_tx_id: Option<String>,
    pub claim_tx_hash: Option<String>,
    /// Gas-sponsor `call_id` for an in-flight claim, persisted right after
    /// `wallet_sendPreparedCalls` and before the confirming poll. If the
    /// process dies in that window the claim still mines, but the tx hash is
    /// not yet known locally; on resume the manager re-polls this `call_id` to
    /// recover the tx hash (and verify on-chain) instead of trusting the WS
    /// `invoice.settled` event. Cleared once `claim_tx_hash` is persisted.
    pub pending_call_id: Option<String>,
    /// Actual stablecoin amount delivered on the destination chain (6
    /// decimals). `None` until the claim receipt is processed. For OFT
    /// destinations this is `amountReceivedLD`; for CCTP it's the attested
    /// delivered amount; for `Direct` delivery it's the final ERC20 `Transfer`
    /// value to the user.
    pub delivered_amount: Option<u64>,
    /// Cross-chain bridge tracking handle, set only for bridged swaps (`None`
    /// for `Direct` Arbitrum-destination swaps). Encoding depends on the
    /// bridge: for `Oft` it's the `LayerZero` message GUID (`0x`-prefixed hex,
    /// looked up on `LayerZero` Scan); for `Cctp` it's `"<source_domain>:<burn_tx_hash>"`,
    /// the key Circle Iris indexes the message by. Used to confirm destination
    /// delivery while the swap is `Settling`. (Serde alias `lz_guid` keeps
    /// swaps persisted before the rename readable.)
    #[serde(alias = "lz_guid")]
    pub bridge_ref: Option<String>,

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
    /// Claim mined on Arbitrum (burn/OFT-send committed), awaiting confirmation
    /// that the cross-chain bridge delivered on the destination chain. Only
    /// `Oft`/`Cctp` swaps pass through this state; `Direct` (same-chain)
    /// completes immediately. Non-terminal: the background manager polls the
    /// bridge's status API and advances to `Completed` once delivery confirms.
    Settling,
    /// Stablecoin delivered to destination.
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

/// Opaque, stable identifier for a selectable swap destination — an
/// *asset-on-chain*, not a bare chain (e.g. `"arbitrum one"` = USDT on
/// Arbitrum, `"usdc-base"` = USDC on Base, `"usdc-arb"` = USDC on Arbitrum).
/// Callers round-trip the `id` from [`DestinationOption`] back into the
/// prepare API and never construct it by hand. Held lowercased; build via
/// [`DestinationId::new`] for the canonical form.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DestinationId(String);

impl DestinationId {
    /// Build a `DestinationId` from any string, lowercasing to canonical form.
    pub fn new(name: impl AsRef<str>) -> Self {
        Self(name.as_ref().to_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DestinationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for DestinationId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Stablecoin the user receives on the destination. First-class dimension:
/// the same physical chain can offer more than one (USDT0 via OFT *and* USDC
/// via CCTP), so asset is tracked independently of the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Asset {
    /// Canonical Tether.
    Usdt,
    /// `LayerZero` USDT0 (distinct ERC20/SPL from canonical Tether).
    Usdt0,
    /// Circle USD Coin.
    Usdc,
}

impl Asset {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Asset::Usdt => "USDT",
            Asset::Usdt0 => "USDT0",
            Asset::Usdc => "USDC",
        }
    }
}

impl std::fmt::Display for Asset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a destination's Arbitrum DEX output reaches the user. Carries the
/// per-bridge routing data the claim path needs. Internal to the registry and
/// claim paths; the public API surfaces only the coarse [`BridgeKind`].
#[derive(Clone, Debug)]
pub enum Bridge {
    /// Delivered on Arbitrum itself — no cross-chain hop. The DEX output is
    /// swept straight to the user (both same-chain USDT and USDC-on-Arbitrum).
    Direct,
    /// `LayerZero` USDT0 OFT cross-chain bridge.
    Oft {
        /// Which USDT0 mesh this destination belongs to (selects the
        /// source-side OFT contract via [`DestinationRegistry::oft_for`]).
        mesh: Usdt0Kind,
        /// `LayerZero` endpoint ID for the destination.
        lz_eid: u32,
    },
    /// Circle CCTP v2 burn + mint.
    Cctp {
        /// Circle CCTP domain id of the destination chain.
        domain: u32,
    },
}

impl Bridge {
    /// Coarse public category for this bridge.
    #[must_use]
    pub fn kind(&self) -> BridgeKind {
        match self {
            Bridge::Direct => BridgeKind::Direct,
            Bridge::Oft { .. } => BridgeKind::Oft,
            Bridge::Cctp { .. } => BridgeKind::Cctp,
        }
    }
}

/// A single selectable destination: an asset delivered on a chain via a
/// specific bridge. Unifies what used to be two parallel registries (OFT
/// `ChainSpec` and static `CctpDestination`). Built once at service init.
#[derive(Clone, Debug)]
pub struct Destination {
    /// Opaque join key / caller-facing handle.
    pub id: DestinationId,
    /// Human chain label for display (`"Arbitrum"`, `"Base"`, `"Solana"`).
    pub chain_label: String,
    /// Asset the user receives.
    pub asset: Asset,
    pub transport: NetworkTransport,
    /// EVM chain ID. `None` for non-EVM transports (Solana, Tron).
    pub evm_chain_id: Option<u64>,
    /// Arbitrum token the DEX leg must produce before the bridge/delivery
    /// (`ARBITRUM_USDT_ADDRESS` for USDT/USDT0 routes, `ARBITRUM_USDC_ADDRESS`
    /// for USDC routes).
    pub dex_output_token: &'static str,
    /// Token contract on the *destination* chain (`0x…` EVM, base58 Solana),
    /// when known. Used for the "don't send to a token contract" guard.
    pub dest_token_address: Option<String>,
    pub bridge: Bridge,
}

impl Destination {
    /// OFT routing data `(mesh, lz_eid)`, present only for `Bridge::Oft`.
    #[must_use]
    pub fn oft(&self) -> Option<(Usdt0Kind, u32)> {
        match &self.bridge {
            Bridge::Oft { mesh, lz_eid } => Some((*mesh, *lz_eid)),
            _ => None,
        }
    }
}

/// Runtime registry of every supported destination across all bridges, plus
/// the source-chain OFT contracts. Built once at service init by merging the
/// USDT0 deployments API, the static [`CCTP_DESTINATIONS`] table, and the
/// Arbitrum-direct entries; stable for the process lifetime.
#[derive(Clone, Debug)]
pub struct DestinationRegistry {
    /// Source-chain destination id (Arbitrum USDT direct).
    pub source_id: DestinationId,
    pub source_evm_chain_id: u64,
    /// Source-side native-mesh OFT contract (`None` if not on the native mesh).
    pub source_native_oft: Option<String>,
    /// Source-side legacy-mesh OFT contract (`None` if not on the legacy mesh).
    pub source_legacy_oft: Option<String>,
    pub destinations: HashMap<DestinationId, Destination>,
}

impl DestinationRegistry {
    #[must_use]
    pub fn get(&self, id: &DestinationId) -> Option<&Destination> {
        self.destinations.get(id)
    }

    /// Source OFT contract for the given mesh, or `None` if the source chain
    /// doesn't participate in that mesh.
    #[must_use]
    pub fn oft_for(&self, mesh: Usdt0Kind) -> Option<&str> {
        match mesh {
            Usdt0Kind::Native => self.source_native_oft.as_deref(),
            Usdt0Kind::Legacy => self.source_legacy_oft.as_deref(),
        }
    }
}

/// Coarse public category of a swap's Arbitrum -> destination leg, for
/// display and delivery-status UX (`Cctp` → Circle Iris, `Oft` → `LayerZero`
/// GUID, `Direct` → none). The data-carrying detail lives in [`Bridge`];
/// claim dispatch resolves that from the destination, not this field.
///
/// - `Direct` — delivered on Arbitrum, no cross-chain hop (USDT or USDC).
/// - `Oft`    — `LayerZero` USDT0 bridge.
/// - `Cctp`   — Circle CCTP v2 (USDC).
///
/// Defaults to `Oft` so swaps persisted before CCTP/Direct existed deserialize
/// correctly (a missing field means a pre-CCTP OFT swap).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BridgeKind {
    Direct,
    #[default]
    Oft,
    Cctp,
}

/// A USDC (CCTP) destination chain. Static compile-time table — CCTP routes
/// are not published by the USDT0 deployments API. `id` is the lowercased
/// asset name (e.g. `"usdc-base"`), kept distinct from OFT chain ids
/// (`"polygon pos"`, `"solana"`, ...) so the two destination spaces never
/// collide for chains that support both bridges.
#[derive(Clone, Debug)]
pub struct CctpDestination {
    /// Lowercased asset name; join key used as a [`DestinationId`]. The
    /// web-app asset identifier (e.g. `"USDC-BASE"`) is just its uppercase.
    pub id: &'static str,
    /// Human chain label for display (`"Base"`, `"Optimism"`). Matches the
    /// USDT0 deployments-API spelling for chains that also support OFT, so a
    /// chain never appears under two different names across the two bridges.
    pub chain_label: &'static str,
    pub transport: NetworkTransport,
    /// Circle CCTP domain id of the destination chain.
    pub domain: u32,
    /// USDC token contract on the destination chain (EVM `0x…`, Solana base58
    /// mint). Used to decode the delivered amount during recovery.
    pub token_address: &'static str,
}

/// Static registry of USDC (CCTP) destinations, mirroring boltz-web-app
/// `boltz-swaps` `cctp/variants.ts`. Addresses and Circle domains are verified
/// against that source.
pub const CCTP_DESTINATIONS: &[CctpDestination] = &[
    CctpDestination {
        id: "usdc-base",
        chain_label: "Base",
        transport: NetworkTransport::Evm,
        domain: 6,
        token_address: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    },
    CctpDestination {
        id: "usdc-eth",
        chain_label: "Ethereum",
        transport: NetworkTransport::Evm,
        domain: 0,
        token_address: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
    },
    CctpDestination {
        id: "usdc-avax",
        chain_label: "Avalanche",
        transport: NetworkTransport::Evm,
        domain: 1,
        token_address: "0xB97EF9Ef8734C71904D8002F8b6Bc66Dd9c48a6E",
    },
    CctpDestination {
        id: "usdc-op",
        chain_label: "Optimism",
        transport: NetworkTransport::Evm,
        domain: 2,
        token_address: "0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85",
    },
    CctpDestination {
        id: "usdc-pol",
        chain_label: "Polygon PoS",
        transport: NetworkTransport::Evm,
        domain: 7,
        token_address: "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359",
    },
    CctpDestination {
        id: "usdc-uni",
        chain_label: "Unichain",
        transport: NetworkTransport::Evm,
        domain: 10,
        token_address: "0x078D782b760474a361dDA0AF3839290b0EF57AD6",
    },
    CctpDestination {
        id: "usdc-linea",
        chain_label: "Linea",
        transport: NetworkTransport::Evm,
        domain: 11,
        token_address: "0x176211869cA2b568f2A7D4EE941E073a821EE1ff",
    },
    CctpDestination {
        id: "usdc-codex",
        chain_label: "Codex",
        transport: NetworkTransport::Evm,
        domain: 12,
        token_address: "0xd996633a415985DBd7D6D12f4A4343E31f5037cf",
    },
    CctpDestination {
        id: "usdc-sonic",
        chain_label: "Sonic",
        transport: NetworkTransport::Evm,
        domain: 13,
        token_address: "0x29219dd400f2Bf60E5a23d13be72b486d4038894",
    },
    CctpDestination {
        id: "usdc-world",
        chain_label: "World Chain",
        transport: NetworkTransport::Evm,
        domain: 14,
        token_address: "0x79A02482A880bCe3F13E09da970dC34dB4cD24D1",
    },
    CctpDestination {
        id: "usdc-mon",
        chain_label: "Monad",
        transport: NetworkTransport::Evm,
        domain: 15,
        token_address: "0x754704Bc059F8C67012fEd69BC8A327a5aafb603",
    },
    CctpDestination {
        id: "usdc-sei",
        chain_label: "Sei",
        transport: NetworkTransport::Evm,
        domain: 16,
        token_address: "0xe15fC38F6D8c56aF07bbCBe3BAf5708A2Bf42392",
    },
    CctpDestination {
        id: "usdc-xdc",
        chain_label: "XDC",
        transport: NetworkTransport::Evm,
        domain: 18,
        token_address: "0xfA2958CB79b0491CC627c1557F441eF849Ca8eb1",
    },
    CctpDestination {
        id: "usdc-ink",
        chain_label: "Ink",
        transport: NetworkTransport::Evm,
        domain: 21,
        token_address: "0x2D270e6886d130D724215A266106e6832161EAEd",
    },
    CctpDestination {
        id: "usdc-plume",
        chain_label: "Plume",
        transport: NetworkTransport::Evm,
        domain: 22,
        token_address: "0x222365EF19F7947e5484218551B56bb3965Aa7aF",
    },
    CctpDestination {
        id: "usdc-sol",
        chain_label: "Solana",
        transport: NetworkTransport::Solana,
        domain: 5,
        token_address: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    },
];

/// Resolve a CCTP destination by its [`DestinationId`] (the lowercased asset
/// name, e.g. `"usdc-base"`).
#[must_use]
pub fn cctp_destination(id: &DestinationId) -> Option<&'static CctpDestination> {
    CCTP_DESTINATIONS.iter().find(|d| d.id == id.as_str())
}

/// A selectable swap destination. Returned by the discovery API so callers can
/// present every asset/chain/bridge combination uniformly; round-trip the `id`
/// back into `prepare_reverse_swap` (never construct it by hand).
#[derive(Clone, Debug)]
pub struct DestinationOption {
    pub id: DestinationId,
    /// Human chain label for display (`"Arbitrum"`, `"Base"`, `"Solana"`).
    pub chain_label: String,
    /// Asset delivered there.
    pub asset: Asset,
    pub transport: NetworkTransport,
    /// Coarse bridge category (for delivery-status UX).
    pub bridge_kind: BridgeKind,
}

/// Quote result returned to caller before committing to a swap.
#[derive(Clone, Debug, Serialize)]
pub struct PreparedSwap {
    pub destination_address: String,
    pub destination_chain: DestinationId,
    /// Coarse bridge category for the destination leg.
    pub bridge_kind: BridgeKind,
    /// Requested stablecoin output (6 decimals).
    pub output_amount: u64,
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
    /// Actual stablecoin amount delivered (6 decimals).
    pub output_delivered: u64,
    pub destination_address: String,
    pub destination_chain: DestinationId,
}

/// Min/max swap limits from the Boltz pairs endpoint.
#[derive(Clone, Debug, Serialize)]
pub struct SwapLimits {
    pub min_sats: u64,
    pub max_sats: u64,
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
        assert!(!BoltzSwapStatus::Settling.is_terminal());
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
            bridge_kind: BridgeKind::Oft,
            claim_key_index: 0,
            chain_id: 42161,
            claim_address: "0xabc".to_string(),
            destination_address: "0xdef".to_string(),
            destination_chain: DestinationId::new("arbitrum one"),
            refund_address: "0x123".to_string(),
            erc20swap_address: "0xswap".to_string(),
            router_address: "0xrouter".to_string(),
            invoice: "lnbc1000n1...".to_string(),
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
        };

        let json = serde_json::to_string(&swap).unwrap();
        let deserialized: BoltzSwap = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "boltz-1");
        assert_eq!(deserialized.status, BoltzSwapStatus::Created);
        assert_eq!(deserialized.chain_id, 42161);
        assert_eq!(deserialized.destination_chain.as_str(), "arbitrum one");
    }

    /// Swaps persisted before the `lz_guid` -> `bridge_ref` rename used the
    /// `lz_guid` JSON key; the serde alias must keep them readable.
    #[macros::test_all]
    fn bridge_ref_deserializes_from_legacy_lz_guid_key() {
        let legacy = r#"{
            "id": "boltz-1", "status": "Created", "claim_key_index": 0,
            "chain_id": 42161, "claim_address": "0xabc", "destination_address": "0xdef",
            "destination_chain": "arbitrum one", "refund_address": "0x123",
            "erc20swap_address": "0xswap", "router_address": "0xrouter",
            "invoice": "lnbc", "invoice_amount_sats": 100000, "onchain_amount": 99500,
            "expected_output_amount": 71000000, "slippage_bps": 100,
            "timeout_block_height": 123456, "lockup_tx_id": null, "claim_tx_hash": null,
            "pending_call_id": null, "delivered_amount": null,
            "lz_guid": "0xdeadbeef", "created_at": 1700000000, "updated_at": 1700000000
        }"#;
        let swap: BoltzSwap = serde_json::from_str(legacy).unwrap();
        assert_eq!(swap.bridge_ref.as_deref(), Some("0xdeadbeef"));
    }

    /// A `Settling` swap with a `bridge_ref` must round-trip (new variant +
    /// new field name on the way out, not just the legacy alias inward).
    #[macros::test_all]
    fn settling_swap_with_bridge_ref_round_trips() {
        let mut swap = BoltzSwap {
            id: "s2".to_string(),
            status: BoltzSwapStatus::Settling,
            bridge_kind: BridgeKind::Cctp,
            claim_key_index: 0,
            chain_id: 42161,
            claim_address: "0xabc".to_string(),
            destination_address: "0xdef".to_string(),
            destination_chain: DestinationId::new("usdc-base"),
            refund_address: "0x123".to_string(),
            erc20swap_address: "0xswap".to_string(),
            router_address: "0xrouter".to_string(),
            invoice: "lnbc".to_string(),
            invoice_amount_sats: 100_000,
            onchain_amount: 99_500,
            expected_output_amount: 71_000_000,
            slippage_bps: 100,
            timeout_block_height: 123_456,
            lockup_tx_id: None,
            claim_tx_hash: None,
            pending_call_id: None,
            delivered_amount: None,
            bridge_ref: Some("6:0xburn".to_string()),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        };
        let json = serde_json::to_string(&swap).unwrap();
        assert!(json.contains("\"bridge_ref\":\"6:0xburn\""));
        let back: BoltzSwap = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, BoltzSwapStatus::Settling);
        assert_eq!(back.bridge_ref.as_deref(), Some("6:0xburn"));
        // Sanity: still non-terminal.
        swap.status = back.status;
        assert!(!swap.status.is_terminal());
    }

    #[macros::test_all]
    fn destination_id_lowercases_on_construction() {
        assert_eq!(DestinationId::new("Arbitrum One").as_str(), "arbitrum one");
        assert_eq!(DestinationId::new("SOLANA").as_str(), "solana");
        assert_eq!(DestinationId::new("USDC-ARB").as_str(), "usdc-arb");
    }

    #[macros::test_all]
    fn destination_id_round_trips_via_serde() {
        let id = DestinationId::new("Polygon PoS");
        let json = serde_json::to_string(&id).unwrap();
        // `#[serde(transparent)]` serialises as a bare string.
        assert_eq!(json, r#""polygon pos""#);
        let back: DestinationId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[macros::test_all]
    fn bridge_kind_defaults_to_oft() {
        // Pre-CCTP swaps have no `bridge_kind` field; it must default to Oft.
        assert_eq!(BridgeKind::default(), BridgeKind::Oft);
    }

    #[macros::test_all]
    fn bridge_kind_back_compat_deserializes() {
        // Old persisted values must still deserialize after adding `Direct`.
        assert_eq!(
            serde_json::from_str::<BridgeKind>(r#""Oft""#).unwrap(),
            BridgeKind::Oft
        );
        assert_eq!(
            serde_json::from_str::<BridgeKind>(r#""Cctp""#).unwrap(),
            BridgeKind::Cctp
        );
    }

    #[macros::test_all]
    fn bridge_kind_from_bridge() {
        assert_eq!(Bridge::Direct.kind(), BridgeKind::Direct);
        assert_eq!(
            Bridge::Oft {
                mesh: Usdt0Kind::Native,
                lz_eid: 30110
            }
            .kind(),
            BridgeKind::Oft
        );
        assert_eq!(Bridge::Cctp { domain: 6 }.kind(), BridgeKind::Cctp);
    }

    #[macros::test_all]
    fn cctp_destinations_table_is_well_formed() {
        use std::collections::HashSet;

        // 15 EVM destinations + Solana.
        assert_eq!(CCTP_DESTINATIONS.len(), 16);

        let mut ids = HashSet::new();
        let mut domains = HashSet::new();
        let mut solana_count = 0;
        for d in CCTP_DESTINATIONS {
            // ids are unique and already lowercased (valid DestinationId keys).
            assert!(ids.insert(d.id), "duplicate id {}", d.id);
            assert_eq!(d.id, d.id.to_lowercase());
            // Circle domains are unique per destination.
            assert!(domains.insert(d.domain), "duplicate domain {}", d.domain);
            match d.transport {
                NetworkTransport::Evm => {
                    assert!(d.token_address.starts_with("0x"));
                    assert_eq!(d.token_address.len(), 42);
                }
                NetworkTransport::Solana => {
                    solana_count += 1;
                    assert!(!d.token_address.starts_with("0x"));
                }
                NetworkTransport::Tron => panic!("CCTP does not support Tron"),
            }
        }
        // Exactly one Solana destination (USDC-SOL).
        assert_eq!(solana_count, 1);
    }

    #[macros::test_all]
    fn cctp_destination_lookup_by_destination_id() {
        let base = cctp_destination(&DestinationId::new("usdc-base")).unwrap();
        assert_eq!(base.chain_label, "Base");
        assert_eq!(base.domain, 6);
        assert_eq!(base.transport, NetworkTransport::Evm);

        // Lookup is case-insensitive via DestinationId normalization.
        let sol = cctp_destination(&DestinationId::new("USDC-SOL")).unwrap();
        assert_eq!(sol.domain, 5);
        assert_eq!(sol.transport, NetworkTransport::Solana);

        // OFT destination ids must NOT resolve as CCTP destinations.
        assert!(cctp_destination(&DestinationId::new("polygon pos")).is_none());
        assert!(cctp_destination(&DestinationId::new("solana")).is_none());
    }
}
