//! Total-cost estimation and ranking for ONE canonical request across venues.
//!
//! Wording contract: results are "best estimated cost among covered and eligible
//! destinations" — never "best execution".

use crate::book::{WalkError, WalkResult, walk};
use crate::comparability::{ComparabilityReport, ReasonCode, is_route_eligible};
use crate::domain::{BookSnapshot, Instrument, InstrumentKey, Provenance, QuoteFirmness, Side};
use crate::fees::{FeeAmount, FeeInputs, FeeSchedule};
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CostLine {
    Known {
        amount: Decimal,
    },
    /// Verified not to be charged to the user for this action.
    NotApplicable {
        reason: String,
    },
    Unknown {
        reason: String,
    },
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Ineligibility {
    Stale,
    ClockSkew,
    PartialCoverage,
    UnknownCosts,
    NotComparable,
    DifferencesNotAccepted,
    BelowMinimumSize,
    InvalidBook,
    CurrencyMismatch,
    PlatformUnhealthy,
    /// Liquidity only retail/interactive takers can match (e.g. Paradex RPI).
    RetailOnlyLiquidity,
    /// Indicative RFQ level (no firm price, no published size), e.g. Rysk.
    IndicativeRfqOnly,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy)]
pub struct FreshnessPolicy {
    /// Max time since our reception.
    pub max_age: Duration,
    /// Tolerated forward skew of the source clock vs ours.
    pub max_forward_skew: Duration,
}

