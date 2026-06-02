# CLAUDE.md

## Build Commands

```bash
make build              # Build workspace
make build-release      # Release build with LTO
make build-wasm         # Build lib for WASM target
make check              # Run all checks (fmt, clippy, wasm-clippy, test, wasm-test)
```

## Testing

```bash
make test               # Rust unit tests (native)
make wasm-test          # WASM tests (browser + Node.js via wasm-pack)
make wasm-test-node     # WASM tests (Node.js only)
make wasm-test-browser  # WASM tests (headless Firefox)
make itest              # Integration tests (requires Docker, starts regtest stack)
```

Run a single test:
```bash
cargo test <test_name> -p boltz-client
```

## Code Quality

```bash
make fmt-check          # Check formatting
make fmt-fix            # Fix formatting
make clippy-check       # Run clippy
make clippy-fix         # Fix clippy issues
make wasm-clippy-check  # Run clippy for WASM target
```

## Architecture

### Crate Structure

- **crates/lib** (`boltz-client`) - Core library: Boltz API client, EVM contracts, swap logic, key management
- **crates/cli** (`boltz-cli`) - Interactive REPL for testing swaps
- **crates/macros** - Proc macros (`#[async_trait]`, test macros for native+WASM)
- **crates/platform-utils** - Cross-platform HTTP client and time/tokio abstractions (native + WASM)

### Swap Flow

Reverse-only (LN -> stablecoin). The DEX always produces a stablecoin on
Arbitrum; how it reaches the user depends on the destination's bridge:

```
Lightning (sats) -> tBTC (Boltz reverse swap) -> stablecoin (DEX on Arbitrum) -> destination
                                                                                   |
  Direct : delivered on Arbitrum (USDT, or USDC-on-Arbitrum) - no cross-chain hop  |
  Oft    : LayerZero USDT0 OFT bridge (native or legacy mesh) ---------------------+
  Cctp   : Circle CCTP v2 burn + mint (USDC; EVM chains + Solana) ------------------+
```

A Router contract makes claim + DEX atomic: one Alchemy-sponsored EVM tx claims
tBTC from ERC20Swap and executes the DEX swap, bundling the OFT/CCTP send (or
direct sweep) in the same call. `Oft`/`Cctp` swaps then pass through a `Settling`
state until the background manager confirms cross-chain delivery; `Direct`
completes immediately. Bridge routing is fully client-derived — the Boltz API
is bridge-agnostic (the LN->Arbitrum leg is always `BTC`->`TBTC`).

Key modules in `crates/lib/src/`:
- `api/` - Boltz REST API client + WebSocket status subscriber
- `config.rs` - `BoltzConfig` (RPC/API URLs, slippage, poll cadence) + `AlchemyConfig`
- `models.rs` - Swap/quote types, `BoltzSwapStatus` lifecycle, and the unified
  `DestinationRegistry` (`Destination` = asset-on-chain + `Bridge`: `Direct`/`Oft`/`Cctp`)
- `swap/reverse.rs` - Core swap executor (prepare quote, create swap, claim + DEX); branches on bridge kind
- `swap/manager.rs` - Background state machine: processes WS status updates and polls `Settling` swaps for delivery
- `evm/contracts.rs` - ABI encoding via `alloy-sol-types` (Router, ERC20Swap, OFT, CCTP)
- `evm/signing.rs` - EIP-712/EIP-191/raw ECDSA signing (incl. Router `ClaimSend`/`ClaimCctp`)
- `evm/alchemy.rs` - EIP-7702 gas-sponsored transactions (users don't need ETH); persists `call_id` for resume
- `evm/provider.rs` - Thin JSON-RPC wrapper for Arbitrum
- `evm/oft.rs` - LayerZero USDT0 OFT registry, fetched at runtime from the deployments API
- `evm/cctp.rs` - Circle CCTP v2: Iris fee client + delivery-status (`CctpMessageStatus`)
- `evm/lz_scan.rs` - LayerZero Scan client for OFT cross-chain delivery confirmation
- `evm/lz_options.rs` - LayerZero executor extra-options (e.g. Polygon `lzReceive` gas bump)
- `evm/lockup.rs` - On-chain ERC20Swap lockup liveness check (the recovery primitive)
- `evm/recipient.rs` - Destination-address validation per transport; rejects known token contracts
- `solana/` - Solana RPC + ATA derivation (CCTP USDC-on-Solana mint recipient)
- `keys.rs` - BIP-32 HD key derivation for EVM keys + deterministic preimage derivation
- `store.rs` - `BoltzStorage` trait (callers implement for persistence)
- `events.rs` - Event emitter with `BoltzEventListener` trait

### Test Conventions

- Use `#[macros::test_all]` for sync tests (runs on both native and WASM)
- Use `#[macros::async_test_all]` for async tests (native: tokio::test, WASM: wasm_bindgen_test)
- Each test module includes `#[cfg(feature = "browser-tests")] wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);`
- Use `platform_utils::time` instead of `std::time` for WASM compatibility
- Use `platform_utils::tokio` instead of `tokio` directly for WASM compatibility

### Key Design Decisions

- **No panics in production code** - always use `Result`, never `expect`/`unwrap`
- **WASM-compatible throughout** - alloy-rs primitives, platform-utils abstractions, no filesystem deps in lib
- **Deterministic preimage derivation** - preimage = SHA256(private_key), no need to store preimages
- **Gas abstraction** - EIP-7702 via a configurable gas-sponsor URL (wraps Alchemy server-side; no hardcoded API key/policy) so users never need ETH
- **Unified destination registry** - one `Destination` table spans Direct/OFT/CCTP bridges; the public API exposes only the coarse `BridgeKind`
- **End-to-end slippage** - a single tolerance anchored on expected stablecoin output gates the claim-time DEX quote drift and the on-chain `minOut` floor; bridge fees (CCTP) are folded in, never charged as a separate per-hop tolerance
- **Confirmed cross-chain delivery** - bridged swaps complete only after delivery is confirmed (CCTP via Circle Iris, OFT via LayerZero Scan); CCTP persists the authoritative `feeExecuted`-adjusted delivered amount
- **Recovery via on-chain liveness** - no blockchain scanning; recovery relies on the ERC20Swap lockup state check plus the persisted Alchemy `call_id` for resume after a crash

## Workspace Configuration

- Rust edition 2024
- Clippy: pedantic + suspicious + complexity + perf warnings
- Release builds: LTO + `opt-level = "z"` for size
