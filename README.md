# boltz-client

Boltz Exchange client for cross-chain swaps.

Swaps between Lightning (sats) and USDT via [Boltz Exchange](https://boltz.exchange). Uses a two-hop architecture:

```
Lightning <-> tBTC (Boltz reverse swap) <-> USDT (DEX swap on Arbitrum)
```

A Router contract makes claim + DEX atomic — one EVM transaction claims tBTC from the ERC20Swap contract and executes the swap to USDT. Cross-chain delivery to other EVM chains uses OFT bridging (LayerZero).

## Key Design Choices

| Decision | Choice |
|----------|--------|
| EVM keys | SDK-managed, derived from seed via BIP-32 |
| Gas | Alchemy EIP-7702 — users don't need ETH |
| ABI + signing | alloy-sol-types + k256 (WASM-compatible) |
| Swap status | WebSocket (real-time updates) |

## Building

```bash
make build          # Build workspace
make check          # Run all checks (fmt, clippy, tests)
```

## Testing

```bash
make test           # Unit tests
make itest          # Integration tests (requires Docker)
```

## External References

- [Boltz API docs](https://api.docs.boltz.exchange)
- [Boltz web app source](https://github.com/BoltzExchange/boltz-web-app)
- [Boltz regtest environment](https://github.com/BoltzExchange/regtest)
- [boltz-core contracts](https://github.com/BoltzExchange/boltz-core)

## License

[MIT](LICENSE)
