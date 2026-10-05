use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::FutureExt;

use crate::error::{EngineError, Result};

/// Where a value came from; surfaced with age on every cached value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Source {
    Registry,
    ChainRpc,
    WsEvent,
    External,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Registry => "registry",
            Source::ChainRpc => "chain-rpc",
            Source::WsEvent => "ws-event",
            Source::External => "external",
        }
    }
}

/// Cache tier: which TTL applies. Negative results have their own tier so a
/// missing pool or absent stat cannot hammer the RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Static,
    Reserves,
    Stats,
    Negative,
}

#[derive(Debug, Clone, Copy)]
pub struct TtlConfig {
    pub static_ttl: Duration,
    pub reserves_ttl: Duration,
    pub stats_ttl: Duration,
    pub negative_ttl: Duration,
}

impl TtlConfig {
    fn ttl_for(&self, tier: Tier) -> Duration {
        match tier {
            Tier::Static => self.static_ttl,
            Tier::Reserves => self.reserves_ttl,
            Tier::Stats => self.stats_ttl,
            Tier::Negative => self.negative_ttl,
        }
    }
}

/// A cached value with its provenance label and age.
#[derive(Debug)]
pub struct Labeled<T> {
    pub value: Arc<T>,
    pub source: Source,
    pub fetched_at: Instant,
}

impl<T> Clone for Labeled<T> {
    fn clone(&self) -> Self {
        Self { value: self.value.clone(), source: self.source, fetched_at: self.fetched_at }
    }
}

impl<T> Labeled<T> {
    pub fn age(&self) -> Duration {
        self.fetched_at.elapsed()
    }

    /// e.g. `chain-rpc age 3.2s` — the source+age label the CLI prints.
    pub fn label(&self) -> String {
        format!("{} age {:.1?}", self.source.label(), self.age())
    }
}

#[derive(Debug, Clone)]
struct Entry<V> {
    value: Option<Arc<V>>, // None = negative hit
    source: Source,
    fetched_at: Instant,
    tier: Tier,
}

type FetchFuture<V> = BoxFuture<'static, std::result::Result<Option<Arc<V>>, Arc<EngineError>>>;
type SharedFetch<V> = futures::future::Shared<FetchFuture<V>>;

/// Tiered TTL cache with single-flight misses: concurrent misses on one key
/// share one in-flight fetch.
pub struct TieredCache<K, V> {
    entries: Mutex<HashMap<K, Entry<V>>>,
    inflight: Mutex<HashMap<K, SharedFetch<V>>>,
    ttls: TtlConfig,
}

impl<K, V> TieredCache<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    pub fn new(ttls: TtlConfig) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            ttls,
        }
    }

    /// Current hit without fetching. Fresh negative returns `Ok(None)`.
    pub fn get(&self, key: &K) -> Result<Option<Labeled<V>>> {
        let entries = self.entries.lock().expect("cache poisoned");
        match entries.get(key) {
            Some(e) if self.ttls.ttl_for(e.tier) > e.fetched_at.elapsed() => {
                Ok(e.value.clone().map(|value| Labeled {
                    value,
                    source: e.source,
                    fetched_at: e.fetched_at,
                }))
            }
            _ => Err(EngineError::Cache("miss".to_string())),
        }
    }

    /// Raw entry peek (even stale) — used by persistence and tests.
    pub fn peek(&self, key: &K) -> Option<(Option<Arc<V>>, Source, Instant, Tier)> {
        let entries = self.entries.lock().expect("cache poisoned");
        entries.get(key).map(|e| (e.value.clone(), e.source, e.fetched_at, e.tier))
    }

    pub fn insert(&self, key: K, value: V, source: Source, tier: Tier) -> Labeled<V> {
        let value = Arc::new(value);
        self.insert_arc(key, value, source, tier)
    }

    pub fn insert_arc(&self, key: K, value: Arc<V>, source: Source, tier: Tier) -> Labeled<V> {
        let fetched_at = Instant::now();
        self.entries.lock().expect("cache poisoned").insert(
            key,
            Entry { value: Some(value.clone()), source, fetched_at, tier },
        );
        Labeled { value, source, fetched_at }
    }

    /// Record a confirmed absence under the negative tier.
    pub fn insert_negative(&self, key: K, source: Source) {
        self.entries.lock().expect("cache poisoned").insert(
            key,
            Entry { value: None, source, fetched_at: Instant::now(), tier: Tier::Negative },
        );
    }

    /// Drop one key so the next read refetches (event invalidation).
    pub fn invalidate(&self, key: &K) {
        self.entries.lock().expect("cache poisoned").remove(key);
    }

    /// Hit-or-single-flight-fetch. `Ok(None)` means a fresh negative hit.
    pub async fn get_or_fetch<F>(&self, key: K, tier: Tier, source: Source, fetch: F)
    -> Result<Option<Labeled<V>>>
    where
        F: std::future::Future<Output = Result<Option<V>>> + Send + 'static,
    {
        if let Ok(hit) = self.get(&key) {
            return Ok(hit);
        }

        let shared = {
            let mut inflight = self.inflight.lock().expect("cache poisoned");
            if let Some(existing) = inflight.get(&key) {
                existing.clone()
            } else {
                let fut: SharedFetch<V> =
                    async move { fetch.await.map(|opt| opt.map(Arc::new)).map_err(Arc::new) }
                        .boxed()
                        .shared();
                inflight.insert(key.clone(), fut.clone());
                fut
            }
        };

        let outcome = shared.clone().await;
        self.inflight.lock().expect("cache poisoned").remove(&key);

        match outcome {
            Ok(Some(value)) => Ok(Some(self.insert_arc(key, value, source, tier))),
            Ok(None) => {
                self.insert_negative(key, source);
                Ok(None)
            }
            Err(e) => Err(EngineError::Cache(format!("{e}"))),
        }
    }

    /// All live entries — persistence snapshot source.
    pub fn snapshot_live(&self) -> Vec<(K, Arc<V>, Source, Instant, Tier)> {
        self.entries
            .lock()
            .expect("cache poisoned")
            .iter()
            .filter_map(|(k, e)| {
                e.value.clone().map(|v| (k.clone(), v, e.source, e.fetched_at, e.tier))
            })
            .collect()
    }

    /// Restore a persisted entry (static tier survives restarts).
    pub fn restore(&self, key: K, value: Arc<V>, source: Source, tier: Tier, fetched_at: Instant) {
        self.entries.lock().expect("cache poisoned").insert(
            key,
            Entry { value: Some(value), source, fetched_at, tier },
        );
    }
}

/// Snapshot record for on-disk persistence of the static tier.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct StaticRecord<K, V> {
    pub key: K,
    pub value: V,
    pub source: Source,
    pub fetched_at_epoch_ms: u128,
}
