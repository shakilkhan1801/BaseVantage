//! The target-order watcher: polls the engine for open orders and fires when
//! a live route meets the order's limit. Firing always goes through the same
//! floor-checked execute path as a manual trade; when the wallet needs the
//! user's passphrase (or the bot is in observe mode), the watcher posts a
//! review prompt instead of sending.

use alloy::signers::local::PrivateKeySigner;

use crate::error::Result;
use crate::tg::port::{Draft, DraftKind, EnginePort};
use crate::tg::store::{OrderDirection, OrderStatus, Store};
use crate::tg::{Button, Card, TgApi};

/// One watcher pass over the open orders. Returns how many fired or prompted.
pub async fn poll_once<A: TgApi, E: EnginePort>(
    api: &A,
    engine: &E,
    store: &mut Store,
    execute_allowed: impl Fn(i64) -> bool,
) -> Result<usize> {
    let mut acted = 0;
    for order in store.watching_orders() {
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
            id: order.id,
            kind,
            token: order.token,
            symbol: order.symbol.clone(),
            settlement: order.settlement,
        };
        let quote = match engine.quote(&draft).await {
            Ok(q) => q,
            Err(_) => continue, // transient: retry next pass
        };
        if quote.limit_met != Some(true) {
            continue;
        }
        acted += 1;
        let direction = match order.direction {
            OrderDirection::Buy => "BUY",
            OrderDirection::Sell => "SELL",
        };
        let bound = match order.direction {
            OrderDirection::Buy => "paid ≤ limit",
            OrderDirection::Sell => "sold ≥ limit",
        };

        // Auto-fire only with execute mode + allowlist + a wallet that opens
        // without the user (no passphrase layer). Everything else prompts.
        let signer: Option<PrivateKeySigner> =
            if engine.mode_execute() && execute_allowed(order.chat) {
                store.open_wallet(order.chat, None).ok()
            } else {
                None
            };

        let Some(signer) = signer else {
            let card = Card {
                text: format!(
                    "BaseVantage · target reached\n{direction} {} · limit {}\nmode {} — review to send, or cancel.",
                    order.symbol,
                    crate::tg::cards::fmt_price(&order.limit_price_1e18),
                    if engine.mode_execute() {
                        "execute (passphrase required)"
                    } else {
                        "observe"
                    },
                ),
                rows: vec![vec![
                    Button::new("Review & Confirm", &format!("ord:fire:{}", order.id)),
                    Button::new("Cancel", &format!("ord:cancel:{}", order.id)),
                ]],
            };
            api.send(order.chat, &card).await?;
            continue;
        };

        match engine.execute(&draft, &signer).await {
            Ok(receipt) => {
                store.set_order_status(
                    order.id,
                    OrderStatus::Fired {
                        tx: receipt.tx_hash.clone(),
                    },
                )?;
                let card = Card {
                    text: format!(
                        "BaseVantage · target order filled\n{direction} {} · limit {}\n{}\n{}\ntx      {}",
                        order.symbol,
                        crate::tg::cards::fmt_price(&order.limit_price_1e18),
                        receipt.fill_line,
                        bound,
                        crate::tg::cards::short_addr(&receipt.tx_hash),
                    ),
                    rows: vec![vec![
                        Button::new("TOKEN ▸", &format!("tok:{}", order.token)),
                        Button::new("Orders", "nav:ord"),
                        Button::new("← Menu", "menu"),
                    ]],
                };
                api.send(order.chat, &card).await?;
            }
            Err(e) => {
                // The order stays watching; a worse fill reverted and retries.
                let card = Card {
                    text: format!(
                        "BaseVantage · target order retrying\n{direction} {} — fill refused: {e}\nthe order stays open; funds untouched.",
                        order.symbol
                    ),
                    rows: vec![vec![Button::new("Orders", "nav:ord")]],
                };
                api.send(order.chat, &card).await?;
            }
        }
    }
    Ok(acted)
}
