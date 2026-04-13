//! OFT deployment registry — fetches `LayerZero` endpoint IDs and OFT contract
//! addresses from the USDT0 API at runtime, matching the Boltz web app.
//!
//! The USDT0 API exposes two meshes:
//! - `native` — the `OFTv2` mesh that carries most EVM chains (Arbitrum,
//!   Ethereum, Polygon, …).
//! - `legacyMesh` — the older `OFTv1` mesh that still hosts Solana, TON, Tron,
//!   Celo, plus duplicate entries for Arbitrum/Ethereum with a different OFT
//!   contract used when bridging into the legacy mesh.
//!
//! Non-EVM legacy chains (Solana, TON, Tron) have `chainId: null` in the API
//! response, so they are kept in a separate name-keyed map.

use std::collections::HashMap;

use platform_utils::http::HttpClient;
use serde::Deserialize;

use crate::error::BoltzError;
use crate::models::{Chain, NetworkTransport};

/// Default OFT token name to look up (matches web app's `defaultOftName`).
const DEFAULT_OFT_NAME: &str = "usdt0";

/// Primary OFT contract names, tried in order — mirrors the web app's
/// `primaryOftContractNames` in `src/utils/oft/registry.ts`.
/// `OFT Program` covers Solana's legacy-mesh deployment.
const PRIMARY_OFT_CONTRACT_NAMES: &[&str] = &["OFT", "OFT Adapter", "OFT Program"];

/// Which USDT0 mesh a chain belongs to. Mirrors the web app's `Usdt0Kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Usdt0Kind {
    Native,
    Legacy,
}

/// Resolved OFT info for a single chain entry.
#[derive(Clone, Debug)]
pub struct OftChainInfo {
    /// `LayerZero` endpoint ID for this chain.
    pub lz_eid: u32,
    /// OFT contract address (hex with 0x prefix for EVM; native encoding
    /// for Solana/TON/Tron).
    pub oft_address: String,
    /// Which mesh this entry came from.
    pub mesh: Usdt0Kind,
}

/// Cached OFT deployment data.
///
/// Entries from the `native` array are stored by EVM chain ID. Entries from
/// `legacyMesh` are split: EVM chains go in `legacy_evm_chains` (also keyed
/// by chain ID), and chains without a numeric chain ID (Solana, TON, Tron)
/// go in `legacy_named_chains` keyed by the lowercased chain name.
#[derive(Clone, Debug)]
pub struct OftDeployments {
    native_chains: HashMap<u64, OftChainInfo>,
    legacy_evm_chains: HashMap<u64, OftChainInfo>,
    legacy_named_chains: HashMap<String, OftChainInfo>,
}

impl OftDeployments {
    /// Fetch OFT deployments from the given URL.
    pub async fn fetch(http_client: &dyn HttpClient, url: &str) -> Result<Self, BoltzError> {
        let response = http_client.get(url.to_string(), None).await?;

        if !response.is_success() {
            return Err(BoltzError::Api {
                reason: format!("Failed to fetch OFT deployments: HTTP {}", response.status),
                code: None,
            });
        }

        Self::parse(&response.body)
    }

    fn parse(body: &str) -> Result<Self, BoltzError> {
        let registry: OftRegistry = serde_json::from_str(body).map_err(|e| BoltzError::Api {
            reason: format!("Failed to parse OFT deployments: {e}"),
            code: None,
        })?;

        let token_config = registry
            .0
            .get(DEFAULT_OFT_NAME)
            .ok_or_else(|| BoltzError::Api {
                reason: format!("OFT token '{DEFAULT_OFT_NAME}' not found in deployments"),
                code: None,
            })?;

        let mut native_chains = HashMap::new();
        for chain in &token_config.native {
            if let Some((chain_id, info)) = resolve_evm_chain(chain, Usdt0Kind::Native) {
                native_chains.insert(chain_id, info);
            }
        }

        let mut legacy_evm_chains = HashMap::new();
        let mut legacy_named_chains = HashMap::new();
        for chain in &token_config.legacy_mesh {
            match resolve_chain(chain, Usdt0Kind::Legacy) {
                Some(ResolvedChain::Evm { chain_id, info }) => {
                    legacy_evm_chains.insert(chain_id, info);
                }
                Some(ResolvedChain::Named { name, info }) => {
                    legacy_named_chains.insert(name, info);
                }
                None => {}
            }
        }

        Ok(Self {
            native_chains,
            legacy_evm_chains,
            legacy_named_chains,
        })
    }

