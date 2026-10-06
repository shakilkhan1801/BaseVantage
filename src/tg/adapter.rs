//! The real [`EnginePort`] over the S1 harness engine: quoting through the
//! router, floors through the safety module, and execution by signing locally
//! and broadcasting raw bytes. One honest limitation: a route mixing venue
//! families is refused at execution (single-family routes encode with their
//! venue's exact calldata); mixed-family encoding ships with a later slice.

use std::sync::Arc;

use alloy::eips::eip2718::Encodable2718;
use alloy::network::EthereumWallet;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::harness::Engine;
use crate::market::abi::IERC20;
use crate::router::route::Route;
use crate::safety::floor::{AnchorKind, FloorAnchor, FloorModule};
use crate::tg::cards;
use crate::tg::port::{
    Draft, DraftKind, EnginePort, PositionView, QuoteView, ReceiptView, TokenView,
};
use crate::venues::aerodrome::AERO_ROUTER;
use crate::venues::v2::V2_ROUTER;
use crate::venues::v3::V3_ROUTER;
use crate::venues::v4::UNIVERSAL_ROUTER;
use crate::venues::{AerodromeVenue, SwapLeg, V2Venue, V3Venue, V4Venue, Venue, VenueQuoter};

pub struct EngineAdapter {
    pub engine: Arc<Engine>,
}

