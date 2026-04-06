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

```
Lightning (sats) -> tBTC (Boltz reverse swap) -> USDT (DEX on Arbitrum) -> destination chain (OFT bridge)
```

Key modules in `crates/lib/src/`:
- `api/` - Boltz REST API client + WebSocket status subscriber
- `swap/reverse.rs` - Core swap executor (prepare quote, create swap, claim + DEX)
- `swap/manager.rs` - Background state machine processing swap status updates
- `evm/contracts.rs` - ABI encoding via `alloy-sol-types` (Router, ERC20Swap, OFT)
- `evm/signing.rs` - EIP-712/EIP-191/raw ECDSA signing
- `evm/alchemy.rs` - EIP-7702 gas-sponsored transactions (users don't need ETH)
- `evm/provider.rs` - Thin JSON-RPC wrapper for Arbitrum
- `evm/oft.rs` - LayerZero OFT deployment registry for cross-chain bridging
- `keys.rs` - BIP-32 HD key derivation for EVM keys + deterministic preimage derivation
- `store.rs` - `BoltzStorage` trait (callers implement for persistence)
- `events.rs` - Event emitter with `BoltzEventListener` trait
- `recover.rs` - Blockchain scanning to recover abandoned swaps

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
- **Gas abstraction** - Alchemy EIP-7702 so users never need ETH

## Workspace Configuration

- Rust edition 2024
- Clippy: pedantic + suspicious + complexity + perf warnings
- Release builds: LTO + `opt-level = "z"` for size
