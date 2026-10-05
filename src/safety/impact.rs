use crate::safety::Verdict;

/// Price-impact cap. Above the cap, pre-send refuses.
#[derive(Debug, Clone, Copy)]
pub struct ImpactCap {
    pub cap_pct: f64,
}

impl ImpactCap {
    pub fn new(cap_pct: f64) -> Self {
        Self { cap_pct }
    }

    pub fn check(&self, impact_pct: f64) -> Verdict {
        if impact_pct > self.cap_pct {
            Verdict::Refuse(format!(
                "impact {impact_pct:.2}% > cap {:.2}%",
                self.cap_pct
            ))
        } else {
            Verdict::Allow
        }
    }
}
