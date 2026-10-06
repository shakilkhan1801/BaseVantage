//! `sim::*` — a multi-hop sell settles USDC with wrapped-native residue 0.

mod common;

use alloy::primitives::U256;
use basevantage::market::Source;

use common::*;

fn tokens(n: u64) -> U256 {
    U256::from(n) * U256::from(1_000_000_000_000_000_000u64)
}

fn inject_route_pools(market: &basevantage::market::MarketData) {
    let key_tw = v2_key(POOL_A, TOKEN, WETH);
    let key_wu = v2_key(POOL_B, WETH, USDC);
    market.inject_pools(TOKEN, vec![key_tw.clone()]);
    market.inject_pools(WETH, vec![key_wu.clone()]);
    for key in [&key_tw, &key_wu] {
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
    market.inject_state(
        key_tw,
        v2_state(1_000_000_000_000_000_000_000, 1_000_000_000_000_000_000),
        Source::ChainRpc,
    );
    market.inject_state(
        key_wu,
        v2_state(100_000_000_000_000_000_000, 300_000_000_000),
        Source::ChainRpc,
    );
}

#[tokio::test]
async fn multihop_sell_settles_usdc_and_wrapped_native_residue_zero() {
    let (engine, market, _chain) = offline_engine();
    inject_route_pools(&market);

    let key_tw = v2_key(POOL_A, TOKEN, WETH);
    let key_wu = v2_key(POOL_B, WETH, USDC);
    let net = engine.net_inputs(0, U256::from(150_000)).await.unwrap();

    // Multi-hop sell TOKEN -> WETH -> USDC.
    let route = two_hop_route(
        key_tw.clone(),
        key_wu.clone(),
        TOKEN,
        WETH,
        USDC,
        tokens(100),
    );
    let sim = engine.simulate(&route, &net, None).await.expect("sim runs");

    assert!(sim.settled > U256::ZERO, "settlement must land in USDC");
    assert_eq!(sim.settlement_asset, USDC);
    assert_eq!(
        sim.wrapped_native_residue,
        U256::ZERO,
        "multi-hop sell must leave exactly zero wrapped-native residue"
    );
    assert_eq!(sim.hop_amounts.len(), 2);
    assert!(
        sim.hop_amounts[1] > U256::ZERO,
        "second hop must deliver USDC"
    );

    // A route that ENDS in wrapped native settles it fully into USDC too:
    // settlement never strands WETH dust.
    let single = single_hop_route(key_tw, TOKEN, WETH, tokens(100));
    let sim = engine
        .simulate(&single, &net, None)
        .await
        .expect("sim runs");
    assert!(sim.settled > U256::ZERO, "settled proceeds must be USDC");
    assert_eq!(
        sim.wrapped_native_residue,
        U256::ZERO,
        "settlement must convert the full wrapped-native balance"
    );
}
