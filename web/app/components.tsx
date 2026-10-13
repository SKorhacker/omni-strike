"use client";

import type {
  ComparabilityReport,
  ContractTerms,
  FeeAmount,
  FundingStatus,
  PlatformView,
  QuoteView,
  Route,
  SimulationRecord,
} from "@/lib/types";

export function Badge({ kind, children, title }: { kind: "ok" | "warn" | "bad" | "info" | "muted"; children: React.ReactNode; title?: string }) {
  return (
    <span className={`badge b-${kind}`} title={title}>
      {children}
    </span>
  );
}

export function ProvenanceBadge({ p }: { p: string }) {
  if (p === "LIVE_DATA") return <Badge kind="ok" title="Public data fetched live (no trading)">LIVE_DATA</Badge>;
  if (p === "FIXTURE") return <Badge kind="info" title="Real recorded response, replayed">FIXTURE</Badge>;
  return <Badge kind="bad" title="Synthetic data">MOCK</Badge>;
}

export function levelKind(level: string): "ok" | "warn" | "bad" | "info" | "muted" {
  if (level.startsWith("VERIFIED")) return "ok";
  if (level === "BLOCKED") return "bad";
  if (level === "MOCK") return "warn";
  return "muted";
}

export function statusKind(s: string): "ok" | "warn" | "bad" | "info" | "muted" {
  if (s === "ACTIVE" || s === "UP") return "ok";
  if (s === "LEGACY" || s === "DEGRADED" || s === "NO_DATA_YET") return "warn";
  if (s === "UNAVAILABLE" || s === "CIRCUIT_OPEN") return "bad";
  return "muted";
}

export function verdictKind(v: string): "ok" | "warn" | "bad" | "muted" {
  if (v === "STRICTLY_COMPARABLE") return "ok";
  if (v === "ECONOMICALLY_SIMILAR_WITH_DIFFERENCES") return "warn";
  if (v === "NOT_COMPARABLE") return "bad";
  return "muted";
}

export function fmtTime(iso: string | null | undefined) {
  if (!iso) return "—";
  const d = new Date(iso);
  return d.toISOString().replace("T", " ").replace(/\.\d+Z$/, "Z");
}

export function feeText(f: FeeAmount) {
  return f.status === "known" ? f.amount : `UNKNOWN (${f.reason})`;
}

export function fundingText(f: FundingStatus) {
  switch (f.status) {
    case "UNVERIFIED":
      return <Badge kind="muted" title="No account data">funding not verified</Badge>;
    case "SIMULATED_SUFFICIENT":
      return <Badge kind="info">simulated balance sufficient ({f.declared_balance})</Badge>;
    case "SIMULATED_INSUFFICIENT":
      return <Badge kind="bad">simulated balance insufficient ({f.declared_balance} &lt; {f.required})</Badge>;
    default:
      return <Badge kind="muted">not assessable</Badge>;
  }
}

