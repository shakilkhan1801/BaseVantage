use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use async_trait::async_trait;

use crate::chain::{BaseChain, ChainAdapter, DynChain};
use crate::config::{Config, Mode};
use crate::error::{EngineError, Result};
use crate::market::pool_state::spawn_event_updates;
use crate::market::{MarketData, NoopStatsSource, PoolKey, Registry, TtlConfig};
use crate::router::{NetInputs, Route, Router};
use crate::safety::floor::{AnchorKind, FloorAnchor, FloorModule};
use crate::safety::{
    AssessmentSource, ProbeAssessor, SafetyPolicy, TaxOracle, TokenAssessment, Verdict,
};
use crate::watchlist::Watchlist;

/// Effective mode banner, printed at boot and on every command.
pub fn mode_banner(mode: Mode) -> &'static str {
    mode.banner()
}

/// The assembled engine.
pub struct Engine {
    pub chain: DynChain,
    pub market: Arc<MarketData>,
    pub router: Router,
    pub policy: SafetyPolicy,
    pub floor: FloorModule,
    pub assessor: Arc<dyn TaxOracle>,
    pub manual: Arc<crate::safety::ManualAssessor>,
    pub watchlist: Arc<Watchlist>,
    pub config: Config,
}

/// Manual assessments overlay the probe assessor.
struct OverlayAssessor {
    manual: Arc<crate::safety::ManualAssessor>,
    probe: ProbeAssessor,
}

#[async_trait]
impl TaxOracle for OverlayAssessor {
    async fn assess(&self, token: Address, pools: &[PoolKey]) -> Result<Option<TokenAssessment>> {
        if let Some(hit) = self.manual.assess(token, pools).await? {
            return Ok(Some(hit));
        }
        self.probe.assess(token, pools).await
    }
}

/// Result of a route execution simulation with settlement.
#[derive(Debug)]
pub struct SimResult {
    pub settled: U256,
    pub settlement_asset: Address,
    pub wrapped_native_residue: U256,
    pub hop_amounts: Vec<U256>,
    pub min_out: U256,
    pub impact_pct: f64,
    pub verdict: Verdict,
}

impl Engine {
    /// Boot: config already validated fail-fast at load. Benches the RPC pool
    /// and restores the persisted static cache tier.
    pub async fn boot(config: Config) -> Result<Arc<Engine>> {
        let ws_urls = config.rpc.ws_urls.clone();
        let chain: Arc<BaseChain> = Arc::new(BaseChain::new(
            &config.rpc.urls,
            ws_urls,
            config.rpc.bench_interval_secs,
        )?);
        let _bench = chain.bench().await;

        let registry = Registry::new(
            chain.clone() as DynChain,
            crate::venues::v2::V2_FACTORY,
            crate::venues::v3::V3_FACTORY,
            crate::venues::aerodrome::AERO_FACTORY,
            crate::venues::v4::POOL_MANAGER,
        );
        let ttls = TtlConfig {
            static_ttl: Duration::from_secs(config.cache.static_ttl_secs),
            reserves_ttl: Duration::from_secs(config.cache.reserves_ttl_secs),
            stats_ttl: Duration::from_secs(config.cache.stats_ttl_secs),
            negative_ttl: Duration::from_secs(config.cache.negative_ttl_secs),
        };
        let market = MarketData::new(
            chain.clone() as DynChain,
            registry,
            ttls,
            Arc::new(NoopStatsSource),
            config.cache.static_store_path.clone(),
        );
        market.load_static_snapshot(Duration::from_secs(config.cache.static_ttl_secs));

        let router = Router::new(
            market.clone(),
            config.settlement_asset(),
            config.wrapped_native(),
        );
        let manual = Arc::new(crate::safety::ManualAssessor::new());
        let probe = ProbeAssessor::new(chain.clone() as DynChain);
        let assessor = Arc::new(OverlayAssessor {
            manual: manual.clone(),
            probe,
        });
        let watchlist = Arc::new(Watchlist::with_json_file(
            config.watchlist.cap,
            config.watchlist.store_path.clone(),
        ));

        Ok(Arc::new(Engine {
            chain: chain.clone() as DynChain,
            market,
            router,
            policy: SafetyPolicy {
                max_sell_tax_pct: config.safety.max_sell_tax_pct,
                impact_cap_pct: config.safety.impact_cap_pct,
                floor_tolerance_pct: config.safety.floor_tolerance_pct,
                refuse_fot_multihop_v3: config.safety.fot_multihop_v3 == "refuse",
            },
            floor: FloorModule::new(config.safety.floor_tolerance_pct),
            assessor,
            manual,
            watchlist,
            config,
        }))
    }

