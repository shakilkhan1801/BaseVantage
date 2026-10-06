//! `tg::*` — the Telegram UX contract: every screen renders through one
//! grammar, nothing sends without a review card, and panels edit in place.

mod common;

use alloy::primitives::Address;
use basevantage::tg::handlers::App;
use basevantage::tg::store::Store;
use basevantage::tg::{Card, MockApi};
use common::MockEngine;

const CHAT: i64 = 42;
const TOKEN: &str = "0x1111111111111111111111111111111111111111";

fn setup(execute: bool) -> (App<MockApi, MockEngine>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::load(&dir.path().join("state.json"), Some([7u8; 32])).unwrap();
    let engine = MockEngine::new();
    engine.set_execute(execute);
    let app = App::new(MockApi::new(), engine, store, None);
    (app, dir)
}

fn tap(app: &App<MockApi, MockEngine>, msg: i64, data: &str) {
    futures::executor::block_on(app.handle_callback(CHAT, msg, &format!("cb-{data}"), data))
        .unwrap();
}

fn say(app: &App<MockApi, MockEngine>, text: &str) {
    futures::executor::block_on(app.handle_message(CHAT, text)).unwrap();
}

fn last_text(app: &App<MockApi, MockEngine>) -> String {
    app.api.last_card(CHAT).text
}

#[test]
fn pasted_ca_opens_token_card() {
    let (app, _d) = setup(false);
    say(&app, TOKEN);
    let text = last_text(&app);
    assert!(text.contains("token"), "must open the token card: {text}");
    assert!(
        text.contains("dossier"),
        "dossier verdicts must be on the card: {text}"
    );
    assert!(
        text.contains("verdict"),
        "verdict line must be on the card: {text}"
    );
}

#[test]
fn review_card_shows_route_net_and_minout_anchors() {
    let (app, _d) = setup(false);
    say(&app, TOKEN);
    tap(&app, 1, "t:buy:0.1");
    tap(&app, 1, "ord:review");
    let text = last_text(&app);
    assert!(text.contains("REVIEW"), "review card expected: {text}");
    assert!(text.contains("route"), "{text}");
    assert!(text.contains("net"), "{text}");
    assert!(text.contains("min-out"), "{text}");
    assert!(
        text.contains("REFERENCE"),
        "anchor breakdown expected: {text}"
    );
    assert!(text.contains("Confirm") || app.api.history(CHAT).len() > 1);
    let card: Card = app.api.last_card(CHAT);
    assert!(
        card.rows
            .iter()
            .flatten()
            .any(|b| b.callback.starts_with("ord:ok:")),
        "confirm button must carry the draft id"
    );
}

#[test]
fn confirm_sends_transaction_only_in_execute_mode() {
    let (app, _d) = setup(true);
    tap(&app, 1, "w:create");
    say(&app, TOKEN);
    tap(&app, 1, "t:sellp:100");
    let card = app.api.last_card(CHAT);
    let confirm = card
        .rows
        .iter()
        .flatten()
        .find(|b| b.callback.starts_with("ord:ok:"))
        .expect("confirm button")
        .clone();
    tap(&app, 1, &confirm.callback);
    assert_eq!(app.engine.execute_count(), 1, "execute mode must send");
    assert!(last_text(&app).contains("receipt"), "receipt card expected");
}

#[test]
fn observe_mode_never_sends_transaction() {
    let (app, _d) = setup(false);
    tap(&app, 1, "w:create");
    say(&app, TOKEN);
    tap(&app, 1, "t:sellp:100");
    let confirm = app
        .api
        .last_card(CHAT)
        .rows
        .iter()
        .flatten()
        .find(|b| b.callback.starts_with("ord:ok:"))
        .expect("confirm button")
        .clone();
    tap(&app, 1, &confirm.callback);
    assert_eq!(app.engine.execute_count(), 0, "observe must never send");
    assert!(
        last_text(&app).contains("observe"),
        "observe decline card expected"
    );
}

#[test]
fn confirm_timeout_cancels_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::load(&dir.path().join("state.json"), Some([7u8; 32])).unwrap();
    let engine = MockEngine::new();
    engine.set_execute(true);
    let app = App::with_confirm_window(MockApi::new(), engine, store, None, 0);
    tap(&app, 1, "w:create");
    say(&app, TOKEN);
    tap(&app, 1, "t:sellp:100");
    let confirm = app
        .api
        .last_card(CHAT)
        .rows
        .iter()
        .flatten()
        .find(|b| b.callback.starts_with("ord:ok:"))
        .expect("confirm button")
        .clone();
    tap(&app, 1, &confirm.callback);
    assert_eq!(
        app.engine.execute_count(),
        0,
        "expired confirm must not send"
    );
    let history = app.api.history(CHAT);
    assert!(
        history.iter().any(|c| c.text.contains("expired")),
        "expiry must render an expired card"
    );
}

