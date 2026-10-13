//! Pre-trade risk checks. Pure: every input (limits, session usage, reference price)
//! is passed in, so the same function guards the API, the MCP and the simulator.

use crate::domain::{InstrumentKey, ProductFamily, Side, TradingMode};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskLimits {
    pub mode: TradingMode,
    pub kill_switch: bool,
    pub allowed_platforms: BTreeSet<String>,
    pub allowed_networks: BTreeSet<String>,
    pub allowed_underlyings: BTreeSet<String>,
    pub allowed_families: BTreeSet<ProductFamily>,
    /// Max premium + fees of a single order, premium currency.
    pub max_order_cost: Decimal,
    /// Max cumulated premium + fees for the session.
    pub max_session_cost: Decimal,
    /// Max |limit - reference| / reference.
    pub max_price_deviation: Decimal,
    /// Selling (possibly uncovered) options is disabled in V1.
    pub allow_sell: bool,
    /// Extra per-platform cap on a single order's worst cost (platform id -> cap). Absent = no extra cap.
    pub per_platform_max_order_cost: BTreeMap<String, Decimal>,
    /// Allowed strategies and their own caps. An intent with an unknown strategy is refused.
    pub strategies: BTreeMap<String, StrategyLimits>,
    /// Max loss a single order may produce in the worst case (long option: premium + fees).
    pub max_loss_per_order: Decimal,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyLimits {
    pub max_order_cost: Decimal,
    pub max_session_cost: Decimal,
}

/// Cumulated worst-case cost already committed in this session (total and per strategy).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionUsage {
    pub total: Decimal,
    pub by_strategy: BTreeMap<String, Decimal>,
}

