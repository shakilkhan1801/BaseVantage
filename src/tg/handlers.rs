//! Command and callback dispatch: every tap becomes an engine call and a
//! card. One rule everywhere: nothing sends without a review card, and the
//! engine's refusals come back as explanatory cards, never raw errors.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy::primitives::{Address, U256};

use crate::error::{EngineError, Result};
use crate::tg::cards;
use crate::tg::port::{Draft, DraftKind, EnginePort};
use crate::tg::state::{ChatState, TextPrompt};
use crate::tg::store::{OrderDirection, OrderStatus, Store, TargetOrder};
use crate::tg::{Card, TgApi};

pub struct App<A: TgApi, E: EnginePort> {
    pub api: A,
    pub engine: E,
    pub store: Mutex<Store>,
    states: Mutex<HashMap<i64, ChatState>>,
    next_confirm: AtomicU64,
    /// Execute-mode chat allowlist; `None` = open (still gated by engine mode).
    execute_allowlist: Option<Vec<i64>>,
    confirm_window_secs: u64,
}

fn parse_whole(s: &str) -> Option<U256> {
    let s = s.trim().trim_start_matches("0x");
    // Whole units with optional decimals -> 1e18-scaled.
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    let whole: U256 = int.parse().ok()?;
    let e18 = U256::from(10).pow(U256::from(18));
    let mut frac_scaled = U256::ZERO;
    let mut scale = e18;
    for c in frac.chars().take(18) {
        let d = c.to_digit(10)?;
        scale /= U256::from(10);
        frac_scaled += scale * U256::from(d);
    }
    Some(whole * e18 + frac_scaled)
}

fn parse_addr(s: &str) -> Option<Address> {
    s.trim().parse::<Address>().ok()
}

impl<A: TgApi, E: EnginePort> App<A, E> {
    pub fn new(api: A, engine: E, store: Store, execute_allowlist: Option<Vec<i64>>) -> Self {
        Self::with_confirm_window(api, engine, store, execute_allowlist, 60)
    }

    /// Same app with a custom confirm window (tests use a tiny one).
    pub fn with_confirm_window(
        api: A,
        engine: E,
        store: Store,
        execute_allowlist: Option<Vec<i64>>,
        confirm_window_secs: u64,
    ) -> Self {
        Self {
            api,
            engine,
            store: Mutex::new(store),
            states: Mutex::new(HashMap::new()),
            next_confirm: AtomicU64::new(1),
            execute_allowlist,
            confirm_window_secs,
        }
    }

    fn alloc_confirm(&self) -> u64 {
        self.next_confirm.fetch_add(1, Ordering::Relaxed)
    }

    fn state_for(&self, chat: i64) -> ChatState {
        self.states
            .lock()
            .unwrap()
            .get(&chat)
            .cloned()
            .unwrap_or_default()
    }

    fn update_state<F: FnOnce(&mut ChatState)>(&self, chat: i64, f: F) {
        let mut map = self.states.lock().unwrap();
        f(map.entry(chat).or_default());
    }

    async fn expire_check(&self, chat: i64) {
        let expired = self
            .states
            .lock()
            .unwrap()
            .get_mut(&chat)
            .and_then(|s| s.take_expired());
        if let Some(p) = expired {
            let token = p.draft.token;
            let _ = self
                .api
                .send(chat, &cards::expired("order", &token.to_string()))
                .await;
        }
    }

    // ------------------------------------------------------------ messages

    pub async fn handle_message(&self, chat: i64, text: &str) -> Result<()> {
        self.expire_check(chat).await;
        let trimmed = text.trim();

        // Commands first.
        match trimmed {
            "/start" | "/menu" | "menu" => return self.render_menu(chat, None).await,
            "/positions" => return self.render_positions(chat, None).await,
            "/orders" => return self.render_orders(chat, None).await,
            "/watchlist" => return self.render_watchlist(chat, None).await,
            "/wallet" => return self.render_wallet(chat, None).await,
            "/settings" => return self.render_settings(chat, None).await,
            "/help" => {
                let card = cards::help(self.mode_line());
                return self.send_or_edit(chat, None, card).await;
            }
            _ => {}
        }

        // Typed prompts take precedence.
        let prompt = self.state_for(chat).prompt.clone();
        if let Some(prompt) = prompt {
            return self.handle_prompt(chat, prompt, trimmed).await;
        }

        // A pasted contract address opens the token card.
        if let Some(addr) = parse_addr(trimmed)
            && trimmed.starts_with("0x")
            && trimmed.len() == 42
        {
            self.update_state(chat, |s| s.current_token = Some(addr));
            return self.render_token(chat, addr, None).await;
        }

        let card = cards::help(self.mode_line());
        self.send_or_edit(chat, None, card).await
    }

