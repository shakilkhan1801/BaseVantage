use alloy::primitives::aliases::{I24, U24};
use alloy::primitives::Bytes;
use alloy::primitives::{Address, Uint, U256, B256};
use alloy::sol_types::SolCall;

use crate::chain::DynChain;
use crate::error::{EngineError, Result};
use crate::market::abi::{
    IAerodromeFactory, IAerodromePool, IExtsloadMany, IUniswapV2Factory, IUniswapV2Pair,
    IUniswapV3Factory, IUniswapV3Pool,
};
use crate::market::{PoolKey, PoolMeta};
use crate::venues::v4::{decode_slot0, decode_tick_info, pool_state_slot, tick_bitmap_slot, tick_slot};
use crate::venues::{AeroState, PoolState, TickData, V2State, V3State, V4State, Venue};

/// Fee tiers queried on discovery (v3 and v4).
pub const V3_FEE_TIERS: [u32; 4] = [100, 500, 3000, 10000];

/// Canonical v4 tick spacing per fee tier.
pub fn v4_tick_spacing_for_fee(fee: u32) -> i32 {
    match fee {
        100 => 1,
        500 => 10,
        3000 => 60,
        10000 => 200,
        _ => 60,
    }
}

const DEFAULT_MAX_TICKS_PER_SIDE: usize = 48;

/// Factory-registry discovery and pool state loading. One eth_call per read;
/// v4 batches storage reads through `extsload`. Cloneable: cheap (Arc chain).
#[derive(Clone)]
pub struct Registry {
    chain: DynChain,
    pub v2_factory: Address,
    pub v3_factory: Address,
    pub aero_factory: Address,
    pub pool_manager: Address,
    max_ticks_per_side: usize,
}

impl Registry {
    /// Bound the initialized-tick walk per side (fork tests quote small sizes).
    pub fn with_max_ticks(mut self, max_ticks_per_side: usize) -> Self {
        self.max_ticks_per_side = max_ticks_per_side;
        self
    }

    pub fn new(
        chain: DynChain,
        v2_factory: Address,
        v3_factory: Address,
        aero_factory: Address,
        pool_manager: Address,
    ) -> Self {
        Self {
            chain,
            v2_factory,
            v3_factory,
            aero_factory,
            pool_manager,
            max_ticks_per_side: DEFAULT_MAX_TICKS_PER_SIDE,
        }
    }

    async fn call<T, D>(&self, to: Address, data: Vec<u8>, decode: D) -> Result<T>
    where
        D: FnOnce(&Bytes) -> Result<T>,
    {
        let out = self
            .chain
            .call(crate::chain::CallRequest { to: Some(to), data: Some(Bytes::from(data)), ..Default::default() })
            .await?;
        decode(&out)
    }

    /// Every pool for `token` paired with any hub, across all four venues.
    pub async fn discover(&self, token: Address, hubs: &[Address]) -> Result<Vec<PoolKey>> {
        let mut pools = Vec::new();
        for hub in hubs {
            if *hub == token {
                continue;
            }
            let (token0, token1) = ordered(token, *hub);
            pools.extend(self.discover_v2(token, *hub, token0, token1).await?);
            pools.extend(self.discover_v3(token, *hub, token0, token1).await?);
            pools.extend(self.discover_v4(token0, token1).await?);
            pools.extend(self.discover_aero(token, *hub, token0, token1).await?);
        }
        Ok(pools)
    }

    async fn discover_v2(
        &self,
        token: Address,
        hub: Address,
        token0: Address,
        token1: Address,
    ) -> Result<Vec<PoolKey>> {
        let data = IUniswapV2Factory::getPairCall { tokenA: token, tokenB: hub }.abi_encode();
        let out = self.call(self.v2_factory, data, |b| {
            IUniswapV2Factory::getPairCall::abi_decode_returns(b)
                .map_err(|e| EngineError::Rpc(e.to_string()))
        })
        .await?;
        if out == Address::ZERO {
            return Ok(Vec::new());
        }
              Ok(vec![PoolKey {
            venue: Venue::V2,
            address: out,
            token0,
            token1,
            fee: 30,
            tick_spacing: 0,
            hooks: Address::ZERO,
            stable: false,
            factory: self.v2_factory,
            v4_pool_id: None,
        }])
    }

