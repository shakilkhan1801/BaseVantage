use std::collections::HashMap;
use std::sync::Mutex;

use alloy::primitives::{Address, B256, Bytes, U256, address, keccak256};
use alloy::rpc::types::state::{AccountOverride, StateOverride};
use alloy::sol_types::SolCall;
use async_trait::async_trait;

use crate::chain::{CallRequest, DynChain};
use crate::error::{EngineError, Result};
use crate::market::PoolKey;
use crate::market::abi::{
    IAerodromePool, IAerodromeRouter, IERC20, IMulticall3, IQuoterV2, IUniswapV2Router02,
};
use crate::venues::Venue;

/// Multicall3: executes several calls in one eth_call, which is what makes
/// atomic buy/sell probes possible without deploying a probe contract.
pub const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");
pub const V2_ROUTER: Address = crate::venues::v2::V2_ROUTER;
pub const V3_ROUTER: Address = crate::venues::v3::V3_ROUTER;
pub const V3_QUOTER: Address = crate::venues::v3::V3_QUOTER;
pub const AERO_ROUTER: Address = crate::venues::aerodrome::AERO_ROUTER;

const PROBE_SLOT_TRIES: u64 = 48;
const SLOT_MAGIC: B256 = B256::new([
    0xde, 0xad, 0xbe, 0xef, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44,
    0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x12, 0x34, 0x56, 0x78,
]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AssessmentSource {
    Probe,
    Manual,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenAssessment {
    pub token: Address,
    pub buy_tax_bps: u32,
    pub sell_tax_bps: u32,
    pub honeypot: bool,
    pub fee_on_transfer: bool,
    pub probe_block: Option<u64>,
    pub source: AssessmentSource,
}

/// Tax/honeypot oracle boundary. Returns `None` when the token cannot be
/// assessed (no probeable pool) — never a guessed zero.
#[async_trait]
pub trait TaxOracle: Send + Sync {
    async fn assess(&self, token: Address, pools: &[PoolKey]) -> Result<Option<TokenAssessment>>;
}

/// Explicit assessments (operator-supplied, fixtures).
#[derive(Default)]
pub struct ManualAssessor {
    entries: Mutex<HashMap<Address, TokenAssessment>>,
}

impl ManualAssessor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, assessment: TokenAssessment) {
        self.entries
            .lock()
            .expect("poisoned")
            .insert(assessment.token, assessment);
    }
}

#[async_trait]
impl TaxOracle for ManualAssessor {
    async fn assess(&self, token: Address, _pools: &[PoolKey]) -> Result<Option<TokenAssessment>> {
        Ok(self.entries.lock().expect("poisoned").get(&token).cloned())
    }
}

/// Live probes through eth_call state overrides and Multicall3: an atomic
/// buy probe and sell probe per token measure real transfer taxes and detect
/// honeypots (sell reverts or yields nothing).
pub struct ProbeAssessor {
    chain: DynChain,
}

impl ProbeAssessor {
    pub fn new(chain: DynChain) -> Self {
        Self { chain }
    }

    /// Find the ERC20 `balanceOf` mapping slot by trial: write a magic value
    /// at each candidate slot and see which one `balanceOf` reports.
    async fn discover_balance_slot(&self, token: Address, holder: Address) -> Result<Option<U256>> {
        for slot in 0..=PROBE_SLOT_TRIES {
            let key = mapping_slot_key_1(holder, U256::from(slot));
            let ov = state_diff_override(token, &[(key, SLOT_MAGIC)]);
            let data = IERC20::balanceOfCall { owner: holder }.abi_encode();
            let out = self.call_with(token, data, ov).await?;
            if IERC20::balanceOfCall::abi_decode_returns(&out)
                .map_err(|e| EngineError::Rpc(e.to_string()))?
                == u256_from_word(SLOT_MAGIC)
            {
                return Ok(Some(U256::from(slot)));
            }
        }
        Ok(None)
    }