    /// Keep the market cache event-fresh (WS-first, RPC fallback).
    pub fn spawn_event_updates(self: &Arc<Engine>) -> tokio::task::JoinHandle<()> {
        spawn_event_updates(self.market.clone(), self.chain.clone(), Default::default())
    }

    /// Net accounting inputs, with the live wrapped-native price.
    pub async fn net_inputs(&self, sell_tax_bps: u32, gas_units: U256) -> Result<NetInputs> {
        let price = self.wrapped_native_price().await?;
        Ok(NetInputs {
            sell_tax_bps,
            gas_units,
            gas_price_wei: U256::from(50_000_000_000u64),
            wrapped_native_price_1e18: price,
        })
    }

    /// Wrapped-native price in the settlement asset, 1e18-scaled, from the
    /// deepest direct pool.
    pub async fn wrapped_native_price(&self) -> Result<U256> {
        let wn = self.config.wrapped_native();
        let settle = self.config.settlement_asset();
        let pools = self
            .market
            .pools_for(wn, &[settle])
            .await?
            .map(|h| h.value.as_ref().clone())
            .unwrap_or_default();
        let mut best: Option<(U256, u128)> = None;
        for key in pools {
            if key.other(wn) != settle {
                continue;
            }
            if let Some(state) = self.market.pool_state(&key).await? {
                let zfo = key.zero_for_one(wn);
                let price = crate::venues::marginal_price_1e18(&state.value, zfo)?;
                let depth = match &*state.value {
                    crate::venues::PoolState::V2(s) => s.reserve0.to::<u128>(),
                    crate::venues::PoolState::Aero(s) => s.reserve0.to::<u128>(),
                    crate::venues::PoolState::V3(s) => s.liquidity,
                    crate::venues::PoolState::V4(s) => s.base.liquidity,
                };
                if best.is_none_or(|(_, d)| depth > d) {
                    best = Some((price, depth));
                }
            }
        }
        best.map(|(p, _)| p).ok_or_else(|| {
            EngineError::Quote("no wrapped-native/settlement pool for pricing".to_string())
        })
    }
}

/// Selling-side floor anchors for a route: REFERENCE from the deepest
/// canonical pool of the sold token, SWAP from the crossed route price, and
/// TARGET when the operator supplies a target price.
fn floor_anchors(
    sell_amount_raw: U256,
    reference_price_1e18: Option<U256>,
    swap_price_1e18: U256,
    target_price_1e18: Option<U256>,
) -> Vec<FloorAnchor> {
    let mut anchors = vec![FloorAnchor {
        kind: AnchorKind::Swap,
        price_1e18: swap_price_1e18,
    }];
    if let Some(p) = reference_price_1e18 {
        anchors.push(FloorAnchor {
            kind: AnchorKind::Reference,
            price_1e18: p,
        });
    }
    if let Some(p) = target_price_1e18 {
        anchors.push(FloorAnchor {
            kind: AnchorKind::Target,
            price_1e18: p,
        });
    }
    let _ = sell_amount_raw;
    anchors
}

impl Engine {
    /// Assess a token through the overlay oracle (manual wins, then probes).
    pub async fn assessment(&self, token: Address) -> Result<Option<TokenAssessment>> {
        let pools = self
            .market
            .pools_for(
                token,
                &[self.config.wrapped_native(), self.config.settlement_asset()],
            )
            .await?
            .map(|h| h.value.as_ref().clone())
            .unwrap_or_default();
        self.assessor.assess(token, &pools).await
    }

    /// Deepest-pool reference price of `token` in the settlement asset.
    pub async fn reference_price(&self, token: Address) -> Option<U256> {
        let settle = self.config.settlement_asset();
        let wn = self.config.wrapped_native();
        let hit = self
            .market
            .pools_for(token, &[settle, wn])
            .await
            .ok()
            .flatten();
        let pools = hit.map(|h| h.value.as_ref().clone()).unwrap_or_default();
        let mut best: Option<(U256, u128)> = None;
        for key in pools {
            let state = self.market.pool_state(&key).await.ok().flatten()?;
            let zfo = key.zero_for_one(token);
            let other = key.other(token);
            let price = crate::venues::marginal_price_1e18(&state.value, zfo).ok()?;
            let price = if other == settle {
                price
            } else {
                let wn_price = self.wrapped_native_price().await.ok()?;
                crate::venues::mul_div_floor(price, wn_price, U256::from(10).pow(U256::from(18)))
                    .ok()?
            };
            let depth = match &*state.value {
                crate::venues::PoolState::V2(s) => s.reserve0.to::<u128>(),
                crate::venues::PoolState::Aero(s) => s.reserve0.to::<u128>(),
                crate::venues::PoolState::V3(s) => s.liquidity,
                crate::venues::PoolState::V4(s) => s.base.liquidity,
            };
            if best.is_none_or(|(_, d)| depth > d) {
                best = Some((price, depth));
            }
        }
        best.map(|(p, _)| p)
    }

