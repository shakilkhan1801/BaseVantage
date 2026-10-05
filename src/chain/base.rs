use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use async_trait::async_trait;

use crate::chain::{BenchReport, CallRequest, ChainAdapter, EventFilter, EventStream, RpcHealth};
use crate::error::{EngineError, Result};

/// One pooled HTTP endpoint with a rolling health score.
pub struct PoolEndpoint {
    pub url: String,
    provider: DynProvider,
    score: AtomicU64,
    last_latency_ms: AtomicU64,
    healthy: AtomicBool,
}

impl PoolEndpoint {
    fn new(url: String) -> Result<Self> {
        let provider = ProviderBuilder::new()
            .connect_http(url.parse().map_err(|e| EngineError::Rpc(format!("bad url {url}: {e}")))?)
            .erased();
        Ok(Self {
            url,
            provider,
            score: AtomicU64::new(0),
            last_latency_ms: AtomicU64::new(u64::MAX),
            healthy: AtomicBool::new(false),
        })
    }

    pub fn provider(&self) -> &DynProvider {
        &self.provider
    }

    pub fn score(&self) -> u64 {
        self.score.load(Ordering::Relaxed)
    }

    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone)]
pub struct EndpointBench {
    pub url: String,
    pub latency_ms: u64,
    pub ok: bool,
    pub score: u64,
}

/// Round-robins scored endpoints; benching runs at boot and after failures.
pub struct RpcPool {
    endpoints: Vec<Arc<PoolEndpoint>>,
    rr: AtomicU64,
}

impl RpcPool {
    pub fn new(urls: &[String]) -> Result<Self> {
        if urls.is_empty() {
            return Err(EngineError::NoHealthyEndpoint);
        }
        let mut endpoints = Vec::with_capacity(urls.len());
        for url in urls {
            endpoints.push(Arc::new(PoolEndpoint::new(url.clone())?));
        }
        Ok(Self { endpoints, rr: AtomicU64::new(0) })
    }

    /// Time every endpoint and re-score. Latency and liveness both count.
    pub async fn bench_all(&self) -> BenchReport {
        let mut rows = Vec::with_capacity(self.endpoints.len());
        for ep in &self.endpoints {
            let started = Instant::now();
            let ok = ep.provider.get_chain_id().await.is_ok();
            let latency_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            ep.last_latency_ms.store(latency_ms, Ordering::Relaxed);
            ep.healthy.store(ok, Ordering::Relaxed);
            // Higher score = better. Alive beats dead; among the alive, faster wins.
            let score = if ok {
                1_000_000u64.saturating_sub(latency_ms.saturating_mul(10))
            } else {
                0
            };
            ep.score.store(score, Ordering::Relaxed);
            rows.push(EndpointBench { url: ep.url.clone(), latency_ms, ok, score });
        }
        BenchReport { endpoints: rows }
    }

    pub fn health(&self) -> RpcHealth {
        let healthy = self.endpoints.iter().filter(|e| e.healthy()).count();
        let best = self
            .endpoints
            .iter()
            .filter(|e| e.healthy())
            .max_by_key(|e| e.score())
            .map(|e| e.url.clone());
        RpcHealth { healthy, total: self.endpoints.len(), best }
    }

    /// Endpoints best-first (score, then round-robin tiebreak), then the rest.
    fn ranked(&self) -> Vec<Arc<PoolEndpoint>> {
        let mut eps: Vec<_> = self.endpoints.clone();
        let rr = self.rr.fetch_add(1, Ordering::Relaxed);
        eps.sort_by_key(|e| std::cmp::Reverse((e.healthy(), e.score())));
        if eps.len() > 1 {
            let n = (rr as usize) % eps.len();
            eps.rotate_left(n);
        }
        eps
    }

