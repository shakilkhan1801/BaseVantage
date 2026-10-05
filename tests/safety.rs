//! `safety::*` — floor anchors, worse-fill reverts, impact cap, tax and
//! honeypot blocking, fee-on-transfer refusal on multi-hop v3.

mod common;

use alloy::primitives::U256;

use basevantage::safety::floor::{AnchorKind, FloorAnchor, FloorModule};
use basevantage::safety::{AssessmentSource, SafetyPolicy, TokenAssessment, Verdict};

use common::*;

const E18: u64 = 1_000_000_000_000_000_000;

fn tokens(n: u64) -> U256 {
    U256::from(n) * U256::from(E18)
}

/// Price in thousandths of a settlement unit per token (2000 = 2.0).
fn anchor(kind: AnchorKind, per_mille: u64) -> FloorAnchor {
    FloorAnchor { kind, price_1e18: U256::from(per_mille) * U256::from(E18) / U256::from(1000) }
}

fn assessment(sell_tax_bps: u32, honeypot: bool, fot: bool) -> TokenAssessment {
    TokenAssessment {
        token: TOKEN,
        buy_tax_bps: 0,
        sell_tax_bps,
        honeypot,
        fee_on_transfer: fot,
        probe_block: Some(19_224_331),
        source: AssessmentSource::Probe,
    }
}

#[test]
fn floor_reference_anchor_worse_fill_reverts() {
    let floor = FloorModule::new(0.5);
    // REFERENCE 2.0 binds over SWAP 1.8; tolerance 0.5% => min-out 1990.
    let result = floor
        .min_out(
            tokens(1000),
            &[anchor(AnchorKind::Reference, 2000), anchor(AnchorKind::Swap, 1800)],
        )
        .unwrap();
    let reference_floor = tokens(1990);
    assert_eq!(result.min_out, reference_floor);
    assert!(result.min_out >= tokens(1990));

    // A worse fill must revert.
    assert!(FloorModule::check_fill(result.min_out, tokens(1989)).is_err());
    assert!(FloorModule::check_fill(result.min_out, tokens(1990)).is_ok());
}

#[test]
fn floor_swap_anchor_worse_fill_reverts() {
    let floor = FloorModule::new(0.5);
    // SWAP 2.2 binds over REFERENCE 2.0 => min-out = 2189 (2.2 - 0.5%).
    let result = floor
        .min_out(tokens(1000), &[anchor(AnchorKind::Reference, 2000), anchor(AnchorKind::Swap, 2200)])
        .unwrap();
    assert_eq!(result.min_out, tokens(2189));

    assert!(FloorModule::check_fill(result.min_out, tokens(2100)).is_err());
    assert!(FloorModule::check_fill(result.min_out, tokens(2189)).is_ok());
}

#[test]
fn target_order_floor_never_below_target() {
    let amount = tokens(1000);
    let target_per_token = tokens(25) / U256::from(10); // 2.5
    let target_floor = FloorModule::new(0.0)
        .min_out(amount, &[anchor(AnchorKind::Target, 2500)])
        .unwrap()
        .min_out;
    // min-out is exactly target × amount.
    assert_eq!(target_floor, amount * target_per_token / tokens(1));

    for tolerance in [0.0, 0.5, 5.0, 50.0] {
        let floor = FloorModule::new(tolerance);
        let result = floor
            .min_out(
                amount,
                &[
                    anchor(AnchorKind::Reference, 2000),
                    anchor(AnchorKind::Swap, 1800),
                    anchor(AnchorKind::Target, 2500),
                ],
            )
            .unwrap();
        // The invariant: min-out is never below target × amount, whatever
        // the tolerance does to the other anchors.
        assert!(
            result.min_out >= target_floor,
            "tolerance {tolerance}%: min-out {} < target floor {target_floor}",
            result.min_out
        );
        assert_eq!(result.min_out, target_floor);
        // Target is never reduced by tolerance.
        let tq = result.quotes.iter().find(|q| q.kind == AnchorKind::Target).unwrap();
        assert!(!tq.reduced_by_tolerance);
    }

    // When the target is the weaker anchor, max() still governs.
    let result = FloorModule::new(0.5)
        .min_out(
            amount,
            &[
                anchor(AnchorKind::Reference, 3000),
                anchor(AnchorKind::Target, 1000),
            ],
        )
        .unwrap();
    assert!(result.min_out >= amount); // ≥ target × amount
    assert_eq!(result.min_out, tokens(2985)); // reference 3000 − 0.5%
}