    /// Execute a route in simulation and settle everything into the
    /// settlement asset. The invariant: wrapped-native residue is exactly 0.
    pub async fn simulate(
        &self,
        route: &Route,
        net: &NetInputs,
        target_price_1e18: Option<U256>,
    ) -> Result<SimResult> {
        let quote = self.router.quote_route(route, net).await?;
        let settlement = self.config.settlement_asset();
        let wrapped_native = self.config.wrapped_native();

        let mut balances: BTreeMap<Address, U256> = BTreeMap::new();
        *balances.entry(route.token_in()).or_insert(U256::ZERO) += route.amount_in();
        let mut hop_amounts = Vec::new();
        for hop in &quote.route.hops {
            let held = balances.entry(hop.token_in).or_insert(U256::ZERO);
            *held = held.saturating_sub(hop.amount_in);
            *balances.entry(hop.token_out).or_insert(U256::ZERO) += hop.amount_out;
            hop_amounts.push(hop.amount_out);
        }

        let mut settled = balances.remove(&settlement).unwrap_or(U256::ZERO);
        let mut residue: BTreeMap<Address, U256> = BTreeMap::new();
        let pool_list = self
            .market
            .pools_for(wrapped_native, &[settlement])
            .await?
            .map(|h| h.value.as_ref().clone())
            .unwrap_or_default();
        for (asset, amount) in balances.iter() {
            if amount.is_zero() {
                continue;
            }
            if *asset == settlement {
                settled += *amount;
                continue;
            }
            match self
                .settle_asset_into(*asset, *amount, settlement, &pool_list)
                .await
            {
                Ok(out) => settled += out,
                Err(_) => {
                    residue.insert(*asset, *amount);
                }
            }
        }
        let wrapped_native_residue = residue.get(&wrapped_native).copied().unwrap_or(U256::ZERO);

        let reference = self.reference_price(route.token_in()).await;
        let normalized = if quote.quote_asset == settlement {
            quote.gross_out
        } else {
            let price = self.wrapped_native_price().await?;
            crate::venues::mul_div_floor(
                quote.gross_out,
                price,
                U256::from(10).pow(U256::from(18)),
            )?
        };
        let swap_price = crate::venues::mul_div_floor(
            normalized,
            U256::from(10).pow(U256::from(18)),
            route.amount_in(),
        )?;
        let anchors = floor_anchors(route.amount_in(), reference, swap_price, target_price_1e18);
        let floor_res = self.floor.min_out(route.amount_in(), &anchors)?;

        let assessment = self
            .assessment(route.token_in())
            .await?
            .unwrap_or_else(|| crate::safety::benign_assessment(route.token_in()));
        let verdict = self
            .policy
            .pre_send_gate(&quote, &assessment, floor_res.min_out);

        Ok(SimResult {
            settled,
            settlement_asset: settlement,
            wrapped_native_residue,
            hop_amounts,
            min_out: floor_res.min_out,
            impact_pct: quote.impact_pct,
            verdict,
        })
    }

    async fn settle_asset_into(
        &self,
        asset: Address,
        amount: U256,
        settlement: Address,
        wn_pools: &[PoolKey],
    ) -> Result<U256> {
        if asset == settlement {
            return Ok(amount);
        }
        let key = if asset == self.config.wrapped_native() {
            wn_pools
                .iter()
                .find(|p| p.other(asset) == settlement)
                .cloned()
                .ok_or_else(|| {
                    EngineError::Settlement("no wrapped-native settlement pool".into())
                })?
        } else {
            let pools = self
                .market
                .pools_for(asset, &[settlement])
                .await?
                .map(|h| h.value.as_ref().clone())
                .unwrap_or_default();
            pools
                .into_iter()
                .find(|p| p.other(asset) == settlement)
                .ok_or_else(|| EngineError::Settlement("no direct settlement pool".into()))?
        };
        let state = self
            .market
            .pool_state(&key)
            .await?
            .ok_or_else(|| EngineError::Settlement("no state for settlement pool".into()))?;
        let zfo = key.zero_for_one(asset);
        let quoter = crate::router::quoter_for(key.venue);
        quoter.quote_exact_in(&state.value, zfo, amount)
    }

