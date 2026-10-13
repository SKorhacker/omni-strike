//! Contract comparability. Underlying + strike + expiry is NOT enough:
//! contractual rules (exercise, settlement asset/mechanism/index/window) are compared too.
//!
//! Rules (documented in docs/comparability.md):
//! - Any *core* term differs (family, underlying, kind, strike, expiry) -> NOT_COMPARABLE.
//! - Any *core* term unknown on either side -> INSUFFICIENT_INFORMATION.
//! - All core terms equal, but a *secondary* term differs or is unknown
//!   -> ECONOMICALLY_SIMILAR_WITH_DIFFERENCES (with one reason code per difference).
//! - Everything known and equal -> STRICTLY_COMPARABLE.
//!
//! A different contract multiplier is NOT a difference: quantities are normalised to
//! underlying units before any comparison.

use crate::domain::{Instrument, ProductFamily};
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    StrictlyComparable,
    EconomicallySimilarWithDifferences,
    InsufficientInformation,
    NotComparable,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReasonCode {
    // Core mismatches -> NOT_COMPARABLE
    FamilyMismatch,
    UnsupportedFamily,
    UnderlyingMismatch,
    OptionKindMismatch,
    StrikeMismatch,
    ExpiryMismatch,
    // Core unknowns -> INSUFFICIENT_INFORMATION
    OptionKindUnknown,
    StrikeUnknown,
    ExpiryUnknown,
    // Secondary differences -> SIMILAR_WITH_DIFFERENCES
    ExerciseStyleDiffers,
    ExerciseStyleUnknown,
    SettlementMechanismDiffers,
    SettlementMechanismUnknown,
    SettlementAssetDiffers,
    SettlementAssetUnknown,
    SettlementIndexDiffers,
    SettlementIndexUnknown,
    SettlementWindowDiffers,
    SettlementWindowUnknown,
    PremiumCurrencyDiffers,
}

