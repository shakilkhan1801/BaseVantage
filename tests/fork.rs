//! `fork::*` — the VPS fork suite. Requires `BASE_RPC_URL`; every test is
//! `#[ignore]`d without it (`cargo test -- --ignored`). Quoter tests compare
//! local math against each venue's own on-chain quoter, exactly. The
//! execution test runs the real calldata on an anvil fork.

mod common;

use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use alloy::network::TransactionBuilder;
use alloy::primitives::{address, Address, Bytes, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use basevantage::chain::base::BaseChain;
use basevantage::chain::{CallRequest, ChainAdapter, DynChain};
use basevantage::market::abi::{
    IAerodromePool, IERC20, IQuoterV2, IUniswapV2Router02, IV4Quoter,
};
use basevantage::market::{PoolKey, Registry};
use basevantage::venues::aerodrome::{
    encode_swap_exact_tokens_for_tokens as aero_encode, AeroRoute, AERO_ROUTER,
};
use basevantage::venues::v2::V2_ROUTER;
use basevantage::venues::v3::{encode_exact_input, V3Venue, V3_ROUTER, V3_QUOTER};
use basevantage::venues::v4::{encode_v4_swap_single, PoolKeyWords, UNIVERSAL_ROUTER};
use basevantage::venues::{
    AerodromeVenue, PoolState, Venue, VenueQuoter, V2Venue, V4Venue,
};

pub const WETH: Address = address!("4200000000000000000000000000000000000006");
pub const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
pub const DAI: Address = address!("50c5725949A6F0c72E6C4a641F24049A917DB0Cb");
pub const AERO: Address = address!("940181a94A35A4569E4529A3CDfB74e38FD98631");
pub const V4_QUOTER: Address = address!("0d5e0f971ed27fbff6c2837bf31316121532048d");
pub const PERMIT2: Address = address!("000000000022D473030F116dDEE9F6B43aC78BA3");
/// anvil's pre-funded unlocked account #0.
pub const ANVIL_ACCOUNT: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

fn fork_url() -> Option<String> {
    std::env::var("BASE_RPC_URL").ok().filter(|s| !s.is_empty())
}

fn chain_for(url: &str) -> Arc<BaseChain> {
    Arc::new(BaseChain::new(&[url.to_string()], Vec::new(), 60).expect("rpc url"))
}

fn registry_for(chain: &Arc<BaseChain>) -> Registry {
    Registry::new(
        chain.clone() as DynChain,
        basevantage::venues::v2::V2_FACTORY,
        basevantage::venues::v3::V3_FACTORY,
        basevantage::venues::aerodrome::AERO_FACTORY,
        basevantage::venues::v4::POOL_MANAGER,
    )
    .with_max_ticks(24)
}

fn unit(n: u64, decimals: u8) -> U256 {
    U256::from(n) * U256::from(10u64).pow(U256::from(decimals))
}

async fn erc20_balance(chain: &dyn ChainAdapter, token: Address, who: Address) -> U256 {
    let data = IERC20::balanceOfCall { owner: who }.abi_encode();
    let out = chain
        .call(CallRequest { to: Some(token), data: Some(Bytes::from(data)), ..Default::default() })
        .await
        .expect("balanceOf");
    IERC20::balanceOfCall::abi_decode_returns(&out).expect("decode balance")
}

/// First discovered pool of `venue` for `a`/`b`.
async fn pick_pool(reg: &Registry, venue: Venue, a: Address, b: Address) -> Option<PoolKey> {
    let mut pools = reg.discover(a, &[b]).await.expect("discover");
    pools.retain(|p| p.venue == venue && p.other(a) == b);
    pools.into_iter().next()
}

async fn load_state(reg: &Registry, key: &PoolKey) -> PoolState {
    let meta = reg.load_meta(key).await.expect("meta");
    reg.load_state(key, &meta).await.expect("state")
}

fn make_i24(v: i32) -> alloy::primitives::aliases::I24 {
    alloy::primitives::aliases::I24::from_raw(alloy::primitives::aliases::U24::from(
        (v as u32) & 0x00ff_ffff,
    ))
}

// ---------------------------------------------------------------- quoter tests

#[tokio::test]
#[ignore = "fork suite: needs BASE_RPC_URL"]
async fn v2_quoter_matches_chain() {
    let Some(url) = fork_url() else { eprintln!("skip: BASE_RPC_URL unset"); return };
    let chain = chain_for(&url);
    let reg = registry_for(&chain);
    let key = pick_pool(&reg, Venue::V2, WETH, USDC).await.expect("v2 WETH/USDC pool");
    let state = load_state(&reg, &key).await;

    for (zfo, amount, path) in [
        (true, unit(1, 18), vec![WETH, USDC]),
        (false, unit(3_000, 6), vec![USDC, WETH]),
    ] {
        let mine = V2Venue.quote_exact_in(&state, zfo, amount).expect("local quote");
        let data = IUniswapV2Router02::getAmountsOutCall { amountIn: amount, path }.abi_encode();
        let out = chain
            .call(CallRequest {
                to: Some(V2_ROUTER),
                data: Some(Bytes::from(data)),
                ..Default::default()
            })
            .await
            .expect("getAmountsOut");
        let theirs: Vec<U256> =
            IUniswapV2Router02::getAmountsOutCall::abi_decode_returns(&out).expect("decode amounts");
        assert_eq!(mine, *theirs.last().expect("amounts"), "v2 local quote must equal chain");
    }
}

#[tokio::test]
#[ignore = "fork suite: needs BASE_RPC_URL"]
async fn v3_quoter_matches_chain() {
    let Some(url) = fork_url() else { eprintln!("skip: BASE_RPC_URL unset"); return };
    let chain = chain_for(&url);
    let reg = registry_for(&chain);
    let key = pick_pool(&reg, Venue::V3, WETH, USDC).await.expect("v3 WETH/USDC pool");
    let state = load_state(&reg, &key).await;

    for (zfo, amount, token_in, token_out) in [
        (true, unit(1, 18), WETH, USDC),
        (false, unit(3_000, 6), USDC, WETH),
    ] {
        let mine = V2Venue.quote_exact_in(&state, zfo, amount).err(); // probe: wrong venue must fail
        assert!(mine.is_some(), "wrong venue state must not quote");
        let mine = basevantage::venues::v3::quote_exact_in_state(
            match &state {
                PoolState::V3(s) => s,
                _ => panic!("v3 state"),
            },
            zfo,
            amount,
        )
        .expect("local quote");
        let params = IQuoterV2::QuoteExactInputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount,
            fee: alloy::primitives::aliases::U24::from(key.fee),
            sqrtPriceLimitX96: alloy::primitives::aliases::U160::ZERO,
        };
        let data = IQuoterV2::quoteExactInputSingleCall { params }.abi_encode();
        let out = chain
            .call(CallRequest {
                to: Some(V3_QUOTER),
                data: Some(Bytes::from(data)),
                ..Default::default()
            })
            .await
            .expect("quoterV2");
        let theirs = IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(&out).expect("decode");
        assert_eq!(mine, theirs.amountOut, "v3 local quote must equal QuoterV2 exactly");
    }
}

