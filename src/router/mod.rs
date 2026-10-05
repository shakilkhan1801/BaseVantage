use std::sync::Arc;

use alloy::primitives::{Address, U256};

pub mod quote;
pub mod route;

pub use quote::{impact_pct, net_out, normalize, NetInputs, PinnedQuote, Quote};
pub use route::{candidate_routes, require_hops, Hop, Route};

use crate::error::{EngineError, Result};
use crate::market::{Labeled, MarketData};
use crate::venues::{marginal_price_1e18, PoolState as VenueState, VenueQuoter};

/// A candidate route plus its quote outcome (or refusal reason).
#[derive(Debug)]
pub struct RouteOutcome {
    pub route: Route,
    pub outcome: std::result::Result<Quote, String>,
}

pub fn quoter_for(venue: crate::venues::Venue) -> &'static dyn VenueQuoter {
    use crate::venues::{AerodromeVenue, V2Venue, V3Venue, V4Venue, Venue};
    match venue {
        Venue::V2 => &V2Venue,
        Venue::V3 => &V3Venue,
        Venue::V4 => &V4Venue,
        Venue::Aerodrome => &AerodromeVenue,
    }
}

/// The router: candidate generation, quoting, and best-route selection by
/// NET settlement-asset out.
pub struct Router {
    market: Arc<MarketData>,
    settlement_asset: Address,
    wrapped_native: Address,
    hubs: Vec<Address>,
}

impl Router {
    pub fn new(market: Arc<MarketData>, settlement_asset: Address, wrapped_native: Address) -> Self {
        Self {
            market,
            settlement_asset,
            wrapped_native,
            hubs: vec![wrapped_native],
        }
    }

    /// Extra hub tokens considered for intermediate hops and normalization.
    pub fn with_hubs(mut self, hubs: Vec<Address>) -> Self {
        self.hubs.extend(hubs);
        self.hubs.retain(|h| *h != self.settlement_asset);
        self
    }

    /// Every pool relevant to selling `sell`: sell↔hubs, sell↔settlement,
    /// hub↔settlement.
    async fn relevant_pools(&self, sell: Address) -> Result<Vec<crate::market::PoolKey>> {
        let mut hubs_and_settlement = self.hubs.clone();
        if !hubs_and_settlement.contains(&self.settlement_asset) {
            hubs_and_settlement.push(self.settlement_asset);
        }
        let mut pools = Vec::new();
        if let Some(hit) = self.market.pools_for(sell, &hubs_and_settlement).await? {
            pools.extend(hit.value.iter().cloned());
        }
        for hub in &self.hubs {
            if let Some(hit) = self.market.pools_for(*hub, &[self.settlement_asset]).await? {
                pools.extend(hit.value.iter().cloned());
            }
        }
        pools.sort_by(|a, b| {
            (a.venue, a.address, a.fee).cmp(&(b.venue, b.address, b.fee))
        });
        pools.dedup_by(|a, b| a == b);
        Ok(pools)
    }

    pub async fn candidates(&self, sell: Address, amount_in: U256) -> Result<Vec<Route>> {
        if amount_in.is_zero() {
            return Err(EngineError::NoRoute("zero input".to_string()));
        }
        let pools = self.relevant_pools(sell).await?;
        Ok(candidate_routes(
            sell,
            amount_in,
            &pools,
            self.settlement_asset,
            &self.hubs,
        ))
    }

    /// Quote one route: hop-by-hop exact-in quotes, then gross→net
    /// accounting and impact.
    pub async fn quote_route(&self, route: &Route, net: &NetInputs) -> Result<Quote> {
        require_hops(route)?;
        let mut route = route.clone();
        let mut labels = Vec::new();
        let mut spot_out: U256 = route.amount_in();

        for i in 0..route.hops.len() {
            let hop = &route.hops[i];
            let state_hit: Labeled<crate::venues::PoolState> = self
                .market
                .pool_state(&hop.pool)
                .await?
                .ok_or_else(|| EngineError::Quote(format!("no state for pool {}", hop.pool.label())))?;
            labels.push(state_hit.label());
            let state: &VenueState = &state_hit.value;
            let zero_for_one = hop.pool.zero_for_one(hop.token_in);
            let amount_in = hop.amount_in;
            let quoter = quoter_for(hop.pool.venue);
            let amount_out = quoter.quote_exact_in(state, zero_for_one, amount_in)?;
            route.hops[i].amount_out = amount_out;
            if i + 1 < route.hops.len() {
                route.hops[i + 1].amount_in = amount_out;
            }
            let price = marginal_price_1e18(state, zero_for_one)?;
            spot_out = crate::venues::mul_div_floor(spot_out, price, E18)?;
        }

        let last = &route.hops[route.hops.len() - 1];
        let gross_out = last.amount_out;
        let quote_asset = last.token_out;
        // Each extra hop costs real gas; multi-hop must not look free.
        let route_net = NetInputs {
            gas_units: net.gas_units + U256::from(50_000) * U256::from(route.hops.len().saturating_sub(1)),
            ..net.clone()
        };
        let (normalized, tax, gas_in_settle, net_value) = net_out(
            gross_out,
            quote_asset,
            self.settlement_asset,
            self.wrapped_native,
            net.wrapped_native_price_1e18,
            &route_net,
        )?;
        let impact = impact_pct(spot_out, gross_out);

          Ok(Quote {
            route,
            gross_out,
            quote_asset,
            settlement_asset: self.settlement_asset,
            normalized_out: normalized,
            tax,
            gas_in_settle,
            net_out: net_value,
            impact_pct: impact,
            spot_out,
            gas_estimate: route_net.gas_units,
            pinned_at: std::time::Instant::now(),
            labels,
        })
    }

    /// Quote every candidate, keeping refusals with their reason.
    pub async fn quotes(&self, sell: Address, amount_in: U256, net: &NetInputs) -> Result<Vec<RouteOutcome>> {
        let candidates = self.candidates(sell, amount_in).await?;
        let mut outcomes = Vec::with_capacity(candidates.len());
        for route in candidates {
            let outcome = self.quote_route(&route, net).await.map_err(|e| e.to_string());
            outcomes.push(RouteOutcome { route, outcome });
        }
        Ok(outcomes)
    }

    /// Best route = max NET settlement-asset out. Assets are normalized into
    /// the settlement asset first; raw amounts across assets are never
    /// compared.
    pub async fn best_route(
        &self,
        sell: Address,
        amount_in: U256,
        net: &NetInputs,
    ) -> Result<Option<Quote>> {
        let mut best: Option<Quote> = None;
        for outcome in self.quotes(sell, amount_in, net).await? {
            if let Ok(quote) = outcome.outcome {
                match &best {
                    Some(b) if b.net_out >= quote.net_out => {}
                    _ => best = Some(quote),
                }
            }
        }
        Ok(best)
    }
}

const E18: U256 = U256::from_limbs([0x0de0_b6b3_a764_0000, 0, 0, 0]);
