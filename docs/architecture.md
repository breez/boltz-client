# Architecture

> **Living document.** Edit this in place as the architecture changes. It is the
> canonical map of the project: what it is and how the pieces fit together.
>
> For **why** a given choice was made, see [`docs/decisions.md`](./decisions.md)
> (an append-only decision log). Do not duplicate rationale here — reference it.
> See also the [`README`](../README.md) and [`CLAUDE.md`](../CLAUDE.md) for build/test
> commands and conventions.

## 1. What this is

`boltz-client` is a **headless Rust library** (WASM-compatible, no UI, no
filesystem deps) that performs **reverse-only swaps**: it turns Lightning sats
into a stablecoin at a destination address. The path is always
`Lightning (sats) -> tBTC (Boltz reverse swap) -> stablecoin (DEX on Arbitrum) -> destination`.
The Boltz exchange only ever sees a bridge-agnostic `BTC -> TBTC` reverse swap;
everything downstream (the DEX swap and the cross-chain bridge) is **client-derived
and client-executed**.

The single public entrypoint is `BoltzService` (see `crates/lib/src/lib.rs`),
which exposes a two-step `prepare` / `create` reverse-swap API plus lifecycle,
slippage, and delivery-refresh controls. Callers supply persistence (the
`BoltzStorage` trait) and a seed; the library owns all swap orchestration,
signing, gas abstraction, and background state machine.

