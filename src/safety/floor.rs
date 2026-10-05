use alloy::primitives::U256;

use crate::error::{EngineError, Result};
use crate::venues::mul_div_floor;

/// Which anchor a floor value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    /// Sanity floor from the target token's deepest canonical pool.
    Reference,
    /// Floor implied by the price of the route actually crossed.
    Swap,
    /// The user's target price. Reserved for S2 target orders; supplied
    /// directly in S1 so the invariant is enforced and tested now.
    Target,
}

#[derive(Debug, Clone, Copy)]
pub struct FloorAnchor {
    pub kind: AnchorKind,
    /// Price in settlement-asset raw units per whole target token, 1e18-scaled.
    pub price_1e18: U256,
}

/// One anchor-derived floor value.
#[derive(Debug, Clone, Copy)]
pub struct FloorQuote {
    pub kind: AnchorKind,
    /// min-out contributed by this anchor for `amount` (target-token whole units).
    pub floor_out: U256,
    /// Whether tolerance reduction applied to this anchor (never to TARGET).
    pub reduced_by_tolerance: bool,
}

/// The floor module: min-out = max(applicable anchors). The REFERENCE and
/// SWAP anchors may be reduced by the configured tolerance; the TARGET anchor
/// is absolute — min-out is NEVER below `target × amount`.
pub struct FloorModule {
    pub tolerance_pct: f64,
}

pub struct FloorResult {
    pub min_out: U256,
    pub quotes: Vec<FloorQuote>,
}

impl FloorModule {
    pub fn new(tolerance_pct: f64) -> Self {
        Self { tolerance_pct }
    }

    /// `amount` is the target-token amount being valued (whole units, 1e18
    /// scaled). Each anchor contributes `amount × price`.
    pub fn min_out(&self, amount_1e18: U256, anchors: &[FloorAnchor]) -> Result<FloorResult> {
        if anchors.is_empty() {
            return Err(EngineError::SafetyRefused(
                "floor: no anchors available".to_string(),
            ));
        }
        let e18 = U256::from(10).pow(U256::from(18));
        let mut quotes = Vec::with_capacity(anchors.len());
        let mut min_out = U256::ZERO;
        let mut target_floor = U256::ZERO;

        for anchor in anchors {
            let raw = mul_div_floor(amount_1e18, anchor.price_1e18, e18)?;
            let floor_out = match anchor.kind {
                AnchorKind::Target => {
                    // Absolute: never reduced by tolerance.
                    target_floor = target_floor.max(raw);
                    raw
                }
                AnchorKind::Reference | AnchorKind::Swap => {
                    let reduced = self.apply_tolerance(raw)?;
                    min_out = min_out.max(reduced);
                    quotes.push(FloorQuote {
                        kind: anchor.kind,
                        floor_out: reduced,
                        reduced_by_tolerance: reduced != raw,
                    });
                    continue;
                }
            };
            quotes.push(FloorQuote {
                kind: anchor.kind,
                floor_out,
                reduced_by_tolerance: false,
            });
        }

          let min_out = min_out.max(target_floor);
        Ok(FloorResult { min_out, quotes })
    }

    fn apply_tolerance(&self, raw: U256) -> Result<U256> {
        let pct = (self.tolerance_pct.clamp(0.0, 100.0) * 100.0).round() as u64; // bps
        mul_div_floor(raw, U256::from(10_000 - pct), U256::from(10_000))
    }

    /// A fill worse than min-out must revert. `Ok(())` when the fill clears
    /// the floor, `Err` when the swap would revert.
    pub fn check_fill(min_out: U256, actual_out: U256) -> Result<()> {
        if actual_out < min_out {
            return Err(EngineError::SafetyRefused(format!(
                "worse fill: {actual_out} < min-out {min_out} (swap reverts)"
            )));
        }
        Ok(())
    }
}