export function PlatformsPanel({ data, error, loading, onRefresh }: { data: PlatformView[] | null; error: string | null; loading: boolean; onRefresh: () => void }) {
  return (
    <section className="panel" aria-labelledby="platforms-h">
      <div className="row" style={{ justifyContent: "space-between" }}>
        <h2 id="platforms-h">Platforms ({data?.length ?? "…"}) — verified status and collection</h2>
        <button onClick={onRefresh} disabled={loading}>{loading ? "Refreshing…" : "Refresh"}</button>
      </div>
      {error && <div className="banner error" role="alert">Inventory unavailable: {error}. Retrying automatically every 15 s.</div>}
      {data && (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Platform</th><th>Families</th><th>Operational status</th><th>Integration</th>
                <th>Collection</th><th>Catalogs</th><th>What is missing / notes</th>
              </tr>
            </thead>
            <tbody>
              {data.map((p) => (
                <tr key={p.id}>
                  <td>
                    <strong>{p.display_name}</strong>
                    {p.aliases.length > 0 && <div className="small muted">aliases: {p.aliases.join(", ")}</div>}
                    <div className="small muted">verified on {p.verified_on}</div>
                  </td>
                  <td className="small">{p.families.join(", ")}</td>
                  <td><Badge kind={statusKind(p.operational_status)}>{p.operational_status}</Badge></td>
                  <td><Badge kind={levelKind(p.integration_level)}>{p.integration_level}</Badge></td>
                  <td>
                    {p.health ? (
                      <>
                        <Badge kind={statusKind(p.health.state)}>{p.health.state}</Badge>
                        {p.health.last_error && <div className="small muted" title={p.health.last_error}>last error: {p.health.last_error.slice(0, 80)}</div>}
                        {p.health.circuit_open_until && <div className="small">circuit open until {fmtTime(p.health.circuit_open_until)}</div>}
                      </>
                    ) : (
                      <span className="small muted">not connected</span>
                    )}
                  </td>
                  <td className="small">
                    {p.health?.catalogs.length
                      ? p.health.catalogs.map((c) => (
                          <div key={c.underlying}>{c.underlying}: {c.state}{c.instruments != null ? ` (${c.instruments})` : ""}</div>
                        ))
                      : "—"}
                  </td>
                  <td className="small">{p.notes}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

export function QuoteTable({ title, rows, selected, onSelect, maxAgeMs, emptyText }: {
  title: string; rows: QuoteView[]; selected: string | null; onSelect: (k: string) => void; maxAgeMs: number; emptyText: string;
}) {
  return (
    <>
      <h3>{title} ({rows.length})</h3>
      {rows.length === 0 ? (
        <p className="small muted">{emptyText}</p>
      ) : (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>#</th><th>Platform / contract</th><th>Source</th><th>Age</th><th>Covered / requested</th>
                <th>Avg price</th><th>Premium</th><th>Fees</th><th>Unknown costs</th><th>Estimated total</th>
                <th>Required funds</th><th>Comparability</th><th>Ineligibility</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((q, i) => {
                const e = q.estimate;
                const key = `${e.instrument.platform}:${e.instrument.native_id}`;
                const stale = e.age_ms > maxAgeMs;
                return (
                  <tr
                    key={key}
                    className={`clickable ${selected === key ? "selected" : ""}`}
                    tabIndex={0}
                    onClick={() => onSelect(key)}
                    onKeyDown={(ev) => (ev.key === "Enter" || ev.key === " ") && onSelect(key)}
                    aria-selected={selected === key}
                  >
                    <td>{i + 1}</td>
                    <td>
                      <strong>{e.instrument.platform}</strong>{q.is_reference && <> <Badge kind="info">reference</Badge></>}
                      <div className="small muted">{e.instrument.native_id}</div>
                    </td>
                    <td><ProvenanceBadge p={e.provenance} /> <Badge kind="muted">{e.firmness}</Badge></td>
                    <td className="num">
                      {(e.age_ms / 1000).toFixed(1)} s {stale && <Badge kind="bad">STALE</Badge>}
                      <div className="small muted">{fmtTime(e.received_at)}</div>
                    </td>
                    <td className="num">{e.covered_underlying} / {e.requested_underlying}</td>
                    <td className="num">{e.avg_price_per_underlying ?? "—"}</td>
                    <td className="num">{e.premium}</td>
                    <td className="num">{feeText(e.fee)}</td>
                    <td className="small">{e.unknown_costs.length ? e.unknown_costs.join("; ") : "none"}</td>
                    <td className="num"><strong>{e.total_estimated ?? "not computable"}</strong> <span className="small muted">{e.premium_currency}</span></td>
                    <td>{e.required_funds ?? "—"}<div>{fundingText(q.funding)}</div></td>
                    <td>{e.comparability ? <Badge kind={verdictKind(e.comparability.verdict)}>{e.comparability.verdict}</Badge> : <span className="small muted">reference contract</span>}</td>
                    <td className="small">{e.ineligible.length ? e.ineligible.join(", ") : <Badge kind="ok">eligible</Badge>}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

export function ReportView({ r }: { r: ComparabilityReport }) {
  return (
    <div>
      <Badge kind={verdictKind(r.verdict)}>{r.verdict}</Badge>
      {r.reasons.length > 0 && <div className="chips" style={{ marginTop: 4 }}>{r.reasons.map((c) => <code key={c}>{c}</code>)}</div>}
      <p className="small">{r.explanation}</p>
    </div>
  );
}

export function QuoteDetail({ q }: { q: QuoteView }) {
  const e = q.estimate;
  const t = q.terms;
  return (
    <div className="grid2">
      <div>
        <h3>Contract — {t.key.platform}</h3>
        <dl className="kv">
          <dt>Native ID</dt><dd>{t.key.native_id}</dd>
          <dt>Network</dt><dd>{t.key.network}</dd>
          <dt>Family</dt><dd>{t.family}</dd>
          <dt>Type / strike</dt><dd>{t.option_kind} {t.strike}</dd>
          <dt>Expiry (UTC)</dt><dd>{fmtTime(t.expiry)}</dd>
          <dt>Exercise</dt><dd>{t.exercise_style ?? "not verified"}</dd>
          <dt>Multiplier</dt><dd>{t.contract_multiplier} underlying / contract</dd>
          <dt>Premium currency</dt><dd>{t.premium_currency}</dd>
          <dt>Collateral</dt><dd>{t.collateral_assets.join(", ")}</dd>
          <dt>Settlement</dt><dd>{t.settlement_mechanism ?? "not verified"} in {t.settlement_asset ?? "?"}</dd>
          <dt>Index</dt><dd>{t.settlement_index ?? "not verified"}</dd>
          <dt>Window</dt><dd>{t.settlement_window ?? <Badge kind="warn">UNVERIFIED</Badge>}</dd>
          <dt>Tick / min / step</dt><dd>{t.tick_size ?? "?"} / {t.min_amount ?? "?"} / {t.amount_step ?? "?"}</dd>
          <dt>Specification</dt><dd>{t.spec_version}</dd>
        </dl>
      </div>
      <div>
        <h3>Cost breakdown</h3>
        <dl className="kv">
          <dt>Method</dt><dd>{e.pricing_method} ({e.levels_consumed} levels)</dd>
          <dt>Premium</dt><dd>{e.premium} {e.premium_currency}</dd>
          <dt>Of which book impact</dt><dd>{e.book_impact ?? "—"} (already included in the premium, not added)</dd>
          <dt>Trading fees</dt><dd>{feeText(e.fee)}</dd>
          <dt>Gas</dt><dd>{e.gas.status === "known" ? e.gas.amount : e.gas.status === "not_applicable" ? `n/a — ${e.gas.reason}` : `UNKNOWN — ${e.gas.reason}`}</dd>
          <dt>Estimated total</dt><dd><strong>{e.total_estimated ?? "not computable (unknown cost or zero coverage)"}</strong></dd>
          <dt>Per unit</dt><dd>{e.total_per_underlying ?? "—"}</dd>
          <dt>Index</dt><dd>{e.index_price ?? "unavailable"}</dd>
          <dt>Source timestamp</dt><dd>{fmtTime(e.source_ts)}</dd>
          <dt>Received</dt><dd>{fmtTime(e.received_at)} (valid until {fmtTime(e.valid_until)})</dd>
          <dt>Confidence</dt><dd>{e.confidence}</dd>
          <dt>Fee source</dt><dd className="small">{e.fee_source}</dd>
        </dl>
        <p className="small muted">{q.fee_notes}</p>
        {e.comparability && (<><h3>Comparability with the reference</h3><ReportView r={e.comparability} /></>)}
        <h3>Assumptions</h3>
        <ul className="tight small">{e.assumptions.map((a) => <li key={a}>{a}</li>)}</ul>
      </div>
    </div>
  );
}

export function RouteView({ r }: { r: Route }) {
  return (
    <div>
      <dl className="kv">
        <dt>Destination</dt><dd>{r.destination.platform} — {r.destination.native_id}</dd>
        <dt>Side / quantity</dt><dd>{r.side} {r.quantity_underlying} underlying ({r.quantity_native} contracts)</dd>
        <dt>Expected avg price</dt><dd>{r.expected_avg_price_per_underlying}</dd>
        <dt>Limit price</dt><dd>{r.limit_price_per_underlying} (tolerance {r.price_tolerance_bps} bp, rounded to tick)</dd>
        <dt>Estimated total</dt><dd>{r.estimated_total} {r.currency}</dd>
        <dt>Fees</dt><dd>{feeText(r.fee)}</dd>
        <dt>Funding</dt><dd>{fundingText(r.funding)}</dd>
        <dt>Accepted differences</dt><dd>{r.accepted_differences_in_scope.length ? r.accepted_differences_in_scope.join(", ") : "none"}</dd>
      </dl>
      <h3>Steps</h3>
      <ol className="small">{r.steps.map((s) => <li key={s}>{s}</li>)}</ol>
      <h3>Warnings</h3>
      <ul className="tight small">{r.warnings.map((s) => <li key={s}>{s}</li>)}</ul>
    </div>
  );
}

export function SimulationView({ s }: { s: SimulationRecord }) {
  const kind = s.status === "SIMULATED_FILL" ? "ok" : s.status === "SIMULATED_PARTIAL_FILL" ? "warn" : "bad";
  return (
    <div>
      <p><Badge kind={kind}>{s.status}</Badge> <Badge kind="info">{s.mode}</Badge> <span className="small muted">id {s.id} — evaluated at {fmtTime(s.evaluated_at)}</span></p>
      {s.result && (
        <dl className="kv">
          <dt>Simulated quantity</dt><dd>{s.result.filled_underlying} underlying ({s.result.filled_native} contracts)</dd>
          <dt>Avg price</dt><dd>{s.result.avg_price_per_underlying ?? "—"}</dd>
          <dt>Premium</dt><dd>{s.result.premium}</dd>
          <dt>Fees</dt><dd>{feeText(s.result.fee)}</dd>
          <dt>Simulated total</dt><dd><strong>{s.result.total ?? "not computable"}</strong></dd>
          <dt>Within limit</dt><dd>{s.result.within_limit_price ? "yes" : "no"}</dd>
        </dl>
      )}
      {s.risk_violations.length > 0 && <p>Risk checks: {s.risk_violations.map((v) => <code key={v.code}>{v.code} </code>)}</p>}
      <p className="small">Strategy: <code>{s.strategy}</code></p>
      {s.price_drift && (
        <p className="small">
          Drift between the quote seen ({s.price_drift.quoted_avg}) and the fresh book ({s.price_drift.fresh_avg}):{" "}
          <strong>{s.price_drift.drift_bps} bp</strong> (quote {(s.price_drift.quote_age_ms / 1000).toFixed(1)} s old). The limit price is derived from the quote seen.
        </p>
      )}
      {s.scenarios.length > 0 && (
        <>
          <h3>Stress scenarios (same recorded book)</h3>
          <div className="table-wrap">
            <table>
              <thead><tr><th>Scenario</th><th>Description</th><th>Outcome</th><th>Filled</th><th>Avg price</th><th>Total</th></tr></thead>
              <tbody>
                {s.scenarios.map((sc) => (
                  <tr key={sc.name}>
                    <td><code>{sc.name}</code></td>
                    <td className="small">{sc.description}</td>
                    <td><Badge kind={sc.outcome === "FILL" ? "ok" : sc.outcome === "PARTIAL_FILL" ? "warn" : "bad"}>{sc.outcome}</Badge></td>
                    <td className="num">{sc.result.filled_underlying}</td>
                    <td className="num">{sc.result.avg_price_per_underlying ?? "—"}</td>
                    <td className="num">{sc.result.total ?? "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
      {s.estimated_saving_vs_baseline && (
        <p>Estimated saving under these assumptions vs {s.estimated_saving_vs_baseline.baseline}: <strong>{s.estimated_saving_vs_baseline.amount} {s.estimated_saving_vs_baseline.currency}</strong> <span className="small muted">({s.estimated_saving_vs_baseline.label})</span></p>
      )}
      <p className="small">Accepted differences: {s.accepted_differences.length ? s.accepted_differences.join(", ") : "none"}</p>
      <div className="grid2">
        <div><h3>Assumptions</h3><ul className="tight small">{s.assumptions.map((a) => <li key={a}>{a}</li>)}</ul></div>
        <div><h3>Limitations</h3><ul className="tight small">{s.limitations.map((a) => <li key={a}>{a}</li>)}</ul></div>
      </div>
    </div>
  );
}

const TERM_ROWS: [string, (t: ContractTerms) => string][] = [
  ["Settlement index", (t) => t.settlement_index ?? "not verified"],
  ["Settlement window", (t) => t.settlement_window ?? "not documented (UNVERIFIED)"],
  ["Mechanism / asset", (t) => `${t.settlement_mechanism ?? "?"} / ${t.settlement_asset ?? "?"}`],
  ["Exercise", (t) => t.exercise_style ?? "not verified"],
  ["Expiry (UTC)", (t) => fmtTime(t.expiry)],
  ["Multiplier", (t) => t.contract_multiplier],
  ["Premium currency", (t) => t.premium_currency],
  ["Step / minimum", (t) => `${t.amount_step ?? "?"} / ${t.min_amount ?? "?"}`],
];

/** Side-by-side contract terms; rows that differ are highlighted. */
export function TermsSideBySide({ a, b }: { a: ContractTerms; b: ContractTerms }) {
  return (
    <div className="table-wrap">
      <table>
        <thead><tr><th>Term</th><th>{a.key.platform}</th><th>{b.key.platform}</th><th></th></tr></thead>
        <tbody>
          {TERM_ROWS.map(([label, f]) => {
            const [x, y] = [f(a), f(b)];
            return (
              <tr key={label}>
                <td>{label}</td>
                <td className="small">{x}</td>
                <td className="small">{y}</td>
                <td>{x !== y ? <Badge kind="warn">differs</Badge> : <Badge kind="ok">identical</Badge>}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
