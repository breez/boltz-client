/// Configuration for the Boltz service.
#[derive(Clone, Debug)]
pub struct BoltzConfig {
    /// Boltz API base URL WITHOUT /v2 suffix (e.g. `https://api.boltz.exchange`).
    /// Endpoint paths include the /v2 prefix (e.g. "/v2/swap/reverse").
    /// WS URL is derived as: `wss://{host}/v2/ws`
    pub api_url: String,
    /// Alchemy configuration for gas abstraction.
    pub alchemy_config: AlchemyConfig,
    /// Arbitrum JSON-RPC URL for read-only operations (contract state, logs).
    /// Keeps Alchemy exclusively for gas-abstracted writes.
    pub arbitrum_rpc_url: String,
    /// EVM chain ID (42161 for Arbitrum One).
    pub chain_id: u64,
    /// Referral ID — sent as an HTTP header on every request and as the
    /// `referralId` field in swap creation requests (attribution tracking).
    pub referral_id: String,
    /// User-facing slippage tolerance in basis points (default: 100 = 1%).
    /// Anchored on the prepare-time quote: a claim only proceeds if the
    /// user is guaranteed to receive at least
    /// `expected * (1 - slippage_bps / 10000)` on the destination chain.
    /// Drift between prepare and claim, internal fee buffers, and OFT
    /// fees all surface either as a normal completion above this floor
    /// or as a `QuoteDegraded` event — never as a quiet under-delivery.
    pub slippage_bps: u32,
    /// URL for fetching OFT (USDT0) deployment data.
    pub oft_deployments_url: String,
    /// Circle CCTP "Iris" API base URL. Used to quote the CCTP burn fee at
    /// prepare time and to fetch the attestation / forwarding tx hash when
    /// checking delivery status for USDC (CCTP) destinations. Sandbox:
    /// `https://iris-api-sandbox.circle.com`.
    pub cctp_api_url: String,
    /// Solana JSON-RPC endpoint used when the destination chain is Solana.
    /// Queried to check whether the recipient's `Associated Token Account`
    /// already exists so the cross-chain message can pre-fund its creation
    /// when it doesn't. Unused for EVM and Tron destinations.
    pub solana_rpc_url: String,
    /// `LayerZero` Scan API base URL. Used to confirm OFT (USDT0) cross-chain
    /// delivery by message GUID so a bridged swap advances from `Settling` to
    /// `Completed`. Mainnet default: `https://scan.layerzero-api.com`.
    pub lz_scan_api_url: String,
    /// Cadence (seconds) at which the background manager polls Circle Iris
    /// (CCTP) / `LayerZero` Scan (OFT) to advance `Settling` swaps to
    /// `Completed`. `None` disables background polling entirely — callers then
    /// drive confirmation on demand via
    /// [`crate::BoltzService::refresh_pending_deliveries`]. Default: 30s.
    pub delivery_poll_interval_secs: Option<u64>,
}

/// Alchemy configuration for EIP-7702 gas abstraction.
#[derive(Clone, Debug)]
pub struct AlchemyConfig {
    /// Gas-sponsor endpoint URL. All `wallet_*` gas-abstraction RPC calls
    /// (`wallet_prepareCalls`, `wallet_sendPreparedCalls`,
    /// `wallet_getCallsStatus`) are sent here. The sponsor wraps Alchemy's
    /// gas-abstraction API server-side and applies the sponsorship policy, so
    /// no Alchemy API key or gas-policy id is held client-side.
    pub gas_sponsor_url: String,
}

impl BoltzConfig {
    /// Returns a default configuration for Arbitrum mainnet.
    ///
    /// `alchemy_config.gas_sponsor_url` is populated with the Boltz-operated
    /// default ([`DEFAULT_GAS_SPONSOR_URL`]). Callers that operate their own
    /// gas sponsor can override it on the returned struct.
    pub fn mainnet(referral_id: String) -> Self {
        Self {
            api_url: "https://api.boltz.exchange".to_string(),
            alchemy_config: AlchemyConfig {
                gas_sponsor_url: DEFAULT_GAS_SPONSOR_URL.to_string(),
            },
            arbitrum_rpc_url: "https://arb1.arbitrum.io/rpc".to_string(),
            chain_id: ARBITRUM_CHAIN_ID,
            referral_id,
            slippage_bps: DEFAULT_SLIPPAGE_BPS,
            oft_deployments_url: DEFAULT_OFT_DEPLOYMENTS_URL.to_string(),
            cctp_api_url: DEFAULT_CCTP_API_URL.to_string(),
            solana_rpc_url: DEFAULT_SOLANA_RPC_URL.to_string(),
            lz_scan_api_url: DEFAULT_LZ_SCAN_API_URL.to_string(),
            delivery_poll_interval_secs: Some(DEFAULT_DELIVERY_POLL_INTERVAL_SECS),
        }
    }