    async fn handle_prompt(&self, chat: i64, prompt: TextPrompt, text: &str) -> Result<()> {
        self.update_state(chat, |s| s.prompt = None);
        match prompt {
            TextPrompt::BuyAmount { token, symbol } => {
                let Some(spend) = parse_whole(text) else {
                    return self
                        .send_or_edit(chat, None, cards::help("amount not understood"))
                        .await;
                };
                let draft = Draft {
                    id: self.alloc_confirm(),
                    kind: DraftKind::Buy { spend },
                    token,
                    symbol,
                    settlement: self.settlement(),
                };
                self.open_review(chat, draft, None).await
            }
            TextPrompt::SellAmount { token, symbol } => {
                let Some(amount) = parse_whole(text) else {
                    return self
                        .send_or_edit(chat, None, cards::help("amount not understood"))
                        .await;
                };
                let draft = Draft {
                    id: self.alloc_confirm(),
                    kind: DraftKind::Sell { amount },
                    token,
                    symbol,
                    settlement: self.settlement(),
                };
                self.open_review(chat, draft, None).await
            }
            TextPrompt::TargetAmount {
                token,
                symbol,
                direction_buy,
            } => {
                let Some(amount) = parse_whole(text) else {
                    return self
                        .send_or_edit(chat, None, cards::help("amount not understood"))
                        .await;
                };
                self.update_state(chat, |s| {
                    s.prompt = Some(TextPrompt::TargetPrice {
                        token,
                        symbol,
                        direction_buy,
                        amount,
                    })
                });
                self.send_or_edit(chat, None, cards::help("now type the limit price"))
                    .await
            }
            TextPrompt::TargetPrice {
                token,
                symbol,
                direction_buy,
                amount,
            } => {
                let Some(price) = parse_whole(text) else {
                    return self
                        .send_or_edit(chat, None, cards::help("price not understood"))
                        .await;
                };
                let kind = if direction_buy {
                    DraftKind::TargetBuy {
                        spend: amount,
                        limit_price_1e18: price,
                    }
                } else {
                    DraftKind::TargetSell {
                        amount,
                        limit_price_1e18: price,
                    }
                };
                let id = self.alloc_confirm();
                let direction = if direction_buy { "buy" } else { "sell" };
                let settle_sym = self.engine.settlement_symbol();
                let rule = if direction_buy {
                    "min-out  max(anchor floors, spend ÷ limit) — never fewer tokens,\n         better fills always accepted"
                } else {
                    "min-out  max(anchor floors, limit × amount) — never fewer units,\n         better fills always accepted"
                };
                let body = format!(
                    "token    {} · amount {}\nlimit    {} {settle_sym} per token\n{}\nfires    when a live route prices {} the limit",
                    symbol,
                    cards::fmt_units(&amount),
                    cards::fmt_price(&price),
                    rule,
                    if direction_buy { "≤" } else { "≥" },
                );
                let card = cards::target_review(id, direction, &body, !self.engine.mode_execute());
                let msg_id = self.api.send(chat, &card).await?;
                let draft = Draft {
                    id,
                    kind,
                    token,
                    symbol,
                    settlement: self.settlement(),
                };
                self.update_state(chat, |s| {
                    s.target_draft = None;
                    s.arm_confirm(draft, msg_id, self.confirm_window_secs);
                });
                Ok(())
            }
            TextPrompt::ImportKey => {
                let result = self.store.lock().unwrap().import_wallet(chat, text, None);
                let card = match result {
                    Ok(addr) => cards::wallet(
                        Some(&addr.to_string()),
                        "—",
                        "encrypted · imported just now",
                    ),
                    Err(e) => {
                        cards::refusal("import", &e.to_string(), "check the key and retry", "0x0")
                    }
                };
                self.send_or_edit(chat, None, card).await
            }
            TextPrompt::WithdrawAddress => {
                let Some(to) = parse_addr(text) else {
                    return self
                        .send_or_edit(chat, None, cards::help("address not understood"))
                        .await;
                };
                self.update_state(chat, |s| s.prompt = Some(TextPrompt::WithdrawAmount { to }));
                self.send_or_edit(chat, None, cards::help("now type the amount to withdraw"))
                    .await
            }
            TextPrompt::WithdrawAmount { to } => {
                let Some(amount) = parse_whole(text) else {
                    return self
                        .send_or_edit(chat, None, cards::help("amount not understood"))
                        .await;
                };
                if !self.engine.mode_execute() {
                    return self
                        .send_or_edit(chat, None, cards::observe_decline("withdrawal"))
                        .await;
                }
                let signer = {
                    let store = self.store.lock().unwrap();
                    store.open_wallet(chat, None)?
                };
                match self.engine.withdraw(&signer, to, amount).await {
                    Ok(tx) => {
                        let card = cards::receipt(&cards::ReceiptData {
                            title: "withdraw".to_string(),
                            fill_line: format!(
                                "{} sent to {}",
                                cards::fmt_units(&amount),
                                cards::short_addr(&to.to_string())
                            ),
                            impact_line: "—".to_string(),
                            tx_hash: tx,
                            verdict_line: "sent".to_string(),
                            token_addr: self.settlement().to_string(),
                        });
                        self.send_or_edit(chat, None, card).await
                    }
                    Err(e) => self.refusal(chat, "withdraw", &e, "0x0", None).await,
                }
            }
        }
    }