    /// `bv quote` output.
    pub async fn cmd_quote(&self, sell: Address, amount_raw: U256) -> Result<String> {
        let assessment = self.assessment(sell).await?;
        let gate = assessment
            .as_ref()
            .map(|a| self.policy.assess_gate(a))
            .unwrap_or(Verdict::Allow);
        if !gate.is_allow() {
            return Ok(format!("safety    {}", gate.label()));
        }
        let tax_bps = assessment.as_ref().map(|a| a.sell_tax_bps).unwrap_or(0);
        let net = self.net_inputs(tax_bps, U256::from(150_000)).await?;
        let Some(quote) = self.router.best_route(sell, amount_raw, &net).await? else {
            return Err(EngineError::NoRoute("no viable route".to_string()));
        };
        let reference = self.reference_price(sell).await;
        let normalized = quote.normalized_out;
        let swap_price = crate::venues::mul_div_floor(
            normalized,
            U256::from(10).pow(U256::from(18)),
            amount_raw,
        )?;
        let anchors = floor_anchors(amount_raw, reference, swap_price, None);
        let floor_res = self.floor.min_out(amount_raw, &anchors)?;

        let mut out = String::new();
        out.push_str(&format!("route     {}\n", quote.route.describe()));
        out.push_str(&format!(
            "gross     {}\n",
            fmt_amount(quote.gross_out, &self.token_symbol(quote.quote_asset))
        ));
        out.push_str(&format!(
            "net       {}   (tax {} bps, gas {} wei settle)\n",
            fmt_amount(quote.net_out, &self.token_symbol(quote.settlement_asset)),
            tax_bps,
            quote.gas_in_settle
        ));
        out.push_str(&format!("impact    {:.2}%\n", quote.impact_pct));
        out.push_str(&format!(
            "floor     REFERENCE {} · SWAP {} → min-out {}\n",
            reference
                .map(|p| (p / U256::from(10).pow(U256::from(12))).to_string())
                .unwrap_or_else(|| "—".into()),
            swap_price / U256::from(10).pow(U256::from(12)),
            floor_res.min_out
        ));
        out.push_str(&format!("safety    {}\n", gate.label()));
        out.push_str(&format!("source    {}\n", quote.labels.join(" · ")));
        out.push_str(self.config.effective_mode().banner());
        Ok(out)
    }

    /// `bv route-list` output.
    pub async fn cmd_route_list(&self, sell: Address, amount_raw: U256) -> Result<String> {
        let assessment = self.assessment(sell).await?;
        let tax_bps = assessment.as_ref().map(|a| a.sell_tax_bps).unwrap_or(0);
        let net = self.net_inputs(tax_bps, U256::from(150_000)).await?;
        let outcomes = self.router.quotes(sell, amount_raw, &net).await?;
        let mut ranked: Vec<_> = outcomes
            .into_iter()
            .filter_map(|o| match o.outcome {
                Ok(q) => Some((q, o.route)),
                Err(_) => None,
            })
            .collect();
        ranked.sort_by_key(|a| std::cmp::Reverse(a.0.net_out));

        let mut out =
            String::from("#  net(settle out)   route                                  verdict\n");
        for (i, (quote, route)) in ranked.iter().enumerate() {
            let mut verdict = if i == 0 {
                "best".to_string()
            } else {
                "ok".to_string()
            };
            if let Some(a) = &assessment {
                let gate = self.policy.assess_gate(a);
                if !gate.is_allow() {
                    verdict = gate.label();
                } else if self.policy.refuse_fot_multihop_v3
                    && a.fee_on_transfer
                    && route.has_multihop_v3()
                {
                    verdict = "refused: fee-on-transfer multi-hop v3 leg".to_string();
                } else {
                    let cap = crate::safety::impact::ImpactCap::new(self.policy.impact_cap_pct);
                    let v = cap.check(quote.impact_pct);
                    if !v.is_allow() {
                        verdict = v.label();
                    }
                }
            }
            out.push_str(&format!(
                "{}  {:>18}   {:<36} {}\n",
                i + 1,
                quote.net_out,
                route.describe(),
                verdict
            ));
        }
        out.push_str(self.config.effective_mode().banner());
        Ok(out)
    }

