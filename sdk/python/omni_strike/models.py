"""Typed models of the Omni Strike API v1 (``TypedDict``: responses are plain dicts).

Written from the server's OpenAPI document (``GET /openapi.json``; schemas generated from the
Rust types). Monetary / quantity fields are ``decimal.Decimal`` after decoding (see
``omni_strike._codec``). Times are RFC 3339 UTC strings. Quantities are in UNDERLYING units
unless the field name says ``native``.
"""

from __future__ import annotations

from decimal import Decimal
from typing import Any, Dict, List, Literal, Optional, Tuple, TypedDict, Union

DateTime = str
Uuid = str
#: Decimal inputs: ``Decimal`` (recommended), ``str`` or ``int``. Floats are refused.
DecimalInput = Union[Decimal, str, int]

PlatformId = Literal[
    "derive", "aevo", "paradex", "hegic", "stryke", "panoptic", "premia", "thales",
    "strike_onchain", "rysk", "myso", "opyn", "thetanuts", "deribit", "okx", "bybit",
    "binance", "bullish", "coincall", "gate", "delta", "kalshi", "hyperliquid", "polymarket",
]
OptionKind = Literal["call", "put"]
Side = Literal["buy", "sell"]
ProductFamily = Literal["dated_option", "perpetual_option", "binary", "yield_strategy", "other"]
TradingMode = Literal["READ_ONLY", "SIMULATION", "TESTNET", "MAINNET_NONCUSTODIAL", "LIVE"]
ReasonCode = Literal[
    "FAMILY_MISMATCH", "UNSUPPORTED_FAMILY", "UNDERLYING_MISMATCH", "OPTION_KIND_MISMATCH",
    "STRIKE_MISMATCH", "EXPIRY_MISMATCH", "OPTION_KIND_UNKNOWN", "STRIKE_UNKNOWN",
    "EXPIRY_UNKNOWN", "EXERCISE_STYLE_DIFFERS", "EXERCISE_STYLE_UNKNOWN",
    "SETTLEMENT_MECHANISM_DIFFERS", "SETTLEMENT_MECHANISM_UNKNOWN", "SETTLEMENT_ASSET_DIFFERS",
    "SETTLEMENT_ASSET_UNKNOWN", "SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_INDEX_UNKNOWN",
    "SETTLEMENT_WINDOW_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN", "PREMIUM_CURRENCY_DIFFERS",
]
Ineligibility = Literal[
    "STALE", "CLOCK_SKEW", "PARTIAL_COVERAGE", "UNKNOWN_COSTS", "NOT_COMPARABLE",
    "DIFFERENCES_NOT_ACCEPTED", "BELOW_MINIMUM_SIZE", "INVALID_BOOK", "CURRENCY_MISMATCH",
    "PLATFORM_UNHEALTHY", "RETAIL_ONLY_LIQUIDITY",
]
OrderState = Literal[
    "CREATED", "VALIDATED", "SUBMITTING", "ACKNOWLEDGED", "PARTIALLY_FILLED", "FILLED",
    "CANCEL_PENDING", "CANCELLED", "REJECTED", "UNKNOWN",
]

# ------------------------------------------------------------------ requests


class InstrumentKey(TypedDict):
    platform: PlatformId
    network: str
    native_id: str


class _CompareRequired(TypedDict):
    underlying: str
    option_kind: OptionKind
    strike: DecimalInput
    #: Exact expiry instant, RFC 3339 UTC (a ``datetime`` is also accepted).
    expiry: Any
    side: Side
    #: Underlying units (binary family: payout amount).
    quantity: DecimalInput


class CompareRequest(_CompareRequired, total=False):
    accepted_differences: List[ReasonCode]
    reference_platform: PlatformId
    baseline_platform: PlatformId
    #: 1000..=60000, default 10000.
    max_age_ms: int
    #: "dated_option" (default) or "binary".
    family: Literal["dated_option", "binary"]
    #: 50..=15000 ms wait for uncached venue books; slower venues -> SLOW_VENUE.
    latency_budget_ms: int
    simulated_balances: Dict[PlatformId, DecimalInput]


class SplitPolicyInput(TypedDict, total=False):
    allow_split: bool
    #: 1..=3 (default 2 when split is allowed).
    max_legs: int
    min_saving_abs: DecimalInput
    min_saving_bps: DecimalInput
    min_leg_underlying: DecimalInput
    #: 0..=500, default 50.
    limit_tolerance_bps: DecimalInput


