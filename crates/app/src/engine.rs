//! The ONE engine behind the HTTP API and the MCP server: search, compare, explain,
//! route preview and simulation. Financial decisions are delegated to `oa_core`.

use chrono::{DateTime, Utc};
use oa_adapters::registry::{self, PlatformInfo};
use oa_adapters::{Connector, Implemented};
use oa_core::Decimal;
use oa_core::comparability::{self, ComparabilityReport, ReasonCode, Verdict};
use oa_core::cost::{self, CostLine, EstimateInput, FreshnessPolicy, Ineligibility, QuoteEstimate};
use oa_core::domain::*;
use oa_core::fees::{FeeAmount, FeeFormula, FeeSchedule};
use oa_core::risk::{self, OrderIntent, RiskLimits, RiskViolation};
use rust_decimal_macros::dec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::metrics::Metrics;
use crate::store::Store;
use crate::venue::{VenueError, VenueHandle, VenueHealth};

pub const ENGINE_VERSION: &str = concat!("oa-app/", env!("CARGO_PKG_VERSION"));
pub const WORDING: &str = "Best ESTIMATED cost among the covered and eligible destinations, \
    from indicative public data. Not a guaranteed price and not best execution across the market.";
/// Underlyings served (OA_UNDERLYINGS, comma separated; default BTC,ETH,SOL,XRP,HYPE). Venues
/// that do not list one simply return no instrument for it.
pub fn supported_underlyings() -> &'static [&'static str] {
    static U: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    U.get_or_init(|| {
        std::env::var("OA_UNDERLYINGS")
            .unwrap_or_else(|_| "BTC,ETH,SOL,XRP,HYPE".into())
            .split(',')
            .map(|x| x.trim().to_uppercase())
            .filter(|x| {
                !x.is_empty() && x.len() <= 10 && x.chars().all(|c| c.is_ascii_alphanumeric())
            })
            .map(|x| &*Box::leak(x.into_boxed_str()))
            .collect()
    })
}
const MAX_QUANTITY: Decimal = dec!(10000);
const CATALOG_WAIT: Duration = Duration::from_secs(20);
pub(crate) const BOOK_WAIT: Duration = Duration::from_secs(12);
/// How long a venue that missed the latency budget is not awaited (display comparisons only).
const SLOW_MEMORY: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------- executable routing

/// Split policy as accepted by the API (all optional; defaults = no split).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SplitPolicyInput {
    #[serde(default)]
    pub allow_split: bool,
    /// 1..=3 (default 2 when split is allowed).
    pub max_legs: Option<usize>,
    /// Minimum absolute saving vs the best single venue for a split (default 1, premium currency).
    pub min_saving_abs: Option<Decimal>,
    /// Minimum saving in bps of the single-venue total (default 10).
    pub min_saving_bps: Option<Decimal>,
    /// Minimum leg size in underlying units (default: venue minimums only).
    pub min_leg_underlying: Option<Decimal>,
    /// IOC limit tolerance above the worst consumed level, bps (default 50, max 500).
    pub limit_tolerance_bps: Option<Decimal>,
}

impl SplitPolicyInput {
    fn to_policy(&self) -> Result<oa_core::routing::SplitPolicy, EngineError> {
        let d = oa_core::routing::SplitPolicy::default();
        let p = oa_core::routing::SplitPolicy {
            allow_split: self.allow_split,
            max_legs: self.max_legs.unwrap_or(d.max_legs),
            min_saving_abs: self.min_saving_abs.unwrap_or(d.min_saving_abs),
            min_saving_bps: self.min_saving_bps.unwrap_or(d.min_saving_bps),
            min_leg_underlying: self.min_leg_underlying.unwrap_or(d.min_leg_underlying),
            limit_tolerance_bps: self.limit_tolerance_bps.unwrap_or(d.limit_tolerance_bps),
        };
        if !(1..=3).contains(&p.max_legs) {
            return Err(EngineError::Invalid("split.max_legs must be 1..=3".into()));
        }
        if p.limit_tolerance_bps < Decimal::ZERO || p.limit_tolerance_bps > dec!(500) {
            return Err(EngineError::Invalid(
                "split.limit_tolerance_bps must be 0..=500".into(),
            ));
        }
        if p.min_saving_abs < Decimal::ZERO
            || p.min_saving_bps < Decimal::ZERO
            || p.min_leg_underlying < Decimal::ZERO
        {
            return Err(EngineError::Invalid("split thresholds must be >= 0".into()));
        }
        Ok(p)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RouteQuoteRequest {
    #[serde(flatten)]
    pub compare: CompareRequest,
    #[serde(default)]
    pub split: SplitPolicyInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RouteCandidate {
    pub instrument: InstrumentKey,
    pub routable: bool,
    /// Why the venue is not used for routing (empty when routable).
    pub reasons: Vec<String>,
    pub executable_here: bool,
    pub quality_penalty_bps: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RouteQuote {
    pub route_id: Uuid,
    /// Bind the execution to exactly this plan: any change needs a new quote.
    pub route_hash: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub mode: TradingMode,
    pub policy: oa_core::routing::SplitPolicy,
    pub plan: Option<oa_core::routing::RoutePlan>,
    pub no_plan_reason: Option<String>,
    pub candidates: Vec<RouteCandidate>,
    /// Real venue balances used as budgets (TESTNET), or the error per venue.
    #[schemars(with = "serde_json::Value")]
    pub balances: serde_json::Value,
    pub executable: bool,
    pub not_executable_reasons: Vec<String>,
    pub compare: CompareResponse,
    pub notes: Vec<String>,
}

pub const ROUTE_QUOTE_TTL_SECS: i64 = 30;

fn json_balance(b: &oa_adapters::exec::VenueBalance) -> serde_json::Value {
    serde_json::json!({"available": b.available, "equity": b.equity, "currency": b.currency, "source": "venue API (authenticated)"})
}

impl Engine {
    /// Real balances of the execution venues (TESTNET); empty object otherwise.
    pub async fn balances(&self) -> serde_json::Value {
        let Some(x) = self.exec.get() else {
            return serde_json::json!({"mode": self.mode, "balances": {}, "note": "no execution venue in this mode"});
        };
        let b: BTreeMap<&str, serde_json::Value> = x
            .balances()
            .await
            .iter()
            .map(|(p, r)| {
                (
                    p.as_str(),
                    r.as_ref()
                        .map(json_balance)
                        .unwrap_or_else(|e| serde_json::json!({"error": e})),
                )
            })
            .collect();
        serde_json::json!({"mode": self.mode, "balances": b})
    }
}

/// Key of a route's instrument metadata. Venues share native names (Aevo, Deribit, CallPut and
/// Paradex all use `ETH-16OCT26-2600-C`): the platform is part of the key, or one venue's signing
/// data would be used for another venue's order.
pub(crate) fn instrument_ref_key(platform: PlatformId, native_id: &str) -> String {
    format!("{}:{native_id}", platform.as_str())
}

/// Execution metadata of every instrument, frozen with a route (venue ids, multipliers, signing data).
pub(crate) fn instrument_refs(
    instruments: &[Instrument],
) -> BTreeMap<String, crate::execution::InstrumentRef> {
    instruments
        .iter()
        .map(|i| {
            let id = i
                .native
                .get("instrument_id")
                .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_u64()));
            // The venue's own instrument data, frozen with the route: executors read what they
            // need from it (Derive: base_asset_address/sub_id; USDT venues: fx rate; OKX: contract size).
            let mut meta = i.native.clone();
            if let Some(o) = meta.as_object_mut() {
                o.insert(
                    "contract_multiplier".into(),
                    serde_json::json!(i.contract_multiplier.to_string()),
                );
                o.insert(
                    "tick_size".into(),
                    serde_json::json!(i.tick_size.map(|t| t.to_string())),
                );
                o.insert(
                    "qty_step".into(),
                    serde_json::json!(i.amount_step.map(|t| t.to_string())),
                );
                o.insert(
                    "min_amount".into(),
                    serde_json::json!(i.min_amount.map(|t| t.to_string())),
                );
                if i.key.platform == PlatformId::Deribit {
                    o.insert(
                        "min_trade_amount".into(),
                        serde_json::json!(i.min_amount.map(|t| t.to_string())),
                    );
                }
            }
            (
                instrument_ref_key(i.key.platform, &i.key.native_id),
                crate::execution::InstrumentRef {
                    venue_instrument_id: id,
                    contract_multiplier: i.contract_multiplier,
                    venue_meta: meta,
                },
            )
        })
        .collect()
}

pub(crate) fn route_hash(
    plan: &oa_core::routing::RoutePlan,
    expires_at: DateTime<Utc>,
    mode: TradingMode,
) -> String {
    use sha2::{Digest, Sha256};
    let mut s = format!("route-v1|{mode:?}|{}", expires_at.timestamp_millis());
    for l in &plan.legs {
        s.push_str(&format!(
            "|{}|{}|{}|{}|{}",
            l.instrument.platform,
            l.instrument.network,
            l.instrument.native_id,
            l.qty_native.normalize(),
            l.limit_price_native.normalize()
        ));
    }
    hex::encode(Sha256::digest(s.as_bytes()))
}

// ---------------------------------------------------------------- errors

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("service busy: {0}")]
    Busy(String),
    #[error("persistence: {0}")]
    Persistence(String),
    /// A venue could not answer (catalog/book unavailable): 502, not a client error nor overload.
    #[error("upstream unavailable: {0}")]
    Upstream(String),
}

