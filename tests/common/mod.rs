//! Shared fixtures for engine tests. No network: everything is built from
//! explicit values so the invariants under test are isolated.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Instant;

use alloy::primitives::{Address, U256, address};

use basevantage::chain::DynChain;
use basevantage::chain::testing::ScriptedChain;
use basevantage::config::Config;
use basevantage::harness::Engine;
use basevantage::market::{MarketData, NoopStatsSource, PoolKey, TtlConfig};
use basevantage::router::{Hop, NetInputs, Quote, Route, Router};
use basevantage::safety::{ManualAssessor, SafetyPolicy};
use basevantage::venues::{PoolState, TickData, V2State, V3State, Venue};
use basevantage::watchlist::{MemoryStore, Watchlist};
use std::path::PathBuf;
use std::time::Duration;

pub const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
pub const WETH: Address = address!("4200000000000000000000000000000000000006");
pub const TOKEN: Address = address!("1111111111111111111111111111111111111111");
pub const POOL_A: Address = address!("2222222222222222222222222222222222222222");
pub const POOL_B: Address = address!("3333333333333333333333333333333333333333");
pub const POOL_C: Address = address!("4444444444444444444444444444444444444444");

pub const CONFIG_TOML: &str = r#"
[engine]
mode = "observe"
chain_id = 8453
settlement_asset = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
wrapped_native = "0x4200000000000000000000000000000000000006"

[rpc]
urls = ["http://localhost:0"]
bench_interval_secs = 60

[cache]
static_ttl_secs = 86400
reserves_ttl_secs = 30
stats_ttl_secs = 60
negative_ttl_secs = 120
static_store_path = "cache/static-pools.json"

[safety]
impact_cap_pct = 1.5
floor_tolerance_pct = 0.5
max_sell_tax_pct = 10.0
fot_multihop_v3 = "refuse"

[watchlist]
cap = 50
store_path = "watchlist.json"

[settlement]
pin_max_age_secs = 30
"#;

pub fn test_config() -> Config {
    Config::from_toml_str(CONFIG_TOML).expect("fixture config valid")
}

pub fn v2_key(pool: Address, token0: Address, token1: Address) -> PoolKey {
    PoolKey {
        venue: Venue::V2,
        address: pool,
        token0,
        token1,
        fee: 30,
        tick_spacing: 0,
        hooks: Address::ZERO,
        stable: false,
        factory: Address::ZERO,
        v4_pool_id: None,
    }
}

pub fn v3_key(pool: Address, token0: Address, token1: Address, fee: u32) -> PoolKey {
    PoolKey {
        venue: Venue::V3,
        address: pool,
        token0,
        token1,
        fee,
        tick_spacing: 60,
        hooks: Address::ZERO,
        stable: false,
        factory: Address::ZERO,
        v4_pool_id: None,
    }
}

pub fn v2_state(reserve0: u128, reserve1: u128) -> PoolState {
    PoolState::V2(V2State {
        reserve0: U256::from(reserve0),
        reserve1: U256::from(reserve1),
        fee_bps: 30,
    })
}

/// Minimal one-tick-range v3 state: current tick 0, one wide range.
pub fn v3_state(sqrt_price_x96: U256, liquidity: u128, fee_pips: u32) -> PoolState {
    PoolState::V3(V3State {
        sqrt_price_x96,
        liquidity,
        tick: 0,
        fee_pips,
        tick_spacing: 60,
        fee_pips_by_dir: None,
        ticks: vec![TickData {
            tick: -887_272,
            liquidity_net: liquidity as i128,
            liquidity_gross: liquidity,
        }],
        ticks_complete: true,
    })
}

/// A no-network market wired to a scripted chain that must stay silent.
pub fn offline_market() -> (Arc<MarketData>, Arc<ScriptedChain>) {
    let chain = ScriptedChain::new(|_| panic!("offline market must not make RPC calls"));
    let ttls = TtlConfig {
        static_ttl: Duration::from_secs(3600),
        reserves_ttl: Duration::from_secs(3600),
        stats_ttl: Duration::from_secs(3600),
        negative_ttl: Duration::from_secs(3600),
    };
    let market = MarketData::new(
        chain.clone() as DynChain,
        basevantage::market::Registry::new(
            chain.clone() as DynChain,
            Address::ZERO,
            Address::ZERO,
            Address::ZERO,
            Address::ZERO,
        ),
        ttls,
        Arc::new(NoopStatsSource),
        PathBuf::from("/nonexistent/static-pools.json"),
    );
    (market, chain)
}

