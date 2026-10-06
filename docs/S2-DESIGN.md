# BaseVantage — S2 Design Doc (Telegram bot over the S1 engine)

Status: **awaiting written approval. No S2 feature code before approval.**
Date: 2026-10-06. Builds on the completed S1 (`docs/S1-REPORT.md`).

## 0. Scope discipline (the point of this doc)

The reference product (@SolanaTradingBot-class bots) is the **organization
standard**, not the feature checklist. S2 ships the smallest bot that is a
complete, professional daily-use trading tool over the S1 engine. Everything
else waits for later batches.

**S2 delivers:** the Telegram surface (menus, cards, buttons), wallets, the
token/dossier card, buy and sell with review + receipt cards, positions,
watchlist, **target-price buy and sell orders** (limit orders, the S2 TARGET
anchor), settings, and the `execute` mode switch with safety gates.

### Target-order price guarantees (both directions)

A target order states an amount and a limit price. The bot monitors net
quotes and fires only inside the bound, and the bound is also enforced
on-chain, so a fill can never violate it:

- **Target BUY** (spend `Q` quote-asset, limit `P`): fires only when the net
  price is **≤ P**; the swap is sent with
  `min_out(tokens) = max(anchor floors, Q ÷ P)` — it may fill at a *better*
  price (more tokens) but never receives fewer tokens than `Q ÷ P`, i.e.
  never pays more than `P`.
- **Target SELL** (sell `A` tokens, limit `P`): fires only when the net price
  is **≥ P**; the swap is sent with
  `min_out(quote) = max(anchor floors, P × A)` — it may fill better but never
  receives less than `P × A`, i.e. never sells below `P`.

Three independent layers hold the line: (1) the watcher triggers on the net
price (after tax and gas, the numbers the user actually receives); (2) the
limit is compiled into the swap calldata as `min-out`, so a worse fill
**reverts on-chain** — a race between trigger and broadcast costs a retry,
never a bad price; (3) the engine re-checks the fill in sim before declaring
a receipt. The sell-side rule is already engine-tested in S1
(`target_order_floor_never_below_target`); S2 adds the mirrored buy-side
engine rule and its test.

**Deliberately NOT in S2** (S3+ or never): sniping/launch filters, copy
trading, DCA, multi-wallet portfolios, referrals, charts, multi-chain,
limit-buy ladders, alerts. A trading bot earns repeat use by being fast,
predictable, and honest — not by being big. Every screen in S2 must belong to
one of the flows below; if a screen doesn't, it's out of scope.

## 1. Architecture

The bot is a **thin UX surface over the engine**. It owns no trading logic: it
translates button presses into `harness::Engine` calls and renders results as
cards. All quoting, routing, safety, and min-out math stays in the S1 engine
(tested invariants are reused, never reimplemented).

```
src/
  tg/
    mod.rs        # TgApi trait (send/edit/answer), Telegram Bot API client
    app.rs        # long-poll loop, dispatch, per-chat serialization
    state.rs      # per-chat finite state machine + pending-order timeout
    cards.rs      # pure render functions: every message the bot ever sends
    handlers.rs   # commands (/start /menu /positions /wallet /watchlist /
                  #            /settings /orders /mode) + callbacks
    notify.rs     # target-order watcher: polls engine, fires orders, notifies
    store.rs      # chat state + wallet persistence (encrypted), behind traits
  bin/bv-tg.rs    # bot binary (reads config, wires Engine + TgApi, runs app)
```

Boundaries:

- **`TgApi` is a trait** (`send`, `edit`, `answer_callback`, `send_photo`-less
  by design) with the real Bot API client as the only production impl and a
  recording mock for tests. No framework dependency: the Bot API is a small
  HTTP surface (getUpdates long-poll, sendMessage, editMessageText,
  answerCallbackQuery) and hand-rolling it keeps `edit`-in-place and error
  handling under our control. Trade-off noted: no teloxide ergonomics; gained:
  deterministic tests and zero dependency sprawl.