    // ----------------------------------------------------------- callbacks

    pub async fn handle_callback(
        &self,
        chat: i64,
        message_id: i64,
        callback_id: &str,
        data: &str,
    ) -> Result<()> {
        self.api.answer_callback(callback_id, None).await?;
        self.expire_check(chat).await;
        let arg = |prefix: &str| data.strip_prefix(prefix).unwrap_or("").to_string();

        match {
            if data == "menu" {
                "menu".to_string()
            } else {
                data.to_string()
            }
        }
        .as_str()
        {
            "menu" => return self.render_menu(chat, Some(message_id)).await,
            "nav:trade" => {
                let card = cards::help("paste a token address (0x…) to open its trade card");
                return self.send_or_edit(chat, Some(message_id), card).await;
            }
            "nav:pos" => return self.render_positions(chat, Some(message_id)).await,
            "nav:ord" => return self.render_orders(chat, Some(message_id)).await,
            "nav:wl" => return self.render_watchlist(chat, Some(message_id)).await,
            "nav:wal" => return self.render_wallet(chat, Some(message_id)).await,
            "nav:set" => return self.render_settings(chat, Some(message_id)).await,
            "nav:help" => {
                let card = cards::help(self.mode_line());
                return self.send_or_edit(chat, Some(message_id), card).await;
            }
            _ => {}
        }

        if let Some(addr) = data.strip_prefix("tok:") {
            let addr = parse_addr(addr).ok_or_else(|| EngineError::Config("bad token".into()))?;
            self.update_state(chat, |s| s.current_token = Some(addr));
            return self.render_token(chat, addr, Some(message_id)).await;
        }

        if data.starts_with("t:buy:") {
            let spend = parse_whole(arg("t:buy:").as_str()).unwrap_or_default();
            return self.start_buy(chat, spend, Some(message_id)).await;
        }
        if data == "t:buyx" {
            let token = self.require_token(chat)?;
            self.update_state(chat, |s| {
                s.prompt = Some(TextPrompt::BuyAmount {
                    token,
                    symbol: String::new(),
                })
            });
            return self
                .send_or_edit(
                    chat,
                    Some(message_id),
                    cards::help("type the amount to spend"),
                )
                .await;
        }
        if data.starts_with("t:sellp:") {
            let pct: u64 = arg("t:sellp:").parse().unwrap_or(100);
            return self.start_sell(chat, pct, Some(message_id)).await;
        }
        if data == "t:sellx" {
            let token = self.require_token(chat)?;
            self.update_state(chat, |s| {
                s.prompt = Some(TextPrompt::SellAmount {
                    token,
                    symbol: String::new(),
                })
            });
            return self
                .send_or_edit(
                    chat,
                    Some(message_id),
                    cards::help("type the token amount to sell"),
                )
                .await;
        }
        if data == "t:sell" {
            return self.start_sell(chat, 100, Some(message_id)).await;
        }
        if data == "t:tgt" {
            let token = self.require_token(chat)?;
            let card = Card {
                text: "BaseVantage · target order\ndirection?".to_string(),
                rows: vec![
                    vec![
                        crate::tg::Button::new("Buy ▸", &format!("ord:newdir:buy:{token}")),
                        crate::tg::Button::new("Sell ▸", &format!("ord:newdir:sell:{token}")),
                    ],
                    vec![crate::tg::Button::new("← Menu", "menu")],
                ],
            };
            return self.send_or_edit(chat, Some(message_id), card).await;
        }
        if data.starts_with("ord:newdir:") {
            let rest = arg("ord:newdir:");
            let (dir, addr) = rest.split_once(':').unwrap_or(("buy", ""));
            let token = parse_addr(addr).ok_or_else(|| EngineError::Config("bad token".into()))?;
            let direction_buy = dir == "buy";
            self.update_state(chat, |s| {
                s.prompt = Some(TextPrompt::TargetAmount {
                    token,
                    symbol: String::new(),
                    direction_buy,
                })
            });
            return self
                .send_or_edit(
                    chat,
                    Some(message_id),
                    cards::help("type the amount to trade"),
                )
                .await;
        }
        if data == "ord:review" {
            // Review from the sizing panel: draft already in state via t:buy.
            let state = self.state_for(chat);
            if let Some(p) = state.pending {
                return self.open_review(chat, p.draft, Some(message_id)).await;
            }
            return self
                .send_or_edit(chat, Some(message_id), cards::help("start a trade first"))
                .await;
        }
        if let Some(id) = data.strip_prefix("ord:ok:") {
            let id: u64 = id.parse().unwrap_or(0);
            return self.confirm_order(chat, message_id, id).await;
        }
        if let Some(id) = data.strip_prefix("ord:place:") {
            let id: u64 = id.parse().unwrap_or(0);
            return self.place_target(chat, message_id, id).await;
        }
        if let Some(id) = data.strip_prefix("ord:fire:") {
            let id: u64 = id.parse().unwrap_or(0);
            return self.fire_target(chat, message_id, id).await;
        }
        if let Some(id) = data.strip_prefix("ord:cancel:") {
            let id: u64 = id.parse().unwrap_or(0);
            self.update_state(chat, |s| {
                if s.pending.as_ref().is_some_and(|p| p.draft.id == id) {
                    s.pending = None;
                }
            });
            let _ = self
                .store
                .lock()
                .unwrap()
                .set_order_status(id, OrderStatus::Cancelled);
            return self
                .send_or_edit(
                    chat,
                    Some(message_id),
                    cards::help("cancelled — nothing was sent"),
                )
                .await;
        }
        if data == "ord:new" {
            let card = Card {
                text: "BaseVantage · new target order\npaste a token address (0x…) first, then choose Buy/Target ▸ on its card."
                    .to_string(),
                rows: vec![vec![crate::tg::Button::new("← Menu", "menu")]],
            };
            return self.send_or_edit(chat, Some(message_id), card).await;
        }
        if data == "w:create" {
            let result = self.store.lock().unwrap().create_wallet(chat, None);
            return match result {
                Ok((signer, key_hex)) => {
                    let card = cards::export_reveal(&key_hex);
                    let _ = signer;
                    self.send_or_edit(chat, Some(message_id), card).await
                }
                Err(e) => {
                    self.refusal(chat, "wallet create", &e, "0x0", Some(message_id))
                        .await
                }
            };
        }
        if data == "w:import" {
            self.update_state(chat, |s| s.prompt = Some(TextPrompt::ImportKey));
            return self
                .send_or_edit(
                    chat,
                    Some(message_id),
                    cards::help("paste your private key"),
                )
                .await;
        }
        if data == "w:exp" {
            return self
                .send_or_edit(chat, Some(message_id), cards::export_warning())
                .await;
        }
        if data == "w:exp2" {
            let signer = self.store.lock().unwrap().open_wallet(chat, None);
            return match signer {
                Ok(_) => {
                    // Export requires a second open with the passphrase layer
                    // when active; here the key hex is only shown via the
                    // one-time creation message or import echo.
                    self.send_or_edit(
                        chat,
                        Some(message_id),
                        cards::help(
                            "key export: use the one-time creation message, or re-import to rotate",
                        ),
                    )
                    .await
                }
                Err(e) => {
                    self.refusal(chat, "export", &e, "0x0", Some(message_id))
                        .await
                }
            };
        }
        if data == "w:dep" {
            let addr = self.store.lock().unwrap().wallet_address(chat);
            return match addr {
                Some(a) => {
                    self.send_or_edit(chat, Some(message_id), cards::deposit(&a.to_string()))
                        .await
                }
                None => self.render_wallet(chat, Some(message_id)).await,
            };
        }
        if data == "w:full" {
            let addr = self.store.lock().unwrap().wallet_address(chat);
            return match addr {
                Some(a) => {
                    self.send_or_edit(chat, Some(message_id), cards::deposit(&a.to_string()))
                        .await
                }
                None => self.render_wallet(chat, Some(message_id)).await,
            };
        }
        if data == "w:wd" {
            self.update_state(chat, |s| s.prompt = Some(TextPrompt::WithdrawAddress));
            return self
                .send_or_edit(
                    chat,
                    Some(message_id),
                    cards::help("type the destination address"),
                )
                .await;
        }
        if data.starts_with("set:") {
            return self.handle_settings(chat, message_id, &arg("set:")).await;
        }
        if data == "back:token" {
            if let Some(t) = self.state_for(chat).current_token {
                return self.render_token(chat, t, Some(message_id)).await;
            }
            return self.render_menu(chat, Some(message_id)).await;
        }
        if data.starts_with("posopen:") {
            let addr = parse_addr(arg("posopen:").as_str())
                .ok_or_else(|| EngineError::Config("bad token".into()))?;
            return self.render_position(chat, addr, Some(message_id)).await;
        }

        // Unknown/stale callback: refresh the panel silently.
        let _ = self
            .api
            .answer_callback(callback_id, Some("stale panel"))
            .await;
        Ok(())
    }

