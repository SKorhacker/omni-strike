// Unit tests with a fake transport (no network). Run: npm run build && node --test test/
import { test } from "node:test";
import assert from "node:assert/strict";
import { OmniStrike, OmniStrikeAdmin, OmniStrikeError, ordersMessage, parseRetryAfter } from "../dist/index.js";

function fakeFetch(responses) {
  const calls = [];
  const queue = [...responses];
  const fn = async (url, init) => {
    calls.push({ url, init });
    const next = queue.shift();
    if (!next) throw new Error("fake fetch: no more responses");
    if (next instanceof Error) throw next;
    if (typeof next === "function") return next(url, init);
    const { status = 200, body, headers = {} } = next;
    return new Response(typeof body === "string" ? body : JSON.stringify(body), {
      status,
      headers: { "content-type": "application/json", ...headers },
    });
  };
  return { fn, calls };
}

function client(fetchFn, extra = {}) {
  const sleeps = [];
  const c = new OmniStrike({
    baseUrl: "http://api.test/",
    apiKey: "key_123",
    fetch: fetchFn,
    sleep: async (ms) => { sleeps.push(ms); },
    ...extra,
  });
  return { c, sleeps };
}

test("sends bearer key, builds query string, keeps decimals as strings", async () => {
  const f = fakeFetch([{ body: { underlying: "BTC", groups: [{ strike: "65000.5", option_kind: "call" }] } }]);
  const { c } = client(f.fn);
  const res = await c.instruments({ underlying: "BTC", option_kind: "call", limit: 5, expiry: new Date("2026-12-25T08:00:00Z") });
  const { url, init } = f.calls[0];
  assert.equal(init.method, "GET");
  assert.equal(init.headers.Authorization, "Bearer key_123");
  const u = new URL(url);
  assert.equal(u.origin + u.pathname, "http://api.test/v1/instruments");
  assert.equal(u.searchParams.get("underlying"), "BTC");
  assert.equal(u.searchParams.get("limit"), "5");
  assert.equal(u.searchParams.get("expiry"), "2026-12-25T08:00:00.000Z");
  assert.equal(u.searchParams.has("offset"), false);
  assert.equal(res.groups[0].strike, "65000.5");
  assert.equal(typeof res.groups[0].strike, "string");
});

test("no Authorization header without api key", async () => {
  const f = fakeFetch([{ body: { mode: "READ_ONLY", platforms: [] } }]);
  const c = new OmniStrike({ baseUrl: "http://api.test", fetch: f.fn });
  await c.platforms();
  assert.equal(f.calls[0].init.headers.Authorization, undefined);
});

test("compare posts JSON body, omits undefined fields", async () => {
  const f = fakeFetch([{ body: { request_id: "r", ranked: [] } }]);
  const { c } = client(f.fn);
  await c.compareQuotes({
    underlying: "BTC", option_kind: "call", strike: "120000", expiry: "2026-12-25T08:00:00Z",
    side: "buy", quantity: "0.5", accepted_differences: ["SETTLEMENT_INDEX_DIFFERS"],
    family: "dated_option", latency_budget_ms: 500, max_age_ms: 5000, baseline_platform: undefined,
  });
  const { url, init } = f.calls[0];
  assert.equal(url, "http://api.test/v1/quotes/compare");
  assert.equal(init.method, "POST");
  assert.equal(init.headers["Content-Type"], "application/json");
  const body = JSON.parse(init.body);
  assert.equal(body.strike, "120000");
  assert.equal(body.quantity, "0.5");
  assert.deepEqual(body.accepted_differences, ["SETTLEMENT_INDEX_DIFFERS"]);
  assert.equal("baseline_platform" in body, false);
});

test("float decimals in a request body are refused", async () => {
  const f = fakeFetch([]);
  const { c } = client(f.fn);
  await assert.rejects(
    c.compareQuotes({ underlying: "BTC", option_kind: "call", strike: "1", expiry: "x", side: "buy", quantity: 0.5 }),
    /pass decimals as strings/,
  );
  assert.equal(f.calls.length, 0);
});

test("route quote flattens compare fields and nests split", async () => {
  const f = fakeFetch([{ body: { route_id: "id", route_hash: "h", executable: false } }]);
  const { c } = client(f.fn);
  await c.quoteRoute({
    underlying: "ETH", option_kind: "put", strike: "3000", expiry: "2026-12-25T08:00:00Z",
    side: "buy", quantity: "2", split: { allow_split: true, max_legs: 3, min_saving_bps: "5" },
  });
  const body = JSON.parse(f.calls[0].init.body);
  assert.equal(f.calls[0].url, "http://api.test/v1/routes/quote");
  assert.equal(body.underlying, "ETH");
  assert.deepEqual(body.split, { allow_split: true, max_legs: 3, min_saving_bps: "5" });
});

