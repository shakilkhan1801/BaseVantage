use alloy::primitives::{address, Address, Bytes, U256, U512};

use crate::error::{EngineError, Result};
use crate::venues::{PoolState, SwapLeg, Venue, VenueQuoter};

/// UniswapV2-style constant-product venue.
pub struct V2Venue;

/// Canonical Base Uniswap V2 router (fork tests target it).
pub const V2_ROUTER: Address = address!("4752ba5DBc23f44D87826276BF6Fd6b1C372aD24");
/// Canonical Base Uniswap V2 factory.
pub const V2_FACTORY: Address = address!("8909Dc15e40173Ff4699343b6eB8132c65e18eC6");

pub const DEFAULT_FEE_BPS: u32 = 30;

impl V2Venue {
    /// Core invariant: out = floor(in_eff * reserveOut / (reserveIn + in_eff))
    /// with in_eff = in * (10000 - fee_bps).
    pub fn amount_out(reserve_in: U256, reserve_out: U256, amount_in: U256, fee_bps: u32) -> Result<U256> {
        if reserve_in.is_zero() || reserve_out.is_zero() {
            return Err(EngineError::Quote("v2: empty reserves".to_string()));
        }
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }
        let fee_den = U512::from(10_000u64);
        let fee_num = U512::from(10_000u64 - u64::from(fee_bps));
        let a_in = U512::from(amount_in);
        let num = a_in * fee_num * U512::from(reserve_out);
        let den = U512::from(reserve_in) * fee_den + a_in * fee_num;
        if den.is_zero() {
            return Err(EngineError::Quote("v2: zero denominator".to_string()));
        }
        let out = num / den;
        if out > U512::from(U256::MAX) {
            return Err(EngineError::Quote("v2: overflow".to_string()));
        }
        Ok(U256::from(out))
    }
}

impl VenueQuoter for V2Venue {
    fn venue(&self) -> Venue {
        Venue::V2
    }

    fn quote_exact_in(
        &self,
        state: &PoolState,
        zero_for_one: bool,
        amount_in: U256,
    ) -> Result<U256> {
        let s = match state {
            PoolState::V2(s) => s,
            _ => return Err(EngineError::Quote("v2: wrong pool state".to_string())),
        };
        let (ra, rb) = if zero_for_one {
            (s.reserve0, s.reserve1)
        } else {
            (s.reserve1, s.reserve0)
        };
        Self::amount_out(ra, rb, amount_in, s.fee_bps)
    }

    fn encode_exact_in(&self, legs: &[SwapLeg], to: Address, deadline: u64) -> Result<Bytes> {
        if legs.is_empty() {
            return Err(EngineError::Quote("v2: empty legs".to_string()));
        }
        for leg in legs {
            if leg.venue != Venue::V2 {
                return Err(EngineError::Quote("v2: non-v2 leg in path".to_string()));
            }
        }
        let amount_in = legs[0].amount_in;
        let min_out = legs[legs.len() - 1].min_out;
        let mut path: Vec<Address> = vec![legs[0].token_in];
        path.extend(legs.iter().map(|l| l.token_out));

        Ok(encode_swap_exact_tokens_for_tokens(amount_in, min_out, &path, to, deadline))
    }
}

/// `swapExactTokensForTokens(uint256,uint256,address[],address,uint256)`
/// hand-encoded; cross-checked against alloy `sol!` in tests.
pub fn encode_swap_exact_tokens_for_tokens(
    amount_in: U256,
    min_out: U256,
    path: &[Address],
    to: Address,
    deadline: u64,
) -> Bytes {
    let selector = alloy::primitives::keccak256(
        "swapExactTokensForTokens(uint256,uint256,address[],address,uint256)",
    )[0..4]
        .to_vec();

    // Head: 5 words; the array is dynamic so word 3 is its tail offset.
    let path_offset = U256::from(5 * 32);
    let mut out = selector;
    out.extend_from_slice(&amount_in.to_be_bytes::<32>());
    out.extend_from_slice(&min_out.to_be_bytes::<32>());
    out.extend_from_slice(&path_offset.to_be_bytes::<32>());
    out.extend_from_slice(&to_word(to));
    out.extend_from_slice(&U256::from(deadline).to_be_bytes::<32>());
    // Tail: array length + elements.
    out.extend_from_slice(&U256::from(path.len()).to_be_bytes::<32>());
    for addr in path {
        out.extend_from_slice(&to_word(*addr));
    }
    Bytes::from(out)
}

pub fn to_word(addr: Address) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(addr.as_slice());
    word
}
