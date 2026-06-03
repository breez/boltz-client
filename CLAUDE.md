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

The full architecture map lives in [`docs/architecture.md`](./docs/architecture.md)
— what the project is, the swap flow, and a per-module reference. The reasoning
behind design choices and every deliberate divergence from `boltz-web-app` lives
in [`docs/decisions.md`](./docs/decisions.md), an append-only, dated decision log.

In one line: a headless, WASM-compatible Rust library for **reverse-only** swaps
(Lightning sats → stablecoin at a destination), ported from `boltz-web-app` with
deliberate, documented divergences.

**Keep both docs current.** Update `architecture.md` in the same change that
alters the design; append a dated entry to `decisions.md` whenever you make a
notable or divergent decision. Record *why*, not *what* — the code and
`architecture.md` own the "what".

## Test Conventions

- Use `#[macros::test_all]` for sync tests (runs on both native and WASM)
- Use `#[macros::async_test_all]` for async tests (native: tokio::test, WASM: wasm_bindgen_test)
- Each test module includes `#[cfg(feature = "browser-tests")] wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);`
- Use `platform_utils::time` instead of `std::time` for WASM compatibility
- Use `platform_utils::tokio` instead of `tokio` directly for WASM compatibility

## Workspace Configuration

- Rust edition 2024
- Clippy: pedantic + suspicious + complexity + perf warnings
- Release builds: LTO + `opt-level = "z"` for size