    /// `bv dossier-data` output.
    pub async fn cmd_dossier(&self, token: Address) -> Result<String> {
        let mut out = String::new();
        let info = self.market.token(token).await?;
        let symbol = info
            .as_ref()
            .map(|l| l.value.symbol.clone())
            .unwrap_or_else(|| "???".to_string());
        let decimals = info.as_ref().map(|l| l.value.decimals).unwrap_or(18);
        out.push_str(&format!(
            "token      {symbol} ({})  decimals {decimals}\n",
            token
        ));
        match self.assessment(token).await? {
            Some(a) => out.push_str(&format!(
                "tax        buy {} bps · sell {} bps · honeypot {} ({}{})\n",
                a.buy_tax_bps,
                a.sell_tax_bps,
                if a.honeypot { "YES" } else { "no" },
                match a.source {
                    AssessmentSource::Probe => "sim probe",
                    AssessmentSource::Manual => "manual",
                },
                a.probe_block
                    .map(|b| format!(" block {b}"))
                    .unwrap_or_default()
            )),
            None => out.push_str("tax        unassessed (no probeable pool)\n"),
        }
        let pools = self
            .market
            .pools_for(
                token,
                &[self.config.wrapped_native(), self.config.settlement_asset()],
            )
            .await?
            .map(|h| h.value.as_ref().clone())
            .unwrap_or_default();
        let mut pool_lines = Vec::new();
        for key in pools.iter().take(6) {
            if let Some(state) = self.market.pool_state(key).await? {
                pool_lines.push(format!("{} {}", key.venue.name(), state.label()));
            }
        }
        out.push_str(&format!("pools      {}\n", pool_lines.join("  ·  ")));
        match self.market.stats_for(token).await? {
            Some(stats) => out.push_str(&format!(
                "stats      vol24h {} · txns {} · holders {}\n",
                stats.value.volume_24h, stats.value.txns_24h, stats.value.holders
            )),
            None => out.push_str("stats      none (external source not configured)\n"),
        }
        if let Some(l) = &info {
            out.push_str(&format!("source     {}\n", l.label()));
        }
        out.push_str(self.config.effective_mode().banner());
        Ok(out)
    }

    /// `bv simulate` output for one route.
    pub async fn cmd_simulate(
        &self,
        sell: Address,
        amount_raw: U256,
        route_index: usize,
        target_price_1e18: Option<U256>,
    ) -> Result<String> {
        let assessment = self.assessment(sell).await?;
        let tax_bps = assessment.as_ref().map(|a| a.sell_tax_bps).unwrap_or(0);
        let net = self.net_inputs(tax_bps, U256::from(150_000)).await?;
        let candidates = self.router.candidates(sell, amount_raw).await?;
        let route = candidates
            .get(route_index.saturating_sub(1))
            .ok_or_else(|| EngineError::NoRoute(format!("no route #{route_index}")))?;
        let sim = self.simulate(route, &net, target_price_1e18).await?;
        let settle_symbol = self.token_symbol(sim.settlement_asset);
        let mut out = String::new();
        out.push_str(&format!(
            "settle     {settle_symbol}  {}\n",
            fmt_amount(sim.settled, &settle_symbol)
        ));
        out.push_str(&format!(
            "residue    WETH {}  {}\n",
            sim.wrapped_native_residue,
            if sim.wrapped_native_residue.is_zero() {
                "✓"
            } else {
                "✗"
            }
        ));
        out.push_str(&format!(
            "floor      min-out {} (target anchor {})\n",
            sim.min_out,
            if target_price_1e18.is_some() {
                "supplied"
            } else {
                "none"
            }
        ));
        out.push_str(&format!(
            "impact     {:.2}% (cap {:.2}%)\n",
            sim.impact_pct, self.policy.impact_cap_pct
        ));
        out.push_str(&format!(
            "verdict    {} (sim only — {})",
            sim.verdict.label(),
            self.config.effective_mode().banner()
        ));
        Ok(out)
    }

    fn token_symbol(&self, asset: Address) -> String {
        if asset == self.config.settlement_asset() {
            "USDC".to_string()
        } else if asset == self.config.wrapped_native() {
            "WETH".to_string()
        } else {
            "TOKEN".to_string()
        }
    }
}

/// Grouped decimal formatting for CLI amounts.
pub fn fmt_amount(raw: U256, _symbol: &str) -> String {
    let s = raw.to_string();
    if s.len() <= 3 {
        return s;
    }
    let mut out = String::new();
    let bytes = s.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*c as char);
    }
    out
}