#[tokio::test]
#[ignore = "fork suite: needs BASE_RPC_URL"]
async fn v4_quoter_matches_chain() {
    let Some(url) = fork_url() else { eprintln!("skip: BASE_RPC_URL unset"); return };
    let chain = chain_for(&url);
    let reg = registry_for(&chain);
    let key = pick_pool(&reg, Venue::V4, WETH, USDC).await.expect("v4 WETH/USDC pool");
    let state = match load_state(&reg, &key).await {
        PoolState::V4(s) => s,
        _ => panic!("v4 state"),
    };

    for (zfo, amount) in [(true, unit(1, 18)), (false, unit(3_000, 6))] {
        let mine = V4Venue::quote(&state, zfo, amount).expect("local quote");
        let params = IV4Quoter::QuoteExactSingleParams {
            poolKey: IV4Quoter::PoolKey {
                currency0: key.token0,
                currency1: key.token1,
                fee: alloy::primitives::aliases::U24::from(key.fee),
                tickSpacing: make_i24(key.tick_spacing),
                hooks: key.hooks,
            },
            zeroForOne: zfo,
            exactAmount: amount,
            hookData: Bytes::new(),
        };
        let data = IV4Quoter::quoteExactInputSingleCall { params }.abi_encode();
        let out = chain
            .call(CallRequest { to: Some(V4_QUOTER), data: Some(Bytes::from(data)), ..Default::default() })
            .await
            .expect("v4 quoter");
        let theirs = IV4Quoter::quoteExactInputSingleCall::abi_decode_returns(&out).expect("decode");
        assert_eq!(mine, theirs.amountOut, "v4 local quote must equal the v4 Quoter exactly");
    }
}

