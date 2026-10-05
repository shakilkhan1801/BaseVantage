//! `encoding::*` — every hand-rolled calldata encoder is byte-identical to
//! the alloy `sol!` reference encoding, and to the committed golden pins.

mod common;

use alloy::primitives::{address, Address, Bytes, U256};
use alloy::sol_types::{SolCall, SolValue};
use basevantage::market::abi::{
    IAerodromeRouter, IUniswapV2Router02, IUniswapV3Router, IUniversalRouter,
};
use basevantage::venues::aerodrome::{encode_swap_exact_tokens_for_tokens as aero_encode, AeroRoute};
use basevantage::venues::v3::{encode_exact_input, encode_exact_input_single};
use basevantage::venues::v4::{
    encode_v4_swap_single, pool_id, PoolKeyWords, POOLS_SLOT,
};
use basevantage::venues::v2::encode_swap_exact_tokens_for_tokens as v2_encode;

use common::{USDC, WETH};

const TOKEN: Address = address!("1111111111111111111111111111111111111111");
const ROUTER_T: Address = address!("9999999999999999999999999999999999999999");

fn amounts() -> (U256, U256) {
    (U256::from(1_234_567_890_123_456_789u128), U256::from(987_654_321u64))
}

/// Golden pin comparison. Run with BV_REGEN_GOLDEN=1 to rewrite the files.
fn assert_golden(name: &str, bytes: &Bytes) {
    let path = format!("fixtures/calldata/{name}.hex");
    let hex = hex::encode(bytes.as_ref());
    if std::env::var("BV_REGEN_GOLDEN").is_ok() {
        std::fs::create_dir_all("fixtures/calldata").unwrap();
        std::fs::write(&path, format!("{hex}\n")).unwrap();
        return;
    }
    let pinned = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden {path}: {e}"));
    assert_eq!(
        hex,
        pinned.trim(),
        "calldata drifted from the pinned golden {path}"
    );
}

#[test]
fn v2_single_hop_encoding_byte_identical() {
    let (amount_in, min_out) = amounts();
    let path = vec![TOKEN, USDC];
    let mine = v2_encode(amount_in, min_out, &path, ROUTER_T, 1_800_000_000);
    let theirs = IUniswapV2Router02::swapExactTokensForTokensCall {
        amountIn: amount_in,
        amountOutMin: min_out,
        path: path.clone(),
        to: ROUTER_T,
        deadline: U256::from(1_800_000_000u64),
    }
    .abi_encode();
    assert_eq!(mine.as_ref(), theirs.as_slice(), "v2 encoding must match sol! reference");
    assert_golden("v2_swap_exact_tokens_for_tokens", &mine);
}

#[test]
fn v2_multihop_path_encoding_byte_identical() {
    let (amount_in, min_out) = amounts();
    let path = vec![TOKEN, WETH, USDC];
    let mine = v2_encode(amount_in, min_out, &path, ROUTER_T, 1_800_000_000);
    let theirs = IUniswapV2Router02::swapExactTokensForTokensCall {
        amountIn: amount_in,
        amountOutMin: min_out,
        path: path.clone(),
        to: ROUTER_T,
        deadline: U256::from(1_800_000_000u64),
    }
    .abi_encode();
    assert_eq!(mine.as_ref(), theirs.as_slice());
    assert_golden("v2_swap_multihop_path", &mine);
}

#[test]
fn v3_single_hop_encoding_byte_identical() {
    let (amount_in, min_out) = amounts();
    let mine = encode_exact_input_single(TOKEN, USDC, 500, ROUTER_T, amount_in, min_out);
    let theirs = IUniswapV3Router::exactInputSingleCall {
        params: IUniswapV3Router::ExactInputSingleParams {
            tokenIn: TOKEN,
            tokenOut: USDC,
            fee: alloy::primitives::aliases::U24::from(500u32),
            recipient: ROUTER_T,
            amountIn: amount_in,
            amountOutMin: min_out,
            sqrtPriceLimitX96: alloy::primitives::aliases::U160::ZERO,
        },
    }
    .abi_encode();
    assert_eq!(mine.as_ref(), theirs.as_slice(), "v3 single encoding must match sol!");
    assert_golden("v3_exact_input_single", &mine);
}

#[test]
fn v3_multihop_path_encoding_byte_identical() {
    let (amount_in, min_out) = amounts();
    let path = basevantage::venues::v3::V3Venue::encode_path(&[TOKEN, WETH, USDC], &[500, 500])
        .unwrap();
    let mine = encode_exact_input(path.clone(), ROUTER_T, amount_in, min_out);
    let theirs = IUniswapV3Router::exactInputCall {
        params: IUniswapV3Router::ExactInputParams {
            path: path.clone(),
            recipient: ROUTER_T,
            amountIn: amount_in,
            amountOutMin: min_out,
        },
    }
    .abi_encode();
    assert_eq!(mine.as_ref(), theirs.as_slice(), "v3 path encoding must match sol!");
    assert_golden("v3_exact_input_multihop", &mine);
}