    /// Derives the WebSocket URL from the API URL.
    /// Converts http(s):// to ws(s):// and appends /v2/ws.
    pub fn ws_url(&self) -> String {
        let ws_base = self
            .api_url
            .replace("https://", "wss://")
            .replace("http://", "ws://");
        format!("{ws_base}/v2/ws")
    }
}

// Chain constants for Arbitrum One
pub const ARBITRUM_CHAIN_ID: u64 = 42161;

/// Polygon `PoS` EVM chain ID. Used only to flag OFT sends to Polygon, which
/// need a temporary `lzReceive` gas bump (see
/// [`crate::evm::lz_options::POLYGON_LZ_RECEIVE_GAS_BUMP`]).
pub const POLYGON_EVM_CHAIN_ID: u64 = 137;

/// Default slippage tolerance: 100 basis points = 1%.
pub const DEFAULT_SLIPPAGE_BPS: u32 = 100;

/// Maximum slippage tolerance: 500 basis points = 5%.
/// Matches the Boltz web app's upper bound.
pub const MAX_SLIPPAGE_BPS: u32 = 500;

/// Minimum required headroom, in **L1** (Ethereum) blocks, between the lockup
/// `timeout_block_height` and the current L1 height before we proceed with a
/// swap. `timeout_block_height` is denominated in L1 block height (Solidity
/// `block.number` on Arbitrum returns the L1 number), so the comparison must
/// use [`crate::evm::provider::EvmProvider::eth_l1_block_number`], never
/// `eth_blockNumber` (which returns the L2 number).
///
/// At ~12s per L1 block, 60 blocks ≈ 12 minutes — enough to cover the full
/// claim pipeline (WS confirmation, on-chain lockup re-checks, gas-sponsor
/// submission and inclusion, receipt polling) with anti-race headroom. Boltz's
/// honest reverse-swap timeout is ~7200 L1 blocks (~24h), so this floor never
/// rejects a legitimate swap; it only catches a malicious/buggy server that
/// returns a too-short timeout to win a refund-vs-claim race and steal the
/// preimage. Used both as an early abort in `create()` and — as the load-
/// bearing, fail-safe gate — immediately before the preimage is revealed at
/// claim time.
pub const MIN_TIMEOUT_L1_MARGIN: u64 = 60;

/// Default URL for fetching OFT (USDT0) deployment data.
pub const DEFAULT_OFT_DEPLOYMENTS_URL: &str = "https://docs.usdt0.to/api/deployments";

/// Default Boltz-operated gas-sponsor endpoint. Wraps Alchemy's
/// gas-abstraction API server-side and applies the sponsorship policy, so the
/// client holds no Alchemy API key or gas-policy id. Callers may override
/// `alchemy_config.gas_sponsor_url` to point at their own sponsor.
pub const DEFAULT_GAS_SPONSOR_URL: &str = "https://sponsor.ccxp.space/";

/// Router contract address on Arbitrum — not available via the Boltz API.
/// If upgraded, the old contract address remains valid.
///
/// boltz-core v5.0.0 deployment. Upgraded from the v4.0.3 deployment
/// (`0x6EA68e965fcd19b6fbC6553BABbF87a5018F9B28`) to gain `claimERC20ExecuteCctp`
/// (CCTP support; added in core v4.0.5). The OFT claim path is unaffected: the
/// `claimERC20ExecuteOft` signature, the `Erc20Claim`/`Call`/`SendData`/
/// `ClaimSendAuthorization` structs, every OFT EIP-712 typehash, and the
/// `{name: "Router", version: "2"}` domain are byte-identical between v4.0.3 and
/// v5.0.0 — only the `verifyingContract` (this address, passed dynamically into
/// the EIP-712 domain) changes.
pub const ARBITRUM_ROUTER_ADDRESS: &str = "0x182589d2A10384e12EE8C1Fe350F4dfba36C7b73";

