use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256};
use alloy::sol_types::SolCall;
use async_trait::async_trait;

pub mod abi;
pub mod cache;
pub mod pool_state;
pub mod registry;

pub use cache::{Labeled, Source, Tier, TieredCache, TtlConfig};
pub use registry::Registry;

use crate::chain::{DynChain, PoolEvent};
use crate::error::{EngineError, Result};
use crate::market::abi::IERC20;
use crate::venues::{PoolState, Venue};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenInfo {
    pub address: Address,
    pub symbol: String,
    pub name: String,
    pub decimals: u8,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PoolMeta {
    pub decimals0: u8,
    pub decimals1: u8,
    pub stable: bool,
    pub fee_bps: u32,
}

/// External market stats (60s tier).
#[derive(Debug, Clone)]
pub struct TokenStats {
    pub volume_24h: U256,
    pub txns_24h: u64,
    pub holders: u64,
}

use alloy::primitives::U256;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PoolKey {
    pub venue: Venue,
    /// Pool contract address. For v4 this is the PoolManager.
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    /// v2/aerodrome: fee bps. v3/v4: fee pips.
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Address,
    pub stable: bool,
    pub factory: Address,
    #[serde(default)]
    pub v4_pool_id: Option<B256>,
}

impl PoolKey {
    pub fn zero_for_one(&self, token_in: Address) -> bool {
        token_in == self.token0
    }

    pub fn other(&self, token: Address) -> Address {
        if token == self.token0 {
            self.token1
        } else {
            self.token0
        }
    }

    pub fn label(&self) -> String {
        match self.venue {
            Venue::Aerodrome => format!("aerodrome {}", if self.stable { "stable" } else { "vol" }),
            Venue::V3 => format!("v3 {}/10000", self.fee),
            Venue::V4 => format!("v4 {}/10000", self.fee),
            Venue::V2 => "v2".to_string(),
        }
    }
}

/// Pluggable external stats source.
#[async_trait]
pub trait StatsSource: Send + Sync {
    async fn stats(&self, token: Address) -> Result<Option<TokenStats>>;
}

pub struct NoopStatsSource;

#[async_trait]
impl StatsSource for NoopStatsSource {
    async fn stats(&self, _token: Address) -> Result<Option<TokenStats>> {
        Ok(None)
    }
}

/// The market-data facade: the only read surface above L1. Every value it
/// hands out carries a `(Source, Age)` label.
pub struct MarketData {
    chain: DynChain,
    registry: Registry,
    tokens: TieredCache<Address, TokenInfo>,
    discovery: TieredCache<Address, Vec<PoolKey>>,
    metas: TieredCache<PoolKey, PoolMeta>,
    states: TieredCache<PoolKey, PoolState>,
    stats: TieredCache<Address, TokenStats>,
    stats_source: Arc<dyn StatsSource>,
    pool_index: Mutex<HashMap<Address, PoolKey>>,
    static_store_path: PathBuf,
    persist_writes: AtomicU64,
}

impl MarketData {
    pub fn new(
        chain: DynChain,
        registry: Registry,
        ttls: TtlConfig,
        stats_source: Arc<dyn StatsSource>,
        static_store_path: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            chain,
            registry,
            tokens: TieredCache::new(ttls),
            discovery: TieredCache::new(ttls),
            metas: TieredCache::new(ttls),
            states: TieredCache::new(ttls),
            stats: TieredCache::new(ttls),
            stats_source,
            pool_index: Mutex::new(HashMap::new()),
            static_store_path,
            persist_writes: AtomicU64::new(0),
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Token metadata (static 24h tier).
    pub async fn token(&self, addr: Address) -> Result<Option<Labeled<TokenInfo>>> {
        let chain = self.chain.clone();
        let hit = self
            .tokens
            .get_or_fetch(addr, Tier::Static, Source::Registry, async move {
                let symbol =
                    call_erc20_string(&chain, addr, IERC20::symbolCall {}.abi_encode()).await?;
                let name =
                    call_erc20_string(&chain, addr, IERC20::nameCall {}.abi_encode()).await?;
                let decimals_data = IERC20::decimalsCall {}.abi_encode();
                let out = chain
                    .call(crate::chain::CallRequest {
                        to: Some(addr),
                        data: Some(alloy::primitives::Bytes::from(decimals_data)),
                        ..Default::default()
                    })
                    .await?;
                let decimals = IERC20::decimalsCall::abi_decode_returns(&out)
                    .map_err(|e| EngineError::Rpc(e.to_string()))?;
                Ok(Some(TokenInfo {
                    address: addr,
                    symbol,
                    name,
                    decimals,
                }))
            })
            .await?;
        if hit.is_some() {
            self.save_static_snapshot();
        }
        Ok(hit)
    }

    /// Pools for `token` against the hub set (static tier, negative cached).
    pub async fn pools_for(
        &self,
        token: Address,
        hubs: &[Address],
    ) -> Result<Option<Labeled<Vec<PoolKey>>>> {
        let hubs = hubs.to_vec();
        let registry = self.registry.clone();
        let fetch = async move {
            let pools = registry.discover(token, &hubs).await?;
            if pools.is_empty() {
                Ok(None)
            } else {
                Ok(Some(pools))
            }
        };
        let hit = self
            .discovery
            .get_or_fetch(token, Tier::Static, Source::Registry, fetch)
            .await?;
        if let Some(h) = &hit {
            for key in h.value.iter() {
                self.pool_index
                    .lock()
                    .expect("poisoned")
                    .insert(key.address, key.clone());
            }
            self.save_static_snapshot();
        }
        Ok(hit)
    }

    /// Static pool metadata (24h tier).
    pub async fn pool_meta(&self, key: &PoolKey) -> Result<Option<Labeled<PoolMeta>>> {
        let key_owned = key.clone();
        let registry = self.registry.clone();
        let fetch = async move { registry.load_meta(&key_owned).await.map(Some) };
        let hit = self
            .metas
            .get_or_fetch(key.clone(), Tier::Static, Source::Registry, fetch)
            .await?;
        if hit.is_some() {
            self.save_static_snapshot();
        }
        Ok(hit)
    }

    /// Pool state (reserves tier: 30s TTL + WS-event refresh).
    pub async fn pool_state(&self, key: &PoolKey) -> Result<Option<Labeled<PoolState>>> {
        self.pool_index
            .lock()
            .expect("poisoned")
            .insert(key.address, key.clone());
        let meta = self.pool_meta(key).await?;
        let key_owned = key.clone();
        let registry = self.registry.clone();
        let fetch = async move {
            let meta = match meta {
                Some(m) => m.value.clone(),
                None => return Ok(None),
            };
            registry.load_state(&key_owned, &meta).await.map(Some)
        };
        self.states
            .get_or_fetch(key.clone(), Tier::Reserves, Source::ChainRpc, fetch)
            .await
    }

    /// External stats (60s tier, negative cached).
    pub async fn stats_for(&self, token: Address) -> Result<Option<Labeled<TokenStats>>> {
        let source = self.stats_source.clone();
        self.stats
            .get_or_fetch(token, Tier::Stats, Source::External, async move {
                source.stats(token).await
            })
            .await
    }

    /// Inject token metadata (event-driven updates and offline tests).
    pub fn inject_token(&self, info: TokenInfo) {
        self.tokens
            .insert(info.address, info, Source::Registry, Tier::Static);
    }

    /// Inject discovery results for a token.
    pub fn inject_pools(&self, token: Address, pools: Vec<PoolKey>) {
        for key in &pools {
            self.pool_index
                .lock()
                .expect("poisoned")
                .insert(key.address, key.clone());
        }
        self.discovery
            .insert(token, pools, Source::Registry, Tier::Static);
    }

    /// Inject static pool metadata.
    pub fn inject_meta(&self, key: PoolKey, meta: PoolMeta) {
        self.metas.insert(key, meta, Source::Registry, Tier::Static);
    }

    /// Inject a state directly (event-driven refresh, tests).
    pub fn inject_state(&self, key: PoolKey, state: PoolState, source: Source) {
        self.pool_index
            .lock()
            .expect("poisoned")
            .insert(key.address, key.clone());
        self.states.insert(key, state, source, Tier::Reserves);
    }

    /// Apply a pool event: patch what the event carries, invalidate the rest.
    pub fn apply_event(&self, event: &PoolEvent) {
        match event {
            PoolEvent::Sync {
                pool,
                reserve0,
                reserve1,
                ..
            } => {
                let key = self.pool_index.lock().expect("poisoned").get(pool).cloned();
                if let Some(key) = key {
                    let updated = match self.states.peek(&key) {
                        Some((Some(state), _, _, _)) => match &*state {
                            PoolState::V2(s) => Some(PoolState::V2(crate::venues::V2State {
                                reserve0: *reserve0,
                                reserve1: *reserve1,
                                ..s.clone()
                            })),
                            PoolState::Aero(s) => Some(PoolState::Aero(crate::venues::AeroState {
                                reserve0: *reserve0,
                                reserve1: *reserve1,
                                ..s.clone()
                            })),
                            _ => None,
                        },
                        _ => None,
                    };
                    match updated {
                        Some(state) => self.inject_state(key, state, Source::WsEvent),
                        None => self.states.invalidate(&key),
                    }
                }
            }
            PoolEvent::Swap { pool, .. }
            | PoolEvent::Mint { pool, .. }
            | PoolEvent::Burn { pool, .. }
            | PoolEvent::SlotUpdate { pool, .. } => {
                let key = self.pool_index.lock().expect("poisoned").get(pool).cloned();
                if let Some(key) = key {
                    self.states.invalidate(&key);
                }
            }
        }
    }

    /// Number of live static-tier entries (persistence tests).
    pub fn static_entry_count(&self) -> usize {
        self.tokens.snapshot_live().len()
            + self.discovery.snapshot_live().len()
            + self.metas.snapshot_live().len()
    }

    fn save_static_snapshot(&self) {
        let snap = self.build_snapshot();
        if let Ok(json) = serde_json::to_string_pretty(&snap)
            && std::fs::write(&self.static_store_path, json).is_ok()
        {
            self.persist_writes.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Snapshot writes performed (tests).
    pub fn persist_writes(&self) -> u64 {
        self.persist_writes.load(Ordering::Relaxed)
    }

    fn build_snapshot(&self) -> Snapshot {
        let now = epoch_ms();
        let mut snap = Snapshot {
            saved_at_epoch_ms: now,
            tokens: Vec::new(),
            metas: Vec::new(),
            discoveries: Vec::new(),
        };
        for (addr, value, _source, fetched_at, _) in self.tokens.snapshot_live() {
            snap.tokens.push(SnapshotToken {
                key: addr,
                value: (*value).clone(),
                fetched_at_epoch_ms: now.saturating_sub(fetched_at.elapsed().as_millis()),
            });
        }
        for (key, value, _source, fetched_at, _) in self.metas.snapshot_live() {
            snap.metas.push(SnapshotMeta {
                key,
                value: (*value).clone(),
                fetched_at_epoch_ms: now.saturating_sub(fetched_at.elapsed().as_millis()),
            });
        }
        for (key, value, _source, fetched_at, _) in self.discovery.snapshot_live() {
            snap.discoveries.push(SnapshotDiscovery {
                key,
                value: (*value).clone(),
                fetched_at_epoch_ms: now.saturating_sub(fetched_at.elapsed().as_millis()),
            });
        }
        snap
    }

    /// Restore the persisted static tier. Returns restored entry count.
    /// Entries whose recorded age already exceeds the static TTL are dropped.
    pub fn load_static_snapshot(&self, static_ttl: Duration) -> usize {
        let Ok(json) = std::fs::read_to_string(&self.static_store_path) else {
            return 0;
        };
        let Ok(snap) = serde_json::from_str::<Snapshot>(&json) else {
            return 0;
        };
        let now = epoch_ms();
        let mut restored = 0usize;
        let restore = |fetched_at_epoch_ms: u128| -> Option<Instant> {
            let age_ms = now.saturating_sub(fetched_at_epoch_ms) as u64;
            if Duration::from_millis(age_ms) < static_ttl {
                Some(Instant::now() - Duration::from_millis(age_ms))
            } else {
                None
            }
        };
        for entry in snap.tokens {
            if let Some(at) = restore(entry.fetched_at_epoch_ms) {
                self.tokens.restore(
                    entry.key,
                    Arc::new(entry.value),
                    Source::Registry,
                    Tier::Static,
                    at,
                );
                restored += 1;
            }
        }
        for entry in snap.metas {
            if let Some(at) = restore(entry.fetched_at_epoch_ms) {
                self.metas.restore(
                    entry.key,
                    Arc::new(entry.value),
                    Source::Registry,
                    Tier::Static,
                    at,
                );
                restored += 1;
            }
        }
        for entry in snap.discoveries {
            if let Some(at) = restore(entry.fetched_at_epoch_ms) {
                for key in &entry.value {
                    self.pool_index
                        .lock()
                        .expect("poisoned")
                        .insert(key.address, key.clone());
                }
                self.discovery.restore(
                    entry.key,
                    Arc::new(entry.value),
                    Source::Registry,
                    Tier::Static,
                    at,
                );
                restored += 1;
            }
        }
        restored
    }
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `symbol()`/`name()` are both single-string returns; decode uniformly.
async fn call_erc20_string(chain: &DynChain, to: Address, data: Vec<u8>) -> Result<String> {
    let out = chain
        .call(crate::chain::CallRequest {
            to: Some(to),
            data: Some(alloy::primitives::Bytes::from(data)),
            ..Default::default()
        })
        .await?;
    IERC20::symbolCall::abi_decode_returns(&out).map_err(|e| EngineError::Rpc(e.to_string()))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Snapshot {
    saved_at_epoch_ms: u128,
    tokens: Vec<SnapshotToken>,
    metas: Vec<SnapshotMeta>,
    discoveries: Vec<SnapshotDiscovery>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotToken {
    key: Address,
    value: TokenInfo,
    fetched_at_epoch_ms: u128,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotMeta {
    key: PoolKey,
    value: PoolMeta,
    fetched_at_epoch_ms: u128,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotDiscovery {
    key: Address,
    value: Vec<PoolKey>,
    fetched_at_epoch_ms: u128,
}