    async fn discover_v3(
        &self,
        token: Address,
        hub: Address,
        token0: Address,
        token1: Address,
    ) -> Result<Vec<PoolKey>> {
        let mut pools = Vec::new();
        for fee in V3_FEE_TIERS {
            let data = IUniswapV3Factory::getPoolCall {
                tokenA: token,
                tokenB: hub,
                fee: U24::from(fee),
            }
            .abi_encode();
            let out = self
                .call(self.v3_factory, data, |b| {
                    IUniswapV3Factory::getPoolCall::abi_decode_returns(b)
                        .map_err(|e| EngineError::Rpc(e.to_string()))
                })
                .await?;
            if out != Address::ZERO {
                pools.push(PoolKey {
                    venue: Venue::V3,
                    address: out,
                    token0,
                    token1,
                    fee,
                    tick_spacing: 0,
                    hooks: Address::ZERO,
                    stable: false,
                    factory: self.v3_factory,
                    v4_pool_id: None,
                });
            }
        }
        Ok(pools)
    }

    async fn discover_v4(&self, token0: Address, token1: Address) -> Result<Vec<PoolKey>> {
        let mut keys = Vec::new();
        let mut slots = Vec::new();
        for fee in V3_FEE_TIERS {
            let tick_spacing = v4_tick_spacing_for_fee(fee);
            let id = crate::venues::v4::pool_id(token0, token1, fee, tick_spacing, Address::ZERO);
            let state_slot = pool_state_slot(id);
            keys.push((fee, tick_spacing, id));
            slots.push(B256::from(state_slot.to_be_bytes::<32>()));
        }
        let words = self.extsload_many(&slots).await?;
        let mut pools = Vec::new();
        for ((fee, tick_spacing, id), word) in keys.into_iter().zip(words) {
            if !word.is_zero() {
                pools.push(PoolKey {
                    venue: Venue::V4,
                    address: self.pool_manager,
                    token0,
                    token1,
                    fee,
                    tick_spacing,
                    hooks: Address::ZERO,
                    stable: false,
                    factory: Address::ZERO,
                    v4_pool_id: Some(id),
                });
            }
        }
        Ok(pools)
    }

    async fn discover_aero(
        &self,
        token: Address,
        hub: Address,
        token0: Address,
        token1: Address,
    ) -> Result<Vec<PoolKey>> {
        let mut pools = Vec::new();
        for stable in [false, true] {
            let data =
                IAerodromeFactory::getPoolCall { tokenA: token, tokenB: hub, stable }.abi_encode();
            let out = self
                .call(self.aero_factory, data, |b| {
                    IAerodromeFactory::getPoolCall::abi_decode_returns(b)
                        .map_err(|e| EngineError::Rpc(e.to_string()))
                })
                .await?;
            if out != Address::ZERO {
                pools.push(PoolKey {
                    venue: Venue::Aerodrome,
                    address: out,
                    token0,
                    token1,
                    fee: 0, // fee comes from the factory at state load
                    tick_spacing: 0,
                    hooks: Address::ZERO,
                    stable,
                    factory: self.aero_factory,
                    v4_pool_id: None,
                });
            }
        }
        Ok(pools)
    }

