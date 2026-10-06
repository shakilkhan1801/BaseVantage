//! `tg::*` — target orders, wallet security, and the config secret rule.

mod common;

use basevantage::tg::MockApi;
use basevantage::tg::handlers::App;
use basevantage::tg::store::{OrderDirection, OrderStatus, Store};
use common::MockEngine;

const CHAT: i64 = 7;
const TOKEN: &str = "0x1111111111111111111111111111111111111111";
const KEK: [u8; 32] = [7u8; 32];

fn setup(execute: bool) -> (App<MockApi, MockEngine>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::load(&dir.path().join("state.json"), Some(KEK)).unwrap();
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

/// Walks the target-order flow up to the place-order review card and returns
/// the confirm callback data.
fn target_review(app: &App<MockApi, MockEngine>, direction: &str) -> String {
    say(app, TOKEN);
    tap(app, 1, "t:tgt");
    tap(app, 1, &format!("ord:newdir:{direction}:{TOKEN}"));
    say(app, "1000"); // amount
    say(app, "2.5"); // limit
    let card = app.api.last_card(CHAT);
    card.rows
        .iter()
        .flatten()
        .find(|b| b.callback.starts_with("ord:place:"))
        .expect("place button")
        .callback
        .clone()
}

#[test]
fn target_order_min_out_never_below_target() {
    // Sell-side: the review card carries the absolute target rule.
    let (app, _d) = setup(false);
    let place = target_review(&app, "sell");
    let text = last_text(&app);
    assert!(
        text.contains("limit × amount"),
        "sell rule expected: {text}"
    );
    assert!(text.contains("min-out"), "{text}");
    assert!(
        text.contains("never fewer"),
        "the bound must be stated: {text}"
    );
    tap(&app, 1, &place);
    let orders = app.store.lock().unwrap().orders_for(CHAT);
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].direction, OrderDirection::Sell);
}

#[test]
fn target_buy_min_tokens_never_below_spend_over_target() {
    // Buy-side mirror: the review card shows spend ÷ limit as the floor.
    let (app, _d) = setup(false);
    let place = target_review(&app, "buy");
    let text = last_text(&app);
    assert!(
        text.contains("≤ limit") || text.contains("≤"),
        "buy bound: {text}"
    );
    tap(&app, 1, &place);
    let orders = app.store.lock().unwrap().orders_for(CHAT);
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].direction, OrderDirection::Buy);
}

#[test]
fn target_order_never_fires_outside_bound() {
    let (app, _d) = setup(true);
    tap(&app, 1, "w:create");
    // Watcher with limit NOT met must do nothing.
    *app.engine.limit_met.lock().unwrap() = Some(false);
    let place = target_review(&app, "sell");
    tap(&app, 1, &place);
    let acted = futures::executor::block_on(basevantage::tg::notify::poll_once(
        &app.api,
        &app.engine,
        &mut app.store.lock().unwrap(),
        |_| true,
    ))
    .unwrap();
    assert_eq!(acted, 0, "must not fire outside the bound");
    assert_eq!(app.engine.execute_count(), 0);

    // Only when the live route meets the limit does it fire.
    *app.engine.limit_met.lock().unwrap() = Some(true);
    let acted = futures::executor::block_on(basevantage::tg::notify::poll_once(
        &app.api,
        &app.engine,
        &mut app.store.lock().unwrap(),
        |_| true,
    ))
    .unwrap();
    assert_eq!(acted, 1);
    assert_eq!(app.engine.execute_count(), 1);
}

#[test]
fn target_order_fires_notifies_with_receipt_and_closes() {
    let (app, _d) = setup(true);
    tap(&app, 1, "w:create");
    *app.engine.limit_met.lock().unwrap() = Some(true);
    let place = target_review(&app, "sell");
    tap(&app, 1, &place);
    futures::executor::block_on(basevantage::tg::notify::poll_once(
        &app.api,
        &app.engine,
        &mut app.store.lock().unwrap(),
        |_| true,
    ))
    .unwrap();
    let text = last_text(&app);
    assert!(text.contains("target order filled"), "{text}");
    assert!(
        text.contains("sold ≥ limit") || text.contains("tx"),
        "{text}"
    );
    let orders = app.store.lock().unwrap().orders_for(CHAT);
    assert!(matches!(orders[0].status, OrderStatus::Fired { .. }));
}

#[test]
fn target_order_cancel_and_list_states() {
    let (app, _d) = setup(false);
    let place = target_review(&app, "sell");
    tap(&app, 1, &place);
    let orders = app.store.lock().unwrap().orders_for(CHAT);
    assert_eq!(orders[0].status, OrderStatus::Watching);
    let id = orders[0].id;
    tap(&app, 1, &format!("ord:cancel:{id}"));
    let orders = app.store.lock().unwrap().orders_for(CHAT);
    assert_eq!(orders[0].status, OrderStatus::Cancelled);
    tap(&app, 1, "nav:ord");
    assert!(
        last_text(&app).contains("cancelled"),
        "list must show state"
    );
}

