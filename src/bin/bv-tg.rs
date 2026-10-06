//! `bv-tg` — the BaseVantage Telegram bot: long-poll loop, target-order
//! watcher, and the engine wired through the real adapter.

use std::path::PathBuf;
use std::sync::Arc;

use basevantage::config::{Config, TelegramConfig};
use basevantage::harness::Engine;
use basevantage::tg::TelegramApi;
use basevantage::tg::adapter::EngineAdapter;
use basevantage::tg::handlers::App;
use basevantage::tg::notify;
use basevantage::tg::store::Store;

fn kek_from_env() -> Result<[u8; 32], Box<dyn std::error::Error>> {
    let raw = std::env::var("TG_WALLET_SECRETS_KEY")
        .map_err(|_| "TG_WALLET_SECRETS_KEY must come from the environment")?;
    let bytes = hex::decode(raw.trim()).map_err(|_| "TG_WALLET_SECRETS_KEY must be hex")?;
    if bytes.len() != 32 {
        return Err("TG_WALLET_SECRETS_KEY must be 32 bytes".into());
    }
    let mut kek = [0u8; 32];
    kek.copy_from_slice(&bytes);
    Ok(kek)
}

fn telegram_section(path: &PathBuf) -> TelegramConfig {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("telegram").cloned())
        .and_then(|t| t.try_into::<TelegramConfig>().ok())
        .unwrap_or_else(|| toml::from_str::<TelegramConfig>("").expect("defaults"))
}

// The store lock is intentionally held across the watcher await: the binary
// runs one sequential task, so nothing ever contends for that guard.
#[allow(clippy::await_holding_lock)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let token = TelegramConfig::token_from_env()?;
    let kek = kek_from_env()?;
    let config_path = PathBuf::from(
        std::env::var("BASEVANTAGE_CONFIG").unwrap_or_else(|_| "config.toml".to_string()),
    );
    let tg_config = telegram_section(&config_path);
    let engine = Engine::boot(Config::load(config_path)?).await?;
    let store = Store::load(&PathBuf::from("tg-state.json"), Some(kek))?;
    let allowlist = if tg_config.execute_allowlist.is_empty() {
        None
    } else {
        Some(tg_config.execute_allowlist.clone())
    };
    let app = Arc::new(App::new(
        TelegramApi::new(&token),
        EngineAdapter::new(engine.clone()),
        store,
        allowlist,
    ));
    engine.spawn_event_updates();

    // One loop: a watcher pass per iteration, then the long poll. Keeping
    // both on this task avoids holding the store lock across tasks.
    let api = TelegramApi::new(&token);
    let mut offset = 0i64;
    loop {
        // Safe to hold the store lock across this await: the binary runs one
        // sequential task — the watcher pass and the handler calls below can
        // never interleave, so nothing contends for the guard.
        let mut store_guard = app.store.lock().unwrap();
        let watcher = notify::poll_once(&app.api, &app.engine, &mut store_guard, |_| true);
        if let Err(e) = watcher.await {
            tracing::warn!("watcher pass: {e}");
        }
        drop(store_guard);
        let updates = match api.get_updates(offset, 30).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("getUpdates: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
        };
        for update in updates {
            offset = offset.max(update.id + 1);
            if let Some(msg) = update.message
                && let Some(text) = msg.text
                && let Err(e) = app.handle_message(msg.chat_id, &text).await
            {
                tracing::warn!("message: {e}");
            }
            if let Some(cb) = update.callback
                && let Err(e) = app
                    .handle_callback(cb.chat_id, cb.message_id, &cb.id, &cb.data)
                    .await
            {
                tracing::warn!("callback: {e}");
            }
        }
    }
}
