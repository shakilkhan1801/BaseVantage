use alloy::primitives::{Address, B256, Bytes, U256, address};

use crate::error::{EngineError, Result};
use crate::venues::v3::quote_exact_in_state;
use crate::venues::{PoolState, SwapLeg, TickData, V3State, V4State, Venue, VenueQuoter};

/// Uniswap V4 venue: v3-style concentrated liquidity behind one PoolManager,
/// quoted locally from `extsload` state and encoded through the Universal Router.
pub struct V4Venue;

/// Canonical Base Uniswap V4 PoolManager (fork tests target it).
pub const POOL_MANAGER: Address = address!("498581fF718922c3f8e6A244956aF099B2652b2b");
/// Canonical Base Universal Router (entrypoint for v4 swaps).
pub const UNIVERSAL_ROUTER: Address = address!("6fF5693b99212Da76ad316178A184AB56D299b43");

/// v4-core `StateLibrary`: index of the `pools` mapping in the PoolManager.
pub const POOLS_SLOT: u64 = 6;
pub const FEE_GROWTH_GLOBAL0_OFFSET: u64 = 1;
pub const LIQUIDITY_OFFSET: u64 = 3;
pub const TICKS_OFFSET: u64 = 4;
pub const TICK_BITMAP_OFFSET: u64 = 5;

/// `PoolId = keccak256(abi.encode(PoolKey))`.
pub fn pool_id(
    currency0: Address,
    currency1: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
) -> B256 {
    let mut enc = Vec::with_capacity(5 * 32);
    enc.extend_from_slice(&word_address(currency0));
    enc.extend_from_slice(&word_address(currency1));
    enc.extend_from_slice(&U256::from(fee).to_be_bytes::<32>());
    enc.extend_from_slice(&word_i24(tick_spacing));
    enc.extend_from_slice(&word_address(hooks));
    alloy::primitives::keccak256(enc)
}

/// `keccak256(abi.encodePacked(poolId, POOLS_SLOT))` — base slot of Pool.State.
pub fn pool_state_slot(id: B256) -> U256 {
    let mut packed = [0u8; 64];
    packed[..32].copy_from_slice(id.as_slice());
    packed[32..].copy_from_slice(&U256::from(POOLS_SLOT).to_be_bytes::<32>());
    U256::from_be_slice(alloy::primitives::keccak256(packed).as_slice())
}

/// `keccak256(abi.encodePacked(int256(key), base_mapping_slot))` for the
/// `ticks` and `tickBitmap` mappings inside Pool.State.
pub fn mapping_slot(base: U256, key: U256) -> U256 {
    let mut packed = [0u8; 64];
    packed[..32].copy_from_slice(&key.to_be_bytes::<32>());
    packed[32..].copy_from_slice(&base.to_be_bytes::<32>());
    U256::from_be_slice(alloy::primitives::keccak256(packed).as_slice())
}

pub fn tick_slot(state_slot: U256, tick: i32) -> U256 {
    mapping_slot(
        state_slot + U256::from(TICKS_OFFSET),
        word_i24_to_u256(tick),
    )
}

pub fn tick_bitmap_slot(state_slot: U256, word_pos: i32) -> U256 {
    mapping_slot(
        state_slot + U256::from(TICK_BITMAP_OFFSET),
        word_i24_to_u256(word_pos),
    )
}

fn word_i24_to_u256(v: i32) -> U256 {
    // int256 sign-extension of the int24/int16 key.
    let mut w = [0xffu8; 32];
    w[29..].copy_from_slice(&v.to_be_bytes()[1..4]);
    if v >= 0 {
        w = [0u8; 32];
        w[29..].copy_from_slice(&v.to_be_bytes()[1..4]);
    }
    U256::from_be_slice(&w)
}

fn word_address(addr: Address) -> [u8; 32] {
    crate::venues::v2::to_word(addr)
}

fn word_i24(v: i32) -> [u8; 32] {
    let mut w = [0xffu8; 32];
    if v >= 0 {
        w = [0u8; 32];
    }
    w[29..].copy_from_slice(&v.to_be_bytes()[1..4]);
    w
}

/// Core's `ProtocolFeeLibrary.calculateSwapFee`: the protocol fee is taken
/// from the input first and the LP fee from the remainder, so the combined
/// rate is `protocolFee + lpFee - protocolFee * lpFee / 1e6`.
pub fn combine_swap_fee(protocol_fee: u32, lp_fee: u32) -> u32 {
    protocol_fee + lp_fee - (protocol_fee * lp_fee) / 1_000_000
}

