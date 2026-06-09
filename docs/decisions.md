# Design Decision Log

Append-only log of notable design decisions in `boltz-client`. It records WHY a
thing was decided, not WHAT the code does — the code and `architecture.md` own
the "what". Every entry is dated to the commit that introduced the decision and
carries a "Diverges from boltz-web-app:" line (boltz-web-app is the reference
implementation this crate was ported from). Never edit an old entry; if a
decision changes, append a new dated entry that supersedes it.

## 2026-04-06 — Headless, dual-target (native + browser-WASM) library
Diverges from boltz-web-app: yes — the web app is browser-only SolidJS/TS; this is a headless Rust lib that must compile to native AND browser-WASM.

The whole crate is built so one source compiles and runs both natively (CLI/servers) and as browser WASM. `platform-utils` cfg-gates the HTTP stack (native bitreq / WASM reqwest), clock (`std::time` / `web_time`), and runtime (`tokio` / `tokio_with_wasm`); `macros` provides a Send-bound-toggling `async_trait` and `test_all`/`async_test_all` so a single test body runs under both tokio and wasm-bindgen-test. The gate predicate is the compound `all(target_family="wasm", target_os="unknown")` so WASI falls into the native branch — only browser-style wasm lacks threads/clock/sockets (`platform-utils/src/lib.rs:9-19`).

## 2026-04-06 — Caller-owned persistence via the `BoltzStorage` trait
Diverges from boltz-web-app: yes — the web app bundles a concrete localforage/IndexedDB store; this lib ships none.

