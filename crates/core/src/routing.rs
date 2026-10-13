//! Executable routing: single venue or split across up to 3 venues (BUY side).
//!
//! Method (documented in docs/routing.md):
//! 1. For every subset of the eligible venues (size <= max_legs), allocate the quantity greedily
//!    across the MERGED ask levels, cheapest price per underlying first, in units of the
//!    strictest amount step of the subset, within each venue's budget.
//! 2. Drop legs below the venue minimum / policy minimum (smaller subsets are evaluated too).
//! 3. Re-cost every leg EXACTLY: book walk on its own venue + its fee formula (fixed fees such as
//!    Derive's $0.5 are paid once per leg — this is why splitting small orders rarely pays).
//! 4. Prefer full coverage; rank by total + execution-quality penalty; keep a split only when it
//!    saves at least `min_saving_abs` and `min_saving_bps` vs the best single venue, or when no
//!    single venue covers the quantity (reason COVERAGE).
//!
//! Unknown fees exclude the venue (listed by the caller); nothing unknown is priced as zero.

use crate::book::{floor_to_step, walk};
use crate::domain::{BookSnapshot, Instrument, InstrumentKey, Side};
use crate::fees::{FeeAmount, FeeInputs, FeeSchedule};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub struct VenueInput<'a> {
    pub instrument: &'a Instrument,
    pub book: &'a BookSnapshot,
    pub fees: &'a FeeSchedule,
    /// Max total spend (premium + fees) available on this venue; None = not constrained.
    pub budget: Option<Decimal>,
    /// Execution-quality penalty from measured fills (bps of premium). 0 without data.
    pub quality_penalty_bps: Decimal,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitPolicy {
    pub allow_split: bool,
    /// 1..=3
    pub max_legs: usize,
    /// A split must save at least this much vs the best single venue (premium currency)...
    pub min_saving_abs: Decimal,
    /// ...and at least this many basis points of the single-venue total.
    pub min_saving_bps: Decimal,
    /// No leg smaller than this (underlying units).
    pub min_leg_underlying: Decimal,
    /// Room added above the worst consumed level for IOC limit prices (bps).
    pub limit_tolerance_bps: Decimal,
}

