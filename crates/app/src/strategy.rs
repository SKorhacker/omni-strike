//! Multi-leg strategy quotes: every leg is compared across all venues (same engine path as
//! `/v1/quotes/compare`, legs in parallel), then `oa_core::strategy::plan` applies the atomicity
//! ladder (single venue within tolerance, else best split).

use crate::engine::{CompareRequest, Engine, EngineError, VenueIssue};
use chrono::{DateTime, Utc};
use oa_core::Decimal;
use oa_core::comparability::ReasonCode;
use oa_core::domain::{OptionKind, PlatformId, ProductFamily, Side};
use oa_core::strategy::{LegInput, LegQuote, StrategyPlan, plan};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_LEGS: usize = 6;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StrategyLeg {
    pub option_kind: OptionKind,
    #[schemars(with = "String")]
    pub strike: Decimal,
    pub expiry: DateTime<Utc>,
    pub side: Side,
    /// Leg size = strategy `quantity` x `ratio` (default 1), e.g. 2 for the body of a ratio spread.
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub ratio: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StrategyRequest {
    pub underlying: String,
    /// Base size in underlying units (each leg = quantity x ratio).
    #[schemars(with = "String")]
    pub quantity: Decimal,
    /// 1..=6 legs.
    pub legs: Vec<StrategyLeg>,
    #[serde(default)]
    pub accepted_differences: Vec<ReasonCode>,
    /// Prefer ONE venue for the whole strategy (atomic where supported) when it costs at most this
    /// many basis points of the strategy notional (quantity x sum of ratios x index) more than the
    /// best split. Default 10.
    #[serde(default)]
    pub single_venue_tolerance_bps: Option<u32>,
    /// Savings are reported against doing the whole strategy on this venue. Default Derive.
    #[serde(default)]
    pub baseline_platform: Option<PlatformId>,
    #[serde(default)]
    pub family: Option<ProductFamily>,
    #[serde(default)]
    pub latency_budget_ms: Option<u64>,
    /// End user's country (ISO alpha-2): restricted venues are excluded from every leg.
    #[serde(default)]
    pub user_country: Option<String>,
    /// Routing only: legs may use only these venues (e.g. where the user has an account).
    /// Empty = every venue.
    #[serde(default)]
    pub venues: Vec<PlatformId>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LegDetail {
    pub leg: usize,
    pub request: StrategyLeg,
    #[schemars(with = "String")]
    pub quantity: Decimal,
    /// Eligible, fully covered quotes (cost if buying, net proceeds if selling), best first.
    pub quotes: Vec<LegQuote>,
    pub unavailable: Vec<VenueIssue>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StrategyQuote {
    pub computed_at: DateTime<Utc>,
    pub wording: String,
    pub request: StrategyRequest,
    pub plan: Option<StrategyPlan>,
    /// Why no plan could be built (a leg not fully covered anywhere, ...).
    pub no_plan_reason: Option<String>,
    pub legs: Vec<LegDetail>,
    #[schemars(with = "Option<String>")]
    pub index_price: Option<Decimal>,
    /// Tolerance actually applied, premium currency.
    #[schemars(with = "String")]
    pub single_venue_tolerance: Decimal,
    pub notes: Vec<String>,
}

pub async fn quote(engine: &Engine, req: StrategyRequest) -> Result<StrategyQuote, EngineError> {
    Ok(quote_inner(engine, req, |_| true).await?.0)
}

/// Quote + the per-leg comparison outcomes (books, fees, instruments) for routing. `allowed` filters
/// the venues a leg may use (e.g. only venues with an execution adapter in TESTNET).
async fn quote_inner(
    engine: &Engine,
    req: StrategyRequest,
    allowed: impl Fn(PlatformId) -> bool,
) -> Result<(StrategyQuote, Vec<crate::engine::CompareOutcome>), EngineError> {
    if req.legs.is_empty() || req.legs.len() > MAX_LEGS {
        return Err(EngineError::Invalid(format!(
            "legs must contain 1..={MAX_LEGS} entries"
        )));
    }
    // Each leg (quantity x ratio <= 100) is bounded again by the compare limits; this bound only
    // keeps the multiplication itself from overflowing.
    if req.quantity <= Decimal::ZERO || req.quantity > Decimal::from(1_000_000) {
        return Err(EngineError::Invalid(
            "quantity must be in (0, 1000000]".into(),
        ));
    }
    for l in &req.legs {
        if l.ratio
            .is_some_and(|r| r <= Decimal::ZERO || r > Decimal::from(100))
        {
            return Err(EngineError::Invalid("ratio must be in (0, 100]".into()));
        }
    }
    let tol_bps = req.single_venue_tolerance_bps.unwrap_or(10).min(10_000);
    let baseline = req.baseline_platform.or(Some(PlatformId::Derive));

    let futs = req.legs.iter().map(|l| {
        let qty = req.quantity * l.ratio.unwrap_or(Decimal::ONE);
        let creq = CompareRequest {
            underlying: req.underlying.clone(),
            option_kind: l.option_kind,
            strike: l.strike,
            expiry: l.expiry,
            side: l.side,
            quantity: qty,
            accepted_differences: req.accepted_differences.clone(),
            reference_platform: None,
            baseline_platform: baseline,
            max_age_ms: None,
            family: req.family,
            latency_budget_ms: req.latency_budget_ms,
            simulated_balances: Default::default(),
            user_country: req.user_country.clone(),
            venues: vec![],
        };
        async move { (qty, engine.compare(creq).await) }
    });
    let results = futures_util::future::join_all(futs).await;

    let mut details = Vec::new();
    let mut inputs = Vec::new();
    let mut outcomes = Vec::new();
    let mut index = None;
    let mut sum_qty = Decimal::ZERO;
    for (i, ((qty, r), l)) in results.into_iter().zip(req.legs.iter()).enumerate() {
        let out = r?;
        let resp = out.response.clone();
        sum_qty += qty;
        let mut quotes: Vec<LegQuote> = resp
            .ranked
            .iter()
            .filter(|q| q.estimate.covered_underlying >= qty)
            .filter(|q| allowed(q.estimate.instrument.platform))
            .filter_map(|q| {
                q.estimate.total_estimated.map(|t| LegQuote {
                    platform: q.estimate.instrument.platform,
                    native_id: q.estimate.instrument.native_id.clone(),
                    amount: t,
                })
            })
            .collect();
        match l.side {
            Side::Buy => quotes.sort_by_key(|q| q.amount),
            Side::Sell => quotes.sort_by_key(|q| std::cmp::Reverse(q.amount)),
        }
        index = index.or_else(|| {
            resp.ranked
                .iter()
                .chain(resp.incomplete.iter())
                .find_map(|q| q.estimate.index_price)
        });
        inputs.push(LegInput {
            side: l.side,
            quotes: quotes.clone(),
        });
        outcomes.push(out);
        details.push(LegDetail {
            leg: i,
            request: l.clone(),
            quantity: qty,
            quotes,
            unavailable: resp.unavailable,
        });
    }
    let tolerance = index
        .map(|ix| ix * sum_qty * Decimal::from(tol_bps) / Decimal::from(10_000))
        .unwrap_or(Decimal::ZERO);
    let (plan, no_plan_reason) = match plan(&inputs, tolerance, baseline) {
        Ok(p) => (Some(p), None),
        Err(e) => (None, Some(e.to_string())),
    };
    Ok((StrategyQuote {
        computed_at: Utc::now(),
        wording: "Estimated net cost of the strategy (buy costs minus sell proceeds, fees included) from visible books; positive = debit, negative = credit. Not a fill guarantee.".into(),
        request: req,
        plan,
        no_plan_reason,
        legs: details,
        index_price: index,
        single_venue_tolerance: tolerance,
        notes: vec![
            "Atomicity ladder: a single venue is preferred within the tolerance (combo/atomic where the venue supports it); otherwise legs are sent in parallel and filled legs are unwound if another leg fails.".into(),
            "Sell legs assume the account can post the venue's required margin/collateral (not modelled).".into(),
        ],
    }, outcomes))
}

// ---------------------------------------------------------------- executable strategy routes

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StrategyRouteRequest {
    #[serde(flatten)]
    pub strategy: StrategyRequest,
    /// IOC limit tolerance per leg vs its worst consumed level (bps). Default: integrator
    /// max_slippage_bps, else 50.
    #[serde(default)]
    pub limit_tolerance_bps: Option<u32>,
    /// Max unwind cost if a leg fails (bps of notional). Default: integrator setting, else 30.
    #[serde(default)]
    pub unwind_cost_cap_bps: Option<u32>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StrategyRoute {
    pub route_id: uuid::Uuid,
    pub route_hash: String,
    pub expires_at: DateTime<Utc>,
    pub executable: bool,
    pub not_executable_reasons: Vec<String>,
    /// Executable legs with IOC limits (sides included); execute with POST /v1/routes/execute-testnet.
    pub route: Option<oa_core::routing::RoutePlan>,
    /// Worst loss at expiry (premium currency); null = unbounded or not computable (mixed expiries).
    #[schemars(with = "Option<String>")]
    pub max_loss: Option<Decimal>,
    pub unwind_cost_cap_bps: u32,
    pub quote: StrategyQuote,
}

pub async fn route(
    engine: &Engine,
    req: StrategyRouteRequest,
    allow_unbounded: bool,
) -> Result<StrategyRoute, EngineError> {
    use oa_core::routing::{Leg, RouteKind, RoutePlan, VenueInput, price_leg_side};
    let exec = engine.execution().cloned();
    let testnet = engine.mode.executes();
    // In an executing mode (TESTNET, MAINNET_NONCUSTODIAL) only venues we can actually execute on
    // are eligible for routing.
    let allowed = req.strategy.venues.clone();
    // Like single routes: an execution route waits for every venue's book unless the caller sets a
    // budget (the fast display default would randomly drop slow venues from a leg).
    let mut sreq = req.strategy.clone();
    sreq.latency_budget_ms
        .get_or_insert(crate::engine::BOOK_WAIT.as_millis() as u64);
    let (q, outcomes) = quote_inner(engine, sreq, |p| {
        (allowed.is_empty() || allowed.contains(&p))
            && (!testnet
                || exec
                    .as_ref()
                    .is_some_and(|x| x.executable_network(p).is_some()))
    })
    .await?;
    let tol = Decimal::from(req.limit_tolerance_bps.unwrap_or(50).min(1_000));
    let unwind_cap = req.unwind_cost_cap_bps.unwrap_or(30).min(1_000);
    let now = Utc::now();
    let expires_at = now + chrono::Duration::seconds(30);
    let route_id = uuid::Uuid::new_v4();
    let mut reasons = Vec::new();
    if !testnet {
        reasons.push(format!(
            "execution exists only in TESTNET or MAINNET_NONCUSTODIAL (server mode is {:?})",
            engine.mode
        ));
    }
    let Some(plan) = q.plan.clone() else {
        reasons.push(q.no_plan_reason.clone().unwrap_or_else(|| "no plan".into()));
        return Ok(StrategyRoute {
            route_id,
            route_hash: String::new(),
            expires_at,
            executable: false,
            not_executable_reasons: reasons,
            route: None,
            max_loss: None,
            unwind_cost_cap_bps: unwind_cap,
            quote: q,
        });
    };
    let mut legs: Vec<Leg> = Vec::new();
    let mut used = Vec::new();
    for pl in &plan.legs {
        let out = &outcomes[pl.leg];
        let qty = q.legs[pl.leg].quantity;
        let j = out.instruments.iter().position(|i| {
            i.key.platform == pl.quote.platform && i.key.native_id == pl.quote.native_id
        });
        let Some(j) = j else {
            reasons.push(format!(
                "leg {}: chosen instrument missing from its comparison",
                pl.leg
            ));
            continue;
        };
        let v = VenueInput {
            instrument: &out.instruments[j],
            book: &out.books[j],
            fees: &out.fees[j],
            budget: None,
            quality_penalty_bps: Decimal::ZERO,
        };
        match price_leg_side(&v, qty, pl.side, tol) {
            Some(l) if l.qty_underlying >= qty => {
                legs.push(l);
                used.push(out.instruments[j].clone());
            }
            _ => reasons.push(format!(
                "leg {}: cannot be priced for the full size on {}",
                pl.leg, pl.quote.platform
            )),
        }
    }
    let same_expiry = req
        .strategy
        .legs
        .windows(2)
        .all(|w| w[0].expiry == w[1].expiry);
    let max_loss = if same_expiry && legs.len() == req.strategy.legs.len() {
        let payoff: Vec<oa_core::strategy::PayoffLeg> = req
            .strategy
            .legs
            .iter()
            .zip(q.legs.iter())
            .map(|(l, d)| oa_core::strategy::PayoffLeg {
                option_kind: l.option_kind,
                strike: l.strike,
                side: l.side,
                qty_underlying: d.quantity,
            })
            .collect();
        oa_core::strategy::max_loss(&payoff, plan.net_cost)
    } else {
        None
    };
    if crate::geo::require_user_country() && req.strategy.user_country.is_none() {
        reasons.push("USER_COUNTRY_REQUIRED: declare user_country (ISO alpha-2) so restricted venues are excluded".into());
    }
    if max_loss.is_none() && !allow_unbounded {
        reasons.push(if same_expiry {
            "UNBOUNDED loss (net short calls): refused unless allow_naked_sells is enabled".into()
        } else {
            "max loss not computable (legs with different expiries): refused unless allow_naked_sells is enabled".into()
        });
    }
    // CallPut opens only 2-leg DEBIT spreads in one transaction (single options revert on-chain).
    let cp: Vec<&Leg> = legs
        .iter()
        .filter(|l| l.instrument.platform == PlatformId::Callput)
        .collect();
    if !cp.is_empty() {
        let shape = match cp.as_slice() {
            [a, b] => {
                let o = |l: &Leg| oa_adapters::exec::ExecOrder {
                    native_id: l.instrument.native_id.clone(),
                    venue_instrument_id: None,
                    venue_meta: serde_json::Value::Null,
                    max_fee_per_unit: None,
                    side: l.side,
                    qty_native: l.qty_native,
                    limit_price_native: l.limit_price_native,
                    salt: 0,
                    timestamp: 0,
                    time_in_force: oa_adapters::exec::TimeInForce::Ioc,
                };
                let (buy, sell) = if a.side == Side::Buy {
                    (o(a), o(b))
                } else {
                    (o(b), o(a))
                };
                oa_adapters::callput_user::check_spread(&buy, &sell).map_err(|e| e.to_string())
            }
            _ => Err("needs exactly 2 legs on CallPut".to_string()),
        };
        if let Err(e) = shape {
            reasons.push(format!("CALLPUT_SPREADS_ONLY: {e}"));
        }
    }
    let platforms: std::collections::BTreeSet<_> =
        legs.iter().map(|l| l.instrument.platform).collect();
    let net: Decimal = legs
        .iter()
        .map(|l| match l.side {
            Side::Buy => l.expected_total,
            Side::Sell => -l.expected_total,
        })
        .sum();
    let rp = RoutePlan {
        kind: if platforms.len() <= 1 {
            RouteKind::Single
        } else {
            RouteKind::Split
        },
        legs,
        requested_underlying: req.strategy.quantity,
        covered_underlying: req.strategy.quantity,
        fully_covered: reasons.iter().all(|r| !r.starts_with("leg ")),
        expected_total: net,
        ranking_total: net,
        best_single: None,
        estimated_saving_vs_best_single: None,
        reason: plan.reason.clone(),
    };
    let hash = crate::engine::route_hash(&rp, expires_at, engine.mode);
    let executable = reasons.is_empty();
    let stored = crate::execution::StoredRoute {
        id: route_id,
        created_at: now,
        expires_at,
        route_hash: hash.clone(),
        mode: engine.mode,
        plan: rp.clone(),
        instruments: crate::engine::instrument_refs(&used),
        executable,
        not_executable_reasons: reasons.clone(),
        strategy: Some(crate::execution::StrategyRisk {
            max_loss,
            allow_unbounded,
            index_price: q.index_price,
            unwind_cost_cap_bps: unwind_cap,
        }),
    };
    engine
        .store
        .save_route(&stored)
        .await
        .map_err(|e| EngineError::Persistence(e.to_string()))?;
    Ok(StrategyRoute {
        route_id,
        route_hash: hash,
        expires_at,
        executable,
        not_executable_reasons: reasons,
        route: Some(rp),
        max_loss,
        unwind_cost_cap_bps: unwind_cap,
        quote: q,
    })
}