impl ReasonCode {
    pub fn explain(self) -> &'static str {
        use ReasonCode::*;
        match self {
            FamilyMismatch => "Different product families are never compared.",
            UnsupportedFamily => {
                "Only dated options and fixed-threshold binaries are compared in this version."
            }
            UnderlyingMismatch => "Different underlying assets.",
            OptionKindMismatch => "One is a call, the other a put.",
            StrikeMismatch => "Different strikes.",
            ExpiryMismatch => "Different expiry instants (UTC).",
            OptionKindUnknown => "Call/put not known for at least one instrument.",
            StrikeUnknown => "Strike not known for at least one instrument.",
            ExpiryUnknown => "Expiry not known for at least one instrument.",
            ExerciseStyleDiffers => "Exercise styles differ (European vs American).",
            ExerciseStyleUnknown => "Exercise style not verified for at least one instrument.",
            SettlementMechanismDiffers => "Cash vs physical settlement.",
            SettlementMechanismUnknown => "Settlement mechanism not verified.",
            SettlementAssetDiffers => "Settled in different assets.",
            SettlementAssetUnknown => "Settlement asset not verified.",
            SettlementIndexDiffers => {
                "Settlement prices come from different index/oracle sources; payoffs can differ at expiry."
            }
            SettlementIndexUnknown => "Settlement index/oracle not verified.",
            SettlementWindowDiffers => "Settlement price is computed over different windows.",
            SettlementWindowUnknown => "Settlement window not verified.",
            PremiumCurrencyDiffers => {
                "Premiums are paid in different currencies; a conversion rate is required."
            }
        }
    }

    fn is_core_mismatch(self) -> bool {
        use ReasonCode::*;
        matches!(
            self,
            FamilyMismatch
                | UnsupportedFamily
                | UnderlyingMismatch
                | OptionKindMismatch
                | StrikeMismatch
                | ExpiryMismatch
        )
    }

    fn is_core_unknown(self) -> bool {
        use ReasonCode::*;
        matches!(self, OptionKindUnknown | StrikeUnknown | ExpiryUnknown)
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparabilityReport {
    pub verdict: Verdict,
    pub reasons: Vec<ReasonCode>,
    pub explanation: String,
}

fn cmp_opt<T: PartialEq>(
    a: &Option<T>,
    b: &Option<T>,
    differs: ReasonCode,
    unknown: ReasonCode,
    out: &mut Vec<ReasonCode>,
) {
    match (a, b) {
        (Some(x), Some(y)) if x == y => {}
        (Some(_), Some(_)) => out.push(differs),
        _ => out.push(unknown),
    }
}

/// Compare two instruments' contractual terms.
pub fn compare(a: &Instrument, b: &Instrument) -> ComparabilityReport {
    let mut r = Vec::new();

    if a.family != b.family {
        r.push(ReasonCode::FamilyMismatch);
    } else if !matches!(a.family, ProductFamily::DatedOption | ProductFamily::Binary) {
        r.push(ReasonCode::UnsupportedFamily);
    }
    if a.underlying != b.underlying {
        r.push(ReasonCode::UnderlyingMismatch);
    }
    cmp_opt(
        &a.option_kind,
        &b.option_kind,
        ReasonCode::OptionKindMismatch,
        ReasonCode::OptionKindUnknown,
        &mut r,
    );
    cmp_opt(
        &a.strike,
        &b.strike,
        ReasonCode::StrikeMismatch,
        ReasonCode::StrikeUnknown,
        &mut r,
    );
    cmp_opt(
        &a.expiry,
        &b.expiry,
        ReasonCode::ExpiryMismatch,
        ReasonCode::ExpiryUnknown,
        &mut r,
    );
    cmp_opt(
        &a.exercise_style,
        &b.exercise_style,
        ReasonCode::ExerciseStyleDiffers,
        ReasonCode::ExerciseStyleUnknown,
        &mut r,
    );
    cmp_opt(
        &a.settlement_mechanism,
        &b.settlement_mechanism,
        ReasonCode::SettlementMechanismDiffers,
        ReasonCode::SettlementMechanismUnknown,
        &mut r,
    );
    cmp_opt(
        &a.settlement_asset,
        &b.settlement_asset,
        ReasonCode::SettlementAssetDiffers,
        ReasonCode::SettlementAssetUnknown,
        &mut r,
    );
    cmp_opt(
        &a.settlement_index,
        &b.settlement_index,
        ReasonCode::SettlementIndexDiffers,
        ReasonCode::SettlementIndexUnknown,
        &mut r,
    );
    cmp_opt(
        &a.settlement_window,
        &b.settlement_window,
        ReasonCode::SettlementWindowDiffers,
        ReasonCode::SettlementWindowUnknown,
        &mut r,
    );
    if a.premium_currency != b.premium_currency {
        r.push(ReasonCode::PremiumCurrencyDiffers);
    }

    r.sort();
    r.dedup();
    let verdict = if r.iter().any(|c| c.is_core_mismatch()) {
        Verdict::NotComparable
    } else if r.iter().any(|c| c.is_core_unknown()) {
        Verdict::InsufficientInformation
    } else if r.is_empty() {
        Verdict::StrictlyComparable
    } else {
        Verdict::EconomicallySimilarWithDifferences
    };
    let explanation = if r.is_empty() {
        "All contractual terms are known and identical.".to_string()
    } else {
        r.iter().map(|c| c.explain()).collect::<Vec<_>>().join(" ")
    };
    ComparabilityReport {
        verdict,
        reasons: r,
        explanation,
    }
}

/// Routing eligibility: strict matches are eligible; similar ones only when EVERY
/// difference was explicitly accepted by the user. Nothing else is ever eligible.
pub fn is_route_eligible(report: &ComparabilityReport, accepted: &[ReasonCode]) -> bool {
    match report.verdict {
        Verdict::StrictlyComparable => true,
        Verdict::EconomicallySimilarWithDifferences => {
            report.reasons.iter().all(|c| accepted.contains(c))
        }
        Verdict::InsufficientInformation | Verdict::NotComparable => false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::*;
    use chrono::{TimeZone, Utc};
    use rust_decimal_macros::dec;

    pub(crate) fn inst(platform: PlatformId, native: &str) -> Instrument {
        Instrument {
            key: InstrumentKey {
                platform,
                network: "test".into(),
                native_id: native.into(),
            },
            family: ProductFamily::DatedOption,
            underlying: "BTC".into(),
            option_kind: Some(OptionKind::Call),
            strike: Some(dec!(100000)),
            expiry: Some(Utc.with_ymd_and_hms(2026, 10, 30, 8, 0, 0).unwrap()),
            exercise_style: Some(ExerciseStyle::European),
            contract_multiplier: dec!(1),
            premium_currency: "USDC".into(),
            collateral_assets: vec!["USDC".into()],
            settlement_asset: Some("USDC".into()),
            settlement_mechanism: Some(SettlementMechanism::Cash),
            settlement_index: Some("IDX".into()),
            settlement_window: Some("30m TWAP".into()),
            tick_size: Some(dec!(0.1)),
            min_amount: Some(dec!(0.01)),
            amount_step: Some(dec!(0.01)),
            is_active: true,
            spec_version: "test-1".into(),
            native: serde_json::Value::Null,
        }
    }

    #[test]
    fn binaries_compare_with_binaries_never_with_vanilla() {
        let mut a = inst(PlatformId::Derive, "A");
        let mut b = inst(PlatformId::Aevo, "B");
        a.family = ProductFamily::Binary;
        b.family = ProductFamily::Binary;
        b.settlement_index = Some("kalshi:CF BRTI".into());
        let r = compare(&a, &b);
        assert!(!r.reasons.contains(&ReasonCode::UnsupportedFamily), "{r:?}");
        assert!(!r.reasons.contains(&ReasonCode::FamilyMismatch));
        let v = inst(PlatformId::Deribit, "V");
        let r = compare(&a, &v);
        assert!(r.reasons.contains(&ReasonCode::FamilyMismatch));
        assert_eq!(r.verdict, Verdict::NotComparable);
    }

    #[test]
    fn identical_terms_are_strict() {
        let a = inst(PlatformId::Derive, "A");
        let b = inst(PlatformId::Aevo, "B");
        let r = compare(&a, &b);
        assert_eq!(r.verdict, Verdict::StrictlyComparable);
        assert!(is_route_eligible(&r, &[]));
    }

    #[test]
    fn multiplier_difference_is_not_a_difference() {
        let a = inst(PlatformId::Derive, "A");
        let mut b = inst(PlatformId::Aevo, "B");
        b.contract_multiplier = dec!(0.1);
        assert_eq!(compare(&a, &b).verdict, Verdict::StrictlyComparable);
    }

    #[test]
    fn strike_mismatch_not_comparable_even_with_unknowns() {
        let a = inst(PlatformId::Derive, "A");
        let mut b = inst(PlatformId::Aevo, "B");
        b.strike = Some(dec!(101000));
        b.expiry = None;
        let r = compare(&a, &b);
        assert_eq!(r.verdict, Verdict::NotComparable);
        assert!(r.reasons.contains(&ReasonCode::StrikeMismatch));
        assert!(!is_route_eligible(&r, &[ReasonCode::StrikeMismatch]));
    }

    #[test]
    fn one_second_expiry_difference_is_a_mismatch() {
        let a = inst(PlatformId::Derive, "A");
        let mut b = inst(PlatformId::Aevo, "B");
        b.expiry = Some(a.expiry.unwrap() + chrono::Duration::seconds(1));
        assert_eq!(compare(&a, &b).verdict, Verdict::NotComparable);
    }

    #[test]
    fn unknown_core_term_is_insufficient() {
        let a = inst(PlatformId::Derive, "A");
        let mut b = inst(PlatformId::Aevo, "B");
        b.strike = None;
        let r = compare(&a, &b);
        assert_eq!(r.verdict, Verdict::InsufficientInformation);
        assert!(!is_route_eligible(&r, &[ReasonCode::StrikeUnknown]));
    }

    #[test]
    fn index_difference_requires_explicit_acceptance() {
        let a = inst(PlatformId::Derive, "A");
        let mut b = inst(PlatformId::Aevo, "B");
        b.settlement_index = Some("OTHER".into());
        let r = compare(&a, &b);
        assert_eq!(r.verdict, Verdict::EconomicallySimilarWithDifferences);
        assert_eq!(r.reasons, vec![ReasonCode::SettlementIndexDiffers]);
        assert!(!is_route_eligible(&r, &[]));
        assert!(is_route_eligible(&r, &[ReasonCode::SettlementIndexDiffers]));
    }

    #[test]
    fn family_mismatch_never_eligible() {
        let a = inst(PlatformId::Derive, "A");
        let mut b = inst(PlatformId::Paradex, "B");
        b.family = ProductFamily::PerpetualOption;
        let r = compare(&a, &b);
        assert_eq!(r.verdict, Verdict::NotComparable);
    }
}
