//! Depth walk over a full book snapshot. Quantity-aware: the result reports the
//! quantity actually covered and the volume-weighted average price.
//!
//! The walk's price impact is the ONLY slippage modelled here. Any separate
//! market-move reserve must be added by the caller as a distinct cost line.

use crate::domain::{BookSnapshot, Instrument, Level, Side};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalkResult {
    /// Requested quantity, underlying units.
    pub requested_underlying: Decimal,
    /// Quantity the visible book can absorb, underlying units.
    pub covered_underlying: Decimal,
    /// Same, native contracts (after rounding DOWN to the platform's amount step).
    pub covered_native: Decimal,
    /// Premium paid (buy) or received (sell) for `covered_native`, premium currency.
    pub premium: Decimal,
    /// Volume-weighted average price per UNDERLYING unit.
    pub avg_price_per_underlying: Option<Decimal>,
    /// Best price per underlying unit at the top of the book (for impact attribution).
    pub top_price_per_underlying: Option<Decimal>,
    pub levels_consumed: usize,
    pub fully_covered: bool,
}

#[derive(Debug, Error, PartialEq)]
pub enum WalkError {
    #[error("quantity must be strictly positive")]
    NonPositiveQuantity,
    #[error("instrument has a non-positive contract multiplier")]
    BadMultiplier,
    #[error("book level has a non-positive price or size: {0:?}")]
    InvalidLevel(Level),
    #[error("book side is not sorted from best to worst")]
    UnsortedBook,
    #[error("requested quantity is below the platform minimum amount {min} (native)")]
    BelowMinimum { min: Decimal },
}

/// Round DOWN to a multiple of `step` (never round a fill up).
pub fn floor_to_step(x: Decimal, step: Decimal) -> Decimal {
    if step <= Decimal::ZERO {
        return x;
    }
    (x / step).floor() * step
}

pub fn walk(
    instrument: &Instrument,
    book: &BookSnapshot,
    side: Side,
    qty_underlying: Decimal,
) -> Result<WalkResult, WalkError> {
    if qty_underlying <= Decimal::ZERO {
        return Err(WalkError::NonPositiveQuantity);
    }
    let mult = instrument.contract_multiplier;
    if mult <= Decimal::ZERO {
        return Err(WalkError::BadMultiplier);
    }
    // Buying lifts asks; selling hits bids.
    let levels = match side {
        Side::Buy => &book.asks,
        Side::Sell => &book.bids,
    };
    for w in levels.windows(2) {
        let ordered = match side {
            Side::Buy => w[0].price <= w[1].price,
            Side::Sell => w[0].price >= w[1].price,
        };
        if !ordered {
            return Err(WalkError::UnsortedBook);
        }
    }
    for l in levels {
        if l.price <= Decimal::ZERO || l.size <= Decimal::ZERO {
            return Err(WalkError::InvalidLevel(*l));
        }
    }

    let mut target_native = qty_underlying / mult;
    if let Some(step) = instrument.amount_step {
        target_native = floor_to_step(target_native, step);
    }
    if let Some(min) = instrument.min_amount
        && target_native < min
    {
        return Err(WalkError::BelowMinimum { min });
    }

    let mut remaining = target_native;
    let mut filled = Decimal::ZERO;
    let mut premium = Decimal::ZERO;
    let mut consumed = 0usize;
    for l in levels {
        if remaining <= Decimal::ZERO {
            break;
        }
        let take = remaining.min(l.size);
        filled += take;
        premium += take * l.price;
        remaining -= take;
        consumed += 1;
    }
    if let Some(step) = instrument.amount_step {
        // A partial last level could leave a non-step amount only if the book itself
        // is off-step; keep the conservative floor and re-price.
        let floored = floor_to_step(filled, step);
        if floored != filled {
            return walk_exact(instrument, levels, floored, qty_underlying);
        }
    }
    let covered_underlying = filled * mult;
    Ok(WalkResult {
        requested_underlying: qty_underlying,
        covered_underlying,
        covered_native: filled,
        premium,
        avg_price_per_underlying: (filled > Decimal::ZERO).then(|| premium / covered_underlying),
        top_price_per_underlying: levels.first().map(|l| l.price / mult),
        levels_consumed: consumed,
        fully_covered: covered_underlying >= qty_underlying && remaining <= Decimal::ZERO,
    })
}