test("execute-testnet sends only route_id + route_hash and is never retried", async () => {
  const f = fakeFetch([{ status: 503, body: { error: { code: "SERVICE_BUSY", message: "busy" } } }]);
  const { c, sleeps } = client(f.fn);
  const err = await c.executeTestnet({ route_id: "abc", route_hash: "h", extra: 1 }).catch((e) => e);
  assert.ok(err instanceof OmniStrikeError);
  assert.equal(err.status, 503);
  assert.equal(err.code, "SERVICE_BUSY");
  assert.equal(f.calls.length, 1);
  assert.equal(sleeps.length, 0);
  assert.deepEqual(JSON.parse(f.calls[0].init.body), { route_id: "abc", route_hash: "h" });
});

test("error envelope becomes typed OmniStrikeError", async () => {
  const f = fakeFetch([{ status: 422, body: { error: { code: "INVALID_REQUEST", message: "strike must be > 0" } } }]);
  const { c } = client(f.fn);
  const err = await c.compareQuotes({ underlying: "BTC", option_kind: "call", strike: "0", expiry: "x", side: "buy", quantity: "1" }).catch((e) => e);
  assert.ok(err instanceof OmniStrikeError);
  assert.equal(err.status, 422);
  assert.equal(err.code, "INVALID_REQUEST");
  assert.equal(err.serverMessage, "strike must be > 0");
  assert.equal(err.isRetryable, false);
});

test("non-envelope error body -> HTTP_<status>", async () => {
  const f = fakeFetch([{ status: 502, body: "Bad Gateway", headers: { "content-type": "text/plain" } }]);
  const { c } = client(f.fn);
  const err = await c.platforms().catch((e) => e);
  assert.equal(err.code, "HTTP_502");
  assert.equal(err.status, 502);
  assert.equal(f.calls.length, 1, "502 is not retried");
});

test("GET retries 429/503 with Retry-After, then succeeds", async () => {
  const f = fakeFetch([
    { status: 429, body: { error: { code: "RATE_LIMITED", message: "slow down" } }, headers: { "retry-after": "2" } },
    { status: 503, body: { error: { code: "SERVICE_BUSY", message: "busy" } } },
    { body: { mode: "READ_ONLY", platforms: [] } },
  ]);
  const { c, sleeps } = client(f.fn, { retryBaseDelayMs: 100 });
  const res = await c.platforms();
  assert.equal(res.mode, "READ_ONLY");
  assert.equal(f.calls.length, 3);
  assert.equal(sleeps[0], 2000, "Retry-After honoured");
  assert.ok(sleeps[1] >= 100 && sleeps[1] <= 200, `backoff with jitter, got ${sleeps[1]}`);
});

test("GET gives up after maxRetries", async () => {
  const busy = { status: 503, body: { error: { code: "SERVICE_BUSY", message: "busy" } } };
  const f = fakeFetch([busy, busy, busy]);
  const { c, sleeps } = client(f.fn, { maxRetries: 2 });
  const err = await c.liveSavings().catch((e) => e);
  assert.equal(err.code, "SERVICE_BUSY");
  assert.equal(f.calls.length, 3);
  assert.equal(sleeps.length, 2);
});

test("Retry-After is capped by maxRetryDelayMs", async () => {
  const f = fakeFetch([
    { status: 429, body: {}, headers: { "retry-after": "3600" } },
    { body: [] },
  ]);
  const { c, sleeps } = client(f.fn, { maxRetryDelayMs: 1500 });
  await c.listOrders({ limit: 10 });
  assert.equal(sleeps[0], 1500);
  assert.ok(f.calls[1].url.endsWith("/v1/orders?limit=10"));
});

test("POST compare is not retried on 503", async () => {
  const f = fakeFetch([{ status: 503, body: { error: { code: "SERVICE_BUSY", message: "x" } } }]);
  const { c } = client(f.fn);
  await assert.rejects(c.compareQuotes({ underlying: "BTC", option_kind: "call", strike: "1", expiry: "x", side: "buy", quantity: "1" }));
  assert.equal(f.calls.length, 1);
});

test("healthReady returns the body on 503 (not ready)", async () => {
  const f = fakeFetch([{ status: 503, body: { ready: false, database: "down", venues: [], mode: "READ_ONLY" } }]);
  const { c } = client(f.fn);
  const r = await c.healthReady();
  assert.equal(r.ready, false);
  assert.equal(f.calls.length, 1);
});

