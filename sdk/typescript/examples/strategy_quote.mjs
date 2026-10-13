// Quickstart: quote a BTC bull call spread (buy the lower strike, sell the higher, same expiry).
// Quote only, nothing is sent to any venue.
//
//   cd sdk/typescript && npm run build
//   node examples/strategy_quote.mjs
//
// Env: OMNI_STRIKE_BASE_URL (default http://127.0.0.1:8080), OMNI_STRIKE_API_KEY.
import { OmniStrike, OmniStrikeError } from "../dist/index.js";

const os = new OmniStrike({
  baseUrl: process.env.OMNI_STRIKE_BASE_URL ?? "http://127.0.0.1:8080",
  apiKey: process.env.OMNI_STRIKE_API_KEY,
});

try {
  // 1. Pick the expiry (> 1 day out) with the most cross-listed strikes, then two adjacent
  //    mid-chain strikes listed on at least 2 venues.
  const { groups } = await os.instruments({ underlying: "BTC", option_kind: "call", limit: 1000 });
  const byExpiry = new Map();
  for (const g of groups) {
    if (Date.parse(g.expiry) <= Date.now() + 86_400_000 || g.listings.length < 2) continue;
    byExpiry.set(g.expiry, [...(byExpiry.get(g.expiry) ?? []), g]);
  }
  const chain = [...byExpiry.values()].sort((a, b) => b.length - a.length)[0];
  if (!chain || chain.length < 2) throw new Error("no expiry with two cross-listed strikes");
  chain.sort((a, b) => Number(a.strike) - Number(b.strike)); // ordering only, not arithmetic
  const mid = Math.floor((chain.length - 1) / 2);
  const [low, high] = [chain[mid], chain[mid + 1]];

  // 2. Quote the spread: the server prices each leg on every venue, then picks ONE venue when it
  //    costs at most `single_venue_tolerance_bps` more than the best split.
  const q = await os.quoteStrategy({
    underlying: "BTC",
    quantity: "0.1", // decimals are strings; each leg = quantity x ratio
    legs: [
      { option_kind: "call", strike: low.strike, expiry: low.expiry, side: "buy" },
      { option_kind: "call", strike: high.strike, expiry: high.expiry, side: "sell" },
    ],
    single_venue_tolerance_bps: 10,
    latency_budget_ms: 3000,
  });

  console.log(`BTC ${low.strike}/${high.strike} call spread ${low.expiry} x0.1 (index ${q.index_price})`);
  for (const leg of q.legs) {
    const best = leg.quotes[0];
    console.log(`  leg ${leg.leg} ${leg.request.side} ${leg.request.strike}: ` +
      (best ? `best ${best.platform} ${best.amount} (${leg.quotes.length} venues)` : "no covered quote"));
  }
  if (q.plan) {
    const p = q.plan;
    console.log(`plan ${p.kind}: net ${p.net_cost} (${p.net_cost.startsWith("-") ? "credit" : "debit"}), ` +
      `best split ${p.best_split_net_cost}, best single venue ${p.best_single_venue?.join(" ") ?? "none"}, ` +
      `saving vs baseline ${p.saving_vs_baseline ?? "n/a"}`);
    for (const l of p.legs) console.log(`  leg ${l.leg} ${l.side} on ${l.quote.platform} (${l.quote.native_id}) ${l.quote.amount}`);
    console.log(`  ${p.reason}`);
  } else {
    console.log(`no plan: ${q.no_plan_reason}`);
  }
} catch (e) {
  if (e instanceof OmniStrikeError) {
    console.error(`API error ${e.status} ${e.code}: ${e.serverMessage}`);
    process.exitCode = 1;
  } else {
    throw e;
  }
}