- **`cards.rs` is pure** (`state -> String + buttons`), so every card is unit
  testable and the layout grammar is enforceable by review.
- **`notify.rs` is the only background task**: it walks open target orders,
  asks the engine for a quote, and fires when the target condition holds —
  using the engine's floor rule, never its own price math.
- Config adds a `[telegram]` section (bot token comes from the **environment**
  (`TELEGRAM_BOT_TOKEN`), never the TOML file or repo; chat allowlist optional
  for a soft launch). `execute` mode stays a config/engine concern; the bot
  only surfaces it.

## 2. UX organization (what "professional" means here)

Four rules, applied to every screen — this is what makes the bot feel like the
reference product instead of a pile of commands:

1. **One grammar for all cards.** Header line (what this is), data lines
   (aligned key/value), a status/verdict line, then buttons. Every card obeys
   it: token card, review card, receipt, refusal card, positions, settings.
2. **Everything is buttons; commands are just shortcuts.** `/start` lands on
   the main menu; every screen is reachable from it (tested:
   `menu_navigation_reaches_every_screen`). Panels update **in place**
   (`editMessageText`) — the chat never fills with message spam.
3. **Nothing sends without a review card.** Buy/sell always renders route,
   gross → net, gas, min-out (with its anchor breakdown), and the safety
   verdict first; the transaction goes out only on Confirm. Engine refusals
   render as explanatory refusal cards (why, and what to change) — never a
   raw error, never a crash.
4. **The safety story is visible.** Dossier verdicts (tax, honeypot, impact,
   FoT) are on the token card; the review card names the floor anchors; the
   receipt card shows the fill vs min-out. The bot is allowed to say "no"
   (`Block`/`Refuse` from L4) and must do so in one readable line.

### The flows (and their screens)

- **Trade flow:** paste CA (or from watchlist/positions) → token card
  (price, pools, dossier verdicts; buttons: Buy preset sizes 0.01/0.05/0.1/
  0.25/0.5 + custom, Sell, Target order, Back) → sizing panel → review card
  → Confirm/Cancel (timed: 60s) → executing spinner card → receipt card
  (tx link, filled amount vs min-out, safety summary) or refusal card.
- **Positions:** list with entry/PnL where known + current value; buttons
  25/50/75/100% sell (jump straight to review) and per-token card.
- **Target orders:** set (token, amount, target price — typed), list (status:
  watching/fired/cancelled), cancel. Firing = engine-quoted route with
  `min_out = max(applicable anchors, TARGET × amount)` — the S1 invariant,
  now reachable from chat.
- **Wallet:** create (key generated, shown **once**, stored encrypted),
  import, export (two-step confirm + warning), deposit (address + QR-less
  copyable), withdraw. Keys never appear in logs or card text.
- **Settings:** slippage tolerance, gas profile, confirm-before-send toggle,
  observation of `mode` (observe/execute) with an explicit switch that
  re-warns before enabling execute.
- **Watchlist:** list/enrol/remove wired to L7 (which never auto-removes).

### Per-chat state machine

`Idle -> Browsing(token) -> Sizing(order) -> AwaitingConfirm(order) ->
Executing -> Idle`, plus `AwaitingText(target_price | custom amount |
import key)` for typed input. States carry an order draft; a pending confirm
expires after 60s into a cancelled card. Illegal callbacks are answered with
`answer_callback("stale panel")` and the panel refreshes — no crashes from
double taps (all handlers idempotent, serialized per chat).

## 3. Security and honesty model

- Wallet keys: encrypted at rest (XChaCha20-Poly1305 with a key from
  `TG_WALLET_SECRETS_KEY` env), plaintext only in the one-time creation
  message and behind the explicit export confirm.
- `observe` mode never signs; `execute` requires the config flag **and** the
  user-facing toggle, each with its own warning text.
- First-run message carries the standing risk disclaimer once. Claims follow
  the branding rule: never "perfect"; "invariants tested".