/// tBTC token address on Arbitrum.
pub const ARBITRUM_TBTC_ADDRESS: &str = "0x6c84a8f1c29108F47a79964b5Fe888D4f4D0dE40";

/// USDT token address on Arbitrum.
pub const ARBITRUM_USDT_ADDRESS: &str = "0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9";

/// USDC token address on Arbitrum (6 decimals). The DEX leg trades tBTC into
/// this token, which the Router then burns via CCTP for USDC destinations.
pub const ARBITRUM_USDC_ADDRESS: &str = "0xaf88d065e77c8cC2239327C5EDb3A432268e5831";

/// Pinned Arbitrum source OFT contracts (USDT0). The Router grants a USDT
/// approval to — and calls `send` on — whichever of these the selected mesh
/// resolves to during a cross-chain claim. They are the *only* values taken
/// from the USDT0 deployments feed whose substitution yields theft (of the
/// in-flight swap amount), so a hijacked feed swapping them in is the one
/// feed-compromise path worth closing. They are immutable per mesh: the
/// registry build verifies the feed's source OFT against these pins and
/// refuses to start on a mismatch, while destination EIDs and contracts stay
/// dynamically discovered — a deliberate trade so a newly-supported chain needs
/// no crate release. An unpinned destination almost always at worst makes
/// `send` revert (invalid/mismatched EID), never steals. The one genuine loss
/// edge: a hijacked feed substitutes a *valid* peer EID for a known chain,
/// misrouting the OFT send to a different EVM chain. For a self-custody
/// recipient the same key controls the same address on every EVM chain, so the
/// tokens land at a recoverable address; loss only occurs when the recipient
/// `to` is not controllable on the substituted chain (e.g. a CEX deposit
/// address). Accepting this is the cost of runtime chain discovery — pinning
/// destination EIDs would defeat that goal. Verified against
/// `https://docs.usdt0.to/api/deployments`.
///
/// Native (`OFTv2`) mesh source OFT on Arbitrum One.
pub const ARBITRUM_USDT0_NATIVE_OFT: &str = "0x14E4A1B13bf7F943c8ff7C51fb60FA964A298D92";

/// Legacy mesh (`OFTv1`) source OFT on Arbitrum One.
pub const ARBITRUM_USDT0_LEGACY_OFT: &str = "0x77652D5aba086137b595875263FC200182919B92";

/// Circle CCTP v2 `TokenMessenger` — same address on every supported EVM
/// chain. The Router's `claimERC20ExecuteCctp` calls `depositForBurn` here.
pub const CCTP_TOKEN_MESSENGER_V2: &str = "0x28b5a0e9C621a5BadaA536219b3a228C8168cf5d";

/// Circle CCTP v2 `MessageTransmitter` — same address on every supported EVM
/// chain. Emits the `MessageSent` log parsed to read the burned amount, and
/// (on the destination chain) mints via `receiveMessage`.
pub const CCTP_MESSAGE_TRANSMITTER_V2: &str = "0x81D40F21F12A8F0E3252Bccb954D722d4c464B64";

/// Circle CCTP domain id for Arbitrum (the burn source). Distinct from the EVM
/// chain id.
pub const CCTP_ARBITRUM_DOMAIN: u32 = 3;

/// CCTP v2 `minFinalityThreshold` for Fast transfers (soft finality, lower
/// latency). The web app defaults to Fast for every CCTP route, so the crate
/// only ever uses Fast — the Standard-finality (2000) threshold is intentionally
/// not defined until a code path actually selects it.
pub const CCTP_FINALITY_FAST: u32 = 1000;

/// CCTP v2 "forwarding service" `hookData` for EVM destinations: the ASCII tag
/// `"cctp-forward"` (12 bytes) right-padded to 32 bytes. Circle's forwarder
/// recognizes it as version 0 with no extra payload. Hex (no `0x`); see
/// boltz-web-app `cctp/evm.ts` `cctpForwardHookData`.
pub const CCTP_FORWARD_HOOK_DATA_HEX: &str =
    "636374702d666f72776172640000000000000000000000000000000000000000";

