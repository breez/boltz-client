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
    pub destination_chain: Chain,
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

    // Timing
    pub timeout_block_height: u64,

    // Results
    pub lockup_tx_id: Option<String>,
    pub claim_tx_hash: Option<String>,

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
/// dispatch, and OFT registry lookup keying.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum NetworkTransport {
    Evm,
    Solana,
    Tron,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Chain {
    Arbitrum,
    Berachain,
    Conflux,
    Corn,
    Ethereum,
    Flare,
    Hedera,
    HyperEvm,
    Ink,
    Mantle,
    MegaEth,
    Monad,
    Morph,
    Optimism,
    Plasma,
    Polygon,
    Rootstock,
    Sei,
    Solana,
    Stable,
    Tempo,
    Tron,
    Unichain,
    XLayer,
}

impl Chain {
    /// EVM chain ID for this chain. `None` for non-EVM transports
    /// (Solana, Tron), which the USDT0 deployments API returns with
    /// `chainId: null`.
    pub fn evm_chain_id(&self) -> Option<u64> {
        match self {
            Self::Arbitrum => Some(42161),
            Self::Berachain => Some(80094),
            Self::Conflux => Some(1030),
            Self::Corn => Some(21_000_000),
            Self::Ethereum => Some(1),
            Self::Flare => Some(14),
            Self::Hedera => Some(295),
            Self::HyperEvm => Some(999),
            Self::Ink => Some(57073),
            Self::Mantle => Some(5000),
            Self::MegaEth => Some(4326),
            Self::Monad => Some(143),
            Self::Morph => Some(2818),
            Self::Optimism => Some(10),
            Self::Plasma => Some(9745),
            Self::Polygon => Some(137),
            Self::Rootstock => Some(30),
            Self::Sei => Some(1329),
            Self::Stable => Some(988),
            Self::Tempo => Some(4217),
            Self::Unichain => Some(130),
            Self::XLayer => Some(196),
            Self::Solana | Self::Tron => None,
        }
    }

    /// Underlying network transport.
    pub fn transport(&self) -> NetworkTransport {
        match self {
            Self::Solana => NetworkTransport::Solana,
            Self::Tron => NetworkTransport::Tron,
            _ => NetworkTransport::Evm,
        }
    }

    /// Lowercased chain name used as the secondary key in the USDT0 OFT
    /// registry for chains where `chainId` is null.
    pub fn registry_name(&self) -> &'static str {
        match self {
            Self::Arbitrum => "arbitrum",
            Self::Berachain => "berachain",
            Self::Conflux => "conflux",
            Self::Corn => "corn",
            Self::Ethereum => "ethereum",
            Self::Flare => "flare",
            Self::Hedera => "hedera",
            Self::HyperEvm => "hyperevm",
            Self::Ink => "ink",
            Self::Mantle => "mantle",
            Self::MegaEth => "megaeth",
            Self::Monad => "monad",
            Self::Morph => "morph",
            Self::Optimism => "optimism",
            Self::Plasma => "plasma",
            Self::Polygon => "polygon",
            Self::Rootstock => "rootstock",
            Self::Sei => "sei",
            Self::Stable => "stable",
            Self::Tempo => "tempo",
            Self::Unichain => "unichain",
            Self::XLayer => "xlayer",
            Self::Solana => "solana",
            Self::Tron => "tron",
        }
    }

    /// Whether this is the source chain (Arbitrum) where claims happen on-chain.
    /// Non-Arbitrum destinations require OFT cross-chain bridging.
    pub fn is_source_chain(&self) -> bool {
        *self == Self::Arbitrum
    }
}

/// Quote result returned to caller before committing to a swap.
#[derive(Clone, Debug, Serialize)]
pub struct PreparedSwap {
    pub destination_address: String,
    pub destination_chain: Chain,
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
    pub destination_chain: Chain,
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
            destination_chain: Chain::Arbitrum,
            refund_address: "0x123".to_string(),
            erc20swap_address: "0xswap".to_string(),
            router_address: "0xrouter".to_string(),
            invoice: "lnbc1000n1...".to_string(),
            invoice_amount_sats: 100_000,
            onchain_amount: 99_500,
            expected_usdt_amount: 71_000_000,
            timeout_block_height: 123_456,
            lockup_tx_id: None,
            claim_tx_hash: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        };

        let json = serde_json::to_string(&swap).unwrap();
        let deserialized: BoltzSwap = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "boltz-1");
        assert_eq!(deserialized.status, BoltzSwapStatus::Created);
        assert_eq!(deserialized.chain_id, 42161);
    }

    #[macros::test_all]
    fn test_chain_equality() {
        assert_eq!(Chain::Arbitrum, Chain::Arbitrum);
        assert_ne!(Chain::Arbitrum, Chain::Ethereum);
    }

    #[macros::test_all]
    fn test_evm_chain_id() {
        assert_eq!(Chain::Arbitrum.evm_chain_id(), Some(42161));
        assert_eq!(Chain::Ethereum.evm_chain_id(), Some(1));
        assert_eq!(Chain::Optimism.evm_chain_id(), Some(10));
        assert_eq!(Chain::Polygon.evm_chain_id(), Some(137));
        assert_eq!(Chain::Tempo.evm_chain_id(), Some(4217));
        assert_eq!(Chain::Solana.evm_chain_id(), None);
        assert_eq!(Chain::Tron.evm_chain_id(), None);
    }

    #[macros::test_all]
    fn test_is_source_chain() {
        assert!(Chain::Arbitrum.is_source_chain());
        assert!(!Chain::Ethereum.is_source_chain());
        assert!(!Chain::Optimism.is_source_chain());
        assert!(!Chain::Solana.is_source_chain());
        assert!(!Chain::Tron.is_source_chain());
    }

    #[macros::test_all]
    fn test_transport() {
        assert_eq!(Chain::Arbitrum.transport(), NetworkTransport::Evm);
        assert_eq!(Chain::Ethereum.transport(), NetworkTransport::Evm);
        assert_eq!(Chain::Tempo.transport(), NetworkTransport::Evm);
        assert_eq!(Chain::Solana.transport(), NetworkTransport::Solana);
        assert_eq!(Chain::Tron.transport(), NetworkTransport::Tron);
    }

    #[macros::test_all]
    fn test_registry_name() {
        assert_eq!(Chain::Arbitrum.registry_name(), "arbitrum");
        assert_eq!(Chain::Ethereum.registry_name(), "ethereum");
        assert_eq!(Chain::Tempo.registry_name(), "tempo");
        assert_eq!(Chain::Solana.registry_name(), "solana");
        assert_eq!(Chain::Tron.registry_name(), "tron");
    }
}
