# @omni-strike/sdk (TypeScript)

Official TypeScript client for the **Omni Strike** crypto options best-execution API.
Zero runtime dependencies (global `fetch`), ESM + type declarations, Node >= 18 and browsers.

## Install / build

```bash
cd sdk/typescript
npm run build            # tsc -> dist/ (in this repo: ..\..\web\node_modules\.bin\tsc -p tsconfig.json)
```

## Quickstart: compare -> route quote -> execute (TESTNET)

```ts
import { OmniStrike, OmniStrikeError } from "@omni-strike/sdk";

const os = new OmniStrike({
  baseUrl: "http://127.0.0.1:8080",
  apiKey: process.env.OMNI_STRIKE_API_KEY, // sent as Authorization: Bearer <key>
});

const intent = {
  underlying: "BTC", option_kind: "call", strike: "120000",
  expiry: "2026-12-25T08:00:00Z", side: "buy", quantity: "0.5", // decimals are STRINGS
} as const;

// 1. Compare every covered venue for this exact contract and size
const cmp = await os.compareQuotes({ ...intent, latency_budget_ms: 500 });
const best = cmp.ranked[0]?.estimate;
console.log(best?.instrument.platform, best?.total_estimated, best?.premium_currency);

// 2. Executable route (opt-in split over up to 3 venues), valid 30 s
const route = await os.quoteRoute({ ...intent, split: { allow_split: true, max_legs: 3 } });

// 3. Execute - TESTNET servers only (403 EXECUTION_DISABLED elsewhere), single use
if (route.executable) {
  try {
    const report = await os.executeTestnet({ route_id: route.route_id, route_hash: route.route_hash });
    console.log(report.filled_underlying, report.orders.map((o) => o.state));
  } catch (e) {
    if (e instanceof OmniStrikeError && e.code === "ROUTE_INVALID") { /* expired: quote again */ }
    else throw e;
  }
}
```

Runnable version: [`examples/quickstart.mjs`](examples/quickstart.mjs).

## Multi-leg strategies: bull call spread

```ts
const q = await os.quoteStrategy({
  underlying: "BTC",
  quantity: "0.1", // each leg = quantity x ratio (ratio defaults to "1")
  legs: [
    { option_kind: "call", strike: "88000", expiry: "2026-12-25T08:00:00Z", side: "buy" },
    { option_kind: "call", strike: "90000", expiry: "2026-12-25T08:00:00Z", side: "sell" },
  ],
  single_venue_tolerance_bps: 10, // prefer ONE venue if within 10 bps of notional of the best split
});
if (q.plan) {
  // net_cost > 0 = debit (you pay), < 0 = credit. kind: "SINGLE_VENUE" | "SPLIT"
  console.log(q.plan.kind, q.plan.net_cost, q.plan.best_split_net_cost, q.plan.best_single_venue);
  for (const l of q.plan.legs) console.log(l.leg, l.side, l.quote.platform, l.quote.amount);
} else {
  console.log("no plan:", q.no_plan_reason);
}
```

Quote only (nothing is sent). Runnable version: [`examples/strategy_quote.mjs`](examples/strategy_quote.mjs).

## Integrator settings and admin

```ts
const me = await os.me();                       // GET /v1/integrator/me (needs an integrator key)
await os.updateSettings({ ...me.settings, max_slippage_bps: 25 }); // PUT: always the FULL object

// Operator side: separate class, admin token (server OA_ADMIN_TOKEN), never shipped to integrators
import { OmniStrikeAdmin } from "@omni-strike/sdk";
const admin = new OmniStrikeAdmin({ baseUrl, adminToken: process.env.OMNI_STRIKE_ADMIN_TOKEN! });
const { integrator, api_key } = await admin.createIntegrator({ name: "acme" }); // key shown ONCE
await admin.setIntegratorActive(integrator.id, false);
```

## API coverage

