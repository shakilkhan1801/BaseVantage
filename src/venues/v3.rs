use alloy::primitives::{address, Address, Bytes, U256, U512};

use crate::error::{EngineError, Result};
use crate::venues::{mul_div_ceil, mul_div_floor, PoolState, SwapLeg, Venue, VenueQuoter, V3State};

/// Uniswap V3-style concentrated-liquidity venue.
pub struct V3Venue;

/// Canonical Base Uniswap V3 SwapRouter02 (fork tests target it).
pub const V3_ROUTER: Address = address!("2626664c2603336E57B271c5C0b26F421741e481");
/// Canonical Base Uniswap V3 factory (fork tests target it).
pub const V3_FACTORY: Address = address!("33128a8fC17869897dcE68Ed026d694621f6FDfD");
/// Canonical Base QuoterV2 (fork-test oracle).
pub const V3_QUOTER: Address = address!("3d4e44Eb1374240CE5F1B871ab261CD16335B76a");

pub const MIN_TICK: i32 = -887_272;
pub const MAX_TICK: i32 = 887_272;
pub const MIN_SQRT_RATIO: U256 = U256::from_limbs([4_295_128_739, 0, 0, 0]);
pub const MAX_SQRT_RATIO: U256 =
    U256::from_limbs([0x6398_8d26, 0x5064_8849_5d95_1d52, 0xfffd_8963_efd1_fc6a, 0]);

const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);
const FEE_DEN: u64 = 1_000_000;

/// sqrt(1.0001^tick) * 2^96 — exact port of Uniswap v3 `TickMath`.
pub fn sqrt_ratio_at_tick(tick: i32) -> Result<U256> {
    let abs_tick = tick.unsigned_abs();
    if abs_tick > MAX_TICK as u32 {
        return Err(EngineError::Quote("v3: tick out of range".to_string()));
    }
    let mut ratio = if abs_tick & 0x1 != 0 {
        U256::from_str_radix("fffcb933bd6fad37aa2d162d1a594001", 16).unwrap()
    } else {
        U256::from(1u64) << 128
    };
      const MULTS: [&str; 19] = [
        "fff97272373d413259a46990580e213a",
        "fff2e50f5f656932ef12357cf3c7fdcc",
        "ffe5caca7e10e4e61c3624eaa0941cd0",
        "ffcb9843d60f6159c9db58835c926644",
        "ff973b41fa98c081472e6896dfb254c0",
        "ff2ea16466c96a3843ec78b326b52861",
        "fe5dee046a99a2a811c461f1969c3053",
        "fcbe86c7900a88aedcffc83b479aa3a4",
        "f987a7253ac413176f2b074cf7815e54",
        "f3392b0822b70005940c7a398e4b70f3",
        "e7159475a2c29b7443b29c7fa6e889d9",
        "d097f3bdfd2022b8845ad8f792aa5825",
        "a9f746462d870fdf8a65dc1f90e061e5",
        "70d869a156d2a1b890bb3df62baf32f7",
        "31be135f97d08fd981231505542fcfa6",
        "9aa508b5b7a84e1c677de54f3e99bc9",
        "5d6af8dedb81196699c329225ee604",
        "2216e584f5fa1ea926041bedfe98",
        "48a170391f7dc42444e8fa2",
    ];
    for (i, mult) in MULTS.iter().enumerate() {
        if abs_tick & (1u32 << (i + 1)) != 0 {
            let m = U256::from_str_radix(mult, 16).unwrap();
            ratio = (ratio * m) >> 128;
        }
    }
    if tick > 0 {
        ratio = U256::MAX / ratio;
    }
    // Q128.128 -> Q128.96, rounding up like the Solidity library.
    let shifted = ratio >> 32;
    Ok(if ratio % (U256::from(1u64) << 32) == U256::ZERO {
        shifted
    } else {
        shifted + U256::from(1)
    })
}

fn ordered(a: U256, b: U256) -> (U256, U256) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn div_round(num: U512, den: U512, round_up: bool) -> Result<U256> {
    if den.is_zero() {
        return Err(EngineError::Quote("v3: zero denominator".to_string()));
    }
    let mut q = num / den;
    if round_up && num % den != U512::ZERO {
        q += U512::from(1);
    }
    if q > U512::from(U256::MAX) {
        return Err(EngineError::Quote("v3: overflow".to_string()));
    }
    Ok(U256::from(q))
}