/// Decode a v4 `Pool.Slot0` word: (sqrtPriceX96, tick, protocolFee, lpFee).
pub fn decode_slot0(word: U256) -> (U256, i32, u32, u32) {
    let mask = U256::from(0xFFFFFFu64);
    let sqrt: U256 = word & ((U256::from(1u64) << 160) - U256::from(1));
    let tick_raw: u32 = ((word >> 160usize) & mask).to();
    let tick = (tick_raw << 8) as i32 >> 8; // sign-extend 24 bits
    let protocol_fee: u32 = ((word >> 184usize) & mask).to();
    let lp_fee: u32 = ((word >> 208usize) & mask).to();
    (sqrt, tick, protocol_fee, lp_fee)
}

/// Decode a v4 TickInfo's first word: (liquidityGross, liquidityNet).
pub fn decode_tick_info(word: U256) -> (u128, i128) {
    let mask: U256 = (U256::from(1u64) << 128) - U256::from(1);
    let gross: u128 = (word & mask).to();
    let net: u128 = (word >> 128usize).to();
    (gross, net as i128)
}

impl V4Venue {
    /// Quote over cached state. Dynamic-fee pools need a resolved fee: the
    /// market layer reads it with the state; a zero fee with `dynamic_fee`
    /// refuses rather than guessing.
    pub fn quote(state: &V4State, zero_for_one: bool, amount_in: U256) -> Result<U256> {
        if state.dynamic_fee && state.base.fee_pips == 0 {
            return Err(EngineError::Quote(
                "v4: dynamic fee unresolved — read fee controller first".to_string(),
            ));
        }
        quote_exact_in_state(&state.base, zero_for_one, amount_in)
    }

    /// Build the v3-style state used for quoting from raw extsload words.
    pub fn state_from_parts(
        slot0_word: U256,
        liquidity: u128,
        ticks: Vec<TickData>,
        ticks_complete: bool,
        hooks: Address,
        dynamic_fee: bool,
        tick_spacing: i32,
    ) -> V4State {
        let (sqrt, tick, protocol_fee, lp_fee) = decode_slot0(slot0_word);
        V4State {
            base: V3State {
                sqrt_price_x96: sqrt,
                liquidity,
                tick,
                fee_pips: lp_fee,
                tick_spacing,
                fee_pips_by_dir: Some((
                    combine_swap_fee(protocol_fee & 0xfff, lp_fee),
                    combine_swap_fee(protocol_fee >> 12, lp_fee),
                )),
                ticks,
                ticks_complete,
            },
            hooks,
            dynamic_fee,
        }
    }
}

impl VenueQuoter for V4Venue {
    fn venue(&self) -> Venue {
        Venue::V4
    }

    fn quote_exact_in(
        &self,
        state: &PoolState,
        zero_for_one: bool,
        amount_in: U256,
    ) -> Result<U256> {
        let s = match state {
            PoolState::V4(s) => s,
            _ => return Err(EngineError::Quote("v4: wrong pool state".to_string())),
        };
        Self::quote(s, zero_for_one, amount_in)
    }

    fn encode_exact_in(&self, legs: &[SwapLeg], to: Address, deadline: u64) -> Result<Bytes> {
        if legs.is_empty() {
            return Err(EngineError::Quote("v4: empty legs".to_string()));
        }
        for leg in legs {
            if leg.venue != Venue::V4 {
                return Err(EngineError::Quote("v4: non-v4 leg in path".to_string()));
            }
        }
        let amount_in = legs[0].amount_in;
        let min_out = legs[legs.len() - 1].min_out;
        if legs.len() == 1 {
            let leg = &legs[0];
            let key = PoolKeyWords {
                currency0: min_addr(leg.token_in, leg.token_out),
                currency1: max_addr(leg.token_in, leg.token_out),
                fee: leg.fee,
                tick_spacing: leg.tick_spacing,
                hooks: leg.hooks,
            };
            return Ok(encode_v4_swap_single(
                key,
                leg.token_in,
                leg.token_out,
                amount_in,
                min_out,
                deadline,
            ));
        }
        let keys: Vec<PoolKeyWords> = legs
            .iter()
            .map(|l| PoolKeyWords {
                currency0: min_addr(l.token_in, l.token_out),
                currency1: max_addr(l.token_in, l.token_out),
                fee: l.fee,
                tick_spacing: l.tick_spacing,
                hooks: l.hooks,
            })
            .collect();
        let out_token = legs[legs.len() - 1].token_out;
        Ok(encode_v4_swap_path(
            keys, legs, to, out_token, amount_in, min_out, deadline,
        ))
    }
}

