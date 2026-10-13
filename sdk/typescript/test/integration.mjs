// READ-ONLY integration check against a running Omni Strike server.
// Never calls execute / cancel / admin-write endpoints.
//
//   npm run build && node test/integration.mjs
//   OMNI_STRIKE_BASE_URL=http://127.0.0.1:8080 OMNI_STRIKE_API_KEY=... node test/integration.mjs
import assert from "node:assert/strict";
import { OmniStrike, OmniStrikeAdmin, OmniStrikeError } from "../dist/index.js";

const baseUrl = process.env.OMNI_STRIKE_BASE_URL ?? "http://127.0.0.1:8080";
const os = new OmniStrike({ baseUrl, apiKey: process.env.OMNI_STRIKE_API_KEY, timeoutMs: 30_000 });
const isDecimal = (v) => typeof v === "string" && /^-?\d+(\.\d+)?$/.test(v);
let failures = 0;

async function step(name, fn) {
  const t = Date.now();
  try {
    const info = await fn();
    console.log(`PASS ${name} (${Date.now() - t} ms)${info ? ` - ${info}` : ""}`);
  } catch (e) {
    failures++;
    console.log(`FAIL ${name}: ${e instanceof OmniStrikeError ? `${e.status} ${e.code} ${e.serverMessage}` : e.stack}`);
  }
}

let target;

await step("health/ready", async () => {
  const r = await os.healthReady();
  return `ready=${r.ready} mode=${r.mode} venues=${r.venues.length}`;
});

await step("GET /v1/platforms", async () => {
  const r = await os.platforms();
  assert.ok(Array.isArray(r.platforms) && r.platforms.length > 0);
  const integrated = r.platforms.filter((p) => p.implemented.length > 0).map((p) => p.id);
  return `mode=${r.mode} platforms=${r.platforms.length} integrated=[${integrated.join(",")}]`;
});

await step("GET /v1/instruments", async () => {
  const r = await os.instruments({ underlying: "BTC", option_kind: "call", limit: 1000 });
  assert.ok(Array.isArray(r.groups));
  for (const g of r.groups.slice(0, 20)) assert.ok(isDecimal(g.strike), `strike is a decimal string: ${g.strike}`);
  // Most cross-listed contract with the nearest expiry at least 1 day out.
  const soon = Date.now() + 86_400_000;
  target = r.groups
    .filter((g) => Date.parse(g.expiry) > soon)
    .sort((a, b) => b.listings.length - a.listings.length || Date.parse(a.expiry) - Date.parse(b.expiry))[0];
  return `total_groups=${r.total_groups} returned=${r.groups.length} target=${target ? `${target.strike} ${target.expiry} x${target.listings.length}` : "none"}`;
});

await step("POST /v1/quotes/compare", async () => {
  assert.ok(target, "no instrument to compare");
  const r = await os.compareQuotes({
    underlying: "BTC", option_kind: "call", strike: target.strike, expiry: target.expiry,
    side: "buy", quantity: "0.1", latency_budget_ms: 3000,
  });
  assert.ok(r.request_id);
  for (const q of [...r.ranked, ...r.incomplete]) {
    assert.ok(isDecimal(q.estimate.premium), `premium decimal: ${q.estimate.premium}`);
  }
  const best = r.ranked[0];
  return `ranked=${r.ranked.length} incomplete=${r.incomplete.length} ineligible=${r.ineligible.length} unavailable=${r.unavailable.length}` +
    (best ? ` best=${best.estimate.instrument.platform} total=${best.estimate.total_estimated} ${best.estimate.premium_currency}` : "");
});

await step("POST /v1/quotes/compare invalid -> typed error", async () => {
  const err = await os
    .compareQuotes({ underlying: "DOGE", option_kind: "call", strike: "1", expiry: "2026-12-25T08:00:00Z", side: "buy", quantity: "1" })
    .catch((e) => e);
  assert.ok(err instanceof OmniStrikeError, String(err));
  assert.equal(err.status, 422);
  return `${err.status} ${err.code}: ${err.serverMessage}`;
});