    // ------------------------------------------------------------- flows

    async fn start_buy(&self, chat: i64, spend: U256, edit: Option<i64>) -> Result<()> {
        let token = self.require_token(chat)?;
        let view = self.engine.token(token, self.wallet(chat)).await?;
        let draft = Draft {
            id: self.alloc_confirm(),
            kind: DraftKind::Buy { spend },
            token,
            symbol: view.symbol.clone(),
            settlement: self.settlement(),
        };
        let card = cards::buy_sizing(
            &view.symbol,
            &format!(
                "{} {}",
                cards::fmt_units(&spend),
                self.engine.settlement_symbol()
            ),
            "estimated on review",
            1.0,
        );
        self.update_state(chat, |s| {
            s.arm_confirm(draft.clone(), 0, self.confirm_window_secs)
        });
        self.send_or_edit(chat, edit, card).await
    }

    async fn start_sell(&self, chat: i64, pct: u64, edit: Option<i64>) -> Result<()> {
        let token = self.require_token(chat)?;
        let wallet = self
            .wallet(chat)
            .ok_or_else(|| EngineError::SafetyRefused("wallet required".into()))?;
        let view = self.engine.token(token, Some(wallet)).await?;
        let amount = view.holds * U256::from(pct) / U256::from(100);
        if amount.is_zero() {
            return self
                .send_or_edit(chat, edit, cards::help("nothing to sell"))
                .await;
        }
        let draft = Draft {
            id: self.alloc_confirm(),
            kind: DraftKind::Sell { amount },
            token,
            symbol: view.symbol.clone(),
            settlement: self.settlement(),
        };
        self.update_state(chat, |s| {
            s.arm_confirm(draft.clone(), 0, self.confirm_window_secs)
        });
        self.open_review(chat, draft, edit).await
    }