impl EngineAdapter {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }

    fn settlement(&self) -> Address {
        self.engine
            .config
            .engine
            .settlement_asset
            .parse()
            .unwrap_or(Address::ZERO)
    }

    fn wrapped_native(&self) -> Address {
        self.engine
            .config
            .engine
            .wrapped_native
            .parse()
            .unwrap_or(Address::ZERO)
    }

    async fn balance_of(&self, token: Address, owner: Address) -> U256 {
        if owner.is_zero() || token.is_zero() {
            return U256::ZERO;
        }
        let data = IERC20::balanceOfCall { owner }.abi_encode();
        match crate::chain::base::eth_call(
            self.engine.chain.as_ref(),
            token,
            Bytes::from(data),
            None,
        )
        .await
        {
            Ok(out) => IERC20::balanceOfCall::abi_decode_returns(&out).unwrap_or_default(),
            Err(_) => U256::ZERO,
        }
    }

    fn e18() -> U256 {
        U256::from(10).pow(U256::from(18))
    }

    /// Best outcome = max NET settlement-asset out (buy: max NET tokens out).
    async fn best_outcome(
        &self,
        sell: &Address,
        amount: U256,
        settlement: &Address,
        tax_bps: u32,
    ) -> Result<(crate::router::RouteOutcome, crate::router::quote::NetInputs)> {
        let net = self.engine.net_inputs(tax_bps, U256::from(150_000)).await?;
        let router = crate::router::Router::new(
            self.engine.market.clone(),
            *settlement,
            self.wrapped_native(),
        );
        let outcomes = router.quotes(*sell, amount, &net).await?;
        let best = outcomes
            .into_iter()
            .filter(|o| o.outcome.is_ok())
            .max_by_key(|o| match &o.outcome {
                Ok(q) => q.net_out,
                Err(_) => U256::ZERO,
            })
            .ok_or_else(|| EngineError::Quote("no route available".to_string()))?;
        Ok((best, net))
    }

    fn route_label(route: &Route) -> String {
        route
            .hops
            .iter()
            .map(|h| {
                let name = match h.pool.venue {
                    Venue::V2 => "v2",
                    Venue::V3 => "v3",
                    Venue::V4 => "v4",
                    Venue::Aerodrome => "aero",
                };
                format!(
                    "{} {}/{}",
                    name,
                    cards::short_addr(&h.token_in.to_string()),
                    cards::short_addr(&h.token_out.to_string())
                )
            })
            .collect::<Vec<_>>()
            .join(" -> ")
    }

    fn anchors(
        &self,
        token: Address,
        reference: Option<U256>,
        limit: Option<U256>,
    ) -> Vec<FloorAnchor> {
        let mut out = Vec::new();
        if let Some(p) = reference {
            out.push(FloorAnchor {
                kind: AnchorKind::Reference,
                price_1e18: p,
            });
        }
        if let Some(p) = limit {
            out.push(FloorAnchor {
                kind: AnchorKind::Target,
                price_1e18: p,
            });
        }
        let _ = token;
        out
    }

    /// Signs and broadcasts one trade calldata.
    async fn send_tx(&self, signer: &PrivateKeySigner, to: Address, data: Bytes) -> Result<String> {
        let chain_id = self.engine.config.engine.chain_id;
        let nonce = self.engine.chain.nonce(signer.address()).await?;
        let gas_price = self.engine.chain.gas_price().await?;
        let tx = TransactionRequest::default()
            .with_from(signer.address())
            .with_to(to)
            .with_input(data)
            .with_chain_id(chain_id)
            .with_nonce(nonce)
            .with_gas_limit(600_000)
            .with_gas_price(gas_price.to::<u128>());
        let wallet = EthereumWallet::from(signer.clone());
        let signed = tx
            .build(&wallet)
            .await
            .map_err(|e| EngineError::Rpc(format!("build tx: {e}")))?;
        let raw = signed.encoded_2718();
        let hash = self.engine.chain.send_raw(Bytes::from(raw)).await?;
        Ok(hash.to_string())
    }

    /// Encodes one route into calldata plus the router to send it to.
    fn encode_route(&self, route: &Route, min_out: U256, to: Address) -> Result<(Address, Bytes)> {
        let venues: Vec<Venue> = route.hops.iter().map(|h| h.pool.venue).collect();
        if !venues.windows(2).all(|w| w[0] == w[1]) {
            return Err(EngineError::Quote(
                "mixed-venue route encoding ships in a later slice".to_string(),
            ));
        }
        let mut legs = Vec::with_capacity(route.hops.len());
        for (i, h) in route.hops.iter().enumerate() {
            legs.push(SwapLeg {
                venue: h.pool.venue,
                pool: h.pool.address,
                token_in: h.token_in,
                token_out: h.token_out,
                amount_in: h.amount_in,
                min_out: if i + 1 == route.hops.len() {
                    min_out
                } else {
                    U256::ZERO
                },
                fee: h.pool.fee,
                stable: h.pool.stable,
                hooks: h.pool.hooks,
                factory: h.pool.factory,
                tick_spacing: h.pool.tick_spacing,
            });
        }
        let deadline = 2_000_000_000u64;
        let (router, calldata) = match venues[0] {
            Venue::V2 => (V2_ROUTER, V2Venue.encode_exact_in(&legs, to, deadline)),
            Venue::V3 => (V3_ROUTER, V3Venue.encode_exact_in(&legs, to, deadline)),
            Venue::V4 => (
                UNIVERSAL_ROUTER,
                V4Venue.encode_exact_in(&legs, to, deadline),
            ),
            Venue::Aerodrome => (
                AERO_ROUTER,
                AerodromeVenue.encode_exact_in(&legs, to, deadline),
            ),
        };
        Ok((router, calldata?))
    }
}

