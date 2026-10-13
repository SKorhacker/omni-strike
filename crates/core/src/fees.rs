//! Fee formulas. Unknown fees are represented explicitly and are NEVER zero.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Inputs available when computing a fee, per fill.
#[derive(Debug, Clone, Copy)]
pub struct FeeInputs {
    /// Native contracts.
    pub qty_native: Decimal,
    /// Underlying units (qty_native * multiplier).
    pub qty_underlying: Decimal,
    /// Premium of the fill, premium currency.
    pub premium: Decimal,
    /// Index/spot price of the underlying, premium currency. `None` if unavailable.
    pub index_price: Option<Decimal>,
    /// Platform mark price per NATIVE contract, premium currency. `None` if unavailable.
    pub mark_price: Option<Decimal>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FeeFormula {
    /// rate * qty_underlying * index_price
    PctOfUnderlyingNotional {
        rate: Decimal,
    },
    /// rate * premium actually traded
    PctOfPremium {
        rate: Decimal,
    },
    /// rate * mark_price * qty_native (e.g. Derive `mark_price_fee_rate_cap`)
    PctOfMarkValue {
        rate: Decimal,
    },
    /// amount * qty_native
    PerContract {
        amount: Decimal,
    },
    /// flat amount per order
    Fixed {
        amount: Decimal,
    },
    /// Prediction-market quadratic fee: rate * qty_native * p * (1 - p), p = average price per
    /// contract as a fraction of the payout (contract_multiplier, usually 1). `round_up_to`: the venue
    /// rounds the order's fee UP to this increment (Kalshi: 0.01 USD); None = no rounding (Polymarket).
    QuadraticProbability {
        rate: Decimal,
        round_up_to: Option<Decimal>,
    },
    Sum {
        parts: Vec<FeeFormula>,
    },
    Min {
        parts: Vec<FeeFormula>,
    },
    /// The fee exists but we could not verify its value.
    Unknown {
        reason: String,
    },
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FeeAmount {
    Known { amount: Decimal },
    Unknown { reason: String },
}

impl FeeFormula {
    pub fn compute(&self, x: &FeeInputs) -> FeeAmount {
        use FeeAmount::*;
        match self {
            FeeFormula::PctOfUnderlyingNotional { rate } => match x.index_price {
                Some(p) => Known {
                    amount: *rate * x.qty_underlying * p,
                },
                None => Unknown {
                    reason: "index price unavailable for notional-based fee".into(),
                },
            },
            FeeFormula::PctOfPremium { rate } => Known {
                amount: *rate * x.premium,
            },
            FeeFormula::PctOfMarkValue { rate } => match x.mark_price {
                Some(m) => Known {
                    amount: *rate * m * x.qty_native,
                },
                None => Unknown {
                    reason: "mark price unavailable for mark-based fee cap".into(),
                },
            },
            FeeFormula::PerContract { amount } => Known {
                amount: *amount * x.qty_native,
            },
            FeeFormula::Fixed { amount } => Known { amount: *amount },
            FeeFormula::Unknown { reason } => Unknown {
                reason: reason.clone(),
            },
            FeeFormula::QuadraticProbability { rate, round_up_to } => {
                if x.qty_native <= Decimal::ZERO || x.qty_underlying <= Decimal::ZERO {
                    return Known {
                        amount: Decimal::ZERO,
                    };
                }
                // payout per contract = qty_underlying / qty_native (the contract multiplier)
                let payout = x.qty_underlying / x.qty_native;
                let p = (x.premium / x.qty_native / payout).clamp(Decimal::ZERO, Decimal::ONE);
                let raw = *rate * x.qty_native * payout * p * (Decimal::ONE - p);
                let amount = match round_up_to {
                    Some(inc) if *inc > Decimal::ZERO => (raw / inc).ceil() * inc,
                    _ => raw,
                };
                Known { amount }
            }
            FeeFormula::Sum { parts } => {
                let mut total = Decimal::ZERO;
                for p in parts {
                    match p.compute(x) {
                        Known { amount } => total += amount,
                        u @ Unknown { .. } => return u,
                    }
                }
                Known { amount: total }
            }
            FeeFormula::Min { parts } => {
                // A min() with an unknown part is unknown: the unknown could be the smallest.
                let mut best: Option<Decimal> = None;
                for p in parts {
                    match p.compute(x) {
                        Known { amount } => best = Some(best.map_or(amount, |b| b.min(amount))),
                        u @ Unknown { .. } => return u,
                    }
                }
                match best {
                    Some(amount) => Known { amount },
                    None => Unknown {
                        reason: "empty min()".into(),
                    },
                }
            }
        }
    }
}

/// Fee schedule for one platform + product, with its documentary source.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeSchedule {
    pub taker: FeeFormula,
    pub maker: FeeFormula,
    pub currency: String,
    pub source_url: String,
    /// ISO date the source was read.
    pub verified_on: String,
    pub notes: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn x(index: Option<Decimal>) -> FeeInputs {
        FeeInputs {
            qty_native: dec!(2),
            qty_underlying: dec!(2),
            premium: dec!(1000),
            index_price: index,
            mark_price: Some(dec!(400)),
        }
    }

    #[test]
    fn min_of_notional_and_premium_cap() {
        let f = FeeFormula::Min {
            parts: vec![
                FeeFormula::PctOfUnderlyingNotional { rate: dec!(0.0003) },
                FeeFormula::PctOfPremium { rate: dec!(0.125) },
            ],
        };
        // 0.0003*2*60000 = 36 ; 0.125*1000 = 125 -> 36
        assert_eq!(
            f.compute(&x(Some(dec!(60000)))),
            FeeAmount::Known { amount: dec!(36) }
        );
    }

    #[test]
    fn missing_index_makes_fee_unknown_not_zero() {
        let f = FeeFormula::Sum {
            parts: vec![
                FeeFormula::Fixed { amount: dec!(0.5) },
                FeeFormula::PctOfUnderlyingNotional { rate: dec!(0.0003) },
            ],
        };
        assert!(matches!(f.compute(&x(None)), FeeAmount::Unknown { .. }));
    }

    #[test]
    fn derive_style_cap_uses_mark_value_not_traded_premium() {
        // $0.5 + min(0.03% notional, 12.5% of mark value)
        let f = FeeFormula::Sum {
            parts: vec![
                FeeFormula::Fixed { amount: dec!(0.5) },
                FeeFormula::Min {
                    parts: vec![
                        FeeFormula::PctOfUnderlyingNotional { rate: dec!(0.0003) },
                        FeeFormula::PctOfMarkValue { rate: dec!(0.125) },
                    ],
                },
            ],
        };
        // notional part 0.0003*2*60000 = 36 ; mark part 0.125*400*2 = 100 -> 0.5 + 36
        assert_eq!(
            f.compute(&x(Some(dec!(60000)))),
            FeeAmount::Known { amount: dec!(36.5) }
        );
        // deep OTM: mark 10 -> cap 0.125*10*2 = 2.5 -> 0.5 + 2.5
        let mut i = x(Some(dec!(60000)));
        i.mark_price = Some(dec!(10));
        assert_eq!(f.compute(&i), FeeAmount::Known { amount: dec!(3.0) });
        i.mark_price = None;
        assert!(matches!(f.compute(&i), FeeAmount::Unknown { .. }));
    }

    #[test]
    fn unknown_inside_min_propagates() {
        let f = FeeFormula::Min {
            parts: vec![
                FeeFormula::Fixed { amount: dec!(1) },
                FeeFormula::Unknown { reason: "x".into() },
            ],
        };
        assert!(matches!(
            f.compute(&x(Some(dec!(1)))),
            FeeAmount::Unknown { .. }
        ));
    }
    #[test]
    fn quadratic_probability_fee_matches_kalshi_and_polymarket() {
        use rust_decimal_macros::dec;
        // Kalshi: 100 contracts at 0.55 -> 0.07*100*0.55*0.45 = 1.7325 -> rounded up 1.74 USD.
        let k = FeeFormula::QuadraticProbability {
            rate: dec!(0.07),
            round_up_to: Some(dec!(0.01)),
        };
        let x = FeeInputs {
            qty_native: dec!(100),
            qty_underlying: dec!(100),
            premium: dec!(55),
            index_price: None,
            mark_price: None,
        };
        assert_eq!(k.compute(&x), FeeAmount::Known { amount: dec!(1.74) });
        // Polymarket: same, no rounding.
        let p = FeeFormula::QuadraticProbability {
            rate: dec!(0.07),
            round_up_to: None,
        };
        assert_eq!(
            p.compute(&x),
            FeeAmount::Known {
                amount: dec!(1.7325)
            }
        );
        // Deep in the money (p -> 1): fee -> 0.
        let y = FeeInputs {
            premium: dec!(99),
            ..x
        };
        assert_eq!(
            p.compute(&y),
            FeeAmount::Known {
                amount: dec!(0.0693)
            }
        );
    }
}
