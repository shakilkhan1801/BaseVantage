# BaseVantage — S1 Report (engine core, no Telegram)

Date: 2026-10-06. Scope: batch S1 exactly as instructed — L1 chain adapter, L2
market data, L3 router, L4 safety, L7 watchlist, and the `bv` dev-harness CLI.
No Telegram, no live execution (`observe` mode only), as required.

## Delivery gates

| Gate | Status |
| --- | --- |
| `cargo test` | green — 27 local tests pass, 0 fail |
| Fork suite (`BASE_RPC_URL`, `#[ignore]` locally) | green — 6/6 against live Base mainnet |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |

Fork suite command used (public endpoints; use your own key on the VPS):

```
BASE_RPC_URL=<rpc[,rpc…]> cargo test --test fork -- --ignored --test-threads=1
```

`BASE_RPC_URL` may list several endpoints; the pool round-robins and backs off
on 429. The execution test additionally spawns `anvil` (foundry) and
auto-selects only backends that serve historical storage (public endpoints
that refuse archive reads are filtered out).

## Files delivered

One crate (`basevantage`, edition 2024), 8,509 lines of Rust across 34 source
and test files, plus 6 golden calldata pins in `fixtures/calldata/`.

| Module | Contents |
| --- | --- |
| `src/config.rs` | typed TOML schema + `validate()` fail-fast, effective mode (`observe`) |
| `src/error.rs` | `thiserror` taxonomy per layer |
| `src/chain/` | `ChainAdapter` trait, `RpcPool` (scored endpoints, bench/health, 429 backoff+retry), `BaseChain`, `WsEventSource` (WS-first, HTTP polling fallback), test doubles |
| `src/market/` | factory discovery (v2/v3/v4/aerodrome), WS-driven pool state, tiered TTL cache (static 24h persisted to disk / reserves 30s+event / stats 60s / negative 120s) with single-flight and `(Source, Age)` labels, `sol!` ABI declarations |
| `src/venues/` | quoters + byte-exact encoders: v2 constant-product, v3 concentrated (core TickMath/SwapMath port, one-word stepping), v4 (PoolManager `extsload` state, per-direction protocol-fee-inclusive swap fee), aerodrome stable/volatile |
| `src/router/` | `Route`/`Hop` candidates, gross→net accounting, `best_route` = max NET settlement-asset out, quote pin at confirm + `revalidate()` at send |
| `src/safety/` | token assess (tax + honeypot probes via `eth_call` state overrides), floor module (REFERENCE anchor + SWAP anchor + user TARGET anchor), impact cap pre-send refuse, fee-on-transfer refusal on multi-hop v3 |
| `src/watchlist/` | enrol/dedupe/collision/cap-50/provenance, manual-only removal, JSON persistence |
| `src/harness/` + `src/bin/bv.rs` | dev-harness CLI: `quote`, `route-list`, `dossier-data`, `simulate` |
| `tests/` | 33 tests: the full S1 acceptance list plus encoding cross-checks and the RPC-retry test |

## Tests + counts (33 total)

Fork (6, `#[ignore]`d without `BASE_RPC_URL`; all pass against Base mainnet):
`v2_quoter_matches_chain`, `v3_quoter_matches_chain`, `v4_quoter_matches_chain`,
`aerodrome_quoter_matches_chain`, `multihop_calldata_executes` (real v2/v3
multi-hop swaps and a v4 Universal Router swap executed on anvil),
`single_hop_encoding_byte_identical_pin`.

Router (4): `best_route_max_net_usdc_out`,
`mixed_quote_assets_normalized_never_raw_compared`,
`pin_revalidate_refuses_stale_or_regressed`, plus the encoding cross-checks
below. Settlement (1): `multihop_sell_settles_usdc_and_wrapped_native_residue_zero`.
Safety (7): `floor_reference_anchor_worse_fill_reverts`,
`floor_swap_anchor_worse_fill_reverts`, `target_order_floor_never_below_target`,
`impact_cap_refuses_pre_send`, `tax_token_blocked`, `honeypot_blocked`,
`fee_on_transfer_rejected_multihop_v3`. Market (1):
`repeat_quote_near_zero_rpc_cache_and_single_flight` (50 sequential + 50
concurrent repeat quotes ⇒ ≤ 1 reserves RPC). Watchlist (5): enrol/provenance,
`dedupe_by_address`, `symbol_collision_refused`, `cap_50_refusal_card`,
`manual_remove_only_bot_never_auto_removes`. Config (3): `valid_schema_loads_at_boot`,
`invalid_schema_fails_fast`, `effective_mode_observed`. Encoding (6): every
hand-rolled encoder is byte-identical to an independent `sol!` encoding and to
the committed golden pins. Chain (1): `rate_limited_call_retries_with_backoff`
(429 → backoff → retry, throttled endpoint stays healthy).

## Charter conflicts and flags

1. **The PROJECT CHARTER never arrived** (referenced twice as attached; checked
   in-thread, in every Capy Drive scope, and in the repo). This S1 was built
   under the batch instruction plus the approved `docs/S1-DESIGN.md` and the
   written corrections (REFERENCE anchor rename, target-order floor rule,
   static-cache persistence, deferred venue coverage). A charter-vs-code
   consistency check could not be performed; when the charter lands, that
   check is still owed.
2. Design-sketch refinements applied and reflected in `docs/S1-DESIGN.md`:
   `ChainAdapter::bench` is `async` (the sync sketch could not do RPC);
   `src/market/abi.rs` was added to hold `sol!` declarations; the fork tests
   live in a single `tests/fork.rs` to preserve the exact `fork::*` test names.
3. Golden calldata pins are generated by the test suite itself after passing an
   independent `sol!` cross-check, and are additionally validated by executing
   the calldata against the live routers on anvil — the pins cover the exact
   bytes that ship.
4. Two behaviours were verified directly against live deployments rather than
   assumed: the Universal Router's v4 action codes and its single-value
   (`abi.decode(elem, (Struct))`) param encoding, and v4's combined
   protocol+LP swap fee (`protocolFee + lpFee − protocolFee·lpFee/1e6`).
5. Non-blocking note from the corrections: the 24h static cache tier persists
   to disk (JSON snapshot) so restarts don't re-fetch — implemented and
   exercised. Venue-coverage check remains deferred to a later batch by
   instruction.

## Claim

Not "perfect" — **invariants tested**: every acceptance invariant has a named
test, the fork suite proves the quoters equal the chain's own answers exactly,
and the calldata that ships is byte-pinned and executed live.

S1 complete — awaiting approval for S2.