    /// Try ranked endpoints in order until one answers.
    pub async fn with_provider<T, F, Fut>(&self, mut f: F) -> Result<T>
    where
        F: FnMut(DynProvider) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut last: Option<EngineError> = None;
        for ep in self.ranked() {
            match f(ep.provider.clone()).await {
                Ok(v) => {
                    ep.healthy.store(true, Ordering::Relaxed);
                    return Ok(v);
                }
                Err(e) => {
                    ep.healthy.store(false, Ordering::Relaxed);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or(EngineError::NoHealthyEndpoint))
    }
}

pub struct BaseChain {
    pool: Arc<RpcPool>,
    ws_urls: Vec<String>,
    bench_interval: Duration,
}

impl BaseChain {
    pub fn new(http_urls: &[String], ws_urls: Vec<String>, bench_interval_secs: u64) -> Result<Self> {
        let pool = Arc::new(RpcPool::new(http_urls)?);
        Ok(Self {
            pool,
            ws_urls,
            bench_interval: Duration::from_secs(bench_interval_secs.max(1)),
        })
    }

    pub fn pool(&self) -> &Arc<RpcPool> {
        &self.pool
    }

    pub fn bench_interval(&self) -> Duration {
        self.bench_interval
    }

    fn to_tx(req: &CallRequest) -> TransactionRequest {
        let mut tx = TransactionRequest::default();
        if let Some(to) = req.to {
            tx = tx.with_to(to);
        }
        if let Some(from) = req.from {
            tx = tx.with_from(from);
        }
        if let Some(data) = &req.data {
            tx = tx.with_input(data.clone());
        }
        if let Some(value) = req.value {
            tx = tx.with_value(value);
        }
        if let Some(gas) = req.gas {
            tx = tx.with_gas_limit(gas);
        }
        tx
    }
}

#[async_trait]
impl ChainAdapter for BaseChain {
    async fn bench(&self) -> BenchReport {
        self.pool.bench_all().await
    }

    fn health(&self) -> RpcHealth {
        self.pool.health()
    }

    async fn gas_price(&self) -> Result<U256> {
        self.pool
            .with_provider(|p| async move { Ok(U256::from(p.get_gas_price().await.map_err(rpc_err)?)) })
            .await
    }

    async fn nonce(&self, account: Address) -> Result<u64> {
        self.pool
            .with_provider(|p| async move {
                p.get_transaction_count(account).await.map_err(rpc_err)
            })
            .await
    }

    async fn call(&self, req: CallRequest) -> Result<Bytes> {
        let block = req.block;
        let overrides = req.state_override.clone();
        let tx = Self::to_tx(&req);
        self.pool
            .with_provider(move |p| {
                let tx = tx.clone();
                let overrides = overrides.clone();
                async move {
                    let mut call = p.call(tx);
                    if let Some(b) = block {
                        call = call.block(b);
                    }
                    if let Some(ov) = overrides {
                        call = call.overrides(ov);
                    }
                    call.await.map_err(rpc_err)
                }
            })
            .await
    }

    async fn estimate_gas(&self, req: CallRequest) -> Result<u64> {
        let block = req.block;
        let tx = Self::to_tx(&req);
        self.pool
            .with_provider(move |p| {
                let tx = tx.clone();
                async move {
                    let mut call = p.estimate_gas(tx);
                    if let Some(b) = block {
                        call = call.block(b);
                    }
                    call.await.map_err(rpc_err)
                }
            })
            .await
    }

    fn events(&self, filter: EventFilter) -> EventStream {
        crate::chain::ws::stream_events(self.ws_urls.clone(), filter, self.pool.clone())
    }
}

pub(crate) fn rpc_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::Rpc(e.to_string())
}

/// Convenience: read a contract's `eth_call` result.
pub async fn eth_call(
    chain: &dyn ChainAdapter,
    to: Address,
    data: Bytes,
    block: Option<alloy::eips::BlockId>,
) -> Result<Bytes> {
    chain.call(CallRequest { to: Some(to), data: Some(data), block, ..Default::default() }).await
}