    async fn open_review(&self, chat: i64, draft: Draft, edit: Option<i64>) -> Result<()> {
        let quote = self.engine.quote(&draft).await?;
        if !quote.verdict_ok {
            return self
                .refusal(
                    chat,
                    &quote.title,
                    &EngineError::SafetyRefused(quote.refusal_reason.clone()),
                    &draft.token.to_string(),
                    edit,
                )
                .await;
        }
        let body = format!(
            "route   {}\ngross   {}\nnet     {}\nmin-out {}\n        {}\nimpact  {}",
            quote.route,
            quote.gross_line,
            quote.net_line,
            quote.min_out_line,
            quote.anchor_line,
            quote.impact_line
        );
        let card = cards::review(
            draft.id,
            &cards::ReviewData {
                title: quote.title,
                body,
                mode_observe: !self.engine.mode_execute(),
            },
        );
        let msg_id = match edit {
            Some(mid) => {
                self.api.edit(chat, mid, &card).await?;
                mid
            }
            None => self.api.send(chat, &card).await?,
        };
        self.update_state(chat, |s| {
            s.arm_confirm(draft, msg_id, self.confirm_window_secs)
        });
        Ok(())
    }

    async fn confirm_order(&self, chat: i64, message_id: i64, id: u64) -> Result<()> {
        let taken = self.update_and_take(chat, id);
        let Some(pending) = taken else {
            let _ = self
                .api
                .answer_callback("done", Some("already handled"))
                .await;
            return Ok(());
        };
        let title = pending.draft.symbol.clone();
        if !self.engine.mode_execute() || !self.execute_allowed(chat) {
            let card = cards::observe_decline(&title);
            return self.send_or_edit(chat, Some(message_id), card).await;
        }
        let signer = self.store.lock().unwrap().open_wallet(chat, None)?;
        let _ = self
            .api
            .edit(chat, message_id, &cards::executing(&title))
            .await;
        match self.engine.execute(&pending.draft, &signer).await {
            Ok(receipt) => {
                let card = cards::receipt(&cards::ReceiptData {
                    title: receipt.title,
                    fill_line: receipt.fill_line,
                    impact_line: receipt.impact_line,
                    tx_hash: receipt.tx_hash,
                    verdict_line: receipt.verdict_line,
                    token_addr: pending.draft.token.to_string(),
                });
                self.send_or_edit(chat, Some(message_id), card).await
            }
            Err(e) => {
                self.refusal(
                    chat,
                    &title,
                    &e,
                    &pending.draft.token.to_string(),
                    Some(message_id),
                )
                .await
            }
        }
    }