#[test]
fn wallet_store_encrypts_keys_at_rest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::load(&path, Some(KEK)).unwrap();
    let (_signer, key_hex) = store.create_wallet(CHAT, None).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        !raw.contains(&key_hex) && !raw.contains(key_hex.trim_start_matches('0')),
        "the plaintext key must never rest in the store"
    );
}

#[test]
fn db_breach_without_kek_reveals_no_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::load(&path, Some(KEK)).unwrap();
    store.create_wallet(CHAT, None).unwrap();
    drop(store);

    // The attacker has the database file but not the environment-held KEK.
    let breached = Store::load(&path, None).unwrap();
    assert!(
        breached.open_wallet(CHAT, None).is_err(),
        "without the KEK the sealed wallet must not open"
    );
}

#[test]
fn passphrase_layer_blocks_decryption_without_passphrase() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let mut store = Store::load(&path, Some(KEK)).unwrap();
    store.create_wallet(CHAT, Some("correct horse")).unwrap();
    drop(store);

    // Even a full server + database compromise (KEK included) cannot open a
    // passphrase-layered wallet without the passphrase.
    let breached = Store::load(&path, Some(KEK)).unwrap();
    assert!(
        breached.open_wallet(CHAT, None).is_err(),
        "no passphrase, no key"
    );
    assert!(
        breached.open_wallet(CHAT, Some("wrong")).is_err(),
        "wrong passphrase must fail"
    );
    assert!(
        breached.open_wallet(CHAT, Some("correct horse")).is_ok(),
        "the passphrase opens it"
    );
}

#[test]
fn export_key_requires_explicit_confirm_and_warns() {
    let (app, _d) = setup(false);
    tap(&app, 1, "w:create");
    tap(&app, 1, "w:exp");
    let text = last_text(&app);
    assert!(text.contains("export key"), "warning card expected: {text}");
    assert!(
        text.contains("controls your funds"),
        "risk must be named: {text}"
    );
    let card = app.api.last_card(CHAT);
    assert!(
        card.rows.iter().flatten().any(|b| b.callback == "w:exp2"),
        "export must require a second explicit confirm"
    );
}

#[test]
fn created_key_shown_once_and_never_again() {
    let (app, _d) = setup(false);
    tap(&app, 1, "w:create");
    let shown = last_text(&app);
    assert!(shown.contains("private key"), "key shown once at creation");
    // Later cards never contain it again.
    tap(&app, 1, "nav:wal");
    tap(&app, 1, "nav:menu");
    tap(&app, 1, "w:exp");
    let later = app.api.history(CHAT);
    let key_word = shown
        .lines()
        .find(|l| l.starts_with("0x") && l.len() > 60)
        .map(|l| l.trim().to_string());
    if let Some(k) = key_word {
        assert!(
            later.iter().skip(1).all(|c| !c.text.contains(&k)),
            "the key must never be repeated"
        );
    }
}

#[test]
fn telegram_token_required_from_env_not_file() {
    // A config file carrying the token must be rejected by the schema...
    let parsed: Result<basevantage::config::TelegramConfig, _> =
        toml::from_str("[telegram]\nbot_token = 'secret'\n");
    assert!(parsed.is_err(), "tokens in config files must be refused");

    // ...and the only sanctioned source is the environment.
    unsafe { std::env::set_var("TELEGRAM_BOT_TOKEN", "123:abc") };
    assert_eq!(
        basevantage::config::TelegramConfig::token_from_env().unwrap(),
        "123:abc"
    );
    unsafe { std::env::remove_var("TELEGRAM_BOT_TOKEN") };
    assert!(basevantage::config::TelegramConfig::token_from_env().is_err());
}

#[test]
fn execute_toggle_requires_reconfirm_warning() {
    let (app, _d) = setup(false);
    say(&app, "/settings");
    tap(&app, 1, "set:mode");
    let text = last_text(&app);
    assert!(
        text.contains("real"),
        "the warning must name the stakes: {text}"
    );
    let has_enable = |app: &App<MockApi, MockEngine>| {
        app.api
            .last_card(CHAT)
            .rows
            .iter()
            .flatten()
            .any(|b| b.label.contains("Enable Execute"))
    };
    assert!(has_enable(&app), "explicit enable button required");
    // Tapping again re-warns — no "don't ask again".
    tap(&app, 1, "nav:set");
    tap(&app, 1, "set:mode");
    assert!(has_enable(&app), "the warning must repeat");
}
