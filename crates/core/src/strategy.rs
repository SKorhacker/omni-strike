//! Multi-leg strategy planning (spreads, straddles, strangles, condors, flies, custom).
//!
//! Input: for every leg, the eligible fully-covered single-venue estimates (cost for a BUY leg, net
//! proceeds for a SELL leg, fees included). Output: the plan, following the atomicity ladder:
//! 1. **Single venue** that lists every leg — executable together (atomic where the venue supports
//!    combos) — chosen when its net cost is within `single_venue_tolerance` of the best split.
//! 2. Otherwise **split**: each leg on its own best venue (legs sent in parallel; unwind on failure).
//!
//! Sign convention: `net_cost` = sum(buy costs) - sum(sell proceeds). Positive = debit (the user pays),
//! negative = credit. Lower is always better.

use crate::domain::{PlatformId, Side};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegQuote {
    pub platform: PlatformId,
    pub native_id: String,
    /// Buy: total cost incl. fees. Sell: net proceeds after fees. Premium currency (USDC-equivalent).
    pub amount: Decimal,
}

#[derive(Debug, Clone)]
pub struct LegInput {
    pub side: Side,
    /// Eligible, fully covered quotes for this leg (any order).
    pub quotes: Vec<LegQuote>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanKind {
    /// Every leg on one venue (atomic when the venue supports combo orders).
    SingleVenue,
    /// Legs on different venues, sent in parallel, unwound if one fails.
    Split,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedLeg {
    pub leg: usize,
    pub side: Side,
    pub quote: LegQuote,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyPlan {
    pub kind: PlanKind,
    pub legs: Vec<PlannedLeg>,
    /// Positive = debit, negative = credit.
    pub net_cost: Decimal,
    /// Best split's net cost (equal to `net_cost` when the split is chosen).
    pub best_split_net_cost: Decimal,
    /// Best single-venue net cost, if any venue lists every leg.
    pub best_single_venue: Option<(PlatformId, Decimal)>,
    /// What the chosen plan saves vs doing everything on `baseline` (if it lists every leg).
    pub saving_vs_baseline: Option<Decimal>,
    pub reason: String,
}

/// One leg of an expiry payoff (all legs must share the same expiry for `max_loss`).
#[derive(Debug, Clone)]
pub struct PayoffLeg {
    pub option_kind: crate::domain::OptionKind,
    pub strike: Decimal,
    pub side: Side,
    pub qty_underlying: Decimal,
}

fn payoff_at(legs: &[PayoffLeg], s: Decimal) -> Decimal {
    use crate::domain::OptionKind;
    legs.iter()
        .map(|l| {
            let intrinsic = match l.option_kind {
                OptionKind::Call => (s - l.strike).max(Decimal::ZERO),
                OptionKind::Put => (l.strike - s).max(Decimal::ZERO),
            };
            signed(l.side, intrinsic * l.qty_underlying)
        })
        .sum()
}

/// Worst loss at expiry of a same-expiry strategy, given its net cost (positive = debit paid,
/// negative = credit received). `None` = UNBOUNDED (net short calls: the loss grows with the price).
/// The payoff is piecewise linear, so its minimum is at S = 0, at a strike, or at S -> infinity.
pub fn max_loss(legs: &[PayoffLeg], net_cost: Decimal) -> Option<Decimal> {
    use crate::domain::OptionKind;
    let slope_up: Decimal = legs
        .iter()
        .filter(|l| l.option_kind == OptionKind::Call)
        .map(|l| signed(l.side, l.qty_underlying))
        .sum();
    if slope_up < Decimal::ZERO {
        return None;
    }
    let mut points = vec![Decimal::ZERO];
    points.extend(legs.iter().map(|l| l.strike));
    let worst_pnl = points
        .into_iter()
        .map(|s| payoff_at(legs, s) - net_cost)
        .min()?;
    Some((-worst_pnl).max(Decimal::ZERO))
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum PlanError {
    #[error("strategy has no legs")]
    NoLegs,
    #[error("leg {0} has no eligible fully-covered quote on any venue")]
    UncoveredLeg(usize),
}

fn signed(side: Side, amount: Decimal) -> Decimal {
    match side {
        Side::Buy => amount,
        Side::Sell => -amount,
    }
}

/// Best quote for a leg: lowest cost when buying, highest proceeds when selling.
fn best(leg: &LegInput) -> Option<&LegQuote> {
    match leg.side {
        Side::Buy => leg.quotes.iter().min_by_key(|q| q.amount),
        Side::Sell => leg.quotes.iter().max_by_key(|q| q.amount),
    }
}

/// Net cost of executing every leg on `p`, if `p` quotes every leg.
fn single_venue(legs: &[LegInput], p: PlatformId) -> Option<(Decimal, Vec<PlannedLeg>)> {
    let mut total = Decimal::ZERO;
    let mut planned = Vec::with_capacity(legs.len());
    for (i, l) in legs.iter().enumerate() {
        let q = match l.side {
            Side::Buy => l
                .quotes
                .iter()
                .filter(|q| q.platform == p)
                .min_by_key(|q| q.amount),
            Side::Sell => l
                .quotes
                .iter()
                .filter(|q| q.platform == p)
                .max_by_key(|q| q.amount),
        }?;
        total += signed(l.side, q.amount);
        planned.push(PlannedLeg {
            leg: i,
            side: l.side,
            quote: q.clone(),
        });
    }
    Some((total, planned))
}

/// Plan a strategy. `tolerance` = how much more (absolute, premium currency) a single venue may cost
/// than the best split and still be preferred for atomicity.
pub fn plan(
    legs: &[LegInput],
    tolerance: Decimal,
    baseline: Option<PlatformId>,
) -> Result<StrategyPlan, PlanError> {
    if legs.is_empty() {
        return Err(PlanError::NoLegs);
    }
    let mut split = Vec::with_capacity(legs.len());
    let mut split_total = Decimal::ZERO;
    for (i, l) in legs.iter().enumerate() {
        let q = best(l).ok_or(PlanError::UncoveredLeg(i))?;
        split_total += signed(l.side, q.amount);
        split.push(PlannedLeg {
            leg: i,
            side: l.side,
            quote: q.clone(),
        });
    }
    let mut venues: Vec<PlatformId> = legs
        .iter()
        .flat_map(|l| l.quotes.iter().map(|q| q.platform))
        .collect();
    venues.sort();
    venues.dedup();
    let singles: BTreeMap<PlatformId, (Decimal, Vec<PlannedLeg>)> = venues
        .into_iter()
        .filter_map(|p| single_venue(legs, p).map(|s| (p, s)))
        .collect();
    let best_single = singles
        .iter()
        .min_by_key(|(_, (t, _))| *t)
        .map(|(p, (t, l))| (*p, *t, l.clone()));
    let baseline_total = baseline.and_then(|b| singles.get(&b).map(|(t, _)| *t));

    let (kind, planned, net, reason) = match best_single {
        Some((p, t, l)) if t - split_total <= tolerance.max(Decimal::ZERO) => {
            let extra = t - split_total;
            let why = if extra.is_zero() {
                format!(
                    "single venue {p} is as cheap as any split: all legs together (atomic where supported)"
                )
            } else {
                format!(
                    "single venue {p} costs {extra} more than the best split, within the atomicity tolerance {tolerance}"
                )
            };
            (PlanKind::SingleVenue, l, t, why)
        }
        Some((p, t, _)) => (
            PlanKind::Split,
            split,
            split_total,
            format!(
                "split saves {} vs the best single venue {p} (beyond tolerance {tolerance}); legs sent in parallel, unwound on failure",
                t - split_total
            ),
        ),
        None => (
            PlanKind::Split,
            split,
            split_total,
            "no single venue lists every leg; legs sent in parallel, unwound on failure".into(),
        ),
    };
    Ok(StrategyPlan {
        kind,
        legs: planned,
        net_cost: net,
        best_split_net_cost: split_total,
        best_single_venue: singles
            .iter()
            .min_by_key(|(_, (t, _))| *t)
            .map(|(p, (t, _))| (*p, *t)),
        saving_vs_baseline: baseline_total.map(|b| b - net),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn q(p: PlatformId, a: Decimal) -> LegQuote {
        LegQuote {
            platform: p,
            native_id: format!("{p}-x"),
            amount: a,
        }
    }
    use PlatformId::{Aevo, Deribit, Derive, Okx};

    /// Bull call spread: buy K1 call, sell K2 call.
    fn spread() -> Vec<LegInput> {
        vec![
            LegInput {
                side: Side::Buy,
                quotes: vec![q(Derive, dec!(100)), q(Okx, dec!(95)), q(Deribit, dec!(97))],
            },
            LegInput {
                side: Side::Sell,
                quotes: vec![q(Derive, dec!(40)), q(Okx, dec!(38)), q(Deribit, dec!(42))],
            },
        ]
    }

    #[test]
    fn split_takes_best_venue_per_leg_and_signs_sells() {
        let p = plan(&spread(), dec!(0), Some(Derive)).unwrap();
        assert_eq!(p.kind, PlanKind::Split);
        // buy OKX 95, sell Deribit 42 -> net 53
        assert_eq!(p.net_cost, dec!(53));
        assert_eq!(p.legs[0].quote.platform, Okx);
        assert_eq!(p.legs[1].quote.platform, Deribit);
        // Derive alone: 100 - 40 = 60 -> saving 7
        assert_eq!(p.saving_vs_baseline, Some(dec!(7)));
        // best single: Deribit 97-42=55 (OKX 57, Derive 60)
        assert_eq!(p.best_single_venue, Some((Deribit, dec!(55))));
    }

    #[test]
    fn single_venue_preferred_within_tolerance_for_atomicity() {
        let p = plan(&spread(), dec!(2), Some(Derive)).unwrap();
        assert_eq!(p.kind, PlanKind::SingleVenue);
        assert_eq!(p.net_cost, dec!(55));
        assert!(p.legs.iter().all(|l| l.quote.platform == Deribit));
        assert_eq!(p.saving_vs_baseline, Some(dec!(5)));
        assert!(p.reason.contains("within the atomicity tolerance"));
    }

    #[test]
    fn credit_strategies_are_negative_and_lower_is_better() {
        // short strangle: sell put + sell call
        let legs = vec![
            LegInput {
                side: Side::Sell,
                quotes: vec![q(Derive, dec!(30)), q(Aevo, dec!(33))],
            },
            LegInput {
                side: Side::Sell,
                quotes: vec![q(Derive, dec!(25)), q(Aevo, dec!(24))],
            },
        ];
        let p = plan(&legs, dec!(0), Some(Derive)).unwrap();
        assert_eq!(p.net_cost, dec!(-58)); // receive 33 + 25
        assert_eq!(p.saving_vs_baseline, Some(dec!(3))); // Derive alone -55
    }

    #[test]
    fn uncovered_leg_and_empty_strategy_are_errors() {
        let legs = vec![LegInput {
            side: Side::Buy,
            quotes: vec![],
        }];
        assert_eq!(plan(&legs, dec!(0), None), Err(PlanError::UncoveredLeg(0)));
        assert_eq!(plan(&[], dec!(0), None), Err(PlanError::NoLegs));
    }

    #[test]
    fn max_loss_of_classic_strategies() {
        use crate::domain::OptionKind::{Call, Put};
        let l = |k: OptionKind, strike: Decimal, side: Side| PayoffLeg {
            option_kind: k,
            strike,
            side,
            qty_underlying: dec!(1),
        };
        use crate::domain::OptionKind;
        // Bull call spread 100/110 for a 4 debit: max loss = 4.
        assert_eq!(
            max_loss(
                &[
                    l(Call, dec!(100), Side::Buy),
                    l(Call, dec!(110), Side::Sell)
                ],
                dec!(4)
            ),
            Some(dec!(4))
        );
        // Bear put spread 110/100 for 3: max loss 3.
        assert_eq!(
            max_loss(
                &[l(Put, dec!(110), Side::Buy), l(Put, dec!(100), Side::Sell)],
                dec!(3)
            ),
            Some(dec!(3))
        );
        // Short call spread 100/110 for a 4 credit: max loss = 10 - 4 = 6.
        assert_eq!(
            max_loss(
                &[
                    l(Call, dec!(100), Side::Sell),
                    l(Call, dec!(110), Side::Buy)
                ],
                dec!(-4)
            ),
            Some(dec!(6))
        );
        // Iron condor 90/95/105/110, 2 credit: max loss 5 - 2 = 3.
        let ic = [
            l(Put, dec!(90), Side::Buy),
            l(Put, dec!(95), Side::Sell),
            l(Call, dec!(105), Side::Sell),
            l(Call, dec!(110), Side::Buy),
        ];
        assert_eq!(max_loss(&ic, dec!(-2)), Some(dec!(3)));
        // Naked short call: unbounded. Naked short put: bounded by the strike (minus credit).
        assert_eq!(max_loss(&[l(Call, dec!(100), Side::Sell)], dec!(-5)), None);
        assert_eq!(
            max_loss(&[l(Put, dec!(100), Side::Sell)], dec!(-5)),
            Some(dec!(95))
        );
        // Long straddle for 12: max loss 12 at the strike.
        assert_eq!(
            max_loss(
                &[l(Call, dec!(100), Side::Buy), l(Put, dec!(100), Side::Buy)],
                dec!(12)
            ),
            Some(dec!(12))
        );
    }

    #[test]
    fn no_common_venue_forces_split() {
        let legs = vec![
            LegInput {
                side: Side::Buy,
                quotes: vec![q(Derive, dec!(10))],
            },
            LegInput {
                side: Side::Buy,
                quotes: vec![q(Okx, dec!(12))],
            },
        ];
        let p = plan(&legs, dec!(100), Some(Derive)).unwrap();
        assert_eq!(p.kind, PlanKind::Split);
        assert_eq!(p.best_single_venue, None);
        assert_eq!(p.saving_vs_baseline, None);
    }
}