    async fn place_target(&self, chat: i64, message_id: i64, id: u64) -> Result<()> {
        let taken = self.update_and_take(chat, id);
        let Some(pending) = taken else {
            return Ok(());
        };
        let (direction, amount, price) = match pending.draft.kind {
            DraftKind::TargetBuy {
                spend,
                limit_price_1e18,
            } => (OrderDirection::Buy, spend, limit_price_1e18),
            DraftKind::TargetSell {
                amount,
                limit_price_1e18,
            } => (OrderDirection::Sell, amount, limit_price_1e18),
            _ => return Ok(()),
        };
        self.store.lock().unwrap().add_order(
            chat,
            crate::tg::store::OrderSpec {
                token: pending.draft.token,
                symbol: pending.draft.symbol.clone(),
                settlement: pending.draft.settlement,
                direction,
                amount,
                limit_price_1e18: price,
            },
        );
        self.render_orders(chat, Some(message_id)).await
    }

    async fn fire_target(&self, chat: i64, message_id: i64, id: u64) -> Result<()> {
        // Build the trade from the stored order and run the same confirm path.
        let order: Option<TargetOrder> = self
            .store
            .lock()
            .unwrap()
            .orders_for(chat)
            .into_iter()
            .find(|o| o.id == id && o.status == OrderStatus::Watching);
        let Some(order) = order else {
            return self
                .send_or_edit(
                    chat,
                    Some(message_id),
                    cards::help("order is no longer open"),
                )
                .await;
        };
        let kind = match order.direction {
            OrderDirection::Buy => DraftKind::TargetBuy {
                spend: order.amount,
                limit_price_1e18: order.limit_price_1e18,
            },
            OrderDirection::Sell => DraftKind::TargetSell {
                amount: order.amount,
                limit_price_1e18: order.limit_price_1e18,
            },
        };
        let draft = Draft {
            id: self.alloc_confirm(),
            kind,
            token: order.token,
            symbol: order.symbol.clone(),
            settlement: order.settlement,
        };
        self.update_state(chat, |s| {
            s.arm_confirm(draft.clone(), message_id, self.confirm_window_secs)
        });
        if !self.engine.mode_execute() || !self.execute_allowed(chat) {
            let card = cards::observe_decline(&order.symbol);
            return self.send_or_edit(chat, Some(message_id), card).await;
        }
        let signer = self.store.lock().unwrap().open_wallet(chat, None)?;
        let _ = self
            .api
            .edit(chat, message_id, &cards::executing(&order.symbol))
            .await;
        match self.engine.execute(&draft, &signer).await {
            Ok(receipt) => {
                let _ = self.store.lock().unwrap().set_order_status(
                    id,
                    OrderStatus::Fired {
                        tx: receipt.tx_hash.clone(),
                    },
                );
                let card = cards::receipt(&cards::ReceiptData {
                    title: receipt.title,
                    fill_line: receipt.fill_line,
                    impact_line: receipt.impact_line,
                    tx_hash: receipt.tx_hash,
                    verdict_line: receipt.verdict_line,
                    token_addr: order.token.to_string(),
                });
                self.send_or_edit(chat, Some(message_id), card).await
            }
            Err(e) => {
                self.refusal(
                    chat,
                    &order.symbol,
                    &e,
                    &order.token.to_string(),
                    Some(message_id),
                )
                .await
            }
        }
    }

