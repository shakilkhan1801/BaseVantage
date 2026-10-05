//! Test doubles for the `ChainAdapter` boundary. They ship with the crate so
//! integration tests and the engine's own unit tests share one set.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy::primitives::{Address, Bytes, U256};
use async_trait::async_trait;

use crate::chain::{
    BenchReport, CallRequest, ChainAdapter, EventFilter, EventStream, PoolEvent, RpcHealth,
};
use crate::error::Result;

/// A chain whose `eth_call` answers come from a scripted closure.
pub struct ScriptedChain {
    responder: Box<dyn Fn(&CallRequest) -> Result<Bytes> + Send + Sync>,
    calls: AtomicUsize,
    gas_price: U256,
    nonce: u64,
    gas_estimate: u64,
}

impl ScriptedChain {
    pub fn new<F>(responder: F) -> Arc<Self>
    where
        F: Fn(&CallRequest) -> Result<Bytes> + Send + Sync + 'static,
    {
        Arc::new(Self {
            responder: Box::new(responder),
            calls: AtomicUsize::new(0),
            gas_price: U256::from(1_000_000_000u64),
            nonce: 7,
            gas_estimate: 120_000,
        })
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ChainAdapter for ScriptedChain {
    async fn bench(&self) -> BenchReport {
        BenchReport { endpoints: Vec::new() }
    }

    fn health(&self) -> RpcHealth {
        RpcHealth { healthy: 1, total: 1, best: Some("scripted".to_string()) }
    }

    async fn gas_price(&self) -> Result<U256> {
        Ok(self.gas_price)
    }

    async fn nonce(&self, _account: Address) -> Result<u64> {
        Ok(self.nonce)
    }

    async fn call(&self, req: CallRequest) -> Result<Bytes> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        (self.responder)(&req)
    }

    async fn estimate_gas(&self, _req: CallRequest) -> Result<u64> {
        Ok(self.gas_estimate)
    }

    fn events(&self, _filter: EventFilter) -> EventStream {
        Box::pin(futures::stream::empty::<PoolEvent>())
    }
}

/// Wraps any adapter and tallies how many RPC round-trips it really made.
pub struct CountingAdapter {
    inner: Arc<dyn ChainAdapter>,
    calls: AtomicUsize,
    estimates: AtomicUsize,
}

impl CountingAdapter {
    pub fn new(inner: Arc<dyn ChainAdapter>) -> Arc<Self> {
        Arc::new(Self { inner, calls: AtomicUsize::new(0), estimates: AtomicUsize::new(0) })
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    pub fn estimate_count(&self) -> usize {
        self.estimates.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ChainAdapter for CountingAdapter {
    async fn bench(&self) -> BenchReport {
        self.inner.bench().await
    }

    fn health(&self) -> RpcHealth {
        self.inner.health()
    }

    async fn gas_price(&self) -> Result<U256> {
        self.inner.gas_price().await
    }

    async fn nonce(&self, account: Address) -> Result<u64> {
        self.inner.nonce(account).await
    }

    async fn call(&self, req: CallRequest) -> Result<Bytes> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.call(req).await
    }

    async fn estimate_gas(&self, req: CallRequest) -> Result<u64> {
        self.estimates.fetch_add(1, Ordering::Relaxed);
        self.inner.estimate_gas(req).await
    }

    fn events(&self, filter: EventFilter) -> EventStream {
        self.inner.events(filter)
    }
}