| Method | Endpoint |
|---|---|
| `platforms()` | `GET /v1/platforms` |
| `instruments({underlying, option_kind?, expiry?, family?, offset?, limit?})` | `GET /v1/instruments` |
| `compareQuotes(req)` | `POST /v1/quotes/compare` (`family: "dated_option" \| "binary"`, `accepted_differences`, `latency_budget_ms`, `max_age_ms`, ...) |
| `quoteRoute(req)` | `POST /v1/routes/quote` (compare fields + `split` policy) |
| `executeTestnet({route_id, route_hash})` | `POST /v1/routes/execute-testnet` |
| `listOrders({limit?})`, `getOrder(id)`, `cancelOrder(id)` | `GET /v1/orders`, `GET /v1/orders/{id}`, `POST /v1/orders/{id}/cancel` |
| `balances()`, `execStats()`, `liveSavings()` | `GET /v1/balances`, `/v1/exec-stats`, `/v1/live/savings` |
| `quoteStrategy(req)` | `POST /v1/strategies/quote` (1..=6 legs, `single_venue_tolerance_bps`, `baseline_platform`, ...) |
| `me()`, `updateSettings(settings)` | `GET /v1/integrator/me`, `PUT /v1/integrator/settings` |
| `OmniStrikeAdmin`: `createIntegrator`, `listIntegrators`, `updateIntegratorSettings`, `setIntegratorActive` | `POST`/`GET /v1/admin/integrators`, `PUT /v1/admin/integrators/{id}/settings`, `PUT /v1/admin/integrators/{id}/active` |
| `healthReady()`, `openapi()` | `GET /health/ready` (resolves on 503 too, check `ready`), `GET /openapi.json` |
| `explainComparability`, `previewRoute`, `simulateOrder`, `getSimulation` | comparability / simulation endpoints |
| `request(method, path, {query, body})` | escape hatch for endpoints not wrapped yet |

## Behaviour

- **Decimals** are JSON strings in both directions (`type Decimal = string`); the SDK never turns
  them into JS numbers. A float in a request body (`quantity: 0.5`) throws a `TypeError` before
  sending. Use a decimal library for arithmetic.
- **Errors**: every failure is an `OmniStrikeError` with `status`, `code`, `message`/`serverMessage`,
  `body`, `retryAfterMs`. Server errors come from the envelope `{"error":{"code","message"}}`
  (`INVALID_REQUEST`, `NOT_FOUND`, `SERVICE_BUSY`, `UNAUTHORIZED`, `EXECUTION_DISABLED`,
  `ROUTE_INVALID`, `RISK_REJECTED`, `LIVE_TRACKER_OFF`, `RATE_LIMITED` (429), `LIMIT_EXCEEDED`
  (422, your integrator limits), ...); a non-envelope body gives
  `HTTP_<status>`. Client-side: `status: 0` with `TIMEOUT`, `NETWORK_ERROR`, `ABORTED`, `INVALID_RESPONSE`.
- **Timeouts**: `timeoutMs` per attempt (default 30 s) via `AbortController`; per-call
  `{ timeoutMs, signal }` overrides.
- **Retries**: only idempotent `GET`s, only on HTTP 429/503, `maxRetries` (default 2), exponential
  backoff with jitter from `retryBaseDelayMs` (250 ms), `Retry-After` honoured (seconds or HTTP
  date), every wait capped by `maxRetryDelayMs` (10 s). POSTs and PUTs (compare, route/strategy
  quote, execute, cancel, settings) are **never** retried automatically.
- `Order.salt` is a u64 on the server and may lose precision in `JSON.parse`; it is informational.

## Tests

```bash
npm run build
node --test test/*.test.mjs                                  # unit tests, fake transport
OMNI_STRIKE_BASE_URL=http://127.0.0.1:8080 node test/integration.mjs   # READ-ONLY live calls
```

The integration script only calls health, platforms, instruments, compare (valid + invalid), live
savings, a bull-call-spread strategy quote, `integrator/me` (typed 401 without a key) and, with
`OMNI_STRIKE_ADMIN_TOKEN`, the admin list (skipped otherwise). It never calls execute, cancel or
any admin write.
