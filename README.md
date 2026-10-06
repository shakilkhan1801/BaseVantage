# BaseVantage

Base-chain trading engine core (batch S1: chain adapter, market data, router,
safety, watchlist, dev-harness CLI — no Telegram).

Build and check:

```
cargo test                                  # unit + integration
BASE_RPC_URL=... cargo test -- --ignored    # fork suite
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Dev harness: `cargo run --bin bv -- quote|route-list|dossier-data|simulate`.

Design: `docs/S1-DESIGN.md`. Charter provenance: `docs/CHARTER-PENDING.md`.
