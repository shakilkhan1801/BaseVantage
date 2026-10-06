# BaseVantage — S2 Report (Telegram bot over the S1 engine)

Date: 2026-10-06. Scope: the S2 design (`docs/S2-DESIGN.md`) and UX
architecture (`docs/S2-UX.md`) as approved: the Telegram surface, wallets,
target-price buy and sell orders, and the execute path — organized to the
reference product's standard, with the feature list kept deliberately small.

## Delivery gates

| Gate | Status |
| --- | --- |
| `cargo test` | green — 15 suites, 54 passing tests (S1's 27 + S2's 27), 6 fork ignored |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| Live Telegram walkthrough (observe mode) | **pending a real bot token** — see flags |

## Files delivered

| Module | Contents |
| --- | --- |
| `src/tg/mod.rs` | `TgApi` trait (send/edit/answer), the real Bot API client (long-poll `getUpdates`, `sendMessage`, `editMessageText`, `answerCallbackQuery`), and the chronological recording mock used by every test |
| `src/tg/cards.rs` | pure renderers for every screen in the UX doc: first contact, menu, token card (with dossier verdicts and blocked variant), sizing, review, executing, receipt, refusal, observe-decline, expired, positions, orders, target review/fill/prompt, watchlist, wallet (deposit/export warning/export reveal), settings, execute confirm |
| `src/tg/state.rs` | per-chat state machine: current token, typed-input prompts, pending confirms with expiry |
| `src/tg/handlers.rs` | command + callback dispatch for all flows; idempotent confirms (one draft id = one send); panels edit in place; stale taps answered and refreshed |
| `src/tg/store.rs` | encrypted wallets (per-wallet DEK under XChaCha20-Poly1305; DEK wrapped by the environment KEK or by an Argon2id passphrase layer), target orders, per-chat prefs — one JSON store |
| `src/tg/notify.rs` | the target-order watcher: fires only when a live route meets the limit (buy ≤ limit, sell ≥ limit), auto-fires only in execute mode with an allowlisted chat and a wallet that opens without the user; otherwise posts a review prompt; refused fills keep the order open |
| `src/tg/adapter.rs` | the real `EnginePort` over the S1 harness engine: router quoting (buys quote as the inverted trade into the token), floor anchoring with the absolute target rule, local signing via `EthereumWallet`, raw broadcast through `ChainAdapter::send_raw` |
| `src/chain/` | `ChainAdapter::send_raw` (+ `BaseChain` implementation) |
| `src/safety/floor.rs` | the buy-side mirror of the target rule: `min_tokens = max(anchor floors, spend ÷ target)` — never fewer tokens than the limit implies |
| `src/bin/bv-tg.rs` | the bot binary: config + `TELEGRAM_BOT_TOKEN`/`TG_WALLET_SECRETS_KEY` from the environment, engine boot, watcher pass + long-poll in one sequential task |
| `tests/tg_flow.rs`, `tests/tg_orders_wallet.rs` | the 26 named S2 tests (27th: the engine buy-floor test lives in `tests/safety.rs`) |

## Tests + counts (S2: 27)

Engine (1): `target_buy_floor_never_below_spend_over_target` (mirror rule at
every tolerance; market anchors may relax, the target may not).
UX contract (14): pasted-CA → token card; review card shows route/net/min-out
anchors; confirm sends only in execute mode; observe never sends; confirm
timeout cancels; sell presets fill from the position; positions card lists
holdings and PnL; refusal cards explain the verdict; **menu navigation reaches
every screen**; panels edit in place; stale callbacks answered; illegal state
transitions rejected; double confirm is idempotent; settings roundtrip.
Orders/wallet/security (12): sell-side and buy-side target rules on the review
cards; watcher never fires outside the bound and fires inside it; fill
notifications close the order; cancel/list states; keys encrypted at rest;
**DB breach without the KEK reveals nothing**; **the passphrase layer survives
full server+DB compromise**; export requires the explicit second confirm; the
key is shown once and never repeated; the bot token is environment-only
(config files carrying it are refused); the execute toggle re-warns every time.

## Flags

1. **The PROJECT CHARTER has still never arrived** (repo, all Capy Drive
   scopes, thread — checked again today). S2 proceeded under the approved
   design + UX docs exactly as S1 did; the reconciliation is still owed.
2. **Live Telegram walkthrough pending**: the final gate needs a real bot
   token (only the user can create one at BotFather). Everything the
   walkthrough would exercise is covered by the 26 mock-driven UX tests; the
   walkthrough itself runs the moment `TELEGRAM_BOT_TOKEN` is provided.
3. **Mixed-venue route execution is refused** by the adapter with a clear
   error (single-family routes execute with their venue's exact calldata);
   Universal-Router mixed-family encoding is its own slice.
4. Buy quotes are computed as the inverted trade (settlement → token) through
   a per-token router; this is the same pool graph the sell side uses.

## Claim

Not "perfect" — **invariants tested**: min-out for target orders is never
below the limit in either direction and is enforced in calldata so worse
fills revert; keys are unreadable without the KEK or the passphrase; observe
mode cannot send; one tap window means one transaction.

S2 complete — awaiting approval for S3.
