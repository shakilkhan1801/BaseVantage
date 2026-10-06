//! `market::*` — repeat quotes cost ≈ 0 RPC (cache + single-flight).

mod common;

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, Bytes, U256, keccak256};
use basevantage::chain::testing::{CountingAdapter, ScriptedChain};
use basevantage::chain::{DynChain, PoolEvent};
use basevantage::market::{MarketData, NoopStatsSource, PoolMeta, Source, TtlConfig};
use basevantage::router::Router;
use std::path::PathBuf;

use common::*;

fn get_reserves_selector() -> [u8; 4] {
    let h = keccak256("getReserves()");
    [h[0], h[1], h[2], h[3]]
}

fn encode_reserves(r0: u128, r1: u128) -> Bytes {
    let mut out = Vec::with_capacity(96);
    out.extend_from_slice(&U256::from(r0).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(r1).to_be_bytes::<32>());
    out.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
    Bytes::from(out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeat_quote_near_zero_rpc_cache_and_single_flight() {
    let selector = get_reserves_selector();
    let scripted = ScriptedChain::new(move |req| {
        let data = req.data.clone().unwrap_or_default();
        if data.len() >= 4 && data[..4] == selector {
            // Slow response on purpose: concurrent misses must share it.
            std::thread::sleep(Duration::from_millis(100));
            return Ok(encode_reserves(
                1_000_000_000_000_000_000_000,
                3_000_000_000_000,
            ));
        }
        panic!("unexpected call in offline test: {data:?}")
    });
    let counting = CountingAdapter::new(scripted as DynChain);

    let ttls = TtlConfig {
        static_ttl: Duration::from_secs(3600),
        reserves_ttl: Duration::from_secs(3600),
        stats_ttl: Duration::from_secs(3600),
        negative_ttl: Duration::from_secs(3600),
    };
    let market = MarketData::new(
        counting.clone() as DynChain,
        basevantage::market::Registry::new(
            counting.clone() as DynChain,
            Address::ZERO,
            Address::ZERO,
            Address::ZERO,
            Address::ZERO,
        ),
        ttls,
        Arc::new(NoopStatsSource),
        PathBuf::from("/nonexistent/static-pools.json"),
    );

    let key_ab = v2_key(POOL_A, TOKEN, WETH);
    let key_bc = v2_key(POOL_B, WETH, USDC);
    market.inject_pools(TOKEN, vec![key_ab.clone()]);
    market.inject_pools(WETH, vec![key_bc.clone()]);
    for key in [&key_ab, &key_bc] {
        market.inject_meta(
            (*key).clone(),
            PoolMeta {
                decimals0: 18,
                decimals1: 18,
                stable: false,
                fee_bps: 30,
            },
        );
    }
    market.inject_state(
        key_ab.clone(),
        v2_state(1_000_000_000_000_000_000_000, 3_000_000_000_000),
        Source::ChainRpc,
    );
    market.inject_state(
        key_bc.clone(),
        v2_state(100_000_000_000_000_000_000, 300_000_000_000),
        Source::ChainRpc,
    );

    let router = Router::new(market.clone(), USDC, WETH);
    let net = net_inputs();
    let amount = U256::from(100_000_000_000_000_000_000u128); // 100 TOKEN

    // 50 sequential repeat quotes: pure cache reads.
    for _ in 0..50 {
        router
            .best_route(TOKEN, amount, &net)
            .await
            .expect("quote")
            .expect("a route");
    }
    // 50 concurrent repeat quotes on the same keys: still pure cache reads.
    let results =
        futures::future::join_all((0..50).map(|_| router.best_route(TOKEN, amount, &net))).await;
    for r in results {
        r.expect("concurrent quote").expect("a route");
    }

    assert_eq!(
        counting.call_count(),
        0,
        "100 repeat quotes must add zero RPC calls beyond the warm cache"
    );

    // Invalidation (e.g. a WS event) forces exactly one refetch even under
    // concurrent readers: single-flight shares the in-flight request.
    market.apply_event(&PoolEvent::SlotUpdate {
        pool: POOL_A,
        block: 1,
    });
    let results = futures::future::join_all((0..50).map(|_| market.pool_state(&key_ab))).await;
    for r in results {
        r.expect("state fetch").expect("state exists");
    }
    assert_eq!(
        counting.call_count(),
        1,
        "50 concurrent misses must share one RPC fetch"
    );

    // And the refetched value is cached again.
    market
        .pool_state(&key_ab)
        .await
        .expect("cached")
        .expect("state exists");
    assert_eq!(
        counting.call_count(),
        1,
        "subsequent reads hit the cache again"
    );
}
