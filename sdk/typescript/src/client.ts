import { OmniStrikeError } from "./errors.js";
import type {
  BalancesResponse,
  CompareRequest,
  CreateIntegratorRequest,
  CreatedIntegrator,
  CompareResponse,
  ExecStatsResponse,
  ExecuteRequest,
  PrepareNonCustodialRequest,
  PreparedExecution,
  SignedSubmission,
  ExecutionReport,
  ExplainRequest,
  ExplainResponse,
  InstrumentsQuery,
  InstrumentsResponse,
  Integrator,
  IntegratorSettings,
  ListOrdersQuery,
  LiveSavingsSnapshot,
  Order,
  PlatformsResponse,
  Readiness,
  RoutePreview,
  RouteQuote,
  RouteQuoteRequest,
  SimulateRequest,
  SimulationRecord,
  StrategyQuote,
  StrategyRequest,
} from "./types.js";

export const SDK_VERSION = "0.1.0";

/** Minimal `fetch` signature the client needs (inject a fake one in tests). */
export type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

export interface OmniStrikeOptions {
  /** API root, e.g. "https://api.omnistrike.io" or "http://127.0.0.1:8080". */
  baseUrl?: string;
  /** Integrator API key, sent as `Authorization: Bearer <apiKey>`. */
  apiKey?: string;
  /** Per-attempt timeout in ms (default 30000; compare can wait up to 15 s for slow venues). */
  timeoutMs?: number;
  /** Retries for idempotent GETs answered 429/503 (default 2). POSTs are never retried. */
  maxRetries?: number;
  /** First backoff delay in ms (default 250), doubled each attempt, with jitter. */
  retryBaseDelayMs?: number;
  /** Upper bound for any single retry wait, including Retry-After (default 10000 ms). */
  maxRetryDelayMs?: number;
  /** Extra headers on every request. */
  headers?: Record<string, string>;
  /** Custom fetch (default: global fetch). */
  fetch?: FetchLike;
  /** Custom sleep (testing hook). */
  sleep?: (ms: number) => Promise<void>;
}

export interface RequestOptions {
  /** Abort the call (combined with the client timeout). */
  signal?: AbortSignal;
  /** Override the client timeout for this call. */
  timeoutMs?: number;
}

type Query = Record<string, string | number | boolean | undefined | null>;

const RETRYABLE = new Set([429, 503]);

/** Parses `Retry-After` (delta-seconds or HTTP-date) into ms. */
export function parseRetryAfter(value: string | null, now: number = Date.now()): number | undefined {
  if (value == null || value.trim() === "") return undefined;
  const v = value.trim();
  if (/^\d+(\.\d+)?$/.test(v)) return Math.round(Number(v) * 1000);
  const at = Date.parse(v);
  if (Number.isNaN(at)) return undefined;
  return Math.max(0, at - now);
}

/**
 * Omni Strike API client: best execution for crypto options across venues.
 *
 * ```ts
 * const os = new OmniStrike({ apiKey: process.env.OMNI_STRIKE_API_KEY });
 * const cmp = await os.compareQuotes({ underlying: "BTC", option_kind: "call",
 *   strike: "120000", expiry: "2026-12-25T08:00:00Z", side: "buy", quantity: "0.5" });
 * ```
 */
/** The text a wallet signs (personal_sign) to read its own orders with `myOrders`. */
export function ordersMessage(wallet: string, timestampMs: number): string {
  return `Omni Strike: read my orders\nWallet: ${wallet.toLowerCase()}\nTimestamp: ${timestampMs}`;
}

export class OmniStrike {
  readonly baseUrl: string;
  private readonly apiKey: string | undefined;
  private readonly timeoutMs: number;
  private readonly maxRetries: number;
  private readonly retryBaseDelayMs: number;
  private readonly maxRetryDelayMs: number;
  private readonly headers: Record<string, string>;
  private readonly fetchImpl: FetchLike;
  private readonly sleep: (ms: number) => Promise<void>;