The library defines a `BoltzStorage` trait and the caller supplies durability, keeping the lib free of filesystem/IndexedDB deps and WASM-safe (`store.rs:4-24`). `update_swap` is update-only and errors if the record is absent, so a lost insert surfaces loudly rather than silently upserting (the web app's `setSwapStorage` is an unconditional upsert). The CLI provides the simplest plausible native impl (one JSON file per swap, non-atomic `std::fs`) and openly documents in-code that a production SDK must use atomic write-temp-then-rename.

## 2026-04-06 — Deterministic preimage derivation; never stored
Diverges from boltz-web-app: no (scheme) — the derivation is a faithful port; the only difference is we re-derive rather than cache.

Preimage = `SHA256(per-swap private key)`, HTLC hash = `SHA256(SHA256(privkey))`, so preimages are fully recoverable from seed+index (`keys.rs:63-77`). This is a faithful port of the web app's scheme: `rescueDerivation.ts` defines `derivePreimage(privateKey) = sha256(privateKey)` at the same `m/44/{chainId}/0/0/{index}` path, which is what makes our keys cross-validate byte-for-byte against `@scure/bip32`. The one implementation difference: the web app *stores* the derived preimage on the swap record (`swapCreator.ts:318`), whereas this crate never persists it and re-derives on demand. Consequence either way: the key-index counter is money-critical — an index regression after a crash re-derives a used preimage. All BIP-32 levels are deliberately non-hardened so the Boltz `/swap/restore` xpub flow can derive child public keys (`keys.rs:14-23,186-189`); `chainId` is embedded in the path so chains yield isolated key trees from one seed.

## 2026-04-06 — Two key roles from one seed: stable gas signer vs. per-swap preimage key
Diverges from boltz-web-app: no

A single HD seed yields an account-level signing/claim-address key at `m/44/{chainId}/1/0` (reused across swaps, keeping the on-chain claim identity stable) and indexed per-swap preimage keys at `m/44/{chainId}/0/0/{index}`. Derivation, EIP-55 checksumming, and preimage hashing are reimplemented self-contained (keccak/sha2) and cross-validated byte-for-byte against `@scure/bip32` + `@noble/hashes` vectors (`keys.rs:370-458`) to guarantee compatibility with the JS stack without a dependency.

## 2026-04-06 — Thin transport API client; policy lives in the swap layer
Diverges from boltz-web-app: yes — the web app's client sorts DEX quotes and zero-guards inside the client; this client returns them raw.

`api/mod.rs` is a thin transport wrapper: it formats URLs, returns `Vec<QuoteResponse>` verbatim, and funnels all failures into a single flat `BoltzError::Api{reason,code}`. Quote ranking (`pick_best_quote`), the empty-list error, and the zero-amount `InvalidQuote` rejection are centralized in `swap/reverse.rs`. The web app instead sorts quotes (descending In / ascending Out) and short-circuits a zero *input* amount inside its client. Keeping `api/` policy-free is the deliberate split.

## 2026-04-06 — Reverse-swap-only API surface (8 methods)
Diverges from boltz-web-app: yes — the web app's client.ts is a general-purpose SDK exposing every swap direction/type.

The client exposes only reverse-swap, DEX-quote/encode, and chain/contracts discovery (`api/mod.rs:31-109`). The submarine, chain-swap, commitment, asset-rescue, xpub-restore, cooperative partial-signature, fee-estimation, node-stats, and BOLT12 endpoints are all omitted because the product is reverse-only (LN -> stablecoin) and claims on Arbitrum via the Router. Likewise the EVM ABI layer declares no EtherSwap/native-ETH surface (only ERC20Swap claim/version/hashValues/swaps) and uses a single current ABI — the web app carries versioned ERC20Swap+EtherSwap ABIs and branches by on-chain `version()` to interoperate with legacy (<=5) deployments.

## 2026-04-06 — Trait-object `HttpClient` injected at construction
Diverges from boltz-web-app: yes — the web app's fetcher is a free function bound to the browser's global `fetch`.

`BoltzApiClient` holds a `Box<dyn HttpClient>` (from `platform-utils`) rather than calling a global fetch, which lets the same client run native (reqwest) and WASM (browser fetch) and lets callers inject a custom User-Agent. Per-component clients are separate cheap instances rather than one shared `Arc`'d client — the comment notes there is no shared connection pool so sharing isn't worth the signature churn (`lib.rs:75-77`).

## 2026-04-06 — Caller-set optional invoice `description` / `invoiceExpiry`
Diverges from boltz-web-app: yes — the web app's createReverseSwap accepts neither parameter and sends neither field.

`CreateReverseSwapRequest` exposes optional `description` and `invoiceExpiry`, each `skip_serializing_if = Option::is_none` so omitted when unset (`api/types.rs:56-59`). This lets a headless caller customize the LN invoice memo and lifetime; absent them, Boltz uses its server-side defaults. u128 amount fields are serialized as decimal strings via a custom serializer because the Boltz API rejects integer-typed amounts — this mirrors the web app's stringified BigInts.

## 2026-04-06 — App-level WebSocket keep-alive ping (15s) on a command-driven reader
Diverges from boltz-web-app: yes — the browser web app sends no app-level ping and relies on the platform WebSocket for liveness.

The headless subscriber sends a `{"op":"ping"}` text frame every 15s and reconnects with a 5s backoff, because a native/WASM client has no ambient browser connection management to keep idle intermediaries from dropping the socket (`api/ws.rs:13-15,21-22,219-224`). Architecture: one WebSocket multiplexes all tracked swap IDs onto a single mpsc channel consumed by the background manager; public methods push `Subscribe`/`Unsubscribe`/`Shutdown` commands to the single reader task that owns the split sink/stream (avoids cross-task write locking); on every reconnect it resubscribes the full ID set; `Drop` aborts the task as a leak safety net.

## 2026-04-06 — Config split: per-instance struct vs. module-level `const`
Diverges from boltz-web-app: no

Runtime-overridable values (URLs, slippage, poll cadence, chain id, referral id) live on `BoltzConfig`/`AlchemyConfig` so callers can point at sandbox/testnet; truly fixed protocol facts (contract addresses, CCTP domains, decimal factors, EIP-712 typehash inputs) are `pub const` so they cannot be misconfigured per-instance (`config.rs:2-94,96-238`). The WS URL is derived from `api_url` (https->wss, append `/v2/ws`) so there is one source of truth for the host. Config-constant tests carry `test_all` + browser-gated `wasm_bindgen_test_configure` because the lib is published as two targets.

## 2026-04-06 — Hand-rolled minimal JSON-RPC provider over `HttpClient`
Diverges from boltz-web-app: yes — the web app uses a viem `PublicClient`.

The EVM provider constructs raw JSON-RPC envelopes by hand and implements only the read methods actually needed (`eth_call`, `eth_getTransactionReceipt`, `eth_chainId`, `eth_getLogs`, `eth_blockNumber`), reusing the same `Box<dyn HttpClient>` as the rest of the lib (`provider.rs:16-19,137-147`). Rejected: an alloy/ethers provider transport, which would add weight and a second HTTP stack. Uses fixed `id=1` and `latest` block tag (no batching/historical state); a null receipt deserializes to `Ok(None)` so callers distinguish "not yet mined" from failure.

## 2026-04-06 — EIP-712 via alloy `sol!`; type hashes pinned by test
Diverges from boltz-web-app: no

`Claim`/`ClaimSend` structs are declared inside `sol!` blocks so alloy's `SolStruct` derives the EIP-712 type hash and encoding automatically, avoiding hand-built `keccak256(abi.encode(...))` and field-ordering bugs (`signing.rs:18-70`). The two name-colliding `Claim` structs (ERC20Swap vs Router) live in separate modules so each type string computes correctly. Every struct has a test asserting its literal type-hash hex against `cast keccak` / deployed bytecode. ERC20Swap domain `version` is a runtime parameter (read on-chain); the Router domain version is hardcoded `"2"`.

## 2026-04-06 — Single sponsor URL for gas abstraction; no client-held key/policy
Diverges from boltz-web-app: no

The EIP-7702 gas sponsor URL is read from config and the client sends no `paymasterService` capability — the server-side sponsor applies policy, so the lib holds no Alchemy key/policy and is safe to ship (`alchemy.rs:115-121`). Alchemy payloads are built with untyped `serde_json::Value` (responses are polymorphic and carry decoy fields). Two distinct signing schemes are used per challenge: the EIP-7702 authorization is a RAW 32-byte ECDSA digest, the UserOp is EIP-191 — and the UserOp signs `data.raw` (not the decoy `rawPayload`). A TRUST NOTE documents that returned digests are signed without independent verification (an accepted assumption of the model).

## 2026-04-07 — Server-response hardening + on-chain claim verification
Diverges from boltz-web-app: no

`create()` defends against a malicious/buggy Boltz server: it rejects if `onchain_amount < estimate`, if `lockup_address` != the expected ERC20Swap contract, or if the decoded BOLT11 amount != requested (preventing LN overpayment) (`reverse.rs:468-506`). A duplicate-preimage HTTP 409 is mapped to a typed `DuplicatePreimage` error rather than retried in a loop (`213c208`). `BoltzError` derives `Clone` and preserves the HTTP status through `From<HttpError>` so callers can branch on codes.

## 2026-04-07 — Reveal preimage only after on-chain lockup is confirmed
Diverges from boltz-web-app: yes — the web app reveals the preimage on the `transaction.confirmed` WS event with no pre-claim lockup verification (it uses the lockup primitive only in refund).

Without chain scanning, the client refuses to put the preimage into claim calldata until it has independently confirmed the tBTC is locked in ERC20Swap, retrying the lockup read up to 10x with 1s sleeps to absorb honest RPC lag, and re-checking between claim retries (`reverse.rs:704-743,800-818`). This defeats a forged `transaction.confirmed` message that would otherwise trick the client into publishing the preimage in a reverting tx, letting an attacker settle the LN HTLC and steal the sats. This is the load-bearing security gate of the claim path.

## 2026-04-07 — Claim retry policy: transient retries, terminal returns
Diverges from boltz-web-app: no

`QuoteDegradedBeyondSlippage` returns immediately without retry, leaving the swap in `TbtcLocked` so the consumer can accept the new rate via `accept_degraded_quote`; other errors retry with exponential backoff (raised to 5 attempts in `40edd06`) but abort early if funds are no longer locked on-chain (`reverse.rs:763-832`). The degraded-quote flow is user-gated end to end — the CLI prints the event and requires an explicit `accept`.

## 2026-04-07 — Atomic reserve-next-index storage contract; sole preimage-reuse defense
Diverges from boltz-web-app: yes — the web app's `newKey` is a non-atomic read-then-write with no durability guarantee.

`increment_key_index() -> Result<u32>` reserves and returns the next index as one atomic trait method, and the trait doc elevates persist-before-return to a security invariant because preimages are derived from the index (`store.rs:9-14,74-81`). The facade calls it before `executor.create` and the doc explicitly forbids relying on Boltz's HTTP 409, because a malicious API could lie about duplicates (`lib.rs:261-272`). `checked_add` prevents silent wraparound.

## 2026-04-07 — Recovery via on-chain lockup liveness, reconstructed from persisted data
Diverges from boltz-web-app: yes — the web app uses the lockup primitive only in its refund flow, not as a recovery primitive.

`is_swap_still_locked_by_swap` rebuilds the `hashValues` inputs purely from stored swap fields (preimage hash from the deterministic key index, amount, the tBTC token constant, addresses, timelock) then does the two-call ERC20Swap check (`hashValues(...)` then `swaps(hash)`) — no tx receipt or event log is read (`lockup.rs:48-72,25-42`). This is the sole recovery primitive (no chain scanning), used by both the manager and the executor resume paths.

## 2026-04-07 — Inline per-transport address validation = the OFT encoder
Diverges from boltz-web-app: yes — the web app uses viem/`@solana`/Tron JS validators.

EVM hex, Solana base58, and full Tron base58check+double-SHA256+0x41 validation are hand-rolled inside `encode_oft_recipient`, so the lib needs no JS-package dependency (`recipient.rs:23-116`). `is_valid_destination_address` is just `encode_oft_recipient(...).is_ok()` — one code path both validates input and produces the 32-byte wire value, eliminating validator/encoder drift. EVM comparison is case-insensitive, Solana/Tron case-sensitive.

## 2026-04-07 — Single end-to-end slippage anchored on prepare-time expected output
Diverges from boltz-web-app: yes — the web app anchors the on-chain floor on the LIVE claim-time quote and re-applies slippage per hop, so tolerance compounds.

`compute_claim_floor` returns `expected*(1-s)` as the *sole* on-chain floor; the OFT DEX-leg `amount_out_min` is a loose sentinel (`1`) because slippage is enforced atomically by `minAmountLD`, so a tight DEX min only raises revert frequency without adding protection (`reverse.rs:2358-2369,1419-1426`; `7b74eeb` fixed the double-slippage, `548f5f7` anchored on expected). This guarantees the user receives `>= expected*(1-s)` end-to-end regardless of internal fee buffers and pushes prepare->claim drift onto a loud revert rather than a silently lower floor.

## 2026-04-10 — Pinned Router address; legacy-OFT mesh handled distinctly
Diverges from boltz-web-app: no

The Arbitrum Router address is hardcoded (`v4.0.3` per `faaef47`) because the Boltz API does not expose it; the comment asserts byte-identical EIP-712 domain/typehashes across versions so old deployments stay valid. Legacy USDT0 mesh estimation is handled separately from native (`fece507`, `0a4815b`): on-chain `quoteOFT` does not deduct the legacy 3bps fee, so a binary search would converge too low — the crate uses a closed-form `ceilDiv(dest*10000, 10000-3)` inverse (a near-verbatim port) and may need a standing ERC20 allowance, while native mint/burn OFTs skip approval.

## 2026-04-10 — Tolerate transient RPC errors; two-layer 429 backoff
Diverges from boltz-web-app: yes — the web app relies on viem's default http retry (HTTP-429 only, no JSON-RPC-body-429 case).

A headless background poller needs deterministic bounded backoff it fully controls, surviving both 429 delivery forms hosted providers use — HTTP status 429 and a JSON-RPC error code 429 in a 200 body — so the provider retries on both, up to 5 attempts with `1000ms * 2^n` capped at 30s; non-429 errors return immediately (`provider.rs:158-238`; Alchemy poll tolerance added in `84555b3`).

## 2026-04-13 — Per-transport OFT recipient encoding; non-EVM destinations admitted
Diverges from boltz-web-app: no

The `Chain` enum was reshaped to admit non-EVM destinations (`47a9cc5`) and recipients are encoded per destination transport (`e27d4f5`), adding Solana USDT0 (`2e2c40e`). LayerZero v2 type-3 `extraOptions` encoding is byte-identical to the web app (2-byte `0x0003` header, worker id 1, lzReceive option, two big-endian uint128s), and the crate emits only the two lzReceive directives it needs — Solana ATA-creation (`2_039_280` lamports) and the Polygon gas bump (`30_000`, added `0952e0b` per boltz-web-app#1500) — omitting native-drop. The extraOptions must be byte-identical across every quote and the signed `SendData` or on-chain signature verification reverts.

## 2026-04-15 — Reject destination addresses matching known token contracts
Diverges from boltz-web-app: no

`validate_destination` rejects sending to a known USDT/tBTC/USDT0-mint/dest-token address (normalized per transport: EVM case-insensitive, Solana/Tron exact) because sending tokens to a token contract burns them (`reverse.rs:1835-1882`; `2887454`).

## 2026-04-15 — Per-swap slippage snapshot persisted on the swap record
Diverges from boltz-web-app: yes — the web app keeps slippage as one global localStorage setting re-read live at claim time, never per-swap.

The resolved `slippage_bps` is snapshotted onto the persisted swap at prepare time (`c32c49c`) and all claim-time drift checks and on-chain floors read it off the record (`models.rs:47-50`). Because the headless lib must resume across process restarts, a per-swap override has to live on the record to survive a crash — config alone would lose it. `update_swap_slippage` re-validates against `10..=MAX_SLIPPAGE_BPS`, rejects terminal swaps, and only takes effect on the next claim attempt (`80d358e`, `lib.rs:389-427`).

## 2026-04-15 — OFT destination registry built at runtime from the USDT0 feed
Diverges from boltz-web-app: no (registry build differs in dedup; see next entry)

The DestinationRegistry is fetched once from the USDT0 deployments API at construction and cached for the service lifetime (`5d48b29`, `lib.rs:98-109`); a restart picks up upstream changes (including new EVM chains) with zero code release. Trades runtime freshness for simplicity. The ERC20Swap contract address is resolved at startup by matching `config.chain_id` against the Boltz `/contracts` response, hard-failing on an unknown chain rather than hardcoding it. Assets are labeled `USDT0` vs canonical `USDT` by presence of a distinct `Token` deployment, so a USDT0 balance isn't conflated with Tether (`a99eb29`).

## 2026-04-15 — Native-first dedup-by-chainId when building the OFT registry
Diverges from boltz-web-app: yes — the web app config-selects one mesh per asset and looks up a single chain; it never builds a merged dedup'd map.

A chain can appear in both the native and legacy-mesh feed sections under different names (e.g. "Arbitrum One" vs "Arbitrum"); the crate inserts native entries first, records seen `chainId`s, and skips legacy duplicates by id so the alias doesn't leak as a second destination, deriving each destination's mesh purely from its feed section (`oft.rs:171-196`). A headless registry must enumerate all reachable destinations data-driven without a hand-maintained per-asset config table. The crate also has no analog to the web app's `VITE_USDT0_*_CAN_SEND` kill-switches or `*_OFT_ETA_SECONDS` hints — both are front-end-only concerns.

## 2026-04-16 — Record the delivered amount from receipt logs, not the requested amount
Diverges from boltz-web-app: no (the source differs for CCTP; see later entry)

The delivered OFT amount is read from `OFTSent.amountReceivedLD` (the LayerZero-v2 destination-credited figure) on the source claim receipt, with the guid taken from the indexed `topic[1]`; a real legacy-mesh payload is pinned as a regression test (`a5ebd91`, `contracts.rs:816-848`). Recording what actually arrived rather than what was requested is the basis of honest delivery accounting.

## 2026-04-28 — Probe invoice for LN routing-fee estimation
Diverges from boltz-web-app: yes — net-new; the web app has no probe-based LN fee estimation because the user's external wallet (WebLN/QR) pays the invoice, so routing fees are never the app's concern. Built on the existing reverse-swap create endpoint, so no new protocol surface.

`create_probe_invoice` generates a throwaway hold invoice (random discarded preimage, 60s expiry = the Boltz documented minimum so the unfunded server state self-clears) that must never be paid, deliberately skipping key-index consumption, storage, and WS subscription (`50db307`, `lib.rs:286-311`). Enables fee estimation against a real BOLT11 without committing a swap.

## 2026-05-29 — Configurable gas-sponsor URL replaces hardcoded Alchemy creds
Diverges from boltz-web-app: no

Hardcoded Alchemy credentials (briefly introduced as Boltz-operated defaults in `891c40f`) were replaced with a configurable gas-sponsor URL wrapping Alchemy server-side (`82bd248`), so the shipped lib holds no API key/policy and callers can point at their own sponsor. The Alchemy `call_id` is persisted so claim polling resumes after a crash (`2834982`).

## 2026-05-29 — CCTP v2 bridge ported from the boltz-swaps CCTP module
Diverges from boltz-web-app: no — CCTP is a Rust port of the web app's `packages/boltz-swaps/src/cctp/` (fee, events, attestation, evm, solana, bridge driver), not a net-new feature.

Circle CCTP v2 (USDC, EVM + Solana) was added across `824e2bc`–`2c9d048`: Router ABI/`CctpData` struct hash, `ClaimCctp` EIP-712 signing (type hash pinned to boltz-core v5.0.0, `signing.rs:62-68`), config constants + configurable Iris URL, and a compile-time const CCTP destination table (CCTP routes are not published by the USDT0 feed, so they are hardcoded with Circle domain ids, taken from boltz-swaps `cctp/variants.ts`). The Router was repointed from v4.0.3 to v5.0.0 for CCTP support (`98ce7bf`). The implementation mirrors the web app's `boltz-swaps` CCTP module; what is "ours" is the Rust reimplementation and its dependency-free choices (see the related CCTP-fee and Solana entries), not the bridge itself. (Note: an earlier draft of this entry wrongly called CCTP a net-new addition absent from the reference — it is not.)

## 2026-05-29 — Authoritative CCTP delivered amount from the Iris-attested message
Diverges from boltz-web-app: yes — the web app fetches the destination forward-tx receipt and decodes `MintAndWithdraw.amount` via a per-chain client.

The crate is single-provider (Arbitrum-only) with no per-destination-chain RPC (CCTP spans many EVM chains + Solana), so it derives delivered = `burnAmount - feeExecuted` purely from the Iris-attested message hex at fixed byte offsets (`decode_cctp_delivered_from_message`), which Circle has finalized by attestation time (`contracts.rs:212-220,750-767`). The source `MessageSent` log gives only an estimate (`feeExecuted`=0); `MintAndWithdraw` is declared but never decoded. The amount is recorded only once both forwarded AND attested (`707a081`, `8c75ae3`). The CCTP `CctpData` typehash is also fetched on-chain from the Router (the web app hardcodes it), so a Router redeploy can't silently produce a non-verifying signature.

## 2026-05-29 — Solana access reimplemented from primitives; no Solana SDK
Diverges from boltz-web-app: yes — the web app delegates all PDA/ATA derivation and RPC to `@solana/web3.js` + `@solana/spl-token`.

ATA/PDA derivation is reimplemented on `sha2` + a minimal `curve25519-dalek` (off-curve check via `CompressedEdwardsY` decompression, program IDs as raw byte constants), keeping the dependency surface tiny and identical across native/WASM, anchored by a real-mainnet ATA vector test (`solana/ata.rs:42-83`). The RPC client is a single hand-written `account_exists` (one `getAccountInfo`) — the lib has no UI, balance, rent-math, or sender-tracking needs. Existence caching lives in the caller and is asymmetric: only positive ("exists") hits are memoized, because the user may create the ATA between calls (`reverse.rs:1898-1919`). The CCTP recipient path deliberately permits an off-curve owner (matching the web app's `allowOwnerOffCurve=true`).

## 2026-06-01 — Unified DestinationRegistry over one opaque `DestinationId` join-key
Diverges from boltz-web-app: yes — the web app keys by asset symbol and dispatches bridge behavior via drivers; it has no Destination/DestinationId/registry join-key space.

Routing was unified into one `Destination` struct (asset-on-chain + a `Bridge` enum: Direct/Oft/Cctp) in a single registry keyed by an opaque, lowercased, serde-transparent `DestinationId` (`a33179d`, `models.rs:234-256`). A single join-key lets callers select any asset/chain/bridge combination uniformly and avoids id collisions when one chain offers multiple bridges (USDT0 via OFT and USDC via CCTP), while the public API exposes only the coarse three-variant `BridgeKind`. The internal `Bridge` carries mesh/lz_eid/domain routing detail; `Bridge::kind()` bridges to the public coarse category. USDC-on-Arbitrum (Direct) was added in the same change.

## 2026-06-01 — Bridged swaps complete only after confirmed delivery
Diverges from boltz-web-app: yes — the web app never confirms destination delivery; it polls only the source tx and runs down a configured ETA timer.

Only confirmed delivery advances `Settling`->`Completed`: CCTP via Circle Iris (forwarded AND attested), OFT via LayerZero Scan `DELIVERED` status — every other LZ status, including terminal FAILED/BLOCKED, leaves the swap in Settling for indefinite re-poll (`lz_scan.rs:44-61`, `manager.rs:725-749`; `3bb104b`, `31d2811`). The lib is a persistent state machine whose recorded status drives funds accounting, so a stuck-but-honest Settling is strictly preferred over a false Completed (stated verbatim in `lz_scan.rs:54-60`). `post_claim_status` even fails safe to Settling if a bridged claim somehow yields no `bridge_ref`. Delivery polling is opt-out (`Option<u64>`, gentle 30s default) since funds are already committed at claim time.

## 2026-06-02 — Pin Arbitrum source OFT contracts against the USDT0 feed
Diverges from boltz-web-app: yes — the web app (via `boltz-swaps`) fetches the source OFT contract address from the same USDT0 deployments feed and uses it as-is, with no compile-time pin; this verify-against-pins theft guard is net-new. (The feed-driven *sourcing* itself matches — see 2026-04-15/2026-04-16.)

The source-chain OFT addresses returned by the USDT0 deployments feed are verified against compile-time pins before use (`verify_pinned_source_oft`, `oft.rs`; `7776d0d`), so a compromised or wrong feed cannot redirect a send through an attacker-controlled OFT contract. The registry is otherwise built data-driven from the feed (see the 2026-04-15/2026-04-16 registry entries); this pin is the theft guard on the one address that moves funds on the source chain.

## 2026-06-02 — Explicit `User-Agent` on every outbound HTTP request
Diverges from boltz-web-app: yes — the browser web app sends no User-Agent; `fetch()` populates it automatically.

The `platform-utils` HttpClient (native bitreq and WASM reqwest) takes an optional User-Agent at construction and stamps it on every request; all production constructions pass `boltz-client/<version>` (`lib.rs:77`; `c6813ab`). A headless native/WASM client gets no browser-supplied UA, and some upstreams reject header-less requests with 403, so the library sets one explicitly. The web app needs no handling because the browser populates the header.

## 2026-06-03 — A terminal WS event never finalizes an already-claimed swap
Diverges from boltz-web-app: yes — the web app polls only the source tx and runs an ETA timer; it has no `Settling`/`Claiming` state machine and no notion of "don't finalize on the WS event alone" for terminal statuses.

The `Settling` short-circuit (2026-06-01) protected an already-claimed *bridged* swap from a late `*.expired`, but only once it reached `Settling`. There is a strictly-earlier window: after `do_claim` records progress the swap sits in `Claiming` until the receipt poll promotes it, and in that window the atomic claim may have already revealed the preimage and committed the bridge-send on-chain. A late or spoofed `invoice.expired`/`swap.expired`/refund/`failedToPay` event arriving then would have finalized the swap `Expired`/`Failed` on the WS event alone — stranding a successful bridged swap (delivery never confirmed, `delivered_amount` never recorded, dropped from tracking). So terminal WS events for a `Claiming` swap now re-check the on-chain `ERC20Swap` lock first (`handle_terminal_ws_event`): finalize only if it is provably still locked (the claim never happened); if it is spent, advance through the post-claim path (`advance_claimed_swap` → `post_claim_status`) instead. This is the same "never finalize on a WS event alone" rule the `invoice.settled` path already followed, applied to the terminal events too. Correspondingly, `check_on_chain_and_retry`'s already-claimed branch now advances the swap rather than silently waiting, so a crash-resume whose gas-sponsor `call_id` became unresolvable can no longer strand the swap in `Claiming`.

## 2026-06-03 — Destinations identified by `(chain_label, asset)`, not an opaque `DestinationId`
Supersedes the 2026-06-01 `DestinationId` join-key. Diverges from boltz-web-app: still yes — the web app has no registry/join-key space.

The opaque, lowercased, serde-transparent `DestinationId` newtype was dropped in favour of identifying every destination by its natural `(chain_label, asset)` pair; the registry is now a `Vec<Destination>` looked up with `DestinationRegistry::find(chain, asset)` (case-insensitive chain, exact asset) instead of a `HashMap<DestinationId, _>`. `DestinationId` carried two id spaces kept apart only by convention — OFT chain names (`"polygon pos"`) and synthetic CCTP ids (`"usdc-base"`) — but a destination genuinely *is* an asset-on-chain, so the composite key is the real identity and removes the synthetic-id layer (the `id` field on `Destination`/`DestinationOption`/`CctpDestination`, the `cctp_destination()` lookup, and the newtype itself all go away). Uniqueness holds because OFT only ever yields USDT/USDT0 and CCTP only USDC, so the two bridge spaces never collide on `(chain, asset)`; the source chain still hosts a distinct `(Arbitrum One, USDT)` Direct and `(Arbitrum One, USDC)` Direct. `BoltzSwap` now persists `destination_chain` (label) + a new `asset` field. Persistence back-compat was deliberately dropped in the same change: since the swap record's identity is changing anyway, the now-stale `bridge_kind` `#[serde(default)]` and the `expected_usdt_amount`/`lz_guid` rename aliases were removed too, so the on-disk format tracks exactly one version (pre-1.0, no swaps in the wild — old records no longer deserialize).


## 2026-06-08 — Reject too-short lockup timeouts against the L1 block height
Diverges from boltz-web-app: yes — the web app never sanity-checks an EVM reverse swap's `timeoutBlockHeight` (its `validateReverse` early-returns via `validateContract` for EVM assets; the timeout is only validated for UTXO chains where it is baked into the Taproot tree).

`create()` previously hardened `onchain_amount`, `lockup_address`, and the decoded BOLT11 amount but trusted `timeout_block_height` verbatim (an acknowledged `TODO`). A malicious/buggy Boltz (in-scope per the threat model) could return a timeout at or just above the current block: the pre-reveal lockup gate proves only that the tBTC is locked *now*, not that the lock survives until our claim mines. With a near-expired timeout the server can decline to include the sponsor-submitted claim (it receives the preimage in the UserOp before inclusion), let the timeout lapse, refund the tBTC, and settle the LN hold invoice with the leaked preimage — keeping both the sats and the tBTC. The fix requires a minimum headroom (`MIN_TIMEOUT_L1_MARGIN`, 60 blocks ≈ 12 min) between the timeout and the current block, checked in two places: a UX early-abort in `create()` (before the invoice is returned, so the user never commits sats) and the load-bearing, fail-safe gate in `claim_and_swap` immediately before the preimage is revealed (aborting there is loss-free — the preimage stays secret and the LN payment refunds on timeout). The guard **fails closed**: an RPC error rejecting the height read rejects the swap rather than proceeding blind.

The denomination is the subtle part, confirmed empirically against mainnet (Boltz `BTC→TBTC`): `timeout_block_height` is denominated in **L1** (Ethereum) block height, because Solidity `block.number` on Arbitrum returns the L1 number, and that is what the `ERC20Swap` timelock is compared against. The standard `eth_blockNumber` RPC returns the **L2** Arbitrum number (an order of magnitude larger), so a new `EvmProvider::eth_l1_block_number()` reads the Arbitrum-specific `l1BlockNumber` field from the latest block header; comparing against the L2 number would reject every swap. Boltz's honest timeout is ~7200 L1 blocks (~24h), so the 60-block floor never rejects a legitimate swap.

## 2026-06-09 — Per-swap serialization + cross-swap parallelism (supersedes the global "race-free" loop)
Diverges from boltz-web-app: yes — the web app is a single-user browser session that progresses one swap per tab against `localStorage`; it has no shared background writer and no multi-swap concurrency model at all.

The original `SwapManager` ran *every* reaction — claim, receipt poll, on-chain check, delivery confirmation — inline in one `tokio::select!` loop and documented the design as "simple and race-free". The race-freedom was real but only because nothing ran concurrently; it relied on the loop being the **sole writer**. That premise was already false: three public `BoltzService` methods (`accept_degraded_quote`, `update_swap_slippage`, `refresh_pending_deliveries`) run on the *caller's* task and write the store directly, off the loop. Because `update_swap` is a whole-record last-write-wins overwrite with no read-modify-write or version check (see the 2026-04-06 caller-owned-persistence entry), a caller's `get → mutate → put` could clobber a field the loop wrote in between (status regression), and `accept_degraded_quote` could run `claim_and_swap` for a swap the loop was *already* claiming — two gas-sponsored claim txs for one swap, the loser reverting and possibly leaving the persisted `claim_tx_hash` pointing at the reverted tx (stuck `Claiming`). The inline model also serialized all swaps behind each other: one swap's ~5-minute receipt poll blocked every other swap's WS update, command, and delivery tick — fine for a low-volume client, a hard ceiling for a high-throughput server integration, which is an explicitly in-scope future use.

The invariant we actually need is not "one global writer" but **serialize work *per swap*; run *different* swaps in parallel** — two swaps touch disjoint records and never need ordering; only concurrent work on the *same* swap must be serialized. So the loop becomes a lightweight **dispatcher**: it owns the WS-subscription/tracking set and the delivery ticker, and it *spawns* each WS handler and delivery poll into a `JoinSet` rather than running it inline. Every task that mutates a swap — the spawned handlers, the delivery poll, and the three caller methods — first acquires that swap's entry in a keyed async mutex (`SwapLocks`, `swap/locks.rs`) and holds it across the whole `get → mutate → persist` sequence, re-reading under the lock. Same swap → serialized (kills the double-claim and the lost-update); different swaps → fully parallel (a slow receipt poll no longer blocks unrelated swaps). The lock is the load-bearing seam: serialization is enforced in-process so it holds for *any* `BoltzStorage` impl (a DB embedder's row transactions then compose as belt-and-suspenders, never a prerequisite) — we deliberately did **not** push optimistic-concurrency/CAS onto the caller-provided trait. `SwapLocks` creates entries on demand and prunes each once its last holder drops, so the map stays bounded by swaps with in-flight work, not swaps ever seen — safe for a long-running server. Chosen over a full per-swap actor/mailbox model (one task owning each swap, structurally impossible to double-write): the actor model is the cleaner end state but a much larger rewrite, and the keyed lock delivers the same two invariants now while leaving the actor refactor as a future evolution rather than a prerequisite. Out-of-order same-swap events (two WS updates for one swap racing into the JoinSet) are safe because the handlers were already idempotent and status-aware — the monotonicity guard and the terminal/`Settling` short-circuits, built for `resume_all` re-delivery — re-validate under the lock. Graceful `shutdown()` drains in-flight tasks (matching the old inline behaviour, where the select was anyway parked on a running handler); `Drop` aborts as the backstop. No concurrency cap is imposed yet (one task per in-flight swap); a semaphore can bound fan-out later if a server workload needs it.
