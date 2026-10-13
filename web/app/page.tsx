"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { api, ApiError } from "@/lib/api";
import { RoutingPanel } from "./routing-panel";
import type {
  CompareRequest,
  CompareResponse,
  PlatformsResponse,
  RoutePreview,
  SearchResponse,
  SimulationRecord,
} from "@/lib/types";
import {
  Badge,
  fmtTime,
  PlatformsPanel,
  QuoteDetail,
  QuoteTable,
  ReportView,
  RouteView,
  SimulationView,
  TermsSideBySide,
} from "./components";

const MEMO_KEY = "oa.accepted-differences.v1";
function loadMemo(): Record<string, string[]> {
  try {
    return JSON.parse(sessionStorage.getItem(MEMO_KEY) ?? "{}");
  } catch {
    return {};
  }
}
function saveMemo(m: Record<string, string[]>) {
  try {
    sessionStorage.setItem(MEMO_KEY, JSON.stringify(m));
  } catch {
    /* storage unavailable: the choice is simply not remembered */
  }
}

const errText = (e: unknown) => (e instanceof ApiError ? `${e.code}: ${e.message}` : String(e));

// Only secondary (contractual) differences can ever be accepted; core mismatches never.
const ACCEPTABLE = new Set([
  "EXERCISE_STYLE_DIFFERS", "EXERCISE_STYLE_UNKNOWN", "SETTLEMENT_MECHANISM_DIFFERS", "SETTLEMENT_MECHANISM_UNKNOWN",
  "SETTLEMENT_ASSET_DIFFERS", "SETTLEMENT_ASSET_UNKNOWN", "SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_INDEX_UNKNOWN",
  "SETTLEMENT_WINDOW_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN", "PREMIUM_CURRENCY_DIFFERS",
]);