fn min_addr(a: Address, b: Address) -> Address {
    if a <= b { a } else { b }
}
fn max_addr(a: Address, b: Address) -> Address {
    if a <= b { b } else { a }
}

#[derive(Debug, Clone, Copy)]
pub struct PoolKeyWords {
    pub currency0: Address,
    pub currency1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Address,
}

impl PoolKeyWords {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(5 * 32);
        out.extend_from_slice(&word_address(self.currency0));
        out.extend_from_slice(&word_address(self.currency1));
        out.extend_from_slice(&U256::from(self.fee).to_be_bytes::<32>());
        out.extend_from_slice(&word_i24(self.tick_spacing));
        out.extend_from_slice(&word_address(self.hooks));
        out
    }
}

// Universal Router action codes — the numbering the Base deployment
// dispatches (verified against the live router; older drafts used a
// different layout and revert with UnsupportedAction).
const ACTION_SWAP_EXACT_IN_SINGLE: u8 = 0x06;
const ACTION_SWAP_EXACT_IN: u8 = 0x07;
const ACTION_SETTLE_ALL: u8 = 0x0c;
const ACTION_TAKE_ALL: u8 = 0x0f;
// Universal Router command codes.
const CMD_V4_SWAP: u8 = 0x10;

/// `execute(bytes commands, bytes[] inputs, uint256 deadline)` for one
/// exact-in single v4 swap: SWAP_EXACT_IN_SINGLE, SETTLE_ALL, TAKE_ALL.
#[allow(clippy::too_many_arguments)]
pub fn encode_v4_swap_single(
    key: PoolKeyWords,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    min_out: U256,
    deadline: u64,
) -> Bytes {
    let actions = vec![
        ACTION_SWAP_EXACT_IN_SINGLE,
        ACTION_SETTLE_ALL,
        ACTION_TAKE_ALL,
    ];
    // ExactInputSingleParams = (PoolKey, bool zeroForOne, uint256 amountIn,
    // uint256 amountOutMin, bytes hookData) — head 9 words + empty hookData.
    let zero_for_one = token_in == key.currency0;
    let mut swap_params = key.encode();
    swap_params.extend_from_slice(&if zero_for_one {
        U256::from(1).to_be_bytes::<32>()
    } else {
        U256::ZERO.to_be_bytes::<32>()
    });
    swap_params.extend_from_slice(&amount_in.to_be_bytes::<32>());
    swap_params.extend_from_slice(&min_out.to_be_bytes::<32>());
    swap_params.extend_from_slice(&U256::from(9 * 32).to_be_bytes::<32>());
    swap_params.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
    let settle_params = encode_words(&[word_address(token_in), amount_in.to_be_bytes()]);
    let take_params = encode_words(&[word_address(token_out), min_out.to_be_bytes()]);
    let params = vec![
        Bytes::from(wrap_single_value(swap_params)),
        Bytes::from(settle_params),
        Bytes::from(take_params),
    ];
    let input = encode_v4_input(&actions, &params);
    encode_execute(&[CMD_V4_SWAP], &[Bytes::from(input)], deadline)
}

/// `execute` for a v4-internal multi-hop exact-in path (SWAP_EXACT_IN).
fn encode_v4_swap_path(
    keys: Vec<PoolKeyWords>,
    legs: &[SwapLeg],
    to: Address,
    out_token: Address,
    amount_in: U256,
    min_out: U256,
    deadline: u64,
) -> Bytes {
    let actions = vec![ACTION_SWAP_EXACT_IN, ACTION_SETTLE_ALL, ACTION_TAKE_ALL];

    // PathKey[] path: (intermediateCurrency, fee, tickSpacing, hooks, hookData)
    let mut path_tail = Vec::new();
    let n = keys.len();
    let mut offsets = Vec::with_capacity(n * 32);
    let mut bodies = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        offsets.extend_from_slice(&U256::from((n + i * 3) * 32).to_be_bytes::<32>());
        let mut body = Vec::new();
        body.extend_from_slice(&word_address(legs[i].token_out));
        body.extend_from_slice(&U256::from(key.fee).to_be_bytes::<32>());
        body.extend_from_slice(&word_i24(key.tick_spacing));
        body.extend_from_slice(&word_address(key.hooks));
        body.extend_from_slice(&U256::from(5 * 32).to_be_bytes::<32>()); // hookData offset in PathKey
        body.extend_from_slice(&U256::ZERO.to_be_bytes::<32>()); // hookData length 0
        bodies.push(body);
    }
    path_tail.extend_from_slice(&U256::from(n).to_be_bytes::<32>());
    path_tail.extend_from_slice(&offsets);
    for b in bodies {
        path_tail.extend_from_slice(&b);
    }

    // SwapParams: (PathKey[] path, address recipient, uint256 amountIn, uint256 amountOutMin)
    let mut swap_params = Vec::new();
    swap_params.extend_from_slice(&U256::from(4 * 32).to_be_bytes::<32>()); // path offset
    swap_params.extend_from_slice(&word_address(to));
    swap_params.extend_from_slice(&amount_in.to_be_bytes::<32>());
    swap_params.extend_from_slice(&min_out.to_be_bytes::<32>());
    swap_params.extend_from_slice(&path_tail);

    let settle_params = encode_words(&[word_address(legs[0].token_in), amount_in.to_be_bytes()]);
    let take_params = encode_words(&[word_address(out_token), min_out.to_be_bytes()]);
    let params = vec![
        Bytes::from(wrap_single_value(swap_params)),
        Bytes::from(settle_params),
        Bytes::from(take_params),
    ];
    let input = encode_v4_input(&actions, &params);
    encode_execute(&[CMD_V4_SWAP], &[Bytes::from(input)], deadline)
}