impl SessionUsage {
    pub fn add(&mut self, strategy: &str, cost: Decimal) {
        self.total += cost;
        *self.by_strategy.entry(strategy.to_string()).or_default() += cost;
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderIntent {
    pub instrument: InstrumentKey,
    pub family: ProductFamily,
    pub underlying: String,
    pub side: Side,
    pub qty_underlying: Decimal,
    /// Worst acceptable premium per underlying unit.
    pub limit_price_per_underlying: Decimal,
    /// Cap on fees the user accepts.
    pub max_fee: Decimal,
    /// Strategy tag the order belongs to (limits apply per strategy).
    pub strategy: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RiskViolation {
    KillSwitchActive,
    ModeForbidsSubmission {
        mode: TradingMode,
    },
    PlatformNotAllowed {
        platform: String,
    },
    NetworkNotAllowed {
        network: String,
    },
    UnderlyingNotAllowed {
        underlying: String,
    },
    FamilyNotAllowed,
    SellDisabled,
    NonPositiveQuantity,
    NonPositiveLimit,
    OrderCostAboveCap {
        cost: String,
        cap: String,
    },
    SessionCostAboveCap {
        would_be: String,
        cap: String,
    },
    PriceDeviationTooLarge {
        deviation: String,
        max: String,
    },
    NoReferencePrice,
    PlatformCapExceeded {
        platform: String,
        cost: String,
        cap: String,
    },
    UnknownStrategy {
        strategy: String,
    },
    StrategyOrderCapExceeded {
        strategy: String,
        cost: String,
        cap: String,
    },
    StrategySessionCapExceeded {
        strategy: String,
        would_be: String,
        cap: String,
    },
    MaxLossExceeded {
        loss: String,
        cap: String,
    },
    /// A short option has no bounded worst-case loss in this model.
    MaxLossUnbounded,
}

impl RiskLimits {
    /// Conservative defaults: read-only, nothing allowed.
    pub fn locked() -> Self {
        Self {
            mode: TradingMode::ReadOnly,
            kill_switch: true,
            allowed_platforms: BTreeSet::new(),
            allowed_networks: BTreeSet::new(),
            allowed_underlyings: BTreeSet::new(),
            allowed_families: BTreeSet::new(),
            max_order_cost: Decimal::ZERO,
            max_session_cost: Decimal::ZERO,
            max_price_deviation: Decimal::ZERO,
            allow_sell: false,
            per_platform_max_order_cost: BTreeMap::new(),
            strategies: BTreeMap::new(),
            max_loss_per_order: Decimal::ZERO,
        }
    }
}

fn s(d: Decimal) -> String {
    d.normalize().to_string()
}

/// Validate an intent. `for_submission` = true when the order would actually be sent
/// (testnet/live); simulation evaluates the same rules except the mode gate.
pub fn check(
    intent: &OrderIntent,
    limits: &RiskLimits,
    session: &SessionUsage,
    reference_price_per_underlying: Option<Decimal>,
    for_submission: bool,
) -> Vec<RiskViolation> {
    let mut v = Vec::new();
    if limits.kill_switch && for_submission {
        v.push(RiskViolation::KillSwitchActive);
    }
    if for_submission
        && !matches!(
            limits.mode,
            TradingMode::Testnet | TradingMode::MainnetNonCustodial | TradingMode::Live
        )
    {
        v.push(RiskViolation::ModeForbidsSubmission { mode: limits.mode });
    }
    let p = intent.instrument.platform.as_str();
    if !limits.allowed_platforms.contains(p) {
        v.push(RiskViolation::PlatformNotAllowed { platform: p.into() });
    }
    if !limits.allowed_networks.contains(&intent.instrument.network) {
        v.push(RiskViolation::NetworkNotAllowed {
            network: intent.instrument.network.clone(),
        });
    }
    if !limits.allowed_underlyings.contains(&intent.underlying) {
        v.push(RiskViolation::UnderlyingNotAllowed {
            underlying: intent.underlying.clone(),
        });
    }
    if !limits.allowed_families.contains(&intent.family) {
        v.push(RiskViolation::FamilyNotAllowed);
    }
    if intent.side == Side::Sell && !limits.allow_sell {
        v.push(RiskViolation::SellDisabled);
    }
    if intent.qty_underlying <= Decimal::ZERO {
        v.push(RiskViolation::NonPositiveQuantity);
    }
    if intent.limit_price_per_underlying <= Decimal::ZERO {
        v.push(RiskViolation::NonPositiveLimit);
    }
    // Worst-case cost at the limit price, fees included.
    let worst = intent.limit_price_per_underlying * intent.qty_underlying + intent.max_fee;
    if worst > limits.max_order_cost {
        v.push(RiskViolation::OrderCostAboveCap {
            cost: worst.normalize().to_string(),
            cap: limits.max_order_cost.normalize().to_string(),
        });
    }
    let would_be = session.total + worst;
    if would_be > limits.max_session_cost {
        v.push(RiskViolation::SessionCostAboveCap {
            would_be: would_be.normalize().to_string(),
            cap: limits.max_session_cost.normalize().to_string(),
        });
    }
    match reference_price_per_underlying {
        Some(r) if r > Decimal::ZERO => {
            let dev = ((intent.limit_price_per_underlying - r) / r).abs();
            if dev > limits.max_price_deviation {
                v.push(RiskViolation::PriceDeviationTooLarge {
                    deviation: dev.round_dp(6).normalize().to_string(),
                    max: limits.max_price_deviation.normalize().to_string(),
                });
            }
        }
        _ => v.push(RiskViolation::NoReferencePrice),
    }
    if let Some(cap) = limits.per_platform_max_order_cost.get(p)
        && worst > *cap
    {
        v.push(RiskViolation::PlatformCapExceeded {
            platform: p.into(),
            cost: s(worst),
            cap: s(*cap),
        });
    }
    match limits.strategies.get(&intent.strategy) {
        None => v.push(RiskViolation::UnknownStrategy {
            strategy: intent.strategy.clone(),
        }),
        Some(sl) => {
            if worst > sl.max_order_cost {
                v.push(RiskViolation::StrategyOrderCapExceeded {
                    strategy: intent.strategy.clone(),
                    cost: s(worst),
                    cap: s(sl.max_order_cost),
                });
            }
            let used = session
                .by_strategy
                .get(&intent.strategy)
                .copied()
                .unwrap_or_default();
            if used + worst > sl.max_session_cost {
                v.push(RiskViolation::StrategySessionCapExceeded {
                    strategy: intent.strategy.clone(),
                    would_be: s(used + worst),
                    cap: s(sl.max_session_cost),
                });
            }
        }
    }
    // Worst-case loss: a long option can lose at most what was paid (premium + fees).
    match intent.side {
        Side::Buy if worst > limits.max_loss_per_order => v.push(RiskViolation::MaxLossExceeded {
            loss: s(worst),
            cap: s(limits.max_loss_per_order),
        }),
        Side::Buy => {}
        Side::Sell => v.push(RiskViolation::MaxLossUnbounded),
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::PlatformId;
    use rust_decimal_macros::dec;

    fn limits() -> RiskLimits {
        RiskLimits {
            mode: TradingMode::Simulation,
            kill_switch: false,
            allowed_platforms: ["derive".to_string()].into(),
            allowed_networks: ["derive-mainnet".to_string()].into(),
            allowed_underlyings: ["BTC".to_string()].into(),
            allowed_families: [ProductFamily::DatedOption].into(),
            max_order_cost: dec!(1000),
            max_session_cost: dec!(1500),
            max_price_deviation: dec!(0.2),
            allow_sell: false,
            per_platform_max_order_cost: BTreeMap::new(),
            strategies: [(
                "manual".to_string(),
                StrategyLimits {
                    max_order_cost: dec!(1000),
                    max_session_cost: dec!(1500),
                },
            )]
            .into(),
            max_loss_per_order: dec!(1000),
        }
    }

    fn used(total: Decimal) -> SessionUsage {
        let mut u = SessionUsage::default();
        if total > dec!(0) {
            u.add("manual", total);
        }
        u
    }

    fn intent() -> OrderIntent {
        OrderIntent {
            instrument: InstrumentKey {
                platform: PlatformId::Derive,
                network: "derive-mainnet".into(),
                native_id: "BTC-X".into(),
            },
            family: ProductFamily::DatedOption,
            underlying: "BTC".into(),
            side: Side::Buy,
            qty_underlying: dec!(0.1),
            limit_price_per_underlying: dec!(5000),
            max_fee: dec!(5),
            strategy: "manual".into(),
        }
    }

    #[test]
    fn valid_simulation_passes() {
        assert!(
            check(
                &intent(),
                &limits(),
                &used(dec!(0)),
                Some(dec!(4800)),
                false
            )
            .is_empty()
        );
    }

    #[test]
    fn simulation_mode_cannot_submit() {
        let v = check(&intent(), &limits(), &used(dec!(0)), Some(dec!(4800)), true);
        assert!(v.contains(&RiskViolation::ModeForbidsSubmission {
            mode: TradingMode::Simulation
        }));
    }

    #[test]
    fn locked_defaults_block_everything() {
        let v = check(
            &intent(),
            &RiskLimits::locked(),
            &used(dec!(0)),
            Some(dec!(4800)),
            true,
        );
        assert!(v.contains(&RiskViolation::KillSwitchActive));
        assert!(v.len() >= 5);
    }

    #[test]
    fn caps_and_deviation() {
        let mut i = intent();
        i.qty_underlying = dec!(1); // worst = 5005 > 1000
        let v = check(&i, &limits(), &used(dec!(0)), Some(dec!(4800)), false);
        assert!(
            v.iter()
                .any(|x| matches!(x, RiskViolation::OrderCostAboveCap { .. }))
        );
        let v = check(
            &intent(),
            &limits(),
            &used(dec!(1000)),
            Some(dec!(4800)),
            false,
        );
        assert!(
            v.iter()
                .any(|x| matches!(x, RiskViolation::SessionCostAboveCap { .. }))
        );
        let v = check(
            &intent(),
            &limits(),
            &used(dec!(0)),
            Some(dec!(1000)),
            false,
        );
        assert!(
            v.iter()
                .any(|x| matches!(x, RiskViolation::PriceDeviationTooLarge { .. }))
        );
        let v = check(&intent(), &limits(), &used(dec!(0)), None, false);
        assert!(v.contains(&RiskViolation::NoReferencePrice));
    }

    #[test]
    fn sell_is_disabled() {
        let mut i = intent();
        i.side = Side::Sell;
        assert!(
            check(&i, &limits(), &used(dec!(0)), Some(dec!(4800)), false)
                .contains(&RiskViolation::SellDisabled)
        );
    }

    #[test]
    fn platform_strategy_and_loss_limits() {
        let mut l = limits();
        l.per_platform_max_order_cost
            .insert("derive".into(), dec!(400)); // worst = 505
        let v = check(&intent(), &l, &used(dec!(0)), Some(dec!(4800)), false);
        assert!(
            v.iter()
                .any(|x| matches!(x, RiskViolation::PlatformCapExceeded { .. }))
        );

        let mut i = intent();
        i.strategy = "unknown".into();
        let v = check(&i, &limits(), &used(dec!(0)), Some(dec!(4800)), false);
        assert!(v.contains(&RiskViolation::UnknownStrategy {
            strategy: "unknown".into()
        }));

        let mut l = limits();
        l.strategies.get_mut("manual").unwrap().max_order_cost = dec!(500);
        let v = check(&intent(), &l, &used(dec!(0)), Some(dec!(4800)), false);
        assert!(
            v.iter()
                .any(|x| matches!(x, RiskViolation::StrategyOrderCapExceeded { .. }))
        );

        // Strategy session cap is per strategy: usage of another strategy does not count.
        let mut u = SessionUsage::default();
        u.add("other", dec!(1400));
        assert!(
            check(&intent(), &limits(), &u, Some(dec!(4800)), false)
                .iter()
                .all(|x| !matches!(x, RiskViolation::StrategySessionCapExceeded { .. }))
        );
        let v = check(
            &intent(),
            &limits(),
            &used(dec!(1200)),
            Some(dec!(4800)),
            false,
        );
        assert!(
            v.iter()
                .any(|x| matches!(x, RiskViolation::StrategySessionCapExceeded { .. }))
        );

        let mut l = limits();
        l.max_loss_per_order = dec!(100);
        let v = check(&intent(), &l, &used(dec!(0)), Some(dec!(4800)), false);
        assert!(v.contains(&RiskViolation::MaxLossExceeded {
            loss: "505".into(),
            cap: "100".into()
        }));

        let mut i = intent();
        i.side = Side::Sell;
        assert!(
            check(&i, &limits(), &used(dec!(0)), Some(dec!(4800)), false)
                .contains(&RiskViolation::MaxLossUnbounded)
        );
    }
}