    /// Look up a native-mesh chain by EVM chain ID.
    ///
    /// This is the default lookup for the current swap flow (all supported
    /// destination chains are EVM on the native mesh).
    pub fn get(&self, evm_chain_id: u64) -> Option<&OftChainInfo> {
        self.native_chains.get(&evm_chain_id)
    }

    /// Look up a legacy-mesh chain by EVM chain ID (e.g. Arbitrum/Ethereum/Celo
    /// legacy entries).
    pub fn get_legacy(&self, evm_chain_id: u64) -> Option<&OftChainInfo> {
        self.legacy_evm_chains.get(&evm_chain_id)
    }

    /// Look up a legacy-mesh chain by its registry name (case-insensitive).
    ///
    /// Needed for Solana, TON, and Tron, which the USDT0 API returns with
    /// `chainId: null`.
    pub fn get_by_name(&self, name: &str) -> Option<&OftChainInfo> {
        self.legacy_named_chains.get(&name.to_lowercase())
    }

    /// Resolve the OFT entry for a `Chain`, dispatching on its transport:
    /// EVM chains use the native-mesh `chainId` lookup; non-EVM chains
    /// (Solana, Tron) use the legacy-mesh name lookup.
    pub fn get_for(&self, chain: &Chain) -> Option<&OftChainInfo> {
        match chain.transport() {
            NetworkTransport::Evm => self.get(chain.evm_chain_id()?),
            NetworkTransport::Solana | NetworkTransport::Tron => {
                self.get_by_name(chain.registry_name())
            }
        }
    }

    /// Get the native-mesh OFT contract address for a source chain (Arbitrum
    /// in the normal flow).
    pub fn source_oft_address(&self, source_chain_id: u64) -> Option<&str> {
        self.native_chains
            .get(&source_chain_id)
            .map(|info| info.oft_address.as_str())
    }
}

// ─── Internal helpers ───────────────────────────────────────────────────

enum ResolvedChain {
    Evm { chain_id: u64, info: OftChainInfo },
    Named { name: String, info: OftChainInfo },
}

fn resolve_evm_chain(chain: &OftApiChain, mesh: Usdt0Kind) -> Option<(u64, OftChainInfo)> {
    match resolve_chain(chain, mesh)? {
        ResolvedChain::Evm { chain_id, info } => Some((chain_id, info)),
        ResolvedChain::Named { .. } => None,
    }
}

fn resolve_chain(chain: &OftApiChain, mesh: Usdt0Kind) -> Option<ResolvedChain> {
    let lz_eid_str = chain.lz_eid.as_ref()?;
    let lz_eid: u32 = lz_eid_str.parse().ok()?;
    let contract = find_primary_contract(&chain.contracts)?;

    let info = OftChainInfo {
        lz_eid,
        oft_address: contract.address.clone(),
        mesh,
    };

    if let Some(chain_id) = chain.chain_id {
        Some(ResolvedChain::Evm {
            chain_id: u64::from(chain_id),
            info,
        })
    } else {
        Some(ResolvedChain::Named {
            name: chain.name.to_lowercase(),
            info,
        })
    }
}

fn find_primary_contract(contracts: &[OftApiContract]) -> Option<&OftApiContract> {
    PRIMARY_OFT_CONTRACT_NAMES
        .iter()
        .find_map(|name| contracts.iter().find(|c| c.name == *name))
}

// ─── API response types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct OftRegistry(HashMap<String, OftTokenConfig>);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OftTokenConfig {
    native: Vec<OftApiChain>,
    #[serde(default)]
    legacy_mesh: Vec<OftApiChain>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OftApiChain {
    name: String,
    chain_id: Option<u32>,
    lz_eid: Option<String>,
    contracts: Vec<OftApiContract>,
}