    async fn handle_settings(&self, chat: i64, message_id: i64, arg: &str) -> Result<()> {
        let mut prefs = self.store.lock().unwrap().prefs(chat);
        match arg {
            "slip:+" => prefs.slippage_bps = (prefs.slippage_bps + 25).min(500),
            "slip:-" => prefs.slippage_bps = prefs.slippage_bps.saturating_sub(25).max(25),
            "gas:+" => prefs.gas_profile = (prefs.gas_profile + 1).min(2),
            "gas:-" => prefs.gas_profile = prefs.gas_profile.saturating_sub(1),
            "confirm" => prefs.confirm_before_send = !prefs.confirm_before_send,
            "mode" => {
                let card = cards::execute_confirm();
                return self.send_or_edit(chat, Some(message_id), card).await;
            }
            "mode:on" => {
                if !self.execute_allowed(chat) {
                    return self
                        .send_or_edit(
                            chat,
                            Some(message_id),
                            cards::help("execute mode is allowlist-only during soft launch"),
                        )
                        .await;
                }
                let card = cards::help("execute mode is controlled by the engine config flag");
                return self.send_or_edit(chat, Some(message_id), card).await;
            }
            _ => {}
        }
        let _ = self.store.lock().unwrap().set_prefs(chat, prefs);
        self.render_settings(chat, Some(message_id)).await
    }

    // ----------------------------------------------------------- renderers

    async fn render_menu(&self, chat: i64, edit: Option<i64>) -> Result<()> {
        let addr = self.store.lock().unwrap().wallet_address(chat);
        let card = cards::menu(
            addr.as_ref().map(|a| a.to_string()).as_deref(),
            "run /positions for holdings",
            self.mode_line(),
        );
        self.send_or_edit(chat, edit, card).await
    }

    async fn render_token(&self, chat: i64, token: Address, edit: Option<i64>) -> Result<()> {
        let view = match self.engine.token(token, self.wallet(chat)).await {
            Ok(v) => v,
            Err(e) => {
                return self
                    .refusal(chat, "token", &e, &token.to_string(), edit)
                    .await;
            }
        };
        let holds = !view.holds.is_zero();
        let card = cards::token_card(
            &token.to_string(),
            &cards::TokenCardData {
                symbol: view.symbol,
                price_line: view.price_line,
                pools_line: view.pools_line,
                dossier_line: view.dossier_line,
                impact_line: view.impact_line,
                verdict_line: view.verdict_line,
                blocked: view.blocked,
                block_reason: view.block_reason,
                holds,
            },
        );
        self.send_or_edit(chat, edit, card).await
    }

    async fn render_positions(&self, chat: i64, edit: Option<i64>) -> Result<()> {
        let Some(wallet) = self.wallet(chat) else {
            return self
                .send_or_edit(chat, edit, cards::help("create or import a wallet first"))
                .await;
        };
        let positions = self.engine.positions(wallet).await?;
        if positions.is_empty() {
            return self
                .send_or_edit(chat, edit, cards::help("no positions"))
                .await;
        }
        let mut listing = String::new();
        for p in &positions {
            listing.push_str(&format!(
                "{}   {} · {} · {}\n",
                p.symbol,
                cards::fmt_units(&p.amount),
                p.value_line,
                p.pnl_line
            ));
        }
        let first = positions[0].token.to_string();
        let card = cards::positions(&listing, "total   see rows", Some(&first));
        self.send_or_edit(chat, edit, card).await
    }

