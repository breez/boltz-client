//! USDT0 deployments API → [`ChainRegistry`] builder.
//!
//! Destination chains are discovered at runtime from
//! `https://docs.usdt0.to/api/deployments`, so adding a new EVM destination
//! requires no client release: once USDT0 publishes it, the next service
//! init picks it up. Non-EVM destinations (Solana, Tron) still require a
//! code-level encoder in [`crate::evm::recipient`], but adding a new chain
//! on an existing transport is pure data.
//!
//! The USDT0 response exposes two meshes:
//! - `native` — the `OFTv2` mesh that carries most EVM chains.
//! - `legacyMesh` — the older `OFTv1` mesh that hosts Solana, TON, Tron,
//!   Celo, and duplicate Arbitrum/Ethereum entries with a different OFT
//!   contract used when bridging into the legacy mesh.
//!
//! Non-EVM legacy chains have `chainId: null` in the response; we infer
//! [`NetworkTransport`] from the lowercased name and drop entries for which
//! no encoder exists.

use std::collections::{HashMap, HashSet};

use platform_utils::http::HttpClient;
use serde::Deserialize;

use crate::error::BoltzError;
use crate::models::{ChainId, ChainRegistry, ChainSpec, NetworkTransport, SourceSpec, Usdt0Kind};

/// Default OFT token name to look up.
const DEFAULT_OFT_NAME: &str = "usdt0";

/// Primary OFT contract names, tried in order. `OFT Program` covers Solana's
/// legacy-mesh deployment.
const PRIMARY_OFT_CONTRACT_NAMES: &[&str] = &["OFT", "OFT Adapter", "OFT Program"];

/// Contract name under which the USDT0 token address is published in the
/// deployments registry. Not every chain publishes one: on Ethereum (and
/// similar adapter-only deployments) the OFT wraps the canonical USDT and
/// there is no separate `Token` entry.
const TOKEN_CONTRACT_NAME: &str = "Token";

/// Flat per-route fee charged by the legacy mesh USDT0 bridge, in basis
/// points. The legacy `quoteOFT` staticcall does not deduct this, so any
/// inverse-quote (destination amount → required source amount) for a legacy
/// mesh route must add it back via [`legacy_mesh_source_amount`].
pub const LEGACY_MESH_FEE_BPS: u32 = 3;

/// Denominator for basis-point math.
pub const HUNDRED_PERCENT_BPS: u32 = 10_000;

/// Closed-form inverse of the legacy mesh OFT bridge fee: given a desired
/// destination amount, return the source amount needed to deliver it after
/// the flat 3 bps fee:
///
/// ```text
///     source = ceilDiv(dest * 10_000, 10_000 - 3)
/// ```
///
/// Used to short-circuit the binary-search inverse-quote for legacy mesh
/// routes, where the on-chain `quoteOFT` does not account for the bridge
/// fee. Returns `None` on `u128` overflow.
#[must_use]
pub fn legacy_mesh_source_amount(destination_amount: u128) -> Option<u128> {
    let numerator = destination_amount.checked_mul(u128::from(HUNDRED_PERCENT_BPS))?;
    let denominator = u128::from(HUNDRED_PERCENT_BPS - LEGACY_MESH_FEE_BPS);
    ceil_div(numerator, denominator)
}

/// Integer ceiling division: `(num + den - 1) / den`. Returns `None` on
/// `u128` overflow or division by zero.
#[must_use]
#[expect(clippy::arithmetic_side_effects)]
pub fn ceil_div(numerator: u128, denominator: u128) -> Option<u128> {
    if denominator == 0 {
        return None;
    }
    // `denominator - 1` cannot underflow (checked above); the final
    // division by `denominator` cannot panic for the same reason.
    let bumped = numerator.checked_add(denominator - 1)?;
    Some(bumped / denominator)
}

/// Fetch the USDT0 deployments JSON from `url` and build a
/// [`ChainRegistry`] anchored at `source_evm_chain_id` (Arbitrum in
/// practice).
///
/// Fails if the fetch errors, the body is not parseable, the `usdt0` token
/// config is missing, or `source_evm_chain_id` is not present in USDT0's
/// `native` section.
pub async fn fetch_chain_registry(
    http_client: &dyn HttpClient,
    url: &str,
    source_evm_chain_id: u64,
) -> Result<ChainRegistry, BoltzError> {
    let response = http_client.get(url.to_string(), None).await?;

    if !response.is_success() {
        return Err(BoltzError::Api {
            reason: format!("Failed to fetch OFT deployments: HTTP {}", response.status),
            code: None,
        });
    }

    parse_chain_registry(&response.body, source_evm_chain_id)
}