class RouteQuoteRequest(CompareRequest, total=False):
    split: SplitPolicyInput


class ExecuteRequest(TypedDict):
    route_id: Uuid
    route_hash: str


# ------------------------------------------------------------------ shared


class FeeAmount(TypedDict, total=False):
    #: "known" | "unknown"
    status: str
    amount: Decimal
    reason: str


class ComparabilityReport(TypedDict):
    verdict: str
    reasons: List[ReasonCode]
    explanation: str


# ------------------------------------------------------------------ platforms


class VenueHealth(TypedDict):
    platform: PlatformId
    network: str
    state: str
    consecutive_failures: int
    last_success_at: Optional[DateTime]
    last_error: Optional[str]
    last_error_at: Optional[DateTime]
    circuit_open_until: Optional[DateTime]
    catalogs: List[Dict[str, Any]]
    cached_books: int
    book_cache_capacity: int
    fetches_inflight: int
    mailbox_capacity: int


class Platform(TypedDict):
    id: PlatformId
    display_name: str
    aliases: List[str]
    families: List[ProductFamily]
    networks: List[str]
    operational_status: str
    integration_level: str
    capabilities: Dict[str, str]
    official_sources: List[str]
    verified_on: str
    notes: str
    implemented: List[str]
    health: Optional[VenueHealth]
    diagnostics: Optional[Any]


class PlatformsResponse(TypedDict):
    mode: TradingMode
    platforms: List[Platform]


# ------------------------------------------------------------------ instruments


class Listing(TypedDict):
    key: InstrumentKey
    settlement_index: Optional[str]
    settlement_window: Optional[str]


class ContractGroup(TypedDict):
    option_kind: OptionKind
    strike: Decimal
    expiry: DateTime
    listings: List[Listing]


class InstrumentsResponse(TypedDict):
    underlying: str
    family: ProductFamily
    expiries: List[Dict[str, Any]]
    total_groups: int
    offset: int
    limit: int
    groups: List[ContractGroup]
    catalogs: List[Dict[str, Any]]


# ------------------------------------------------------------------ compare


class ContractTerms(TypedDict):
    key: InstrumentKey
    family: ProductFamily
    underlying: str
    option_kind: Optional[OptionKind]
    strike: Optional[Decimal]
    expiry: Optional[DateTime]
    exercise_style: Optional[str]
    contract_multiplier: Decimal
    premium_currency: str
    collateral_assets: List[str]
    settlement_asset: Optional[str]
    settlement_mechanism: Optional[str]
    settlement_index: Optional[str]
    settlement_window: Optional[str]
    tick_size: Optional[Decimal]
    min_amount: Optional[Decimal]
    amount_step: Optional[Decimal]
    spec_version: str


class QuoteEstimate(TypedDict):
    instrument: InstrumentKey
    side: Side
    provenance: str
    firmness: str
    pricing_method: str
    source_ts: Optional[DateTime]
    received_at: DateTime
    age_ms: int
    valid_until: DateTime
    requested_underlying: Decimal
    covered_underlying: Decimal
    covered_native: Decimal
    premium_currency: str
    premium: Decimal
    avg_price_per_underlying: Optional[Decimal]
    book_impact: Optional[Decimal]
    levels_consumed: int
    fee: FeeAmount
    gas: Dict[str, Any]
    index_price: Optional[Decimal]
    total_estimated: Optional[Decimal]
    total_per_underlying: Optional[Decimal]
    unknown_costs: List[str]
    required_funds: Optional[Decimal]
    comparability: Optional[ComparabilityReport]
    ineligible: List[Ineligibility]
    assumptions: List[str]
    confidence: str
    fee_source: str


class QuoteView(TypedDict):
    estimate: QuoteEstimate
    terms: ContractTerms
    #: {"status": "UNVERIFIED" | "SIMULATED_SUFFICIENT" | "SIMULATED_INSUFFICIENT" | "NOT_EVALUABLE", ...}
    funding: Dict[str, Any]
    fee_notes: str
    is_reference: bool


class VenueIssue(TypedDict):
    platform: PlatformId
    kind: str
    detail: str


class SavingEstimate(TypedDict):
    baseline: PlatformId
    amount: Decimal
    currency: str
    label: str


