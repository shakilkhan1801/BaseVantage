# BaseVantage — S1 Design Doc (engine core, no Telegram)

Status: APPROVED as the S1 implementation spec where consistent with the charter;
where they differ, the charter wins. Corrections of 2026-10-05 applied.

> Charter provenance note: the CHARTER document was referenced as attached but
> never materialized (no attachment payload, empty drives, empty repo). See
> `docs/CHARTER-PENDING.md`. This spec is the ratified working source of truth
> plus the written corrections in the approval message.

## 1. Module layout

Single crate `basevantage` (lib + one bin). No workspace: S1 is one engine, one
binary; boundaries are enforced by module privacy and traits, not crates.

```
basevantage/
  Cargo.toml                 # edition 2024, rust 1.97
  config.example.toml        # full schema, every key documented
  fixtures/calldata/         # golden single-hop encodings (hex pins)
  src/
    lib.rs
    config.rs                # L-config: typed schema + validate(), fail-fast
    error.rs                 # thiserror taxonomy per layer
    chain/                   # L1
      mod.rs                 # ChainAdapter trait, RpcPool, health + bench
      base.rs                # BaseChain: gas, nonce, call, send, fork helpers
      ws.rs                  # WsEventSource: pool events over WS, RPC fallback
    market/                  # L2
      registry.rs            # factory-registry discovery (v2/v3/v4/aerodrome)
      pool_state.rs          # WS-driven pool state (reserves, slots, ticks)
      cache.rs               # tiered TTL cache, single-flight, Source+Age labels
      mod.rs                 # MarketData facade used by router/safety
    venues/                  # quoters + calldata encoders, one per AMM family
      mod.rs                 # VenueQuoter trait
      v2.rs v3.rs v4.rs aerodrome.rs
    router/                  # L3
      route.rs               # Route { hops }, Hop, candidate generation
      quote.rs               # gross→net accounting, pin at confirm, revalidate at send
      mod.rs                 # best_route: max NET settlement-asset out
    safety/                  # L4
      assess.rs              # token assess: tax probe, honeypot probe
      floor.rs               # floor module: REFERENCE anchor + SWAP anchor (+ TARGET)
      impact.rs              # impact cap, pre-send refuse
      mod.rs                 # verdicts: Allow | Refuse(reason) | Block(reason)
    watchlist/               # L7
      mod.rs                 # enrol/dedupe/cap/remove, provenance, persistence
    harness/                 # command implementations shared by the CLI
      mod.rs
  src/bin/bv.rs              # dev-harness CLI (quote, dossier-data, simulate, route-list)
  tests/
    config_schema.rs  router_net_out.rs  sim_settlement.rs
    safety_floor.rs   safety_block.rs    market_cache.rs    watchlist.rs
    fork/             # BASE_RPC_URL-gated, #[ignore] locally
      quoters.rs      calldata_pin.rs
```

Dependencies: `alloy` (RPC/WS/ABI), `tokio`, `serde`, `toml`, `clap`, `thiserror`,
`tracing`, `async-trait`, `futures`, `sha3`, `hex`. No ORM: watchlist persistence
is one JSON file behind a trait.

## 2. Trait boundaries

**L1 `ChainAdapter`** (dyn-compatible via `async-trait`):

```rust
trait ChainAdapter: Send + Sync {
    fn bench(&self) -> BenchReport;                 // latency/score every endpoint
    fn health(&self) -> RpcHealth;                  // scored pool state
    async fn gas_price(&self) -> Result<U256>;
    async fn nonce(&self, acct: Address) -> Result<u64>;
    async fn call(&self, req: CallRequest) -> Result<Bytes>;
    async fn estimate_gas(&self, req: CallRequest) -> Result<u64>;
    fn events(&self, f: EventFilter) -> BoxStream<'_, PoolEvent>; // WS-first
}
```

`RpcPool` round-robins scored endpoints; benching runs at boot and on failure.
The test double `CountingAdapter` wraps any adapter and tallies calls — this is
what the ≈0-RPC cache test asserts against.