#[tokio::test]
#[ignore = "fork suite: needs BASE_RPC_URL"]
async fn aerodrome_quoter_matches_chain() {
    let Some(url) = fork_url() else { eprintln!("skip: BASE_RPC_URL unset"); return };
    let chain = chain_for(&url);
    let reg = registry_for(&chain);
    let key = pick_pool(&reg, Venue::Aerodrome, WETH, USDC).await.expect("aerodrome WETH/USDC pool");
    let state = match load_state(&reg, &key).await {
        PoolState::Aero(s) => s,
        _ => panic!("aero state"),
    };

    for (zfo, amount, token_in) in [(true, unit(1, 18), WETH), (false, unit(3_000, 6), USDC)] {
        let mine = AerodromeVenue::quote_with_fee(&state, zfo, amount).expect("local quote");
        let data = IAerodromePool::getAmountOutCall { amountIn: amount, tokenIn: token_in }.abi_encode();
        let out = chain
            .call(CallRequest { to: Some(key.address), data: Some(Bytes::from(data)), ..Default::default() })
            .await
            .expect("getAmountOut");
        let theirs = IAerodromePool::getAmountOutCall::abi_decode_returns(&out).expect("decode");
        assert_eq!(mine, theirs, "aerodrome local quote must equal the pool exactly");
    }
}

// ------------------------------------------------------------ calldata pin test

#[test]
#[ignore = "fork suite grouping: byte-identical pins"]
fn single_hop_encoding_byte_identical_pin() {
    // Same inputs as the committed goldens in fixtures/calldata.
    let amount_in = U256::from(1_234_567_890_123_456_789u128);
    let min_out = U256::from(987_654_321u64);
    let recipient: Address = address!("9999999999999999999999999999999999999999");
    let token: Address = address!("1111111111111111111111111111111111111111");

    let v2 = basevantage::venues::v2::encode_swap_exact_tokens_for_tokens(
        amount_in,
        min_out,
        &[token, USDC],
        recipient,
        1_800_000_000,
    );
    assert_pin("v2_swap_exact_tokens_for_tokens", &v2);

    let v3 = basevantage::venues::v3::encode_exact_input_single(
        token,
        USDC,
        500,
        recipient,
        amount_in,
        min_out,
    );
    assert_pin("v3_exact_input_single", &v3);

    let aero = aero_encode(
        amount_in,
        min_out,
        &[
            AeroRoute { from: token, to: WETH, stable: false, factory: recipient },
            AeroRoute { from: WETH, to: USDC, stable: true, factory: recipient },
        ],
        recipient,
        1_800_000_000,
    );
      assert_pin("aerodrome_swap_multihop", &aero);

    let key = PoolKeyWords {
        currency0: USDC,
        currency1: WETH,
        fee: 500,
        tick_spacing: 10,
        hooks: Address::ZERO,
    };
    let v4 = encode_v4_swap_single(key, WETH, USDC, amount_in, min_out, 1_800_000_000);
    assert_pin("v4_execute_single", &v4);
}

fn assert_pin(name: &str, bytes: &Bytes) {
    let pinned = std::fs::read_to_string(format!("fixtures/calldata/{name}.hex"))
        .unwrap_or_else(|e| panic!("missing golden {name}: {e}"));
    assert_eq!(
        hex::encode(bytes.as_ref()),
        pinned.trim(),
        "{name} encoding drifted from its pinned golden"
    );
}

// ------------------------------------------------------- calldata execution test

struct Anvil {
    child: Child,
    url: String,
}

impl Anvil {
    fn spawn(fork_url: &str) -> Self {
        let port = 40_000 + (std::process::id() % 20_000) as u16;
        let child = Command::new("anvil")
            .args([
                "--fork-url",
                fork_url,
                "--port",
                &port.to_string(),
                "--silent",
                "--no-rate-limit",
            ])
            .spawn()
            .expect("anvil must be installed for the execution fork test");
        let url = format!("http://127.0.0.1:{port}");
        Self { child, url }
    }

    async fn spawn_ready(fork_url: &str) -> Self {
        let node = Self::spawn(fork_url);
        node.wait_ready().await;
        node
    }

    fn url(&self) -> &str {
        &self.url
    }