class CommonQuantity(TypedDict):
    quantity: Decimal
    platforms: List[PlatformId]
    explanation: str


class CompareResponse(TypedDict):
    request_id: Uuid
    computed_at: DateTime
    mode: TradingMode
    wording: str
    request: Dict[str, Any]
    reference: Optional[InstrumentKey]
    reference_reason: str
    ranked: List[QuoteView]
    incomplete: List[QuoteView]
    ineligible: List[QuoteView]
    unavailable: List[VenueIssue]
    comparability: List[Dict[str, Any]]
    estimated_saving_vs_baseline: Optional[SavingEstimate]
    common_quantity: Optional[CommonQuantity]
    notes: List[str]


# ------------------------------------------------------------------ routing / execution


class RouteLeg(TypedDict):
    instrument: InstrumentKey
    qty_underlying: Decimal
    qty_native: Decimal
    expected_premium: Decimal
    expected_avg_per_underlying: Decimal
    worst_level_price_native: Decimal
    limit_price_native: Decimal
    fee: FeeAmount
    expected_total: Decimal
    levels_consumed: int


class RoutePlan(TypedDict):
    kind: Literal["SINGLE", "SPLIT"]
    legs: List[RouteLeg]
    requested_underlying: Decimal
    covered_underlying: Decimal
    fully_covered: bool
    expected_total: Decimal
    ranking_total: Decimal
    #: [InstrumentKey, Decimal] or None
    best_single: Optional[Tuple[InstrumentKey, Decimal]]
    estimated_saving_vs_best_single: Optional[Decimal]
    reason: str


class RouteCandidate(TypedDict):
    instrument: InstrumentKey
    routable: bool
    reasons: List[str]
    executable_here: bool
    quality_penalty_bps: Decimal


class RouteQuote(TypedDict):
    route_id: Uuid
    route_hash: str
    created_at: DateTime
    expires_at: DateTime
    mode: TradingMode
    policy: Dict[str, Any]
    plan: Optional[RoutePlan]
    no_plan_reason: Optional[str]
    candidates: List[RouteCandidate]
    balances: Dict[str, Any]
    executable: bool
    not_executable_reasons: List[str]
    compare: CompareResponse
    notes: List[str]


class Order(TypedDict):
    id: Uuid
    route_id: Uuid
    leg_index: int
    mode: TradingMode
    platform: PlatformId
    network: str
    native_id: str
    side: Side
    qty_native: Decimal
    limit_price_native: Decimal
    contract_multiplier: Decimal
    salt: int
    timestamp: int
    venue_instrument_id: Optional[int]
    venue_meta: Any
    max_fee_per_unit: Optional[Decimal]
    expected_avg_per_underlying: Decimal
    expected_total: Decimal
    expected_fee: Optional[Decimal]
    precomputed_venue_id: Optional[str]
    venue_order_id: Optional[str]
    state: OrderState
    is_final: bool
    filled_native: Decimal
    avg_fill_price_native: Optional[Decimal]
    last_error: Optional[str]
    created_at: DateTime
    submitted_at: Optional[DateTime]
    acked_at: Optional[DateTime]
    updated_at: DateTime
    submit_to_ack_ms: Optional[int]
    reconcile_attempts: int
    last_venue_response: Optional[Any]


class ExecutionReport(TypedDict):
    route_id: Uuid
    mode: TradingMode
    orders: List[Order]
    requested_underlying: Decimal
    filled_underlying: Decimal
    expected_total: Decimal
    notes: List[str]


class BalancesResponse(TypedDict, total=False):
    mode: TradingMode
    #: platform -> {"available", "equity", "currency", "source"} or {"error": ...}
    balances: Dict[str, Dict[str, Any]]
    note: str


class VenueExecStats(TypedDict):
    platform: PlatformId
    orders: int
    filled: int
    partially_filled: int
    rejected: int
    cancelled: int
    unknown_or_open: int
    fill_ratio: Optional[Decimal]
    slippage_bps_mean: Optional[Decimal]
    slippage_bps_max: Optional[Decimal]
    submit_to_ack_ms_p50: Optional[int]
    submit_to_ack_ms_max: Optional[int]
    router_penalty_bps: Decimal
    sample_note: str


class ExecStatsResponse(TypedDict):
    mode: TradingMode
    venues: List[VenueExecStats]
    execution_venues: Any
    note: str


# ------------------------------------------------------------------ live / health