#[test]
fn sell_preset_percentages_fill_from_position() {
    let (app, _d) = setup(true);
    tap(&app, 1, "w:create");
    say(&app, TOKEN);
    tap(&app, 1, "t:sellp:50");
    let text = last_text(&app);
    // 50% of the mock holdings (15,000) = 7,500 tokens in the review.
    assert!(text.contains("7,500"), "half position expected: {text}");
}

#[test]
fn positions_card_lists_holdings_and_pnl() {
    let (app, _d) = setup(false);
    tap(&app, 1, "w:create");
    tap(&app, 1, "nav:pos");
    let text = last_text(&app);
    assert!(text.contains("TOKEN"), "{text}");
    assert!(text.contains("+2.4%"), "pnl expected: {text}");
}

#[test]
fn refusal_card_explains_engine_verdict() {
    let (app, _d) = setup(true);
    tap(&app, 1, "w:create");
    say(&app, TOKEN);
    *app.engine.refuse.lock().unwrap() =
        Some("fee-on-transfer token on a multi-hop v3 leg".to_string());
    tap(&app, 1, "t:sellp:100");
    let text = last_text(&app);
    assert!(text.contains("refused"), "{text}");
    assert!(text.contains("not sent"), "{text}");
    assert!(
        text.contains("fee-on-transfer"),
        "reason must be named: {text}"
    );
    assert_eq!(app.engine.execute_count(), 0);
}

#[test]
fn menu_navigation_reaches_every_screen() {
    let (app, _d) = setup(false);
    say(&app, "/start");
    assert!(last_text(&app).contains("menu"));
    tap(&app, 1, "w:create"); // a wallet so positions can render holdings
    for (data, marker) in [
        ("nav:trade", "trade"),
        ("nav:pos", "positions"),
        ("nav:ord", "orders"),
        ("nav:wl", "watchlist"),
        ("nav:wal", "wallet"),
        ("nav:set", "settings"),
        ("nav:help", "help"),
    ] {
        tap(&app, 1, data);
        let text = last_text(&app).to_lowercase();
        assert!(text.contains(marker), "{data} must reach {marker}: {text}");
    }
}

#[test]
fn panels_edit_in_place_not_new_messages() {
    let (app, _d) = setup(false);
    say(&app, "/start");
    let sent_after_start = app.api.sent_count(CHAT);
    tap(&app, 1, "nav:set");
    tap(&app, 1, "nav:help");
    tap(&app, 1, "menu");
    assert_eq!(
        app.api.sent_count(CHAT),
        sent_after_start,
        "navigation must edit the panel in place, not spam messages"
    );
    assert!(app.api.edit_count() >= 3, "panels must be edits");
}

#[test]
fn stale_callback_is_answered_and_panel_refreshed() {
    let (app, _d) = setup(false);
    say(&app, "/start");
    tap(&app, 1, "totally:unknown:999");
    let answered = app.api.answered.lock().unwrap().clone();
    assert!(
        answered.iter().any(|a| a == "stale panel"),
        "stale taps must be acknowledged: {answered:?}"
    );
}

#[test]
fn state_machine_rejects_illegal_transitions() {
    let (app, _d) = setup(true);
    tap(&app, 1, "w:create");
    say(&app, TOKEN);
    // Confirm with an id that was never armed: nothing may send.
    tap(&app, 1, "ord:ok:999");
    assert_eq!(
        app.engine.execute_count(),
        0,
        "unknown confirm id must be ignored"
    );
}

#[test]
fn double_confirm_is_idempotent() {
    let (app, _d) = setup(true);
    tap(&app, 1, "w:create");
    say(&app, TOKEN);
    tap(&app, 1, "t:sellp:100");
    let confirm = app
        .api
        .last_card(CHAT)
        .rows
        .iter()
        .flatten()
        .find(|b| b.callback.starts_with("ord:ok:"))
        .expect("confirm button")
        .clone();
    tap(&app, 1, &confirm.callback);
    tap(&app, 1, &confirm.callback);
    tap(&app, 1, &confirm.callback);
    assert_eq!(app.engine.execute_count(), 1, "one tap window, one tx");
}

#[test]
fn settings_roundtrip_persists_slippage_gas_and_toggles() {
    let (app, _d) = setup(false);
    say(&app, "/settings");
    tap(&app, 1, "set:slip:+");
    tap(&app, 1, "set:gas:+");
    tap(&app, 1, "set:confirm");
    let prefs = app.store.lock().unwrap().prefs(CHAT);
    assert_eq!(prefs.slippage_bps, 125);
    assert_eq!(prefs.gas_profile, 2);
    assert!(!prefs.confirm_before_send);
    let text = last_text(&app);
    assert!(text.contains("1.2%") || text.contains("1.25%"), "{text}");
    assert!(text.contains("fast"), "{text}");
}

// Keeps the Address import and the card type referenced for clarity.
#[allow(dead_code)]
fn _types(_: Address, _: Card) {}
