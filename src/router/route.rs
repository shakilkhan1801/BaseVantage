use alloy::primitives::{Address, U256};

use crate::error::{EngineError, Result};
use crate::market::PoolKey;

/// One pool crossing inside a route.
#[derive(Debug, Clone)]
pub struct Hop {
    pub pool: PoolKey,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub amount_out: U256,
}

/// An ordered chain of hops. Single-hop is `hops.len() == 1`.
#[derive(Debug, Clone)]
pub struct Route {
    pub hops: Vec<Hop>,
}

impl Route {
    pub fn token_in(&self) -> Address {
        self.hops[0].token_in
    }

    pub fn token_out(&self) -> Address {
        self.hops[self.hops.len() - 1].token_out
    }

    pub fn amount_in(&self) -> U256 {
        self.hops[0].amount_in
    }

    pub fn describe(&self) -> String {
        self.hops
            .iter()
            .map(|h| format!("{} {}", h.pool.venue.name(), h.pool.label()))
            .collect::<Vec<_>>()
            .join(" → ")
    }

    /// Multi-hop v3 legs are where fee-on-transfer accounting breaks
    /// exact-in assumptions.
    pub fn has_multihop_v3(&self) -> bool {
        self.hops.len() > 1
            && self
                .hops
                .iter()
                .any(|h| h.pool.venue == crate::venues::Venue::V3)
    }
}

/// Build one candidate route per (pool, pool) combination: direct pools from
/// sell token to the settlement asset, plus one-hop-through-hub candidates.
/// Single-hop routes that end in a hub token stay candidates too — the quote
/// normalizes them into the settlement asset before comparison.
pub fn candidate_routes(
    sell: Address,
    amount_in: U256,
    pools: &[PoolKey],
    settlement: Address,
    hubs: &[Address],
) -> Vec<Route> {
    let mut routes = Vec::new();

    let pools_from = |token: Address| -> Vec<PoolKey> {
        pools
            .iter()
            .filter(|p| p.token0 == token || p.token1 == token)
            .cloned()
            .collect()
    };

    // Direct: sell -> settlement, and sell -> hub (quote asset normalization).
    let mut direct_targets: Vec<Address> = vec![settlement];
    for hub in hubs {
        if *hub != settlement && !direct_targets.contains(hub) {
            direct_targets.push(*hub);
        }
    }
    for target in &direct_targets {
        for pool in pools_from(sell) {
            if pool.token0 == *target || pool.token1 == *target {
                routes.push(Route {
                    hops: vec![Hop {
                        token_in: sell,
                        token_out: *target,
                        amount_in,
                        amount_out: U256::ZERO,
                        pool,
                    }],
                });
            }
        }
    }

    // Multi-hop: sell -> hub -> settlement (hub != settlement).
    for hub in hubs {
        if *hub == settlement {
            continue;
        }
        for first in pools_from(sell) {
            if first.token0 != *hub && first.token1 != *hub {
                continue;
            }
            for second in pools_from(*hub) {
                if second.token0 != settlement && second.token1 != settlement {
                    continue;
                }
                if second.address == first.address {
                    continue;
                }
                routes.push(Route {
                    hops: vec![
                        Hop {
                            token_in: sell,
                            token_out: *hub,
                            amount_in,
                            amount_out: U256::ZERO,
                            pool: first.clone(),
                        },
                        Hop {
                            token_in: *hub,
                            token_out: settlement,
                            amount_in: U256::ZERO,
                            amount_out: U256::ZERO,
                            pool: second.clone(),
                        },
                    ],
                });
            }
        }
    }

    routes
}

pub fn require_hops(route: &Route) -> Result<()> {
    if route.hops.is_empty() {
        return Err(EngineError::NoRoute("empty route".to_string()));
    }
    for w in route.hops.windows(2) {
        if w[0].token_out != w[1].token_in {
            return Err(EngineError::NoRoute(
                "route legs do not connect".to_string(),
            ));
        }
    }
    Ok(())
}