/// Parse a USDT0 deployments JSON body into a [`ChainRegistry`]. Split out
/// from [`fetch_chain_registry`] for unit testing without an HTTP roundtrip.
pub fn parse_chain_registry(
    body: &str,
    source_evm_chain_id: u64,
) -> Result<ChainRegistry, BoltzError> {
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

    let source_evm_chain_id_u32 = source_evm_chain_id_as_u32(source_evm_chain_id)?;

    // Locate the source chain in the native section to derive its display
    // name. The source must be present in the native mesh — if it isn't,
    // there's no native destination to bridge to and the claim path is
    // non-functional.
    let source_native_entry = token_config
        .native
        .iter()
        .find(|c| c.chain_id == Some(source_evm_chain_id_u32))
        .ok_or_else(|| BoltzError::Api {
            reason: format!(
                "Source chain ID {source_evm_chain_id} not found in USDT0 native deployments",
            ),
            code: None,
        })?;
    let source_id = ChainId::new(&source_native_entry.name);

    let source_native_oft = resolve_chain_info(source_native_entry).map(|info| info.oft_address);
    let source_legacy_oft = token_config
        .legacy_mesh
        .iter()
        .find(|c| c.chain_id == Some(source_evm_chain_id_u32))
        .and_then(resolve_chain_info)
        .map(|info| info.oft_address);

    let source = SourceSpec {
        id: source_id.clone(),
        evm_chain_id: source_evm_chain_id,
        native_oft_address: source_native_oft,
        legacy_oft_address: source_legacy_oft,
    };

    // Build destinations. Native-mesh entries are inserted first so that a
    // chain appearing in both sections keeps the native spec. An EVM chain
    // can show up in both sections under different names ("Arbitrum One" in
    // native, "Arbitrum" in legacyMesh) — dedup by `chainId`, not by name,
    // so the legacy duplicate doesn't land as a second destination.
    let mut destinations: HashMap<ChainId, ChainSpec> = HashMap::new();
    let mut seen_evm_chain_ids: HashSet<u64> = HashSet::new();

    for entry in &token_config.native {
        if let Some(spec) = build_chain_spec(entry, Usdt0Kind::Native) {
            if let Some(cid) = spec.evm_chain_id {
                seen_evm_chain_ids.insert(cid);
            }
            destinations.insert(spec.id.clone(), spec);
        }
    }
    for entry in &token_config.legacy_mesh {
        if let Some(spec) = build_chain_spec(entry, Usdt0Kind::Legacy) {
            if let Some(cid) = spec.evm_chain_id
                && seen_evm_chain_ids.contains(&cid)
            {
                continue;
            }
            destinations.entry(spec.id.clone()).or_insert(spec);
        }
    }

    // The source chain must be reachable as a destination (same-chain
    // delivery path in the claim flow). Verify it landed in the map under
    // the same ID we derived for `SourceSpec`.
    if !destinations.contains_key(&source_id) {
        return Err(BoltzError::Api {
            reason: format!(
                "Source chain '{source_id}' missing from USDT0 destinations after registry build",
            ),
            code: None,
        });
    }

    Ok(ChainRegistry {
        source,
        destinations,
    })
}

fn source_evm_chain_id_as_u32(source: u64) -> Result<u32, BoltzError> {
    source
        .try_into()
        .map_err(|_| BoltzError::Generic(format!("Source chain ID {source} exceeds u32")))
}

/// Build a `ChainSpec` for a single USDT0 entry, or `None` if the entry is
/// unsupported (missing `lzEid`, missing primary OFT contract, or non-EVM
/// transport with no encoder).
fn build_chain_spec(entry: &OftApiChain, mesh: Usdt0Kind) -> Option<ChainSpec> {
    let (transport, evm_chain_id) = classify_transport(entry)?;
    let info = resolve_chain_info(entry)?;

    Some(ChainSpec {
        id: ChainId::new(&entry.name),
        display_name: entry.name.clone(),
        transport,
        evm_chain_id,
        lz_eid: info.lz_eid,
        oft_address: info.oft_address,
        token_address: info.token_address,
        mesh,
    })
}