    /// Static per-pool metadata (24h tier).
    pub async fn load_meta(&self, key: &PoolKey) -> Result<PoolMeta> {
        let mut meta = PoolMeta { decimals0: 18, decimals1: 18, stable: key.stable, fee_bps: key.fee };
        if key.venue == Venue::Aerodrome {
            let data = IAerodromePool::decimals0Call {}.abi_encode();
            meta.decimals0 = self
                .call(key.address, data, |b| {
                    IAerodromePool::decimals0Call::abi_decode_returns(b)
                        .map_err(|e| EngineError::Rpc(e.to_string()))
                })
                .await?;
            let data = IAerodromePool::decimals1Call {}.abi_encode();
            meta.decimals1 = self
                .call(key.address, data, |b| {
                    IAerodromePool::decimals1Call::abi_decode_returns(b)
                        .map_err(|e| EngineError::Rpc(e.to_string()))
                })
                .await?;
        }
        Ok(meta)
    }

    /// Pool state for the reserves tier. `meta` supplies static data.
    pub async fn load_state(&self, key: &PoolKey, meta: &PoolMeta) -> Result<PoolState> {
        match key.venue {
            Venue::V2 => {
                let data = IUniswapV2Pair::getReservesCall {}.abi_encode();
                let r = self
                    .call(key.address, data, |b| {
                        Ok(IUniswapV2Pair::getReservesCall::abi_decode_returns(b)
                            .map_err(|e| EngineError::Rpc(e.to_string()))?)
                    })
                    .await?;
                Ok(PoolState::V2(V2State {
                    reserve0: uint_to_u256(r.reserve0),
                    reserve1: uint_to_u256(r.reserve1),
                    fee_bps: meta.fee_bps,
                }))
            }
            Venue::Aerodrome => {
                let data = IAerodromePool::getReservesCall {}.abi_encode();
                let r = self
                    .call(key.address, data, |b| {
                        Ok(IAerodromePool::getReservesCall::abi_decode_returns(b)
                            .map_err(|e| EngineError::Rpc(e.to_string()))?)
                    })
                    .await?;
                let fee_data = IAerodromeFactory::getFeeCall {
                    pool: key.address,
                    stable: key.stable,
                }
                .abi_encode();
                let fee_bps: u32 = self
                    .call(key.factory, fee_data, |b| {
                        Ok(IAerodromeFactory::getFeeCall::abi_decode_returns(b)
                            .map_err(|e| EngineError::Rpc(e.to_string()))?
                            .to::<u64>() as u32)
                    })
                    .await?;
                Ok(PoolState::Aero(AeroState {
                    stable: key.stable,
                    reserve0: r.reserve0,
                    reserve1: r.reserve1,
                    fee_bps,
                    decimals0: meta.decimals0,
                    decimals1: meta.decimals1,
                }))
            }
            Venue::V3 => {
                let s = self.load_v3_state(key, None).await?;
                Ok(PoolState::V3(s))
            }
            Venue::V4 => {
                let s = self.load_v4_state(key).await?;
                Ok(PoolState::V4(s))
            }
        }
    }

    async fn load_v3_state(&self, key: &PoolKey, forced_fee: Option<u32>) -> Result<V3State> {
        let data = IUniswapV3Pool::slot0Call {}.abi_encode();
        let slot0 = self
            .call(key.address, data, |b| {
                Ok(IUniswapV3Pool::slot0Call::abi_decode_returns(b)
                    .map_err(|e| EngineError::Rpc(e.to_string()))?)
            })
            .await?;
        let data = IUniswapV3Pool::liquidityCall {}.abi_encode();
        let liquidity = self
            .call(key.address, data, |b| {
                IUniswapV3Pool::liquidityCall::abi_decode_returns(b)
                    .map_err(|e| EngineError::Rpc(e.to_string()))
            })
            .await?;
        let fee_pips = match forced_fee {
            Some(f) => f,
            None => {
                let data = IUniswapV3Pool::feeCall {}.abi_encode();
                self.call(key.address, data, |b| {
                    Ok(IUniswapV3Pool::feeCall::abi_decode_returns(b)
                        .map_err(|e| EngineError::Rpc(e.to_string()))?
                        .to::<u32>())
                })
                .await?
            }
        };
        let ticks = self.load_v3_ticks(key.address, signed_i32(slot0.tick)).await?;
        Ok(V3State {
            sqrt_price_x96: uint_to_u256(slot0.sqrtPriceX96),
            liquidity,
            tick: signed_i32(slot0.tick),
            fee_pips,
            ticks,
            ticks_complete: false,
        })
    }