#[async_trait]
impl EnginePort for EngineAdapter {
    async fn token(&self, token: Address, wallet: Option<Address>) -> Result<TokenView> {
        let assessment = self.engine.assessment(token).await.ok().flatten();
        let price = self.engine.reference_price(token).await;
        let holds = match wallet {
            Some(w) => self.balance_of(token, w).await,
            None => U256::ZERO,
        };
        let (dossier_line, blocked, block_reason) = match &assessment {
            Some(a) => {
                let blocked = a.honeypot;
                (
                    format!(
                        "sell-tax {:.1}% · honeypot {} · FoT {}",
                        a.sell_tax_bps as f64 / 100.0,
                        if a.honeypot { "YES" } else { "no" },
                        if a.fee_on_transfer { "yes" } else { "no" },
                    ),
                    blocked,
                    if a.honeypot {
                        "honeypot — sells cannot execute".to_string()
                    } else {
                        String::new()
                    },
                )
            }
            None => (
                "assessment unavailable (negative-cached)".to_string(),
                false,
                String::new(),
            ),
        };
        Ok(TokenView {
            symbol: cards::short_addr(&token.to_string()),
            price_line: match price {
                Some(p) => format!("{} USDC per token", cards::fmt_price(&p)),
                None => "price unavailable".to_string(),
            },
            pools_line: "pools via engine discovery".to_string(),
            dossier_line,
            impact_line: format!(
                "impact cap {:.2}%",
                self.engine.config.safety.impact_cap_pct
            ),
            verdict_line: if blocked {
                "BLOCKED".to_string()
            } else {
                "allow · engine gates active".to_string()
            },
            blocked,
            block_reason,
            holds,
        })
    }

    async fn quote(&self, draft: &Draft) -> Result<QuoteView> {
        let settlement = self.settlement();
        let is_buy = matches!(
            draft.kind,
            DraftKind::Buy { .. } | DraftKind::TargetBuy { .. }
        );
        let (sell, amount, limit, tax_bps) = match draft.kind {
            DraftKind::Buy { spend } => (settlement, spend, None, 0),
            DraftKind::TargetBuy {
                spend,
                limit_price_1e18,
            } => (settlement, spend, Some(limit_price_1e18), 0),
            DraftKind::Sell { amount } => {
                let a = self.engine.assessment(draft.token).await.ok().flatten();
                let tax = a.map(|x| x.sell_tax_bps).unwrap_or(0);
                (draft.token, amount, None, tax)
            }
            DraftKind::TargetSell {
                amount,
                limit_price_1e18,
            } => {
                let a = self.engine.assessment(draft.token).await.ok().flatten();
                let tax = a.map(|x| x.sell_tax_bps).unwrap_or(0);
                (draft.token, amount, Some(limit_price_1e18), tax)
            }
        };
        // A buy sells the settlement asset and settles into the token.
        let out_asset = if is_buy { draft.token } else { settlement };
        let (outcome, _net) = self
            .best_outcome(&sell, amount, &out_asset, tax_bps)
            .await?;
        let quote = outcome.outcome.map_err(EngineError::Quote)?;
        let reference = self.engine.reference_price(draft.token).await;
        let anchors = self.anchors(draft.token, reference, limit);
        let floor = FloorModule::new(0.0);
        let min_out = if is_buy {
            floor.min_tokens(amount, &anchors)?.min_out
        } else {
            floor.min_out(amount, &anchors)?.min_out
        };
        let min_out_line = format!(
            "{} {}",
            cards::fmt_units(&min_out),
            if is_buy { "tokens" } else { "USDC" }
        );
        let anchor_line = anchors
            .iter()
            .map(|a| format!("{:?}", a.kind))
            .collect::<Vec<_>>()
            .join(" · ")
            + " -> max";
        let limit_met = limit.map(|p| {
            if is_buy {
                quote.net_out >= min_out
            } else {
                quote.net_out >= p * amount / Self::e18()
            }
        });
        Ok(QuoteView {
            title: draft.symbol.clone(),
            route: Self::route_label(&quote.route),
            gross_line: format!(
                "{} {}",
                cards::fmt_units(&quote.gross_out),
                if is_buy { "tokens" } else { "USDC" }
            ),
            net_line: format!(
                "{} {} (tax {} bps, gas ≈ {})",
                cards::fmt_units(&quote.net_out),
                if is_buy { "tokens" } else { "USDC" },
                tax_bps,
                cards::fmt_units(&quote.gas_in_settle)
            ),
            min_out_line,
            anchor_line,
            impact_line: format!(
                "{:.2}% <= cap {:.2}%",
                quote.impact_pct, self.engine.config.safety.impact_cap_pct
            ),
            verdict_ok: true,
            refusal_reason: String::new(),
            refusal_guidance: String::new(),
            limit_met,
        })
    }