/// Infer the underlying transport from a USDT0 entry:
/// - `chainId: Some(…)` → EVM.
/// - `chainId: null` + known non-EVM name → matching variant.
/// - Anything else → `None`, causing the entry to be dropped from the
///   registry (no code-level encoder).
fn classify_transport(entry: &OftApiChain) -> Option<(NetworkTransport, Option<u64>)> {
    if let Some(id) = entry.chain_id {
        return Some((NetworkTransport::Evm, Some(u64::from(id))));
    }
    match entry.name.to_lowercase().as_str() {
        "solana" => Some((NetworkTransport::Solana, None)),
        "tron" => Some((NetworkTransport::Tron, None)),
        _ => None,
    }
}

/// Flat OFT fields the registry needs from one USDT0 entry.
struct ResolvedOftInfo {
    lz_eid: u32,
    oft_address: String,
    token_address: Option<String>,
}

fn resolve_chain_info(entry: &OftApiChain) -> Option<ResolvedOftInfo> {
    let lz_eid_str = entry.lz_eid.as_ref()?;
    let lz_eid: u32 = lz_eid_str.parse().ok()?;
    let contract = find_primary_contract(&entry.contracts)?;

    Some(ResolvedOftInfo {
        lz_eid,
        oft_address: contract.address.clone(),
        token_address: find_token_contract(&entry.contracts).map(|c| c.address.clone()),
    })
}

fn find_primary_contract(contracts: &[OftApiContract]) -> Option<&OftApiContract> {
    PRIMARY_OFT_CONTRACT_NAMES
        .iter()
        .find_map(|name| contracts.iter().find(|c| c.name == *name))
}

