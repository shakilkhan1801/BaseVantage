//! The engine surface the Telegram layer consumes, kept as a trait so every
//! bot test runs against a mock engine and the real adapter stays thin.

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;

use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftKind {
    /// Buy: spend this much settlement asset.
    Buy { spend: U256 },
    /// Sell: sell this many tokens (1e18 whole units).
    Sell { amount: U256 },
    /// Target buy: spend `spend` at limit price.
    TargetBuy { spend: U256, limit_price_1e18: U256 },
    /// Target sell: sell `amount` at limit price.
    TargetSell {
        amount: U256,
        limit_price_1e18: U256,
    },
}

/// A pending trade under construction. `id` names this draft in confirm
/// callbacks so a double tap cannot confirm anything twice.
#[derive(Debug, Clone)]
pub struct Draft {
    pub id: u64,
    pub kind: DraftKind,
    pub token: Address,
    pub symbol: String,
    pub settlement: Address,
}

/// Everything the token card shows, straight from the engine.
#[derive(Debug, Clone)]
pub struct TokenView {
    pub symbol: String,
    pub price_line: String,
    pub pools_line: String,
    pub dossier_line: String,
    pub impact_line: String,
    pub verdict_line: String,
    pub blocked: bool,
    pub block_reason: String,
    pub holds: U256,
}

/// Everything the review card shows.
#[derive(Debug, Clone)]
pub struct QuoteView {
    pub title: String,
    pub route: String,
    pub gross_line: String,
    pub net_line: String,
    pub min_out_line: String,
    pub anchor_line: String,
    pub impact_line: String,
    pub verdict_ok: bool,
    pub refusal_reason: String,
    pub refusal_guidance: String,
    /// For target orders: `Some(true)` when a live route meets the limit
    /// (buy at ≤ limit, sell at ≥ limit). `None` for ordinary trades.
    pub limit_met: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ReceiptView {
    pub title: String,
    pub fill_line: String,
    pub impact_line: String,
    pub tx_hash: String,
    pub verdict_line: String,
}

#[derive(Debug, Clone)]
pub struct PositionView {
    pub token: Address,
    pub symbol: String,
    pub amount: U256,
    pub value_line: String,
    pub pnl_line: String,
}

#[async_trait]
pub trait EnginePort: Send + Sync {
    /// Token card data; `wallet` supplies holdings for the sell buttons.
    async fn token(&self, token: Address, wallet: Option<Address>) -> Result<TokenView>;
    /// Builds the review data for a draft (quote + floor + safety).
    async fn quote(&self, draft: &Draft) -> Result<QuoteView>;
    /// Signs and sends when in execute mode. An engine refusal comes back as
    /// `Err` with the reason; observe mode must refuse to send.
    async fn execute(&self, draft: &Draft, signer: &PrivateKeySigner) -> Result<ReceiptView>;
    /// Holdings + valuations for the positions screen.
    async fn positions(&self, wallet: Address) -> Result<Vec<PositionView>>;
    /// Withdraws settlement asset to an external address; returns the tx hash.
    async fn withdraw(
        &self,
        signer: &PrivateKeySigner,
        to: Address,
        amount: U256,
    ) -> Result<String>;
    /// Sells/buys are shown against these settings.
    fn mode_execute(&self) -> bool;
    /// Settlement-asset symbol for card labels (USDC/ETH).
    fn settlement_symbol(&self) -> &'static str;
    /// The settlement asset address (USDC/ETH placeholder).
    fn settlement(&self) -> Address;
}