#[test]
fn impact_cap_refuses_pre_send() {
    let policy = SafetyPolicy {
        max_sell_tax_pct: 10.0,
        impact_cap_pct: 1.5,
        floor_tolerance_pct: 0.5,
        refuse_fot_multihop_v3: true,
    };
    let route = single_hop_route(v2_key(POOL_A, TOKEN, USDC), TOKEN, USDC, tokens(1000));
    let over = quote_fixture(route.clone(), tokens(970), USDC, 2.4);
    let a = assessment(0, false, false);

    let verdict = policy.pre_send_gate(&over, &a, U256::ZERO);
    assert!(!verdict.is_allow(), "over-cap impact must refuse pre-send");
    assert!(matches!(verdict, Verdict::Refuse(_)));
    assert!(verdict.label().contains("impact"));

    let ok = quote_fixture(route, tokens(999), USDC, 0.31);
    assert!(policy.pre_send_gate(&ok, &a, U256::ZERO).is_allow());
}

#[test]
fn tax_token_blocked() {
    let policy = SafetyPolicy {
        max_sell_tax_pct: 10.0,
        impact_cap_pct: 1.5,
        floor_tolerance_pct: 0.5,
        refuse_fot_multihop_v3: true,
    };
    let taxed = assessment(1500, false, true); // 15% sell tax
    let verdict = policy.assess_gate(&taxed);
    assert_eq!(
        verdict,
        Verdict::Block("sell tax 15.00% > max 10.00%".to_string()),
        "tax above the cap must block before any route is offered"
    );

    let clean = assessment(500, false, false); // 5% ≤ 10%
    assert!(policy.assess_gate(&clean).is_allow());
}

#[test]
fn honeypot_blocked() {
    let policy = SafetyPolicy {
        max_sell_tax_pct: 10.0,
        impact_cap_pct: 1.5,
        floor_tolerance_pct: 0.5,
        refuse_fot_multihop_v3: true,
    };
    let honeypot = assessment(0, true, false);
    let verdict = policy.assess_gate(&honeypot);
    assert!(matches!(verdict, Verdict::Block(_)));
    assert!(verdict.label().contains("honeypot"));
}

#[test]
fn fee_on_transfer_rejected_multihop_v3() {
    let policy = SafetyPolicy {
        max_sell_tax_pct: 10.0,
        impact_cap_pct: 1.5,
        floor_tolerance_pct: 0.5,
        refuse_fot_multihop_v3: true,
    };
    let fot = assessment(0, false, true);
    let multi_v3 = two_hop_route(
        v3_key(POOL_A, TOKEN, WETH, 3000),
        v3_key(POOL_B, WETH, USDC, 500),
        TOKEN,
        WETH,
        USDC,
        tokens(1000),
    );
    let quote = quote_fixture(multi_v3, tokens(3_000), USDC, 0.2);
    let verdict = policy.pre_send_gate(&quote, &fot, U256::ZERO);
    assert!(!verdict.is_allow());
    assert!(verdict.label().contains("fee-on-transfer"));

    // Single-hop v3 with the same token is fine: exact-in accounting holds.
    let single_v3 = single_hop_route(v3_key(POOL_A, TOKEN, USDC, 3000), TOKEN, USDC, tokens(1000));
    let quote = quote_fixture(single_v3, tokens(3_000), USDC, 0.2);
    assert!(policy.pre_send_gate(&quote, &fot, U256::ZERO).is_allow());
}
