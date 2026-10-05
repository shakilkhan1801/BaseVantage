use alloy::primitives::{Address, U256};

pub mod assess;
pub mod floor;
pub mod impact;

pub use assess::{AssessmentSource, ManualAssessor, ProbeAssessor, TaxOracle, TokenAssessment};
pub use floor::{AnchorKind, FloorAnchor, FloorModule, FloorQuote};

use crate::router::Quote;

/// Safety verdict consumed at the two gates: assess (before routes are
/// offered) and pre-send (before anything would go out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Refuse(String),
    Block(String),
}

impl Verdict {
    pub fn is_allow(&self) -> bool {
        matches!(self, Verdict::Allow)
    }

    pub fn label(&self) -> String {
        match self {
            Verdict::Allow => "allow".to_string(),
            Verdict::Refuse(r) => format!("refused: {r}"),
            Verdict::Block(r) => format!("blocked: {r}"),
        }
    }

    fn worse(self, other: Verdict) -> Verdict {
        use Verdict::*;
        match (&self, &other) {
            (Block(_), _) => self,
            (_, Block(_)) => other,
            (Refuse(_), _) => self,
            (_, Refuse(_)) => other,
            _ => Verdict::Allow,
        }
    }
}

/// Policy configuration (mirrors `[safety]` in the config schema).
#[derive(Debug, Clone)]
pub struct SafetyPolicy {
    pub max_sell_tax_pct: f64,
    pub impact_cap_pct: f64,
    pub floor_tolerance_pct: f64,
    pub refuse_fot_multihop_v3: bool,
}

impl SafetyPolicy {
    /// Gate 1: tax and honeypot verdicts for a token.
    pub fn assess_gate(&self, assessment: &TokenAssessment) -> Verdict {
        if assessment.honeypot {
            return Verdict::Block("honeypot (sell probe failed or zero output)".to_string());
        }
        let sell_tax_pct = f64::from(assessment.sell_tax_bps) / 100.0;
        if sell_tax_pct > self.max_sell_tax_pct {
            return Verdict::Block(format!(
                "sell tax {sell_tax_pct:.2}% > max {:.2}%",
                self.max_sell_tax_pct
            ));
        }
        Verdict::Allow
    }

    /// Gate 2: pre-send checks — impact cap, fee-on-transfer on multi-hop v3,
    /// and the floor (min-out) invariant.
    pub fn pre_send_gate(
        &self,
        quote: &Quote,
        assessment: &TokenAssessment,
        min_out: U256,
    ) -> Verdict {
        let impact = impact::ImpactCap::new(self.impact_cap_pct).check(quote.impact_pct);
        let fot = if self.refuse_fot_multihop_v3
            && assessment.fee_on_transfer
            && quote.route.has_multihop_v3()
        {
            Verdict::Refuse("fee-on-transfer token on multi-hop v3".to_string())
        } else {
            Verdict::Allow
        };
        let floor = if quote.gross_out < min_out && quote.quote_asset == quote.settlement_asset {
            Verdict::Refuse(format!(
                "quoted out {} below floor min-out {}",
                quote.gross_out, min_out
            ))
        } else {
            Verdict::Allow
        };
        impact.worse(fot).worse(floor)
    }
}

/// Convenience: assessment for a token with no known flags (tests/fixtures).
pub fn benign_assessment(token: Address) -> TokenAssessment {
    TokenAssessment {
        token,
        buy_tax_bps: 0,
        sell_tax_bps: 0,
        honeypot: false,
        fee_on_transfer: false,
        probe_block: None,
        source: AssessmentSource::Manual,
    }
}
