# CHARTER — PENDING RECEIPT (provenance record)

The BaseVantage PROJECT CHARTER was referenced as attached to the batch
instruction of 2026-10-05, but the document never materialized: no attachment
payload in the thread, empty Capy Drive scopes, empty repository. A second
"re-attached" reference likewise arrived without content.

This file records, verbatim, the operative written instructions received so far.
When the charter document arrives, it is the master source of truth and this
file should be replaced by it.

## Verbatim — batch S1 instruction

> PROJECT: BaseVantage — professional Base-chain Telegram trading aggregator
> (Rust). The attached CHARTER is the single source of truth; you are a fresh
> engineer with NO prior context. Read it fully first. If code reality conflicts
> with the charter, CODE WINS and you flag it.
>
> THIS INSTRUCTION COVERS BATCH S1 ONLY (engine core, NO Telegram):
>   L1 chain-adapter trait (Base impl: RPC pool health/benching, gas,
>      nonce, WS event source)
>   L2 market-data (factory-registry discovery, WS-driven pool-state,
>      tiered cache [static 24h / reserves 30s+event / stats 60s /
>      negative 120s], single-flight, source+age labels)
>   L3 router (single-hop + multi-hop Route.hops; best route by NET
>      settlement-asset out; quote pinned at confirm, re-validated at send)
>   L4 safety (token assess, floor module [TARGET anchor + SWAP anchor],
>      impact cap pre-send refuse, tax/honeypot block, fee-on-transfer
>      refused on multi-hop v3)
>   L7 watchlist (enrol; dedupe address-then-symbol with collision refuse;
>      provenance auto/manual; cap 50 with refusal card; manual-only
>      removal; bot never auto-removes)
>   + dev-harness CLI (quote / dossier-data / simulate / route-list) so the
>     engine is exercisable WITHOUT Telegram.
>
> STRICT PROCESS:
>  1. FIRST produce a short DESIGN DOC: module layout, trait boundaries,
>     the exact S1 test list, sample CLI outputs. STOP and wait for my
>     written approval. NO feature code before approval.
>  2. After approval: implement S1 ONLY; tests green;
>     clippy --all-targets -D warnings; fmt clean; then a report (files,
>     tests+counts, charter conflicts flagged). End with
>     "S1 complete — awaiting approval for S2." Do NOT start S2.
>
> S1 ACCEPTANCE (must appear in the test list):
>  - VPS fork tests (BASE_RPC_URL) for v2/v3/v4/aerodrome quoters +
>    multi-hop calldata (single-hop encoding byte-identical pin)
>  - best route = max NET USDC out; mixed quote assets never compared raw
>  - multi-hop sell settles USDC with wrapped-native residue 0 (sim)
>  - floor: TARGET anchor + SWAP anchor; worse-fill reverts
>  - impact cap refuses pre-send; tax/honeypot blocked
>  - repeat quote ≈ 0 RPC (cache + single-flight) test
>  - watchlist enrol/dedupe/collision/cap/manual-remove tests
>  - config schema validates at boot (fail-fast); effective mode observe
>
> Branding: product/display name "BaseVantage"; crate/repo "basevantage".
> Never claim "perfect"; claim "invariants tested."

## Verbatim — approval + correction message

> CHARTER RE-ATTACHED (pasted above) = MASTER source of truth. Your
> docs/S1-DESIGN.md is APPROVED as the S1 implementation spec WHERE
> CONSISTENT; where they differ, the charter wins.
>
> REQUIRED CORRECTION (apply before any feature code):
>  - Rename your "TARGET anchor" (currently = deepest canonical pool
>    reference) to "REFERENCE anchor" (a sanity floor).
>  - Reserve "TARGET anchor" for the USER'S target price (introduced in
>    S2). For any target order, min-out must NEVER be below
>    target × amount. min-out = max(applicable anchors).
>  - Add test: target_order_floor_never_below_target (unit-level in S1
>    with a supplied target value), alongside the existing swap-anchor
>    and worse-fill-revert tests.
>
> NON-BLOCKING NOTES:
>  - Persist the 24h static cache tier to disk (SQLite/file) so restarts
>    don't re-fetch.
>  - Venue-coverage check deferred to a later batch; note it in the doc.
>  - Single crate acceptable for now.
>
> PROCESS: create the INITIAL GIT COMMIT (charter + docs/S1-DESIGN.md +
> scaffold) BEFORE feature code, per the charter (commit 0 = provenance).
>
> Everything else in the design doc is approved. Apply the correction,
> then implement S1 only; end with tests green + clippy/fmt clean + the
> S1 report, finishing with "S1 complete — awaiting approval for S2."