  constructor(options: OmniStrikeOptions = {}) {
    this.baseUrl = (options.baseUrl ?? "http://127.0.0.1:8080").replace(/\/+$/, "");
    this.apiKey = options.apiKey;
    this.timeoutMs = options.timeoutMs ?? 30_000;
    this.maxRetries = Math.max(0, options.maxRetries ?? 2);
    this.retryBaseDelayMs = options.retryBaseDelayMs ?? 250;
    this.maxRetryDelayMs = options.maxRetryDelayMs ?? 10_000;
    this.headers = { ...(options.headers ?? {}) };
    if (!options.fetch && typeof globalThis.fetch !== "function") {
      throw new Error("OmniStrike: no fetch available; pass options.fetch (Node >= 18 has it)");
    }
    // Late-bound global fetch (keeps `this` = globalThis, required by browsers).
    this.fetchImpl = options.fetch ?? ((input, init) => globalThis.fetch(input, init));
    this.sleep = options.sleep ?? ((ms) => new Promise((r) => setTimeout(r, ms)));
  }

  // ---------------------------------------------------------------- discovery

  /** `GET /v1/platforms`: all platforms with status, capabilities and live health. */
  platforms(opts?: RequestOptions): Promise<PlatformsResponse> {
    return this.request("GET", "/v1/platforms", { opts });
  }

  /** `GET /v1/instruments`: dated options (or binaries) grouped by kind/strike/expiry. */
  instruments(query: InstrumentsQuery, opts?: RequestOptions): Promise<InstrumentsResponse> {
    const expiry = query.expiry instanceof Date ? query.expiry.toISOString() : query.expiry;
    return this.request("GET", "/v1/instruments", {
      query: {
        underlying: query.underlying,
        option_kind: query.option_kind,
        expiry,
        family: query.family,
        offset: query.offset,
        limit: query.limit,
      },
      opts,
    });
  }

  // ---------------------------------------------------------------- pricing

  /** `POST /v1/quotes/compare`: quantity-aware cross-venue comparison (never retried). */
  compareQuotes(request: CompareRequest, opts?: RequestOptions): Promise<CompareResponse> {
    return this.request("POST", "/v1/quotes/compare", { body: request, opts });
  }

  /** `POST /v1/comparability/explain`: why two instruments are (not) comparable. */
  explainComparability(request: ExplainRequest, opts?: RequestOptions): Promise<ExplainResponse> {
    return this.request("POST", "/v1/comparability/explain", { body: request, opts });
  }

  /** `POST /v1/routes/preview`: single-destination preview, nothing is sent. */
  previewRoute(request: SimulateRequest, opts?: RequestOptions): Promise<RoutePreview> {
    return this.request("POST", "/v1/routes/preview", { body: request, opts });
  }

  /** `POST /v1/orders/simulate`: simulate the previewed route; the record is persisted. */
  simulateOrder(request: SimulateRequest, opts?: RequestOptions): Promise<SimulationRecord> {
    return this.request("POST", "/v1/orders/simulate", { body: request, opts });
  }

  /** `GET /v1/simulations/{id}`. */
  getSimulation(id: string, opts?: RequestOptions): Promise<SimulationRecord> {
    return this.request("GET", `/v1/simulations/${encodeURIComponent(id)}`, { opts });
  }

  // ---------------------------------------------------------------- routing & execution

  /**
   * `POST /v1/routes/quote`: executable BUY route (single venue or opt-in split).
   * Returns a single-use `route_id` + `route_hash` valid 30 s.
   */
  quoteRoute(request: RouteQuoteRequest, opts?: RequestOptions): Promise<RouteQuote> {
    return this.request("POST", "/v1/routes/quote", { body: request, opts });
  }

  /**
   * `POST /v1/routes/execute-testnet`: TESTNET only (403 EXECUTION_DISABLED otherwise).
   * Never retried automatically: a route can be consumed once.
   */
  executeTestnet(request: ExecuteRequest, opts?: RequestOptions): Promise<ExecutionReport> {
    return this.request("POST", "/v1/routes/execute-testnet", {
      body: { route_id: request.route_id, route_hash: request.route_hash },
      opts,
    });
  }

  /**
   * `POST /v1/noncustodial/prepare`: NON-CUSTODIAL step 1. Returns, per leg, the EIP-712 `typed_data`
   * the end user signs with `eth_signTypedData_v4`. Omni Strike holds no key. Consumes the route.
   */
  prepareNonCustodial(request: PrepareNonCustodialRequest, opts?: RequestOptions): Promise<PreparedExecution> {
    return this.request("POST", "/v1/noncustodial/prepare", { body: request, opts });
  }

