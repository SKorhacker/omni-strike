"use client";

import Link from "next/link";
import { useEffect, useState } from "react";
import { api, ApiError } from "@/lib/api";
import type { CompareRequest } from "@/lib/types";
import type { ExecutionReport, RouteQuote, SplitPolicyInput } from "@/lib/routing";
import { Badge, feeText, fmtTime } from "./components";
import { WalletExecution } from "./wallet-execution";

const errText = (e: unknown) => (e instanceof ApiError ? `${e.code}: ${e.message}` : String(e));

/** Executable routing for the current intent: single venue or opt-in split, then execution (server
 * account in TESTNET, or the user's own wallet in TESTNET / MAINNET_NONCUSTODIAL). */
export function RoutingPanel({ request, mode }: { request: CompareRequest; mode: string }) {
  const [split, setSplit] = useState<SplitPolicyInput>({ allow_split: false, max_legs: 2, min_saving_abs: "1", min_saving_bps: "10", limit_tolerance_bps: "50" });
  const [quote, setQuote] = useState<RouteQuote | null>(null);
  const [report, setReport] = useState<ExecutionReport | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [confirm, setConfirm] = useState(false);

  const getQuote = async () => {
    setBusy("quote");
    setErr(null);
    setReport(null);
    setConfirm(false);
    try {
      const { price_tolerance_bps: _tol, ...base } = request;
      void _tol;
      setQuote(await api<RouteQuote>("/v1/routes/quote", { method: "POST", body: { ...base, split } }));
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(null);
    }
  };

  const execute = async () => {
    if (!quote) return;
    setBusy("execute");
    setErr(null);
    try {
      setReport(await api<ExecutionReport>("/v1/routes/execute-testnet", { method: "POST", body: { route_id: quote.route_id, route_hash: quote.route_hash } }));
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(null);
      setConfirm(false);
    }
  };

  // Clock for the expiry badge (render must stay pure: no Date.now() during render).
  const [now, setNow] = useState(0);
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  const expired = quote && now > 0 ? Date.parse(quote.expires_at) < now : false;
  const p = quote?.plan;

  return (
    <section className="panel" aria-labelledby="routing-h">
      <h2 id="routing-h">Executable routing <Badge kind="info">{mode}</Badge></h2>
      <p className="small muted">
        Single venue or split (2–3 venues, optional). IOC limit orders per leg; the legs of a split are not atomic across venues.
        Only the contract differences accepted above are used.
      </p>
      <div className="row">
        <label style={{ flexDirection: "row", alignItems: "center" }}>
          <input type="checkbox" checked={split.allow_split} onChange={(e) => setSplit({ ...split, allow_split: e.target.checked })} />
          Allow split
        </label>
        <label>Max legs
          <select value={split.max_legs} onChange={(e) => setSplit({ ...split, max_legs: Number(e.target.value) })} disabled={!split.allow_split}>
            <option value={2}>2</option><option value={3}>3</option>
          </select>
        </label>
        <label>Min saving (USDC)
          <input value={split.min_saving_abs} onChange={(e) => setSplit({ ...split, min_saving_abs: e.target.value })} size={6} disabled={!split.allow_split} />
        </label>
        <label>Min saving (bp)
          <input value={split.min_saving_bps} onChange={(e) => setSplit({ ...split, min_saving_bps: e.target.value })} size={6} disabled={!split.allow_split} />
        </label>
        <label>Limit tolerance (bp)
          <input value={split.limit_tolerance_bps} onChange={(e) => setSplit({ ...split, limit_tolerance_bps: e.target.value })} size={6} />
        </label>
        <button className="primary" disabled={busy !== null || request.side !== "buy"} onClick={getQuote}>
          {busy === "quote" ? "Routing…" : "Propose a route"}
        </button>
        {request.side !== "buy" && <span className="small muted">Routing: buy only.</span>}
      </div>
      {err && <div className="banner error" role="alert">{err}</div>}

      {quote && (
        <div style={{ marginTop: 10 }}>
          {!p ? (
            <p>No plan: {quote.no_plan_reason}</p>
          ) : (
            <>
              <p>
                <Badge kind={p.kind === "SPLIT" ? "warn" : "ok"}>{p.kind}</Badge>{" "}
                {!p.fully_covered && <Badge kind="bad">PARTIAL COVERAGE</Badge>}{" "}
                <strong>Estimated total {p.expected_total}</strong> for {p.covered_underlying}/{p.requested_underlying}
                {p.estimated_saving_vs_best_single && p.best_single && (
                  <span className="small muted"> — estimated saving vs best single venue ({p.best_single[0].platform}, {p.best_single[1]}): {p.estimated_saving_vs_best_single}</span>
                )}
              </p>
              <p className="small">{p.reason}</p>
              <div className="table-wrap">
                <table>
                  <thead><tr><th>#</th><th>Venue / contract</th><th>Quantity</th><th>Avg price</th><th>Worst level</th><th>IOC limit</th><th>Fees</th><th>Total</th><th>Levels</th></tr></thead>
                  <tbody>
                    {p.legs.map((l, i) => (
                      <tr key={i}>
                        <td>{i + 1}</td>
                        <td><strong>{l.instrument.platform}</strong><div className="small muted">{l.instrument.native_id} · {l.instrument.network}</div></td>
                        <td className="num">{l.qty_underlying} ({l.qty_native} c.)</td>
                        <td className="num">{l.expected_avg_per_underlying}</td>
                        <td className="num">{l.worst_level_price_native}</td>
                        <td className="num">{l.limit_price_native}</td>
                        <td className="num">{feeText(l.fee)}</td>
                        <td className="num">{l.expected_total}</td>
                        <td className="num">{l.levels_consumed}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </>
          )}
          <h3>Venues considered</h3>
          <ul className="tight small">
            {quote.candidates.map((c, i) => (
              <li key={i}>
                <strong>{c.instrument.platform}</strong> {c.instrument.native_id} —{" "}
                {c.routable ? <Badge kind="ok">routable</Badge> : <><Badge kind="muted">excluded</Badge> {c.reasons.join(", ")}</>}
                {Number(c.quality_penalty_bps) > 0 && <> · quality penalty {c.quality_penalty_bps} bp</>}
              </li>
            ))}
          </ul>
          <ul className="tight small muted">{quote.notes.map((n) => <li key={n}>{n}</li>)}</ul>
          <p className="small">Route {quote.route_id} — single use, expires at {fmtTime(quote.expires_at)} {expired && <Badge kind="bad">EXPIRED</Badge>}</p>

          {quote.executable && !expired ? (
            <>
            {(mode === "TESTNET" || mode === "MAINNET_NONCUSTODIAL") && (
              <WalletExecution key={quote.route_id} quote={quote} mode={mode} onReport={setReport} />
            )}
            {mode === "TESTNET" && (
            <div className="banner" role="group" aria-label="Testnet execution" style={{ marginTop: 8 }}>
              <p><Badge kind="warn">TESTNET</Badge> Test funds only. No live order exists in this version.</p>
              <label style={{ flexDirection: "row", alignItems: "center" }}>
                <input type="checkbox" checked={confirm} onChange={(e) => setConfirm(e.target.checked)} />
                I confirm the execution of this exact route on testnet
              </label>
              <button className="primary" disabled={!confirm || busy !== null} onClick={execute} style={{ marginTop: 6 }}>
                {busy === "execute" ? "Executing…" : "Execute on testnet (server account)"}
              </button>
            </div>
            )}
            </>
          ) : (
            <p className="small"><Badge kind="muted">not executable</Badge> {quote.not_executable_reasons.join("; ") || (expired ? "route expired: request a new route" : "")}</p>
          )}
        </div>
      )}

      {report && (
        <div style={{ marginTop: 10 }}>
          <h3>Execution result <Badge kind="info">{report.mode}</Badge></h3>
          <p>Filled {report.filled_underlying} / {report.requested_underlying} (route estimated total: {report.expected_total})</p>
          <ul className="tight small">{report.notes.map((n) => <li key={n}>{n}</li>)}</ul>
          <p className="small">Details and reconciliation: <Link href="/orders">Order tracking</Link> page.</p>
        </div>
      )}
    </section>
  );
}
