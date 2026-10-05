use alloy::primitives::{address, Address, Bytes, U256};

use crate::error::{EngineError, Result};
use crate::venues::{PoolState, SwapLeg, Venue, VenueQuoter, AeroState};

/// Aerodrome (Solidly-style) venue: volatile constant-product and stable
/// `x³y + y³x >= k` pools.
pub struct AerodromeVenue;

/// Canonical Base Aerodrome router (fork tests target it).
pub const AERO_ROUTER: Address = address!("cF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43");
/// Canonical Base Aerodrome factory.
pub const AERO_FACTORY: Address = address!("420DD381b31aEf6683db6B902084cB0FFECe40Da");

const E18: U256 = U256::from_limbs([0x0de0_b6b3_a764_0000, 0, 0, 0]); // 1e18

fn pow10(n: u8) -> U256 {
    U256::from(10u64).pow(U256::from(n))
}

/// Solidly `_f(x0, y) = (x0*y/1e18) * (x0²/1e18 + y²/1e18) / 1e18`, with the
/// exact intermediate floor divisions of the Solidity original.
fn f(x0: U256, y: U256) -> Result<U256> {
    let a = (x0 * y) / E18;
    let b = (x0 * x0) / E18 + (y * y) / E18;
    Ok((a * b) / E18)
}

fn d(x0: U256, y: U256) -> Result<U256> {
    let t = (U256::from(3) * x0 * ((y * y) / E18)) / E18;
    Ok(t + (((x0 * x0) / E18) * x0) / E18)
}

/// Solidly `_k` on raw reserves, normalized by token decimals.
fn k(state: &AeroState) -> Result<U256> {
    let x = (state.reserve0 * E18) / pow10(state.decimals0);
    let y = (state.reserve1 * E18) / pow10(state.decimals1);
    f(x, y)
}

/// Newton solve of `f(x0, y) = xy` for `y` — Solidly `_get_y`.
fn get_y(x0: U256, xy: U256, y0: U256) -> Result<U256> {
    let mut y = y0;
    for _ in 0..255 {
        let fy = f(x0, y)?;
        if fy < xy {
            let mut dy = ((xy - fy) * E18) / d(x0, y)?;
            if dy.is_zero() {
                if fy == xy {
                    return Ok(y);
                }
                if f(x0, y + U256::from(1))? > xy {
                    return Ok(y + U256::from(1));
                }
                dy = U256::from(1);
            }
            y = y + dy;
        } else {
            let mut dy = ((fy - xy) * E18) / d(x0, y)?;
            if dy.is_zero() {
                if fy == xy || f(x0, y - U256::from(1))? < xy {
                    return Ok(y);
                }
                dy = U256::from(1);
            }
            y = y - dy;
        }
    }
    Err(EngineError::Quote("aerodrome: _get_y did not converge".to_string()))
}

impl AerodromeVenue {
    /// Pool `getAmountOut(amount_in_after_fee, token_in)` exact port.
    pub fn amount_out(state: &AeroState, token_in_is_zero: bool, amount_in: U256) -> Result<U256> {
        let (dec_in, dec_out) = if token_in_is_zero {
            (state.decimals0, state.decimals1)
        } else {
            (state.decimals1, state.decimals0)
        };
        if state.stable {
            let xy = k(state)?;
            let r0n = (state.reserve0 * E18) / pow10(state.decimals0);
            let r1n = (state.reserve1 * E18) / pow10(state.decimals1);
            let (reserve_a, reserve_b) =
                if token_in_is_zero { (r0n, r1n) } else { (r1n, r0n) };
            let amount_in_n = (amount_in * E18) / pow10(dec_in);
            let y = reserve_b
                .checked_sub(get_y(amount_in_n + reserve_a, xy, reserve_b)?)
                .ok_or_else(|| EngineError::Quote("aerodrome: stable y underflow".to_string()))?;
            Ok((y * pow10(dec_out)) / E18)
        } else {
            let (reserve_a, reserve_b) = if token_in_is_zero {
                (state.reserve0, state.reserve1)
            } else {
                (state.reserve1, state.reserve0)
            };
            if reserve_a.is_zero() || reserve_b.is_zero() {
                return Err(EngineError::Quote("aerodrome: empty reserves".to_string()));
            }
            Ok((amount_in * reserve_b) / (reserve_a + amount_in))
        }
    }

