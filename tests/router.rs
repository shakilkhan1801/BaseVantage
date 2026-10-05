//! `router::*` — best route by NET settlement-asset out; mixed quote assets
//! normalized, never compared raw; pin/re-validate at send.

mod common;

use alloy::primitives::U256;
use basevantage::router::quote::normalize;
use basevantage::router::PinnedQuote;

use common::*;

fn tokens(n: u64) -> U256 {
    U256::from(n) * U256::from(1_000_000_000_000_000_000u64)
}

/// Reserves chosen so the 2-hop route has the higher GROSS USDC out while the
/// direct route keeps the higher NET (extra-hop gas decides it).
#[tokio::test]
async fn best_route_max_net_usdc_out() {
    let (engine, market, _chain) = offline_engine();
    let key_direct = v2_key(POOL_A, TOKEN, USDC);
    let key_tw = v2_key(POOL_B, TOKEN, WETH);
    let key_wu = v2_key(POOL_C, WETH, USDC);
    market.inject_pools(TOKEN, vec![key_direct.clone(), key_tw.clone()]);
    market.inject_pools(WETH, vec![key_wu.clone()]);
    for key in [&key_direct, &key_tw, &key_wu] {
        market.inject_meta(
            (*key).clone(),
            basevantage::market::PoolMeta {
                decimals0: 18,
                decimals1: 18,
                stable: false,
                fee_bps: 30,
            },
        );
    }
      market.inject_state(key_direct.clone(), v2_state(1_000_000_000_000_000_000_000, 2_923_000_000), basevantage::market::Source::ChainRpc);
    market.inject_state(key_tw.clone(), v2_state(1_000_000_000_000_000_000_000, 1_000_000_000_000_000_000), basevantage::market::Source::ChainRpc);
    market.inject_state(key_wu.clone(), v2_state(100_000_000_000_000_000_000, 300_000_000_000), basevantage::market::Source::ChainRpc);

    let net = engine.net_inputs(0, U256::from(150_000)).await.unwrap();
    let outcomes = engine.router.quotes(TOKEN, tokens(100), &net).await.unwrap();

    let usdc_out: Vec<_> = outcomes
        .iter()
        .filter_map(|o| o.outcome.as_ref().ok())
        .filter(|q| q.quote_asset == USDC)
        .collect();
    let direct = usdc_out.iter().find(|q| q.route.hops.len() == 1).expect("direct route");
    let two_hop = usdc_out.iter().find(|q| q.route.hops.len() == 2).expect("2-hop route");

    // The whole point: gross ordering and net ordering disagree.
    assert!(
        two_hop.gross_out > direct.gross_out,
        "fixture must give 2-hop the higher gross ({}) vs direct ({})",
        two_hop.gross_out,
        direct.gross_out
    );
    assert!(
        two_hop.net_out < direct.net_out,
        "2-hop net ({}) must lose to direct net ({}) after gas",
        two_hop.net_out,
        direct.net_out
    );

    // Best route = max NET out across all candidates (normalized).
    let best = engine.router.best_route(TOKEN, tokens(100), &net).await.unwrap().expect("best");
    let max_net = outcomes
        .iter()
        .filter_map(|o| o.outcome.as_ref().ok())
        .map(|q| q.net_out)
        .max()
        .unwrap();
    assert_eq!(best.net_out, max_net, "best route must carry the maximum net");
    assert_ne!(
        best.route.hops.len(),
        two_hop.route.hops.len(),
        "the higher-gross route must lose"
    );
  }