fn find_token_contract(contracts: &[OftApiContract]) -> Option<&OftApiContract> {
    contracts.iter().find(|c| c.name == TOKEN_CONTRACT_NAME)
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

    const ARBITRUM_CHAIN_ID: u64 = 42161;

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
                },
                {
                    "name": "TON",
                    "lzEid": "30343",
                    "contracts": [
                        {"name": "OFT", "address": "EQCxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "explorer": "https://tonviewer.com/"}
                    ]
                }
            ]
        }
    }"#;

    fn sample_registry() -> ChainRegistry {
        parse_chain_registry(SAMPLE_DEPLOYMENTS, ARBITRUM_CHAIN_ID).unwrap()
    }

    #[macros::test_all]
    fn native_evm_entries_are_registered() {
        let registry = sample_registry();

        let tempo = registry.get(&ChainId::new("tempo")).expect("tempo");
        assert_eq!(tempo.transport, NetworkTransport::Evm);
        assert_eq!(tempo.evm_chain_id, Some(4217));
        assert_eq!(tempo.lz_eid, 30410);
        assert_eq!(tempo.mesh, Usdt0Kind::Native);
        assert_eq!(
            tempo.oft_address,
            "0xaf37E8B6C9ED7f6318979f56Fc287d76c30847ff"
        );
        assert_eq!(
            tempo.token_address.as_deref(),
            Some("0x20C00000000000000000000014f22CA97301EB73")
        );
        assert_eq!(tempo.display_name, "Tempo");
    }

    #[macros::test_all]
    fn native_duplicate_in_legacy_prefers_native() {
        // Arbitrum appears in both `native` (as "Arbitrum One") and
        // `legacyMesh` (as "Arbitrum"). Native precedence means the "arbitrum
        // one" key wins with the native OFT address, and the legacy entry's
        // alias "arbitrum" does not leak into destinations as a second
        // Arbitrum (dedup-by-chainId).
        let registry = sample_registry();

        let arb = registry
            .get(&ChainId::new("arbitrum one"))
            .expect("arbitrum one");
        assert_eq!(arb.mesh, Usdt0Kind::Native);
        assert_eq!(
            arb.oft_address,
            "0x14E4A1B13bf7F943c8ff7C51fb60FA964A298D92"
        );

        assert!(
            registry.get(&ChainId::new("arbitrum")).is_none(),
            "legacy-mesh alias `arbitrum` must not leak as a second destination"
        );
    }

    #[macros::test_all]
    fn legacy_only_evm_chain_falls_through_to_legacy_mesh() {
        // Celo is only in `legacyMesh`, so it lands in the destinations
        // map with `mesh == Legacy`.
        let registry = sample_registry();

        let celo = registry.get(&ChainId::new("celo")).expect("celo");
        assert_eq!(celo.mesh, Usdt0Kind::Legacy);
        assert_eq!(celo.transport, NetworkTransport::Evm);
        assert_eq!(celo.evm_chain_id, Some(42220));
        assert_eq!(celo.lz_eid, 30125);
    }

    #[macros::test_all]
    fn non_evm_legacy_chains_infer_transport_from_name() {
        let registry = sample_registry();

        let solana = registry.get(&ChainId::new("solana")).expect("solana");
        assert_eq!(solana.transport, NetworkTransport::Solana);
        assert_eq!(solana.evm_chain_id, None);
        assert_eq!(solana.lz_eid, 30168);

        let tron = registry.get(&ChainId::new("tron")).expect("tron");
        assert_eq!(tron.transport, NetworkTransport::Tron);
        assert_eq!(tron.evm_chain_id, None);
    }

    #[macros::test_all]
    fn unsupported_non_evm_family_is_dropped() {
        // TON is present in the fixture but has no NetworkTransport variant,
        // so `classify_transport` returns None and the entry is skipped.
        let registry = sample_registry();
        assert!(registry.get(&ChainId::new("ton")).is_none());
    }

    #[macros::test_all]
    fn entry_without_lz_eid_is_dropped() {
        // HyperCore in the fixture has no `lzEid` and no `chainId`. It would
        // classify as non-EVM, but since its name isn't in the encoder map,
        // it drops anyway — and even if the name were recognised, the missing
        // `lzEid` would keep it out.
        let registry = sample_registry();
        assert!(registry.get(&ChainId::new("hypercore")).is_none());
    }

    #[macros::test_all]
    fn source_spec_aggregates_native_and_legacy_oft_addresses() {
        let registry = sample_registry();

        assert_eq!(registry.source.id, ChainId::new("arbitrum one"));
        assert_eq!(registry.source.evm_chain_id, ARBITRUM_CHAIN_ID);
        assert_eq!(
            registry.source.native_oft_address.as_deref(),
            Some("0x14E4A1B13bf7F943c8ff7C51fb60FA964A298D92")
        );
        assert_eq!(
            registry.source.legacy_oft_address.as_deref(),
            Some("0x77652D5aba086137b595875263FC200182919B92")
        );
    }

    #[macros::test_all]
    fn source_spec_oft_for_picks_by_mesh() {
        let registry = sample_registry();

        assert_eq!(
            registry.source.oft_for(Usdt0Kind::Native),
            Some("0x14E4A1B13bf7F943c8ff7C51fb60FA964A298D92")
        );
        assert_eq!(
            registry.source.oft_for(Usdt0Kind::Legacy),
            Some("0x77652D5aba086137b595875263FC200182919B92")
        );
    }

    #[macros::test_all]
    fn is_source_true_for_registry_source_id() {
        let registry = sample_registry();
        assert!(registry.is_source(&ChainId::new("arbitrum one")));
        assert!(!registry.is_source(&ChainId::new("tempo")));
        assert!(!registry.is_source(&ChainId::new("solana")));
    }

    #[macros::test_all]
    fn supported_chains_lists_registered_destinations_only() {
        let registry = sample_registry();
        let chains = registry.supported_chains();

        assert!(chains.contains(&ChainId::new("arbitrum one")));
        assert!(chains.contains(&ChainId::new("ethereum")));
        assert!(chains.contains(&ChainId::new("tempo")));
        assert!(chains.contains(&ChainId::new("celo")));
        assert!(chains.contains(&ChainId::new("solana")));
        assert!(chains.contains(&ChainId::new("tron")));
        // TON and HyperCore are unsupported and must be absent.
        assert!(!chains.contains(&ChainId::new("ton")));
        assert!(!chains.contains(&ChainId::new("hypercore")));
    }

    #[macros::test_all]
    fn missing_source_chain_errors() {
        // Source chain ID 99999 is not in the fixture → init must fail hard.
        let err = parse_chain_registry(SAMPLE_DEPLOYMENTS, 99999).unwrap_err();
        match err {
            BoltzError::Api { reason, .. } => {
                assert!(reason.contains("Source chain ID 99999"), "reason: {reason}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[macros::test_all]
    fn unknown_evm_chain_requires_zero_code_changes() {
        // Smoke test: an EVM entry the client has never seen before still
        // lands in the registry keyed by its lowercased name, fully wired up.
        let body = r#"{
            "usdt0": {
                "native": [
                    {
                        "name": "Arbitrum One",
                        "chainId": 42161,
                        "lzEid": "30110",
                        "contracts": [
                            {"name": "OFT", "address": "0x14E4A1B13bf7F943c8ff7C51fb60FA964A298D92", "explorer": ""}
                        ]
                    },
                    {
                        "name": "FutureChain",
                        "chainId": 9999,
                        "lzEid": "30999",
                        "contracts": [
                            {"name": "OFT", "address": "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "explorer": ""}
                        ]
                    }
                ]
            }
        }"#;
        let registry = parse_chain_registry(body, ARBITRUM_CHAIN_ID).unwrap();
        let future = registry
            .get(&ChainId::new("futurechain"))
            .expect("futurechain");
        assert_eq!(future.transport, NetworkTransport::Evm);
        assert_eq!(future.evm_chain_id, Some(9999));
        assert_eq!(future.lz_eid, 30999);
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
        let registry = parse_chain_registry(body, ARBITRUM_CHAIN_ID).unwrap();
        assert!(
            registry
                .get(&ChainId::new("arbitrum one"))
                .is_some_and(|s| s.mesh == Usdt0Kind::Native)
        );
        assert!(registry.source.legacy_oft_address.is_none());
    }

    #[macros::test_all]
    fn primary_contract_name_precedence_prefers_oft_over_adapter() {
        // When a chain advertises both `OFT` and `OFT Adapter`, the resolver
        // must pick `OFT` (first entry in `PRIMARY_OFT_CONTRACT_NAMES`),
        // regardless of array order. Guards against a future reorder of the
        // precedence list or a `.find()` → `.rfind()` slip.
        let body = r#"{
            "usdt0": {
                "native": [
                    {
                        "name": "Arbitrum One",
                        "chainId": 42161,
                        "lzEid": "30110",
                        "contracts": [
                            {"name": "OFT", "address": "0x14E4A1B13bf7F943c8ff7C51fb60FA964A298D92", "explorer": ""}
                        ]
                    },
                    {
                        "name": "Synthetic",
                        "chainId": 88888,
                        "lzEid": "30888",
                        "contracts": [
                            {"name": "OFT Adapter", "address": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "explorer": ""},
                            {"name": "OFT", "address": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "explorer": ""}
                        ]
                    }
                ]
            }
        }"#;
        let registry = parse_chain_registry(body, ARBITRUM_CHAIN_ID).unwrap();
        let spec = registry.get(&ChainId::new("synthetic")).expect("synthetic");
        assert_eq!(
            spec.oft_address, "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "must prefer `OFT` over `OFT Adapter` regardless of array order"
        );
    }

    #[macros::test_all]
    fn primary_contract_falls_back_to_adapter_when_oft_absent() {
        let registry = sample_registry();
        let eth = registry.get(&ChainId::new("ethereum")).expect("ethereum");
        assert_eq!(
            eth.oft_address,
            "0x6C96dE32CEa08842dcc4058c14d3aaAD7Fa41dee"
        );
        // Ethereum only publishes an adapter — no separate Token entry.
        assert!(eth.token_address.is_none());
    }

    #[macros::test_all]
    fn missing_token_config_fails() {
        let body = r#"{"other": {"native": [], "legacyMesh": []}}"#;
        let err = parse_chain_registry(body, ARBITRUM_CHAIN_ID).unwrap_err();
        match err {
            BoltzError::Api { reason, .. } => assert!(reason.contains("usdt0")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[macros::test_all]
    fn legacy_mesh_source_amount_matches_known_vector() {
        assert_eq!(
            legacy_mesh_source_amount(1_000_000_000),
            Some(1_000_300_091)
        );
    }

    #[macros::test_all]
    fn legacy_mesh_source_amount_zero() {
        assert_eq!(legacy_mesh_source_amount(0), Some(0));
    }

    #[macros::test_all]
    fn legacy_mesh_source_amount_rounds_up() {
        assert_eq!(legacy_mesh_source_amount(1), Some(2));
    }

    #[macros::test_all]
    fn legacy_mesh_source_amount_returns_none_on_overflow() {
        assert_eq!(legacy_mesh_source_amount(u128::MAX), None);
    }

    #[macros::test_all]
    fn ceil_div_basic_cases() {
        assert_eq!(ceil_div(0, 5), Some(0));
        assert_eq!(ceil_div(6, 3), Some(2));
        assert_eq!(ceil_div(7, 3), Some(3));
        assert_eq!(ceil_div(1, 1), Some(1));
    }

    #[macros::test_all]
    fn ceil_div_division_by_zero_is_none() {
        assert_eq!(ceil_div(42, 0), None);
    }

    #[macros::test_all]
    fn ceil_div_overflow_is_none() {
        assert_eq!(ceil_div(u128::MAX, 3), None);
    }
}