**L2 `MarketData`** — the only read surface router/safety use. Every value leaves
the cache tagged `(Source, Age)`: `Source ∈ {Registry, ChainRpc, WsEvent, External}`,
printed by the CLI. TTL tiers: static pool metadata 24h (persisted to disk so
restarts don't re-fetch — file-backed snapshot of the tier), reserves 30s + event
invalidation (WS Sync/Swap refreshes and marks fresh), stats 60s, negative results
120s (missing pool / no-tax result won't hammer the RPC). Single-flight: concurrent
misses on one key share one in-flight request.

**L3 venue boundary** — `VenueQuoter { fn quote(&self, hop, amount_in) -> Quote;
fn encode(&self, plan) -> Bytes; }` with per-family impls (v2 constant-product,
v3 concentrated, v4 hook-aware single PoolManager, aerodrome stable/volatile).
Encoding lives beside quoting so the pinned goldens cover the exact bytes we send.
`Route` is `Vec<Hop>`, one hop = one pool crossing; single-hop is just len 1.
`Quote` carries gross out, net out in the settlement asset, gas estimate, impact,
and the pin timestamp. Pinned at confirm; `revalidate()` re-quotes at send and
refuses if net-out regressed past the floor or the pin expired.

**L3 router rule:** candidate quotes in mixed assets (WETH-quoted, USDC-quoted) are
normalized into the settlement asset (USDC for sells) before comparison — raw
amounts across assets are never compared; `best_route` picks max NET out.

**L4 safety** returns verdicts consumed at two gates: assess (tax/honeypot block
before any route is offered) and pre-send (impact cap refuse, floor check).

Floor module (anchor naming per correction of 2026-10-05):

- **REFERENCE anchor** — sanity floor derived from the target token's deepest
  canonical pool (the former "TARGET anchor", renamed).
- **SWAP anchor** — floor implied by the price of the route we actually cross.
- **TARGET anchor** — the user's target price. Reserved for S2 target orders; the
  S1 module accepts a supplied target value and enforces the invariant now.

  `min-out = max(applicable anchors)`, applicable = REFERENCE and SWAP always, and
`TARGET × amount` whenever a target is set. Invariant: for any target order,
min-out is **never below `target × amount`**. min-out is enforced via the swap
calldata and re-checked in sim; a worse fill reverts.

Fee-on-transfer tokens are refused on multi-hop v3 hops (exact-in accounting
would lie).

**L7 `Watchlist`:** `enrol(token, provenance)`, `remove(id)` manual-only, `list()`.
Dedupe order: address match → identical entry refused; symbol match with different
address → collision refused with a card naming both addresses. Cap 50, over-cap
enrol returns a refusal card. Provenance `Auto | Manual` recorded per entry; the
bot never auto-removes — sweep/refresh passes may only annotate, never delete.

**Config:** one TOML file deserialized into a typed `Config`, then `validate()`
runs semantic checks (URLs parse, TTLs > 0, cap > 0, impact ≤ 100, addresses
checksummed) and exits non-zero listing every violation before any network I/O.
Boot banner and `bv` output state the effective mode (`observe` = quote/sim only,
never sends; `execute` gated to S2+).

## 3. Exact S1 test list

Fork (VPS, `BASE_RPC_URL`, `#[ignore]` without it):
1. `fork::v2_quoter_matches_chain`
2. `fork::v3_quoter_matches_chain`
3. `fork::v4_quoter_matches_chain`
4. `fork::aerodrome_quoter_matches_chain`
5. `fork::multihop_calldata_executes`
6. `fork::single_hop_encoding_byte_identical_pin` — encoder output byte-equals committed golden hex for all four venues

Router:
7. `router::best_route_max_net_usdc_out` — higher-gross-but-worse-net loses
8. `router::mixed_quote_assets_normalized_never_raw_compared` — WETH-quoted vs USDC-quoted candidates, misleading raw amounts fixture

Settlement:
9. `sim::multihop_sell_settles_usdc_and_wrapped_native_residue_zero`

Safety:
10. `safety::floor_reference_anchor_worse_fill_reverts`
11. `safety::floor_swap_anchor_worse_fill_reverts`
12. `safety::target_order_floor_never_below_target` — unit-level, supplied target value
13. `safety::impact_cap_refuses_pre_send`
14. `safety::tax_token_blocked`
15. `safety::honeypot_blocked`
16. `safety::fee_on_transfer_rejected_multihop_v3`

Market:
17. `market::repeat_quote_near_zero_rpc_cache_and_single_flight` — CountingAdapter: 50 sequential + 50 concurrent repeat quotes ⇒ ≤ 1 reserves RPC

Watchlist:
18. `watchlist::enrol_persists_with_provenance`
19. `watchlist::dedupe_by_address`
20. `watchlist::symbol_collision_refused`
21. `watchlist::cap_50_refusal_card`
22. `watchlist::manual_remove_only_bot_never_auto_removes`

Config:
23. `config::valid_schema_loads_at_boot`
24. `config::invalid_schema_fails_fast` (per-field violation cases)
25. `config::effective_mode_observed`

## 4. Sample CLI outputs

```
$ bv quote --sell 0x833589fC... --with 1.5 --settle USDC
route     v3 WETH/USDC 5bps → v2 TOKEN/WETH
gross     2,412,880.44 TOKEN
net       2,397,310.02 TOKEN   (tax 0.5% sell, gas 0.00042 ETH)
impact    0.31%
floor     REFERENCE 2,390,000 · SWAP 2,394,100 → min-out 2,394,100
safety    allow
source    chain-rpc age 3.2s · pool meta registry age 4h12m · stats age 21s
mode      observe (no transactions sent)
```

```
$ bv route-list --sell 0x833589fC... --with 1.5 --settle USDC
#  net(USDC out)   route                                  verdict
1  2,397,310.02    v3 5bps → v2 TOKEN/WETH               best
2  2,391,004.77    v2 TOKEN/WETH                         ok
3  2,402,881.00    aerodrome vol → v2 TOKEN/WETH         refused: fee-on-transfer multi-hop v3 leg
4  —               v4 TOKEN/WETH 30bps                   refused: impact 2.4% > cap 1.5%
```

```
$ bv dossier-data 0x833589fC...
token      TOKEN (TOKEN)  decimals 18
tax        buy 0.0% · sell 0.5% · honeypot no (sim probe block 19,224,331)
pools      v2 TOKEN/WETH res 1.2M/311.4  ·  v3 5bps TVL 842K
stats      24h vol 1.9M · txns 4,113 · holders 8,204
source     chain-rpc age 3.2s · stats external age 58s · negative hits 120s TTL
```

```
$ bv simulate --route 3 --sell 0x833589fC... --with 1.5
settle     USDC  2,397,310.02
residue    WETH 0.000000000000000000  ✓
floor      REFERENCE ok · SWAP ok (fill 2,397,310 > min-out 2,394,100)
impact     0.31% ≤ cap 1.5%  ✓
verdict    allow (sim only — mode observe)
```

## 5. Deferred / non-blocking notes

- Static 24h cache tier is persisted to disk so restarts don't re-fetch (in S1).
- Venue-coverage check is deferred to a later batch.

## 6. Delivery gates

`cargo test` green (fork suite with `BASE_RPC_URL`), `cargo clippy
--all-targets -- -D warnings` clean, `cargo fmt --check` clean, then the S1 report:
files, tests + counts, charter conflicts flagged. Ends with "S1 complete — awaiting
approval for S2."
