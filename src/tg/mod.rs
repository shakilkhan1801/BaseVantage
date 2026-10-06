//! The Telegram surface: a thin `TgApi` abstraction over the Bot API, the
//! real HTTP client, and a recording mock for tests. All rendering lives in
//! [`cards`]; all trading logic stays in the engine.

pub mod cards;
pub mod handlers;
pub mod notify;
pub mod port;
pub mod state;
pub mod store;

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{EngineError, Result};

/// One inline button: a label and the callback data it sends back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub label: String,
    pub callback: String,
}

impl Button {
    pub fn new(label: &str, callback: &str) -> Self {
        Self {
            label: label.to_string(),
            callback: callback.to_string(),
        }
    }
}

/// A rendered message: body text plus inline keyboard rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub text: String,
    pub rows: Vec<Vec<Button>>,
}

/// The bot's only output surface. Everything the user ever sees goes through
/// these three calls, which is what makes the panel behaviour testable.
#[async_trait]
pub trait TgApi: Send + Sync {
    /// Sends a new message; returns its message id.
    async fn send(&self, chat: i64, card: &Card) -> Result<i64>;
    /// Rewrites an existing message in place.
    async fn edit(&self, chat: i64, message_id: i64, card: &Card) -> Result<()>;
    /// Acknowledges a button press (optionally with a toast).
    async fn answer_callback(&self, callback_id: &str, toast: Option<&str>) -> Result<()>;
}

// ------------------------------------------------------- real Bot API client

#[derive(Debug, Clone, Deserialize)]
pub struct IncomingMessage {
    pub chat_id: i64,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IncomingCallback {
    pub id: String,
    pub chat_id: i64,
    pub message_id: i64,
    pub data: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub id: i64,
    #[serde(default)]
    pub message: Option<IncomingMessage>,
    #[serde(default)]
    pub callback: Option<IncomingCallback>,
}

/// Minimal Telegram Bot API client: long-poll `getUpdates` plus the three
/// write methods the bot needs. No framework, no hidden retries — the engine
/// layer owns retry policy.
pub struct TelegramApi {
    token: String,
    client: reqwest::Client,
}

impl TelegramApi {
    pub fn new(token: &str) -> Self {
        Self {
            token: token.to_string(),
            client: reqwest::Client::new(),
        }
    }

    async fn call(&self, method: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("https://api.telegram.org/bot{}/{}", self.token, method);
        let resp = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| EngineError::Rpc(format!("telegram: {e}")))?;
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EngineError::Rpc(format!("telegram: {e}")))?;
        if value.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Err(EngineError::Rpc(format!(
                "telegram {method}: {}",
                value
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
            )));
        }
        Ok(value
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    /// Long-polls for updates since `offset`.
    pub async fn get_updates(&self, offset: i64, timeout_secs: u32) -> Result<Vec<Update>> {
        let body = serde_json::json!({
            "offset": offset,
            "timeout": timeout_secs,
            "allowed_updates": ["message", "callback_query"],
        });
        let result = self.call("getUpdates", body).await?;
        serde_json::from_value(result)
            .map_err(|e| EngineError::Rpc(format!("telegram updates: {e}")))
    }
}

fn keyboard(rows: &[Vec<Button>]) -> serde_json::Value {
    let inline: Vec<Vec<serde_json::Value>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|b| serde_json::json!({ "text": b.label, "callback_data": b.callback }))
                .collect()
        })
        .collect();
    serde_json::json!({ "inline_keyboard": inline })
}

#[async_trait]
impl TgApi for TelegramApi {
    async fn send(&self, chat: i64, card: &Card) -> Result<i64> {
        let body = serde_json::json!({
            "chat_id": chat,
            "text": card.text,
            "reply_markup": keyboard(&card.rows),
        });
        let result = self.call("sendMessage", body).await?;
        result
            .get("message_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| EngineError::Rpc("telegram: no message id".to_string()))
    }

    async fn edit(&self, chat: i64, message_id: i64, card: &Card) -> Result<()> {
        let body = serde_json::json!({
            "chat_id": chat,
            "message_id": message_id,
            "text": card.text,
            "reply_markup": keyboard(&card.rows),
        });
        self.call("editMessageText", body).await?;
        Ok(())
    }

    async fn answer_callback(&self, callback_id: &str, toast: Option<&str>) -> Result<()> {
        let mut body = serde_json::json!({ "callback_query_id": callback_id });
        if let Some(t) = toast {
            body["text"] = serde_json::Value::String(t.to_string());
        }
        self.call("answerCallbackQuery", body).await?;
        Ok(())
    }
}

// -------------------------------------------------------------- test mock

/// Recording mock used by every tg test: captures cards instead of sending.
/// One chronological log of (chat, message id, card) — `message_id == 0`
/// marks a new message, otherwise an in-place edit — so `last_card` is
/// genuinely the latest panel the user would see.
#[derive(Default)]
pub struct MockApi {
    pub log: std::sync::Mutex<Vec<(i64, i64, Card)>>,
    pub answered: std::sync::Mutex<Vec<String>>,
    pub next_id: std::sync::atomic::AtomicI64,
}

impl MockApi {
    pub fn new() -> Self {
        Self::default()
    }

    /// All cards sent or edited to `chat`, in chronological order.
    pub fn history(&self, chat: i64) -> Vec<Card> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(c, _, _)| *c == chat)
            .map(|(_, _, card)| card.clone())
            .collect()
    }

    pub fn last_card(&self, chat: i64) -> Card {
        self.history(chat).pop().expect("no card sent to this chat")
    }

    pub fn sent_count(&self, chat: i64) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(c, mid, _)| *c == chat && *mid == 0)
            .count()
    }

    pub fn edit_count(&self) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, mid, _)| *mid != 0)
            .count()
    }
}

#[async_trait]
impl TgApi for MockApi {
    async fn send(&self, chat: i64, card: &Card) -> Result<i64> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.log.lock().unwrap().push((chat, 0, card.clone()));
        Ok(id)
    }

    async fn edit(&self, chat: i64, message_id: i64, card: &Card) -> Result<()> {
        self.log
            .lock()
            .unwrap()
            .push((chat, message_id, card.clone()));
        Ok(())
    }

    async fn answer_callback(&self, callback_id: &str, toast: Option<&str>) -> Result<()> {
        self.answered
            .lock()
            .unwrap()
            .push(toast.unwrap_or(callback_id).to_string());
        Ok(())
    }
}
