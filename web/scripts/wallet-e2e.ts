// End-to-end check of the page's non-custodial wallet flow (lib/noncustodial.ts + lib/wallet.ts)
// against a running TESTNET server, with a mock EIP-1193 wallet backed by a THROWAWAY key
// (signatures by eth_account, an independent implementation). No funds anywhere: the throwaway
// wallet has no Derive subaccount, so the venue refuses after our server accepted the signatures.
//
//   node --experimental-transform-types scripts/wallet-e2e.ts [http://127.0.0.1:8081]
//
// Expected: subaccount lookup answers (empty list), prepare OK, signatures verified by the server,
// then Derive testnet rejects the order (unknown subaccount) => leg REJECTED, nothing filled.

import { execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { collectSignatures, type PreparedExecution } from "../lib/noncustodial.ts";
import type { Eip1193 } from "../lib/wallet.ts";

const BASE = process.argv[2] ?? "http://127.0.0.1:8081";
const KEY = `0x${randomBytes(32).toString("hex")}`; // throwaway, never funded
const signer = (req: object) =>
  execFileSync("python", ["scripts/wallet-e2e.py"], { input: JSON.stringify({ key: KEY, ...req }) }).toString().trim();
const ADDRESS = signer({ method: "address" });

let chain = 1;
const calls: string[] = [];
const wallet: Eip1193 = {
  async request({ method, params }) {
    calls.push(method);
    const p = (params ?? []) as unknown[];
    switch (method) {
      case "eth_requestAccounts":
        return [ADDRESS];
      case "eth_chainId":
        return `0x${chain.toString(16)}`;
      case "wallet_switchEthereumChain":
        chain = Number((p[0] as { chainId: string }).chainId);
        return null;
      case "eth_signTypedData_v4": {
        const td = JSON.parse(p[1] as string);
        // A real wallet refuses a domain on another chain: so does this one.
        if (Number(td.domain.chainId) !== chain) throw { code: -32000, message: "chainId mismatch" };
        return signer({ method, typed_data: td });
      }
      case "personal_sign":
        return signer({ method, hex: p[0] });
      default:
        throw { code: 4200, message: `mock wallet: ${method} not supported` };
    }
  },
};

async function post<T>(path: string, body: unknown): Promise<[number, T]> {
  const r = await fetch(`${BASE}${path}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  return [r.status, (await r.json()) as T];
}

const ok = (cond: boolean, what: string) => {
  console.log(`${cond ? "PASS" : "FAIL"}  ${what}`);
  if (!cond) process.exitCode = 1;
};

console.log(`throwaway wallet ${ADDRESS} -> ${BASE}`);

// 1. Subaccount lookup (read-only, Derive testnet).
const ts = Date.now();
const auth = await wallet.request({ method: "personal_sign", params: [`0x${Buffer.from(String(ts)).toString("hex")}`, ADDRESS] });
const [ls, lookup] = await post<{ subaccount_ids?: number[]; error?: unknown }>("/v1/noncustodial/derive/subaccounts", { wallet: ADDRESS, timestamp_ms: ts, signature: auth });
console.log("  lookup:", ls, JSON.stringify(lookup));
ok(ls === 200 || ls === 502, "subaccount lookup reaches Derive with a signature our server accepted");
const [bs] = await post("/v1/noncustodial/derive/subaccounts", { wallet: ADDRESS, timestamp_ms: ts, signature: `0x${"11".repeat(65)}` });
ok(bs === 422, "forged lookup signature refused locally (422)");

// 2. Quote a small Derive route (first Derive ETH call, >3 days out, that is executable).
type Group = { strike: string; expiry: string; listings: { key: { platform: string } }[] };
const search = (await (await fetch(`${BASE}/v1/instruments?underlying=ETH&option_kind=call&limit=500`)).json()) as { groups?: Group[] };
const groups = (search.groups ?? []).filter(
  (g) => g.listings.some((l) => l.key.platform === "derive") && Date.parse(g.expiry) > Date.now() + 3 * 86400e3,
);
console.log(`  ${groups.length} Derive ETH call contracts`);
type Quote = { route_id: string; route_hash: string; executable: boolean; not_executable_reasons: string[]; no_plan_reason?: string };
let quote: Quote | null = null;
for (const g of groups.slice(Math.floor(groups.length / 3)).slice(0, 25)) {
  const [qs, q] = await post<Quote>("/v1/routes/quote", {
    underlying: "ETH", option_kind: "call", strike: g.strike, expiry: g.expiry, side: "buy", quantity: "0.1",
    venues: ["derive"], accepted_differences: ["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"],
  });
  if (qs === 200 && q.executable) {
    quote = q;
    console.log(`  route on ETH ${g.strike} C ${g.expiry}`);
    break;
  }
}
ok(quote !== null, "executable Derive route");
if (!quote) process.exit(1);

// 3. Prepare with the throwaway account (subaccount that does not exist).
const [ps, prep] = await post<PreparedExecution>("/v1/noncustodial/prepare", {
  route_id: quote.route_id,
  route_hash: quote.route_hash,
  accounts: { derive: { owner: ADDRESS, subaccount_id: 1 } },
});
ok(ps === 200 && prep.legs?.length > 0, `prepare (${prep.legs?.length ?? 0} leg(s))`);
if (ps !== 200) {
  console.log(JSON.stringify(prep));
  process.exit(1);
}

// 4. The page's own code collects the signatures through the wallet.
const steps: string[] = [];
const sub = await collectSignatures(wallet, ADDRESS, prep, (s) => steps.push(s));
ok(chain === Number(prep.legs[0].typed_data?.domain?.chainId), "wallet switched to the signing domain's chain");
ok(calls.includes("eth_signTypedData_v4") && calls.includes("personal_sign"), "orders signed with eth_signTypedData_v4 + auth personal_sign");

// 5. Submit: our server verifies the signatures, then Derive refuses the unknown subaccount.
const [ss, rep] = await post<{ orders?: { state: string; last_error: string | null; filled_native: string }[]; error?: { code: string; message: string } }>("/v1/noncustodial/submit", sub);
console.log("  submit:", ss, JSON.stringify(rep).slice(0, 600));
const err = JSON.stringify(rep);
ok(!err.includes("does not match the declared signer"), "server accepted the wallet's signatures (no signer mismatch)");
ok((rep.orders ?? []).every((o) => o.filled_native === "0"), "nothing filled (throwaway account)");
const [rs] = await post("/v1/noncustodial/submit", sub);
ok(rs !== 200, "replay refused");