**Reference implementation.** The behavioral reference is
[`boltz-web-app`](https://github.com/BoltzExchange/boltz-web-app), a SolidJS
browser UI plus its `boltz-swaps` TypeScript package. This crate is a headless
port of that logic. It **diverges deliberately** in documented ways: there is a
single `BoltzService` facade (the web app spreads the flow across reactive
stores/components), an explicit `Settling` state machine and confirmed-delivery
gating, client-derived bridge routing, and a native+WASM cross-platform
infra layer the browser app has no analog for. Where this doc cites a web-app
counterpart, treat it as orientation, not a guarantee of file-for-file parity.

## 2. Workspace layout

| Crate | Name | Responsibility |
|---|---|---|
| `crates/lib` | `boltz-client` | Core library: Boltz API client, EVM/Solana contracts, swap orchestration, key management, background state machine. The public surface is `BoltzService`. |
| `crates/cli` | `boltz-cli` | Interactive native REPL (rustyline) around `BoltzService` for manually exercising swaps. Developer/testing harness with file-backed storage — not a production interface. |
| `crates/macros` | (proc-macros) | Target-conditional `#[async_trait]` (Send vs ?Send), dual-target test macros (`test_all` / `async_test_all`), and Rust→TypeScript type mirroring (`extern_wasm_bindgen`, `derive_from`/`derive_into`). |
| `crates/platform-utils` | `platform-utils` | Cross-platform `HttpClient` (reqwest on native and WASM), unified `HttpError`, and target-correct `tokio`/`time` re-exports. The native↔WASM seam. |

## 3. The swap flow

Reverse-only. The DEX always produces a stablecoin on **Arbitrum**; how it
reaches the user depends on the destination's **bridge**:

- **Direct** — delivered on Arbitrum (USDT, or USDC-on-Arbitrum). No cross-chain hop. Completes immediately.
- **Oft** — LayerZero USDT0 OFT bridge (native or legacy mesh). Cross-chain; passes through `Settling`.
- **Cctp** — Circle CCTP v2 burn + mint (USDC; EVM chains + Solana). Cross-chain; passes through `Settling`.

A **Router** contract makes claim + DEX atomic: one Alchemy-sponsored EIP-7702
EVM tx claims tBTC from `ERC20Swap`, executes the DEX swap, and bundles the
direct sweep / OFT send / CCTP burn in the same call. Bridge routing is fully
client-derived — the Boltz API is bridge-agnostic. Bridged (`Oft`/`Cctp`) swaps
enter a non-terminal `Settling` state and complete **only after confirmed
cross-chain delivery** (LayerZero Scan for OFT, Circle Iris for CCTP); `Direct`
skips straight to `Completed`.

```
                    LIGHTNING                                   ARBITRUM                              DESTINATION
                    =========                                   ========                              ===========

  user pays  ─────► hold invoice ──► Boltz locks tBTC ──► ┌──────────── Router (atomic, EIP-7702 gas-sponsored) ────────────┐
  (sats)            (BTC->TBTC,        on ERC20Swap        │                                                                 │
                     bridge-agnostic)  (lockup)            │  claim tBTC ──► DEX swap (tBTC->stablecoin) ──► bridge dispatch  │
                                                           │                                                  │              │
                                                           └──────────────────────────────────────────────── │ ────────────┘
                                                                                                              ▼
                                                              ┌────────────────────┬──────────────────────────┬───────────────────────┐
                                                              │ Direct             │ Oft                      │ Cctp                  │
                                                              │ sweep on Arbitrum  │ LayerZero USDT0 OFT send │ Circle CCTP v2 burn   │
                                                              │ (USDT / USDC-Arb)  │ (native/legacy mesh)     │ (USDC; EVM + Solana)  │
                                                              └─────────┬──────────┴───────────┬──────────────┴───────────┬───────────┘
                                                                        │                      │                          │
                                                              Completed │            Settling  │  Settling                │
                                                              (immediate)         (poll LZ Scan)│   (poll Circle Iris)     │
                                                                        ▼                      ▼                          ▼
                                                                   delivered            DELIVERED ─► Completed   forwarded+attested ─► Completed


  Status lifecycle (BoltzSwapStatus):
    Created ─► InvoicePaid ─► TbtcLocked ─► Claiming ─► [Settling] ─► Completed
                                                  └──────────────────► Failed{reason}
    (also: Expired)   Settling: Oft/Cctp only; Direct goes Claiming ─► Completed
    Terminal: Completed | Failed | Expired
```

**Two-step API.** `prepare_reverse_swap[_from_sats]` is a pure quote (no side
effects); `create_reverse_swap` commits — it reserves the next HD key index,
inserts the swap, returns the hold invoice, and begins background monitoring.
The background `SwapManager` reacts to Boltz WebSocket status updates, triggers
the atomic claim on `transaction.confirmed`, polls the EVM receipt, and gates
bridged-swap completion on confirmed delivery. Recovery after a crash relies on
the persisted Alchemy `call_id` (stored as the `pending_call_id` field on
`BoltzSwap`) and an on-chain `ERC20Swap` lockup liveness check — no blockchain
scanning.

## 4. Module reference

All paths are under `crates/lib/src/` unless noted.

### Service facade & errors

| File | Responsibility | Key types |
|---|---|---|
| `lib.rs` | `BoltzService` — the single public entrypoint. Wires together every subsystem in `new` (does network I/O), exposes prepare/create + lifecycle/slippage/delivery controls and destination discovery. | `BoltzService`, `USER_AGENT` |
| `error.rs` | One flat error type for the whole public surface; `From<HttpError>` maps HTTP failures to `Api{reason,code}`. | `BoltzError` (`Api`/`Evm`/`WebSocket`/`Signing`/`Store`/`SwapExpired`/`SwapFailed`/`QuoteExpired`/`AmountOutOfRange`/`InvalidQuote`/`QuoteDegradedBeyondSlippage`/`DuplicatePreimage`/`Generic`) |

### API layer (`api/`)

| File | Responsibility | Key types |
|---|---|---|
| `api/mod.rs` | Thin REST client over the injected `HttpClient`: reverse pairs, create swap, status, lockup tx, DEX quote/encode, chain contracts. Sends the referral header on every request (attribution). | `BoltzApiClient` |
| `api/types.rs` | serde request/response DTOs and WS wire types. `EncodeRequest` amounts serialize as decimal **strings** (the API rejects integer-typed amounts). | `ReversePairsResponse`, `CreateReverseSwap{Request,Response}`, `QuoteResponse`, `EncodeRequest/Response`, `QuoteCalldata`, `ContractsResponse`, `SwapStatusResponse`, `WsSubscribeMessage`, `WsMessage`, `WsSwapUpdate` |
| `api/ws.rs` | One persistent, reconnecting WebSocket multiplexing `swap.update` for all tracked IDs onto a single mpsc channel. 15s keep-alive ping, 5s reconnect, re-subscribe after reconnect, Drop-time task abort. Does **not** replay missed updates — on (re)subscribe Boltz re-pushes each swap's current status, and the manager reconciles state against the **on-chain `ERC20Swap` lock** (plus the receipt / persisted `call_id`), never on the WS event alone. | `SwapStatusSubscriber`, `SwapStatusUpdate` |

### Config & model

| File | Responsibility | Key types |
|---|---|---|
| `config.rs` | Runtime config + a body of protocol-fact `const`s (chain IDs, contract/token addresses, CCTP/OFT params, fee scales, default URLs/cadences). `mainnet(referral_id)` is the canonical Arbitrum preset. | `BoltzConfig`, `AlchemyConfig`; consts `ARBITRUM_*_ADDRESS`, `CCTP_*`, `ARBITRUM_USDT0_NATIVE/LEGACY_OFT`, `SOLANA_*_MINT`, `*_SLIPPAGE_BPS`, `SATS_TO_TBTC_FACTOR`, default endpoint URLs |
| `models.rs` | The persisted swap record, its lifecycle, and the **unified destination registry** spanning all three bridges. Public API exposes only the coarse `BridgeKind`; claim dispatch resolves the data-carrying internal `Bridge` from the destination. | `BoltzSwap` (preimage/hash are *not* fields — derived; crash-resume fields `pending_call_id`/`claim_tx_hash`), `BoltzSwapStatus`, `DestinationRegistry` (a `Vec<Destination>` looked up by `find(chain, asset)`), `Destination` (identified by its `(chain_label, asset)` pair), `Asset`, `Bridge`/`BridgeKind`, `NetworkTransport`, `Usdt0Kind`, `CCTP_DESTINATIONS`, DTOs `PreparedSwap`/`CreatedSwap`/`SwapLimits`/`DestinationOption` |

### Swap orchestration (`swap/`)

| File | Responsibility | Key types |
|---|---|---|
| `swap/reverse.rs` | Core executor. Works fee/DEX-quote math backwards (`prepare`) or forwards (`prepare_from_sats`); creates the Boltz reverse swap; at claim time verifies on-chain lockup, derives the preimage, builds + signs the **single atomic Router tx** (claim + DEX + bridge), branching per bridge. Owns the end-to-end slippage model and quote selection. | `ReverseSwapExecutor`, `ClaimAddresses` (`.output_token_address` = DEX output token; may be USDC), `FeeCalc`, `QuoteDirection`, slippage helpers (`compute_claim_floor`, `check_quote_drift`, `resolve_slippage_bps`) |
| `swap/manager.rs` | Headless background state machine. One `tokio::select!` loop multiplexes WS updates, a track-swap channel, a background ticker, and shutdown. Drives `BoltzSwapStatus` transitions, triggers/poll the claim, and gates bridged completion on confirmed delivery. A single store-driven pass (`poll_pending_swaps`) on the ticker both confirms delivery (`Settling`) and autonomously recovers stuck `Claiming` swaps so progress doesn't depend solely on a WS event: it re-checks the claim receipt, and a lockup spent past the timeout with no success receipt is finalized `Failed`. Crash-resume: with a tx hash, verify the receipt; else poll Alchemy by the persisted `pending_call_id` to recover the tx hash; else fall back to the on-chain lock-state check. Never finalizes on a WS message alone. | `SwapManager`, `post_claim_status` (Direct=Completed, Oft/Cctp=Settling), `poll_pending_swaps`/`recover_claiming_swap`/`confirm_delivery`; `RECEIPT_POLL_MAX_ATTEMPTS=60`/`_INTERVAL_SECS=5` |

### EVM (`evm/`)

| File | Responsibility | Key types |
|---|---|---|
| `evm/contracts.rs` | Codec layer: one `alloy sol!` block declaring Router v2 / ERC20Swap / ERC20 / OFT / CCTP surfaces; pure functions to ABI-encode the three atomic Router claims, recompute EIP-712 struct hashes, and decode return values + delivered-amount event logs. | `encode_claim_erc20_execute{,_oft,_cctp}` (encode the `claimERC20Execute*` selectors), `Erc20Claim`, `Call`/`quote_calldata_to_call`, `SendData`/`hash_send_data`, `CctpData`/`hash_cctp_data`, `DeliveredAmountSource`/`DeliveredAmount`, `decode_delivered_from_logs`, address/U256 helpers |
| `evm/signing.rs` | The EVM signer: raw 32-byte digests (EIP-7702 auth), EIP-191 personal_sign (Alchemy UserOps), and EIP-712 typed data for the cooperative `ERC20Swap` claim and Router `Claim`/`ClaimSend`/`ClaimCctp`. Normalizes to legacy `{v=27/28,r,s}`. | `EvmSigner`, `EvmSignature`, `erc20swap_eip712::Claim`, `router_eip712::{Claim,ClaimSend,ClaimCctp}` |
| `evm/alchemy.rs` | EIP-7702 gas-sponsored tx flow: `wallet_prepareCalls` → sign auth + UserOp challenges → `wallet_sendPreparedCalls` → poll `wallet_getCallsStatus`. Splits submit (returns `call_id`) from confirm so the caller can persist it (as `pending_call_id` on the swap) first (crash resume). | `AlchemyGasClient`, `EvmCall`, `AlchemyResult`; `submit_calls`/`poll_call_status` |
| `evm/provider.rs` | Thin WASM-safe JSON-RPC client for read-only Arbitrum ops (`eth_call`, `eth_getTransactionReceipt`, `eth_chainId`, `eth_getLogs`, `eth_blockNumber`, `eth_l1_block_number`) with 429 backoff. `eth_call` is pinned to `latest`. `eth_block_number` is the **L2** height; `eth_l1_block_number` reads `l1BlockNumber` for the **L1** height a swap's `timeout_block_height` is denominated in. | `EvmProvider`, `TxReceipt` (`is_success`), `LogEntry` |
| `evm/lockup.rs` | On-chain `ERC20Swap` lockup liveness check — the recovery primitive. Reconstructs `hashValues` from persisted swap data and queries `swaps(bytes32)`. | `is_swap_still_locked`, `is_swap_still_locked_by_swap` |
| `evm/recipient.rs` | Encodes a destination address into the `bytes32` form OFT sends require (EVM left-pad / Solana raw pubkey / Tron base58check); the encoder **is** the validator. Plus token-address normalization for the don't-send-to-token-contract blocklist. | `encode_oft_recipient`, `is_valid_destination_address`, `normalize_token_address` |

### Bridges (`evm/`)

| File | Responsibility | Key types |
|---|---|---|
| `evm/oft.rs` | Builds the `DestinationRegistry` from the USDT0 deployments feed (native + legacy meshes), verifies the source OFT against compile-time pins (theft guard), folds in CCTP + Direct destinations, and provides the legacy-mesh 3 bps fee inverse. | `fetch_chain_registry`/`parse_chain_registry`, `verify_pinned_source_oft`, `legacy_mesh_source_amount`; `LEGACY_MESH_FEE_BPS=3` |
| `evm/lz_scan.rs` | LayerZero Scan client: confirms OFT cross-chain delivery by GUID. Only `DELIVERED` finalizes; terminal LZ failures keep polling; 404 = not-yet-indexed. | `LzScanClient`, `LzMessageStatus`/`is_delivered`; `LZ_STATUS_DELIVERED` |
| `evm/lz_options.rs` | Encodes LayerZero v2 type-3 executor extra-options (Solana ATA-creation rent, Polygon `lzReceive` gas bump). Must be byte-identical everywhere it appears. | `build_extra_options`; `SOLANA_ATA_RENT_EXEMPT_LAMPORTS`, `POLYGON_LZ_RECEIVE_GAS_BUMP` |
| `evm/cctp.rs` | Circle CCTP v2: encodes `mintRecipient` + forwarding `hookData` (EVM/Solana), burn-fee math, Iris fee quoting, and delivery-status polling (authoritative `feeExecuted`-adjusted delivered amount). | `CctpFee`, `CctpFeeClient`, `CctpMessageStatus`/`is_forwarded`, `compute_total_fee`/`cctp_required_burn`/`add_fee_buffer` |

### Solana (`solana/`)

| File | Responsibility | Key types |
|---|---|---|
| `solana/ata.rs` | Deterministically derives the recipient's SPL-Token ATA (the CCTP/OFT mint target) by reimplementing `find_program_address` (bump loop + SHA256 + ed25519 off-curve check). | `derive_ata`, `is_on_curve` |
| `solana/rpc.rs` | Minimal Solana JSON-RPC client; one op: `getAccountInfo` to report whether an ATA already exists (so the cross-chain message pre-funds ATA creation only when needed). | `SolanaRpcClient`/`account_exists` |

### Keys, store, events

| File | Responsibility | Key types |
|---|---|---|
| `keys.rs` | BIP-32 EVM key derivation (gas signer at `m/44/{chainId}/1/0`, preimage keys at `m/44/{chainId}/0/0/{index}`) and **deterministic preimage = SHA256(privkey)**, hash = SHA256(SHA256(privkey)). EIP-55 helpers. All levels non-hardened (restore compatibility). | `EvmKeyManager`, `EvmKeyPair`, `keccak256` |
| `store.rs` | The persistence boundary callers implement: insert/update/get swap, list active (non-terminal), and atomically reserve the next HD key index. `increment_key_index` durability is the sole defense against preimage reuse. | `BoltzStorage` (trait); a volatile `MemoryBoltzStorage` exists only under `#[cfg(test)]` for the crate's own unit tests — never compiled when the crate is a dependency, so it can't back a production service |
| `events.rs` | In-process event broadcast to the embedding app. | `BoltzSwapEvent` (`SwapUpdated`/`QuoteDegraded`), `BoltzEventListener`, `EventEmitter` |

### Cross-platform infra

| Crate / file | Responsibility | Key types |
|---|---|---|
| `platform-utils` (`http/`, `auth.rs`, `lib.rs`) | reqwest-based `HttpClient` (one backend for native and WASM; native adds HTTP/2 + TCP keepalives, WASM omits the User-Agent), unified `HttpError` with `.status()`, and target-correct `tokio`/`time` re-exports. WASM gate is `all(target_family="wasm", target_os="unknown")`. | `HttpClient`, `HttpError`, `HttpResponse`, `DefaultHttpClient`, `create_http_client`, `ContentType` |
| `macros` (`src/lib.rs`) | Proc-macros for target-conditional async traits/tests and Rust→TS type mirroring. | `#[async_trait]`, `#[extern_wasm_bindgen]`, `derive_from`/`derive_into`, `test_all`/`async_test_all` (+ not_wasm/wasm variants) |
| `cli` (`src/main.rs`) | Native REPL harness around `BoltzService`: clap args, mnemonic load-or-generate, file-backed `BoltzStorage`, rustyline command loop (`info`/`limits`/`prepare`/`swap`/`accept`/`refresh-deliveries`/`exit`). `FileBoltzStorage` is non-atomic and unfit for production. | `Cli`, `Command`, `PrintingEventListener`, `FileBoltzStorage` |

## 5. Cross-cutting design principles

These are invariants the whole codebase upholds. For the reasoning behind each,
see [`docs/decisions.md`](./decisions.md).

- **No panics in production code.** Always `Result`, never `expect`/`unwrap`. (One deliberate exception: `ReqwestHttpClient::new` panics if the reqwest client fails to build, treated as unrecoverable misconfiguration rather than a per-request error.)
- **WASM-compatible throughout.** alloy-rs primitives, `platform_utils` abstractions, no filesystem deps in the lib. Use `platform_utils::{time,tokio}`, never `std::time` / `tokio` directly. Annotate async traits with `#[macros::async_trait]`, never `async_trait::async_trait`.
- **Deterministic preimage derivation.** `preimage = SHA256(private_key)`; preimages are never stored. Correctness depends entirely on stable seed + chainId + index — changing the derivation path or hashing scheme silently invalidates recovery of all existing swaps.
- **Gas abstraction.** EIP-7702 via a configurable gas-sponsor URL (wraps Alchemy server-side; no hardcoded API key/policy) so users never need ETH on Arbitrum.
- **Unified destination registry.** One `Destination` table spans Direct/OFT/CCTP. The public API exposes only the coarse `BridgeKind` (display/UX); actual claim dispatch resolves the data-carrying internal `Bridge` from the destination.
- **End-to-end slippage.** A single tolerance anchored on expected stablecoin output gates both the claim-time DEX quote drift and the on-chain `minOut` floor; bridge fees (CCTP) are folded in, never charged as a separate per-hop tolerance. The on-chain `minAmountOut`/`minAmount` is the sole floor bounding DEX manipulation — never widen it casually.
- **Confirmed cross-chain delivery.** Bridged swaps complete only after delivery is confirmed (CCTP via Circle Iris once forwarded **and** attested; OFT via LayerZero Scan `DELIVERED`). CCTP persists the authoritative `feeExecuted`-adjusted delivered amount; the source burn amount is only an estimate.
- **Recovery via on-chain liveness.** No blockchain scanning. Recovery uses the `ERC20Swap` lockup state check plus the persisted Alchemy `call_id` (the `pending_call_id` field) for resume after a crash. Never finalize a swap on a WebSocket success event alone — always verify via receipt, `call_id` recovery, or lock-state.

### Load-bearing invariants worth surfacing

- `create_reverse_swap` MUST persist the incremented key index durably before returning; Boltz's HTTP 409 (`DuplicatePreimage`) must **not** auto-retry with the next index.
- `Settling` is non-terminal and is short-circuited *before* the WS status match in the manager — treating it as terminal, or letting a late `*.expired` past it, would strand or wrongly fail an already-claimed bridged swap. The same protection extends one state earlier: a swap in `Claiming` whose lockup is already spent on-chain must not be finalized `Expired`/`Failed` by a late/spoofed terminal WS event — the manager re-checks the lock and advances it through the post-claim path instead (`handle_terminal_ws_event`).
- The lockup `timeout_block_height` is denominated in **L1** block height; it is validated (≥ `MIN_TIMEOUT_L1_MARGIN` over the current L1 height via `eth_l1_block_number`, **not** the L2 `eth_block_number`) both as an early abort in `create()` and, fail-safe, immediately before the preimage is revealed in `claim_and_swap` — a too-short timeout would let a malicious server refund and settle the LN HTLC with the leaked preimage.
- `create_probe_invoice` returns an invoice that must **never** be paid (its random preimage is discarded; payment locks funds unrecoverably).
- OFT `extraOptions`/`composeMsg`/`oftCmd` must be byte-identical across every `quoteOFT`/`quoteSend` call, the on-chain `SendData`, and the EIP-712 hash input — the Router signs over their keccak hashes.

## 6. Pointers

- **Why** decisions were made: [`docs/decisions.md`](./decisions.md) (append-only).
- Build/test/quality commands and conventions: [`CLAUDE.md`](../CLAUDE.md).
- Project overview: [`README.md`](../README.md).
- Reference implementation: [`boltz-web-app`](https://github.com/BoltzExchange/boltz-web-app) (SolidJS UI + `boltz-swaps` package) — orientation only; this crate diverges as documented above.