await step("GET /v1/live/savings", async () => {
  try {
    const r = await os.liveSavings();
    return `basket=${r.basket_size} observations=${r.summary.observations} win_rate=${r.summary.win_rate}`;
  } catch (e) {
    if (e instanceof OmniStrikeError && e.code === "LIVE_TRACKER_OFF") return "tracker off on this server (404, typed)";
    if (e instanceof OmniStrikeError && e.code === "HTTP_404") return "SKIPPED: route not deployed on this server build (plain 404)";
    throw e;
  }
});

await step("POST /v1/strategies/quote (bull call spread)", async () => {
  const { groups } = await os.instruments({ underlying: "BTC", option_kind: "call", limit: 1000 });
  const soon = Date.now() + 86_400_000;
  const byExpiry = new Map();
  for (const g of groups) {
    if (Date.parse(g.expiry) <= soon || g.listings.length < 2) continue;
    byExpiry.set(g.expiry, [...(byExpiry.get(g.expiry) ?? []), g]);
  }
  const chain = [...byExpiry.values()].sort((a, b) => b.length - a.length)[0];
  assert.ok(chain && chain.length >= 2, "no expiry with two cross-listed strikes");
  chain.sort((a, b) => Number(a.strike) - Number(b.strike));
  const mid = Math.floor((chain.length - 1) / 2);
  const [lo, hi] = [chain[mid], chain[mid + 1]];
  const q = await os.quoteStrategy({
    underlying: "BTC", quantity: "0.1", latency_budget_ms: 3000,
    legs: [
      { option_kind: "call", strike: lo.strike, expiry: lo.expiry, side: "buy" },
      { option_kind: "call", strike: hi.strike, expiry: hi.expiry, side: "sell" },
    ],
  });
  assert.equal(q.legs.length, 2);
  assert.ok(isDecimal(q.single_venue_tolerance), `tolerance decimal: ${q.single_venue_tolerance}`);
  for (const l of q.legs) {
    assert.ok(isDecimal(l.quantity));
    for (const lq of l.quotes) assert.ok(isDecimal(lq.amount), `leg amount decimal: ${lq.amount}`);
  }
  if (q.plan) {
    assert.ok(["SINGLE_VENUE", "SPLIT"].includes(q.plan.kind));
    assert.ok(isDecimal(q.plan.net_cost) && isDecimal(q.plan.best_split_net_cost));
    if (q.plan.best_single_venue) assert.ok(isDecimal(q.plan.best_single_venue[1]));
  }
  const quotes = q.legs.map((l) => l.quotes.length).join("/");
  return `${lo.strike}/${hi.strike} ${lo.expiry} quotes=${quotes} ` +
    (q.plan ? `plan=${q.plan.kind} net=${q.plan.net_cost} split=${q.plan.best_split_net_cost} single=${q.plan.best_single_venue?.join(":") ?? "none"}`
      : `no plan: ${q.no_plan_reason}`);
});

await step("GET /v1/integrator/me", async () => {
  try {
    const me = await os.me();
    assert.ok(isDecimal(me.settings.max_notional_per_order_usd));
    return `integrator=${me.name} active=${me.active} rps=${me.settings.rate_limit_rps}`;
  } catch (e) {
    if (e instanceof OmniStrikeError && e.status === 401 && e.code === "UNAUTHORIZED") {
      return `no integrator key -> typed 401 UNAUTHORIZED (${e.serverMessage})`;
    }
    throw e;
  }
});

await step("GET /v1/admin/integrators (read-only)", async () => {
  const token = process.env.OMNI_STRIKE_ADMIN_TOKEN;
  if (!token) {
    // Unauthenticated probe: must be a typed 401 (admin disabled or token required).
    const err = await new OmniStrike({ baseUrl }).request("GET", "/v1/admin/integrators").catch((e) => e);
    assert.ok(err instanceof OmniStrikeError && err.status === 401, String(err));
    return `SKIPPED (no OMNI_STRIKE_ADMIN_TOKEN); unauthenticated -> typed 401 ${err.code}`;
  }
  const list = await new OmniStrikeAdmin({ baseUrl, adminToken: token }).listIntegrators();
  return `integrators=${list.length}`;
});

console.log(failures ? `\n${failures} step(s) failed` : "\nall read-only integration steps passed");
// exitCode instead of process.exit(): lets keep-alive sockets close cleanly (libuv assert on Windows).
process.exitCode = failures ? 1 : 0;