class LiveSummary(TypedDict):
    since: Optional[DateTime]
    observations: int
    observations_beating_derive: int
    hypothetical_saving_usd: float
    hypothetical_derive_cost_usd: float
    win_rate: float
    median_saving_pct: float
    mean_saving_pct: float
    wins_by_venue: Dict[str, int]


class LiveSavingsSnapshot(TypedDict):
    updated_at: Optional[DateTime]
    wording: str
    basket_size: int
    summary: LiveSummary
    #: rows with Decimal strike/quantity/best_total/derive_total and float saving_* stats
    contracts: List[Dict[str, Any]]
    feed: List[Dict[str, Any]]
    series: List[Dict[str, Any]]


class Readiness(TypedDict):
    ready: bool
    database: str
    venues: List[VenueHealth]
    mode: TradingMode



# ------------------------------------------------------------------ multi-leg strategies


class _StrategyLegRequired(TypedDict):
    option_kind: OptionKind
    strike: DecimalInput
    #: RFC 3339 UTC (a ``datetime`` is also accepted).
    expiry: Any
    side: Side


class StrategyLeg(_StrategyLegRequired, total=False):
    #: Leg size = strategy ``quantity`` x ``ratio`` (default 1), in (0, 100].
    ratio: Optional[DecimalInput]


class _StrategyRequired(TypedDict):
    underlying: str
    #: Base size in underlying units; each leg = quantity x ratio.
    quantity: DecimalInput
    #: 1..=6 legs.
    legs: List[StrategyLeg]


class StrategyRequest(_StrategyRequired, total=False):
    accepted_differences: List[ReasonCode]
    #: Prefer one venue when it costs at most this many bps of notional more than the best split
    #: (default: your integrator setting, else 10).
    single_venue_tolerance_bps: Optional[int]
    #: Savings are reported vs doing the whole strategy on this venue (default "derive").
    baseline_platform: Optional[PlatformId]
    family: Optional[ProductFamily]
    latency_budget_ms: Optional[int]


class LegQuote(TypedDict):
    platform: PlatformId
    native_id: str
    #: Buy: total cost incl. fees. Sell: net proceeds after fees. Premium currency.
    amount: Decimal


class PlannedLeg(TypedDict):
    leg: int
    side: Side
    quote: LegQuote


PlanKind = Literal["SINGLE_VENUE", "SPLIT"]


class StrategyPlan(TypedDict):
    kind: PlanKind
    legs: List[PlannedLeg]
    #: sum(buy costs) - sum(sell proceeds): positive = debit, negative = credit.
    net_cost: Decimal
    best_split_net_cost: Decimal
    #: ``[platform, net_cost]`` of the best venue listing every leg, or None.
    best_single_venue: Optional[List[Any]]
    saving_vs_baseline: Optional[Decimal]
    reason: str


class LegDetail(TypedDict):
    leg: int
    request: Dict[str, Any]
    quantity: Decimal
    #: Eligible, fully covered quotes, best first.
    quotes: List[LegQuote]
    unavailable: List[VenueIssue]


class StrategyQuote(TypedDict):
    computed_at: DateTime
    wording: str
    request: Dict[str, Any]
    plan: Optional[StrategyPlan]
    no_plan_reason: Optional[str]
    legs: List[LegDetail]
    index_price: Optional[Decimal]
    #: Tolerance actually applied, premium currency.
    single_venue_tolerance: Decimal
    notes: List[str]


# ------------------------------------------------------------------ integrators


class IntegratorSettings(TypedDict):
    """Self-service limits. ``PUT /v1/integrator/settings`` takes the FULL object."""

    max_notional_per_order_usd: DecimalInput
    max_notional_per_day_usd: DecimalInput
    #: None = all venues.
    allowed_platforms: Optional[List[PlatformId]]
    #: None = all underlyings.
    allowed_underlyings: Optional[List[str]]
    max_slippage_bps: int
    unwind_cost_cap_bps: int
    single_venue_tolerance_bps: int
    simulation_first: bool
    rate_limit_rps: int


class Integrator(TypedDict):
    id: Uuid
    name: str
    key_prefix: str
    settings: IntegratorSettings
    active: bool
    created_at: DateTime
    updated_at: DateTime


class CreatedIntegrator(TypedDict):
    integrator: Integrator
    #: Shown ONCE: store it now, it cannot be retrieved later.
    api_key: str