/// SPL token mint for USDC on Solana — used to derive the recipient's USDC
/// Associated Token Account as the CCTP `mintRecipient` for Solana
/// destinations.
pub const SOLANA_USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Circle CCTP v2 `MessageTransmitter` program on Solana (the *destination*
/// transmitter, distinct from the EVM/source [`CCTP_MESSAGE_TRANSMITTER_V2`]).
/// On `receiveMessage` it creates a per-message `used_nonce` PDA as its
/// replay-protection record; that PDA's existence is a forwarder-agnostic proof
/// the mint landed, used to complete a swap whose Circle forward stalled.
pub const SOLANA_MESSAGE_TRANSMITTER_V2: &str = "CCTPV2Sm4AdWt5296sk4P66VBZ7bEhcARwFaaS9YPbeC";

/// Default Circle CCTP "Iris" API base URL (mainnet). Sandbox is
/// `https://iris-api-sandbox.circle.com`.
pub const DEFAULT_CCTP_API_URL: &str = "https://iris-api.circle.com";

/// Default `LayerZero` Scan API base URL (mainnet). Testnet is
/// `https://scan-testnet.layerzero-api.com`.
pub const DEFAULT_LZ_SCAN_API_URL: &str = "https://scan.layerzero-api.com";

/// Default background delivery-confirmation poll cadence (seconds). Gentle by
/// design: a bridged swap's funds are already committed at claim time, so a few
/// missed ticks before `Completed` are harmless, and a tight loop against
/// Circle Iris / `LayerZero` Scan would be wasteful for background callers.
pub const DEFAULT_DELIVERY_POLL_INTERVAL_SECS: u64 = 30;

/// Extra basis points added on top of Circle's quoted burn fee to absorb
/// fee movement between prepare and claim (`maxFee` cushion — NOT user
/// slippage). Matches the web app's `cctpMaxFeeBufferBps`.
pub const CCTP_MAX_FEE_BUFFER_BPS: u64 = 2;

/// Scale of Circle's `minimumFee` field (basis points scaled by 10^9).
pub const CCTP_FEE_SCALE: u128 = 1_000_000_000;

/// Denominator for applying `minimumFee`: `10_000 * CCTP_FEE_SCALE`. The
/// burn fee is `amount * bpsUnits / CCTP_FEE_BPS_DENOMINATOR + forwardFee`.
pub const CCTP_FEE_BPS_DENOMINATOR: u128 = 10_000 * CCTP_FEE_SCALE;

/// SPL token mint for USDT0 on Solana. Used to derive the recipient's
/// `Associated Token Account` when building the `LayerZero` OFT send from
/// Arbitrum. Not exposed by the USDT0 deployments API.
pub const SOLANA_USDT0_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

/// Default Solana JSON-RPC endpoint used when the destination is Solana.
/// Public mainnet endpoint, CORS-enabled so it works from the browser (the
/// official `api.mainnet-beta.solana.com` returns 403 on any browser-origin
/// request). Rate-limited, so callers with non-trivial throughput should
/// override with a dedicated provider.
pub const DEFAULT_SOLANA_RPC_URL: &str = "https://solana-rpc.publicnode.com";

/// tBTC has 18 decimals on EVM. Sats have 8 decimals. Conversion factor = 10^10.
pub const SATS_TO_TBTC_FACTOR: u64 = 10_000_000_000;

/// Zero address — used as `tokenOut` in Boltz DEX quote API to represent native ETH.
pub const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// Invoice expiry (seconds) used for probe-only reverse swap invoices.
/// Matches Boltz's documented minimum from `GET /v2/swap/reverse/expiry` so
/// the unfunded swap's server-side state self-clears as quickly as possible.
pub const PROBE_INVOICE_EXPIRY_SECS: u64 = 60;

// ─── Inbound deposits ────────────────────────────────────────────────────

/// Protocol facts for a supported deposit source chain. The
/// runtime-overridable parts (RPC URL, confirmation depth) live on
/// [`DepositChainConfig`].
#[derive(Clone, Copy, Debug)]
pub struct DepositChainSpec {
    pub chain_id: u64,
    /// Stable lowercase identifier — also the per-chain watermark key, so
    /// renaming one orphans that chain's stored scan cursor.
    pub label: &'static str,
    /// Native USDC token contract on this chain.
    pub usdc_address: &'static str,
    /// Circle CCTP domain id.
    pub cctp_domain: u32,
    /// Default confirmation depth a deposit must reach before it may drive
    /// the irreversible CCTP burn — a reorged-out transfer must never burn.
    pub default_confirmations: u64,
    pub default_rpc_url: &'static str,
}

/// Supported deposit source chains. Confirmation depths for ETH/POL/BASE
/// mirror boltz-web-app deposits. Arbitrum is the special local case: inflows
/// there (cooperative-refund returns, direct deposits) skip the bridge
/// entirely, and the guarded action is only the lock, so a shallow depth
/// suffices (sequencer reorgs are practically nonexistent).
pub const DEPOSIT_SOURCE_CHAINS: &[DepositChainSpec] = &[
    DepositChainSpec {
        chain_id: 1,
        label: "ethereum",
        usdc_address: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        cctp_domain: 0,
        default_confirmations: 12,
        default_rpc_url: "https://ethereum-rpc.publicnode.com",
    },
    DepositChainSpec {
        chain_id: 137,
        label: "polygon",
        usdc_address: "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359",
        cctp_domain: 7,
        default_confirmations: 64,
        default_rpc_url: "https://polygon-bor-rpc.publicnode.com",
    },
    DepositChainSpec {
        chain_id: 8453,
        label: "base",
        usdc_address: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
        cctp_domain: 6,
        default_confirmations: 12,
        default_rpc_url: "https://base-rpc.publicnode.com",
    },
    DepositChainSpec {
        chain_id: ARBITRUM_CHAIN_ID,
        label: "arbitrum",
        usdc_address: ARBITRUM_USDC_ADDRESS,
        cctp_domain: CCTP_ARBITRUM_DOMAIN,
        default_confirmations: 12,
        default_rpc_url: "https://arb1.arbitrum.io/rpc",
    },
];

/// Look up a deposit source chain's protocol facts by EVM chain id.
pub fn deposit_chain_spec(chain_id: u64) -> Option<&'static DepositChainSpec> {
    DEPOSIT_SOURCE_CHAINS
        .iter()
        .find(|s| s.chain_id == chain_id)
}