#[test]
fn aerodrome_encoding_byte_identical() {
    let (amount_in, min_out) = amounts();
    let routes = vec![
        AeroRoute { from: TOKEN, to: WETH, stable: false, factory: ROUTER_T },
        AeroRoute { from: WETH, to: USDC, stable: true, factory: ROUTER_T },
    ];
    let mine = aero_encode(amount_in, min_out, &routes, ROUTER_T, 1_800_000_000);
    let theirs = IAerodromeRouter::swapExactTokensForTokensCall {
        amountIn: amount_in,
        amountOutMin: min_out,
        routes: routes
            .iter()
            .map(|r| IAerodromeRouter::Route {
                from: r.from,
                to: r.to,
                stable: r.stable,
                factory: r.factory,
            })
            .collect(),
        to: ROUTER_T,
        deadline: U256::from(1_800_000_000u64),
    }
    .abi_encode();
    assert_eq!(mine.as_ref(), theirs.as_slice(), "aerodrome encoding must match sol!");
    assert_golden("aerodrome_swap_multihop", &mine);
}

#[test]
fn v4_single_swap_encoding_byte_identical() {
    let (amount_in, min_out) = amounts();
    let key = PoolKeyWords {
        currency0: USDC,
        currency1: WETH,
        fee: 500,
        tick_spacing: 10,
        hooks: Address::ZERO,
    };
    let mine = encode_v4_swap_single(key, WETH, USDC, amount_in, min_out, 1_800_000_000);

    // Reference envelope: execute([0x10], [abi.encode(actions, params)], deadline)
    let actions = [0x01u8, 0x06, 0x08];
    // The PoolKey part comes from sol!, not from our hand encoder.
    let sol_key = basevantage::market::abi::IV4Quoter::PoolKey {
        currency0: key.currency0,
        currency1: key.currency1,
        fee: alloy::primitives::aliases::U24::from(key.fee),
        tickSpacing: make_i24(key.tick_spacing),
        hooks: key.hooks,
    };
    let mut swap_params = sol_key.abi_encode();
    swap_params.extend_from_slice(&amount_in.to_be_bytes::<32>());
    swap_params.extend_from_slice(&min_out.to_be_bytes::<32>());

    let settle_params = {
        let mut p = Vec::new();
        p.extend_from_slice(&basevantage::venues::v2::to_word(WETH));
        p.extend_from_slice(&amount_in.to_be_bytes::<32>());
        p
    };
    let take_params = {
        let mut p = Vec::new();
        p.extend_from_slice(&basevantage::venues::v2::to_word(USDC));
        p.extend_from_slice(&min_out.to_be_bytes::<32>());
        p
    };

    let params: Vec<Bytes> = vec![
        Bytes::from(swap_params),
        Bytes::from(settle_params),
        Bytes::from(take_params),
    ];
    // sol! reference for the full execute() envelope, with the nested
    // (bytes actions, bytes[] params) tuple encoded by the same rules.
    let input = sol_encode_bytes_bytes_array(&actions, &params);
    let theirs = IUniversalRouter::executeCall {
        commands: Bytes::from(vec![0x10u8]),
        inputs: vec![Bytes::from(input)],
        deadline: U256::from(1_800_000_000u64),
    }
    .abi_encode();
    {
        let (a, b) = (mine.as_ref(), theirs.as_slice());
        if a != b {
            let n = a.len().min(b.len());
            for i in 0..n {
                if a[i] != b[i] {
                    let lo = i.saturating_sub(32);
                    eprintln!("first diff at byte {i} (len {} vs {})", a.len(), b.len());
                    eprintln!("mine  [{lo}..{}]: {}", n.min(i + 64), hex::encode(&a[lo..n.min(i + 64)]));
                    eprintln!("theirs[{lo}..{}]: {}", n.min(i + 64), hex::encode(&b[lo..n.min(i + 64)]));
                    break;
                }
            }
        }
    }
    assert_eq!(mine.as_ref(), theirs.as_slice(), "v4 execute envelope must match sol!");
    assert_golden("v4_execute_single", &mine);

    // Pool id sanity: deterministic across calls.
    let id = pool_id(key.currency0, key.currency1, key.fee, key.tick_spacing, key.hooks);
    assert_eq!(id, pool_id(USDC, WETH, 500, 10, Address::ZERO));
    assert_eq!(POOLS_SLOT, 6);
}

/// `abi.encode(bytes actions, bytes[] params)` via sol! tuple encoding.
fn sol_encode_bytes_bytes_array(actions: &[u8], params: &[Bytes]) -> Vec<u8> {
    use alloy::sol_types::{sol_data, SolType};
    type T = (sol_data::Bytes, sol_data::Array<sol_data::Bytes>);
    <T as SolType>::abi_encode_params(&(Bytes::copy_from_slice(actions), params.to_vec()))
}

fn make_i24(v: i32) -> alloy::primitives::aliases::I24 {
    alloy::primitives::aliases::I24::from_raw(alloy::primitives::aliases::U24::from(
        (v as u32) & 0x00ff_ffff,
    ))
}