impl Default for FreshnessPolicy {
    fn default() -> Self {
        Self {
            max_age: Duration::seconds(10),
            max_forward_skew: Duration::seconds(2),
        }
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteEstimate {
    pub instrument: InstrumentKey,
    pub side: Side,
    pub provenance: Provenance,
    pub firmness: QuoteFirmness,
    pub pricing_method: String,
    pub source_ts: Option<DateTime<Utc>>,
    pub received_at: DateTime<Utc>,
    pub age_ms: i64,
    /// After this instant the estimate must be refreshed before any use.
    pub valid_until: DateTime<Utc>,
    pub requested_underlying: Decimal,
    pub covered_underlying: Decimal,
    pub covered_native: Decimal,
    pub premium_currency: String,
    pub premium: Decimal,
    pub avg_price_per_underlying: Option<Decimal>,
    /// Walk impact vs top of book, premium currency (already inside `premium`; NOT added again).
    pub book_impact: Option<Decimal>,
    pub levels_consumed: usize,
    pub fee: FeeAmount,
    pub gas: CostLine,
    pub index_price: Option<Decimal>,
    /// premium ± known fees; `None` when any cost line is unknown.
    pub total_estimated: Option<Decimal>,
    /// Same, per covered underlying unit.
    pub total_per_underlying: Option<Decimal>,
    pub unknown_costs: Vec<String>,
    /// Buy: premium + fees that must be available (margin for sells not modelled in V1).
    pub required_funds: Option<Decimal>,
    pub comparability: Option<ComparabilityReport>,
    pub ineligible: Vec<Ineligibility>,
    pub assumptions: Vec<String>,
    pub confidence: Confidence,
    pub fee_source: String,
}

impl QuoteEstimate {
    pub fn eligible(&self) -> bool {
        self.ineligible.is_empty()
    }
}

pub struct EstimateInput<'a> {
    pub instrument: &'a Instrument,
    pub book: &'a BookSnapshot,
    pub side: Side,
    pub qty_underlying: Decimal,
    pub fees: &'a FeeSchedule,
    pub gas: CostLine,
    pub now: DateTime<Utc>,
    pub freshness: FreshnessPolicy,
    /// Comparability vs the reference contract (None for the reference itself).
    pub comparability: Option<ComparabilityReport>,
    pub accepted_differences: &'a [ReasonCode],
    pub platform_healthy: bool,
}

pub fn estimate(inp: EstimateInput<'_>) -> QuoteEstimate {
    let i = inp.instrument;
    let mut ineligible = Vec::new();
    let mut assumptions = vec![
        "Indicative: computed from a public orderbook snapshot; no fill is guaranteed.".to_string(),
        "Taker fee assumed (the order crosses the book).".to_string(),
        "Visible depth only: hidden liquidity, queue priority, cancels, latency and market reaction are not modelled.".to_string(),
    ];

    let age = inp.now - inp.book.received_at;
    if age > inp.freshness.max_age {
        ineligible.push(Ineligibility::Stale);
    }
    if let Some(src) = inp.book.source_ts
        && src - inp.book.received_at > inp.freshness.max_forward_skew
    {
        ineligible.push(Ineligibility::ClockSkew);
    }
    if !inp.platform_healthy {
        ineligible.push(Ineligibility::PlatformUnhealthy);
    }
    if inp.book.access == crate::domain::BookAccess::IndicativeRfq {
        ineligible.push(Ineligibility::IndicativeRfqOnly);
        assumptions.push(
            "Indicative level, not firm for this exact contract (e.g. Rysk RFQ without size, Hegic strike set at purchase): shown for price discovery, never ranked or routed.".to_string(),
        );
    }
    if inp.book.access == crate::domain::BookAccess::RetailOnly {
        ineligible.push(Ineligibility::RetailOnlyLiquidity);
        assumptions.push(
            "Retail-only liquidity (e.g. Paradex RPI): matched only by Retail (interactive) orders, which this router does not send. Shown for price discovery, never ranked or routed.".to_string(),
        );
    }
    if let Some(rep) = &inp.comparability
        && !is_route_eligible(rep, inp.accepted_differences)
    {
        ineligible.push(match rep.verdict {
            crate::comparability::Verdict::EconomicallySimilarWithDifferences => {
                Ineligibility::DifferencesNotAccepted
            }
            _ => Ineligibility::NotComparable,
        });
    }

    let walked: Result<WalkResult, WalkError> = walk(i, inp.book, inp.side, inp.qty_underlying);
    let w = match walked {
        Ok(w) => w,
        Err(e) => {
            ineligible.push(match e {
                WalkError::BelowMinimum { .. } => Ineligibility::BelowMinimumSize,
                _ => Ineligibility::InvalidBook,
            });
            assumptions.push(format!("Book walk failed: {e}"));
            WalkResult {
                requested_underlying: inp.qty_underlying,
                covered_underlying: Decimal::ZERO,
                covered_native: Decimal::ZERO,
                premium: Decimal::ZERO,
                avg_price_per_underlying: None,
                top_price_per_underlying: None,
                levels_consumed: 0,
                fully_covered: false,
            }
        }
    };
    if !w.fully_covered && !ineligible.contains(&Ineligibility::InvalidBook) {
        ineligible.push(Ineligibility::PartialCoverage);
    }
    if inp.fees.currency != i.premium_currency {
        ineligible.push(Ineligibility::CurrencyMismatch);
    }

    let fee = inp.fees.taker.compute(&FeeInputs {
        qty_native: w.covered_native,
        qty_underlying: w.covered_underlying,
        premium: w.premium,
        index_price: inp.book.index_price,
        mark_price: inp.book.mark_price,
    });

    let mut unknown_costs = Vec::new();
    if let FeeAmount::Unknown { reason } = &fee {
        unknown_costs.push(format!("trading fee: {reason}"));
    }
    if let CostLine::Unknown { reason } = &inp.gas {
        unknown_costs.push(format!("gas: {reason}"));
    }
    if !unknown_costs.is_empty() {
        ineligible.push(Ineligibility::UnknownCosts);
    }

    let known_fee = match &fee {
        FeeAmount::Known { amount } => Some(*amount),
        FeeAmount::Unknown { .. } => None,
    };
    let known_gas = match &inp.gas {
        CostLine::Known { amount } => Some(*amount),
        CostLine::NotApplicable { .. } => Some(Decimal::ZERO),
        CostLine::Unknown { .. } => None,
    };
    let total = match (known_fee, known_gas) {
        (Some(f), Some(g)) if w.covered_native > Decimal::ZERO => Some(match inp.side {
            // Buyer pays premium + costs; seller receives premium - costs.
            Side::Buy => w.premium + f + g,
            Side::Sell => w.premium - f - g,
        }),
        _ => None,
    };
    let book_impact = match (w.avg_price_per_underlying, w.top_price_per_underlying) {
        (Some(avg), Some(top)) => Some(match inp.side {
            Side::Buy => (avg - top) * w.covered_underlying,
            Side::Sell => (top - avg) * w.covered_underlying,
        }),
        _ => None,
    };

    ineligible.sort();
    ineligible.dedup();
    let confidence = if ineligible.is_empty() {
        if age <= inp.freshness.max_age / 2 {
            Confidence::Medium
        } else {
            Confidence::Low
        }
    } else {
        Confidence::Low
    };
    // "High" is reserved for firm platform quotes; a book snapshot is at most Medium.

    QuoteEstimate {
        instrument: i.key.clone(),
        side: inp.side,
        provenance: inp.book.provenance,
        firmness: QuoteFirmness::Indicative,
        pricing_method: "visible_book_walk".into(),
        source_ts: inp.book.source_ts,
        received_at: inp.book.received_at,
        age_ms: age.num_milliseconds(),
        valid_until: inp.book.received_at + inp.freshness.max_age,
        requested_underlying: w.requested_underlying,
        covered_underlying: w.covered_underlying,
        covered_native: w.covered_native,
        premium_currency: i.premium_currency.clone(),
        premium: w.premium,
        avg_price_per_underlying: w.avg_price_per_underlying,
        book_impact,
        levels_consumed: w.levels_consumed,
        fee,
        gas: inp.gas,
        index_price: inp.book.index_price,
        total_estimated: total,
        total_per_underlying: total
            .and_then(|t| (w.covered_underlying > Decimal::ZERO).then(|| t / w.covered_underlying)),
        unknown_costs,
        required_funds: match inp.side {
            Side::Buy => total,
            Side::Sell => None,
        },
        comparability: inp.comparability,
        ineligible,
        assumptions,
        confidence,
        fee_source: format!("{} (read {})", inp.fees.source_url, inp.fees.verified_on),
    }
}

/// Rank: eligible estimates first, best total first (lowest for buys, highest for sells).
/// Ineligible ones keep their reasons and are never ranked above an eligible one.
pub fn rank(mut quotes: Vec<QuoteEstimate>, side: Side) -> Vec<QuoteEstimate> {
    quotes.sort_by(|a, b| {
        b.eligible()
            .cmp(&a.eligible())
            .then_with(|| match (a.total_estimated, b.total_estimated) {
                (Some(x), Some(y)) => match side {
                    Side::Buy => x.cmp(&y),
                    Side::Sell => y.cmp(&x),
                },
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
    });
    quotes
}

/// "Estimated saving under these assumptions" vs a baseline venue (where the user already
/// has funds). Positive = the best eligible destination is cheaper. Never "realised".
pub fn estimated_saving_vs_baseline(
    ranked: &[QuoteEstimate],
    baseline: &InstrumentKey,
    side: Side,
) -> Option<Decimal> {
    let best = ranked.iter().find(|q| q.eligible())?;
    let base = ranked
        .iter()
        .find(|q| &q.instrument == baseline && q.eligible())?;
    let (b, x) = (best.total_estimated?, base.total_estimated?);
    Some(match side {
        Side::Buy => x - b,
        Side::Sell => b - x,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparability::tests::inst;
    use crate::comparability::{Verdict, compare};
    use crate::domain::*;
    use crate::fees::FeeFormula;
    use rust_decimal_macros::dec;

    fn sched(f: FeeFormula) -> FeeSchedule {
        FeeSchedule {
            taker: f.clone(),
            maker: f,
            currency: "USDC".into(),
            source_url: "fixture".into(),
            verified_on: "2026-10-05".into(),
            notes: String::new(),
        }
    }

    fn snap(i: &Instrument, asks: &[(Decimal, Decimal)], at: DateTime<Utc>) -> BookSnapshot {
        BookSnapshot {
            access: crate::domain::BookAccess::Api,
            instrument: i.key.clone(),
            asks: asks
                .iter()
                .map(|&(price, size)| Level { price, size })
                .collect(),
            bids: vec![],
            source_ts: Some(at),
            received_at: at,
            index_price: Some(dec!(60000)),
            mark_price: Some(dec!(100)),
            provenance: Provenance::Fixture,
        }
    }

    fn est<'a>(
        i: &'a Instrument,
        b: &'a BookSnapshot,
        f: &'a FeeSchedule,
        now: DateTime<Utc>,
        qty: Decimal,
    ) -> QuoteEstimate {
        estimate(EstimateInput {
            instrument: i,
            book: b,
            side: Side::Buy,
            qty_underlying: qty,
            fees: f,
            gas: CostLine::NotApplicable {
                reason: "test".into(),
            },
            now,
            freshness: FreshnessPolicy::default(),
            comparability: None,
            accepted_differences: &[],
            platform_healthy: true,
        })
    }

    #[test]
    fn stale_quote_is_ineligible() {
        let i = inst(PlatformId::Derive, "A");
        let t0 = Utc::now();
        let b = snap(&i, &[(dec!(100), dec!(10))], t0);
        let f = sched(FeeFormula::Fixed { amount: dec!(1) });
        let q = est(&i, &b, &f, t0 + Duration::seconds(11), dec!(1));
        assert!(q.ineligible.contains(&Ineligibility::Stale));
        let q = est(&i, &b, &f, t0 + Duration::seconds(1), dec!(1));
        assert!(q.eligible());
        assert_eq!(q.total_estimated, Some(dec!(101)));
    }

    #[test]
    fn unknown_fee_gives_no_total_and_is_listed() {
        let i = inst(PlatformId::Derive, "A");
        let t0 = Utc::now();
        let b = snap(&i, &[(dec!(100), dec!(10))], t0);
        let f = sched(FeeFormula::Unknown {
            reason: "not documented".into(),
        });
        let q = est(&i, &b, &f, t0, dec!(1));
        assert_eq!(q.total_estimated, None);
        assert!(q.ineligible.contains(&Ineligibility::UnknownCosts));
        assert_eq!(q.unknown_costs.len(), 1);
    }

    #[test]
    fn source_clock_ahead_is_flagged() {
        let i = inst(PlatformId::Derive, "A");
        let t0 = Utc::now();
        let mut b = snap(&i, &[(dec!(100), dec!(10))], t0);
        b.source_ts = Some(t0 + Duration::seconds(30));
        let f = sched(FeeFormula::Fixed { amount: dec!(0) });
        assert!(
            est(&i, &b, &f, t0, dec!(1))
                .ineligible
                .contains(&Ineligibility::ClockSkew)
        );
    }

    #[test]
    fn ranking_is_quantity_aware() {
        // Venue A: cheap top but thin; venue B: slightly worse but deep.
        let t0 = Utc::now();
        let f = sched(FeeFormula::Fixed { amount: dec!(0) });
        let a = inst(PlatformId::Derive, "A");
        let b = inst(PlatformId::Aevo, "B");
        let ba = snap(&a, &[(dec!(100), dec!(0.1)), (dec!(150), dec!(10))], t0);
        let bb = snap(&b, &[(dec!(110), dec!(10))], t0);
        // small size: A wins
        let r = rank(
            vec![
                est(&a, &ba, &f, t0, dec!(0.1)),
                est(&b, &bb, &f, t0, dec!(0.1)),
            ],
            Side::Buy,
        );
        assert_eq!(r[0].instrument.platform, PlatformId::Derive);
        // large size: B wins
        let r = rank(
            vec![est(&a, &ba, &f, t0, dec!(2)), est(&b, &bb, &f, t0, dec!(2))],
            Side::Buy,
        );
        assert_eq!(r[0].instrument.platform, PlatformId::Aevo);
        let saving = estimated_saving_vs_baseline(&r, &a.key, Side::Buy).unwrap();
        // A: 0.1@100 + 1.9@150 = 295 ; B: 2@110 = 220
        assert_eq!(saving, dec!(75));
    }

    #[test]
    fn partial_coverage_never_outranks_full_coverage() {
        let t0 = Utc::now();
        let f = sched(FeeFormula::Fixed { amount: dec!(0) });
        let a = inst(PlatformId::Derive, "A");
        let b = inst(PlatformId::Aevo, "B");
        let ba = snap(&a, &[(dec!(1), dec!(0.5))], t0); // very cheap but thin
        let bb = snap(&b, &[(dec!(100), dec!(10))], t0);
        let r = rank(
            vec![est(&a, &ba, &f, t0, dec!(1)), est(&b, &bb, &f, t0, dec!(1))],
            Side::Buy,
        );
        assert_eq!(r[0].instrument.platform, PlatformId::Aevo);
        assert!(r[1].ineligible.contains(&Ineligibility::PartialCoverage));
    }

    #[test]
    fn similar_contract_needs_accepted_differences() {
        let t0 = Utc::now();
        let f = sched(FeeFormula::Fixed { amount: dec!(0) });
        let a = inst(PlatformId::Derive, "A");
        let mut b = inst(PlatformId::Aevo, "B");
        b.settlement_index = Some("other".into());
        let rep = compare(&a, &b);
        assert_eq!(rep.verdict, Verdict::EconomicallySimilarWithDifferences);
        let bb = snap(&b, &[(dec!(100), dec!(10))], t0);
        let mk = |acc: &'static [ReasonCode]| {
            estimate(EstimateInput {
                instrument: &b,
                book: &bb,
                side: Side::Buy,
                qty_underlying: dec!(1),
                fees: &f,
                gas: CostLine::NotApplicable { reason: "t".into() },
                now: t0,
                freshness: FreshnessPolicy::default(),
                comparability: Some(rep.clone()),
                accepted_differences: acc,
                platform_healthy: true,
            })
        };
        assert!(
            mk(&[])
                .ineligible
                .contains(&Ineligibility::DifferencesNotAccepted)
        );
        assert!(mk(&[ReasonCode::SettlementIndexDiffers]).eligible());
    }

    #[test]
    fn fee_currency_differs_from_premium_currency_is_ineligible() {
        let i = inst(PlatformId::Derive, "A");
        let t0 = Utc::now();
        let b = snap(&i, &[(dec!(100), dec!(10))], t0);
        let mut f = sched(FeeFormula::Fixed { amount: dec!(1) });
        f.currency = "USDT".into();
        assert!(
            est(&i, &b, &f, t0, dec!(1))
                .ineligible
                .contains(&Ineligibility::CurrencyMismatch)
        );
    }

    #[test]
    fn sell_ranks_highest_net_proceeds_first() {
        let t0 = Utc::now();
        let f = sched(FeeFormula::Fixed { amount: dec!(5) });
        let a = inst(PlatformId::Derive, "A");
        let b = inst(PlatformId::Aevo, "B");
        let mk = |i: &Instrument, bid: Decimal| {
            let mut s = snap(i, &[], t0);
            s.bids = vec![Level {
                price: bid,
                size: dec!(10),
            }];
            estimate(EstimateInput {
                side: Side::Sell,
                qty_underlying: dec!(1),
                ..input(i, &s, &f, t0)
            })
        };
        let r = rank(vec![mk(&a, dec!(90)), mk(&b, dec!(95))], Side::Sell);
        assert_eq!(r[0].instrument.platform, PlatformId::Aevo);
        assert_eq!(r[0].total_estimated, Some(dec!(90))); // 95 - 5 fee
        assert_eq!(
            r[0].required_funds, None,
            "sell margin is not modelled, never zero"
        );
    }

    #[test]
    fn impact_is_reported_but_not_double_counted() {
        let t0 = Utc::now();
        let f = sched(FeeFormula::Fixed { amount: dec!(0) });
        let a = inst(PlatformId::Derive, "A");
        let b = snap(&a, &[(dec!(100), dec!(1)), (dec!(110), dec!(1))], t0);
        let q = estimate(input(&a, &b, &f, t0));
        assert_eq!(q.book_impact, Some(dec!(10)));
        assert_eq!(
            q.total_estimated,
            Some(dec!(210)),
            "impact is inside premium, not added again"
        );
    }

    #[test]
    fn unhealthy_platform_is_ineligible() {
        let t0 = Utc::now();
        let f = sched(FeeFormula::Fixed { amount: dec!(0) });
        let a = inst(PlatformId::Derive, "A");
        let b = snap(&a, &[(dec!(100), dec!(10))], t0);
        let q = estimate(EstimateInput {
            platform_healthy: false,
            ..input(&a, &b, &f, t0)
        });
        assert!(q.ineligible.contains(&Ineligibility::PlatformUnhealthy));
    }

    #[test]
    fn retail_only_liquidity_is_priced_but_never_eligible() {
        let t0 = Utc::now();
        let f = sched(FeeFormula::Fixed { amount: dec!(0) });
        let a = inst(PlatformId::Paradex, "A");
        let mut b = snap(&a, &[(dec!(100), dec!(10))], t0);
        b.access = crate::domain::BookAccess::RetailOnly;
        let q = estimate(input(&a, &b, &f, t0));
        assert_eq!(q.ineligible, vec![Ineligibility::RetailOnlyLiquidity]);
        assert_eq!(q.premium, dec!(200), "prices still computed for display");
        assert!(q.assumptions.iter().any(|s| s.contains("Retail-only")));
    }

    fn input<'a>(
        i: &'a Instrument,
        b: &'a BookSnapshot,
        f: &'a FeeSchedule,
        t0: DateTime<Utc>,
    ) -> EstimateInput<'a> {
        EstimateInput {
            instrument: i,
            book: b,
            side: Side::Buy,
            qty_underlying: dec!(2),
            fees: f,
            gas: CostLine::NotApplicable { reason: "t".into() },
            now: t0,
            freshness: FreshnessPolicy::default(),
            comparability: None,
            accepted_differences: &[],
            platform_healthy: true,
        }
    }

    proptest::proptest! {
        /// Whatever the books and sizes: no ineligible estimate is ranked above an eligible
        /// one, and eligible buys are sorted by ascending total.
        #[test]
        fn ranking_invariants(
            venues in proptest::collection::vec((1u32..500, 1u32..300, proptest::bool::ANY), 1..6),
            q in 1u32..400,
        ) {
            let t0 = Utc::now();
            let f = sched(FeeFormula::Fixed { amount: dec!(1) });
            let insts: Vec<_> = (0..venues.len())
                .map(|k| inst(PlatformId::ALL[k], &format!("I{k}")))
                .collect();
            let books: Vec<_> = venues.iter().zip(&insts)
                .map(|((p, sz, stale), i)| {
                    let at = if *stale { t0 - Duration::seconds(60) } else { t0 };
                    snap(i, &[(Decimal::from(*p), Decimal::new(*sz as i64, 2))], at)
                })
                .collect();
            let qty = Decimal::new(q as i64, 2);
            let quotes: Vec<_> = books.iter().zip(&insts).map(|(b, i)| est(i, b, &f, t0, qty)).collect();
            let r = rank(quotes, Side::Buy);
            let first_inel = r.iter().position(|x| !x.eligible()).unwrap_or(r.len());
            proptest::prop_assert!(r[first_inel..].iter().all(|x| !x.eligible()));
            for w in r[..first_inel].windows(2) {
                proptest::prop_assert!(w[0].total_estimated.unwrap() <= w[1].total_estimated.unwrap());
            }
        }
    }
}