    async fn load_v3_ticks(&self, pool: Address, current_tick: i32) -> Result<Vec<TickData>> {
        // Pool.getTickSpacing() pins the bitmap stride.
        let data = IUniswapV3Pool::tickSpacingCall {}.abi_encode();
        let spacing = self
            .call(pool, data, |b| {
                Ok(IUniswapV3Pool::tickSpacingCall::abi_decode_returns(b)
                    .map_err(|e| EngineError::Rpc(e.to_string()))
                    .map(|v| signed_i32(v))?)
            })
            .await?;
        let mut out = Vec::new();
        let center = current_tick.div_euclid(spacing.max(1));
        for direction in [0i32, 1i32] {
            let mut word_pos = center >> 8;
            let mut collected = 0usize;
            let mut misses = 0usize;
            while collected < self.max_ticks_per_side && misses < 3 {
                let word_pos_i16 = i16::try_from(word_pos).unwrap_or(if direction == 0 { i16::MAX } else { i16::MIN });
                let data = IUniswapV3Pool::tickBitmapCall { wordPosition: word_pos_i16 }.abi_encode();
                let word = self
                    .call(pool, data, |b| {
                        IUniswapV3Pool::tickBitmapCall::abi_decode_returns(b)
                            .map_err(|e| EngineError::Rpc(e.to_string()))
                    })
                    .await?;
        let word = uint_to_u256(word);
                if word.is_zero() {
                    misses += 1;
                } else {
                    misses = 0;
                    for bit in 0..256u32 {
                        if word.bit(bit as usize) {
                            let t_index = ((word_pos as i64) << 8) + i64::from(bit);
                            let tick = t_index * i64::from(spacing);
                            if tick < i64::from(i32::MIN) || tick > i64::from(i32::MAX) {
                                continue;
                            }
                            let tick = tick as i32;
                            let data = IUniswapV3Pool::ticksCall { tick: make_i24(tick) }.abi_encode();
                            let info = self
                                .call(pool, data, |b| {
                                    Ok(IUniswapV3Pool::ticksCall::abi_decode_returns(b)
                                        .map_err(|e| EngineError::Rpc(e.to_string()))?)
                                })
                                .await?;
                            out.push(TickData {
                                tick,
                                liquidity_net: info.liquidityNet,
                                liquidity_gross: info.liquidityGross,
                            });
                            collected += 1;
                        }
                                          }
                }
                if direction == 0 {
                    word_pos += 1;
                } else {
                    word_pos -= 1;
                }
            }
        }
        out.sort_by_key(|t| t.tick);
        out.dedup_by_key(|t| t.tick);
        Ok(out)
    }

    async fn load_v4_state(&self, key: &PoolKey) -> Result<V4State> {
        let id = key
            .v4_pool_id
            .unwrap_or_else(|| {
                crate::venues::v4::pool_id(
                    key.token0,
                    key.token1,
                    key.fee,
                    key.tick_spacing,
                    key.hooks,
                )
            });
        let state_slot = pool_state_slot(id);
        let words = self
            .extsload_many(&[
                B256::from(state_slot.to_be_bytes::<32>()),
                B256::from((state_slot + U256::from(3)).to_be_bytes::<32>()),
            ])
            .await?;
        let slot0_word = U256::from_be_slice(words[0].as_slice());
        let liquidity = U256::from_be_slice(words[1].as_slice()).to::<u128>();
        let (sqrt, tick, _proto, lp_fee) = decode_slot0(slot0_word);
        if sqrt.is_zero() {
            return Err(EngineError::Quote("v4: pool has no state".to_string()));
        }
        let ticks = self.load_v4_ticks(id, state_slot, tick, key.tick_spacing.max(1)).await?;
        Ok(V4State {
            base: V3State {
                sqrt_price_x96: sqrt,
                liquidity,
                tick,
                fee_pips: lp_fee,
                ticks,
                ticks_complete: false,
            },
            hooks: key.hooks,
            dynamic_fee: lp_fee == 0,
        })
    }