export default function Home() {
  // Platforms + periodic refresh (reconnection: errors are shown and retried).
  const [platforms, setPlatforms] = useState<PlatformsResponse | null>(null);
  const [platErr, setPlatErr] = useState<string | null>(null);
  const [platLoading, setPlatLoading] = useState(false);
  const loadPlatforms = useCallback(async () => {
    setPlatLoading(true);
    try {
      setPlatforms(await api<PlatformsResponse>("/v1/platforms"));
      setPlatErr(null);
    } catch (e) {
      setPlatErr(errText(e));
    } finally {
      setPlatLoading(false);
    }
  }, []);
  useEffect(() => {
    // Initial load + polling. setState happens after the await inside loadPlatforms.
    const first = setTimeout(loadPlatforms, 0);
    const t = setInterval(loadPlatforms, 15_000);
    return () => {
      clearTimeout(first);
      clearInterval(t);
    };
  }, [loadPlatforms]);

  // Search form.
  const [underlying, setUnderlying] = useState("BTC");
  const [kind, setKind] = useState<"call" | "put">("call");
  const [search, setSearch] = useState<SearchResponse | null>(null);
  const [searchErr, setSearchErr] = useState<string | null>(null);
  const [searching, setSearching] = useState(false);
  const [expiry, setExpiry] = useState("");
  const [strike, setStrike] = useState("");
  const [side, setSide] = useState<"buy" | "sell">("buy");
  const [quantity, setQuantity] = useState("0.1");
  const [baseline, setBaseline] = useState("");
  const [maxAge, setMaxAge] = useState(10_000);
  const [balances, setBalances] = useState<Record<string, string>>({});
  const [tolerance, setTolerance] = useState(50);
  const [strategy, setStrategy] = useState("manual");
  const [remember, setRemember] = useState(false);
  const [prefilled, setPrefilled] = useState<string[]>([]);

  const runSearch = useCallback(async (u: string, k: string) => {
    setSearching(true);
    setSearchErr(null);
    try {
      const r = await api<SearchResponse>(`/v1/instruments?underlying=${u}&option_kind=${k}&limit=1000`);
      setSearch(r);
      const now = Date.now();
      const multi = r.expiries.filter((e) => e.platforms.length > 1 && Date.parse(e.expiry) > now);
      setExpiry((multi[0] ?? r.expiries[0])?.expiry ?? "");
      setStrike("");
    } catch (e) {
      setSearchErr(errText(e));
      setSearch(null);
    } finally {
      setSearching(false);
    }
  }, []);
  useEffect(() => {
    const t = setTimeout(() => runSearch(underlying, kind), 0);
    return () => clearTimeout(t);
  }, [underlying, kind, runSearch]);

  const strikes = useMemo(
    () => (search?.groups ?? []).filter((g) => g.expiry === expiry && g.option_kind === kind),
    [search, expiry, kind],
  );

  // Comparison, explicit acceptance, preview, simulation.
  const [accepted, setAccepted] = useState<string[]>([]);
  const [cmp, setCmp] = useState<CompareResponse | null>(null);
  const [cmpErr, setCmpErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [preview, setPreview] = useState<RoutePreview | null>(null);
  const [sim, setSim] = useState<SimulationRecord | null>(null);
  const [actionErr, setActionErr] = useState<string | null>(null);

  const request = (acc: string[], qty: string = quantity): CompareRequest => {
    const bal: Record<string, string> = {};
    for (const [k, v] of Object.entries(balances)) if (v.trim() !== "") bal[k] = v.trim();
    return {
      underlying, option_kind: kind, strike, expiry, side, quantity: qty,
      accepted_differences: acc,
      baseline_platform: baseline || undefined,
      max_age_ms: maxAge,
      simulated_balances: bal,
    };
  };

  const pairKey = (r: CompareResponse) =>
    Array.from(new Set(r.comparability.flatMap((p) => [p.a.platform, p.b.platform]))).sort().join("|");

  const compare = async (acc: string[], qty: string = quantity) => {
    setBusy("compare");
    setCmpErr(null);
    setPreview(null);
    setSim(null);
    setActionErr(null);
    try {
      const r = await api<CompareResponse>("/v1/quotes/compare", { method: "POST", body: request(acc, qty) });
      setCmp(r);
      setSelected(null);
      // Remembered acceptance (opt-in, this tab only): the boxes are PRE-CHECKED visibly; nothing
      // applies until the user clicks "Compare again".
      const memo = loadMemo()[pairKey(r)] ?? [];
      const codes = new Set(r.comparability.flatMap((p) => p.report.reasons));
      const usable = memo.filter((c) => codes.has(c));
      if (acc.length === 0 && usable.length > 0) {
        setAccepted(usable);
        setPrefilled(usable);
      } else {
        setPrefilled([]);
      }
    } catch (e) {
      setCmpErr(errText(e));
    } finally {
      setBusy(null);
    }
  };

  const doPreview = async () => {
    setBusy("preview");
    setActionErr(null);
    try {
      const p = await api<RoutePreview>("/v1/routes/preview", { method: "POST", body: { ...request(accepted), price_tolerance_bps: tolerance } });
      setPreview(p);
      setCmp(p.compare);
      setSim(null);
    } catch (e) {
      setActionErr(errText(e));
    } finally {
      setBusy(null);
    }
  };

  const doSimulate = async () => {
    setBusy("simulate");
    setActionErr(null);
    try {
      // Send the quote the user is looking at: the limit is derived from it (a moved book is rejected).
      const best = cmp?.ranked[0]?.estimate;
      const quoted = best?.avg_price_per_underlying && cmp
        ? { platform: best.instrument.platform, avg_price_per_underlying: best.avg_price_per_underlying, computed_at: cmp.computed_at }
        : undefined;
      const s = await api<SimulationRecord>("/v1/orders/simulate", {
        method: "POST",
        body: { ...request(accepted), price_tolerance_bps: tolerance, strategy, quoted },
      });
      setSim(s);
    } catch (e) {
      setActionErr(errText(e));
    } finally {
      setBusy(null);
    }
  };

  const allQuotes = cmp ? [...cmp.ranked, ...cmp.incomplete, ...cmp.ineligible] : [];
  const selectedQuote = allQuotes.find((q) => `${q.estimate.instrument.platform}:${q.estimate.instrument.native_id}` === selected);
  const differenceCodes = Array.from(new Set((cmp?.comparability ?? []).flatMap((p) => p.report.reasons))).filter((c) => ACCEPTABLE.has(c));
  const integrated = platforms?.platforms.filter((p) => p.implemented.length > 0) ?? [];
  const canCompare = Boolean(expiry && strike && Number(quantity) > 0);

  return (
    <main>
      <header className="row" style={{ justifyContent: "space-between" }}>
        <div>
          <h1>Omni Strike</h1>
          <div className="small muted">Best <strong>estimated</strong> cost among covered and eligible destinations — never a guaranteed best execution.</div>
        </div>
        <div className="chips" aria-label="Active mode">
          <Link href="/orders">Testnet order tracking →</Link>
          <Badge kind="info" title="Server mode">{platforms?.mode ?? "…"}</Badge>
          <Badge kind="muted" title="No order can be sent">TRADING DISABLED</Badge>
        </div>
      </header>

      <PlatformsPanel data={platforms?.platforms ?? null} error={platErr} loading={platLoading} onRefresh={loadPlatforms} />

      <section className="panel" aria-labelledby="search-h">
        <h2 id="search-h">Compare a contract across venues</h2>
        {searchErr && <div className="banner error" role="alert">Search failed: {searchErr} <button onClick={() => runSearch(underlying, kind)}>Retry</button></div>}
        {search?.catalogs.filter((c) => !c.ok).map((c) => (
          <div key={c.platform} className="banner" role="status">Catalog {c.platform} unavailable: {c.error}</div>
        ))}
        <div className="row">
          <label>Underlying
            <select value={underlying} onChange={(e) => setUnderlying(e.target.value)}>
              <option>BTC</option><option>ETH</option>
            </select>
          </label>
          <label>Type
            <select value={kind} onChange={(e) => setKind(e.target.value as "call" | "put")}>
              <option value="call">Call</option><option value="put">Put</option>
            </select>
          </label>
          <label>Expiry (UTC)
            <select value={expiry} onChange={(e) => { setExpiry(e.target.value); setStrike(""); }} disabled={searching || !search}>
              {searching && <option>Loading…</option>}
              {search?.expiries.map((e) => (
                <option key={e.expiry} value={e.expiry}>{fmtTime(e.expiry)} — {e.platforms.join(" + ")}</option>
              ))}
            </select>
          </label>
          <label>Strike
            <select value={strike} onChange={(e) => setStrike(e.target.value)} disabled={!strikes.length}>
              <option value="">{strikes.length ? "Choose…" : "No contract"}</option>
              {strikes.map((g) => (
                <option key={g.strike} value={g.strike}>{g.strike} — {g.listings.map((l) => l.key.platform).join(" + ")}</option>
              ))}
            </select>
          </label>
          <label>Side
            <select value={side} onChange={(e) => setSide(e.target.value as "buy" | "sell")}>
              <option value="buy">Buy</option><option value="sell">Sell (comparison only)</option>
            </select>
          </label>
          <label>Quantity ({underlying} units)
            <input inputMode="decimal" value={quantity} onChange={(e) => setQuantity(e.target.value)} size={8} />
          </label>
          <label>Max age (s)
            <input type="number" min={1} max={60} value={maxAge / 1000} onChange={(e) => setMaxAge(Math.round(Number(e.target.value) * 1000))} style={{ width: 70 }} />
          </label>
          <label>Baseline (existing funds)
            <select value={baseline} onChange={(e) => setBaseline(e.target.value)}>
              <option value="">none</option>
              {integrated.map((p) => <option key={p.id} value={p.id}>{p.display_name}</option>)}
            </select>
          </label>
        </div>
        <details style={{ marginTop: 8 }}>
          <summary className="small">Declared simulation balances (optional — never presented as real)</summary>
          <div className="row" style={{ marginTop: 6 }}>
            {integrated.map((p) => (
              <label key={p.id}>{p.display_name} (USDC)
                <input inputMode="decimal" placeholder="not declared" value={balances[p.id] ?? ""} onChange={(e) => setBalances({ ...balances, [p.id]: e.target.value })} size={10} />
              </label>
            ))}
          </div>
        </details>
        <div className="row" style={{ marginTop: 10 }}>
          <button className="primary" disabled={!canCompare || busy !== null} onClick={() => { setAccepted([]); compare([]); }}>
            {busy === "compare" ? "Comparing…" : "Compare (no difference accepted)"}
          </button>
          {!canCompare && <span className="small muted">Choose an expiry, a strike and a quantity &gt; 0.</span>}
        </div>
      </section>

      {cmpErr && <div className="banner error" role="alert">Comparison failed: {cmpErr}</div>}

      {cmp && (
        <section className="panel" aria-labelledby="cmp-h" aria-busy={busy !== null}>
          <h2 id="cmp-h">Comparison — {cmp.request.underlying} {cmp.request.option_kind} {cmp.request.strike} {fmtTime(cmp.request.expiry)} — {cmp.request.side} {cmp.request.quantity}</h2>
          <p className="small muted">{cmp.wording} Computed at {fmtTime(cmp.computed_at)}. Reference: {cmp.reference ? `${cmp.reference.platform} ${cmp.reference.native_id}` : "none"} ({cmp.reference_reason}).</p>
          <ul className="tight small">{cmp.notes.map((n) => <li key={n}>{n}</li>)}</ul>

          <QuoteTable title="Ranking (eligible, full coverage, known costs)" rows={cmp.ranked} selected={selected} onSelect={setSelected} maxAgeMs={maxAge} emptyText="No eligible destination with full coverage and known costs." />
          <QuoteTable title="Indicative / incomplete (not ranked)" rows={cmp.incomplete} selected={selected} onSelect={setSelected} maxAgeMs={maxAge} emptyText="None." />
          <QuoteTable title="Ineligible" rows={cmp.ineligible} selected={selected} onSelect={setSelected} maxAgeMs={maxAge} emptyText="None." />

          {cmp.unavailable.length > 0 && (
            <>
              <h3>Unavailable ({cmp.unavailable.length})</h3>
              <ul className="tight small">{cmp.unavailable.map((u) => <li key={u.platform + u.kind}><strong>{u.platform}</strong> — {u.kind}: {u.detail}</li>)}</ul>
            </>
          )}

          {cmp.common_quantity && (
            <div className="banner" role="status">
              <p className="small">{cmp.common_quantity.explanation}</p>
              <button disabled={busy !== null} onClick={() => { const q = cmp.common_quantity!.quantity; setQuantity(q); compare(accepted, q); }}>
                Compare again at {cmp.common_quantity.quantity} {cmp.request.underlying} (covered by {cmp.common_quantity.platforms.join(" + ")})
              </button>
            </div>
          )}

          {cmp.estimated_saving_vs_baseline && (
            <p>Estimated saving vs {cmp.estimated_saving_vs_baseline.baseline}: <strong>{cmp.estimated_saving_vs_baseline.amount} {cmp.estimated_saving_vs_baseline.currency}</strong> <span className="small muted">— {cmp.estimated_saving_vs_baseline.label}</span></p>
          )}

          {cmp.comparability.length > 0 && (
            <>
              <h3>Contract differences between platforms</h3>
              {cmp.comparability.map((p) => (
                <div key={p.a.native_id + p.b.native_id}>
                  <div className="small">{p.a.platform} {p.a.native_id} ↔ {p.b.platform} {p.b.native_id}</div>
                  <ReportView r={p.report} />
                  {(() => {
                    const ta = allQuotes.find((q) => q.terms.key.native_id === p.a.native_id)?.terms;
                    const tb = allQuotes.find((q) => q.terms.key.native_id === p.b.native_id)?.terms;
                    return ta && tb ? <TermsSideBySide a={ta} b={tb} /> : null;
                  })()}
                </div>
              ))}
              {differenceCodes.length > 0 && (
                <fieldset>
                  <legend className="small">Explicitly accept these differences for an economic comparison (scope: this comparison and the simulation that follows)</legend>
                  <div className="chips">
                    {differenceCodes.map((c) => (
                      <label key={c} style={{ flexDirection: "row", alignItems: "center" }}>
                        <input type="checkbox" checked={accepted.includes(c)} onChange={(e) => setAccepted(e.target.checked ? [...accepted, c] : accepted.filter((x) => x !== c))} />
                        <code>{c}</code>
                      </label>
                    ))}
                  </div>
                  {prefilled.length > 0 && (
                    <p className="small"><Badge kind="info">pre-checked</Badge> from your remembered choice for this tab ({prefilled.join(", ")}). Nothing is applied until you compare again.</p>
                  )}
                  <label style={{ flexDirection: "row", alignItems: "center", marginTop: 6 }}>
                    <input type="checkbox" checked={remember} onChange={(e) => setRemember(e.target.checked)} />
                    Remember this choice for these platforms until the tab is closed (visible pre-check, revocable)
                  </label>
                  <div className="row" style={{ marginTop: 6 }}>
                    <button disabled={busy !== null} onClick={() => {
                      const m = loadMemo();
                      if (remember) m[pairKey(cmp)] = accepted; else delete m[pairKey(cmp)];
                      saveMemo(m);
                      compare(accepted);
                    }}>
                      Compare again with {accepted.length} accepted difference(s)
                    </button>
                    <button disabled={busy !== null} onClick={() => { const m = loadMemo(); delete m[pairKey(cmp)]; saveMemo(m); setAccepted([]); setPrefilled([]); setRemember(false); }}>
                      Forget the remembered choice
                    </button>
                  </div>
                </fieldset>
              )}
            </>
          )}

          {selectedQuote && (<><h3>Details</h3><QuoteDetail q={selectedQuote} /></>)}

          <div className="row" style={{ marginTop: 12 }}>
            <label>Price tolerance (bp)
              <input type="number" min={0} max={1000} value={tolerance} onChange={(e) => setTolerance(Number(e.target.value))} style={{ width: 80 }} />
            </label>
            <label>Strategy
              <input value={strategy} onChange={(e) => setStrategy(e.target.value)} size={10} />
            </label>
            <button disabled={busy !== null} onClick={doPreview}>{busy === "preview" ? "Previewing…" : "Route preview"}</button>
            <button className="primary" disabled={busy !== null} onClick={doSimulate}>{busy === "simulate" ? "Simulating…" : "Simulate"}</button>
            <span className="small muted">Accepted differences used: {accepted.length ? accepted.join(", ") : "none"}. Data is re-quoted on every action.</span>
          </div>
          {actionErr && <div className="banner error" role="alert">{actionErr}</div>}
        </section>
      )}

      {cmp && <RoutingPanel request={request(accepted)} mode={platforms?.mode ?? "?"} />}

      {preview && (
        <section className="panel" aria-labelledby="route-h">
          <h2 id="route-h">Route preview <Badge kind="muted">nothing is sent</Badge></h2>
          {preview.route ? <RouteView r={preview.route} /> : (
            <><p>No route possible:</p><ul className="tight small">{preview.no_route_reasons.map((r) => <li key={r}>{r}</li>)}</ul></>
          )}
        </section>
      )}

      {sim && (
        <section className="panel" aria-labelledby="sim-h">
          <h2 id="sim-h">Simulation result</h2>
          <SimulationView s={sim} />
        </section>
      )}
    </main>
  );
}