#[tokio::test]
async fn mixed_quote_assets_normalized_never_raw_compared() {
    let (engine, market, _chain) = offline_engine();
    let key_direct = v2_key(POOL_A, TOKEN, USDC);
    let key_tw = v2_key(POOL_B, TOKEN, WETH);
    let key_wu = v2_key(POOL_C, WETH, USDC);
    market.inject_pools(TOKEN, vec![key_direct.clone(), key_tw.clone()]);
    market.inject_pools(WETH, vec![key_wu.clone()]);
    for key in [&key_direct, &key_tw, &key_wu] {
        market.inject_meta(
            (*key).clone(),
            basevantage::market::PoolMeta {
                decimals0: 18,
                decimals1: 18,
                stable: false,
                fee_bps: 30,
            },
        );
    }
    // Direct USDC pool is priced above the WETH route, but the WETH route's
    // RAW amount (18 decimals) dwarfs the USDC amount (6 decimals): a raw
    // comparison would pick the wrong route.
    market.inject_state(key_direct.clone(), v2_state(1_000_000_000_000_000_000_000, 3_080_000_000), basevantage::market::Source::ChainRpc);
    market.inject_state(key_tw.clone(), v2_state(1_000_000_000_000_000_000_000, 1_000_000_000_000_000_000), basevantage::market::Source::ChainRpc);
    market.inject_state(key_wu.clone(), v2_state(100_000_000_000_000_000_000, 300_000_000_000), basevantage::market::Source::ChainRpc);

    let net = engine.net_inputs(0, U256::from(150_000)).await.unwrap();
    let outcomes = engine.router.quotes(TOKEN, tokens(100), &net).await.unwrap();
    let quotes: Vec<_> = outcomes.iter().filter_map(|o| o.outcome.as_ref().ok()).collect();

    let weth_single = quotes
        .iter()
        .find(|q| q.quote_asset == WETH && q.route.hops.len() == 1)
        .expect("WETH-quoted candidate");
    let usdc_single = quotes
        .iter()
        .find(|q| q.quote_asset == USDC && q.route.hops.len() == 1)
        .expect("USDC-quoted candidate");

    // Raw comparison would pick the WETH route: 9e16 ≫ 2.8e8.
    assert!(
        weth_single.gross_out > usdc_single.gross_out,
        "fixture must make the raw comparison misleading ({} vs {})",
        weth_single.gross_out,
        usdc_single.gross_out
    );

    // Normalized into the settlement asset the ordering flips back.
    let normalized_weth = normalize(
        weth_single.gross_out,
        WETH,
        USDC,
        WETH,
        net.wrapped_native_price_1e18,
    )
    .unwrap();
    assert!(
        normalized_weth < usdc_single.gross_out,
        "normalized WETH route ({normalized_weth}) must lose to USDC route ({})",
        usdc_single.gross_out
    );
    assert_eq!(weth_single.normalized_out, normalized_weth);

    // And the router picks by normalized net, not by raw amount.
    let best = engine.router.best_route(TOKEN, tokens(100), &net).await.unwrap().expect("best");
    assert_eq!(best.quote_asset, USDC);
    assert_eq!(best.net_out, usdc_single.net_out);
}

#[test]
fn pin_revalidate_refuses_stale_or_regressed() {
    use std::time::Instant;

    let route = single_hop_route(v2_key(POOL_A, TOKEN, USDC), TOKEN, USDC, tokens(100));
    let fresh = quote_fixture(route.clone(), tokens(300), USDC, 0.1);
    let min_out = tokens(290);
    let pinned = fresh.pin(min_out);
    assert!(pinned.revalidate(quote_fixture(route.clone(), tokens(295), USDC, 0.1), std::time::Duration::from_secs(30)).is_ok());

    // Re-quote below the pinned floor: refuse.
    let regressed = quote_fixture(route.clone(), tokens(280), USDC, 0.1);
    assert!(pinned.revalidate(regressed, std::time::Duration::from_secs(30)).is_err());

    // Stale pin: refuse regardless of price.
    let mut stale = quote_fixture(route, tokens(300), USDC, 0.1);
    stale.pinned_at = Instant::now() - std::time::Duration::from_secs(60);
    let stale = PinnedQuote { quote: stale, min_out };
    assert!(stale
        .revalidate(quote_fixture(single_hop_route(v2_key(POOL_A, TOKEN, USDC), TOKEN, USDC, tokens(100)), tokens(300), USDC, 0.1), std::time::Duration::from_secs(30))
        .is_err());
}
