use std::path::{Path, PathBuf};

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

use crate::error::{checksum_ok, ConfigViolation, EngineError, Result};

/// Effective engine mode. `observe` never sends transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Observe,
    Execute,
}

impl Mode {
    pub fn banner(self) -> &'static str {
        match self {
            Mode::Observe => "mode: observe (no transactions sent)",
            Mode::Execute => "mode: execute (S2+ gate — sends disabled in S1)",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub engine: EngineSection,
    pub rpc: RpcSection,
    pub cache: CacheSection,
    pub safety: SafetySection,
    pub watchlist: WatchlistSection,
    pub settlement: SettlementSection,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EngineSection {
    pub mode: Mode,
    pub chain_id: u64,
    pub settlement_asset: String,
    pub wrapped_native: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcSection {
    pub urls: Vec<String>,
    #[serde(default)]
    pub ws_urls: Vec<String>,
    #[serde(default = "default_bench_interval")]
    pub bench_interval_secs: u64,
}

fn default_bench_interval() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheSection {
    pub static_ttl_secs: u64,
    pub reserves_ttl_secs: u64,
    pub stats_ttl_secs: u64,
    pub negative_ttl_secs: u64,
    pub static_store_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafetySection {
    pub impact_cap_pct: f64,
    pub floor_tolerance_pct: f64,
    pub max_sell_tax_pct: f64,
    #[serde(default = "default_fot_policy")]
    pub fot_multihop_v3: String,
}

fn default_fot_policy() -> String {
    "refuse".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchlistSection {
    pub cap: usize,
    pub store_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SettlementSection {
    pub pin_max_age_secs: u64,
}

impl Config {
    /// Parse and semantically validate. Collects every violation instead of
    /// stopping at the first so boot errors are actionable (fail-fast at boot:
    /// no network I/O happens before this returns Ok).
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            EngineError::Config(format!("cannot read {}: {e}", path.as_ref().display()))
        })?;
        Self::from_toml_str(&text)
    }

    pub fn from_toml_str(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)
            .map_err(|e| EngineError::Config(format!("schema error: {e}")))?;
        let violations = cfg.validate();
        if violations.is_empty() {
            Ok(cfg)
        } else {
            Err(EngineError::config(&violations))
        }
    }

    /// All semantic violations, empty when the config is valid.
    pub fn validate(&self) -> Vec<ConfigViolation> {
        let mut v = Vec::new();

        if self.engine.chain_id != 8453 {
            v.push(ConfigViolation::new(
                "engine.chain_id",
                format!("must be 8453 (Base mainnet), got {}", self.engine.chain_id),
            ));
        }
        for (key, addr) in [
            ("engine.settlement_asset", &self.engine.settlement_asset),
            ("engine.wrapped_native", &self.engine.wrapped_native),
        ] {
            if addr.parse::<Address>().is_err() {
                v.push(ConfigViolation::new(key, "not a valid address"));
            } else if !checksum_ok(addr) {
                v.push(ConfigViolation::new(key, "address is not checksummed"));
            }
                  }
        if self.engine.settlement_asset == self.engine.wrapped_native {
            v.push(ConfigViolation::new(
                "engine.settlement_asset",
                "settlement asset must differ from wrapped native",
            ));
        }

        if self.rpc.urls.is_empty() {
            v.push(ConfigViolation::new("rpc.urls", "at least one HTTP RPC url required"));
        }
        for (i, url) in self.rpc.urls.iter().enumerate() {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                v.push(ConfigViolation::new(format!("rpc.urls[{i}]"), "must be http(s)"));
            }
        }
        for (i, url) in self.rpc.ws_urls.iter().enumerate() {
            if !(url.starts_with("ws://") || url.starts_with("wss://")) {
                v.push(ConfigViolation::new(format!("rpc.ws_urls[{i}]"), "must be ws(s)"));
            }
        }
        if self.rpc.bench_interval_secs == 0 {
            v.push(ConfigViolation::new("rpc.bench_interval_secs", "must be > 0"));
        }

        for (key, ttl) in [
            ("cache.static_ttl_secs", self.cache.static_ttl_secs),
            ("cache.reserves_ttl_secs", self.cache.reserves_ttl_secs),
            ("cache.stats_ttl_secs", self.cache.stats_ttl_secs),
            ("cache.negative_ttl_secs", self.cache.negative_ttl_secs),
        ] {
            if ttl == 0 {
                v.push(ConfigViolation::new(key, "must be > 0"));
            }
        }
        if self.cache.static_store_path.as_os_str().is_empty() {
            v.push(ConfigViolation::new("cache.static_store_path", "must not be empty"));
        }

          if !(0.0..=100.0).contains(&self.safety.impact_cap_pct) {
            v.push(ConfigViolation::new("safety.impact_cap_pct", "must be within 0..=100"));
        }
        if !(0.0..=100.0).contains(&self.safety.floor_tolerance_pct) {
            v.push(ConfigViolation::new("safety.floor_tolerance_pct", "must be within 0..=100"));
        }
        if !(0.0..=100.0).contains(&self.safety.max_sell_tax_pct) {
            v.push(ConfigViolation::new("safety.max_sell_tax_pct", "must be within 0..=100"));
        }
        if self.safety.fot_multihop_v3 != "refuse" {
            v.push(ConfigViolation::new(
                "safety.fot_multihop_v3",
                "only \"refuse\" is supported (S1)",
            ));
        }

        if self.watchlist.cap == 0 {
            v.push(ConfigViolation::new("watchlist.cap", "must be > 0"));
        }
        if self.watchlist.store_path.as_os_str().is_empty() {
            v.push(ConfigViolation::new("watchlist.store_path", "must not be empty"));
        }

        if self.settlement.pin_max_age_secs == 0 {
            v.push(ConfigViolation::new("settlement.pin_max_age_secs", "must be > 0"));
        }

        v
    }

    pub fn effective_mode(&self) -> Mode {
        self.engine.mode
    }

    pub fn settlement_asset(&self) -> Address {
        self.engine.settlement_asset.parse().expect("validated")
    }

    pub fn wrapped_native(&self) -> Address {
        self.engine.wrapped_native.parse().expect("validated")
    }
}