  /**
   * `POST /v1/noncustodial/submit`: NON-CUSTODIAL step 2 (single use, never retried automatically).
   * `auth_signature` = personal_sign(String(auth_timestamp_ms)) by the same signer.
   */
  submitNonCustodial(request: SignedSubmission, opts?: RequestOptions): Promise<ExecutionReport> {
    return this.request("POST", "/v1/noncustodial/submit", { body: request, opts });
  }

  /**
   * `POST /v1/noncustodial/derive/subaccounts`: READ-ONLY, the user's Derive subaccount ids.
   * `signature` = personal_sign(String(timestamp_ms)) by `wallet` (fresh, < 2 min).
   */
  deriveSubaccounts(
    request: { wallet: string; timestamp_ms: number; signature: string },
    opts?: RequestOptions,
  ): Promise<{ wallet: string; network: string; chain_id: number; subaccount_ids: number[] }> {
    return this.request("POST", "/v1/noncustodial/derive/subaccounts", { body: request, opts });
  }

  /**
   * `POST /v1/noncustodial/orders`: the orders of ONE end user (the wallet declared at prepare time),
   * newest first. `signature` = personal_sign by `wallet` of `ordersMessage(wallet, timestamp_ms)`
   * (fresh, < 2 min).
   */
  myOrders(
    request: { wallet: string; timestamp_ms: number; signature: string; limit?: number },
    opts?: RequestOptions,
  ): Promise<{ wallet: string; mode: string; orders: Order[] }> {
    return this.request("POST", "/v1/noncustodial/orders", { body: request, opts });
  }

  /**
   * `POST /v1/noncustodial/derive/account`: READ-ONLY, the user's Derive subaccount (collaterals,
   * positions, open orders, value, margins) exactly as Derive returns it. Same auth as `myOrders`.
   */
  deriveAccount(
    request: { wallet: string; timestamp_ms: number; signature: string; subaccount_id: number },
    opts?: RequestOptions,
  ): Promise<{ wallet: string; network: string; chain_id: number; subaccount_id: number; account: Record<string, unknown> }> {
    return this.request("POST", "/v1/noncustodial/derive/account", { body: request, opts });
  }

  /** `GET /v1/orders`: order journal, newest first (TESTNET; private in MAINNET_NONCUSTODIAL). */
  listOrders(query: ListOrdersQuery = {}, opts?: RequestOptions): Promise<Order[]> {
    return this.request("GET", "/v1/orders", { query: { limit: query.limit }, opts });
  }

  /** `GET /v1/orders/{id}`. */
  getOrder(id: string, opts?: RequestOptions): Promise<Order> {
    return this.request("GET", `/v1/orders/${encodeURIComponent(id)}`, { opts });
  }

  /** `POST /v1/orders/{id}/cancel`: TESTNET, non-final orders only. */
  cancelOrder(id: string, opts?: RequestOptions): Promise<Order> {
    return this.request("POST", `/v1/orders/${encodeURIComponent(id)}/cancel`, { opts });
  }

  /** `GET /v1/balances`: real execution-venue balances (TESTNET); empty otherwise. */
  balances(opts?: RequestOptions): Promise<BalancesResponse> {
    return this.request("GET", "/v1/balances", { opts });
  }

  /** `GET /v1/exec-stats`: fill ratio, slippage and latency per venue. */
  execStats(opts?: RequestOptions): Promise<ExecStatsResponse> {
    return this.request("GET", "/v1/exec-stats", { opts });
  }

  /** `GET /v1/live/savings`: live "router vs single venue" snapshot (404 LIVE_TRACKER_OFF). */
  liveSavings(opts?: RequestOptions): Promise<LiveSavingsSnapshot> {
    return this.request("GET", "/v1/live/savings", { opts });
  }

  // ---------------------------------------------------------------- strategies

  /**
   * `POST /v1/strategies/quote`: multi-leg strategy (1..=6 legs) priced on every venue, with an
   * atomicity-aware plan (single venue within tolerance, else best split). Quote only, nothing is
   * sent; never retried. 422 `LIMIT_EXCEEDED` when your integrator limits refuse it.
   */
  quoteStrategy(request: StrategyRequest, opts?: RequestOptions): Promise<StrategyQuote> {
    return this.request("POST", "/v1/strategies/quote", { body: request, opts });
  }

  // ---------------------------------------------------------------- integrator self-service