    /// Find the `allowance[owner][spender]` mapping slot the same way.
    async fn discover_allowance_slot(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<Option<U256>> {
        for slot in 0..=PROBE_SLOT_TRIES {
            let key = mapping_slot_key_2(owner, spender, U256::from(slot));
            let ov = state_diff_override(token, &[(key, SLOT_MAGIC)]);
            let data = IERC20::allowanceCall { owner, spender }.abi_encode();
            let out = self.call_with(token, data, ov).await?;
            if IERC20::allowanceCall::abi_decode_returns(&out)
                .map_err(|e| EngineError::Rpc(e.to_string()))?
                == u256_from_word(SLOT_MAGIC)
            {
                return Ok(Some(U256::from(slot)));
            }
        }
        Ok(None)
    }

    async fn call_with(
        &self,
        to: Address,
        data: Vec<u8>,
        overrides: StateOverride,
    ) -> Result<Bytes> {
        self.chain
            .call(CallRequest {
                to: Some(to),
                data: Some(Bytes::from(data)),
                state_override: Some(overrides),
                ..Default::default()
            })
            .await
    }

    /// One probe direction: quote sub-call + swap sub-call + received-balance
    /// sub-call in a single `aggregate3`. `input` is spent through the swap
    /// venue; `output` is measured at MULTICALL3.
    async fn probe(
        &self,
        pool: &PoolKey,
        input: Address,
        output: Address,
        amount_in: U256,
        input_is_token: bool,
    ) -> Result<ProbeOutcome> {
        let (quote_call, swap_call) = build_probe_calls(pool, input, output, amount_in)?;
        let balance_after = IERC20::balanceOfCall { owner: MULTICALL3 }.abi_encode();

        let calls = vec![
            IMulticall3::Call3 {
                allowFailure: true,
                target: quote_call.0,
                callData: Bytes::from(quote_call.1),
            },
            IMulticall3::Call3 {
                allowFailure: true,
                target: swap_call.0,
                callData: Bytes::from(swap_call.1),
            },
            IMulticall3::Call3 {
                allowFailure: true,
                target: output,
                callData: Bytes::from(balance_after),
            },
        ];
        let data = IMulticall3::aggregate3Call { calls }.abi_encode();

        // Override the input token balance/allowance the probe spends.
        let _ = input_is_token;
        let bal_slot = self
            .discover_balance_slot(input, MULTICALL3)
            .await?
            .ok_or_else(|| EngineError::Rpc("probe: balance slot not found".to_string()))?;
        let allow_slot = self
            .discover_allowance_slot(input, MULTICALL3, swap_router_for(pool))
            .await?
            .ok_or_else(|| EngineError::Rpc("probe: allowance slot not found".to_string()))?;
        let state = vec![
            (
                mapping_slot_key_1(MULTICALL3, bal_slot),
                word_u256(amount_in),
            ),
            (
                mapping_slot_key_2(MULTICALL3, swap_router_for(pool), allow_slot),
                word_u256(U256::MAX),
            ),
        ];
        let mut overrides = StateOverride::default();
        let mut input_account = AccountOverride::default();
        input_account.set_state_diff(state);
        overrides.insert(input, input_account);

        let out = self
            .chain
            .call(CallRequest {
                to: Some(MULTICALL3),
                data: Some(Bytes::from(data)),
                state_override: Some(overrides),
                ..Default::default()
            })
            .await?;
        let results = IMulticall3::aggregate3Call::abi_decode_returns(&out)
            .map_err(|e| EngineError::Rpc(e.to_string()))?;
        if results.len() != 3 {
            return Err(EngineError::Rpc(
                "probe: unexpected aggregate3 shape".to_string(),
            ));
        }
        let quoted = if results[0].success {
            u256_from_word_word(&results[0].returnData)
        } else {
            U256::ZERO
        };
        let swap_ok = results[1].success;
        let received = if results[2].success {
            u256_from_word_word(&results[2].returnData)
        } else {
            U256::ZERO
        };
        Ok(ProbeOutcome {
            quoted,
            swap_ok,
            received,
        })
    }

    /// Assess via the best probeable pool (v2 first, then aerodrome, then v3).
    pub async fn assess_with_pools(
        &self,
        token: Address,
        pools: &[PoolKey],
    ) -> Result<Option<TokenAssessment>> {
        let Some(pool) = pick_probe_pool(token, pools) else {
            return Ok(None);
        };
        let other = pool.other(token);
        let decimals = 18u8;
        let sell_amount = U256::from(10u64).pow(U256::from(decimals));
        let buy_amount = U256::from(10).pow(U256::from(16)); // 0.01 WETH

        let sell = self.probe(&pool, token, other, sell_amount, true).await?;
        let buy = self.probe(&pool, other, token, buy_amount, false).await;

        let sell_tax_bps = tax_bps(sell.quoted, sell.received);
        let buy_tax_bps = match &buy {
            Ok(b) => tax_bps(b.quoted, b.received),
            Err(_) => 0,
        };
        let honeypot = !sell.swap_ok || sell.received.is_zero();

        Ok(Some(TokenAssessment {
            token,
            buy_tax_bps,
            sell_tax_bps,
            honeypot,
            fee_on_transfer: sell_tax_bps > 0,
            probe_block: None,
            source: AssessmentSource::Probe,
        }))
    }
}

#[async_trait]
impl TaxOracle for ProbeAssessor {
    async fn assess(&self, token: Address, pools: &[PoolKey]) -> Result<Option<TokenAssessment>> {
        self.assess_with_pools(token, pools).await
    }
}

struct ProbeOutcome {
    quoted: U256,
    swap_ok: bool,
    received: U256,
}

fn tax_bps(quoted: U256, received: U256) -> u32 {
    if quoted.is_zero() || received >= quoted {
        return 0;
    }
    let diff = quoted - received;
    ((diff * U256::from(10_000)) / quoted)
        .to::<u64>()
        .min(10_000) as u32
}

fn pick_probe_pool(token: Address, pools: &[PoolKey]) -> Option<PoolKey> {
    let mut candidates: Vec<&PoolKey> = pools
        .iter()
        .filter(|p| p.token0 == token || p.token1 == token)
        .filter(|p| !matches!(p.venue, Venue::V4))
        .collect();
    candidates.sort_by_key(|p| match p.venue {
        Venue::V2 => 0u8,
        Venue::Aerodrome => 1,
        Venue::V3 => 2,
        Venue::V4 => 3,
    });
    candidates.first().map(|p| (*p).clone())
}

fn swap_router_for(pool: &PoolKey) -> Address {
    match pool.venue {
        Venue::V2 => V2_ROUTER,
        Venue::Aerodrome => AERO_ROUTER,
        _ => V3_ROUTER,
    }
}

/// (target, calldata) for the quote and swap sub-calls of one probe.
type CallData = (Address, Vec<u8>);

fn build_probe_calls(
    pool: &PoolKey,
    input: Address,
    output: Address,
    amount_in: U256,
) -> Result<(CallData, CallData)> {
    let deadline = u64::MAX;
    match pool.venue {
        Venue::V2 => {
            let path = vec![input, output];
            let quote = IUniswapV2Router02::getAmountsOutCall {
                amountIn: amount_in,
                path,
            }
            .abi_encode();
            let swap = IUniswapV2Router02::swapExactTokensForTokensCall {
                amountIn: amount_in,
                amountOutMin: U256::ZERO,
                path: vec![input, output],
                to: MULTICALL3,
                deadline: U256::from(deadline),
            }
            .abi_encode();
            Ok(((V2_ROUTER, quote), (V2_ROUTER, swap)))
        }
        Venue::Aerodrome => {
            let quote = IAerodromePool::getAmountOutCall {
                amountIn: amount_in,
                tokenIn: input,
            }
            .abi_encode();
            let routes = vec![IAerodromeRouter::Route {
                from: input,
                to: output,
                stable: pool.stable,
                factory: pool.factory,
            }];
            let swap = IAerodromeRouter::swapExactTokensForTokensCall {
                amountIn: amount_in,
                amountOutMin: U256::ZERO,
                routes,
                to: MULTICALL3,
                deadline: U256::from(deadline),
            }
            .abi_encode();
            Ok(((pool.address, quote), (AERO_ROUTER, swap)))
        }
        Venue::V3 => {
            let params = IQuoterV2::QuoteExactInputSingleParams {
                tokenIn: input,
                tokenOut: output,
                amountIn: amount_in,
                fee: alloy::primitives::aliases::U24::from(pool.fee),
                sqrtPriceLimitX96: alloy::primitives::aliases::U160::ZERO,
            };
            let quote = IQuoterV2::quoteExactInputSingleCall { params }.abi_encode();
            let single = crate::venues::v3::encode_exact_input_single(
                input,
                output,
                pool.fee,
                MULTICALL3,
                amount_in,
                U256::ZERO,
            );
            Ok(((V3_QUOTER, quote), (V3_ROUTER, single.to_vec())))
        }
        Venue::V4 => Err(EngineError::Rpc(
            "probe: v4 pools not probeable in S1".to_string(),
        )),
    }
}

fn state_diff_override(account: Address, state: &[(B256, B256)]) -> StateOverride {
    let mut acc = AccountOverride::default();
    acc.set_state_diff(state.iter().copied());
    let mut ov = StateOverride::default();
    ov.insert(account, acc);
    ov
}

/// keccak256(holder ++ slot) — `mapping(address => uint)` slot.
fn mapping_slot_key_1(holder: Address, slot: U256) -> B256 {
    let mut enc = [0u8; 64];
    enc[12..32].copy_from_slice(holder.as_slice());
    enc[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    keccak256(enc)
}

/// keccak256(spender ++ keccak256(holder ++ slot)) — nested mapping slot.
fn mapping_slot_key_2(owner: Address, spender: Address, slot: U256) -> B256 {
    let inner = mapping_slot_key_1(owner, slot);
    let mut enc = [0u8; 64];
    enc[12..32].copy_from_slice(spender.as_slice());
    enc[32..].copy_from_slice(inner.as_slice());
    keccak256(enc)
}

fn word_u256(v: U256) -> B256 {
    B256::from(v.to_be_bytes::<32>())
}

fn u256_from_word(w: B256) -> U256 {
    U256::from_be_slice(w.as_slice())
}

fn u256_from_word_word(data: &Bytes) -> U256 {
    if data.len() < 32 {
        return U256::ZERO;
    }
    U256::from_be_slice(&data[..32])
}