test("timeout -> TIMEOUT error", async () => {
  const hang = (_url, init) => new Promise((_, reject) => {
    init.signal.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")));
  });
  const f = fakeFetch([hang]);
  const { c } = client(f.fn, { timeoutMs: 20 });
  const err = await c.platforms().catch((e) => e);
  assert.equal(err.code, "TIMEOUT");
  assert.equal(err.status, 0);
});

test("caller abort -> ABORTED; network failure -> NETWORK_ERROR", async () => {
  const hang = (_url, init) => new Promise((_, reject) => {
    init.signal.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")));
  });
  const f = fakeFetch([hang, new TypeError("fetch failed")]);
  const { c } = client(f.fn);
  const ac = new AbortController();
  const p = c.balances({ signal: ac.signal });
  ac.abort();
  assert.equal((await p.catch((e) => e)).code, "ABORTED");
  assert.equal((await c.execStats().catch((e) => e)).code, "NETWORK_ERROR");
});

test("path ids are URL-encoded; cancel is a POST", async () => {
  const f = fakeFetch([{ body: { id: "a/b" } }, { body: { id: "x" } }]);
  const { c } = client(f.fn);
  await c.getOrder("a/b");
  await c.cancelOrder("00000000-0000-0000-0000-000000000000");
  assert.equal(f.calls[0].url, "http://api.test/v1/orders/a%2Fb");
  assert.equal(f.calls[1].init.method, "POST");
  assert.equal(f.calls[1].url, "http://api.test/v1/orders/00000000-0000-0000-0000-000000000000/cancel");
});

test("parseRetryAfter handles seconds and HTTP dates", () => {
  assert.equal(parseRetryAfter("1.5"), 1500);
  assert.equal(parseRetryAfter(null), undefined);
  assert.equal(parseRetryAfter("garbage"), undefined);
  const now = Date.parse("2026-10-07T00:00:00Z");
  assert.equal(parseRetryAfter("Wed, 07 Oct 2026 00:00:03 GMT", now), 3000);
});

// ------------------------------------------------------------------ strategies / integrators / admin

const SETTINGS = {
  max_notional_per_order_usd: "250000", max_notional_per_day_usd: "2000000",
  allowed_platforms: ["derive", "aevo"], allowed_underlyings: null, max_slippage_bps: 50,
  unwind_cost_cap_bps: 30, single_venue_tolerance_bps: 10, simulation_first: true, rate_limit_rps: 20,
};
const INTEGRATOR = {
  id: "11111111-1111-1111-1111-111111111111", name: "acme", key_prefix: "os_1a2b3c4d",
  settings: SETTINGS, active: true, created_at: "2026-10-07T00:00:00Z", updated_at: "2026-10-07T00:00:00Z",
};

test("quoteStrategy posts the legs and keeps plan decimals as strings", async () => {
  const f = fakeFetch([{ body: {
    computed_at: "2026-10-07T00:00:00Z", wording: "w", request: {}, no_plan_reason: null, notes: [],
    index_price: "62000.5", single_venue_tolerance: "12.4",
    plan: { kind: "SINGLE_VENUE", legs: [{ leg: 0, side: "buy", quote: { platform: "derive", native_id: "BTC-C", amount: "1200.5" } }],
      net_cost: "-35.25", best_split_net_cost: "-40.1", best_single_venue: ["derive", "-35.25"],
      saving_vs_baseline: "0", reason: "r" },
    legs: [],
  } }]);
  const { c } = client(f.fn);
  const q = await c.quoteStrategy({
    underlying: "BTC", quantity: "0.5", single_venue_tolerance_bps: 20,
    legs: [
      { option_kind: "call", strike: "60000", expiry: "2026-12-25T08:00:00Z", side: "buy" },
      { option_kind: "call", strike: "65000", expiry: "2026-12-25T08:00:00Z", side: "sell", ratio: "1" },
    ],
  });
  const { url, init } = f.calls[0];
  assert.equal(url, "http://api.test/v1/strategies/quote");
  assert.equal(init.method, "POST");
  const body = JSON.parse(init.body);
  assert.equal(body.quantity, "0.5");
  assert.equal(body.legs.length, 2);
  assert.equal(body.legs[1].side, "sell");
  assert.equal(body.legs[1].ratio, "1");
  assert.equal(q.plan.kind, "SINGLE_VENUE");
  assert.equal(q.plan.net_cost, "-35.25");
  assert.deepEqual(q.plan.best_single_venue, ["derive", "-35.25"]);
});

test("quoteStrategy: 422 LIMIT_EXCEEDED is typed and never retried", async () => {
  const f = fakeFetch([{ status: 422, body: { error: { code: "LIMIT_EXCEEDED", message: "notional above cap" } } }]);
  const { c, sleeps } = client(f.fn);
  const err = await c.quoteStrategy({ underlying: "BTC", quantity: "100", legs: [] }).catch((e) => e);
  assert.ok(err instanceof OmniStrikeError);
  assert.equal(err.status, 422);
  assert.equal(err.code, "LIMIT_EXCEEDED");
  assert.equal(f.calls.length, 1);
  assert.equal(sleeps.length, 0);
});

