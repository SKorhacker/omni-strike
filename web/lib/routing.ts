import type { CompareResponse, Dec, FeeAmount, InstrumentKey } from "./types";

export interface SplitPolicyInput {
  allow_split: boolean;
  max_legs?: number;
  min_saving_abs?: Dec;
  min_saving_bps?: Dec;
  min_leg_underlying?: Dec;
  limit_tolerance_bps?: Dec;
}

export interface Leg {
  instrument: InstrumentKey;
  qty_underlying: Dec;
  qty_native: Dec;
  expected_premium: Dec;
  expected_avg_per_underlying: Dec;
  worst_level_price_native: Dec;
  limit_price_native: Dec;
  fee: FeeAmount;
  expected_total: Dec;
  levels_consumed: number;
}

export interface RoutePlan {
  kind: "SINGLE" | "SPLIT";
  legs: Leg[];
  requested_underlying: Dec;
  covered_underlying: Dec;
  fully_covered: boolean;
  expected_total: Dec;
  ranking_total: Dec;
  best_single: [InstrumentKey, Dec] | null;
  estimated_saving_vs_best_single: Dec | null;
  reason: string;
}

export interface RouteQuote {
  route_id: string;
  route_hash: string;
  created_at: string;
  expires_at: string;
  mode: string;
  plan: RoutePlan | null;
  no_plan_reason: string | null;
  candidates: { instrument: InstrumentKey; routable: boolean; reasons: string[]; executable_here: boolean; quality_penalty_bps: Dec }[];
  executable: boolean;
  not_executable_reasons: string[];
  compare: CompareResponse;
  notes: string[];
}

export interface OrderRow {
  id: string;
  route_id: string;
  leg_index: number;
  mode: string;
  platform: string;
  network: string;
  native_id: string;
  side: string;
  qty_native: Dec;
  limit_price_native: Dec;
  expected_avg_per_underlying: Dec;
  expected_total: Dec;
  expected_fee: Dec | null;
  venue_order_id: string | null;
  precomputed_venue_id: string | null;
  state: string;
  is_final: boolean;
  filled_native: Dec;
  avg_fill_price_native: Dec | null;
  last_error: string | null;
  created_at: string;
  submit_to_ack_ms: number | null;
  reconcile_attempts: number;
}

export interface ExecutionReport {
  route_id: string;
  mode: string;
  orders: OrderRow[];
  requested_underlying: Dec;
  filled_underlying: Dec;
  expected_total: Dec;
  notes: string[];
}

export interface VenueExecStats {
  platform: string;
  orders: number;
  filled: number;
  partially_filled: number;
  rejected: number;
  cancelled: number;
  unknown_or_open: number;
  fill_ratio: Dec | null;
  slippage_bps_mean: Dec | null;
  slippage_bps_max: Dec | null;
  submit_to_ack_ms_p50: number | null;
  submit_to_ack_ms_max: number | null;
  router_penalty_bps: Dec;
  sample_note: string;
}

export interface ExecStatsResponse {
  mode: string;
  venues: VenueExecStats[];
  execution_venues: Record<string, unknown>[];
  note: string;
}
