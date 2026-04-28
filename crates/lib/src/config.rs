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
    /// Referral ID — sent as HTTP header on pairs endpoint (required to unlock TBTC pair)
    /// and as `referralId` field in swap creation requests (attribution tracking).
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
    /// Solana JSON-RPC endpoint used when the destination chain is Solana.
    /// Queried to check whether the recipient's `Associated Token Account`
    /// already exists so the cross-chain message can pre-fund its creation
    /// when it doesn't. Unused for EVM and Tron destinations.
    pub solana_rpc_url: String,
}

/// Alchemy configuration for EIP-7702 gas abstraction.
#[derive(Clone, Debug)]
pub struct AlchemyConfig {
    /// Alchemy API key. RPC URL is derived as: `https://api.g.alchemy.com/v2/{api_key}`.
    pub api_key: String,
    /// Gas sponsorship policy ID.
    pub gas_policy_id: String,
}

impl AlchemyConfig {
    /// Returns the Alchemy JSON-RPC URL derived from the API key.
    pub fn rpc_url(&self) -> String {
        format!("https://api.g.alchemy.com/v2/{}", self.api_key)
    }
}

impl BoltzConfig {
    /// Returns a default configuration for Arbitrum mainnet.
    ///
    /// `alchemy_config` is populated with the Boltz-operated defaults
    /// ([`DEFAULT_ALCHEMY_API_KEY`] / [`DEFAULT_ALCHEMY_GAS_POLICY_ID`]). These
    /// are hardcoded for v1; long-term they will be fetched from a Boltz
    /// endpoint at startup. Callers that need custom Alchemy credentials can
    /// override `alchemy_config` on the returned struct.
    pub fn mainnet(referral_id: String) -> Self {
        Self {
            api_url: "https://api.boltz.exchange".to_string(),
            alchemy_config: AlchemyConfig {
                api_key: DEFAULT_ALCHEMY_API_KEY.to_string(),
                gas_policy_id: DEFAULT_ALCHEMY_GAS_POLICY_ID.to_string(),
            },
            arbitrum_rpc_url: "https://arb1.arbitrum.io/rpc".to_string(),
            chain_id: ARBITRUM_CHAIN_ID,
            referral_id,
            slippage_bps: DEFAULT_SLIPPAGE_BPS,
            oft_deployments_url: DEFAULT_OFT_DEPLOYMENTS_URL.to_string(),
            solana_rpc_url: DEFAULT_SOLANA_RPC_URL.to_string(),
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

/// Default slippage tolerance: 100 basis points = 1%.
pub const DEFAULT_SLIPPAGE_BPS: u32 = 100;

/// Maximum slippage tolerance: 500 basis points = 5%.
/// Matches the Boltz web app's upper bound.
pub const MAX_SLIPPAGE_BPS: u32 = 500;

/// Default URL for fetching OFT (USDT0) deployment data.
pub const DEFAULT_OFT_DEPLOYMENTS_URL: &str = "https://docs.usdt0.to/api/deployments";

/// Default Alchemy API key used for gas abstraction. Hardcoded as a
/// Boltz-operated default so the SDK layer is oblivious to credentials;
/// long-term this is expected to be fetched from a Boltz endpoint at startup.
pub const DEFAULT_ALCHEMY_API_KEY: &str = "R-iU8US4vKEe2GH6VlCTg";

/// Default Alchemy gas sponsorship policy ID paired with
/// [`DEFAULT_ALCHEMY_API_KEY`].
pub const DEFAULT_ALCHEMY_GAS_POLICY_ID: &str = "dcf46730-a11c-4869-a38b-35bcd73fe73f";

/// Router contract address on Arbitrum — not available via the Boltz API.
/// If upgraded, the old contract address remains valid.
pub const ARBITRUM_ROUTER_ADDRESS: &str = "0x6EA68e965fcd19b6fbC6553BABbF87a5018F9B28";

/// tBTC token address on Arbitrum.
pub const ARBITRUM_TBTC_ADDRESS: &str = "0x6c84a8f1c29108F47a79964b5Fe888D4f4D0dE40";

/// USDT token address on Arbitrum.
pub const ARBITRUM_USDT_ADDRESS: &str = "0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9";

/// SPL token mint for USDT0 on Solana. Used to derive the recipient's
/// `Associated Token Account` when building the `LayerZero` OFT send from
/// Arbitrum. Not exposed by the USDT0 deployments API.
pub const SOLANA_USDT0_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

/// Default Solana JSON-RPC endpoint used when the destination is Solana.
/// Public mainnet endpoint — fine for casual use but rate-limited, so
/// callers with non-trivial throughput should override with a dedicated
/// provider.
pub const DEFAULT_SOLANA_RPC_URL: &str = "https://api.mainnet.solana.com";

/// tBTC has 18 decimals on EVM. Sats have 8 decimals. Conversion factor = 10^10.
pub const SATS_TO_TBTC_FACTOR: u64 = 10_000_000_000;

/// Zero address — used as `tokenOut` in Boltz DEX quote API to represent native ETH.
pub const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// Block height at which the `ERC20Swap` contract was deployed on Arbitrum.
/// Used as the lower bound when scanning for Lockup events during recovery.
/// Matches the web app's `config.assets.TBTC.contracts.deployHeight` for mainnet.
pub const ARBITRUM_ERC20SWAP_DEPLOY_BLOCK: u64 = 435_848_678;

/// Number of blocks per scanning batch for log recovery on Arbitrum.
/// Arbitrum uses large intervals due to fast block times (~0.25s).
/// Matches the web app's `scanInterval` for Arbitrum.
pub const RECOVERY_SCAN_BATCH_SIZE: u64 = 100_000;

/// Maximum number of preimage key indices to derive during recovery.
/// Matches the web app's `maxIterations` constant.
pub const RECOVERY_MAX_KEY_INDEX: u32 = 100_000;

/// Invoice expiry (seconds) used for probe-only reverse swap invoices.
/// Matches Boltz's documented minimum from `GET /v2/swap/reverse/expiry` so
/// the unfunded swap's server-side state self-clears as quickly as possible.
pub const PROBE_INVOICE_EXPIRY_SECS: u64 = 60;