    async fn wait_ready(&self) {
        for _ in 0..240 {
            let provider = ProviderBuilder::new()
                .connect_http(self.url.parse().expect("anvil url"));
            if provider.get_chain_id().await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!("anvil did not become ready at {}", self.url);
    }
}

impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

async fn send_tx(provider: &DynProvider, to: Address, input: Bytes, value: Option<U256>) -> bool {
    let mut tx = TransactionRequest::default().with_from(ANVIL_ACCOUNT).with_to(to).with_input(input);
    if let Some(v) = value {
        tx = tx.with_value(v);
    }
    let pending = provider.send_transaction(tx).await.expect("send tx");
    let receipt = pending.get_receipt().await.expect("tx receipt");
    receipt.status()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "fork suite: needs BASE_RPC_URL + anvil"]
async fn multihop_calldata_executes() {
    let Some(url) = fork_url() else { eprintln!("skip: BASE_RPC_URL unset"); return };
    let _anvil = Anvil::spawn_ready(&url).await;
    let provider: DynProvider = ProviderBuilder::new()
        .connect_http(_anvil.url().parse().expect("anvil url"))
        .erased();
    let chain = chain_for(_anvil.url());
    let reg = registry_for(&chain);

    // Inventory: wrap ETH, then acquire DAI through v2 AND v3 single-hop
    // encodings — both single-hop encoders execute against the live routers.
    assert!(send_tx(&provider, WETH, Bytes::from_static(&[0xd0, 0xe3, 0x0d, 0xb0]), Some(unit(5, 18))).await);
    let weth_start = erc20_balance(&*chain, WETH, ANVIL_ACCOUNT).await;
    assert!(weth_start >= unit(5, 18), "wrap must credit WETH");

    let approve = |spender: Address, amount: U256| -> Bytes {
        Bytes::from(IERC20::approveCall { spender, amount }.abi_encode())
    };

    // v2 single: WETH -> DAI
    assert!(send_tx(&provider, WETH, approve(V2_ROUTER, U256::MAX), None).await);
    let v2_single = basevantage::venues::v2::encode_swap_exact_tokens_for_tokens(
        unit(1, 18),
        U256::ZERO,
        &[WETH, DAI],
        ANVIL_ACCOUNT,
        2_000_000_000,
    );
    assert!(send_tx(&provider, V2_ROUTER, v2_single, None).await, "v2 single-hop calldata must execute");
    assert!(erc20_balance(&*chain, DAI, ANVIL_ACCOUNT).await > U256::ZERO, "v2 swap must deliver DAI");

    // v3 single: WETH -> DAI (500)
    assert!(send_tx(&provider, WETH, approve(V3_ROUTER, U256::MAX), None).await);
    let v3_single =
        basevantage::venues::v3::encode_exact_input_single(WETH, DAI, 500, ANVIL_ACCOUNT, unit(1, 18), U256::ZERO);
    assert!(send_tx(&provider, V3_ROUTER, v3_single, None).await, "v3 single-hop calldata must execute");

    // v2 MULTI-HOP: DAI -> WETH -> USDC, min-out from our own chained quote.
    let dai_key = pick_pool(&reg, Venue::V2, DAI, WETH).await.expect("v2 DAI/WETH pool");
    let weth_key = pick_pool(&reg, Venue::V2, WETH, USDC).await.expect("v2 WETH/USDC pool");
    let dai_state = load_state(&reg, &dai_key).await;
    let weth_state = load_state(&reg, &weth_key).await;
    let sell = erc20_balance(&*chain, DAI, ANVIL_ACCOUNT).await / U256::from(2);
    let mid = V2Venue
        .quote_exact_in(&dai_state, dai_key.zero_for_one(DAI), sell)
        .expect("quote DAI->WETH");
    let expected_out = V2Venue
        .quote_exact_in(&weth_state, weth_key.zero_for_one(WETH), mid)
        .expect("quote WETH->USDC");
    let min_out = expected_out * U256::from(99) / U256::from(100);

    let usdc_before = erc20_balance(&*chain, USDC, ANVIL_ACCOUNT).await;
    let weth_before = erc20_balance(&*chain, WETH, ANVIL_ACCOUNT).await;
    assert!(send_tx(&provider, DAI, approve(V2_ROUTER, U256::MAX), None).await);
    let v2_multi = basevantage::venues::v2::encode_swap_exact_tokens_for_tokens(
        sell,
        min_out,
        &[DAI, WETH, USDC],
        ANVIL_ACCOUNT,
        2_000_000_000,
    );
      assert!(send_tx(&provider, V2_ROUTER, v2_multi, None).await, "v2 multi-hop calldata must execute");
    let usdc_after = erc20_balance(&*chain, USDC, ANVIL_ACCOUNT).await;
    let weth_after = erc20_balance(&*chain, WETH, ANVIL_ACCOUNT).await;
    assert!(usdc_after >= usdc_before + min_out, "multi-hop sell must settle USDC above min-out");
    assert_eq!(weth_after, weth_before, "wrapped-native residue must be zero after settlement");

    // v3 MULTI-HOP: DAI -> WETH 500 -> USDC 500 packed path.
    let v3_weth = pick_pool(&reg, Venue::V3, DAI, WETH).await.expect("v3 DAI/WETH pool");
    let v3_usdc = pick_pool(&reg, Venue::V3, WETH, USDC).await.expect("v3 WETH/USDC pool");
    let path = V3Venue::encode_path(&[DAI, WETH, USDC], &[v3_weth.fee, v3_usdc.fee]).expect("path");
    let usdc_before = erc20_balance(&*chain, USDC, ANVIL_ACCOUNT).await;
    assert!(send_tx(&provider, DAI, approve(V3_ROUTER, U256::MAX), None).await);
    let v3_multi = encode_exact_input(path, ANVIL_ACCOUNT, sell, U256::ZERO);
    assert!(send_tx(&provider, V3_ROUTER, v3_multi, None).await, "v3 multi-hop calldata must execute");
    assert!(erc20_balance(&*chain, USDC, ANVIL_ACCOUNT).await > usdc_before, "v3 multi-hop must settle USDC");

    // aerodrome MULTI-HOP: WETH -> AERO -> USDC volatile Route[].
    let aero_first = pick_pool(&reg, Venue::Aerodrome, WETH, AERO).await.expect("aero WETH/AERO");
    let aero_second = pick_pool(&reg, Venue::Aerodrome, AERO, USDC).await.expect("aero AERO/USDC");
    let usdc_before = erc20_balance(&*chain, USDC, ANVIL_ACCOUNT).await;
    assert!(send_tx(&provider, WETH, approve(AERO_ROUTER, U256::MAX), None).await);
    let aero_multi = aero_encode(
        unit(1, 18),
        U256::ZERO,
        &[
            AeroRoute { from: WETH, to: AERO, stable: aero_first.stable, factory: basevantage::venues::aerodrome::AERO_FACTORY },
            AeroRoute { from: AERO, to: USDC, stable: aero_second.stable, factory: basevantage::venues::aerodrome::AERO_FACTORY },
        ],
        ANVIL_ACCOUNT,
        2_000_000_000,
    );
    assert!(send_tx(&provider, AERO_ROUTER, aero_multi, None).await, "aerodrome multi-hop calldata must execute");
    assert!(erc20_balance(&*chain, USDC, ANVIL_ACCOUNT).await > usdc_before, "aero multi-hop must settle USDC");

    // v4 SINGLE-HOP live through the Universal Router (Permit2 approvals),
    // exercising the exact pinned envelope.
    let v4_key = pick_pool(&reg, Venue::V4, WETH, USDC).await.expect("v4 WETH/USDC pool");
    assert!(send_tx(&provider, WETH, approve(PERMIT2, U256::MAX), None).await);
    alloy::sol! {
        interface IPermit2 {
            function approve(address token, address spender, uint160 amount, uint48 expiration) external;
        }
          }
    let permit2_call = IPermit2::approveCall {
        token: WETH,
        spender: UNIVERSAL_ROUTER,
        amount: alloy::primitives::aliases::U160::MAX,
        expiration: alloy::primitives::aliases::U48::MAX,
    }
    .abi_encode();
    assert!(send_tx(&provider, PERMIT2, Bytes::from(permit2_call), None).await);
    let usdc_before = erc20_balance(&*chain, USDC, ANVIL_ACCOUNT).await;
    let key_words = PoolKeyWords {
        currency0: v4_key.token0,
        currency1: v4_key.token1,
        fee: v4_key.fee,
        tick_spacing: v4_key.tick_spacing,
        hooks: v4_key.hooks,
    };
    let v4_single = encode_v4_swap_single(
        key_words,
        WETH,
        USDC,
        unit(1, 18),
        U256::ZERO,
        2_000_000_000,
    );
    assert!(send_tx(&provider, UNIVERSAL_ROUTER, v4_single, None).await, "v4 universal-router calldata must execute");
    assert!(erc20_balance(&*chain, USDC, ANVIL_ACCOUNT).await > usdc_before, "v4 swap must settle USDC");
}