  /** `GET /v1/integrator/me`: the calling integrator (needs an integrator API key, else 401). */
  me(opts?: RequestOptions): Promise<Integrator> {
    return this.request("GET", "/v1/integrator/me", { opts });
  }

  /**
   * `PUT /v1/integrator/settings`: replaces ALL your settings (send the full object, e.g. from
   * `me()`). Hard caps apply (422 `INVALID_REQUEST`). Not retried.
   */
  updateSettings(settings: IntegratorSettings, opts?: RequestOptions): Promise<Integrator> {
    return this.request("PUT", "/v1/integrator/settings", { body: settings, opts });
  }

  // ---------------------------------------------------------------- health

  /**
   * `GET /health/ready`. Resolves with the readiness body for both 200 (ready) and
   * 503 (not ready) instead of throwing; check `ready`.
   */
  healthReady(opts?: RequestOptions): Promise<Readiness> {
    return this.request("GET", "/health/ready", { opts, acceptStatus: [503], retry: false });
  }

  /** `GET /openapi.json`: the server's OpenAPI 3 document. */
  openapi(opts?: RequestOptions): Promise<Record<string, unknown>> {
    return this.request("GET", "/openapi.json", { opts });
  }

  // ---------------------------------------------------------------- transport

  /** Low-level call for endpoints not wrapped yet. GETs get the retry policy. */
  async request<T>(
    method: "GET" | "POST" | "PUT" | "PATCH" | "DELETE",
    path: string,
    init: {
      query?: Query;
      body?: unknown;
      opts?: RequestOptions;
      /** Non-2xx statuses whose JSON body is a valid result (e.g. 503 on /health/ready). */
      acceptStatus?: number[];
      retry?: boolean;
    } = {},
  ): Promise<T> {
    const url = this.buildUrl(path, init.query);
    const headers: Record<string, string> = { Accept: "application/json", ...this.headers };
    if (this.apiKey) headers["Authorization"] = `Bearer ${this.apiKey}`;
    let bodyText: string | undefined;
    if (init.body !== undefined) {
      headers["Content-Type"] = "application/json";
      bodyText = JSON.stringify(init.body, decimalSafeReplacer);
    }
    const retryable = method === "GET" && init.retry !== false;
    const attempts = retryable ? this.maxRetries + 1 : 1;

    for (let attempt = 0; ; attempt++) {
      const res = await this.once(method, url, headers, bodyText, init.opts);
      const ok = (res.status >= 200 && res.status < 300) || init.acceptStatus?.includes(res.status);
      if (ok) {
        return parseBody<T>(res);
      }
      const err = await toError(res);
      if (attempt + 1 < attempts && RETRYABLE.has(res.status)) {
        await this.sleep(this.backoff(attempt, err.retryAfterMs));
        continue;
      }
      throw err;
    }
  }

  private backoff(attempt: number, retryAfterMs: number | undefined): number {
    if (retryAfterMs !== undefined) return Math.min(retryAfterMs, this.maxRetryDelayMs);
    const exp = this.retryBaseDelayMs * 2 ** attempt;
    const jitter = exp * (0.5 + Math.random() * 0.5);
    return Math.min(Math.round(jitter), this.maxRetryDelayMs);
  }

  private async once(
    method: string,
    url: string,
    headers: Record<string, string>,
    body: string | undefined,
    opts: RequestOptions | undefined,
  ): Promise<Response> {
    const controller = new AbortController();
    const timeoutMs = opts?.timeoutMs ?? this.timeoutMs;
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      controller.abort();
    }, timeoutMs);
    const onAbort = () => controller.abort();
    if (opts?.signal) {
      if (opts.signal.aborted) controller.abort();
      else opts.signal.addEventListener("abort", onAbort, { once: true });
    }
    try {
      return await this.fetchImpl(url, { method, headers, body, signal: controller.signal });
    } catch (e) {
      if (timedOut) {
        throw new OmniStrikeError(0, "TIMEOUT", `${method} ${url} timed out after ${timeoutMs} ms`, { cause: e });
      }
      if (opts?.signal?.aborted) {
        throw new OmniStrikeError(0, "ABORTED", `${method} ${url} aborted by caller`, { cause: e });
      }
      throw new OmniStrikeError(0, "NETWORK_ERROR", `${method} ${url} failed: ${String((e as Error)?.message ?? e)}`, { cause: e });
    } finally {
      clearTimeout(timer);
      opts?.signal?.removeEventListener("abort", onAbort);
    }
  }

  private buildUrl(path: string, query?: Query): string {
    let url = this.baseUrl + path;
    if (query) {
      const params = new URLSearchParams();
      for (const [k, v] of Object.entries(query)) {
        if (v !== undefined && v !== null) params.append(k, String(v));
      }
      const qs = params.toString();
      if (qs) url += `?${qs}`;
    }
    return url;
  }
}