pub fn single_hop_route(
    pool: PoolKey,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Route {
    Route {
        hops: vec![Hop {
            pool,
            token_in,
            token_out,
            amount_in,
            amount_out: U256::ZERO,
        }],
    }
}

pub fn two_hop_route(
    first: PoolKey,
    second: PoolKey,
    sell: Address,
    mid: Address,
    settle: Address,
    amount_in: U256,
) -> Route {
    Route {
        hops: vec![
            Hop {
                pool: first,
                token_in: sell,
                token_out: mid,
                amount_in,
                amount_out: U256::ZERO,
            },
            Hop {
                pool: second,
                token_in: mid,
                token_out: settle,
                amount_in: U256::ZERO,
                amount_out: U256::ZERO,
            },
        ],
    }
}

/// Quote fixture with explicitly chosen numbers.
pub fn quote_fixture(route: Route, gross_out: U256, quote_asset: Address, impact: f64) -> Quote {
    Quote {
        route,
        gross_out,
        quote_asset,
        settlement_asset: USDC,
        normalized_out: gross_out,
        tax: U256::ZERO,
        gas_in_settle: U256::ZERO,
        net_out: gross_out,
        impact_pct: impact,
        spot_out: gross_out,
        gas_estimate: U256::from(150_000),
        pinned_at: Instant::now(),
        labels: Vec::new(),
    }
}

pub fn net_inputs() -> NetInputs {
    NetInputs {
        sell_tax_bps: 0,
        gas_units: U256::from(150_000),
        gas_price_wei: U256::from(50_000_000_000u64),
        // 3000 USDC (6 dec) per WETH (18 dec), 1e18-scaled raw ratio:
        // 3000e6 per 1e18 wei => 3000e6 * 1e18 / 1e18 = 3e9
        wrapped_native_price_1e18: U256::from(3_000_000_000u64),
    }
}

pub fn safety_policy() -> SafetyPolicy {
    SafetyPolicy {
        max_sell_tax_pct: 10.0,
        impact_cap_pct: 1.5,
        floor_tolerance_pct: 0.5,
        refuse_fot_multihop_v3: true,
    }
}

/// Offline engine: scripted chain, injected market state, manual assessor.
pub fn offline_engine() -> (Engine, Arc<MarketData>, Arc<ScriptedChain>) {
    let (market, chain) = offline_market();
    let config = test_config();
    let router = Router::new(market.clone(), USDC, WETH);
    let manual = Arc::new(ManualAssessor::new());
    let watchlist = Arc::new(Watchlist::new(50, Box::<MemoryStore>::default()));
    let engine = Engine {
        chain: chain.clone() as DynChain,
        market: market.clone(),
        router,
        policy: safety_policy(),
        floor: basevantage::safety::FloorModule::new(0.5),
        assessor: manual.clone(),
        manual,
        watchlist,
        config,
    };
    (engine, market, chain)
}

// ------------------------------------------------------- telegram mock engine

use alloy::signers::local::PrivateKeySigner;
use basevantage::tg::port::{
    Draft, DraftKind, EnginePort, PositionView, QuoteView, ReceiptView, TokenView,
};

/// 1e18-scaled whole units for telegram tests.
pub fn tk(n: u64) -> alloy::primitives::U256 {
    alloy::primitives::U256::from(n)
        * alloy::primitives::U256::from(10).pow(alloy::primitives::U256::from(18))
}

/// Scripted engine for the telegram tests: canned views, recorded
/// executions, and a switchable observe/execute mode.
pub struct MockEngine {
    pub execute_mode: std::sync::atomic::AtomicBool,
    pub executed: std::sync::Mutex<Vec<(Draft, String)>>,
    pub limit_met: std::sync::Mutex<Option<bool>>,
    pub refuse: std::sync::Mutex<Option<String>>,
    pub holdings: alloy::primitives::U256,
}

impl MockEngine {
    pub fn new() -> Self {
        Self {
            execute_mode: std::sync::atomic::AtomicBool::new(false),
            executed: std::sync::Mutex::new(Vec::new()),
            limit_met: std::sync::Mutex::new(None),
            refuse: std::sync::Mutex::new(None),
            holdings: tk(15_000),
        }
    }

    pub fn set_execute(&self, on: bool) {
        self.execute_mode
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn execute_count(&self) -> usize {
        self.executed.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl EnginePort for MockEngine {
    async fn token(
        &self,
        _token: alloy::primitives::Address,
        _wallet: Option<alloy::primitives::Address>,
    ) -> basevantage::error::Result<TokenView> {
        Ok(TokenView {
            symbol: "TOKEN".to_string(),
            price_line: "0.00000012 USDC".to_string(),
            pools_line: "v2 TKN/WETH 1.2M/311".to_string(),
            dossier_line: "sell-tax 0.5% · honeypot no · FoT no".to_string(),
            impact_line: "0.05 ETH ≈ 0.31% (cap 1.5%)".to_string(),
            verdict_line: "allow · sources chain-rpc 3s".to_string(),
            blocked: false,
            block_reason: String::new(),
            holds: self.holdings,
        })
    }

    async fn quote(&self, draft: &Draft) -> basevantage::error::Result<QuoteView> {
        if let Some(reason) = self.refuse.lock().unwrap().clone() {
            return Ok(QuoteView {
                title: draft.symbol.clone(),
                route: "—".to_string(),
                gross_line: "—".to_string(),
                net_line: "—".to_string(),
                min_out_line: "—".to_string(),
                anchor_line: "—".to_string(),
                impact_line: "—".to_string(),
                verdict_ok: false,
                refusal_reason: reason,
                refusal_guidance: "pick a direct pool route".to_string(),
                limit_met: None,
            });
        }
        let e18 = tk(1);
        let (amount, min_out_line, anchor_line, limit_met) = match draft.kind {
            DraftKind::Buy { spend } => (
                spend,
                "2,394,100 USDC".to_string(),
                "REFERENCE 2,390,000 · SWAP 2,394,100 -> max".to_string(),
                None,
            ),
            DraftKind::Sell { amount } => (
                amount,
                "2,394,100 USDC".to_string(),
                "REFERENCE 2,390,000 · SWAP 2,394,100 -> max".to_string(),
                None,
            ),
            DraftKind::TargetBuy {
                spend,
                limit_price_1e18,
            } => {
                let min_tokens = spend * e18 / limit_price_1e18;
                (
                    spend,
                    format!(
                        "{} tokens (spend ÷ limit)",
                        basevantage::tg::cards::fmt_units(&min_tokens)
                    ),
                    "REFERENCE · SWAP · TARGET -> max".to_string(),
                    *self.limit_met.lock().unwrap(),
                )
            }
            DraftKind::TargetSell {
                amount,
                limit_price_1e18,
            } => {
                let min_out = limit_price_1e18 * amount / e18;
                (
                    amount,
                    format!(
                        "{} (limit × amount)",
                        basevantage::tg::cards::fmt_units(&min_out)
                    ),
                    "REFERENCE · SWAP · TARGET -> max".to_string(),
                    *self.limit_met.lock().unwrap(),
                )
            }
        };
        Ok(QuoteView {
            title: draft.symbol.clone(),
            route: "v3 WETH/USDC 5bps -> v2 TKN/WETH".to_string(),
            gross_line: format!("{} (gross)", basevantage::tg::cards::fmt_units(&amount)),
            net_line: "2,397,310.02 USDC (tax 0.5%, gas 0.00042 ETH)".to_string(),
            min_out_line,
            anchor_line,
            impact_line: "0.31% <= cap 1.5%".to_string(),
            verdict_ok: true,
            refusal_reason: String::new(),
            refusal_guidance: String::new(),
            limit_met,
        })
    }

    async fn execute(
        &self,
        draft: &Draft,
        _signer: &PrivateKeySigner,
    ) -> basevantage::error::Result<ReceiptView> {
        if !self.mode_execute() {
            return Err(basevantage::error::EngineError::SafetyRefused(
                "observe mode: nothing is sent".to_string(),
            ));
        }
        if let Some(reason) = self.refuse.lock().unwrap().clone() {
            return Err(basevantage::error::EngineError::SafetyRefused(reason));
        }
        let tx = format!("0xdeadbeef{}", self.execute_count());
        self.executed
            .lock()
            .unwrap()
            .push((draft.clone(), tx.clone()));
        Ok(ReceiptView {
            title: draft.symbol.clone(),
            fill_line: "filled 2,398,120.55 USDC >= min-out 2,394,100".to_string(),
            impact_line: "0.30% · gas 0.00043 ETH".to_string(),
            tx_hash: tx,
            verdict_line: "allow · filled above floor".to_string(),
        })
    }

    async fn positions(
        &self,
        _wallet: alloy::primitives::Address,
    ) -> basevantage::error::Result<Vec<PositionView>> {
        Ok(vec![PositionView {
            token: alloy::primitives::Address::repeat_byte(0x11),
            symbol: "TOKEN".to_string(),
            amount: self.holdings,
            value_line: "18.42 USDC".to_string(),
            pnl_line: "+2.4%".to_string(),
        }])
    }

    async fn withdraw(
        &self,
        _signer: &PrivateKeySigner,
        _to: alloy::primitives::Address,
        _amount: alloy::primitives::U256,
    ) -> basevantage::error::Result<String> {
        Ok("0xwithdraw".to_string())
    }

    fn mode_execute(&self) -> bool {
        self.execute_mode.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn settlement_symbol(&self) -> &'static str {
        "USDC"
    }

    fn settlement(&self) -> alloy::primitives::Address {
        alloy::primitives::Address::ZERO
    }
}