## 4. Exact S2 test list

Telegram surface (`tests/tg_*.rs`, mock `TgApi`):
1. `tg::pasted_ca_opens_token_card` — card shows price, pools, dossier verdicts
2. `tg::review_card_shows_route_net_and_minout_anchors`
3. `tg::confirm_sends_transaction_only_in_execute_mode`
4. `tg::observe_mode_never_sends_transaction` (refusal card instead)
5. `tg::confirm_timeout_cancels_order`
6. `tg::sell_preset_percentages_fill_from_position`
7. `tg::positions_card_lists_holdings_and_pnl`
8. `tg::refusal_card_explains_engine_verdict` (Block and Refuse both)
9. `tg::menu_navigation_reaches_every_screen` (organization guarantee)
10. `tg::panels_edit_in_place_not_new_messages`
11. `tg::stale_callback_is_answered_and_panel_refreshed`
12. `tg::state_machine_rejects_illegal_transitions`
13. `tg::double_confirm_is_idempotent` (one tx, never two)
14. `tg::settings_roundtrip_persists_slippage_gas_and_toggles`

Orders and wallet:
15. `tg::target_order_min_out_never_below_target` (bot path mirrors the S1
    invariant through the full render+execute pipeline)
16. `tg::target_buy_min_tokens_never_below_spend_over_target` (mirrored rule:
    buy fills never cost more than the target price)
17. `tg::target_order_never_fires_outside_bound` (buy never above its limit,
    sell never below theirs — trigger + calldata both checked)
18. `tg::target_order_fires_notifies_with_receipt_and_closes`
19. `tg::target_order_cancel_and_list_states`
20. `tg::wallet_store_encrypts_keys_at_rest`
21. `tg::export_key_requires_explicit_confirm_and_warns`
22. `tg::created_key_shown_once_and_never_again`

Config/gates:
23. `config::telegram_token_required_from_env_not_file`
24. `tg::execute_toggle_requires_reconfirm_warning`

## 5. Sample screens

```
BaseVantage · token card
TOKEN (TOKEN) · Base · 0.00000012 USDC
pools   v2 TKN/WETH 1.2M/311 · v3 5bps TVL 842K
dossier sell-tax 0.5% · honeypot no · impact cap ok · FoT no
verdict allow · sources chain-rpc 3s · mode observe
[Buy 0.05] [Buy 0.1] [Buy 0.25] [Sell] [Target] [Back]
```

```
BaseVantage · review · sell 15,000 TKN
route   v3 WETH/USDC 5bps -> v2 TKN/WETH
gross   2,412,880.44 USDC
net     2,397,310.02 USDC   (tax 0.5%, gas 0.00042 ETH)
min-out 2,394,100 (REFERENCE 2,390,000 · SWAP 2,394,100 -> max)
impact  0.31% <= cap 1.5%
verdict allow · mode observe (nothing will be sent)
[Confirm] [Cancel]   · confirm window 60s
```

```
BaseVantage · receipt · sell 15,000 TKN
filled  2,398,120.55 USDC >= min-out 2,394,100
tx      0xabc… (basescan link)
verdict allow · filled above floor
[Positions] [Menu]
```

## 6. Delivery gates

`cargo test` green (all S1 suites + the 22 S2 tests), `cargo clippy
--all-targets -- -D warnings` clean, `cargo fmt --check` clean, a manual
Telegram walkthrough of every flow in `observe` mode, then the S2 report:
files, tests + counts, deviations flagged. Ends with "S2 complete — awaiting
approval for S3."

## 7. Open items for the approver

1. The PROJECT CHARTER has still never arrived (checked again today: repo,
   all Capy Drive scopes, thread). If it exists, it should be reconciled
   before S2 code starts; otherwise this doc proceeds as the working spec
   exactly as S1 did, with the gap flagged.
2. Soft-launch gating: chat allowlist for `execute` mode in the first week —
   recommended, one config key, reversible.