/// Per-chain runtime deposit configuration.
#[derive(Clone, Debug)]
pub struct DepositChainConfig {
    /// Must match a [`DEPOSIT_SOURCE_CHAINS`] entry.
    pub chain_id: u64,
    pub rpc_url: String,
    /// Confirmation depth before the deposit may be acted on.
    pub confirmations: u64,
}

impl DepositChainConfig {
    /// Chain config with the spec's defaults.
    pub fn from_spec(spec: &DepositChainSpec) -> Self {
        Self {
            chain_id: spec.chain_id,
            rpc_url: spec.default_rpc_url.to_string(),
            confirmations: spec.default_confirmations,
        }
    }
}

/// Configuration for the inbound stablecoin deposit feature. Supplied via
/// `DepositParams` at service construction; the feature is fully absent
/// without it.
#[derive(Clone, Debug)]
pub struct DepositConfig {
    /// Chains to accept deposits on. Defaults to every supported chain with
    /// public-RPC defaults; trim the set or override URLs per chain.
    pub source_chains: Vec<DepositChainConfig>,
    /// Deposit scanner cadence in seconds (per source chain).
    pub scan_interval_secs: u64,
    /// Whether this instance scans for deposits and drives them. Disable on
    /// secondary instances to save RPC — money-safety NEVER depends on
    /// there being a single watcher (see the chain-derived scheduling
    /// design), only redundant work does.
    pub watch: bool,
}

impl Default for DepositConfig {
    fn default() -> Self {
        Self {
            source_chains: DEPOSIT_SOURCE_CHAINS
                .iter()
                .map(DepositChainConfig::from_spec)
                .collect(),
            scan_interval_secs: DEFAULT_DEPOSIT_SCAN_INTERVAL_SECS,
            watch: true,
        }
    }
}