    async fn load_v4_ticks(
        &self,
        _id: B256,
        state_slot: U256,
        current_tick: i32,
        spacing: i32,
    ) -> Result<Vec<TickData>> {
        let mut out = Vec::new();
        let center = current_tick.div_euclid(spacing);
        for direction in [0i32, 1i32] {
            let mut word_pos = center >> 8;
            let mut collected = 0usize;
            let mut misses = 0usize;
            while collected < self.max_ticks_per_side && misses < 3 {
                let word_slot = tick_bitmap_slot(state_slot, word_pos);
                let word = self
                    .extsload_many(&[B256::from(word_slot.to_be_bytes::<32>())])
                    .await?
                    .remove(0);
                let word = U256::from_be_slice(word.as_slice());
                if word.is_zero() {
                    misses += 1;
                } else {
                    misses = 0;
                    for bit in 0..256u32 {
                        if word.bit(bit as usize) {
                            let t_index = ((word_pos as i64) << 8) + i64::from(bit);
                            let tick = t_index * i64::from(spacing);
                            if tick < i64::from(i32::MIN) || tick > i64::from(i32::MAX) {
                                continue;
                            }
                            let tick = tick as i32;
                            let t_slot = tick_slot(state_slot, tick);
                            let t_word = self
                                .extsload_many(&[B256::from(t_slot.to_be_bytes::<32>())])
                                .await?
                                .remove(0);
                            let (gross, net) =
                                decode_tick_info(U256::from_be_slice(t_word.as_slice()));
                            out.push(TickData {
                                tick,
                                liquidity_net: net,
                                liquidity_gross: gross,
                            });
                            collected += 1;
                        }
                    }
                }
                              if direction == 0 {
                    word_pos += 1;
                } else {
                    word_pos -= 1;
                }
            }
        }
        out.sort_by_key(|t| t.tick);
        out.dedup_by_key(|t| t.tick);
        Ok(out)
    }

    async fn extsload_many(&self, slots: &[B256]) -> Result<Vec<B256>> {
        let data = IExtsloadMany::extsloadCall { slots: slots.to_vec() }.abi_encode();
        self.call(self.pool_manager, data, |b| {
            IExtsloadMany::extsloadCall::abi_decode_returns(b)
                .map_err(|e| EngineError::Rpc(e.to_string()))
        })
        .await
    }
}

/// Sign-aware narrowing for sol! signed ints.
pub fn signed_i32(x: I24) -> i32 {
    let (sign, abs) = x.into_sign_and_abs();
    let v = abs.to::<u32>();
    match sign {
        alloy::primitives::Sign::Negative => -(v as i32),
        _ => v as i32,
    }
}

/// i32 -> sol! int24 (two's complement in 24 bits).
pub fn make_i24(v: i32) -> I24 {
    I24::from_raw(U24::from((v as u32) & 0x00FF_FFFF))
}

/// Any ruint `Uint<BITS, LIMBS>` up to 256 bits -> U256.
pub fn uint_to_u256<const BITS: usize, const LIMBS: usize>(x: Uint<BITS, LIMBS>) -> U256 {
    let mut limbs = [0u64; 4];
    let src = x.as_limbs();
    limbs[..src.len().min(4)].copy_from_slice(&src[..src.len().min(4)]);
    U256::from_limbs(limbs)
}

fn ordered(a: Address, b: Address) -> (Address, Address) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}