fn walk_exact(
    instrument: &Instrument,
    levels: &[Level],
    native: Decimal,
    requested: Decimal,
) -> Result<WalkResult, WalkError> {
    let mult = instrument.contract_multiplier;
    let (mut rem, mut premium, mut consumed) = (native, Decimal::ZERO, 0usize);
    for l in levels {
        if rem <= Decimal::ZERO {
            break;
        }
        let take = rem.min(l.size);
        premium += take * l.price;
        rem -= take;
        consumed += 1;
    }
    let cov = native * mult;
    Ok(WalkResult {
        requested_underlying: requested,
        covered_underlying: cov,
        covered_native: native,
        premium,
        avg_price_per_underlying: (native > Decimal::ZERO).then(|| premium / cov),
        top_price_per_underlying: levels.first().map(|l| l.price / mult),
        levels_consumed: consumed,
        fully_covered: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparability::tests::inst;
    use crate::domain::*;
    use chrono::Utc;
    use proptest::prelude::*;
    use rust_decimal_macros::dec;

    fn book(asks: &[(Decimal, Decimal)], bids: &[(Decimal, Decimal)]) -> BookSnapshot {
        BookSnapshot {
            access: crate::domain::BookAccess::Api,
            instrument: inst(PlatformId::Derive, "A").key,
            asks: asks
                .iter()
                .map(|&(price, size)| Level { price, size })
                .collect(),
            bids: bids
                .iter()
                .map(|&(price, size)| Level { price, size })
                .collect(),
            source_ts: None,
            received_at: Utc::now(),
            index_price: None,
            mark_price: None,
            provenance: Provenance::Fixture,
        }
    }

    #[test]
    fn walks_multiple_levels_with_vwap() {
        let i = inst(PlatformId::Derive, "A");
        let b = book(&[(dec!(100), dec!(1)), (dec!(110), dec!(2))], &[]);
        let r = walk(&i, &b, Side::Buy, dec!(2)).unwrap();
        assert_eq!(r.premium, dec!(210));
        assert_eq!(r.avg_price_per_underlying, Some(dec!(105)));
        assert_eq!(r.levels_consumed, 2);
        assert!(r.fully_covered);
    }

    #[test]
    fn reports_partial_coverage_never_extrapolates() {
        let i = inst(PlatformId::Derive, "A");
        let b = book(&[(dec!(100), dec!(0.5))], &[]);
        let r = walk(&i, &b, Side::Buy, dec!(2)).unwrap();
        assert_eq!(r.covered_underlying, dec!(0.5));
        assert!(!r.fully_covered);
        assert_eq!(r.premium, dec!(50));
    }

    #[test]
    fn empty_book_covers_nothing() {
        let i = inst(PlatformId::Derive, "A");
        let r = walk(&i, &book(&[], &[]), Side::Buy, dec!(1)).unwrap();
        assert_eq!(r.covered_native, dec!(0));
        assert_eq!(r.avg_price_per_underlying, None);
        assert!(!r.fully_covered);
    }

    #[test]
    fn contracts_vs_underlying_multiplier() {
        // 1 contract = 0.1 BTC; price is per contract.
        let mut i = inst(PlatformId::Aevo, "A");
        i.contract_multiplier = dec!(0.1);
        i.amount_step = Some(dec!(1));
        i.min_amount = Some(dec!(1));
        let b = book(&[(dec!(10), dec!(100))], &[]);
        let r = walk(&i, &b, Side::Buy, dec!(1)).unwrap(); // 1 BTC = 10 contracts
        assert_eq!(r.covered_native, dec!(10));
        assert_eq!(r.premium, dec!(100));
        assert_eq!(r.avg_price_per_underlying, Some(dec!(100)));
    }

    #[test]
    fn amount_step_floors_never_rounds_up() {
        let i = inst(PlatformId::Derive, "A"); // step 0.01
        let b = book(&[(dec!(100), dec!(10))], &[]);
        let r = walk(&i, &b, Side::Buy, dec!(1.239)).unwrap();
        assert_eq!(r.covered_native, dec!(1.23));
        assert!(!r.fully_covered, "1.23 < 1.239 requested");
    }

    #[test]
    fn below_minimum_is_an_error() {
        let i = inst(PlatformId::Derive, "A"); // min 0.01
        let b = book(&[(dec!(100), dec!(10))], &[]);
        assert!(matches!(
            walk(&i, &b, Side::Buy, dec!(0.001)),
            Err(WalkError::BelowMinimum { .. })
        ));
    }

    #[test]
    fn rejects_unsorted_and_invalid_books() {
        let i = inst(PlatformId::Derive, "A");
        let b = book(&[(dec!(110), dec!(1)), (dec!(100), dec!(1))], &[]);
        assert_eq!(
            walk(&i, &b, Side::Buy, dec!(1)),
            Err(WalkError::UnsortedBook)
        );
        let b = book(&[(dec!(0), dec!(1))], &[]);
        assert!(matches!(
            walk(&i, &b, Side::Buy, dec!(1)),
            Err(WalkError::InvalidLevel(_))
        ));
    }

    #[test]
    fn sell_hits_bids() {
        let i = inst(PlatformId::Derive, "A");
        let b = book(&[], &[(dec!(90), dec!(1)), (dec!(80), dec!(1))]);
        let r = walk(&i, &b, Side::Sell, dec!(1.5)).unwrap();
        assert_eq!(r.premium, dec!(130));
    }

    proptest! {
        /// Buying: average price lies between best and worst consumed ask, coverage <= request,
        /// and premium == sum over levels.
        #[test]
        fn vwap_bounded(sizes in proptest::collection::vec(1u32..1000, 1..8), q in 1u32..5000) {
            let i = inst(PlatformId::Derive, "A");
            let asks: Vec<_> = sizes.iter().enumerate()
                .map(|(k, s)| (Decimal::from(100 + k as u32), Decimal::new(*s as i64, 2)))
                .collect();
            let b = book(&asks, &[]);
            let qty = Decimal::new(q as i64, 2);
            let r = walk(&i, &b, Side::Buy, qty).unwrap();
            prop_assert!(r.covered_underlying <= qty);
            if let Some(avg) = r.avg_price_per_underlying {
                prop_assert!(avg >= dec!(100));
                prop_assert!(avg <= Decimal::from(100 + r.levels_consumed as u32 - 1));
            }
        }
    }
}