/** Refuses JS numbers with a fractional part in request bodies: decimals must be strings. */
function decimalSafeReplacer(this: unknown, key: string, value: unknown): unknown {
  if (typeof value === "number" && !Number.isInteger(value)) {
    throw new TypeError(
      `OmniStrike: field "${key}" is a float (${value}); pass decimals as strings, e.g. "${value}"`,
    );
  }
  if (typeof value === "bigint") return value.toString();
  return value;
}

async function parseBody<T>(res: Response): Promise<T> {
  const text = await res.text();
  if (text === "") return undefined as T;
  try {
    return JSON.parse(text) as T;
  } catch (e) {
    throw new OmniStrikeError(res.status, "INVALID_RESPONSE", `response is not JSON: ${text.slice(0, 200)}`, {
      body: text,
      cause: e,
    });
  }
}

async function toError(res: Response): Promise<OmniStrikeError> {
  const retryAfterMs = parseRetryAfter(res.headers.get("retry-after"));
  let text = "";
  try {
    text = await res.text();
  } catch {
    /* body unreadable: keep the status */
  }
  let body: unknown = text;
  try {
    body = text ? JSON.parse(text) : undefined;
  } catch {
    /* not JSON */
  }
  const env = (body as { error?: { code?: unknown; message?: unknown } } | undefined)?.error;
  if (env && typeof env.code === "string") {
    return new OmniStrikeError(res.status, env.code, typeof env.message === "string" ? env.message : "", {
      body,
      retryAfterMs,
    });
  }
  return new OmniStrikeError(res.status, `HTTP_${res.status}`, text.slice(0, 500) || res.statusText, {
    body,
    retryAfterMs,
  });
}

export interface OmniStrikeAdminOptions extends Omit<OmniStrikeOptions, "apiKey"> {
  /** Operator admin token (server `OA_ADMIN_TOKEN`). Never ship it to integrators. */
  adminToken: string;
}

/**
 * Operator client for `/v1/admin/*` (integrator provisioning). Separate from {@link OmniStrike}
 * so an integrator key and the admin token are never mixed. Same transport, errors and retry
 * policy (only the GET list is retried). 401 `UNAUTHORIZED` when admin is disabled on the server.
 */
export class OmniStrikeAdmin {
  private readonly client: OmniStrike;

  constructor(options: OmniStrikeAdminOptions) {
    if (!options?.adminToken) throw new Error("OmniStrikeAdmin: adminToken is required");
    const { adminToken, ...rest } = options;
    this.client = new OmniStrike({ ...rest, apiKey: adminToken });
  }

  get baseUrl(): string {
    return this.client.baseUrl;
  }

  /** `POST /v1/admin/integrators`: returns the integrator AND its API key (shown once). */
  createIntegrator(request: CreateIntegratorRequest, opts?: RequestOptions): Promise<CreatedIntegrator> {
    return this.client.request("POST", "/v1/admin/integrators", { body: request, opts });
  }

  /** `GET /v1/admin/integrators`. */
  listIntegrators(opts?: RequestOptions): Promise<Integrator[]> {
    return this.client.request("GET", "/v1/admin/integrators", { opts });
  }

  /** `PUT /v1/admin/integrators/{id}/settings`: full settings (admin may exceed self-service caps). */
  updateIntegratorSettings(id: string, settings: IntegratorSettings, opts?: RequestOptions): Promise<Integrator> {
    return this.client.request("PUT", `/v1/admin/integrators/${encodeURIComponent(id)}/settings`, {
      body: settings,
      opts,
    });
  }

  /** `PUT /v1/admin/integrators/{id}/active`: enable / disable a key (404 NOT_FOUND if unknown). */
  setIntegratorActive(id: string, active: boolean, opts?: RequestOptions): Promise<Integrator> {
    return this.client.request("PUT", `/v1/admin/integrators/${encodeURIComponent(id)}/active`, {
      body: { active },
      opts,
    });
  }
}