/// ceil/floor( L * (sqrtB - sqrtA) / 2^96 ) — v3 `getAmount1Delta`.
pub fn amount1_delta(sqrt_a: U256, sqrt_b: U256, liquidity: u128, round_up: bool) -> Result<U256> {
    let (lo, hi) = ordered(sqrt_a, sqrt_b);
    let diff = hi - lo;
    if round_up {
        mul_div_ceil(U256::from(liquidity), diff, Q96)
    } else {
        mul_div_floor(U256::from(liquidity), diff, Q96)
    }
}

/// ceil/floor( L * 2^96 * (sqrtB - sqrtA) / (sqrtB * sqrtA) ) — v3 `getAmount0Delta`.
pub fn amount0_delta(sqrt_a: U256, sqrt_b: U256, liquidity: u128, round_up: bool) -> Result<U256> {
    let (lo, hi) = ordered(sqrt_a, sqrt_b);
    if lo.is_zero() {
        return Err(EngineError::Quote("v3: zero sqrt price".to_string()));
    }
    let numerator1 = U512::from(liquidity) << 96;
    let numerator2 = U512::from(hi - lo);
    let product = numerator1 * numerator2;
    let den = U512::from(lo) * U512::from(hi);
    div_round(product, den, round_up)
}

/// v3 `getNextSqrtPriceFromInput`: price after swapping in `amount_in`.
pub fn next_sqrt_price_from_input(
    sqrt_price: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> Result<U256> {
    if sqrt_price.is_zero() || liquidity == 0 {
        return Err(EngineError::Quote("v3: zero liquidity or price".to_string()));
    }
    if zero_for_one {
        // amount0 in: sqrt' = ceil( (L<<96) * sqrt / ((L<<96) + amountIn*sqrt) )
        if amount_in.is_zero() {
            return Ok(sqrt_price);
        }
        let numerator1 = U512::from(liquidity) << 96;
        let product = U512::from(amount_in) * U512::from(sqrt_price);
        let denominator = numerator1 + product;
        div_round(numerator1 * U512::from(sqrt_price), denominator, true)
    } else {
        // amount1 in: sqrt' = sqrt + floor((amountIn<<96)/L)
        let quotient = mul_div_floor(amount_in, Q96, U256::from(liquidity))?;
        sqrt_price
            .checked_add(quotient)
            .ok_or_else(|| EngineError::Quote("v3: sqrt price overflow".to_string()))
    }
}

/// One exact-in swap step across a single liquidity range — v3 `SwapMath`.
pub struct SwapStep {
    pub sqrt_price_next: U256,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_amount: U256,
}

pub fn compute_swap_step(
    sqrt_current: U256,
    sqrt_target: U256,
    liquidity: u128,
    amount_remaining: U256,
    fee_pips: u32,
) -> Result<SwapStep> {
    let zero_for_one = sqrt_current >= sqrt_target;
    let fee_scale = U256::from(FEE_DEN - u64::from(fee_pips));
    let remaining_less_fee =
        mul_div_ceil(amount_remaining, fee_scale, U256::from(FEE_DEN))?;

    let amount_in_to_target = if zero_for_one {
        amount0_delta(sqrt_target, sqrt_current, liquidity, true)?
    } else {
        amount1_delta(sqrt_current, sqrt_target, liquidity, true)?
    };

    let (sqrt_next, amount_in) = if remaining_less_fee >= amount_in_to_target {
        (sqrt_target, amount_in_to_target)
    } else {
        let next =
            next_sqrt_price_from_input(sqrt_current, liquidity, remaining_less_fee, zero_for_one)?;
        (next, remaining_less_fee)
    };

    let amount_out = if zero_for_one {
        amount1_delta(sqrt_next, sqrt_current, liquidity, false)?
    } else {
        amount0_delta(sqrt_current, sqrt_next, liquidity, false)?
    };

    let fee_amount = if sqrt_next != sqrt_target {
        // Input exhausted inside the range: fee is whatever rounding left over.
        amount_remaining.saturating_sub(amount_in)
    } else {
        mul_div_ceil(amount_in, U256::from(fee_pips), fee_scale)?
    };

    Ok(SwapStep { sqrt_price_next: sqrt_next, amount_in, amount_out, fee_amount })
}

fn add_delta(liquidity: u128, delta: i128) -> Result<u128> {
    if delta < 0 {
        liquidity
            .checked_sub(delta.unsigned_abs())
            .ok_or_else(|| EngineError::Quote("v3: liquidity underflow".to_string()))
    } else {
        liquidity
            .checked_add(delta as u128)
            .ok_or_else(|| EngineError::Quote("v3: liquidity overflow".to_string()))
    }
}

/// Next initialized tick in swap direction. Down (`zero_for_one`): greatest
/// initialized tick <= current. Up: smallest initialized tick > current.
pub fn next_initialized_tick(
    ticks: &[crate::venues::TickData],
    tick: i32,
    zero_for_one: bool,
) -> Option<(i32, i128)> {
    if zero_for_one {
        ticks
            .iter()
            .rev()
            .find(|t| t.tick <= tick)
            .map(|t| (t.tick, t.liquidity_net))
    } else {
        ticks.iter().find(|t| t.tick > tick).map(|t| (t.tick, t.liquidity_net))
    }
}

/// Exact-in quote across initialized ticks (the v3 swap loop). Refuses rather
/// than approximates when the loaded tick depth would be exhausted mid-swap
/// and the tick set is not marked complete.
pub fn quote_exact_in_state(state: &V3State, zero_for_one: bool, amount_in: U256) -> Result<U256> {
    if amount_in.is_zero() {
        return Ok(U256::ZERO);
    }
    if state.liquidity == 0 {
        return Err(EngineError::Quote("v3: zero active liquidity".to_string()));
    }
    let limit = if zero_for_one { MIN_SQRT_RATIO } else { MAX_SQRT_RATIO };
    let mut sqrt_price = state.sqrt_price_x96;
    let mut liquidity = state.liquidity;
    let mut tick = state.tick;
    let mut remaining = amount_in;
    let mut out_total = U256::ZERO;

    while remaining > U256::ZERO && sqrt_price != limit {
        let next_tick = next_initialized_tick(&state.ticks, tick, zero_for_one);
        let (sqrt_next_raw, liquidity_net) = match next_tick {
            Some((t, net)) => (sqrt_ratio_at_tick(t)?, Some((t, net))),
            None => (limit, None),
        };
        let sqrt_target = if zero_for_one {
            sqrt_next_raw.max(limit)
        } else {
            sqrt_next_raw.min(limit)
        };

        let step = compute_swap_step(sqrt_price, sqrt_target, liquidity, remaining, state.fee_pips)?;
        remaining = remaining.saturating_sub(step.amount_in + step.fee_amount);
        out_total += step.amount_out;
        sqrt_price = step.sqrt_price_next;

        match liquidity_net {
            Some((t, net)) if sqrt_price == sqrt_next_raw => {
                liquidity = add_delta(liquidity, if zero_for_one { -net } else { net })?;
                tick = if zero_for_one { t - 1 } else { t };
            }
            Some(_) => break, // input exhausted inside the range
            None => {
                              if remaining > U256::ZERO && !state.ticks_complete {
                    return Err(EngineError::Quote(
                        "v3: tick depth exhausted — reload state with more initialized ticks"
                            .to_string(),
                    ));
                }
                break;
            }
        }
    }

    Ok(out_total)
}

impl V3Venue {
    /// Packed `bytes path` for exactInput: token ++ fee(3) ++ token [++ ...].
    pub fn encode_path(tokens: &[Address], fees: &[u32]) -> Result<Bytes> {
        if tokens.len() < 2 || fees.len() + 1 != tokens.len() {
            return Err(EngineError::Quote("v3: bad path shape".to_string()));
        }
        let mut out = Vec::with_capacity(20 + fees.len() * 23);
        for (i, fee) in fees.iter().enumerate() {
            out.extend_from_slice(tokens[i].as_slice());
            out.push((fee >> 16) as u8);
            out.push((fee >> 8) as u8);
            out.push(*fee as u8);
        }
        out.extend_from_slice(tokens[tokens.len() - 1].as_slice());
        Ok(Bytes::from(out))
    }
}

impl VenueQuoter for V3Venue {
    fn venue(&self) -> Venue {
        Venue::V3
    }

    fn quote_exact_in(
        &self,
        state: &PoolState,
        zero_for_one: bool,
        amount_in: U256,
    ) -> Result<U256> {
        let s = match state {
            PoolState::V3(s) => s,
            _ => return Err(EngineError::Quote("v3: wrong pool state".to_string())),
        };
        quote_exact_in_state(s, zero_for_one, amount_in)
    }

    fn encode_exact_in(&self, legs: &[SwapLeg], to: Address, _deadline: u64) -> Result<Bytes> {
        if legs.is_empty() {
            return Err(EngineError::Quote("v3: empty legs".to_string()));
        }
        for leg in legs {
            if leg.venue != Venue::V3 {
                return Err(EngineError::Quote("v3: non-v3 leg in path".to_string()));
            }
        }
        let amount_in = legs[0].amount_in;
        let min_out = legs[legs.len() - 1].min_out;
        if legs.len() == 1 {
            return Ok(encode_exact_input_single(
                legs[0].token_in,
                legs[0].token_out,
                legs[0].fee,
                to,
                amount_in,
                min_out,
            ));
        }
        let mut tokens = vec![legs[0].token_in];
        tokens.extend(legs.iter().map(|l| l.token_out));
        let fees: Vec<u32> = legs.iter().map(|l| l.fee).collect();
        let path = Self::encode_path(&tokens, &fees)?;
        Ok(encode_exact_input(path, to, amount_in, min_out))
    }
}

/// `exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))`
/// on SwapRouter02 — hand-encoded; cross-checked against alloy `sol!` in tests.
#[allow(clippy::too_many_arguments)]
pub fn encode_exact_input_single(
    token_in: Address,
    token_out: Address,
    fee: u32,
    to: Address,
    amount_in: U256,
    min_out: U256,
) -> Bytes {
    let selector = alloy::primitives::keccak256(
        "exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))",
    )[0..4]
        .to_vec();
    let mut out = selector;
    out.extend_from_slice(&crate::venues::v2::to_word(token_in));
    out.extend_from_slice(&crate::venues::v2::to_word(token_out));
    out.extend_from_slice(&U256::from(fee).to_be_bytes::<32>());
    out.extend_from_slice(&crate::venues::v2::to_word(to));
    out.extend_from_slice(&amount_in.to_be_bytes::<32>());
    out.extend_from_slice(&min_out.to_be_bytes::<32>());
    out.extend_from_slice(&U256::ZERO.to_be_bytes::<32>()); // sqrtPriceLimitX96
    Bytes::from(out)
}

/// `exactInput((bytes,address,uint256,uint256))` on SwapRouter02.
pub fn encode_exact_input(path: Bytes, to: Address, amount_in: U256, min_out: U256) -> Bytes {
    let selector =
        alloy::primitives::keccak256("exactInput((bytes,address,uint256,uint256))")[0..4].to_vec();
    let mut out = selector;
    // Head: the single tuple argument is dynamic -> its offset word.
    out.extend_from_slice(&U256::from(32).to_be_bytes::<32>());
    // Tuple head: (bytes path, address recipient, uint256 amountIn, uint256 amountOutMin)
    out.extend_from_slice(&U256::from(4 * 32).to_be_bytes::<32>()); // path offset in tuple
    out.extend_from_slice(&crate::venues::v2::to_word(to));
    out.extend_from_slice(&amount_in.to_be_bytes::<32>());
    out.extend_from_slice(&min_out.to_be_bytes::<32>());
    // Tuple tail: bytes
    let padded = (path.len() + 31) / 32 * 32;
    out.extend_from_slice(&U256::from(path.len()).to_be_bytes::<32>());
    let mut data = path.to_vec();
    data.resize(padded, 0);
    out.extend_from_slice(&data);
    Bytes::from(out)
}
