"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import styles from "./live.module.css";

type Point = { t: string; saving_pct: number };
type VenueQuote = { platform: string; total: string };
type Row = {
  id: string;
  underlying: string;
  option_kind: "call" | "put";
  strike: string;
  expiry: string;
  quantity: string;
  best_venue: string | null;
  best_total: string | null;
  derive_total: string | null;
  saving_usd: number | null;
  saving_pct: number | null;
  venues: VenueQuote[];
  updated_at: string | null;
  history: Point[];
};
type Feed = { t: string; contract: string; best_venue: string; saving_usd: number; saving_pct: number };
type SeriesPoint = { t: string; median_saving_pct: number; win_rate: number };
type Snapshot = {
  updated_at: string | null;
  wording: string;
  basket_size: number;
  summary: {
    since: string | null;
    observations: number;
    observations_beating_derive: number;
    hypothetical_saving_usd: number;
    hypothetical_derive_cost_usd: number;
    win_rate: number;
    median_saving_pct: number;
    mean_saving_pct: number;
    wins_by_venue: Record<string, number>;
  };
  contracts: Row[];
  feed: Feed[];
  series: SeriesPoint[];
};

const VENUE_COLORS: Record<string, string> = {
  derive: "#8b8fff",
  okx: "#e8e8e8",
  bybit: "#f7a600",
  binance: "#f0b90b",
  deribit: "#22d3a6",
  gate: "#2354e6",
  aevo: "#ff5c8a",
  paradex: "#a46bff",
  bullish: "#ff7a3d",
  coincall: "#3dc6ff",
  delta: "#5ad15a",
  thetanuts: "#ffd166",
};
const NAMES: Record<string, string> = {
  derive: "Derive",
  okx: "OKX",
  bybit: "Bybit",
  binance: "Binance",
  deribit: "Deribit",
  gate: "Gate",
  aevo: "Aevo",
  paradex: "Paradex",
  bullish: "Bullish",
  coincall: "Coincall",
  delta: "Delta",
  thetanuts: "Thetanuts",
};
const vname = (v: string | null | undefined) => (v ? NAMES[v] ?? v : "—");
const vcolor = (v: string | null | undefined) => (v ? VENUE_COLORS[v] ?? "#9aa1ad" : "#9aa1ad");

const usd = (n: number, d = 0) =>
  n.toLocaleString("en-US", { style: "currency", currency: "USD", maximumFractionDigits: d, minimumFractionDigits: d });
const pct = (n: number | null | undefined, d = 1) => (n == null ? "—" : `${n.toFixed(d)}%`);

/** Smoothly tween a number toward its target (requestAnimationFrame). */
function useTween(target: number, ms = 900) {
  const [v, setV] = useState(target);
  const from = useRef(target);
  const start = useRef(0);
  const cur = useRef(target);
  useEffect(() => {
    from.current = cur.current;
    start.current = performance.now();
    let raf = 0;
    const step = (now: number) => {
      const k = Math.min(1, (now - start.current) / ms);
      const e = 1 - Math.pow(1 - k, 3);
      const x = from.current + (target - from.current) * e;
      cur.current = x;
      setV(x);
      if (k < 1) raf = requestAnimationFrame(step);
    };
    raf = requestAnimationFrame(step);
    return () => cancelAnimationFrame(raf);
  }, [target, ms]);
  return v;
}

function Sparkline({ pts, w = 120, h = 28 }: { pts: number[]; w?: number; h?: number }) {
  if (pts.length < 2) return <svg width={w} height={h} />;
  const max = Math.max(...pts, 0.01);
  const min = Math.min(...pts, 0);
  const span = max - min || 1;
  const xy = pts.map((p, i) => [(i / (pts.length - 1)) * w, h - 2 - ((p - min) / span) * (h - 4)]);
  const d = xy.map(([x, y], i) => `${i ? "L" : "M"}${x.toFixed(1)},${y.toFixed(1)}`).join(" ");
  const last = xy[xy.length - 1];
  return (
    <svg width={w} height={h} className={styles.spark}>
      <path d={`${d} L${w},${h} L0,${h} Z`} className={styles.sparkFill} />
      <path d={d} className={styles.sparkLine} />
      <circle cx={last[0]} cy={last[1]} r={2.5} className={styles.sparkDot} />
    </svg>
  );
}