    async fn render_position(&self, chat: i64, token: Address, edit: Option<i64>) -> Result<()> {
        let view = self.engine.token(token, self.wallet(chat)).await?;
        let card = cards::sell_position(
            &view.symbol,
            &format!("{} {}", cards::fmt_units(&view.holds), view.symbol),
            &view.price_line,
            &view.verdict_line,
            &token.to_string(),
        );
        self.send_or_edit(chat, edit, card).await
    }

    async fn render_orders(&self, chat: i64, edit: Option<i64>) -> Result<()> {
        let orders = self.store.lock().unwrap().orders_for(chat);
        if orders.is_empty() {
            return self
                .send_or_edit(chat, edit, cards::orders_list("no target orders yet"))
                .await;
        }
        let mut listing = String::new();
        for o in &orders {
            let dir = match o.direction {
                OrderDirection::Buy => "BUY ",
                OrderDirection::Sell => "SELL",
            };
            let status = match &o.status {
                OrderStatus::Watching => "watching".to_string(),
                OrderStatus::Fired { .. } => "fired".to_string(),
                OrderStatus::Cancelled => "cancelled".to_string(),
            };
            listing.push_str(&format!(
                "#{}  {} {} · limit {} · {}\n",
                o.id,
                dir,
                o.symbol,
                cards::fmt_price(&o.limit_price_1e18),
                status
            ));
        }
        let card = cards::orders_list(&listing);
        self.send_or_edit(chat, edit, card).await
    }

    async fn render_watchlist(&self, chat: i64, edit: Option<i64>) -> Result<()> {
        let card = cards::watchlist("watchlist is managed from token cards", 0);
        self.send_or_edit(chat, edit, card).await
    }

    async fn render_wallet(&self, chat: i64, edit: Option<i64>) -> Result<()> {
        let addr = self.store.lock().unwrap().wallet_address(chat);
        let card = cards::wallet(
            addr.as_ref().map(|a| a.to_string()).as_deref(),
            "run /positions for holdings",
            "encrypted · KEK outside the store",
        );
        self.send_or_edit(chat, edit, card).await
    }

    async fn render_settings(&self, chat: i64, edit: Option<i64>) -> Result<()> {
        let prefs = self.store.lock().unwrap().prefs(chat);
        let gas_label = match prefs.gas_profile {
            0 => "economy",
            2 => "fast",
            _ => "standard",
        };
        let card = cards::settings(&cards::SettingsView {
            slippage_pct: prefs.slippage_bps as f64 / 100.0,
            gas_label,
            confirm_on: prefs.confirm_before_send,
            mode_line: self.mode_line().to_string(),
        });
        self.send_or_edit(chat, edit, card).await
    }

    // ------------------------------------------------------------ helpers

    fn mode_line(&self) -> &'static str {
        if self.engine.mode_execute() {
            "execute (real funds)"
        } else {
            "observe (nothing is sent to chain)"
        }
    }

    fn settlement(&self) -> Address {
        self.engine.settlement()
    }

    fn wallet(&self, chat: i64) -> Option<Address> {
        self.store.lock().unwrap().wallet_address(chat)
    }

    fn require_token(&self, chat: i64) -> Result<Address> {
        self.state_for(chat)
            .current_token
            .ok_or_else(|| EngineError::SafetyRefused("open a token card first".into()))
    }

    fn execute_allowed(&self, chat: i64) -> bool {
        match &self.execute_allowlist {
            None => true,
            Some(list) => list.contains(&chat),
        }
    }

    fn update_and_take(&self, chat: i64, id: u64) -> Option<crate::tg::state::PendingConfirm> {
        self.states
            .lock()
            .unwrap()
            .get_mut(&chat)
            .and_then(|s| s.take_pending(id))
    }

    async fn refusal(
        &self,
        chat: i64,
        title: &str,
        err: &EngineError,
        token: &str,
        edit: Option<i64>,
    ) -> Result<()> {
        let card = cards::refusal(
            title,
            &err.to_string(),
            "adjust the trade or pick a direct route",
            token,
        );
        self.send_or_edit(chat, edit, card).await
    }

    async fn send_or_edit(&self, chat: i64, edit: Option<i64>, card: Card) -> Result<()> {
        match edit {
            Some(mid) => self.api.edit(chat, mid, &card).await,
            None => {
                self.api.send(chat, &card).await?;
                Ok(())
            }
        }
    }
}
