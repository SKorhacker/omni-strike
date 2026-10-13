// Subset of the API schemas (see GET /openapi.json). Decimals are strings: they are displayed
// as-is and never converted to floating point for money computations.

export type Dec = string;
export type PlatformId = string;

export interface InstrumentKey {
  platform: PlatformId;
  network: string;
  native_id: string;
}

export interface CatalogInfo {
  underlying: string;
  state: string;
  instruments: number | null;
  fetched_at: string | null;
  error: string | null;
}

export interface VenueHealth {
  platform: PlatformId;
  state: string;
  consecutive_failures: number;
  last_success_at: string | null;
  last_error: string | null;
  circuit_open_until: string | null;
  catalogs: CatalogInfo[];
  cached_books: number;
}

export interface PlatformView {
  id: PlatformId;
  display_name: string;
  aliases: string[];
  families: string[];
  networks: string[];
  operational_status: string;
  integration_level: string;
  capabilities: Record<string, string>;
  official_sources: string[];
  verified_on: string;
  notes: string;
  implemented: string[];
  health: VenueHealth | null;
}

export interface PlatformsResponse {
  mode: string;
  platforms: PlatformView[];
}

export interface Listing {
  key: InstrumentKey;
  settlement_index: string | null;
  settlement_window: string | null;
}

export interface ContractGroup {
  option_kind: "call" | "put";
  strike: Dec;
  expiry: string;
  listings: Listing[];
}

export interface SearchResponse {
  underlying: string;
  expiries: { expiry: string; platforms: PlatformId[] }[];
  total_groups: number;
  groups: ContractGroup[];
  catalogs: { platform: PlatformId; ok: boolean; instruments: number; error: string | null }[];
}

export type FeeAmount = { status: "known"; amount: Dec } | { status: "unknown"; reason: string };
export type CostLine =
  | { status: "known"; amount: Dec }
  | { status: "not_applicable"; reason: string }
  | { status: "unknown"; reason: string };

export interface ComparabilityReport {
  verdict: string;
  reasons: string[];
  explanation: string;
}

export interface QuoteEstimate {
  instrument: InstrumentKey;
  side: string;
  provenance: "LIVE_DATA" | "FIXTURE" | "MOCK";
  firmness: string;
  pricing_method: string;
  source_ts: string | null;
  received_at: string;
  age_ms: number;
  valid_until: string;
  requested_underlying: Dec;
  covered_underlying: Dec;
  covered_native: Dec;
  premium_currency: string;
  premium: Dec;
  avg_price_per_underlying: Dec | null;
  book_impact: Dec | null;
  levels_consumed: number;
  fee: FeeAmount;
  gas: CostLine;
  index_price: Dec | null;
  total_estimated: Dec | null;
  total_per_underlying: Dec | null;
  unknown_costs: string[];
  required_funds: Dec | null;
  comparability: ComparabilityReport | null;
  ineligible: string[];
  assumptions: string[];
  confidence: string;
  fee_source: string;
}

export interface ContractTerms {
  key: InstrumentKey;
  family: string;
  underlying: string;
  option_kind: string | null;
  strike: Dec | null;
  expiry: string | null;
  exercise_style: string | null;
  contract_multiplier: Dec;
  premium_currency: string;
  collateral_assets: string[];
  settlement_asset: string | null;
  settlement_mechanism: string | null;
  settlement_index: string | null;
  settlement_window: string | null;
  tick_size: Dec | null;
  min_amount: Dec | null;
  amount_step: Dec | null;
  spec_version: string;
}

export type FundingStatus =
  | { status: "UNVERIFIED" }
  | { status: "SIMULATED_SUFFICIENT"; declared_balance: Dec }
  | { status: "SIMULATED_INSUFFICIENT"; declared_balance: Dec; required: Dec }
  | { status: "NOT_EVALUABLE" };

export interface QuoteView {
  estimate: QuoteEstimate;
  terms: ContractTerms;
  funding: FundingStatus;
  fee_notes: string;
  is_reference: boolean;
}

export interface CompareRequest {
  underlying: string;
  option_kind: "call" | "put";
  strike: Dec;
  expiry: string;
  side: "buy" | "sell";
  quantity: Dec;
  accepted_differences: string[];
  baseline_platform?: PlatformId;
  max_age_ms?: number;
  simulated_balances?: Record<PlatformId, Dec>;
  price_tolerance_bps?: number;
  strategy?: string;
  quoted?: { platform: PlatformId; avg_price_per_underlying: Dec; computed_at: string };
}

export interface CompareResponse {
  request_id: string;
  computed_at: string;
  mode: string;
  wording: string;
  request: CompareRequest;
  reference: InstrumentKey | null;
  reference_reason: string;
  ranked: QuoteView[];
  incomplete: QuoteView[];
  ineligible: QuoteView[];
  unavailable: { platform: PlatformId; kind: string; detail: string }[];
  comparability: { a: InstrumentKey; b: InstrumentKey; report: ComparabilityReport }[];
  estimated_saving_vs_baseline: { baseline: PlatformId; amount: Dec; currency: string; label: string } | null;
  common_quantity: { quantity: Dec; platforms: PlatformId[]; explanation: string } | null;
  notes: string[];
}

export interface Route {
  destination: InstrumentKey;
  side: string;
  quantity_underlying: Dec;
  quantity_native: Dec;
  expected_avg_price_per_underlying: Dec;
  limit_price_per_underlying: Dec;
  price_tolerance_bps: number;
  estimated_total: Dec;
  fee: FeeAmount;
  currency: string;
  funding: FundingStatus;
  accepted_differences_in_scope: string[];
  steps: string[];
  warnings: string[];
}

export interface RoutePreview {
  compare: CompareResponse;
  route: Route | null;
  no_route_reasons: string[];
}

export interface SimulationRecord {
  id: string;
  created_at: string;
  evaluated_at: string;
  mode: string;
  status: string;
  accepted_differences: string[];
  route: Route | null;
  result: {
    filled_underlying: Dec;
    filled_native: Dec;
    avg_price_per_underlying: Dec | null;
    premium: Dec;
    fee: FeeAmount;
    total: Dec | null;
    within_limit_price: boolean;
  } | null;
  risk_violations: { code: string; [k: string]: unknown }[];
  strategy: string;
  price_drift: { quoted_avg: Dec; fresh_avg: Dec; drift_bps: Dec; quote_age_ms: number } | null;
  scenarios: { name: string; description: string; outcome: string; result: { filled_underlying: Dec; avg_price_per_underlying: Dec | null; total: Dec | null; within_limit_price: boolean } }[];
  estimated_saving_vs_baseline: CompareResponse["estimated_saving_vs_baseline"];
  assumptions: string[];
  limitations: string[];
}

export interface ApiErrorBody {
  error: { code: string; message: string };
}