function AreaChart({ series }: { series: SeriesPoint[] }) {
  const w = 760;
  const h = 220;
  const pad = 28;
  if (series.length < 2) {
    return <div className={styles.chartEmpty}>Collecting live observations…</div>;
  }
  const ys = series.map((s) => s.median_saving_pct);
  const max = Math.max(...ys, 1) * 1.15;
  const x = (i: number) => pad + (i / (series.length - 1)) * (w - pad * 2);
  const y = (v: number) => h - pad - (v / max) * (h - pad * 2);
  const yw = (v: number) => h - pad - v * (h - pad * 2);
  const line = series.map((s, i) => `${i ? "L" : "M"}${x(i).toFixed(1)},${y(s.median_saving_pct).toFixed(1)}`).join(" ");
  const win = series.map((s, i) => `${i ? "L" : "M"}${x(i).toFixed(1)},${yw(s.win_rate).toFixed(1)}`).join(" ");
  const ticks = [0, max / 2, max].map((v) => ({ v, y: y(v) }));
  const lastI = series.length - 1;
  return (
    <svg viewBox={`0 0 ${w} ${h}`} className={styles.chart} preserveAspectRatio="none">
      <defs>
        <linearGradient id="g" x1="0" x2="0" y1="0" y2="1">
          <stop offset="0%" stopColor="var(--glow)" stopOpacity="0.55" />
          <stop offset="100%" stopColor="var(--glow)" stopOpacity="0" />
        </linearGradient>
      </defs>
      {ticks.map((t) => (
        <g key={t.v}>
          <line x1={pad} x2={w - pad} y1={t.y} y2={t.y} className={styles.grid} />
          <text x={4} y={t.y + 4} className={styles.axis}>
            {t.v.toFixed(1)}%
          </text>
        </g>
      ))}
      <path d={`${line} L${x(lastI)},${h - pad} L${x(0)},${h - pad} Z`} fill="url(#g)" />
      <path d={line} className={styles.mainLine} />
      <path d={win} className={styles.winLine} />
      <circle cx={x(lastI)} cy={y(series[lastI].median_saving_pct)} r={5} className={styles.pulseDot} />
    </svg>
  );
}

