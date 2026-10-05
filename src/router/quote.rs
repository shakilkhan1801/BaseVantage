use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};

use crate::error::{EngineError, Result};
use crate::router::route::Route;

const E18: U256 = U256::from_limbs([0x0de0_b6b3_a764_0000, 0, 0, 0]);

/// Inputs for gross→net accounting, supplied by the harness from the safety
/// layer and the chain.
#[derive(Debug, Clone)]
pub struct NetInputs {
    /// Sell tax of the token being sold, bps.
    pub sell_tax_bps: u32,
    pub gas_units: U256,
    pub gas_price_wei: U256,
    /// Marginal price of wrapped-native in the settlement asset, 1e18-scaled
    /// (settle raw units per wrapped-native raw unit, times 1e18).
    pub wrapped_native_price_1e18: U256,
}

/// One quoted route. Every comparable amount is normalized into the
/// settlement asset before comparison; raw amounts of different assets are
/// never compared.
#[derive(Debug, Clone)]
pub struct Quote {
    pub route: Route,
    pub gross_out: U256,
    /// Asset the route actually outputs (may differ from settlement).
    pub quote_asset: Address,
    pub settlement_asset: Address,
    /// gross_out in settlement-asset raw units.
    pub normalized_out: U256,
    pub tax: U256,
    pub gas_in_settle: U256,
    /// normalized_out − tax − gas, floored at zero.
    pub net_out: U256,
    pub impact_pct: f64,
    /// spot out (zero-size) in quote_asset raw units.
    pub spot_out: U256,
    pub gas_estimate: U256,
    pub pinned_at: Instant,
    /// source+age labels of every cached input this quote consumed.
    pub labels: Vec<String>,
}

impl Quote {
    /// Pin this quote at confirm with the floor-derived min-out.
    pub fn pin(self, min_out: U256) -> PinnedQuote {
        PinnedQuote { quote: self, min_out }
    }
}

/// A quote pinned at confirm. At send it must be re-validated: too old, or
/// re-quote net-out below the pinned min-out, and sending is refused.
#[derive(Debug, Clone)]
pub struct PinnedQuote {
    pub quote: Quote,
    pub min_out: U256,
}

impl PinnedQuote {
    pub fn age(&self) -> Duration {
        self.quote.pinned_at.elapsed()
    }

    /// Re-validate at send against a freshly computed quote for the same
    /// route. Returns the fresh quote when the pin still holds.
    pub fn revalidate(&self, fresh: Quote, max_age: Duration) -> Result<Quote> {
        if self.age() > max_age {
            return Err(EngineError::PinExpired(format!(
                "pin age {:.1?} exceeds max {:.1?}",
                self.age(),
                max_age
            )));
        }
        if fresh.net_out < self.min_out {
            return Err(EngineError::PinExpired(format!(
                "re-quote net {} fell below min-out {}",
                fresh.net_out, self.min_out
            )));
        }
        Ok(fresh)
    }
}

/// gross→net accounting. Sell tax is taken from proceeds; gas is priced in
/// the settlement asset through the wrapped-native marginal price.
pub fn net_out(
    gross: U256,
    quote_asset: Address,
    settlement_asset: Address,
    wrapped_native: Address,
    wrapped_native_price_1e18: U256,
    net: &NetInputs,
) -> Result<(U256, U256, U256, U256)> {
    let normalized = normalize(gross, quote_asset, settlement_asset, wrapped_native, wrapped_native_price_1e18)?;
    let tax = mul_frac(normalized, net.sell_tax_bps, 10_000)?;
    let gas_cost = net.gas_units * net.gas_price_wei;
    let gas_in_settle =
        crate::venues::mul_div_floor(gas_cost, wrapped_native_price_1e18, E18)?;
    let deductions = tax + gas_in_settle;
    let net = normalized.saturating_sub(deductions);
    Ok((normalized, tax, gas_in_settle, net))
}

/// Settlement-asset normalization: identity for the settlement asset,
/// wrapped-native priced through the given marginal rate. Anything else
/// refuses — it must never be compared raw.
pub fn normalize(
    amount: U256,
    asset: Address,
    settlement_asset: Address,
    wrapped_native: Address,
    wrapped_native_price_1e18: U256,
) -> Result<U256> {
    if asset == settlement_asset {
        return Ok(amount);
    }
    if asset == wrapped_native {
        return crate::venues::mul_div_floor(amount, wrapped_native_price_1e18, E18);
    }
    Err(EngineError::Quote(format!(
        "cannot normalize asset {asset} into settlement asset {settlement_asset}"
    )))
}

fn mul_frac(amount: U256, num: u32, den: u32) -> Result<U256> {
    crate::venues::mul_div_floor(amount, U256::from(num), U256::from(den))
}

/// Price impact vs zero-size spot, in percent.
pub fn impact_pct(spot_out: U256, gross_out: U256) -> f64 {
    if spot_out.is_zero() {
        return 0.0;
    }
    let spot = spot_out.to::<u128>() as f64;
    let exec = gross_out.to::<u128>() as f64;
    ((spot - exec) / spot * 100.0).clamp(0.0, 100.0)
}
