//! Binary (prediction-market) contracts vs listed options: "BTC above K at T, pays $1" can be
//! bought directly on Kalshi / Hyperliquid HIP-4 / Polymarket…, or SUPER-REPLICATED with a vertical
//! spread on the options venues. The spread is chosen so that it pays AT LEAST the binary's payout in
//! every scenario at expiry; if it is also cheaper, it is objectively better for the user.
//!
//! Above K (pays 1 if S > K): buy call K1, sell call K2 with K1 < K2 <= K, quantity = payout/(K2-K1).
//!   For S > K >= K2 the spread pays exactly `payout`; between K1 and K2 it pays more than 0.
//! Below K (pays 1 if S < K): buy put Kb, sell put Ka with K <= Ka < Kb, quantity = payout/(Kb-Ka).
//! Strikes are the closest listed ones (tightest dominating spread = cheapest dominating spread).
//! Same expiry timestamp only: an option expiring at another time does not replicate the binary.

use crate::engine::{CompareRequest, Engine, EngineError};
use crate::strategy::{StrategyLeg, StrategyQuote, StrategyRequest};
use chrono::{DateTime, Utc};
use oa_core::Decimal;
use oa_core::comparability::ReasonCode;
use oa_core::domain::{OptionKind, PlatformId, ProductFamily, Side};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Above,
    Below,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BinaryCompareRequest {
    pub underlying: String,
    pub direction: Direction,
    #[schemars(with = "String")]
    pub strike: Decimal,
    pub expiry: DateTime<Utc>,
    /// Payout if the binary wins, in USD (e.g. 1000 = a $1000 bet).
    #[schemars(with = "String")]
    pub payout: Decimal,
    #[serde(default)]
    pub accepted_differences: Vec<ReasonCode>,
    #[serde(default)]
    pub latency_budget_ms: Option<u64>,
    #[serde(default)]
    pub user_country: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BinaryBest {
    pub platform: PlatformId,
    pub native_id: String,
    /// All-in cost for the full payout (premium + fees).
    #[schemars(with = "String")]
    pub total: Decimal,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Replication {
    /// Vertical spread legs (same expiry), sized so the payout is reached.
    pub legs: Vec<StrategyLeg>,
    #[schemars(with = "String")]
    pub width: Decimal,
    #[schemars(with = "String")]
    pub quantity_underlying: Decimal,
    /// Net premium + fees of the cheapest plan (None if no executable plan).
    #[schemars(with = "Option<String>")]
    pub net_cost: Option<Decimal>,
    pub explanation: String,
    pub quote: StrategyQuote,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BinaryCompareResponse {
    pub computed_at: DateTime<Utc>,
    pub request: BinaryCompareRequest,
    /// Best binary contract (prediction markets / binary venues), if any is listed at this exact
    /// strike and expiry.
    pub binary_best: Option<BinaryBest>,
    /// Cheapest fully-covered binary whose fees are unpublished (premium only = LOWER BOUND of cost).
    pub binary_premium_only: Option<BinaryBest>,
    pub binary_compare: Option<crate::engine::CompareResponse>,
    pub replication: Option<Replication>,
    /// Why no replication (no option expiry at this timestamp, no strikes around K…).
    pub replication_unavailable: Option<String>,
    /// "BINARY", "OPTIONS_SPREAD" (cheapest way to get AT LEAST the payout), "UNDECIDED_UNKNOWN_FEES", or null.
    pub cheapest: Option<String>,
    #[schemars(with = "Option<String>")]
    pub saving: Option<Decimal>,
    pub notes: Vec<String>,
}

/// Up to `max` dominating pairs, tightest first (then closest to K).
pub fn dominating_pairs(
    dir: Direction,
    k: Decimal,
    strikes: &BTreeSet<Decimal>,
    max: usize,
) -> Vec<(Decimal, Decimal)> {
    let mut out = Vec::new();
    match dir {
        Direction::Above => {
            for hi in strikes.range(..=k).rev().take(3).copied() {
                for lo in strikes.range(..hi).rev().take(3).copied() {
                    out.push((lo, hi));
                }
            }
        }
        Direction::Below => {
            for lo in strikes.range(k..).take(3).copied() {
                let above = (std::ops::Bound::Excluded(lo), std::ops::Bound::Unbounded);
                for hi in strikes.range(above).take(3).copied() {
                    out.push((lo, hi));
                }
            }
        }
    }
    out.sort_by_key(|(lo, hi)| (*hi - *lo, (k - *hi).abs().min((k - *lo).abs())));
    out.truncate(max);
    out
}

pub async fn compare(
    engine: &Engine,
    req: BinaryCompareRequest,
) -> Result<BinaryCompareResponse, EngineError> {
    if req.payout <= Decimal::ZERO {
        return Err(EngineError::Invalid("payout must be > 0".into()));
    }
    let kind = match req.direction {
        Direction::Above => OptionKind::Call,
        Direction::Below => OptionKind::Put,
    };
    let mut notes = vec![
        "The options spread pays AT LEAST the binary payout in every scenario at expiry (super-replication); it can pay more between the two strikes.".to_string(),
    ];

    // 1. Binary venues (quantity = payout).
    let bin = engine
        .compare(CompareRequest {
            underlying: req.underlying.clone(),
            option_kind: kind,
            strike: req.strike,
            expiry: req.expiry,
            side: Side::Buy,
            quantity: req.payout,
            accepted_differences: req.accepted_differences.clone(),
            reference_platform: None,
            baseline_platform: None,
            max_age_ms: None,
            family: Some(ProductFamily::Binary),
            latency_budget_ms: req.latency_budget_ms,
            simulated_balances: Default::default(),
            user_country: req.user_country.clone(),
            venues: vec![],
        })
        .await;
    let (binary_best, binary_premium_only, binary_compare) = match bin {
        Ok(o) => {
            // Ranked quotes are fully covered with known costs: total is always present.
            let best = o.response.ranked.iter().find_map(|q| {
                Some(BinaryBest {
                    platform: q.estimate.instrument.platform,
                    native_id: q.estimate.instrument.native_id.clone(),
                    total: q.estimate.total_estimated?,
                })
            });
            // Fully covered but fees unpublished: premium = LOWER BOUND of the cost.
            let premium_only = o
                .response
                .incomplete
                .iter()
                .filter(|q| q.estimate.covered_underlying >= req.payout)
                .min_by_key(|q| q.estimate.premium)
                .map(|q| BinaryBest {
                    platform: q.estimate.instrument.platform,
                    native_id: q.estimate.instrument.native_id.clone(),
                    total: q.estimate.premium,
                });
            (best, premium_only, Some(o.response))
        }
        Err(EngineError::Invalid(e)) => return Err(EngineError::Invalid(e)),
        Err(e) => {
            notes.push(format!("binary venues: {e}"));
            (None, None, None)
        }
    };

    // 2. Options spread at the SAME expiry; tightest dominating pair first, wider pairs if a strike
    //    is only listed on an illiquid venue.
    let strikes = engine
        .listed_strikes(&req.underlying, kind, req.expiry)
        .await?;
    let pairs = dominating_pairs(req.direction, req.strike, &strikes, 6);
    let mut replication: Option<Replication> = None;
    let mut replication_unavailable = if strikes.is_empty() {
        Some(format!(
            "no {kind:?} option listed at expiry {}",
            req.expiry
        ))
    } else if pairs.is_empty() {
        Some("no pair of listed strikes dominates the binary at this strike".into())
    } else {
        None
    };
    for (lo, hi) in pairs {
        let width = hi - lo;
        let qty = (req.payout / width).round_dp(8);
        let (buy, sell) = match req.direction {
            Direction::Above => (lo, hi),
            Direction::Below => (hi, lo),
        };
        let legs = vec![
            StrategyLeg {
                option_kind: kind,
                strike: buy,
                expiry: req.expiry,
                side: Side::Buy,
                ratio: None,
            },
            StrategyLeg {
                option_kind: kind,
                strike: sell,
                expiry: req.expiry,
                side: Side::Sell,
                ratio: None,
            },
        ];
        let q = crate::strategy::quote(
            engine,
            StrategyRequest {
                underlying: req.underlying.clone(),
                quantity: qty,
                legs: legs.clone(),
                accepted_differences: req.accepted_differences.clone(),
                single_venue_tolerance_bps: None,
                baseline_platform: None,
                family: None,
                latency_budget_ms: req.latency_budget_ms,
                user_country: req.user_country.clone(),
                venues: vec![],
            },
        )
        .await?;
        let net_cost = q.plan.as_ref().map(|p| p.net_cost);
        let kind_s = format!("{kind:?}").to_lowercase();
        let (cmp, edge) = match req.direction {
            Direction::Above => (">=", hi),
            Direction::Below => ("<=", lo),
        };
        let explanation = format!(
            "buy {kind_s} {buy} / sell {kind_s} {sell}, {qty} units: pays {} if S {cmp} {edge} at expiry",
            req.payout.normalize()
        );
        replication = Some(Replication {
            legs,
            width,
            quantity_underlying: qty,
            net_cost,
            explanation,
            quote: q,
        });
        if net_cost.is_some() {
            replication_unavailable = None;
            break;
        }
        replication_unavailable =
            Some("no executable plan for the dominating spreads tried (liquidity)".into());
    }

    // 3. Verdict (never claims a winner the data cannot support).
    let spread_cost = replication.as_ref().and_then(|r| r.net_cost);
    let premium_floor = binary_premium_only.as_ref().map(|b| b.total);
    let (cheapest, saving) = match (binary_best.as_ref().map(|b| b.total), spread_cost) {
        (Some(b), Some(s)) if s < b && premium_floor.is_none_or(|f| s < f) => (
            Some("OPTIONS_SPREAD".to_string()),
            Some(b.min(premium_floor.unwrap_or(b)) - s),
        ),
        (Some(b), Some(s)) if b <= s => (Some("BINARY".to_string()), Some(s - b)),
        (Some(_), Some(_)) => (Some("UNDECIDED_UNKNOWN_FEES".to_string()), None),
        (None, Some(s)) => match premium_floor {
            // Cheaper than the binary's premium ALONE: cheaper whatever its fees are.
            Some(f) if s < f => (Some("OPTIONS_SPREAD".to_string()), Some(f - s)),
            Some(_) => (Some("UNDECIDED_UNKNOWN_FEES".to_string()), None),
            None => (Some("OPTIONS_SPREAD".to_string()), None),
        },
        (Some(_), None) => (Some("BINARY".to_string()), None),
        (None, None) => (None, None),
    };
    if binary_premium_only.is_some() && binary_best.is_none() {
        notes.push("Binary fees not published by the venue: its premium is shown as a lower bound of its cost.".into());
    }
    if req.expiry.format("%H:%M").to_string() != "08:00" {
        notes.push("Most options venues expire at 08:00 UTC: binaries resolving at other times usually have no same-expiry spread.".into());
    }
    Ok(BinaryCompareResponse {
        computed_at: Utc::now(),
        request: req,
        binary_best,
        binary_premium_only,
        binary_compare,
        replication,
        replication_unavailable,
        cheapest,
        saving,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// Tightest dominating pair (first of `dominating_pairs`).
    fn tightest(
        dir: Direction,
        k: Decimal,
        strikes: &BTreeSet<Decimal>,
    ) -> Option<(Decimal, Decimal)> {
        dominating_pairs(dir, k, strikes, 1).first().copied()
    }

    fn grid() -> BTreeSet<Decimal> {
        [dec!(80000), dec!(82000), dec!(84000), dec!(86000)]
            .into_iter()
            .collect()
    }

    #[test]
    fn above_uses_the_tightest_spread_at_or_below_k() {
        assert_eq!(
            tightest(Direction::Above, dec!(85000), &grid()),
            Some((dec!(82000), dec!(84000)))
        );
        assert_eq!(
            tightest(Direction::Above, dec!(84000), &grid()),
            Some((dec!(82000), dec!(84000)))
        );
        assert_eq!(tightest(Direction::Above, dec!(81000), &grid()), None);
    }

    #[test]
    fn fallback_pairs_all_dominate_and_start_tightest() {
        let p = dominating_pairs(Direction::Above, dec!(85000), &grid(), 6);
        assert_eq!(p[0], (dec!(82000), dec!(84000)));
        assert!(p.iter().all(|(lo, hi)| lo < hi && *hi <= dec!(85000)));
        let p = dominating_pairs(Direction::Below, dec!(81000), &grid(), 6);
        assert_eq!(p[0], (dec!(82000), dec!(84000)));
        assert!(p.iter().all(|(lo, hi)| lo < hi && *lo >= dec!(81000)));
    }

    #[test]
    fn below_uses_the_tightest_spread_at_or_above_k() {
        assert_eq!(
            tightest(Direction::Below, dec!(83000), &grid()),
            Some((dec!(84000), dec!(86000)))
        );
        assert_eq!(tightest(Direction::Below, dec!(86000), &grid()), None);
    }

    /// The chosen spread pays >= the binary in every state (checked on a fine grid).
    #[test]
    fn spread_dominates_the_binary_everywhere() {
        let payout = dec!(1000);
        for (dir, k) in [
            (Direction::Above, dec!(85000)),
            (Direction::Below, dec!(83000)),
        ] {
            let (lo, hi) = tightest(dir, k, &grid()).unwrap();
            let q = payout / (hi - lo);
            let mut s = dec!(70000);
            while s <= dec!(95000) {
                let spread = match dir {
                    Direction::Above => {
                        ((s - lo).max(Decimal::ZERO) - (s - hi).max(Decimal::ZERO)) * q
                    }
                    Direction::Below => {
                        ((hi - s).max(Decimal::ZERO) - (lo - s).max(Decimal::ZERO)) * q
                    }
                };
                let binary = match dir {
                    Direction::Above if s > k => payout,
                    Direction::Below if s < k => payout,
                    _ => Decimal::ZERO,
                };
                assert!(
                    spread >= binary,
                    "{dir:?} S={s}: spread {spread} < binary {binary}"
                );
                s += dec!(250);
            }
        }
    }
}
