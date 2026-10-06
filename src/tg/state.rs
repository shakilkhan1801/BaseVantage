//! Per-chat state: which panel is showing, what draft is pending, and which
//! text prompt the next message answers. Transitions are driven by handlers.

use alloy::primitives::{Address, U256};
use std::time::Instant;

use crate::tg::port::{Draft, DraftKind};

/// What the next typed message means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextPrompt {
    /// Custom buy amount (settlement asset, whole units).
    BuyAmount {
        token: Address,
        symbol: String,
    },
    /// Custom sell amount (tokens, whole units).
    SellAmount {
        token: Address,
        symbol: String,
    },
    /// Target order amount (sell: tokens; buy: spend), awaiting its price.
    TargetAmount {
        token: Address,
        symbol: String,
        direction_buy: bool,
    },
    /// Target order limit price (settlement asset per token, whole units).
    TargetPrice {
        token: Address,
        symbol: String,
        direction_buy: bool,
        amount: U256,
    },
    ImportKey,
    WithdrawAddress,
    WithdrawAmount {
        to: Address,
    },
}

/// A draft moving toward a review, with its confirm expiry.
#[derive(Debug, Clone)]
pub struct PendingConfirm {
    pub draft: Draft,
    pub message_id: i64,
    pub expires_at: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct ChatState {
    /// Token currently open (the back-navigation target).
    pub current_token: Option<Address>,
    pub prompt: Option<TextPrompt>,
    pub pending: Option<PendingConfirm>,
    /// Set when a target order flow awaits its price.
    pub target_draft: Option<(DraftKind, Address, String)>,
}

impl ChatState {
    pub fn arm_confirm(&mut self, draft: Draft, message_id: i64, window_secs: u64) {
        self.pending = Some(PendingConfirm {
            draft,
            message_id,
            expires_at: Instant::now() + std::time::Duration::from_secs(window_secs),
        });
    }

    /// Returns the expired draft, if any, and clears it.
    pub fn take_expired(&mut self) -> Option<PendingConfirm> {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| Instant::now() >= p.expires_at)
        {
            self.pending.take()
        } else {
            None
        }
    }

    /// Consumes the pending confirm only if it is the one being confirmed.
    /// A second tap therefore finds nothing and sends nothing.
    pub fn take_pending(&mut self, draft_id: u64) -> Option<PendingConfirm> {
        match &self.pending {
            Some(p) if p.draft.id == draft_id => self.pending.take(),
            _ => None,
        }
    }
}