impl Default for SplitPolicy {
    fn default() -> Self {
        Self {
            allow_split: false,
            max_legs: 2,
            min_saving_abs: Decimal::ONE,
            min_saving_bps: Decimal::from(10),
            min_leg_underlying: Decimal::ZERO,
            limit_tolerance_bps: Decimal::from(50),
        }
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Leg {
    pub instrument: InstrumentKey,
    /// Buy (default; single-intent routes) or sell (strategy legs).
    #[serde(default = "default_buy")]
    pub side: Side,
    pub qty_underlying: Decimal,
    pub qty_native: Decimal,
    pub expected_premium: Decimal,
    pub expected_avg_per_underlying: Decimal,
    /// Worst book level consumed (per native contract).
    pub worst_level_price_native: Decimal,
    /// IOC limit: worst level + tolerance, rounded UP to the tick (never below the worst level).
    pub limit_price_native: Decimal,
    pub fee: FeeAmount,
    pub expected_total: Decimal,
    pub levels_consumed: usize,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RouteKind {
    Single,
    Split,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutePlan {
    pub kind: RouteKind,
    pub legs: Vec<Leg>,
    pub requested_underlying: Decimal,
    pub covered_underlying: Decimal,
    pub fully_covered: bool,
    pub expected_total: Decimal,
    /// expected_total + quality penalties (used for ranking only).
    pub ranking_total: Decimal,
    /// Best single-venue alternative with full coverage, if any.
    pub best_single: Option<(InstrumentKey, Decimal)>,
    /// best_single total - this plan's total (>= 0 when this plan is better). Estimate only.
    pub estimated_saving_vs_best_single: Option<Decimal>,
    pub reason: String,
}

#[derive(Debug, Error, PartialEq)]
pub enum RoutingError {
    #[error("only BUY routes are supported in this version (no uncovered selling)")]
    SellNotSupported,
    #[error("quantity must be > 0")]
    NonPositiveQuantity,
    #[error("no eligible venue")]
    NoVenue,
    #[error("max_legs must be 1..=3")]
    BadPolicy,
}

fn default_buy() -> Side {
    Side::Buy
}

fn floor_to(x: Decimal, step: Option<Decimal>) -> Decimal {
    match step {
        Some(t) if t > Decimal::ZERO => (x / t).floor() * t,
        _ => x,
    }
}

/// Exact, side-aware price of one strategy leg on its own venue (buy walks asks, sell walks bids).
/// Limit: worst level consumed +/- `tolerance_bps`, rounded to the tick AWAY from the user's favour
/// only within the tolerance (buy: ceil, never below the worst level; sell: floor, never above it).
/// `expected_total` = cost incl. fees for a buy, NET PROCEEDS (premium - fees) for a sell.
pub fn price_leg_side(
    v: &VenueInput,
    qty_underlying: Decimal,
    side: Side,
    tolerance_bps: Decimal,
) -> Option<Leg> {
    let w = walk(v.instrument, v.book, side, qty_underlying).ok()?;
    if w.covered_native.is_zero() {
        return None;
    }
    let fee = v.fees.taker.compute(&FeeInputs {
        qty_native: w.covered_native,
        qty_underlying: w.covered_underlying,
        premium: w.premium,
        index_price: v.book.index_price,
        mark_price: v.book.mark_price,
    });
    let FeeAmount::Known { amount } = fee else {
        return None;
    };
    let levels = match side {
        Side::Buy => &v.book.asks,
        Side::Sell => &v.book.bids,
    };
    let worst = levels.get(w.levels_consumed.checked_sub(1)?)?.price;
    let tol = tolerance_bps / Decimal::from(10_000);
    let limit = match side {
        Side::Buy => ceil_to(worst * (Decimal::ONE + tol), v.instrument.tick_size).max(worst),
        Side::Sell => floor_to(worst * (Decimal::ONE - tol), v.instrument.tick_size).min(worst),
    };
    if limit <= Decimal::ZERO {
        return None;
    }
    Some(Leg {
        instrument: v.instrument.key.clone(),
        side,
        qty_underlying: w.covered_underlying,
        qty_native: w.covered_native,
        expected_premium: w.premium,
        expected_avg_per_underlying: w.avg_price_per_underlying?,
        worst_level_price_native: worst,
        limit_price_native: limit,
        fee: FeeAmount::Known { amount },
        expected_total: match side {
            Side::Buy => w.premium + amount,
            Side::Sell => w.premium - amount,
        },
        levels_consumed: w.levels_consumed,
    })
}

fn ceil_to(x: Decimal, step: Option<Decimal>) -> Decimal {
    match step {
        Some(t) if t > Decimal::ZERO => (x / t).ceil() * t,
        _ => x,
    }
}

/// Exact leg cost on its own venue. None if the venue cannot price it (unknown fee, walk error).
fn price_leg(v: &VenueInput, qty_underlying: Decimal, policy: &SplitPolicy) -> Option<Leg> {
    let w = walk(v.instrument, v.book, Side::Buy, qty_underlying).ok()?;
    if w.covered_native.is_zero() {
        return None;
    }
    let fee = v.fees.taker.compute(&FeeInputs {
        qty_native: w.covered_native,
        qty_underlying: w.covered_underlying,
        premium: w.premium,
        index_price: v.book.index_price,
        mark_price: v.book.mark_price,
    });
    let FeeAmount::Known { amount } = fee else {
        return None;
    };
    let worst = v.book.asks.get(w.levels_consumed.checked_sub(1)?)?.price;
    let tol = Decimal::ONE + policy.limit_tolerance_bps / Decimal::from(10_000);
    let limit = ceil_to(worst * tol, v.instrument.tick_size).max(worst);
    Some(Leg {
        instrument: v.instrument.key.clone(),
        side: Side::Buy,
        qty_underlying: w.covered_underlying,
        qty_native: w.covered_native,
        expected_premium: w.premium,
        expected_avg_per_underlying: w.avg_price_per_underlying?,
        worst_level_price_native: worst,
        limit_price_native: limit,
        fee: FeeAmount::Known { amount },
        expected_total: w.premium + amount,
        levels_consumed: w.levels_consumed,
    })
}

/// Greedy allocation over the merged books of `subset`; returns per-venue quantities.
fn allocate(
    inputs: &[VenueInput],
    subset: &[usize],
    qty: Decimal,
    policy: &SplitPolicy,
    caps: &[Option<Decimal>],
) -> Vec<Decimal> {
    let unit = subset
        .iter()
        .filter_map(|&i| {
            inputs[i]
                .instrument
                .amount_step
                .map(|s| s * inputs[i].instrument.contract_multiplier)
        })
        .fold(Decimal::ZERO, Decimal::max);
    let mut levels: Vec<(Decimal, Decimal, usize)> = subset
        .iter()
        .flat_map(|&i| {
            let m = inputs[i].instrument.contract_multiplier;
            inputs[i]
                .book
                .asks
                .iter()
                .map(move |l| (l.price / m, l.size * m, i))
        })
        .collect();
    levels.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
    let mut alloc = vec![Decimal::ZERO; inputs.len()];
    let mut spent = vec![Decimal::ZERO; inputs.len()];
    let mut remaining = floor_to_step(qty, unit);
    for (price, size, i) in levels {
        if remaining <= Decimal::ZERO {
            break;
        }
        let mut take = floor_to_step(size.min(remaining), unit);
        if let Some(cap) = caps[i] {
            take = take.min((cap - alloc[i]).max(Decimal::ZERO));
        }
        if let Some(b) = inputs[i].budget {
            // Premium only here; exact fees are checked after re-costing.
            let room = (b - spent[i]).max(Decimal::ZERO);
            if price > Decimal::ZERO {
                take = take.min(floor_to_step(room / price, unit));
            }
        }
        if take <= Decimal::ZERO {
            continue;
        }
        alloc[i] += take;
        spent[i] += take * price;
        remaining -= take;
    }
    for &i in subset {
        let inst = inputs[i].instrument;
        let min = inst
            .min_amount
            .map(|m| m * inst.contract_multiplier)
            .unwrap_or_default();
        if alloc[i] > Decimal::ZERO && (alloc[i] < min || alloc[i] < policy.min_leg_underlying) {
            alloc[i] = Decimal::ZERO;
        }
    }
    alloc
}

fn evaluate(
    inputs: &[VenueInput],
    subset: &[usize],
    qty: Decimal,
    policy: &SplitPolicy,
) -> Option<RoutePlan> {
    // A leg shrunk to fit its budget (fees included) caps that venue; the freed quantity is
    // re-allocated to the others. Converges in a few rounds (each round only lowers caps).
    let mut caps: Vec<Option<Decimal>> = vec![None; inputs.len()];
    let mut legs = Vec::new();
    for _round in 0..4 {
        let alloc = allocate(inputs, subset, qty, policy, &caps);
        legs.clear();
        let mut shrunk = false;
        for &i in subset {
            if alloc[i].is_zero() {
                continue;
            }
            let mut q = alloc[i];
            let mut leg = price_leg(&inputs[i], q, policy)?;
            if let Some(b) = inputs[i].budget {
                let step = inputs[i].instrument.amount_step.unwrap_or_default()
                    * inputs[i].instrument.contract_multiplier;
                while leg.expected_total > b {
                    if step.is_zero() {
                        return None;
                    }
                    q -= step;
                    if q <= Decimal::ZERO {
                        return None;
                    }
                    leg = price_leg(&inputs[i], q, policy)?;
                }
                if q < alloc[i] {
                    caps[i] = Some(q);
                    shrunk = true;
                }
            }
            legs.push((i, leg));
        }
        if !shrunk {
            break;
        }
    }
    if legs.is_empty() || legs.len() != subset.len() {
        // Subsets with an empty leg are evaluated as their smaller subset.
        return None;
    }
    let ranking: Decimal = legs
        .iter()
        .map(|(i, l)| {
            l.expected_total
                + l.expected_premium * inputs[*i].quality_penalty_bps / Decimal::from(10_000)
        })
        .sum();
    let legs: Vec<Leg> = legs.into_iter().map(|(_, l)| l).collect();
    let covered: Decimal = legs.iter().map(|l| l.qty_underlying).sum();
    let total: Decimal = legs.iter().map(|l| l.expected_total).sum();
    Some(RoutePlan {
        kind: if legs.len() == 1 {
            RouteKind::Single
        } else {
            RouteKind::Split
        },
        legs,
        requested_underlying: qty,
        covered_underlying: covered,
        fully_covered: covered >= qty,
        expected_total: total,
        ranking_total: ranking,
        best_single: None,
        estimated_saving_vs_best_single: None,
        reason: String::new(),
    })
}

fn subsets(n: usize, max: usize) -> Vec<Vec<usize>> {
    (1u32..(1 << n))
        .map(|m| (0..n).filter(|i| m & (1 << i) != 0).collect::<Vec<_>>())
        .filter(|s| s.len() <= max)
        .collect()
}

pub fn plan(
    inputs: &[VenueInput],
    side: Side,
    qty: Decimal,
    policy: &SplitPolicy,
) -> Result<RoutePlan, RoutingError> {
    if side == Side::Sell {
        return Err(RoutingError::SellNotSupported);
    }
    if qty <= Decimal::ZERO {
        return Err(RoutingError::NonPositiveQuantity);
    }
    if inputs.is_empty() {
        return Err(RoutingError::NoVenue);
    }
    if !(1..=3).contains(&policy.max_legs) {
        return Err(RoutingError::BadPolicy);
    }
    let max = if policy.allow_split {
        policy.max_legs
    } else {
        1
    };
    let plans: Vec<RoutePlan> = subsets(inputs.len(), max)
        .iter()
        .filter_map(|s| evaluate(inputs, s, qty, policy))
        .collect();
    let by_rank = |a: &&RoutePlan, b: &&RoutePlan| a.ranking_total.cmp(&b.ranking_total);
    let best_single = plans
        .iter()
        .filter(|p| p.kind == RouteKind::Single && p.fully_covered)
        .min_by(by_rank)
        .cloned();
    let best_full = plans
        .iter()
        .filter(|p| p.fully_covered)
        .min_by(by_rank)
        .cloned();
    let mut chosen = match (best_full, &best_single) {
        (Some(full), Some(single)) if full.kind == RouteKind::Split => {
            let saving = single.expected_total - full.expected_total;
            let bps = if single.expected_total > Decimal::ZERO {
                saving / single.expected_total * Decimal::from(10_000)
            } else {
                Decimal::ZERO
            };
            if saving >= policy.min_saving_abs && bps >= policy.min_saving_bps {
                RoutePlan {
                    reason: format!(
                        "SPLIT_COST: saves {} ({} bps) vs the best single venue",
                        saving.round_dp(6).normalize(),
                        bps.round_dp(2).normalize()
                    ),
                    ..full
                }
            } else {
                RoutePlan {
                    reason: format!(
                        "SINGLE: a split would save only {} ({} bps), below the thresholds ({} / {} bps)",
                        saving.round_dp(6).normalize(),
                        bps.round_dp(2).normalize(),
                        policy.min_saving_abs.normalize(),
                        policy.min_saving_bps.normalize()
                    ),
                    ..single.clone()
                }
            }
        }
        (Some(full), Some(_)) => RoutePlan {
            reason: "SINGLE: cheapest full-coverage venue".into(),
            ..full
        },
        (Some(full), None) => RoutePlan {
            reason: "SPLIT_COVERAGE: no single venue covers the quantity; the split does".into(),
            ..full
        },
        (None, _) => {
            // Nothing covers the quantity: best partial plan, marked not fully covered.
            let best = plans
                .iter()
                .max_by(|a, b| {
                    a.covered_underlying
                        .cmp(&b.covered_underlying)
                        .then(b.ranking_total.cmp(&a.ranking_total))
                })
                .cloned()
                .ok_or(RoutingError::NoVenue)?;
            RoutePlan {
                reason: "PARTIAL: visible depth of all eligible venues does not cover the quantity"
                    .into(),
                ..best
            }
        }
    };
    if let Some(s) = &best_single {
        chosen.best_single = Some((s.legs[0].instrument.clone(), s.expected_total));
        chosen.estimated_saving_vs_best_single = Some(s.expected_total - chosen.expected_total);
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparability::tests::inst;
    use crate::domain::{Level, PlatformId, Provenance};
    use crate::fees::FeeFormula;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn book(i: &Instrument, asks: &[(Decimal, Decimal)]) -> BookSnapshot {
        BookSnapshot {
            access: crate::domain::BookAccess::Api,
            instrument: i.key.clone(),
            bids: vec![],
            asks: asks
                .iter()
                .map(|&(price, size)| Level { price, size })
                .collect(),
            source_ts: None,
            received_at: Utc::now(),
            index_price: Some(dec!(60000)),
            mark_price: Some(dec!(100)),
            provenance: Provenance::Fixture,
        }
    }

    fn fees(fixed: Decimal) -> FeeSchedule {
        FeeSchedule {
            taker: FeeFormula::Fixed { amount: fixed },
            maker: FeeFormula::Fixed { amount: fixed },
            currency: "USDC".into(),
            source_url: "t".into(),
            verified_on: "t".into(),
            notes: String::new(),
        }
    }

    fn split_policy() -> SplitPolicy {
        SplitPolicy {
            allow_split: true,
            max_legs: 2,
            min_saving_abs: dec!(1),
            min_saving_bps: dec!(0),
            ..Default::default()
        }
    }

    struct Fx {
        a: Instrument,
        b: Instrument,
        ba: BookSnapshot,
        bb: BookSnapshot,
        f: FeeSchedule,
    }

    fn fx(asks_a: &[(Decimal, Decimal)], asks_b: &[(Decimal, Decimal)], fixed: Decimal) -> Fx {
        let a = inst(PlatformId::Derive, "A");
        let b = inst(PlatformId::Aevo, "B");
        Fx {
            ba: book(&a, asks_a),
            bb: book(&b, asks_b),
            a,
            b,
            f: fees(fixed),
        }
    }

    fn inputs(x: &Fx) -> Vec<VenueInput<'_>> {
        vec![
            VenueInput {
                instrument: &x.a,
                book: &x.ba,
                fees: &x.f,
                budget: None,
                quality_penalty_bps: dec!(0),
            },
            VenueInput {
                instrument: &x.b,
                book: &x.bb,
                fees: &x.f,
                budget: None,
                quality_penalty_bps: dec!(0),
            },
        ]
    }

    #[test]
    fn single_when_split_disabled_even_if_cheaper() {
        let x = fx(
            &[(dec!(100), dec!(1)), (dec!(150), dec!(5))],
            &[(dec!(101), dec!(1)), (dec!(160), dec!(5))],
            dec!(0),
        );
        let p = plan(&inputs(&x), Side::Buy, dec!(2), &SplitPolicy::default()).unwrap();
        assert_eq!(p.kind, RouteKind::Single);
    }

    #[test]
    fn split_when_it_saves_enough() {
        // Each venue has 1 cheap contract; buying 2 on one venue costs 100+150 or 101+160.
        let x = fx(
            &[(dec!(100), dec!(1)), (dec!(150), dec!(5))],
            &[(dec!(101), dec!(1)), (dec!(160), dec!(5))],
            dec!(0),
        );
        let p = plan(&inputs(&x), Side::Buy, dec!(2), &split_policy()).unwrap();
        assert_eq!(p.kind, RouteKind::Split, "{}", p.reason);
        assert_eq!(p.expected_total, dec!(201));
        assert_eq!(p.best_single.as_ref().unwrap().1, dec!(250));
        assert_eq!(p.estimated_saving_vs_best_single, Some(dec!(49)));
        assert!(p.reason.starts_with("SPLIT_COST"));
    }

    #[test]
    fn fixed_fee_per_leg_can_make_split_worse() {
        // Saving on price = 1, but the second leg pays another fixed fee of 5.
        let x = fx(
            &[(dec!(100), dec!(1)), (dec!(101), dec!(5))],
            &[(dec!(100), dec!(1)), (dec!(102), dec!(5))],
            dec!(5),
        );
        let p = plan(&inputs(&x), Side::Buy, dec!(2), &split_policy()).unwrap();
        assert_eq!(p.kind, RouteKind::Single, "{}", p.reason);
        assert_eq!(p.expected_total, dec!(206));
    }

    #[test]
    fn split_enables_coverage() {
        let x = fx(&[(dec!(100), dec!(1))], &[(dec!(105), dec!(1))], dec!(0));
        let p = plan(&inputs(&x), Side::Buy, dec!(2), &split_policy()).unwrap();
        assert!(p.fully_covered);
        assert!(p.reason.starts_with("SPLIT_COVERAGE"));
        assert_eq!(p.best_single, None);
        let p = plan(&inputs(&x), Side::Buy, dec!(2), &SplitPolicy::default()).unwrap();
        assert!(!p.fully_covered, "without split: partial plan, flagged");
        assert!(p.reason.starts_with("PARTIAL"));
    }

    #[test]
    fn budgets_are_respected_with_fees() {
        let x = fx(&[(dec!(100), dec!(5))], &[(dec!(110), dec!(5))], dec!(1));
        let mut ins = inputs(&x);
        ins[0].budget = Some(dec!(150)); // derive: at most 1 contract (100 + 1 fee)
        let p = plan(&ins, Side::Buy, dec!(2), &split_policy()).unwrap();
        let derive_leg = p
            .legs
            .iter()
            .find(|l| l.instrument.platform == PlatformId::Derive)
            .unwrap();
        assert!(derive_leg.expected_total <= dec!(150));
        assert!(p.fully_covered);
    }

    #[test]
    fn quality_penalty_shifts_ranking_not_prices() {
        let x = fx(&[(dec!(100), dec!(5))], &[(dec!(100.5), dec!(5))], dec!(0));
        let mut ins = inputs(&x);
        ins[0].quality_penalty_bps = dec!(100); // derive measured 1 % adverse slippage
        let p = plan(&ins, Side::Buy, dec!(1), &SplitPolicy::default()).unwrap();
        assert_eq!(p.legs[0].instrument.platform, PlatformId::Aevo);
        assert_eq!(
            p.expected_total,
            dec!(100.5),
            "expected total stays the unpenalised estimate"
        );
    }

    #[test]
    fn limit_price_is_above_worst_level_and_on_tick() {
        let x = fx(
            &[(dec!(100), dec!(1)), (dec!(103.37), dec!(5))],
            &[],
            dec!(0),
        );
        let ins = vec![VenueInput {
            instrument: &x.a,
            book: &x.ba,
            fees: &x.f,
            budget: None,
            quality_penalty_bps: dec!(0),
        }];
        let p = plan(&ins, Side::Buy, dec!(2), &SplitPolicy::default()).unwrap();
        let l = &p.legs[0];
        assert_eq!(l.worst_level_price_native, dec!(103.37));
        // 103.37 * 1.005 = 103.88685 -> ceil to tick 0.1 = 103.9
        assert_eq!(l.limit_price_native, dec!(103.9));
    }

    #[test]
    fn unknown_fee_venue_is_never_routed() {
        let mut x = fx(&[(dec!(1), dec!(5))], &[(dec!(100), dec!(5))], dec!(0));
        let unknown = FeeSchedule {
            taker: FeeFormula::Unknown { reason: "x".into() },
            ..x.f.clone()
        };
        let ins = vec![
            VenueInput {
                instrument: &x.a,
                book: &x.ba,
                fees: &unknown,
                budget: None,
                quality_penalty_bps: dec!(0),
            },
            VenueInput {
                instrument: &x.b,
                book: &x.bb,
                fees: &x.f,
                budget: None,
                quality_penalty_bps: dec!(0),
            },
        ];
        let p = plan(&ins, Side::Buy, dec!(1), &split_policy()).unwrap();
        assert!(
            p.legs
                .iter()
                .all(|l| l.instrument.platform == PlatformId::Aevo)
        );
        x.f = fees(dec!(0));
    }

    #[test]
    fn sell_and_bad_policy_refused() {
        let x = fx(&[(dec!(1), dec!(5))], &[], dec!(0));
        let ins = vec![VenueInput {
            instrument: &x.a,
            book: &x.ba,
            fees: &x.f,
            budget: None,
            quality_penalty_bps: dec!(0),
        }];
        assert_eq!(
            plan(&ins, Side::Sell, dec!(1), &SplitPolicy::default()),
            Err(RoutingError::SellNotSupported)
        );
        let bad = SplitPolicy {
            max_legs: 4,
            ..Default::default()
        };
        assert_eq!(
            plan(&ins, Side::Buy, dec!(1), &bad),
            Err(RoutingError::BadPolicy)
        );
    }

    proptest::proptest! {
        /// Whatever the books: legs never exceed the request, every leg respects the venue
        /// minimum, and the chosen plan is never more expensive than the best single venue.
        #[test]
        fn split_never_worse_than_best_single(
            a in proptest::collection::vec((90u32..200, 1u32..300), 1..5),
            b in proptest::collection::vec((90u32..200, 1u32..300), 1..5),
            q in 1u32..600, fixed in 0u32..5,
        ) {
            let mut a: Vec<(Decimal, Decimal)> = a.into_iter().map(|(p, s)| (Decimal::from(p), Decimal::new(s as i64, 2))).collect();
            let mut b: Vec<(Decimal, Decimal)> = b.into_iter().map(|(p, s)| (Decimal::from(p), Decimal::new(s as i64, 2))).collect();
            a.sort(); b.sort(); a.dedup_by_key(|x| x.0); b.dedup_by_key(|x| x.0);
            let x = fx(&a, &b, Decimal::from(fixed));
            let qty = Decimal::new(q as i64, 2);
            let p = plan(&inputs(&x), Side::Buy, qty, &split_policy()).unwrap();
            proptest::prop_assert!(p.covered_underlying <= qty);
            for l in &p.legs { proptest::prop_assert!(l.qty_underlying >= dec!(0.01)); }
            if let (true, Some((_, single))) = (p.fully_covered, &p.best_single) {
                proptest::prop_assert!(p.expected_total <= *single);
            }
        }
    }
}