/// Default deposit scan cadence. Half the web app's 15s: the defaults point
/// at rate-limited public RPCs and detection latency is dwarfed by
/// confirmation depth anyway.
pub const DEFAULT_DEPOSIT_SCAN_INTERVAL_SECS: u64 = 30;

/// Canonical ERC-4337 `EntryPoint` v0.7 — same address on every supported EVM
/// chain. Deposit sends read `getNonce` here to anchor the send-schedule
/// derivation (see `deposit::sends`).
pub const ENTRYPOINT_V07_ADDRESS: &str = "0x0000000071727De22E5E9d8BAf0edAc6f37da032";

/// The 4337 2D-nonce key the gas sponsor's wallet stack uses (verified
/// empirically 2026-07-20: every `wallet_prepareCalls` returns key = 1 with a
/// chain-read sequence). The value itself is NOT load-bearing: the sender
/// compares the prepared `UserOp` nonce against `getNonce(sender, this key)`
/// and refuses to send on any mismatch, so a sponsor-side key change stalls
/// deposit sends loudly instead of silently voiding the collision guarantee.
pub const DEPOSIT_NONCE_KEY: u64 = 1;

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;

    /// The CCTP forwarding hookData must be exactly the ASCII tag
    /// `"cctp-forward"` (12 bytes) right-padded with 20 zero bytes to 32 bytes.
    #[macros::test_all]
    fn cctp_forward_hook_data_is_cctp_forward_padded() {
        let bytes = hex::decode(CCTP_FORWARD_HOOK_DATA_HEX).expect("valid hex");
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[..12], b"cctp-forward");
        assert_eq!(&bytes[12..], &[0u8; 20]);
    }

    #[macros::test_all]
    fn cctp_fee_denominator_is_scaled_bps() {
        assert_eq!(CCTP_FEE_BPS_DENOMINATOR, 10_000 * CCTP_FEE_SCALE);
    }

    /// The deposit source-chain table must agree with the outbound CCTP
    /// destination table wherever a chain appears in both — one source of
    /// truth would be circular (they serve different directions), so pin
    /// them against each other instead.
    #[macros::test_all]
    fn deposit_chains_consistent_with_cctp_destinations() {
        for spec in DEPOSIT_SOURCE_CHAINS {
            if let Some(dest) = crate::models::CCTP_DESTINATIONS
                .iter()
                .find(|d| d.evm_chain_id == Some(spec.chain_id))
            {
                assert_eq!(spec.cctp_domain, dest.domain, "{}", spec.label);
                assert!(
                    spec.usdc_address.eq_ignore_ascii_case(dest.token_address),
                    "{}",
                    spec.label
                );
            }
        }
        // Arbitrum is the outbound source, never in CCTP_DESTINATIONS — pin
        // it against the outbound source constants directly.
        let arb = deposit_chain_spec(ARBITRUM_CHAIN_ID).unwrap();
        assert_eq!(arb.cctp_domain, CCTP_ARBITRUM_DOMAIN);
        assert_eq!(arb.usdc_address, ARBITRUM_USDC_ADDRESS);
    }

    #[macros::test_all]
    fn deposit_config_default_covers_all_supported_chains() {
        let cfg = DepositConfig::default();
        assert_eq!(cfg.source_chains.len(), DEPOSIT_SOURCE_CHAINS.len());
        for chain in &cfg.source_chains {
            let spec = deposit_chain_spec(chain.chain_id).unwrap();
            assert_eq!(chain.confirmations, spec.default_confirmations);
            assert_eq!(chain.rpc_url, spec.default_rpc_url);
        }
        assert!(cfg.watch);
        // Ethereum, Polygon, Base + local Arbitrum.
        assert_eq!(cfg.source_chains.len(), 4);
    }

    #[macros::test_all]
    fn mainnet_enables_delivery_polling_with_lz_scan() {
        let cfg = BoltzConfig::mainnet("ref".to_string());
        // Background delivery confirmation is on by default.
        assert_eq!(
            cfg.delivery_poll_interval_secs,
            Some(DEFAULT_DELIVERY_POLL_INTERVAL_SECS)
        );
        assert_eq!(cfg.lz_scan_api_url, DEFAULT_LZ_SCAN_API_URL);
    }
}
