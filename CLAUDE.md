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

**Keep both docs current — but keep the bar high.** Update `architecture.md`
in the same change that alters the design; append a dated entry to
`decisions.md` whenever you make a notable or divergent decision. Record *why*,
not *what* — the code and `architecture.md` own the "what".

**Keep entries as short as possible while making the point.** A decision entry
is the `Diverges` line plus, ideally, a single tight paragraph: the problem, the
choice, and the load-bearing trade-off — nothing more. Leave out mechanics,
step-by-step behaviour, and rejected-alternative detail the code/comments
already carry. If an entry runs to multiple long paragraphs, it's too long.

Only document decisions that are **high-impact or non-obvious**: a divergence
from `boltz-web-app`, a security boundary, a money-critical invariant, or a
trade-off a future reader would otherwise re-litigate. Do **not** log routine,
self-explanatory hardening — input validation, status-machine ordering guards,
defensive re-checks, overflow guards, renames — even when it closes an audit
finding; the code and its comments already carry that. The same bar applies to
`architecture.md`: it describes subsystem structure and load-bearing
invariants, not low-level mechanics obvious from reading the function. When in
doubt, leave it out: a thorough code comment at the site usually suffices.

## Code Comments

Same bar for rustdoc and inline comments: short and to the point. State the
load-bearing *why* — the contract, invariant, or non-obvious trade-off — not a
play-by-play the signature and body already show. Prefer a sentence or two over
a paragraph; if a doc comment runs long, cut the mechanics and keep the
rationale. When in doubt, leave it out.

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