test("quoteStrategy refuses float decimals in legs", async () => {
  const f = fakeFetch([]);
  const { c } = client(f.fn);
  await assert.rejects(
    c.quoteStrategy({ underlying: "BTC", quantity: "1", legs: [{ option_kind: "call", strike: "1", expiry: "x", side: "buy", ratio: 1.5 }] }),
    /pass decimals as strings/,
  );
  assert.equal(f.calls.length, 0);
});

test("me() GETs /v1/integrator/me and retries 429 honouring Retry-After", async () => {
  const f = fakeFetch([
    { status: 429, body: { error: { code: "RATE_LIMITED", message: "rate limit 20 req/s exceeded" } }, headers: { "retry-after": "1" } },
    { body: INTEGRATOR },
  ]);
  const { c, sleeps } = client(f.fn);
  const me = await c.me();
  assert.equal(f.calls[1].url, "http://api.test/v1/integrator/me");
  assert.equal(f.calls[1].init.method, "GET");
  assert.equal(sleeps[0], 1000);
  assert.equal(me.settings.max_notional_per_order_usd, "250000");
});

test("me() without integrator key -> typed 401", async () => {
  const f = fakeFetch([{ status: 401, body: { error: { code: "UNAUTHORIZED", message: "integrator API key required" } } }]);
  const { c } = client(f.fn);
  const err = await c.me().catch((e) => e);
  assert.equal(err.status, 401);
  assert.equal(err.code, "UNAUTHORIZED");
  assert.equal(f.calls.length, 1);
});

test("updateSettings PUTs the full settings and is not retried on 429", async () => {
  const f = fakeFetch([
    { body: INTEGRATOR },
    { status: 429, body: { error: { code: "RATE_LIMITED", message: "x" } }, headers: { "retry-after": "1" } },
  ]);
  const { c, sleeps } = client(f.fn);
  await c.updateSettings({ ...SETTINGS, max_slippage_bps: 25 });
  assert.equal(f.calls[0].url, "http://api.test/v1/integrator/settings");
  assert.equal(f.calls[0].init.method, "PUT");
  assert.deepEqual(JSON.parse(f.calls[0].init.body), { ...SETTINGS, max_slippage_bps: 25 });
  const err = await c.updateSettings(SETTINGS).catch((e) => e);
  assert.equal(err.code, "RATE_LIMITED");
  assert.equal(f.calls.length, 2);
  assert.equal(sleeps.length, 0);
});

test("OmniStrikeAdmin: bearer admin token, create / list / settings / active", async () => {
  const f = fakeFetch([
    { body: { integrator: INTEGRATOR, api_key: "os_secret" } },
    { body: [INTEGRATOR] },
    { body: INTEGRATOR },
    { body: { ...INTEGRATOR, active: false } },
  ]);
  const a = new OmniStrikeAdmin({ baseUrl: "http://api.test", adminToken: "adm", fetch: f.fn });
  const created = await a.createIntegrator({ name: "acme" });
  assert.equal(created.api_key, "os_secret");
  const list = await a.listIntegrators();
  assert.equal(list.length, 1);
  await a.updateIntegratorSettings("id/1", SETTINGS);
  const off = await a.setIntegratorActive(INTEGRATOR.id, false);
  assert.equal(off.active, false);
  assert.deepEqual(f.calls.map((x) => [x.init.method, x.url]), [
    ["POST", "http://api.test/v1/admin/integrators"],
    ["GET", "http://api.test/v1/admin/integrators"],
    ["PUT", "http://api.test/v1/admin/integrators/id%2F1/settings"],
    ["PUT", `http://api.test/v1/admin/integrators/${INTEGRATOR.id}/active`],
  ]);
  for (const x of f.calls) assert.equal(x.init.headers.Authorization, "Bearer adm");
  assert.deepEqual(JSON.parse(f.calls[0].init.body), { name: "acme" });
  assert.deepEqual(JSON.parse(f.calls[3].init.body), { active: false });
});

test("OmniStrikeAdmin requires an admin token", () => {
  assert.throws(() => new OmniStrikeAdmin({ adminToken: "" }), /adminToken is required/);
});

test("ordersMessage matches the server format", () => {
  assert.equal(
    ordersMessage("0xABCdef0000000000000000000000000000000001", 1791000000000),
    "Omni Strike: read my orders\nWallet: 0xabcdef0000000000000000000000000000000001\nTimestamp: 1791000000000",
  );
});