/// The router decodes swap params with `abi.decode(elem, (Struct))`, i.e.
/// single-value encoding: a dynamic struct carries a leading offset word
/// before its tuple body. Static params (settle/take) are unwrapped tuples.
fn wrap_single_value(tuple: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(tuple.len() + 32);
    out.extend_from_slice(&U256::from(32).to_be_bytes::<32>());
    out.extend_from_slice(&tuple);
    out
}

fn encode_words(words: &[[u8; 32]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(words.len() * 32);
    for w in words {
        out.extend_from_slice(w);
    }
    out
}

fn encode_bytes_word(data: &[u8]) -> Vec<u8> {
    let padded = data.len().div_ceil(32) * 32;
    let mut out = U256::from(data.len()).to_be_bytes::<32>().to_vec();
    out.extend_from_slice(data);
    out.resize(out.len() + (padded - data.len()), 0);
    out
}

/// `abi.encode(bytes actions, bytes[] params)` — the V4_SWAP command input.
fn encode_v4_input(actions: &[u8], params: &[Bytes]) -> Vec<u8> {
    let actions_tail = encode_bytes_word(actions);
    let actions_head_offset = 2 * 32;
    let params_head_offset = actions_head_offset + actions_tail.len();

    let mut params_tail = Vec::new();
    params_tail.extend_from_slice(&U256::from(params.len()).to_be_bytes::<32>());
    let mut cursor = params.len() * 32;
    for p in params {
        params_tail.extend_from_slice(&U256::from(cursor).to_be_bytes::<32>());
        cursor += 32 + p.len().div_ceil(32) * 32;
    }
    for p in params {
        params_tail.extend_from_slice(&encode_bytes_word(p));
    }

    let mut out = Vec::new();
    out.extend_from_slice(&U256::from(actions_head_offset).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(params_head_offset).to_be_bytes::<32>());
    out.extend_from_slice(&actions_tail);
    out.extend_from_slice(&params_tail);
    out
}

/// `execute(bytes commands, bytes[] inputs, uint256 deadline)`.
fn encode_execute(commands: &[u8], inputs: &[Bytes], deadline: u64) -> Bytes {
    let selector = alloy::primitives::keccak256("execute(bytes,bytes[],uint256)")[0..4].to_vec();
    let mut out = selector;
    // Head: (bytes commands, bytes[] inputs, uint256 deadline)
    out.extend_from_slice(&U256::from(3 * 32).to_be_bytes::<32>()); // commands offset
    let commands_tail_len = 32 + commands.len().div_ceil(32) * 32;
    let inputs_offset = 3 * 32 + commands_tail_len;
    out.extend_from_slice(&U256::from(inputs_offset).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(deadline).to_be_bytes::<32>());
    // commands tail
    out.extend_from_slice(&encode_bytes_word(commands));
    // inputs tail: array of bytes
    let n = inputs.len();
    out.extend_from_slice(&U256::from(n).to_be_bytes::<32>());
    let mut cursor = n * 32;
    for p in inputs {
        out.extend_from_slice(&U256::from(cursor).to_be_bytes::<32>());
        cursor += 32 + p.len().div_ceil(32) * 32;
    }
    for p in inputs {
        out.extend_from_slice(&encode_bytes_word(p));
    }
    Bytes::from(out)
}