// ---------------------------------------------------------------- platforms

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlatformView {
    #[serde(flatten)]
    pub info: PlatformInfoView,
    /// Capabilities OUR connector implements (empty when not integrated).
    pub implemented: Vec<Capability>,
    /// Live collection health (only for integrated platforms).
    pub health: Option<VenueHealth>,
    /// Connector diagnostics, e.g. WebSocket stream stats (connects, gaps, invalidations).
    #[schemars(with = "Option<serde_json::Value>")]
    pub diagnostics: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlatformInfoView {
    pub id: PlatformId,
    pub display_name: String,
    pub aliases: Vec<String>,
    pub families: Vec<ProductFamily>,
    pub networks: Vec<String>,
    pub operational_status: OperationalStatus,
    pub integration_level: IntegrationLevel,
    pub capabilities: BTreeMap<Capability, CapabilityStatus>,
    pub official_sources: Vec<String>,
    pub verified_on: String,
    pub notes: String,
}

impl From<PlatformInfo> for PlatformInfoView {
    fn from(p: PlatformInfo) -> Self {
        Self {
            id: p.id,
            display_name: p.display_name.into(),
            aliases: p.aliases.iter().map(|s| s.to_string()).collect(),
            families: p.families.to_vec(),
            networks: p.networks.iter().map(|s| s.to_string()).collect(),
            operational_status: p.operational_status,
            integration_level: p.integration_level,
            capabilities: p.capabilities.into_iter().collect(),
            official_sources: p.official_sources.iter().map(|s| s.to_string()).collect(),
            verified_on: p.verified_on.into(),
            notes: p.notes.into(),
        }
    }
}

// ---------------------------------------------------------------- search

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchQuery {
    /// "BTC" or "ETH".
    pub underlying: String,
    pub option_kind: Option<OptionKind>,
    /// Exact expiry instant (UTC, RFC 3339).
    pub expiry: Option<DateTime<Utc>>,
    /// `dated_option` (default) or `binary`.
    #[serde(default)]
    pub family: Option<ProductFamily>,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Listing {
    pub key: InstrumentKey,
    pub settlement_index: Option<String>,
    pub settlement_window: Option<String>,
}

/// Contracts sharing (kind, strike, expiry) across platforms. Grouping is NOT a
/// comparability verdict: see `/v1/quotes/compare` for that.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ContractGroup {
    pub option_kind: OptionKind,
    pub strike: Decimal,
    pub expiry: DateTime<Utc>,
    pub listings: Vec<Listing>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExpiryInfo {
    pub expiry: DateTime<Utc>,
    pub platforms: Vec<PlatformId>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CatalogStatus {
    pub platform: PlatformId,
    pub ok: bool,
    pub instruments: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchResponse {
    pub underlying: String,
    pub family: ProductFamily,
    pub expiries: Vec<ExpiryInfo>,
    pub total_groups: usize,
    pub offset: usize,
    pub limit: usize,
    pub groups: Vec<ContractGroup>,
    pub catalogs: Vec<CatalogStatus>,
}

// ---------------------------------------------------------------- compare

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompareRequest {
    pub underlying: String,
    pub option_kind: OptionKind,
    pub strike: Decimal,
    /// Exact expiry instant (UTC).
    pub expiry: DateTime<Utc>,
    pub side: Side,
    /// Quantity in UNDERLYING units (e.g. 0.5 = options on 0.5 BTC).
    pub quantity: Decimal,
    /// Contractual differences the user explicitly accepts for cross-platform ranking.
    /// Empty by default: nothing is accepted implicitly.
    #[serde(default)]
    pub accepted_differences: Vec<ReasonCode>,
    /// Platform whose contract defines the reference terms. Default: the baseline if set,
    /// else the first integrated platform listing the contract.
    pub reference_platform: Option<PlatformId>,
    /// Where the user already holds funds; used for "estimated saving vs baseline".
    pub baseline_platform: Option<PlatformId>,
    /// Max data age in milliseconds (1000..=60000). Default 10000.
    pub max_age_ms: Option<u64>,
    /// Product family compared: `dated_option` (default) or `binary` (prediction-market digitals;
    /// `quantity` is then a payout amount, `option_kind` call = above / put = below the strike).
    #[serde(default)]
    pub family: Option<ProductFamily>,
    /// Max time (ms, 50..=15000) to wait for venue books that are not already cached. Venues still
    /// fetching after it are reported as SLOW_VENUE (their fetch continues; the next request includes
    /// them). Default: server config OA_LATENCY_BUDGET_MS (220).
    #[serde(default)]
    pub latency_budget_ms: Option<u64>,
    /// Explicitly DECLARED simulation balances per platform (premium currency). Never
    /// presented as real account data.
    #[serde(default)]
    pub simulated_balances: BTreeMap<PlatformId, Decimal>,
    /// End user's country (ISO 3166-1 alpha-2), declared by the integrator. Venues whose terms
    /// exclude it are reported as GEO_RESTRICTED and never compared or routed.
    #[serde(default)]
    pub user_country: Option<String>,
    /// Routing only: restrict execution routes to these venues (e.g. where the end user has an
    /// account). Empty = every venue. Comparisons still show all venues.
    #[serde(default)]
    pub venues: Vec<PlatformId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FundingStatus {
    /// No account data: we cannot say whether the route is funded.
    Unverified,
    SimulatedSufficient {
        declared_balance: Decimal,
    },
    SimulatedInsufficient {
        declared_balance: Decimal,
        required: Decimal,
    },
    /// No complete cost: funding cannot be evaluated.
    NotEvaluable,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContractTerms {
    pub key: InstrumentKey,
    pub family: ProductFamily,
    pub underlying: String,
    pub option_kind: Option<OptionKind>,
    pub strike: Option<Decimal>,
    pub expiry: Option<DateTime<Utc>>,
    pub exercise_style: Option<ExerciseStyle>,
    pub contract_multiplier: Decimal,
    pub premium_currency: String,
    pub collateral_assets: Vec<String>,
    pub settlement_asset: Option<String>,
    pub settlement_mechanism: Option<SettlementMechanism>,
    pub settlement_index: Option<String>,
    pub settlement_window: Option<String>,
    pub tick_size: Option<Decimal>,
    pub min_amount: Option<Decimal>,
    pub amount_step: Option<Decimal>,
    pub spec_version: String,
}

impl From<&Instrument> for ContractTerms {
    fn from(i: &Instrument) -> Self {
        Self {
            key: i.key.clone(),
            family: i.family,
            underlying: i.underlying.clone(),
            option_kind: i.option_kind,
            strike: i.strike,
            expiry: i.expiry,
            exercise_style: i.exercise_style,
            contract_multiplier: i.contract_multiplier,
            premium_currency: i.premium_currency.clone(),
            collateral_assets: i.collateral_assets.clone(),
            settlement_asset: i.settlement_asset.clone(),
            settlement_mechanism: i.settlement_mechanism,
            settlement_index: i.settlement_index.clone(),
            settlement_window: i.settlement_window.clone(),
            tick_size: i.tick_size,
            min_amount: i.min_amount,
            amount_step: i.amount_step,
            spec_version: i.spec_version.clone(),
        }
    }
}

impl From<&ContractTerms> for Instrument {
    fn from(t: &ContractTerms) -> Self {
        Self {
            key: t.key.clone(),
            family: t.family,
            underlying: t.underlying.clone(),
            option_kind: t.option_kind,
            strike: t.strike,
            expiry: t.expiry,
            exercise_style: t.exercise_style,
            contract_multiplier: t.contract_multiplier,
            premium_currency: t.premium_currency.clone(),
            collateral_assets: t.collateral_assets.clone(),
            settlement_asset: t.settlement_asset.clone(),
            settlement_mechanism: t.settlement_mechanism,
            settlement_index: t.settlement_index.clone(),
            settlement_window: t.settlement_window.clone(),
            tick_size: t.tick_size,
            min_amount: t.min_amount,
            amount_step: t.amount_step,
            is_active: true,
            spec_version: t.spec_version.clone(),
            native: serde_json::Value::Null,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuoteView {
    pub estimate: QuoteEstimate,
    pub terms: ContractTerms,
    pub funding: FundingStatus,
    pub fee_notes: String,
    /// True when this platform's contract defines the reference terms.
    pub is_reference: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VenueIssue {
    pub platform: PlatformId,
    /// NO_LISTING | CATALOG_UNAVAILABLE | BOOK_UNAVAILABLE | GEO_RESTRICTED | SLOW_VENUE
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PairComparability {
    pub a: InstrumentKey,
    pub b: InstrumentKey,
    pub report: ComparabilityReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SavingEstimate {
    pub baseline: PlatformId,
    pub amount: Decimal,
    pub currency: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompareResponse {
    pub request_id: Uuid,
    pub computed_at: DateTime<Utc>,
    pub mode: TradingMode,
    pub wording: String,
    pub request: CompareRequest,
    pub reference: Option<InstrumentKey>,
    pub reference_reason: String,
    /// Eligible, fully covered, all costs known — ranked best first.
    pub ranked: Vec<QuoteView>,
    /// Comparable but incomplete (partial depth or unknown costs): indicative only, not ranked.
    pub incomplete: Vec<QuoteView>,
    /// Not eligible (stale, not comparable, differences not accepted, unhealthy...).
    pub ineligible: Vec<QuoteView>,
    /// Platforms that could not produce a quote, with the reason.
    pub unavailable: Vec<VenueIssue>,
    /// Pairwise comparability of all candidate contracts.
    pub comparability: Vec<PairComparability>,
    pub estimated_saving_vs_baseline: Option<SavingEstimate>,
    /// When at least one otherwise-comparable platform cannot cover the requested quantity:
    /// the largest quantity EVERY such platform can cover (rounded down to the strictest step,
    /// above every minimum). A suggestion only: the user must re-run the comparison with it.
    pub common_quantity: Option<CommonQuantity>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CommonQuantity {
    pub quantity: Decimal,
    pub platforms: Vec<PlatformId>,
    pub explanation: String,
}

/// Internal: what a comparison used, kept for simulation records and replay.
#[derive(Clone)]
pub struct CompareOutcome {
    pub response: CompareResponse,
    pub instruments: Vec<Instrument>,
    pub books: Vec<BookSnapshot>,
    pub fees: Vec<FeeSchedule>,
}

// ---------------------------------------------------------------- route & simulation

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Route {
    pub destination: InstrumentKey,
    pub side: Side,
    pub quantity_underlying: Decimal,
    pub quantity_native: Decimal,
    pub expected_avg_price_per_underlying: Decimal,
    /// Worst acceptable premium per underlying unit = expected avg x (1 ± tolerance).
    pub limit_price_per_underlying: Decimal,
    pub price_tolerance_bps: u32,
    pub estimated_total: Decimal,
    pub fee: FeeAmount,
    pub currency: String,
    pub funding: FundingStatus,
    pub accepted_differences_in_scope: Vec<ReasonCode>,
    pub steps: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RoutePreview {
    pub compare: CompareResponse,
    pub route: Option<Route>,
    pub no_route_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SimulateRequest {
    #[serde(flatten)]
    pub compare: CompareRequest,
    /// Price tolerance for the limit price, in basis points (0..=1000). Default 50.
    pub price_tolerance_bps: Option<u32>,
    /// Strategy tag (risk limits apply per strategy). Default "manual".
    pub strategy: Option<String>,
    /// The quote the user saw before asking for the simulation. When given for the chosen
    /// destination, the limit price is derived from THIS price (as a real order would be), and a
    /// fresh book that moved beyond it makes the simulation REJECTED_PRICE_MOVED.
    pub quoted: Option<QuotedReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuotedReference {
    pub platform: PlatformId,
    pub avg_price_per_underlying: Decimal,
    pub computed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PriceDrift {
    pub quoted_avg: Decimal,
    pub fresh_avg: Decimal,
    /// (fresh - quoted) / quoted, in basis points; positive = worse for a buyer.
    pub drift_bps: Decimal,
    pub quote_age_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScenarioOutcome {
    Fill,
    PartialFill,
    LimitExceeded,
    NoFill,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScenarioResult {
    pub name: String,
    pub description: String,
    pub outcome: ScenarioOutcome,
    pub result: SimResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SimStatus {
    SimulatedFill,
    SimulatedPartialFill,
    NotSimulatedNoRoute,
    RejectedByRiskChecks,
    /// The fresh book is beyond the limit derived from the quote the user saw.
    RejectedPriceMoved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SimResult {
    pub filled_underlying: Decimal,
    pub filled_native: Decimal,
    pub avg_price_per_underlying: Option<Decimal>,
    pub premium: Decimal,
    pub fee: FeeAmount,
    pub total: Option<Decimal>,
    pub within_limit_price: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SimulationRecord {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    /// Instant used as "now" for freshness; replay reuses it.
    pub evaluated_at: DateTime<Utc>,
    pub engine_version: String,
    pub mode: TradingMode,
    pub status: SimStatus,
    pub request: SimulateRequest,
    pub accepted_differences: Vec<ReasonCode>,
    pub route: Option<Route>,
    pub result: Option<SimResult>,
    pub risk_violations: Vec<RiskViolation>,
    pub estimated_saving_vs_baseline: Option<SavingEstimate>,
    pub compare: CompareResponse,
    /// Exact inputs used (for deterministic replay).
    pub instruments_used: Vec<ContractTerms>,
    pub books_used: Vec<BookSnapshot>,
    pub fees_used: Vec<FeeSchedule>,
    pub strategy: String,
    /// Simulated session usage BEFORE this simulation (risk caps are checked against it).
    pub session_usage_before: risk::SessionUsage,
    pub price_drift: Option<PriceDrift>,
    /// Stress scenarios computed on the same recorded book (no network).
    pub scenarios: Vec<ScenarioResult>,
    pub assumptions: Vec<String>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ExplainRequest {
    pub a: InstrumentKey,
    pub b: InstrumentKey,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExplainResponse {
    pub a: ContractTerms,
    pub b: ContractTerms,
    pub report: ComparabilityReport,
    pub route_eligible_without_acceptance: bool,
    pub codes_to_accept_for_economic_comparison: Vec<ReasonCode>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Readiness {
    pub ready: bool,
    pub database: String,
    pub venues: Vec<VenueHealth>,
    pub mode: TradingMode,
}

// ---------------------------------------------------------------- engine

pub struct Engine {
    venues: Vec<VenueHandle>,
    pub store: Arc<dyn Store>,
    pub metrics: Arc<Metrics>,
    pub mode: TradingMode,
    catalog_ttl: Duration,
    default_max_age: Duration,
    /// Default wait for uncached books per comparison (see `CompareRequest::latency_budget_ms`).
    latency_budget: Duration,
    /// Venues that missed the latency budget recently: not awaited again for SLOW_MEMORY (served from
    /// cache if fresh, else joining the next request) so one slow venue cannot slow every answer.
    recent_slow: std::sync::Mutex<HashMap<PlatformId, Instant>>,
    compare_permits: tokio::sync::Semaphore,
    /// Simulated session usage (process lifetime) for session / strategy caps.
    session: std::sync::Mutex<risk::SessionUsage>,
    /// TESTNET execution service (absent in READ_ONLY / SIMULATION).
    exec: std::sync::OnceLock<Arc<crate::execution::ExecutionService>>,
    /// Bounded queue to the batching observations writer (see `record_observations`).
    observations: std::sync::OnceLock<tokio::sync::mpsc::Sender<crate::store::ObservationBatch>>,
    observation_writer: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    observation_stop: Arc<tokio::sync::Notify>,
    /// Cached journal penalties (see `quality_penalties`).
    penalties: std::sync::Mutex<Option<(Instant, BTreeMap<PlatformId, Decimal>)>>,
}

const PENALTY_TTL: Duration = Duration::from_secs(30);

/// Comparisons waiting for the observations writer before new ones are dropped.
const OBS_QUEUE: usize = 2_048;
/// The writer flushes at least this often, or as soon as this many comparisons are queued.
const OBS_FLUSH_EVERY: Duration = Duration::from_millis(250);
const OBS_FLUSH_BATCHES: usize = 200;

/// Single writer: drains the queue into multi-row INSERTs (one statement for many comparisons instead
/// of one per row), so logging never competes with request-path queries for the connection pool.
async fn observation_writer(
    mut rx: tokio::sync::mpsc::Receiver<crate::store::ObservationBatch>,
    store: Arc<dyn Store>,
    metrics: Arc<Metrics>,
    stop: Arc<tokio::sync::Notify>,
) {
    let mut buf: Vec<crate::store::ObservationBatch> = Vec::new();
    let mut tick = tokio::time::interval(OBS_FLUSH_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let stopping = tokio::select! {
            m = rx.recv() => match m {
                Some(b) => { buf.push(b); if buf.len() < OBS_FLUSH_BATCHES { continue; } false }
                None => true,
            },
            _ = tick.tick() => false,
            _ = stop.notified() => true,
        };
        if stopping {
            while let Ok(b) = rx.try_recv() {
                buf.push(b);
            }
        }
        if !buf.is_empty() {
            let rows: usize = buf.iter().map(|b| b.quotes.len()).sum();
            let t = Instant::now();
            match store.record_quote_batches(&buf).await {
                Ok(()) => {
                    metrics.observe("observations_flush", t.elapsed());
                    metrics.incr_by("observations_written", rows as u64);
                }
                Err(e) => {
                    metrics.incr("persistence_errors");
                    tracing::warn!(error = %e, rows, "failed to record quote observations");
                }
            }
            buf.clear();
        }
        if stopping {
            return;
        }
    }
}

fn validate_underlying(u: &str) -> Result<String, EngineError> {
    let u = u.trim().to_uppercase();
    if supported_underlyings().contains(&u.as_str()) {
        Ok(u)
    } else {
        Err(EngineError::Invalid(format!(
            "underlying must be one of {:?}",
            supported_underlyings()
        )))
    }
}

fn unknown_fees(currency: &str, reason: String) -> FeeSchedule {
    FeeSchedule {
        taker: FeeFormula::Unknown {
            reason: reason.clone(),
        },
        maker: FeeFormula::Unknown {
            reason: reason.clone(),
        },
        currency: currency.into(),
        source_url: "unavailable".into(),
        verified_on: "-".into(),
        notes: reason,
    }
}

/// Risk limits by mode. TESTNET: testnet networks only and much lower caps (test funds).
pub fn risk_limits(mode: TradingMode) -> RiskLimits {
    let mut l = simulation_limits();
    if mode == TradingMode::Testnet {
        l.mode = TradingMode::Testnet;
        l.allowed_networks = [
            oa_adapters::derive::TESTNET_NETWORK.to_string(),
            oa_adapters::derive_exec::TESTNET_V3_NETWORK.to_string(),
            oa_adapters::aevo::TESTNET_NETWORK.to_string(),
            oa_adapters::paradex::TESTNET_NETWORK.to_string(),
            oa_adapters::bybit_exec::TESTNET_NETWORK.to_string(),
            oa_adapters::deribit_exec::TESTNET_NETWORK.to_string(),
            oa_adapters::okx_exec::DEMO_NETWORK.to_string(),
        ]
        .into();
        l.max_order_cost = dec!(1000);
        l.max_session_cost = dec!(5000);
        l.per_platform_max_order_cost = [
            ("derive".to_string(), dec!(1000)),
            ("aevo".to_string(), dec!(1000)),
        ]
        .into();
        l.strategies = [(
            "manual".to_string(),
            risk::StrategyLimits {
                max_order_cost: dec!(1000),
                max_session_cost: dec!(5000),
            },
        )]
        .into();
        l.max_loss_per_order = dec!(1000);
    }
    if mode == TradingMode::MainnetNonCustodial {
        l = mainnet_nc_limits(|k| std::env::var(k).ok());
    }
    l
}

/// Hard ceiling of any mainnet cap, whatever the environment says (premium currency, ~USD).
pub const MAINNET_HARD_MAX_ORDER: Decimal = dec!(25000);
pub const MAINNET_HARD_MAX_SESSION: Decimal = dec!(100000);

/// MAINNET NON-CUSTODIAL limits: venues with user-signed execution only, LOW default caps
/// (OA_MAINNET_MAX_ORDER_USD default 100, OA_MAINNET_MAX_SESSION_USD default 500), clamped to the
/// hard ceilings above. Selling stays disabled (no uncovered risk on real money).
pub fn mainnet_nc_limits(get: impl Fn(&str) -> Option<String>) -> RiskLimits {
    let cap = |k: &str, default: Decimal, hard: Decimal| {
        get(k)
            .and_then(|v| v.trim().parse::<Decimal>().ok())
            .filter(|v| *v > Decimal::ZERO)
            .unwrap_or(default)
            .min(hard)
    };
    let order = cap(
        "OA_MAINNET_MAX_ORDER_USD",
        dec!(100),
        MAINNET_HARD_MAX_ORDER,
    );
    let session = cap(
        "OA_MAINNET_MAX_SESSION_USD",
        dec!(500),
        MAINNET_HARD_MAX_SESSION,
    )
    .max(order);
    let venues: [(PlatformId, &str); 8] = [
        (PlatformId::Derive, oa_adapters::derive::NETWORK),
        (PlatformId::Aevo, oa_adapters::aevo::NETWORK),
        (PlatformId::Paradex, oa_adapters::paradex::NETWORK),
        (PlatformId::Thetanuts, oa_adapters::thetanuts::NETWORK),
        (PlatformId::Callput, oa_adapters::callput::NETWORK),
        (PlatformId::Hypercall, oa_adapters::hypercall::NETWORK),
        (PlatformId::Hyperliquid, oa_adapters::hyperliquid::NETWORK),
        (PlatformId::Limitless, oa_adapters::limitless::NETWORK),
    ];
    RiskLimits {
        mode: TradingMode::MainnetNonCustodial,
        kill_switch: true,
        allowed_platforms: venues.iter().map(|(p, _)| p.as_str().to_string()).collect(),
        allowed_networks: venues.iter().map(|(_, n)| n.to_string()).collect(),
        allowed_underlyings: supported_underlyings()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        allowed_families: [ProductFamily::DatedOption, ProductFamily::Binary].into(),
        max_order_cost: order,
        max_session_cost: session,
        max_price_deviation: dec!(0.5),
        allow_sell: false,
        per_platform_max_order_cost: venues
            .iter()
            .map(|(p, _)| (p.as_str().to_string(), order))
            .collect(),
        strategies: [(
            "manual".to_string(),
            risk::StrategyLimits {
                max_order_cost: order,
                max_session_cost: session,
            },
        )]
        .into(),
        max_loss_per_order: order,
    }
}

fn simulation_limits() -> RiskLimits {
    RiskLimits {
        mode: TradingMode::Simulation,
        kill_switch: true, // irrelevant for simulation (for_submission = false), stays ON
        allowed_platforms: [
            "derive".to_string(),
            "aevo".to_string(),
            "paradex".to_string(),
        ]
        .into(),
        allowed_networks: [
            oa_adapters::derive::NETWORK.to_string(),
            oa_adapters::aevo::NETWORK.to_string(),
            oa_adapters::paradex::NETWORK.to_string(),
        ]
        .into(),
        allowed_underlyings: supported_underlyings()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        allowed_families: [ProductFamily::DatedOption].into(),
        max_order_cost: dec!(1000000),
        max_session_cost: dec!(1000000),
        max_price_deviation: dec!(0.5),
        allow_sell: false,
        // Simulation defaults; a testnet profile will
        // load its own, much lower, values.
        per_platform_max_order_cost: [
            ("derive".to_string(), dec!(250000)),
            ("aevo".to_string(), dec!(250000)),
            ("paradex".to_string(), dec!(250000)),
        ]
        .into(),
        strategies: [(
            "manual".to_string(),
            risk::StrategyLimits {
                max_order_cost: dec!(250000),
                max_session_cost: dec!(1000000),
            },
        )]
        .into(),
        max_loss_per_order: dec!(250000),
    }
}

impl Engine {
    pub fn new(
        connectors: Vec<Arc<dyn Connector>>,
        store: Arc<dyn Store>,
        metrics: Arc<Metrics>,
        mode: TradingMode,
        catalog_ttl: Duration,
        default_max_age: Duration,
        max_concurrent_compares: usize,
    ) -> Self {
        let venues = connectors
            .into_iter()
            .map(|c| VenueHandle::spawn(c, metrics.clone()))
            .collect();
        Self {
            venues,
            store,
            metrics,
            mode,
            catalog_ttl,
            default_max_age,
            latency_budget: BOOK_WAIT,
            recent_slow: std::sync::Mutex::new(HashMap::new()),
            compare_permits: tokio::sync::Semaphore::new(max_concurrent_compares),
            session: std::sync::Mutex::new(risk::SessionUsage::default()),
            exec: std::sync::OnceLock::new(),
            observations: std::sync::OnceLock::new(),
            observation_writer: std::sync::Mutex::new(None),
            observation_stop: Arc::new(tokio::sync::Notify::new()),
            penalties: std::sync::Mutex::new(None),
        }
    }

    /// Queue one comparison's observations for the batching writer (started on first use, inside the
    /// runtime). Never blocks a request: when the queue is full (database slow or down) the batch is
    /// dropped and counted; observations are telemetry, the response does not depend on them.
    fn record_observations(&self, batch: crate::store::ObservationBatch) {
        let tx = self.observations.get_or_init(|| {
            let (tx, rx) = tokio::sync::mpsc::channel(OBS_QUEUE);
            let h = tokio::spawn(observation_writer(
                rx,
                self.store.clone(),
                self.metrics.clone(),
                self.observation_stop.clone(),
            ));
            *self.observation_writer.lock().expect("writer lock") = Some(h);
            tx
        });
        if tx.try_send(batch).is_err() {
            self.metrics.incr("observations_dropped");
        }
    }

    /// Default latency budget for comparisons (server config). Builder-style, before `Arc::new`.
    pub fn with_latency_budget(mut self, d: Duration) -> Self {
        self.latency_budget = d;
        self
    }

    /// Keeps every catalog younger than the TTL in the background, so a comparison never waits for
    /// a catalog refresh (they take 0.3-3.5 s on some venues).
    pub fn spawn_catalog_refresher(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let period = self.catalog_ttl.mul_f64(0.5).max(Duration::from_secs(10));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let Some(e) = me.upgrade() else { break };
                for v in &e.venues {
                    for u in supported_underlyings() {
                        let _ = v.catalog(u, period, CATALOG_WAIT).await;
                    }
                }
            }
        });
    }

    /// Streaming venues (Derive): subscribe in advance to the books most likely to be requested
    /// (expiry <= 30 days, strike within +-12 % of each expiry's median strike, <= 150 per underlying),
    /// so a first comparison finds the book already pushed instead of waiting ~1 s for the
    /// subscription. Re-touched every 4 minutes (the stream drops channels idle for 10 minutes).
    pub fn spawn_stream_prewarm(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            loop {
                let Some(e) = me.upgrade() else { break };
                for v in e.venues.iter().filter(|v| v.connector.streams_books()) {
                    for u in supported_underlyings()
                        .iter()
                        .filter(|u| matches!(**u, "BTC" | "ETH"))
                    {
                        let Ok(cat) = v.catalog(u, e.catalog_ttl, CATALOG_WAIT).await else {
                            continue;
                        };
                        v.connector.prewarm(&prewarm_set(&cat, Utc::now()));
                    }
                }
                drop(e);
                tokio::time::sleep(Duration::from_secs(240)).await;
            }
        });
    }

    /// Pre-load catalogs (best effort, errors visible in health).
    pub async fn warm_up(&self) {
        for v in &self.venues {
            for u in supported_underlyings() {
                let _ = v.catalog(u, self.catalog_ttl, CATALOG_WAIT).await;
            }
        }
    }

    pub async fn shutdown(&self) {
        for v in &self.venues {
            v.stop().await;
        }
        // Flush queued observations (bounded wait): closing the queue ends the writer after its last
        // batch.
        let writer = self.observation_writer.lock().expect("writer lock").take();
        if let Some(h) = writer {
            // `observations` keeps a sender; the writer exits on the shutdown signal below instead.
            self.observation_stop.notify_one();
            let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
        }
    }

    /// Time-boxed: the database ping and every venue's health are asked in parallel, each bounded. The
    /// database gates readiness only where it is load-bearing (execution modes journal every order);
    /// read-only serving does not need it (observations are queued and dropped when it is down).
    pub async fn readiness(&self) -> Readiness {
        let ping = tokio::time::timeout(Duration::from_secs(1), self.store.ping());
        let healths = futures_util::future::join_all(self.venues.iter().map(|v| v.health()));
        let (ping, healths) = tokio::join!(ping, healths);
        let database = match ping {
            Ok(Ok(())) => format!("ok ({})", self.store.kind()),
            Ok(Err(e)) => format!("error: {e}"),
            Err(_) => "error: no answer within 1 s".to_string(),
        };
        let venues: Vec<VenueHealth> = healths.into_iter().flatten().collect();
        let any_catalog = venues
            .iter()
            .any(|h| h.catalogs.iter().any(|c| c.state == "READY"));
        let db_ok = database.starts_with("ok") || !self.mode.executes();
        Readiness {
            ready: db_ok && any_catalog,
            database,
            venues,
            mode: self.mode,
        }
    }

    pub async fn platforms(&self) -> Vec<PlatformView> {
        let mut out = Vec::new();
        let healths: HashMap<PlatformId, VenueHealth> =
            futures_util::future::join_all(self.venues.iter().map(|v| v.health()))
                .await
                .into_iter()
                .flatten()
                .map(|h| (h.platform, h))
                .collect();
        for info in registry::all() {
            let venue = self.venues.iter().find(|v| v.platform() == info.id);
            let (implemented, health, diagnostics) = match venue {
                Some(v) => (
                    Capability::ALL
                        .into_iter()
                        .filter(|c| v.connector.implemented(*c) == Implemented::Yes)
                        .collect(),
                    healths.get(&info.id).cloned(),
                    v.connector.diagnostics(),
                ),
                None => (vec![], None, None),
            };
            out.push(PlatformView {
                info: info.into(),
                implemented,
                health,
                diagnostics,
            });
        }
        out
    }

    /// Strikes listed (dated options, any venue) for this underlying, kind and exact expiry.
    pub async fn listed_strikes(
        &self,
        underlying: &str,
        kind: OptionKind,
        expiry: DateTime<Utc>,
    ) -> Result<BTreeSet<Decimal>, EngineError> {
        let u = validate_underlying(underlying)?;
        let lists = futures_util::future::join_all(
            self.venues
                .iter()
                .map(|v| v.catalog(&u, self.catalog_ttl, CATALOG_WAIT)),
        )
        .await;
        Ok(lists
            .into_iter()
            .flatten()
            .flat_map(|l| {
                l.iter()
                    .filter(|i| {
                        i.family == ProductFamily::DatedOption
                            && i.option_kind == Some(kind)
                            && i.expiry == Some(expiry)
                    })
                    .filter_map(|i| i.strike)
                    .collect::<Vec<_>>()
            })
            .collect())
    }

    pub async fn search(&self, q: SearchQuery) -> Result<SearchResponse, EngineError> {
        let underlying = validate_underlying(&q.underlying)?;
        let limit = q.limit.unwrap_or(200).clamp(1, 1000);
        let mut catalogs = Vec::new();
        let mut groups: BTreeMap<(DateTime<Utc>, Decimal, u8), Vec<Listing>> = BTreeMap::new();
        let mut expiries: BTreeMap<DateTime<Utc>, BTreeSet<PlatformId>> = BTreeMap::new();
        let results = futures_util::future::join_all(self.venues.iter().map(|v| {
            let u = underlying.clone();
            async move {
                (
                    v.platform(),
                    v.catalog(&u, self.catalog_ttl, CATALOG_WAIT).await,
                )
            }
        }))
        .await;
        for (p, r) in results {
            match r {
                Ok(list) => {
                    catalogs.push(CatalogStatus {
                        platform: p,
                        ok: true,
                        instruments: list.len(),
                        error: None,
                    });
                    let fam = q.family.unwrap_or(ProductFamily::DatedOption);
                    for i in list.iter().filter(|i| i.family == fam) {
                        let (Some(kind), Some(strike), Some(expiry)) =
                            (i.option_kind, i.strike, i.expiry)
                        else {
                            continue;
                        };
                        expiries.entry(expiry).or_default().insert(p);
                        if q.option_kind.is_some_and(|k| k != kind)
                            || q.expiry.is_some_and(|e| e != expiry)
                        {
                            continue;
                        }
                        let k = (expiry, strike, matches!(kind, OptionKind::Put) as u8);
                        groups.entry(k).or_default().push(Listing {
                            key: i.key.clone(),
                            settlement_index: i.settlement_index.clone(),
                            settlement_window: i.settlement_window.clone(),
                        });
                    }
                }
                Err(e) => catalogs.push(CatalogStatus {
                    platform: p,
                    ok: false,
                    instruments: 0,
                    error: Some(e.to_string()),
                }),
            }
        }
        let total = groups.len();
        let groups = groups
            .into_iter()
            .skip(q.offset)
            .take(limit)
            .map(|((expiry, strike, put), listings)| ContractGroup {
                option_kind: if put == 1 {
                    OptionKind::Put
                } else {
                    OptionKind::Call
                },
                strike,
                expiry,
                listings,
            })
            .collect();
        Ok(SearchResponse {
            underlying,
            family: ProductFamily::DatedOption,
            expiries: expiries
                .into_iter()
                .map(|(expiry, ps)| ExpiryInfo {
                    expiry,
                    platforms: ps.into_iter().collect(),
                })
                .collect(),
            total_groups: total,
            offset: q.offset,
            limit,
            groups,
            catalogs,
        })
    }

    async fn find_instrument(&self, key: &InstrumentKey) -> Result<Instrument, EngineError> {
        let v = self
            .venues
            .iter()
            .find(|v| v.platform() == key.platform && v.connector.network() == key.network)
            .ok_or_else(|| {
                EngineError::NotFound(format!("{key}: platform/network not integrated"))
            })?;
        // The underlying named in the instrument id first (no catalog walk for the usual case), every
        // catalog asked concurrently; one unavailable catalog does not fail an instrument found in another.
        let named = key
            .native_id
            .split(['-', '_'])
            .next()
            .unwrap_or("")
            .trim_end_matches("USD")
            .to_uppercase();
        let mut unders: Vec<&str> = supported_underlyings().to_vec();
        unders.sort_by_key(|u| *u != named.as_str());
        let cats = futures_util::future::join_all(
            unders
                .iter()
                .map(|u| v.catalog(u, self.catalog_ttl, CATALOG_WAIT)),
        )
        .await;
        let mut failed = None;
        for c in cats {
            match c {
                Ok(cat) => {
                    if let Some(i) = cat.iter().find(|i| i.key == *key) {
                        return Ok(i.clone());
                    }
                }
                Err(e) => failed = Some(e),
            }
        }
        match failed {
            Some(e) => Err(EngineError::Upstream(format!("{}: {e}", key.platform))),
            None => Err(EngineError::NotFound(format!("{key}"))),
        }
    }

    pub async fn explain(&self, r: ExplainRequest) -> Result<ExplainResponse, EngineError> {
        let (a, b) = tokio::join!(self.find_instrument(&r.a), self.find_instrument(&r.b));
        let (a, b) = (a?, b?);
        let report = comparability::compare(&a, &b);
        let codes = match report.verdict {
            Verdict::EconomicallySimilarWithDifferences => report.reasons.clone(),
            _ => vec![],
        };
        Ok(ExplainResponse {
            a: (&a).into(),
            b: (&b).into(),
            route_eligible_without_acceptance: comparability::is_route_eligible(&report, &[]),
            codes_to_accept_for_economic_comparison: codes,
            report,
        })
    }

    fn validate_compare(&self, r: &mut CompareRequest) -> Result<Duration, EngineError> {
        r.underlying = validate_underlying(&r.underlying)?;
        if let Some(c) = &r.user_country {
            r.user_country = Some(crate::geo::normalize(c).map_err(EngineError::Invalid)?);
        }
        if r.quantity <= Decimal::ZERO || r.quantity > MAX_QUANTITY {
            return Err(EngineError::Invalid(format!(
                "quantity must be in (0, {MAX_QUANTITY}]"
            )));
        }
        if r.strike <= Decimal::ZERO {
            return Err(EngineError::Invalid("strike must be > 0".into()));
        }
        let max_age = r
            .max_age_ms
            .unwrap_or(self.default_max_age.as_millis() as u64);
        if !matches!(
            r.family.unwrap_or(ProductFamily::DatedOption),
            ProductFamily::DatedOption | ProductFamily::Binary
        ) {
            return Err(EngineError::Invalid(
                "family must be dated_option or binary".into(),
            ));
        }
        if let Some(b) = r.latency_budget_ms
            && !(50..=15_000).contains(&b)
        {
            return Err(EngineError::Invalid(
                "latency_budget_ms must be in [50, 15000]".into(),
            ));
        }
        if !(1000..=60_000).contains(&max_age) {
            return Err(EngineError::Invalid(
                "max_age_ms must be in [1000, 60000]".into(),
            ));
        }
        // Core mismatches/unknowns can never be "accepted".
        for c in &r.accepted_differences {
            let probe = ComparabilityReport {
                verdict: Verdict::EconomicallySimilarWithDifferences,
                reasons: vec![*c],
                explanation: String::new(),
            };
            if !comparability::is_route_eligible(&probe, &[*c]) || is_core_code(*c) {
                return Err(EngineError::Invalid(format!("{c:?} cannot be accepted")));
            }
        }
        for b in r.simulated_balances.values() {
            if *b < Decimal::ZERO {
                return Err(EngineError::Invalid(
                    "simulated balances must be >= 0".into(),
                ));
            }
        }
        Ok(Duration::from_millis(max_age))
    }

    pub async fn compare(&self, mut req: CompareRequest) -> Result<CompareOutcome, EngineError> {
        let max_age = self.validate_compare(&mut req)?;
        let family = req.family.unwrap_or(ProductFamily::DatedOption);
        // Short queue instead of an immediate 503: overload becomes latency first, then backpressure.
        let _permit =
            tokio::time::timeout(Duration::from_millis(250), self.compare_permits.acquire())
                .await
                .map_err(|_| EngineError::Busy("too many concurrent comparisons".into()))?
                .map_err(|_| EngineError::Busy("comparisons closed".into()))?;
        let t_total = Instant::now();
        let mut unavailable = Vec::new();

        // 1. Candidates: exact (kind, strike, expiry) per integrated platform.
        // 0. Country filter: a venue whose terms exclude the user's country is never queried.
        let geo = crate::geo::policy();
        let allowed: Vec<&VenueHandle> = self
            .venues
            .iter()
            .filter(|v| {
                let Some(c) = &req.user_country else {
                    return true;
                };
                match geo.restriction(v.platform(), c) {
                    Some(why) => {
                        unavailable.push(VenueIssue {
                            platform: v.platform(),
                            kind: "GEO_RESTRICTED".into(),
                            detail: why,
                        });
                        false
                    }
                    None => true,
                }
            })
            .collect();
        // A cold catalog may not hold the whole request past its latency budget: the fetch goes on in
        // the background and the venue is reported unavailable for this request only.
        let budget = req
            .latency_budget_ms
            .map(Duration::from_millis)
            .unwrap_or(self.latency_budget);
        let catalog_wait = budget.clamp(Duration::from_millis(50), CATALOG_WAIT);
        let catalogs = futures_util::future::join_all(allowed.into_iter().map(|v| {
            let u = req.underlying.clone();
            async move { (v, v.catalog(&u, self.catalog_ttl, catalog_wait).await) }
        }))
        .await;
        let mut candidates: Vec<(&VenueHandle, Instrument)> = Vec::new();
        for (v, r) in catalogs {
            match r {
                Err(e) => unavailable.push(VenueIssue {
                    platform: v.platform(),
                    kind: "CATALOG_UNAVAILABLE".into(),
                    detail: e.to_string(),
                }),
                Ok(list) => match list.iter().find(|i| {
                    i.family == family
                        && i.option_kind == Some(req.option_kind)
                        && i.strike == Some(req.strike)
                        && i.expiry == Some(req.expiry)
                }) {
                    Some(i) => candidates.push((v, i.clone())),
                    None => unavailable.push(VenueIssue {
                        platform: v.platform(),
                        kind: "NO_LISTING".into(),
                        detail: "no active contract with this kind/strike/expiry".into(),
                    }),
                },
            }
        }

        // 2. Reference contract.
        let wanted = req.reference_platform.or(req.baseline_platform);
        let (reference, reference_reason) = match wanted {
            Some(p) => match candidates.iter().find(|(v, _)| v.platform() == p) {
                Some((_, i)) => (
                    Some(i.key.clone()),
                    format!("requested reference platform {p}"),
                ),
                None => (
                    None,
                    format!("requested reference platform {p} has no listing"),
                ),
            },
            None => match candidates.first() {
                Some((_, i)) => (
                    Some(i.key.clone()),
                    format!(
                        "default: first integrated platform listing the contract ({})",
                        i.key.platform
                    ),
                ),
                None => (None, "no platform lists this contract".into()),
            },
        };
        let reference_inst = reference.as_ref().and_then(|k| {
            candidates
                .iter()
                .find(|(_, i)| &i.key == k)
                .map(|(_, i)| i.clone())
        });

        // 3. Books (concurrently, through the venue actors' caches), bounded by the latency budget:
        // cached books return at once; a venue still fetching after the budget is reported SLOW_VENUE.
        let book_wait = budget
            .saturating_sub(t_total.elapsed())
            .clamp(Duration::from_millis(30), BOOK_WAIT);
        let skip_slow = book_wait < BOOK_WAIT;
        let slow_now: std::collections::HashSet<PlatformId> = {
            let g = self.recent_slow.lock().expect("slow lock");
            g.iter()
                .filter(|(_, t)| t.elapsed() < SLOW_MEMORY)
                .map(|(p, _)| *p)
                .collect()
        };
        // No separate health round trip per venue: an open circuit already refuses the book (or serves
        // one that is still fresh), and every book's age is checked by the freshness policy below.
        let fetched = futures_util::future::join_all(candidates.iter().map(|(v, i)| {
            let wait = if skip_slow && slow_now.contains(&v.platform()) {
                Duration::from_millis(5)
            } else {
                book_wait
            };
            async move { (v, i, v.book(i, max_age, wait).await) }
        }))
        .await;

        // 4. Pure computation.
        let t_compute = Instant::now();
        let now = Utc::now();
        let freshness = FreshnessPolicy {
            max_age: chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::seconds(10)),
            ..FreshnessPolicy::default()
        };
        let mut views = Vec::new();
        let (mut instruments, mut books, mut fees_used) = (vec![], vec![], vec![]);
        for (v, inst, book) in fetched {
            let md = match book {
                Ok(md) => md,
                Err(VenueError::Timeout { waited_ms }) if book_wait < BOOK_WAIT => {
                    self.metrics.incr(format!("slow_venue.{}", v.platform()));
                    if !slow_now.contains(&v.platform()) {
                        self.recent_slow
                            .lock()
                            .expect("slow lock")
                            .insert(v.platform(), Instant::now());
                    }
                    unavailable.push(VenueIssue {
                        platform: v.platform(),
                        kind: "SLOW_VENUE".into(),
                        detail: format!("book not ready within the {waited_ms} ms latency budget; fetch continues, included in the next request"),
                    });
                    continue;
                }
                Err(e) => {
                    unavailable.push(VenueIssue {
                        platform: v.platform(),
                        kind: "BOOK_UNAVAILABLE".into(),
                        detail: e.to_string(),
                    });
                    continue;
                }
            };
            let fees = v
                .connector
                .get_fees(inst)
                .unwrap_or_else(|e| unknown_fees(&inst.premium_currency, e.to_string()));
            let comparability = match &reference_inst {
                Some(r) if r.key != inst.key => Some(comparability::compare(r, inst)),
                Some(_) => None,
                None => None,
            };
            let est = cost::estimate(EstimateInput {
                instrument: inst,
                book: &md.book,
                side: req.side,
                qty_underlying: req.quantity,
                fees: &fees,
                gas: gas_line(inst.key.platform),
                now,
                freshness,
                comparability,
                accepted_differences: &req.accepted_differences,
                platform_healthy: true,
            });
            self.metrics
                .observe_value("quote_age_at_compare_ms", est.age_ms as f64);
            views.push((est, inst.clone(), fees.notes.clone()));
            instruments.push(inst.clone());
            books.push(md.book.clone());
            fees_used.push(fees);
        }
        let response = assemble(
            &req,
            now,
            self.mode,
            reference,
            reference_reason,
            views,
            unavailable,
            &instruments,
        );
        self.metrics.observe("compare_compute", t_compute.elapsed());
        self.metrics.observe("compare_total", t_total.elapsed());
        self.metrics.incr("compare_requests");

        // 5. Persist observations without delaying the response; failures are counted.
        let all: Vec<QuoteEstimate> = response
            .ranked
            .iter()
            .chain(&response.incomplete)
            .chain(&response.ineligible)
            .map(|q| q.estimate.clone())
            .collect();
        let rid = response.request_id;
        self.record_observations(crate::store::ObservationBatch {
            request_id: rid,
            at: now,
            quotes: all,
        });
        Ok(CompareOutcome {
            response,
            instruments,
            books,
            fees: fees_used,
        })
    }

    pub async fn preview(
        &self,
        req: CompareRequest,
        tolerance_bps: u32,
    ) -> Result<(RoutePreview, CompareOutcome), EngineError> {
        if tolerance_bps > 1000 {
            return Err(EngineError::Invalid(
                "price_tolerance_bps must be <= 1000".into(),
            ));
        }
        let outcome = self.compare(req).await?;
        let preview = build_route(&outcome.response, &outcome.instruments, tolerance_bps);
        Ok((preview, outcome))
    }

    pub async fn simulate(&self, req: SimulateRequest) -> Result<SimulationRecord, EngineError> {
        let tol = req.price_tolerance_bps.unwrap_or(50);
        let strategy = req.strategy.clone().unwrap_or_else(|| "manual".into());
        if strategy.is_empty() || strategy.len() > 64 {
            return Err(EngineError::Invalid(
                "strategy must be 1..=64 characters".into(),
            ));
        }
        let (preview, outcome) = self.preview(req.compare.clone(), tol).await?;
        let session = self.session.lock().expect("session lock").clone();
        let record = build_simulation(
            Uuid::new_v4(),
            Utc::now(),
            self.mode,
            req,
            preview,
            &outcome,
            &session,
            &risk_limits(self.mode),
        );
        if let (SimStatus::SimulatedFill | SimStatus::SimulatedPartialFill, Some(total)) =
            (&record.status, record.result.as_ref().and_then(|r| r.total))
        {
            self.session
                .lock()
                .expect("session lock")
                .add(&record.strategy, total);
        }
        let rec_json =
            serde_json::to_value(&record).map_err(|e| EngineError::Persistence(e.to_string()))?;
        let req_json = serde_json::to_value(&record.request)
            .map_err(|e| EngineError::Persistence(e.to_string()))?;
        self.store
            .save_simulation(
                record.id,
                record.created_at,
                &format!("{:?}", record.status),
                record
                    .route
                    .as_ref()
                    .map(|r| r.destination.platform.as_str()),
                &req_json,
                &rec_json,
            )
            .await
            .map_err(|e| EngineError::Persistence(e.to_string()))?;
        let _ = self
            .store
            .audit(
                "simulate",
                Some(&record.id.to_string()),
                &serde_json::json!({"status": record.status}),
            )
            .await;
        self.metrics.incr("simulations");
        Ok(record)
    }

    /// Attach the execution service (TESTNET, or MAINNET_NONCUSTODIAL; same mode on both sides).
    pub fn attach_execution(
        &self,
        exec: Arc<crate::execution::ExecutionService>,
    ) -> Result<(), EngineError> {
        if !self.mode.executes() || exec.mode != self.mode {
            return Err(EngineError::Invalid(
                "execution can only be attached in TESTNET or MAINNET_NONCUSTODIAL mode".into(),
            ));
        }
        self.exec
            .set(exec)
            .map_err(|_| EngineError::Invalid("execution already attached".into()))
    }

    pub fn execution(&self) -> Option<&Arc<crate::execution::ExecutionService>> {
        self.exec.get()
    }

    /// Per-venue routing penalties from the execution journal, cached for PENALTY_TTL (they are
    /// statistics over many orders: recomputing them from up to 10k journal rows on every route quote
    /// cost a full table read). No execution service = no orders = no database read at all.
    async fn quality_penalties(&self) -> BTreeMap<PlatformId, Decimal> {
        if self.exec.get().is_none() {
            return BTreeMap::new();
        }
        if let Some((at, p)) = &*self.penalties.lock().expect("penalties lock")
            && at.elapsed() < PENALTY_TTL
        {
            return p.clone();
        }
        let p: BTreeMap<PlatformId, Decimal> = match self.store.list_orders(10_000).await {
            Ok(orders) => crate::execution::exec_stats(&orders)
                .into_iter()
                .map(|s| (s.platform, s.router_penalty_bps))
                .collect(),
            Err(_) => return BTreeMap::new(),
        };
        *self.penalties.lock().expect("penalties lock") = Some((Instant::now(), p.clone()));
        p
    }

    pub async fn route_quote(&self, req: RouteQuoteRequest) -> Result<RouteQuote, EngineError> {
        let policy = req.split.to_policy()?;
        if req.compare.side == Side::Sell {
            return Err(EngineError::Invalid(
                "only BUY routes are supported (no uncovered selling)".into(),
            ));
        }
        // An execution route must not miss a venue because it was slow: unless the caller asks for a
        // budget explicitly, wait for every venue (the fast 220 ms default is for display comparisons).
        let mut creq = req.compare.clone();
        creq.latency_budget_ms
            .get_or_insert(BOOK_WAIT.as_millis() as u64);
        let exec = self.exec.get();
        // The comparison, the journal penalties and the real balances are independent: asked at once.
        // TESTNET: the venue's REAL available balance is the budget (a declared simulated balance
        // can only lower it). Balance errors are reported, never treated as zero or infinite.
        let (outcome, penalties, real_balances) =
            tokio::join!(self.compare(creq), self.quality_penalties(), async {
                match exec {
                    Some(x) => x.balances().await,
                    None => BTreeMap::new(),
                }
            });
        let outcome = outcome?;
        let c = &outcome.response;
        let mut candidates = vec![];
        let mut routable_idx = vec![];
        for q in c.ranked.iter().chain(&c.incomplete).chain(&c.ineligible) {
            let e = &q.estimate;
            // Partial depth is fine for routing (the split can complete it); anything else is not.
            let blocking: Vec<String> = e
                .ineligible
                .iter()
                .filter(|r| **r != Ineligibility::PartialCoverage)
                .map(|r| format!("{r:?}"))
                .collect();
            let executable_here = exec.and_then(|x| x.executable_network(e.instrument.platform))
                == Some(e.instrument.network.as_str());
            let mut reasons = blocking.clone();
            if e.covered_native.is_zero() {
                reasons.push("NO_VISIBLE_DEPTH".into());
            }
            if !req.compare.venues.is_empty()
                && !req.compare.venues.contains(&e.instrument.platform)
            {
                reasons.push("EXCLUDED_BY_REQUEST (venues)".into());
            }
            if e.instrument.platform == PlatformId::Callput {
                // Verified 2026-10-09 by eth_call on Base mainnet: a single-leg createOpenPosition
                // reverts with StrategyNotAllowed(); only 2-leg spreads are accepted.
                reasons.push("CALLPUT_SPREADS_ONLY (single options are refused on-chain)".into());
            }
            if self.mode == TradingMode::Testnet && !executable_here {
                reasons.push("NO_TESTNET_EXECUTION_FOR_THIS_VENUE".into());
            }
            if self.mode == TradingMode::MainnetNonCustodial && !executable_here {
                reasons.push("NO_NONCUSTODIAL_EXECUTION_FOR_THIS_VENUE".into());
            }
            if executable_here {
                match real_balances.get(&e.instrument.platform) {
                    Some(Ok(b)) if b.available <= Decimal::ZERO => {
                        reasons.push("INSUFFICIENT_FUNDS (venue balance 0)".into())
                    }
                    Some(Err(err)) => reasons.push(format!("BALANCE_UNAVAILABLE ({err})")),
                    _ => {}
                }
            }
            let routable = reasons.is_empty();
            if routable
                && let Some(i) = outcome
                    .instruments
                    .iter()
                    .position(|i| i.key == e.instrument)
            {
                routable_idx.push(i);
            }
            candidates.push(RouteCandidate {
                instrument: e.instrument.clone(),
                routable,
                reasons,
                executable_here,
                quality_penalty_bps: penalties
                    .get(&e.instrument.platform)
                    .copied()
                    .unwrap_or_default(),
            });
        }
        for u in &c.unavailable {
            candidates.push(RouteCandidate {
                instrument: InstrumentKey {
                    platform: u.platform,
                    network: String::new(),
                    native_id: String::new(),
                },
                routable: false,
                reasons: vec![format!("{}: {}", u.kind, u.detail)],
                executable_here: false,
                quality_penalty_bps: Decimal::ZERO,
            });
        }
        let inputs: Vec<oa_core::routing::VenueInput> = routable_idx
            .iter()
            .map(|&i| oa_core::routing::VenueInput {
                instrument: &outcome.instruments[i],
                book: &outcome.books[i],
                fees: &outcome.fees[i],
                budget: {
                    let p = outcome.instruments[i].key.platform;
                    let declared = req.compare.simulated_balances.get(&p).copied();
                    let real = match real_balances.get(&p) {
                        Some(Ok(b)) => Some(b.available),
                        _ => None,
                    };
                    match (declared, real) {
                        (Some(d), Some(r)) => Some(d.min(r)),
                        (d, r) => d.or(r),
                    }
                },
                quality_penalty_bps: penalties
                    .get(&outcome.instruments[i].key.platform)
                    .copied()
                    .unwrap_or_default(),
            })
            .collect();
        let (plan, no_plan_reason) = if inputs.is_empty() {
            (None, Some("no routable venue (see candidates)".to_string()))
        } else {
            match oa_core::routing::plan(&inputs, req.compare.side, req.compare.quantity, &policy) {
                Ok(p) => (Some(p), None),
                Err(e) => (None, Some(e.to_string())),
            }
        };
        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(ROUTE_QUOTE_TTL_SECS);
        let mut not_exec = vec![];
        if !self.mode.executes() {
            not_exec.push(format!(
                "mode {:?}: execution exists only in TESTNET or MAINNET_NONCUSTODIAL",
                self.mode
            ));
        }
        match &plan {
            None => not_exec.push("no plan".into()),
            Some(p) if !p.fully_covered => {
                not_exec.push("plan does not cover the full quantity".into())
            }
            _ => {}
        }
        if let Some(x) = exec
            && x.kill_switch()
        {
            not_exec.push("kill switch ON".into());
        }
        if crate::geo::require_user_country() && req.compare.user_country.is_none() {
            not_exec.push("USER_COUNTRY_REQUIRED: declare user_country (ISO alpha-2) so restricted venues are excluded".into());
        }
        let route_id = Uuid::new_v4();
        let hash = plan
            .as_ref()
            .map(|p| route_hash(p, expires_at, self.mode))
            .unwrap_or_default();
        let instruments = instrument_refs(&outcome.instruments);
        let mut notes = vec![
            "Route quotes are single-use and expire after 30 s; execution re-checks risk limits and uses IOC limit orders (no resting order).".to_string(),
            "Legs are priced from visible depth; a split assumes the legs execute independently (not atomic across venues).".to_string(),
        ];
        if !policy.allow_split {
            notes.push(
                "Split disabled (default). Enable split to let the router combine venues.".into(),
            );
        }
        if let Some(p) = &plan {
            if let Some(sv) = p.estimated_saving_vs_best_single {
                notes.push(format!(
                    "Estimated saving vs best single venue: {} (estimate, not realised).",
                    sv.normalize()
                ));
            }
            let stored = crate::execution::StoredRoute {
                id: route_id,
                created_at: now,
                expires_at,
                route_hash: hash.clone(),
                mode: self.mode,
                plan: p.clone(),
                instruments,
                executable: not_exec.is_empty(),
                not_executable_reasons: not_exec.clone(),
                strategy: None,
            };
            self.store
                .save_route(&stored)
                .await
                .map_err(|e| EngineError::Persistence(e.to_string()))?;
        }
        self.metrics
            .incr(format!("route_quotes.{:?}", plan.as_ref().map(|p| p.kind)));
        Ok(RouteQuote {
            route_id,
            route_hash: hash,
            created_at: now,
            expires_at,
            mode: self.mode,
            policy,
            executable: not_exec.is_empty(),
            not_executable_reasons: not_exec,
            plan,
            no_plan_reason,
            candidates,
            balances: serde_json::to_value(
                real_balances
                    .iter()
                    .map(|(p, r)| {
                        (
                            p.as_str(),
                            r.as_ref()
                                .map(json_balance)
                                .unwrap_or_else(|e| serde_json::json!({"error": e})),
                        )
                    })
                    .collect::<BTreeMap<_, _>>(),
            )
            .unwrap_or_default(),
            compare: outcome.response,
            notes,
        })
    }

    pub async fn execute_testnet(
        &self,
        route_id: Uuid,
        route_hash: &str,
    ) -> Result<crate::execution::ExecutionReport, crate::execution::ExecError> {
        let exec = self.exec.get().ok_or_else(|| {
            crate::execution::ExecError::Disabled(format!(
                "mode {:?}: no testnet execution service",
                self.mode
            ))
        })?;
        let mut limits = risk_limits(self.mode);
        limits.kill_switch = exec.kill_switch();
        let r = exec.execute(route_id, route_hash, &limits).await;
        let _ = self
            .store
            .audit("execute_testnet", Some(&route_id.to_string()), &serde_json::json!({"ok": r.is_ok(), "error": r.as_ref().err().map(|e| e.to_string())}))
            .await;
        r
    }

    pub async fn prepare_user_signed(
        &self,
        route_id: Uuid,
        route_hash: &str,
        accounts: &BTreeMap<PlatformId, serde_json::Value>,
    ) -> Result<crate::execution::PreparedExecution, crate::execution::ExecError> {
        let exec = self.exec.get().ok_or_else(|| {
            crate::execution::ExecError::Disabled(format!(
                "mode {:?}: no execution service",
                self.mode
            ))
        })?;
        let mut limits = risk_limits(self.mode);
        limits.kill_switch = exec.kill_switch();
        let r = exec
            .prepare_user_signed(route_id, route_hash, accounts, &limits)
            .await;
        let _ = self
            .store
            .audit("noncustodial_prepare", Some(&route_id.to_string()), &serde_json::json!({"ok": r.is_ok(), "error": r.as_ref().err().map(|e| e.to_string())}))
            .await;
        r
    }

    pub async fn submit_user_signed(
        &self,
        sub: crate::execution::SignedSubmission,
    ) -> Result<crate::execution::ExecutionReport, crate::execution::ExecError> {
        let exec = self.exec.get().ok_or_else(|| {
            crate::execution::ExecError::Disabled(format!(
                "mode {:?}: no execution service",
                self.mode
            ))
        })?;
        let id = sub.route_id;
        let r = exec.submit_user_signed(sub).await;
        let _ = self
            .store
            .audit("noncustodial_submit", Some(&id.to_string()), &serde_json::json!({"ok": r.is_ok(), "error": r.as_ref().err().map(|e| e.to_string())}))
            .await;
        r
    }

    pub async fn orders(&self, limit: i64) -> Result<Vec<crate::execution::OrderRow>, EngineError> {
        self.store
            .list_orders(limit)
            .await
            .map_err(|e| EngineError::Persistence(e.to_string()))
    }

    pub async fn orders_by_owner(
        &self,
        owner: &str,
        limit: i64,
    ) -> Result<Vec<crate::execution::OrderRow>, EngineError> {
        self.store
            .list_orders_by_owner(owner, limit)
            .await
            .map_err(|e| EngineError::Persistence(e.to_string()))
    }

    pub async fn order(&self, id: Uuid) -> Result<crate::execution::OrderRow, EngineError> {
        self.store
            .get_order(id)
            .await
            .map_err(|e| EngineError::Persistence(e.to_string()))?
            .ok_or_else(|| EngineError::NotFound(format!("order {id}")))
    }

    pub async fn exec_stats(&self) -> Result<Vec<crate::execution::VenueExecStats>, EngineError> {
        Ok(crate::execution::exec_stats(&self.orders(10_000).await?))
    }

    pub async fn get_simulation(&self, id: Uuid) -> Result<SimulationRecord, EngineError> {
        let v = self
            .store
            .get_simulation(id)
            .await
            .map_err(|e| EngineError::Persistence(e.to_string()))?
            .ok_or_else(|| EngineError::NotFound(format!("simulation {id}")))?;
        serde_json::from_value(v).map_err(|e| EngineError::Persistence(e.to_string()))
    }
}

fn is_core_code(c: ReasonCode) -> bool {
    use ReasonCode::*;
    matches!(
        c,
        FamilyMismatch
            | UnsupportedFamily
            | UnderlyingMismatch
            | OptionKindMismatch
            | StrikeMismatch
            | ExpiryMismatch
            | OptionKindUnknown
            | StrikeUnknown
            | ExpiryUnknown
    )
}

/// Books worth subscribing in advance: dated options expiring within 30 days whose strike is within
/// +-12 % of the median strike of their expiry (a spot-free proxy for "near the money"), nearest
/// expiries first, at most 150.
fn prewarm_set(cat: &[Instrument], now: DateTime<Utc>) -> Vec<Instrument> {
    let horizon = now + chrono::Duration::days(30);
    let mut by_exp: BTreeMap<DateTime<Utc>, Vec<&Instrument>> = BTreeMap::new();
    for i in cat {
        if let (Some(e), Some(_)) = (i.expiry, i.strike)
            && e > now
            && e <= horizon
            && i.family == ProductFamily::DatedOption
        {
            by_exp.entry(e).or_default().push(i);
        }
    }
    let mut out = Vec::new();
    for (_, v) in by_exp {
        let mut ks: Vec<Decimal> = v.iter().filter_map(|i| i.strike).collect();
        ks.sort();
        let mid = ks[ks.len() / 2];
        let (lo, hi) = (mid * Decimal::new(88, 2), mid * Decimal::new(112, 2));
        out.extend(
            v.into_iter()
                .filter(|i| i.strike.is_some_and(|k| k >= lo && k <= hi))
                .cloned(),
        );
        if out.len() >= 150 {
            break;
        }
    }
    out.truncate(150);
    out
}

/// Derive and Aevo match orders off-chain; the trader does not send a transaction per trade.
/// [DOC] both platforms: off-chain orderbook, on-chain settlement by the venue.
fn gas_line(p: PlatformId) -> CostLine {
    match p {
        PlatformId::Derive | PlatformId::Aevo | PlatformId::Paradex | PlatformId::Deribit | PlatformId::Okx | PlatformId::Bybit | PlatformId::Binance | PlatformId::Bullish | PlatformId::Coincall | PlatformId::Gate | PlatformId::Delta
        | PlatformId::Kalshi
        | PlatformId::Hypercall
        | PlatformId::Hyperliquid => CostLine::NotApplicable {
            reason: "off-chain matching; no user transaction per trade (deposits/withdrawals not included)".into(),
        },
        // Base L2 fillOrder: gas price 0.006 gwei measured 2026-10-06 -> well under 0.01 USDC +
        // L1 data fee; 0.05 USDC is a conservative ceiling (one-time USDC approval not included).
        PlatformId::Thetanuts => CostLine::Known { amount: Decimal::new(5, 2) },
        // Myriad BNB order book: signed off-chain orders, operator-settled (Polymarket-style CLOB).
        // ASSUMPTION: no per-trade user gas.
        PlatformId::Myriad | PlatformId::Limitless => CostLine::NotApplicable {
            reason: "off-chain order book, operator-settled (assumed: no per-trade user gas)".into(),
        },
        // Hegic (Arbitrum): one buy transaction; Arbitrum gas ~0.01-0.05 USD -> 0.10 ceiling.
        PlatformId::Hegic => CostLine::Known { amount: Decimal::new(10, 2) },
        // CallPut: PositionManager.executionFee() = 0.00006 ETH (~0.15 USD at ETH 2,570, read
        // on-chain 2026-10-08) + Base gas (< 0.01): 0.20 USDC ceiling.
        PlatformId::Callput => CostLine::Known { amount: Decimal::new(20, 2) },
        _ => CostLine::Unknown { reason: "gas not modelled for this platform".into() },
    }
}

fn funding(q: &QuoteEstimate, balances: &BTreeMap<PlatformId, Decimal>) -> FundingStatus {
    let Some(required) = q.required_funds else {
        return FundingStatus::NotEvaluable;
    };
    match balances.get(&q.instrument.platform) {
        None => FundingStatus::Unverified,
        Some(b) if *b >= required => FundingStatus::SimulatedSufficient {
            declared_balance: *b,
        },
        Some(b) => FundingStatus::SimulatedInsufficient {
            declared_balance: *b,
            required,
        },
    }
}

const INCOMPLETE_ONLY: [Ineligibility; 3] = [
    Ineligibility::PartialCoverage,
    Ineligibility::UnknownCosts,
    Ineligibility::BelowMinimumSize,
];

/// Largest quantity covered by every platform that is comparable (or accepted) and whose only
/// blocker is partial coverage. None unless at least two such platforms exist and one is partial.
pub fn common_quantity(
    req: &CompareRequest,
    ranked: &[QuoteView],
    incomplete: &[QuoteView],
    instruments: &[Instrument],
) -> Option<CommonQuantity> {
    let partial: Vec<&QuoteView> = incomplete
        .iter()
        .filter(|q| q.estimate.ineligible == [Ineligibility::PartialCoverage])
        .collect();
    if partial.is_empty() || ranked.len() + partial.len() < 2 {
        return None;
    }
    let group: Vec<&QuoteView> = ranked.iter().chain(partial.iter().copied()).collect();
    let mut q = group.iter().map(|v| v.estimate.covered_underlying).min()?;
    let (mut step, mut min) = (Decimal::ZERO, Decimal::ZERO);
    for v in &group {
        let i = instruments
            .iter()
            .find(|i| i.key == v.estimate.instrument)?;
        // Steps and minimums are in native units; convert to underlying.
        if let Some(s) = i.amount_step {
            step = step.max(s * i.contract_multiplier);
        }
        if let Some(m) = i.min_amount {
            min = min.max(m * i.contract_multiplier);
        }
    }
    if step > Decimal::ZERO {
        q = (q / step).floor() * step;
    }
    if q <= Decimal::ZERO || q < min || q >= req.quantity {
        return None;
    }
    Some(CommonQuantity {
        quantity: q.normalize(),
        platforms: group
            .iter()
            .map(|v| v.estimate.instrument.platform)
            .collect(),
        explanation: format!(
            "Visible depth does not cover {} on every platform. {} {} is covered by all of them              (rounded down to the strictest amount step). Re-run the comparison with this quantity              to compare them completely; the requested quantity itself is not changed automatically.",
            req.quantity.normalize(),
            q.normalize(),
            req.underlying
        ),
    })
}

#[allow(clippy::too_many_arguments)]
fn assemble(
    req: &CompareRequest,
    now: DateTime<Utc>,
    mode: TradingMode,
    reference: Option<InstrumentKey>,
    reference_reason: String,
    views: Vec<(QuoteEstimate, Instrument, String)>,
    unavailable: Vec<VenueIssue>,
    instruments: &[Instrument],
) -> CompareResponse {
    let ranked_est = cost::rank(views.iter().map(|v| v.0.clone()).collect(), req.side);
    let mut ranked = vec![];
    let mut incomplete = vec![];
    let mut ineligible = vec![];
    for est in ranked_est {
        let (_, inst, notes) = views
            .iter()
            .find(|v| v.0.instrument == est.instrument)
            .expect("same set");
        let view = QuoteView {
            funding: funding(&est, &req.simulated_balances),
            terms: inst.into(),
            fee_notes: notes.clone(),
            is_reference: reference.as_ref() == Some(&est.instrument),
            estimate: est,
        };
        if view.estimate.eligible() {
            ranked.push(view);
        } else if view
            .estimate
            .ineligible
            .iter()
            .all(|r| INCOMPLETE_ONLY.contains(r))
        {
            incomplete.push(view);
        } else {
            ineligible.push(view);
        }
    }
    let mut comparability = vec![];
    for (i, a) in instruments.iter().enumerate() {
        for b in &instruments[i + 1..] {
            comparability.push(PairComparability {
                a: a.key.clone(),
                b: b.key.clone(),
                report: comparability::compare(a, b),
            });
        }
    }
    let common_quantity = common_quantity(req, &ranked, &incomplete, instruments);
    let saving = req.baseline_platform.and_then(|bp| {
        let base = ranked
            .iter()
            .find(|q| q.estimate.instrument.platform == bp)?;
        let amount = cost::estimated_saving_vs_baseline(
            &ranked
                .iter()
                .map(|q| q.estimate.clone())
                .collect::<Vec<_>>(),
            &base.estimate.instrument,
            req.side,
        )?;
        Some(SavingEstimate {
            baseline: bp,
            amount,
            currency: base.estimate.premium_currency.clone(),
            label: "Estimated saving under the stated assumptions (not realised)".into(),
        })
    });
    let mut notes = vec![
        "Quotes are indicative: computed from public orderbook snapshots; nothing guarantees a fill.".into(),
        "No shared margin, transfer or bridge between platforms is assumed; funds must already be on the destination.".into(),
    ];
    if req.simulated_balances.is_empty() {
        notes.push("Financial eligibility not verified: no account data (declare simulated_balances to test funding).".into());
    } else {
        notes.push("Funding uses DECLARED simulation balances, not real account data.".into());
    }
    if !req.accepted_differences.is_empty() {
        notes.push(format!(
            "User explicitly accepted these contractual differences for this comparison only: {:?}",
            req.accepted_differences
        ));
    }
    CompareResponse {
        request_id: Uuid::new_v4(),
        computed_at: now,
        mode,
        wording: WORDING.into(),
        request: req.clone(),
        reference,
        reference_reason,
        ranked,
        incomplete,
        ineligible,
        unavailable,
        comparability,
        estimated_saving_vs_baseline: saving,
        common_quantity,
        notes,
    }
}

pub fn build_route(
    c: &CompareResponse,
    instruments: &[Instrument],
    tolerance_bps: u32,
) -> RoutePreview {
    let Some(best) = c.ranked.first() else {
        let mut reasons =
            vec!["No eligible destination with full coverage and known costs.".to_string()];
        for q in c.incomplete.iter().chain(&c.ineligible) {
            reasons.push(format!(
                "{}: {:?}",
                q.estimate.instrument, q.estimate.ineligible
            ));
        }
        for u in &c.unavailable {
            reasons.push(format!("{}: {} ({})", u.platform, u.kind, u.detail));
        }
        return RoutePreview {
            compare: c.clone(),
            route: None,
            no_route_reasons: reasons,
        };
    };
    let e = &best.estimate;
    let avg = e
        .avg_price_per_underlying
        .expect("eligible estimate has a fill");
    let tol = Decimal::from(tolerance_bps) / dec!(10000);
    let limit = match e.side {
        Side::Buy => avg * (Decimal::ONE + tol),
        Side::Sell => avg * (Decimal::ONE - tol),
    };
    let inst = instruments.iter().find(|i| i.key == e.instrument);
    // Round the limit to the platform tick, in the conservative direction for the user.
    let limit = match inst.and_then(|i| i.tick_size) {
        Some(t) if t > Decimal::ZERO => match e.side {
            Side::Buy => (limit / t).floor() * t,
            Side::Sell => (limit / t).ceil() * t,
        },
        _ => limit,
    };
    let mut warnings = vec![
        "Single destination; no split routing in this version.".to_string(),
        "Indicative quote: the book may change before any order; re-quote before acting."
            .to_string(),
        "No automatic transfer, bridge, withdrawal or token approval.".to_string(),
    ];
    if matches!(best.funding, FundingStatus::Unverified) {
        warnings.push(format!("Funds on {} not verified.", e.instrument.platform));
    }
    if !c.request.accepted_differences.is_empty() {
        warnings.push(format!(
            "Route relies on accepted contractual differences: {:?}",
            c.request.accepted_differences
        ));
    }
    let route = Route {
        destination: e.instrument.clone(),
        side: e.side,
        quantity_underlying: e.covered_underlying,
        quantity_native: e.covered_native,
        expected_avg_price_per_underlying: avg,
        limit_price_per_underlying: limit,
        price_tolerance_bps: tolerance_bps,
        estimated_total: e.total_estimated.expect("eligible estimate has a total"),
        fee: e.fee.clone(),
        currency: e.premium_currency.clone(),
        funding: best.funding.clone(),
        accepted_differences_in_scope: c.request.accepted_differences.clone(),
        steps: vec![
            format!(
                "Use funds already on {} ({}).",
                e.instrument.platform, e.instrument.network
            ),
            format!(
                "{:?} {} contracts of {} (= {} {} underlying) as a limit order at {} {} per unit.",
                e.side,
                e.covered_native.normalize(),
                e.instrument.native_id,
                e.covered_underlying.normalize(),
                c.request.underlying,
                limit.normalize(),
                e.premium_currency
            ),
            "In this MVP the order is NOT sent: simulation only.".into(),
        ],
        warnings,
    };
    RoutePreview {
        compare: c.clone(),
        route: Some(route),
        no_route_reasons: vec![],
    }
}

/// Deterministic: same inputs (books, instruments, fees, evaluated_at) => same result.
pub fn simulate_fill(
    route: &Route,
    inst: &Instrument,
    book: &BookSnapshot,
    fees: &FeeSchedule,
) -> SimResult {
    let w = oa_core::book::walk(inst, book, route.side, route.quantity_underlying);
    match w {
        Ok(w) => {
            let fee = fees.taker.compute(&oa_core::fees::FeeInputs {
                qty_native: w.covered_native,
                qty_underlying: w.covered_underlying,
                premium: w.premium,
                index_price: book.index_price,
                mark_price: book.mark_price,
            });
            let total = match &fee {
                FeeAmount::Known { amount } => Some(match route.side {
                    Side::Buy => w.premium + amount,
                    Side::Sell => w.premium - amount,
                }),
                FeeAmount::Unknown { .. } => None,
            };
            let within = w
                .avg_price_per_underlying
                .is_some_and(|a| match route.side {
                    Side::Buy => a <= route.limit_price_per_underlying,
                    Side::Sell => a >= route.limit_price_per_underlying,
                });
            SimResult {
                filled_underlying: w.covered_underlying,
                filled_native: w.covered_native,
                avg_price_per_underlying: w.avg_price_per_underlying,
                premium: w.premium,
                fee,
                total,
                within_limit_price: within,
            }
        }
        Err(_) => SimResult {
            filled_underlying: Decimal::ZERO,
            filled_native: Decimal::ZERO,
            avg_price_per_underlying: None,
            premium: Decimal::ZERO,
            fee: FeeAmount::Unknown {
                reason: "no fill".into(),
            },
            total: None,
            within_limit_price: false,
        },
    }
}

fn scenario(
    name: &str,
    description: &str,
    route: &Route,
    inst: &Instrument,
    book: &BookSnapshot,
    fees: &FeeSchedule,
) -> ScenarioResult {
    let r = simulate_fill(route, inst, book, fees);
    let outcome = if r.filled_underlying.is_zero() {
        ScenarioOutcome::NoFill
    } else if !r.within_limit_price {
        ScenarioOutcome::LimitExceeded
    } else if r.filled_underlying < route.quantity_underlying {
        ScenarioOutcome::PartialFill
    } else {
        ScenarioOutcome::Fill
    };
    ScenarioResult {
        name: name.into(),
        description: description.into(),
        outcome,
        result: r,
    }
}

/// Stress variants of the recorded book: prices moved against the taker, depth halved.
fn scenarios(
    route: &Route,
    inst: &Instrument,
    book: &BookSnapshot,
    fees: &FeeSchedule,
) -> Vec<ScenarioResult> {
    let shift = |bps: i64| {
        let f = Decimal::ONE + Decimal::new(bps, 4);
        let mut b = book.clone();
        for l in b.asks.iter_mut() {
            l.price *= f;
        }
        for l in b.bids.iter_mut() {
            l.price *= Decimal::TWO - f;
        }
        b
    };
    let mut half = book.clone();
    for l in half.asks.iter_mut().chain(half.bids.iter_mut()) {
        l.size /= Decimal::TWO;
    }
    vec![
        scenario(
            "as_recorded",
            "Recorded book, unchanged.",
            route,
            inst,
            book,
            fees,
        ),
        scenario(
            "adverse_50bps",
            "All levels 0.5 % worse for the taker.",
            route,
            inst,
            &shift(50),
            fees,
        ),
        scenario(
            "adverse_200bps",
            "All levels 2 % worse for the taker.",
            route,
            inst,
            &shift(200),
            fees,
        ),
        scenario(
            "half_depth",
            "Every level keeps only half its size (others took liquidity first).",
            route,
            inst,
            &half,
            fees,
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
pub fn build_simulation(
    id: Uuid,
    created_at: DateTime<Utc>,
    mode: TradingMode,
    req: SimulateRequest,
    preview: RoutePreview,
    outcome: &CompareOutcome,
    session: &risk::SessionUsage,
    limits: &RiskLimits,
) -> SimulationRecord {
    let c = &preview.compare;
    let strategy = req.strategy.clone().unwrap_or_else(|| "manual".into());
    let mut risk_violations = vec![];
    let mut route = preview.route.clone();
    let mut price_drift = None;
    let mut scen = vec![];
    let (status, result) = match route.as_mut() {
        None => (SimStatus::NotSimulatedNoRoute, None),
        Some(route) => {
            let idx = outcome
                .instruments
                .iter()
                .position(|i| i.key == route.destination)
                .expect("route from candidates");
            let (inst, book, fees) = (
                &outcome.instruments[idx],
                &outcome.books[idx],
                &outcome.fees[idx],
            );
            // Limit from the quote the user saw (if any, for this destination).
            let mut quoted_limit = false;
            if let Some(q) = req
                .quoted
                .as_ref()
                .filter(|q| q.platform == route.destination.platform)
                && q.avg_price_per_underlying > Decimal::ZERO
            {
                let tol = Decimal::from(route.price_tolerance_bps) / dec!(10000);
                let raw = match route.side {
                    Side::Buy => q.avg_price_per_underlying * (Decimal::ONE + tol),
                    Side::Sell => q.avg_price_per_underlying * (Decimal::ONE - tol),
                };
                route.limit_price_per_underlying = match inst.tick_size {
                    Some(t) if t > Decimal::ZERO => match route.side {
                        Side::Buy => (raw / t).floor() * t,
                        Side::Sell => (raw / t).ceil() * t,
                    },
                    _ => raw,
                };
                quoted_limit = true;
                price_drift = Some(PriceDrift {
                    quoted_avg: q.avg_price_per_underlying,
                    fresh_avg: route.expected_avg_price_per_underlying,
                    drift_bps: ((route.expected_avg_price_per_underlying
                        - q.avg_price_per_underlying)
                        / q.avg_price_per_underlying
                        * dec!(10000))
                    .round_dp(2),
                    quote_age_ms: (c.computed_at - q.computed_at).num_milliseconds(),
                });
                route.warnings.push(
                    "Limit price derived from the quote you saw, not from the fresh book.".into(),
                );
            }
            let max_fee = match &route.fee {
                FeeAmount::Known { amount } => *amount,
                FeeAmount::Unknown { .. } => Decimal::ZERO,
            };
            let intent = OrderIntent {
                instrument: route.destination.clone(),
                family: inst.family,
                underlying: inst.underlying.clone(),
                side: route.side,
                qty_underlying: route.quantity_underlying,
                limit_price_per_underlying: route.limit_price_per_underlying,
                max_fee,
                strategy: strategy.clone(),
            };
            let reference = book.mark_price.map(|m| m / inst.contract_multiplier);
            risk_violations = risk::check(&intent, limits, session, reference, false);
            scen = scenarios(route, inst, book, fees);
            if !risk_violations.is_empty() {
                (SimStatus::RejectedByRiskChecks, None)
            } else {
                let r = simulate_fill(route, inst, book, fees);
                let st = if quoted_limit && !r.within_limit_price {
                    SimStatus::RejectedPriceMoved
                } else if r.filled_underlying >= route.quantity_underlying {
                    SimStatus::SimulatedFill
                } else {
                    SimStatus::SimulatedPartialFill
                };
                (st, Some(r))
            }
        }
    };
    SimulationRecord {
        id,
        created_at,
        evaluated_at: c.computed_at,
        engine_version: ENGINE_VERSION.into(),
        mode,
        status,
        accepted_differences: req.compare.accepted_differences.clone(),
        request: req,
        route,
        result,
        risk_violations,
        estimated_saving_vs_baseline: c.estimated_saving_vs_baseline.clone(),
        compare: c.clone(),
        instruments_used: outcome.instruments.iter().map(Into::into).collect(),
        books_used: outcome.books.clone(),
        fees_used: outcome.fees.clone(),
        strategy,
        session_usage_before: session.clone(),
        price_drift,
        scenarios: scen,
        assumptions: vec![
            "Immediate taker fill against the recorded visible book snapshot.".into(),
            "No queue priority, hidden liquidity, cancellations, latency or market reaction."
                .into(),
            "Fees from the documented schedule at the time of the snapshot.".into(),
            "Funds assumed already on the destination platform (no transfer).".into(),
            "Stress scenarios move or thin the SAME recorded book; they are not forecasts.".into(),
        ],
        limitations: vec![
            "A simulated result is not a realised saving or a guaranteed price.".into(),
            "Book depth is limited to what the platform publishes (Aevo: often 2 levels per side)."
                .into(),
            "Settlement-index differences, if accepted, can change the payoff at expiry.".into(),
            "Session caps use the simulated session of this server process only.".into(),
        ],
    }
}

/// Replay a stored simulation from its recorded inputs only (no network, no clock).
/// Returns the recomputed result, which must equal the stored one.
pub fn replay(rec: &SimulationRecord) -> Option<SimResult> {
    let route = rec.route.as_ref()?;
    let idx = rec
        .instruments_used
        .iter()
        .position(|i| i.key == route.destination)?;
    let inst: Instrument = (&rec.instruments_used[idx]).into();
    let book = rec
        .books_used
        .iter()
        .find(|b| b.instrument == route.destination)?;
    Some(simulate_fill(route, &inst, book, &rec.fees_used[idx]))
}

#[cfg(test)]
mod instrument_ref_tests {
    use super::*;
    use oa_adapters::Connector;

    /// Regression: two venues listing the same native name must keep separate signing metadata.
    #[tokio::test]
    async fn same_native_name_on_two_venues_keeps_both_refs() {
        let mut cat = crate::fixtures::FixtureConnector::derive()
            .list_instruments("ETH")
            .await
            .unwrap();
        let a = cat.remove(0);
        let mut b = a.clone();
        b.key.platform = PlatformId::Aevo;
        b.native = serde_json::json!({"instrument_id": "4242"});
        let refs = instrument_refs(&[a.clone(), b.clone()]);
        assert_eq!(refs.len(), 2);
        let ra = &refs[&instrument_ref_key(PlatformId::Derive, &a.key.native_id)];
        let rb = &refs[&instrument_ref_key(PlatformId::Aevo, &b.key.native_id)];
        assert_eq!(rb.venue_instrument_id, Some(4242));
        assert_ne!(ra.venue_instrument_id, Some(4242));
        assert!(ra.venue_meta.get("base_asset_address").is_some());
    }
}