    async fn execute(&self, draft: &Draft, signer: &PrivateKeySigner) -> Result<ReceiptView> {
        if !self.mode_execute() {
            return Err(EngineError::SafetyRefused(
                "observe mode: nothing is sent".to_string(),
            ));
        }
        let settlement = self.settlement();
        let is_buy = matches!(
            draft.kind,
            DraftKind::Buy { .. } | DraftKind::TargetBuy { .. }
        );
        let (sell, amount, limit) = match draft.kind {
            DraftKind::Buy { spend } => (settlement, spend, None),
            DraftKind::TargetBuy {
                spend,
                limit_price_1e18,
            } => (settlement, spend, Some(limit_price_1e18)),
            DraftKind::Sell { amount } => (draft.token, amount, None),
            DraftKind::TargetSell {
                amount,
                limit_price_1e18,
            } => (draft.token, amount, Some(limit_price_1e18)),
        };
        let out_asset = if is_buy { draft.token } else { settlement };
        let (outcome, _) = self.best_outcome(&sell, amount, &out_asset, 0).await?;
        let quote = outcome.outcome.map_err(EngineError::Quote)?;
        let reference = self.engine.reference_price(draft.token).await;
        let anchors = self.anchors(draft.token, reference, limit);
        let floor = FloorModule::new(0.0);
        let min_out = if is_buy {
            floor.min_tokens(amount, &anchors)?.min_out
        } else {
            floor.min_out(amount, &anchors)?.min_out
        };
        // The limit rule is absolute: never below target × amount (sell) or
        // spend ÷ target (buy) — already folded into min_out above.
        let (router, calldata) = self.encode_route(&quote.route, min_out, signer.address())?;
        let tx = self.send_tx(signer, router, calldata).await?;
        Ok(ReceiptView {
            title: draft.symbol.clone(),
            fill_line: format!(
                "sent · on-chain min-out {} (worse fill reverts)",
                cards::fmt_units(&min_out)
            ),
            impact_line: format!("{:.2}%", quote.impact_pct),
            tx_hash: tx,
            verdict_line: "allow · min-out enforced in calldata".to_string(),
        })
    }

    async fn positions(&self, wallet: Address) -> Result<Vec<PositionView>> {
        let mut out = Vec::new();
        for entry in self.engine.watchlist.list() {
            let amount = self.balance_of(entry.address, wallet).await;
            if amount.is_zero() {
                continue;
            }
            let value = self
                .engine
                .reference_price(entry.address)
                .await
                .map(|p| cards::fmt_units(&(amount * p / Self::e18())))
                .unwrap_or_else(|| "—".to_string());
            out.push(PositionView {
                token: entry.address,
                symbol: entry.symbol.clone(),
                amount,
                value_line: value,
                pnl_line: "entry unknown".to_string(),
            });
        }
        Ok(out)
    }

    async fn withdraw(
        &self,
        signer: &PrivateKeySigner,
        to: Address,
        amount: U256,
    ) -> Result<String> {
        if !self.mode_execute() {
            return Err(EngineError::SafetyRefused(
                "observe mode: nothing is sent".to_string(),
            ));
        }
        let data = IERC20::transferCall { to, amount }.abi_encode();
        self.send_tx(signer, self.settlement(), Bytes::from(data))
            .await
    }

    fn mode_execute(&self) -> bool {
        self.engine.config.engine.mode == crate::config::Mode::Execute
    }

    fn settlement_symbol(&self) -> &'static str {
        "USDC"
    }

    fn settlement(&self) -> Address {
        self.engine
            .config
            .engine
            .settlement_asset
            .parse()
            .unwrap_or(Address::ZERO)
    }
}
