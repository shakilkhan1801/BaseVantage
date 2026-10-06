use alloy::primitives::{Address, Bytes, U256, U512};

use crate::error::{EngineError, Result};

pub mod aerodrome;
pub mod v2;
pub mod v3;
pub mod v4;

pub use aerodrome::AerodromeVenue;
pub use v2::V2Venue;
pub use v3::V3Venue;
pub use v4::V4Venue;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Venue {
    V2,
    V3,
    V4,
    Aerodrome,
}

impl Venue {
    pub fn name(self) -> &'static str {
        match self {
            Venue::V2 => "v2",
            Venue::V3 => "v3",
            Venue::V4 => "v4",
            Venue::Aerodrome => "aerodrome",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickData {
    pub tick: i32,
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
}

#[derive(Debug, Clone)]
pub struct V2State {
    pub reserve0: U256,
    pub reserve1: U256,
    pub fee_bps: u32,
}

#[derive(Debug, Clone)]
pub struct V3State {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub fee_pips: u32,
    /// Bitmap stride of the pool. The swap loop steps the way core's
    /// `TickBitmap` does — at most one bitmap word per step — which depends
    /// on the spacing; skipping those boundaries changes rounding.
    pub tick_spacing: i32,
    /// Effective swap fee per direction `(zero_for_one, one_for_zero)` when
    /// it differs by direction (v4 pools combine a per-direction protocol
    /// fee with the LP fee). `None` means `fee_pips` applies both ways.
    pub fee_pips_by_dir: Option<(u32, u32)>,
    /// Initialized ticks sorted ascending by tick.
    pub ticks: Vec<TickData>,
    /// True when the tick bitmap walk reached the end of initialized ticks,
    /// so quoting to the price bound is legitimate instead of under-informed.
    pub ticks_complete: bool,
}

#[derive(Debug, Clone)]
pub struct V4State {
    pub base: V3State,
    pub hooks: Address,
    pub dynamic_fee: bool,
}

#[derive(Debug, Clone)]
pub struct AeroState {
    pub stable: bool,
    pub reserve0: U256,
    pub reserve1: U256,
    pub fee_bps: u32,
    pub decimals0: u8,
    pub decimals1: u8,
}

#[derive(Debug, Clone)]
pub enum PoolState {
    V2(V2State),
    V3(V3State),
    V4(V4State),
    Aero(AeroState),
}

/// One swap leg: quoting input and calldata material in one place so the
/// pinned golden encodings cover the exact bytes the engine would send.
#[derive(Debug, Clone)]
pub struct SwapLeg {
    pub venue: Venue,
    pub pool: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub min_out: U256,
    /// v2/aerodrome: fee bps. v3/v4: fee pips (pool tier).
    pub fee: u32,
    /// aerodrome: stable pool; v3/v4 ignored.
    pub stable: bool,
    /// v4: hooks address; others zero.
    pub hooks: Address,
    /// v4: tick spacing; others zero.
    pub tick_spacing: i32,
    /// aerodrome: factory; others zero.
    pub factory: Address,
}

/// A venue-family quoter and calldata encoder.
pub trait VenueQuoter: Send + Sync {
    fn venue(&self) -> Venue;
    /// Exact-in quote against cached pool state. `zero_for_one` says whether
    /// token_in is pool token0.
    fn quote_exact_in(
        &self,
        state: &PoolState,
        zero_for_one: bool,
        amount_in: U256,
    ) -> Result<U256>;
    /// Encode one transaction's calldata for an ordered chain of legs of this
    /// venue. `to` receives the output; `deadline` bounds the router call.
    fn encode_exact_in(&self, legs: &[SwapLeg], to: Address, deadline: u64) -> Result<Bytes>;
}

fn overflow() -> EngineError {
    EngineError::Quote("math overflow".to_string())
}

/// floor(a * b / d) with 512-bit intermediate precision.
pub fn mul_div_floor(a: U256, b: U256, d: U256) -> Result<U256> {
    if d.is_zero() {
        return Err(EngineError::Quote("division by zero".to_string()));
    }
    let prod = U512::from(a) * U512::from(b);
    let q = prod / U512::from(d);
    if q > U512::from(U256::MAX) {
        return Err(overflow());
    }
    Ok(U256::from(q))
}

/// ceil(a * b / d) with 512-bit intermediate precision.
pub fn mul_div_ceil(a: U256, b: U256, d: U256) -> Result<U256> {
    if d.is_zero() {
        return Err(EngineError::Quote("division by zero".to_string()));
    }
    let prod = U512::from(a) * U512::from(b);
    let dd = U512::from(d);
    let mut q = prod / dd;
    if prod % dd != U512::ZERO {
        q += U512::from(1);
    }
    if q > U512::from(U256::MAX) {
        return Err(overflow());
    }
    Ok(U256::from(q))
}

const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);
const E18: U256 = U256::from_limbs([0x0de0_b6b3_a764_0000, 0, 0, 0]);

/// Marginal price of token_out per token_in at zero size, 1e18-scaled.
/// Used for settlement-asset normalization and impact; exact execution
/// amounts always come from the venue quoters.
pub fn marginal_price_1e18(state: &PoolState, zero_for_one: bool) -> Result<U256> {
    match state {
        PoolState::V2(s) => ratio_marginal(s.reserve0, s.reserve1, zero_for_one),
        PoolState::Aero(s) => ratio_marginal(s.reserve0, s.reserve1, zero_for_one),
        PoolState::V3(s) => sqrt_marginal(s.sqrt_price_x96, zero_for_one),
        PoolState::V4(s) => sqrt_marginal(s.base.sqrt_price_x96, zero_for_one),
    }
}

fn ratio_marginal(reserve0: U256, reserve1: U256, zero_for_one: bool) -> Result<U256> {
    let (ra, rb) = if zero_for_one {
        (reserve0, reserve1)
    } else {
        (reserve1, reserve0)
    };
    if ra.is_zero() {
        return Err(EngineError::Quote("marginal: empty reserves".to_string()));
    }
    mul_div_floor(rb, E18, ra)
}

fn sqrt_marginal(sqrt_price_x96: U256, zero_for_one: bool) -> Result<U256> {
    // price(token1/token0) = (sqrt / 2^96)^2
    let num = U512::from(sqrt_price_x96) * U512::from(sqrt_price_x96);
    let den_q96sq = U512::from(Q96) * U512::from(Q96);
    if zero_for_one {
        let scaled = num * U512::from(E18) / den_q96sq;
        if scaled > U512::from(U256::MAX) {
            return Err(EngineError::Quote("marginal: overflow".to_string()));
        }
        Ok(U256::from(scaled))
    } else {
        let scaled = U512::from(E18) * den_q96sq / num;
        if scaled > U512::from(U256::MAX) {
            return Err(EngineError::Quote("marginal: overflow".to_string()));
        }
        Ok(U256::from(scaled))
    }
}