    /// Pool `getAmountOut`: fee is removed from input first, floor division,
    /// exactly as the on-chain pool does with `factory.getFee`.
    pub fn quote_with_fee(
        state: &AeroState,
        token_in_is_zero: bool,
        amount_in: U256,
    ) -> Result<U256> {
        let fee = U256::from(state.fee_bps);
        let after_fee = amount_in - (amount_in * fee) / U256::from(10_000u64);
        Self::amount_out(state, token_in_is_zero, after_fee)
    }
}

impl VenueQuoter for AerodromeVenue {
    fn venue(&self) -> Venue {
        Venue::Aerodrome
    }

    fn quote_exact_in(
        &self,
        state: &PoolState,
        zero_for_one: bool,
        amount_in: U256,
    ) -> Result<U256> {
        let s = match state {
            PoolState::Aero(s) => s,
            _ => return Err(EngineError::Quote("aerodrome: wrong pool state".to_string())),
        };
        Self::quote_with_fee(s, zero_for_one, amount_in)
    }

    fn encode_exact_in(&self, legs: &[SwapLeg], to: Address, deadline: u64) -> Result<Bytes> {
        if legs.is_empty() {
            return Err(EngineError::Quote("aerodrome: empty legs".to_string()));
        }
        for leg in legs {
            if leg.venue != Venue::Aerodrome {
                return Err(EngineError::Quote(
                    "aerodrome: non-aerodrome leg in path".to_string(),
                ));
            }
        }
              let amount_in = legs[0].amount_in;
        let min_out = legs[legs.len() - 1].min_out;
        let routes: Vec<AeroRoute> = legs
            .iter()
            .map(|l| AeroRoute {
                from: l.token_in,
                to: l.token_out,
                stable: l.stable,
                factory: l.factory,
            })
            .collect();
        Ok(encode_swap_exact_tokens_for_tokens(
            amount_in, min_out, &routes, to, deadline,
        ))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AeroRoute {
    pub from: Address,
    pub to: Address,
    pub stable: bool,
    pub factory: Address,
}

/// `swapExactTokensForTokens(uint256,uint256,(address,address,bool,address)[],
/// address,uint256)` — hand-encoded; cross-checked against alloy `sol!` in tests.
pub fn encode_swap_exact_tokens_for_tokens(
    amount_in: U256,
    min_out: U256,
    routes: &[AeroRoute],
    to: Address,
    deadline: u64,
) -> Bytes {
    let selector = alloy::primitives::keccak256(
        "swapExactTokensForTokens(uint256,uint256,(address,address,bool,address)[],address,uint256)",
    )[0..4]
        .to_vec();
      let mut out = selector;
    out.extend_from_slice(&amount_in.to_be_bytes::<32>());
    out.extend_from_slice(&min_out.to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(5 * 32).to_be_bytes::<32>()); // routes offset
    out.extend_from_slice(&crate::venues::v2::to_word(to));
    out.extend_from_slice(&U256::from(deadline).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(routes.len()).to_be_bytes::<32>());
    for r in routes {
        out.extend_from_slice(&crate::venues::v2::to_word(r.from));
        out.extend_from_slice(&crate::venues::v2::to_word(r.to));
        out.extend_from_slice(&U256::from(u8::from(r.stable)).to_be_bytes::<32>());
        out.extend_from_slice(&crate::venues::v2::to_word(r.factory));
    }
    Bytes::from(out)
}
