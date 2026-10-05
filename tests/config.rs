//! `config::*` — schema validation at boot (fail-fast) and effective mode.

use basevantage::config::{Config, Mode};

const VALID: &str = r#"
[engine]
mode = "observe"
chain_id = 8453
settlement_asset = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
wrapped_native = "0x4200000000000000000000000000000000000006"

[rpc]
urls = ["https://mainnet.base.org"]
ws_urls = ["wss://base-rpc.publicnode.com"]
bench_interval_secs = 60

[cache]
static_ttl_secs = 86400
reserves_ttl_secs = 30
stats_ttl_secs = 60
negative_ttl_secs = 120
static_store_path = "cache/static-pools.json"

[safety]
impact_cap_pct = 1.5
floor_tolerance_pct = 0.5
max_sell_tax_pct = 10.0
fot_multihop_v3 = "refuse"

[watchlist]
cap = 50
store_path = "watchlist.json"

[settlement]
pin_max_age_secs = 30
"#;

#[test]
fn valid_schema_loads_at_boot() {
    let cfg = Config::from_toml_str(VALID).expect("valid config must load");
    assert_eq!(cfg.engine.chain_id, 8453);
    assert_eq!(cfg.watchlist.cap, 50);
    assert_eq!(cfg.cache.static_ttl_secs, 86400);
    assert_eq!(cfg.safety.impact_cap_pct, 1.5);
    // Loading performs semantic validation before any network I/O.
    assert!(cfg.validate().is_empty());
}

#[test]
fn invalid_schema_fails_fast() {
    let bad = VALID
        .replace("mode = \"observe\"", "mode = \"execute\"")
        .replace("chain_id = 8453", "chain_id = 1")
        .replace(
            "settlement_asset = \"0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913\"",
            "settlement_asset = \"0xdeadbeef\"",
        )
        .replace("static_ttl_secs = 86400", "static_ttl_secs = 0")
        .replace("impact_cap_pct = 1.5", "impact_cap_pct = 150.0")
        .replace("cap = 50", "cap = 0")
        .replace("pin_max_age_secs = 30", "pin_max_age_secs = 0");

    let err = Config::from_toml_str(&bad).unwrap_err().to_string();
    // Every violation is reported at once — fail-fast with actionable output.
    for key in [
        "engine.chain_id",
        "engine.settlement_asset",
        "cache.static_ttl_secs",
        "safety.impact_cap_pct",
        "watchlist.cap",
        "settlement.pin_max_age_secs",
    ] {
        assert!(err.contains(key), "missing violation for {key}: {err}");
    }
}

#[test]
fn effective_mode_observed() {
    let observe = Config::from_toml_str(VALID).unwrap();
    assert_eq!(observe.effective_mode(), Mode::Observe);
    let banner = observe.effective_mode().banner();
    assert!(banner.contains("observe"), "banner must state the effective mode: {banner}");

    let execute = Config::from_toml_str(&VALID.replace("mode = \"observe\"", "mode = \"execute\""))
        .unwrap();
    assert_eq!(execute.effective_mode(), Mode::Execute);
    assert!(execute.effective_mode().banner().contains("execute"));
}
