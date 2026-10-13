// Quickstart: compare -> route quote -> (optional) execute on TESTNET.
//
//   cd sdk/typescript && npm run build
//   node examples/quickstart.mjs                      # read-only: compare + route quote
//   OMNI_STRIKE_EXECUTE=1 node examples/quickstart.mjs # also executes IF the server is in TESTNET
//
// Env: OMNI_STRIKE_BASE_URL (default http://127.0.0.1:8080), OMNI_STRIKE_API_KEY.
import { OmniStrike, OmniStrikeError } from "../dist/index.js";

const os = new OmniStrike({
  baseUrl: process.env.OMNI_STRIKE_BASE_URL ?? "http://127.0.0.1:8080",
  apiKey: process.env.OMNI_STRIKE_API_KEY,
});

try {
  // 1. Pick a contract: the most cross-listed BTC call expiring in more than a day.
  const { groups } = await os.instruments({ underlying: "BTC", option_kind: "call", limit: 1000 });
  const g = groups
    .filter((x) => Date.parse(x.expiry) > Date.now() + 86_400_000)
    .sort((a, b) => b.listings.length - a.listings.length)[0];
  if (!g) throw new Error("no contract listed");
  const intent = {
    underlying: "BTC", option_kind: "call", strike: g.strike, expiry: g.expiry,
    side: "buy", quantity: "0.1", // decimals are strings
  };

  // 2. Compare every covered venue for that exact contract and size.
  const cmp = await os.compareQuotes({ ...intent, latency_budget_ms: 1000 });
  console.log(`BTC ${g.strike} C ${g.expiry} x0.1 - ${cmp.wording}`);
  for (const q of cmp.ranked) {
    const e = q.estimate;
    console.log(`  #${cmp.ranked.indexOf(q) + 1} ${e.instrument.platform.padEnd(9)} total ${e.total_estimated} ${e.premium_currency}`);
  }
  for (const q of cmp.ineligible) {
    console.log(`  -- ${q.estimate.instrument.platform.padEnd(9)} ineligible: ${q.estimate.ineligible.join(", ")}`);
  }

  // 3. Executable route (opt-in split across up to 3 venues).
  const route = await os.quoteRoute({ ...intent, split: { allow_split: true, max_legs: 3 } });
  if (route.plan) {
    console.log(`route ${route.plan.kind}: expected total ${route.plan.expected_total}, legs:`);
    for (const l of route.plan.legs) {
      console.log(`  ${l.instrument.platform} ${l.qty_underlying} @ limit ${l.limit_price_native} (native)`);
    }
  } else {
    console.log(`no plan: ${route.no_plan_reason}`);
  }
  console.log(`executable=${route.executable} ${route.not_executable_reasons.join("; ")} (expires ${route.expires_at})`);

  // 4. Execute on TESTNET only, and only when explicitly asked.
  if (process.env.OMNI_STRIKE_EXECUTE === "1" && route.executable && route.mode === "TESTNET") {
    const report = await os.executeTestnet({ route_id: route.route_id, route_hash: route.route_hash });
    console.log(`filled ${report.filled_underlying}/${report.requested_underlying}`);
    for (const o of report.orders) console.log(`  ${o.platform} ${o.state} filled ${o.filled_native}`);
  }
} catch (e) {
  if (e instanceof OmniStrikeError) {
    console.error(`API error ${e.status} ${e.code}: ${e.serverMessage}`);
    process.exitCode = 1;
  } else {
    throw e;
  }
}