export default function LivePage() {
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const r = await fetch("/api/v1/live/savings", { cache: "no-store" });
        if (!r.ok) throw new Error(`HTTP ${r.status}`);
        const j = (await r.json()) as Snapshot;
        if (alive) {
          setSnap(j);
          setErr(null);
        }
      } catch (e) {
        if (alive) setErr(String(e));
      }
    };
    tick();
    const a = setInterval(tick, 1500);
    const b = setInterval(() => setNow(Date.now()), 1000);
    return () => {
      alive = false;
      clearInterval(a);
      clearInterval(b);
    };
  }, []);

  const s = snap?.summary;
  const saved = useTween(s?.hypothetical_saving_usd ?? 0, 1200);
  const median = useTween(s?.median_saving_pct ?? 0);
  const winRate = useTween((s?.win_rate ?? 0) * 100);
  const obs = useTween(s?.observations ?? 0);

  const leaderboard = useMemo(() => {
    const e = Object.entries(s?.wins_by_venue ?? {}).sort((a, b) => b[1] - a[1]);
    const max = Math.max(1, ...e.map(([, n]) => n));
    return e.map(([v, n]) => ({ v, n, w: (n / max) * 100 }));
  }, [s?.wins_by_venue]);

  const rows = useMemo(
    () => [...(snap?.contracts ?? [])].sort((a, b) => (b.saving_pct ?? -1) - (a.saving_pct ?? -1)),
    [snap?.contracts],
  );

  const age = snap?.updated_at ? Math.max(0, Math.round((now - Date.parse(snap.updated_at)) / 1000)) : null;
  const derivePct = s && s.hypothetical_derive_cost_usd > 0 ? (s.hypothetical_saving_usd / s.hypothetical_derive_cost_usd) * 100 : 0;

  return (
    <div className={styles.page}>
      <header className={styles.header}>
        <div className={styles.brand}>
          <span className={styles.logo}>◆</span>
          <span>
            <b>Omni Strike</b> <span className={styles.dim}>· live best-execution tracker</span>
          </span>
        </div>
        <div className={styles.live}>
          <span className={`${styles.dot} ${err ? styles.dotBad : ""}`} />
          {err ? "reconnecting…" : "LIVE"}
          <span className={styles.dim}>{age != null ? ` · updated ${age}s ago` : ""}</span>
        </div>
      </header>

      <section className={styles.hero}>
        <div className={styles.heroMain}>
          <div className={styles.kicker}>Estimated savings vs. trading on Derive alone</div>
          <div className={styles.bigNumber}>{usd(saved)}</div>
          <div className={styles.sub}>
            if each of the <b>{Math.round(obs).toLocaleString()}</b> live comparisons had been an order (1 BTC / 10 ETH)
            {" · "}
            <b>{pct(derivePct)}</b> of Derive&apos;s cost
          </div>
        </div>
        <div className={styles.kpis}>
          <div className={styles.kpi}>
            <div className={styles.kpiLabel}>Cheaper than Derive</div>
            <div className={styles.kpiValue}>{winRate.toFixed(0)}%</div>
            <div className={styles.bar}>
              <span style={{ width: `${winRate}%` }} />
            </div>
          </div>
          <div className={styles.kpi}>
            <div className={styles.kpiLabel}>Median saving</div>
            <div className={styles.kpiValue}>{median.toFixed(2)}%</div>
            <div className={styles.kpiHint}>per order, basket now</div>
          </div>
          <div className={styles.kpi}>
            <div className={styles.kpiLabel}>Contracts tracked</div>
            <div className={styles.kpiValue}>{snap?.basket_size ?? "—"}</div>
            <div className={styles.kpiHint}>BTC + ETH, near the money</div>
          </div>
        </div>
      </section>

      <div className={styles.grid2}>
        <section className={styles.panel}>
          <div className={styles.panelHead}>
            <h2>Median saving vs Derive — live</h2>
            <div className={styles.legend}>
              <span className={styles.legMain} /> median saving % <span className={styles.legWin} /> share of orders beating Derive
            </div>
          </div>
          <AreaChart series={snap?.series ?? []} />
        </section>

        <section className={styles.panel}>
          <div className={styles.panelHead}>
            <h2>Where the best price is</h2>
          </div>
          <div className={styles.board}>
            {leaderboard.length === 0 && <div className={styles.dim}>waiting for the first wins…</div>}
            {leaderboard.map((l) => (
              <div key={l.v} className={styles.boardRow}>
                <span className={styles.venueTag} style={{ borderColor: vcolor(l.v), color: vcolor(l.v) }}>
                  {vname(l.v)}
                </span>
                <div className={styles.boardBar}>
                  <span style={{ width: `${l.w}%`, background: vcolor(l.v) }} />
                </div>
                <span className={styles.boardN}>{l.n}</span>
              </div>
            ))}
          </div>
        </section>
      </div>

      <div className={styles.grid2}>
        <section className={styles.panel}>
          <div className={styles.panelHead}>
            <h2>Live basket</h2>
            <span className={styles.dim}>best eligible venue vs Derive, same contract & size, fees included</span>
          </div>
          <div className={styles.tableWrap}>
            <table className={styles.table}>
              <thead>
                <tr>
                  <th>Contract</th>
                  <th className={styles.num}>Derive</th>
                  <th className={styles.num}>Best</th>
                  <th>Venue</th>
                  <th className={styles.num}>Saving</th>
                  <th>Trend</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => (
                  <tr key={`${r.id}-${r.updated_at}`} className={styles.flash}>
                    <td>
                      <b>{r.underlying}</b> {Number(r.strike).toLocaleString()} {r.option_kind === "call" ? "C" : "P"}{" "}
                      <span className={styles.dim}>
                        {new Date(r.expiry).toLocaleDateString("en-GB", { day: "2-digit", month: "short" })} · {r.quantity}
                      </span>
                    </td>
                    <td className={styles.num}>{r.derive_total ? usd(Number(r.derive_total), 2) : "—"}</td>
                    <td className={styles.num}>{r.best_total ? usd(Number(r.best_total), 2) : "—"}</td>
                    <td>
                      {r.best_venue && (
                        <span className={styles.venueTag} style={{ borderColor: vcolor(r.best_venue), color: vcolor(r.best_venue) }}>
                          {vname(r.best_venue)}
                        </span>
                      )}
                    </td>
                    <td className={`${styles.num} ${(r.saving_pct ?? 0) > 0 ? styles.good : styles.dim}`}>
                      {r.saving_usd != null ? `${usd(r.saving_usd, 2)} · ${pct(r.saving_pct)}` : "—"}
                    </td>
                    <td>
                      <Sparkline pts={r.history.map((h) => h.saving_pct)} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>

        <section className={styles.panel}>
          <div className={styles.panelHead}>
            <h2>Live feed</h2>
          </div>
          <ul className={styles.feed}>
            {(snap?.feed ?? []).slice(0, 14).map((f) => (
              <li key={`${f.t}-${f.contract}`} className={styles.feedItem}>
                <span className={styles.feedTime}>{new Date(f.t).toLocaleTimeString("en-GB")}</span>
                <span className={styles.feedContract}>{f.contract}</span>
                <span className={styles.venueTag} style={{ borderColor: vcolor(f.best_venue), color: vcolor(f.best_venue) }}>
                  {vname(f.best_venue)}
                </span>
                <span className={styles.good}>
                  −{usd(f.saving_usd, 2)} <span className={styles.dim}>({pct(f.saving_pct)})</span>
                </span>
              </li>
            ))}
          </ul>
        </section>
      </div>

      <footer className={styles.footer}>
        {snap?.wording ??
          "Estimated total cost (visible books + documented fees). Not a fill guarantee."}{" "}
        Centralised venues require the user&apos;s own account. Settlement index/currency differences are accepted and flagged. Not
        investment advice.
      </footer>
    </div>
  );
}