#[derive(Deserialize)]
struct OftApiContract {
    name: String,
    address: String,
    #[allow(dead_code)]
    explorer: String,
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "browser-tests")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use super::*;

    const SAMPLE_DEPLOYMENTS: &str = r#"{
        "usdt0": {
            "native": [
                {
                    "name": "Arbitrum One",
                    "chainId": 42161,
                    "lzEid": "30110",
                    "contracts": [
                        {"name": "Token", "address": "0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9", "explorer": "https://arbiscan.io/"},
                        {"name": "OFT", "address": "0x14E4A1B13bf7F943c8ff7C51fb60FA964A298D92", "explorer": "https://arbiscan.io/"}
                    ]
                },
                {
                    "name": "Ethereum",
                    "chainId": 1,
                    "lzEid": "30101",
                    "contracts": [
                        {"name": "OFT Adapter", "address": "0x6C96dE32CEa08842dcc4058c14d3aaAD7Fa41dee", "explorer": "https://etherscan.io/"}
                    ]
                },
                {
                    "name": "Tempo",
                    "chainId": 4217,
                    "lzEid": "30410",
                    "contracts": [
                        {"name": "Token", "address": "0x20C00000000000000000000014f22CA97301EB73", "explorer": "https://explore.mainnet.tempo.xyz/"},
                        {"name": "OFT", "address": "0xaf37E8B6C9ED7f6318979f56Fc287d76c30847ff", "explorer": "https://explore.mainnet.tempo.xyz/"}
                    ]
                },
                {
                    "name": "HyperCore",
                    "contracts": [
                        {"name": "Token", "address": "0x25faedc3f054130dbb4e4203aca63567", "explorer": "https://app.hyperliquid.xyz/"}
                    ]
                }
            ],
            "legacyMesh": [
                {
                    "name": "Arbitrum",
                    "chainId": 42161,
                    "lzEid": "30110",
                    "contracts": [
                        {"name": "OFT", "address": "0x77652D5aba086137b595875263FC200182919B92", "explorer": "https://arbiscan.io/"},
                        {"name": "Composer", "address": "0x759BA420bF1ded1765F18C2DC3Fc57A1964A2Ad1", "explorer": "https://arbiscan.io/"}
                    ]
                },
                {
                    "name": "Celo",
                    "chainId": 42220,
                    "lzEid": "30125",
                    "contracts": [
                        {"name": "OFT", "address": "0xf10E161027410128E63E75D0200Fb6d34b2db243", "explorer": "https://celoscan.io/"}
                    ]
                },
                {
                    "name": "Solana",
                    "lzEid": "30168",
                    "contracts": [
                        {"name": "OFT Store", "address": "HyXJcgYpURfDhgzuyRL7zxP4FhLg7LZQMeDrR4MXZcMN", "explorer": "https://solscan.io/"},
                        {"name": "OFT Program", "address": "Fuww9mfc8ntAwxPUzFia7VJFAdvLppyZwhPJoXySZXf7", "explorer": "https://solscan.io/"}
                    ]
                },
                {
                    "name": "Tron",
                    "lzEid": "30420",
                    "contracts": [
                        {"name": "OFT", "address": "TFG4wBaDQ8sHWWP1ACeSGnoNR6RRzevLPt", "explorer": "https://tronscan.org/"}
                    ]
                }
            ]
        }
    }"#;

    #[macros::test_all]
    fn parses_native_evm_entries() {
        let deployments = OftDeployments::parse(SAMPLE_DEPLOYMENTS).unwrap();

        let arb = deployments.get(42161).expect("arbitrum native");
        assert_eq!(arb.lz_eid, 30110);
        assert_eq!(
            arb.oft_address,
            "0x14E4A1B13bf7F943c8ff7C51fb60FA964A298D92"
        );
        assert_eq!(arb.mesh, Usdt0Kind::Native);

        let eth = deployments.get(1).expect("ethereum native");
        assert_eq!(
            eth.oft_address,
            "0x6C96dE32CEa08842dcc4058c14d3aaAD7Fa41dee"
        );
        assert_eq!(eth.mesh, Usdt0Kind::Native);
    }

    #[macros::test_all]
    fn skips_native_entries_without_chain_id() {
        let deployments = OftDeployments::parse(SAMPLE_DEPLOYMENTS).unwrap();
        // HyperCore has no chainId and no lzEid — must not appear anywhere.
        assert!(deployments.get_by_name("hypercore").is_none());
    }

    #[macros::test_all]
    fn parses_legacy_evm_entries_with_separate_oft_address() {
        let deployments = OftDeployments::parse(SAMPLE_DEPLOYMENTS).unwrap();

        let arb_legacy = deployments.get_legacy(42161).expect("arbitrum legacy");
        assert_eq!(
            arb_legacy.oft_address,
            "0x77652D5aba086137b595875263FC200182919B92"
        );
        assert_eq!(arb_legacy.mesh, Usdt0Kind::Legacy);

        // Native Arbitrum must be a different address.
        let arb_native = deployments.get(42161).unwrap();
        assert_ne!(arb_native.oft_address, arb_legacy.oft_address);

        let celo = deployments.get_legacy(42220).expect("celo legacy");
        assert_eq!(celo.lz_eid, 30125);
        assert_eq!(celo.mesh, Usdt0Kind::Legacy);
    }

    #[macros::test_all]
    fn parses_legacy_named_entries_for_non_evm_chains() {
        let deployments = OftDeployments::parse(SAMPLE_DEPLOYMENTS).unwrap();

        let solana = deployments.get_by_name("Solana").expect("solana by name");
        assert_eq!(solana.lz_eid, 30168);
        assert_eq!(
            solana.oft_address,
            "Fuww9mfc8ntAwxPUzFia7VJFAdvLppyZwhPJoXySZXf7"
        );
        assert_eq!(solana.mesh, Usdt0Kind::Legacy);

        // Lookup is case-insensitive.
        assert!(deployments.get_by_name("SOLANA").is_some());
        assert!(deployments.get_by_name("solana").is_some());

        let tron = deployments.get_by_name("tron").expect("tron by name");
        assert_eq!(tron.lz_eid, 30420);
        assert_eq!(tron.oft_address, "TFG4wBaDQ8sHWWP1ACeSGnoNR6RRzevLPt");
    }

    #[macros::test_all]
    fn legacy_entries_do_not_leak_into_native_lookup() {
        let deployments = OftDeployments::parse(SAMPLE_DEPLOYMENTS).unwrap();

        assert!(deployments.get(42220).is_none()); // Celo is legacy-only
        assert!(deployments.get_by_name("arbitrum one").is_none()); // native entries aren't named
    }

    #[macros::test_all]
    fn get_for_resolves_tempo_via_registry_chain_id() {
        let deployments = OftDeployments::parse(SAMPLE_DEPLOYMENTS).unwrap();
        let tempo = deployments.get_for(&Chain::Tempo).expect("tempo");
        assert_eq!(tempo.lz_eid, 30410);
        assert_eq!(
            tempo.oft_address,
            "0xaf37E8B6C9ED7f6318979f56Fc287d76c30847ff"
        );
        assert_eq!(tempo.mesh, Usdt0Kind::Native);
    }

    #[macros::test_all]
    fn get_for_dispatches_evm_to_native_mesh() {
        let deployments = OftDeployments::parse(SAMPLE_DEPLOYMENTS).unwrap();

        let arb = deployments.get_for(&Chain::Arbitrum).expect("arbitrum");
        assert_eq!(arb.lz_eid, 30110);
        assert_eq!(arb.mesh, Usdt0Kind::Native);
        // Must be the native-mesh entry, not the legacy-mesh duplicate.
        assert_eq!(
            arb.oft_address,
            "0x14E4A1B13bf7F943c8ff7C51fb60FA964A298D92"
        );

        let eth = deployments.get_for(&Chain::Ethereum).expect("ethereum");
        assert_eq!(
            eth.oft_address,
            "0x6C96dE32CEa08842dcc4058c14d3aaAD7Fa41dee"
        );
    }

    #[macros::test_all]
    fn get_for_dispatches_non_evm_to_named_legacy() {
        let deployments = OftDeployments::parse(SAMPLE_DEPLOYMENTS).unwrap();

        let solana = deployments.get_for(&Chain::Solana).expect("solana");
        assert_eq!(solana.lz_eid, 30168);
        assert_eq!(solana.mesh, Usdt0Kind::Legacy);

        let tron = deployments.get_for(&Chain::Tron).expect("tron");
        assert_eq!(tron.lz_eid, 30420);
        assert_eq!(tron.mesh, Usdt0Kind::Legacy);
    }

    #[macros::test_all]
    fn get_for_returns_none_when_evm_chain_id_missing_from_native_mesh() {
        let deployments = OftDeployments::parse(SAMPLE_DEPLOYMENTS).unwrap();
        // Polygon isn't in the sample fixture — must miss cleanly, not panic.
        assert!(deployments.get_for(&Chain::Polygon).is_none());
    }

    #[macros::test_all]
    fn source_oft_address_returns_native() {
        let deployments = OftDeployments::parse(SAMPLE_DEPLOYMENTS).unwrap();
        assert_eq!(
            deployments.source_oft_address(42161),
            Some("0x14E4A1B13bf7F943c8ff7C51fb60FA964A298D92")
        );
    }

    #[macros::test_all]
    fn missing_legacy_mesh_array_parses_successfully() {
        let body = r#"{
            "usdt0": {
                "native": [
                    {
                        "name": "Arbitrum One",
                        "chainId": 42161,
                        "lzEid": "30110",
                        "contracts": [
                            {"name": "OFT", "address": "0xaa", "explorer": ""}
                        ]
                    }
                ]
            }
        }"#;
        let deployments = OftDeployments::parse(body).unwrap();
        assert!(deployments.get(42161).is_some());
        assert!(deployments.get_legacy(42161).is_none());
    }

    #[macros::test_all]
    fn primary_contract_name_precedence_prefers_oft_over_adapter() {
        // When a chain advertises both `OFT` and `OFT Adapter`, the resolver
        // must pick `OFT` (first entry in `PRIMARY_OFT_CONTRACT_NAMES`),
        // regardless of the order they appear in the API's `contracts` array.
        // Guards against a future reorder of the precedence list or a
        // `.find()` → `.rfind()` slip.
        let body = r#"{
            "usdt0": {
                "native": [
                    {
                        "name": "Synthetic",
                        "chainId": 99999,
                        "lzEid": "30999",
                        "contracts": [
                            {"name": "OFT Adapter", "address": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "explorer": ""},
                            {"name": "OFT", "address": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "explorer": ""}
                        ]
                    }
                ]
            }
        }"#;
        let deployments = OftDeployments::parse(body).unwrap();
        let chain = deployments.get(99999).expect("synthetic chain");
        assert_eq!(
            chain.oft_address, "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "must prefer `OFT` over `OFT Adapter` regardless of array order"
        );
    }

    #[macros::test_all]
    fn primary_contract_falls_back_to_adapter_when_oft_absent() {
        // Ethereum-style: no `OFT` entry, only `OFT Adapter`. The resolver
        // must fall through the precedence list and pick the Adapter.
        let body = r#"{
            "usdt0": {
                "native": [
                    {
                        "name": "EthLike",
                        "chainId": 1,
                        "lzEid": "30101",
                        "contracts": [
                            {"name": "OFT Adapter", "address": "0xadadadadadadadadadadadadadadadadadadadad", "explorer": ""}
                        ]
                    }
                ]
            }
        }"#;
        let deployments = OftDeployments::parse(body).unwrap();
        let chain = deployments.get(1).expect("eth-like chain");
        assert_eq!(
            chain.oft_address,
            "0xadadadadadadadadadadadadadadadadadadadad"
        );
    }

    #[macros::test_all]
    fn missing_token_config_fails() {
        let body = r#"{"other": {"native": [], "legacyMesh": []}}"#;
        let err = OftDeployments::parse(body).unwrap_err();
        match err {
            BoltzError::Api { reason, .. } => assert!(reason.contains("usdt0")),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
