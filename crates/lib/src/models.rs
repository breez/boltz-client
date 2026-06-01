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
    /// Gas-sponsor `call_id` for an in-flight claim, persisted right after
    /// `wallet_sendPreparedCalls` and before the confirming poll. If the
    /// process dies in that window the claim still mines, but the tx hash is
    /// not yet known locally; on resume the manager re-polls this `call_id` to
    /// recover the tx hash (and verify on-chain) instead of trusting the WS
    /// `invoice.settled` event. Cleared once `claim_tx_hash` is persisted.
    pub pending_call_id: Option<String>,
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

/// Which bridge carries a swap's Arbitrum -> destination leg.
/// `Oft` = `LayerZero` USDT0 (the original USDT path); `Cctp` = Circle CCTP v2
/// (USDC). Stored on the swap so the claim/recovery paths branch without
/// re-deriving. Defaults to `Oft` so swaps persisted before CCTP existed
/// deserialize correctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BridgeKind {
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
    /// Lowercased asset name; join key used as a [`ChainId`].
    pub id: &'static str,
    /// Asset identifier as published by the web app (e.g. `"USDC-BASE"`).
    pub asset: &'static str,
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
        asset: "USDC-BASE",
        transport: NetworkTransport::Evm,
        domain: 6,
        token_address: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    },
    CctpDestination {
        id: "usdc-eth",
        asset: "USDC-ETH",
        transport: NetworkTransport::Evm,
        domain: 0,
        token_address: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
    },
    CctpDestination {
        id: "usdc-avax",
        asset: "USDC-AVAX",
        transport: NetworkTransport::Evm,
        domain: 1,
        token_address: "0xB97EF9Ef8734C71904D8002F8b6Bc66Dd9c48a6E",
    },
    CctpDestination {
        id: "usdc-op",
        asset: "USDC-OP",
        transport: NetworkTransport::Evm,
        domain: 2,
        token_address: "0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85",
    },
    CctpDestination {
        id: "usdc-pol",
        asset: "USDC-POL",
        transport: NetworkTransport::Evm,
        domain: 7,
        token_address: "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359",
    },
    CctpDestination {
        id: "usdc-uni",
        asset: "USDC-UNI",
        transport: NetworkTransport::Evm,
        domain: 10,
        token_address: "0x078D782b760474a361dDA0AF3839290b0EF57AD6",
    },
    CctpDestination {
        id: "usdc-linea",
        asset: "USDC-LINEA",
        transport: NetworkTransport::Evm,
        domain: 11,
        token_address: "0x176211869cA2b568f2A7D4EE941E073a821EE1ff",
    },
    CctpDestination {
        id: "usdc-codex",
        asset: "USDC-CODEX",
        transport: NetworkTransport::Evm,
        domain: 12,
        token_address: "0xd996633a415985DBd7D6D12f4A4343E31f5037cf",
    },
    CctpDestination {
        id: "usdc-sonic",
        asset: "USDC-SONIC",
        transport: NetworkTransport::Evm,
        domain: 13,
        token_address: "0x29219dd400f2Bf60E5a23d13be72b486d4038894",
    },
    CctpDestination {
        id: "usdc-world",
        asset: "USDC-WORLD",
        transport: NetworkTransport::Evm,
        domain: 14,
        token_address: "0x79A02482A880bCe3F13E09da970dC34dB4cD24D1",
    },
    CctpDestination {
        id: "usdc-mon",
        asset: "USDC-MON",
        transport: NetworkTransport::Evm,
        domain: 15,
        token_address: "0x754704Bc059F8C67012fEd69BC8A327a5aafb603",
    },
    CctpDestination {
        id: "usdc-sei",
        asset: "USDC-SEI",
        transport: NetworkTransport::Evm,
        domain: 16,
        token_address: "0xe15fC38F6D8c56aF07bbCBe3BAf5708A2Bf42392",
    },
    CctpDestination {
        id: "usdc-xdc",
        asset: "USDC-XDC",
        transport: NetworkTransport::Evm,
        domain: 18,
        token_address: "0xfA2958CB79b0491CC627c1557F441eF849Ca8eb1",
    },
    CctpDestination {
        id: "usdc-ink",
        asset: "USDC-INK",
        transport: NetworkTransport::Evm,
        domain: 21,
        token_address: "0x2D270e6886d130D724215A266106e6832161EAEd",
    },
    CctpDestination {
        id: "usdc-plume",
        asset: "USDC-PLUME",
        transport: NetworkTransport::Evm,
        domain: 22,
        token_address: "0x222365EF19F7947e5484218551B56bb3965Aa7aF",
    },
    CctpDestination {
        id: "usdc-sol",
        asset: "USDC-SOL",
        transport: NetworkTransport::Solana,
        domain: 5,
        token_address: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    },
];

impl CctpDestination {
    /// Human-ish chain label derived from the asset name (`"USDC-BASE"` ->
    /// `"BASE"`), for discovery/UX listings.
    #[must_use]
    pub fn chain_label(&self) -> &'static str {
        self.asset.strip_prefix("USDC-").unwrap_or(self.asset)
    }
}

/// Resolve a CCTP destination by its [`ChainId`] (the lowercased asset name).
#[must_use]
pub fn cctp_destination(id: &ChainId) -> Option<&'static CctpDestination> {
    CCTP_DESTINATIONS.iter().find(|d| d.id == id.as_str())
}

/// A selectable swap destination, spanning both bridges. Returned by the
/// discovery API so callers can present USDT0 (OFT) and USDC (CCTP) options
/// uniformly; the `id` is what you pass to `prepare_reverse_swap`.
#[derive(Clone, Debug)]
pub struct DestinationOption {
    pub id: ChainId,
    /// Display label for the destination chain.
    pub label: String,
    /// Asset delivered there (`"USDT"`, `"USDT0"`, or `"USDC"`).
    pub asset: String,
    pub transport: NetworkTransport,
    pub bridge_kind: BridgeKind,
}

/// Quote result returned to caller before committing to a swap.
#[derive(Clone, Debug, Serialize)]
pub struct PreparedSwap {
    pub destination_address: String,
    pub destination_chain: ChainId,
    /// Which bridge will carry the destination leg (OFT for USDT, CCTP for USDC).
    pub bridge_kind: BridgeKind,
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
            bridge_kind: BridgeKind::Oft,
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
            pending_call_id: None,
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

    #[macros::test_all]
    fn bridge_kind_defaults_to_oft() {
        assert_eq!(BridgeKind::default(), BridgeKind::Oft);
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
            // ids are unique and already lowercased (valid ChainId join keys).
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
    fn cctp_destination_lookup_by_chain_id() {
        let base = cctp_destination(&ChainId::new("usdc-base")).unwrap();
        assert_eq!(base.asset, "USDC-BASE");
        assert_eq!(base.domain, 6);
        assert_eq!(base.transport, NetworkTransport::Evm);

        // Lookup is case-insensitive via ChainId normalization.
        let sol = cctp_destination(&ChainId::new("USDC-SOL")).unwrap();
        assert_eq!(sol.domain, 5);
        assert_eq!(sol.transport, NetworkTransport::Solana);

        // OFT chain ids must NOT resolve as CCTP destinations.
        assert!(cctp_destination(&ChainId::new("polygon pos")).is_none());
        assert!(cctp_destination(&ChainId::new("solana")).is_none());
    }
}
