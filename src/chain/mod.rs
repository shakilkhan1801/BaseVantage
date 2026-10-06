use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes, U256};
use alloy::rpc::types::state::StateOverride;
use async_trait::async_trait;

pub mod base;
pub mod testing;
pub mod ws;

pub use base::{BaseChain, EndpointBench, RpcPool};
pub use ws::{decode_event, stream_events};

/// A request to execute a message call, pinned to an optional block.
#[derive(Debug, Clone, Default)]
pub struct CallRequest {
    pub to: Option<Address>,
    pub from: Option<Address>,
    pub data: Option<Bytes>,
    pub value: Option<U256>,
    pub gas: Option<u64>,
    pub block: Option<BlockId>,
    /// State overrides for `eth_call` (assessment probes use them).
    pub state_override: Option<StateOverride>,
}

/// Latency/score snapshot for every pooled endpoint.
#[derive(Debug, Clone)]
pub struct BenchReport {
    pub endpoints: Vec<EndpointBench>,
}

#[derive(Debug, Clone)]
pub struct RpcHealth {
    pub healthy: usize,
    pub total: usize,
    pub best: Option<String>,
}

/// Pool state change surfaced by the event source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolEvent {
    Sync {
        pool: Address,
        reserve0: U256,
        reserve1: U256,
        block: u64,
    },
    Swap {
        pool: Address,
        amount0_in: U256,
        amount1_in: U256,
        block: u64,
    },
    Mint {
        pool: Address,
        block: u64,
    },
    Burn {
        pool: Address,
        block: u64,
    },
    // v3/v4: price/liquidity changed; exact deltas are re-read from chain.
    SlotUpdate {
        pool: Address,
        block: u64,
    },
}

#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub addresses: Vec<Address>,
}

/// The single chain-IO boundary of the engine. Everything above L1 talks to
/// this trait; test doubles implement it (see `chain::testing`).
#[async_trait]
pub trait ChainAdapter: Send + Sync {
    /// Time every endpoint (boot and on failure). Updates pool scoring.
    /// Async: the design-doc sketch showed a sync signature, but benching does
    /// network I/O and must not block the runtime.
    async fn bench(&self) -> BenchReport;
    /// Current scored pool state.
    fn health(&self) -> RpcHealth;
    async fn gas_price(&self) -> crate::error::Result<U256>;
    async fn nonce(&self, account: Address) -> crate::error::Result<u64>;
    async fn call(&self, req: CallRequest) -> crate::error::Result<Bytes>;
    async fn estimate_gas(&self, req: CallRequest) -> crate::error::Result<u64>;
    /// WS-first event stream with RPC polling fallback.
    fn events(&self, filter: EventFilter) -> EventStream;
}

/// Boxed event stream; the trait stays object-safe through async-trait.
pub type EventStream = std::pin::Pin<Box<dyn futures::Stream<Item = PoolEvent> + Send + 'static>>;

pub type DynChain = Arc<dyn ChainAdapter>;
