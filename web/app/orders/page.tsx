"use client";

import Link from "next/link";
import { useCallback, useEffect, useState } from "react";
import { api, ApiError } from "@/lib/api";
import type { ExecStatsResponse, OrderRow } from "@/lib/routing";
import { Badge } from "../components";
import { MyAccount } from "./my-account";
import { OrdersTable } from "./orders-table";

const MAINNET_NC = "MAINNET_NONCUSTODIAL";

export default function OrdersPage() {
  const [orders, setOrders] = useState<OrderRow[] | null>(null);
  const [stats, setStats] = useState<ExecStatsResponse | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const s = await api<ExecStatsResponse>("/v1/exec-stats");
      setStats(s);
      // Mainnet: the journal holds users' orders and is private; each user reads theirs below.
      setOrders(s.mode === MAINNET_NC ? null : await api<OrderRow[]>("/v1/orders?limit=200"));
      setErr(null);
    } catch (e) {
      setErr(e instanceof ApiError ? `${e.code}: ${e.message}` : String(e));
    }
  }, []);

  useEffect(() => {
    const first = setTimeout(load, 0);
    const t = setInterval(load, 5000);
    return () => {
      clearTimeout(first);
      clearInterval(t);
    };
  }, [load]);

  const cancel = async (id: string) => {
    try {
      await api<OrderRow>(`/v1/orders/${id}/cancel`, { method: "POST" });
      await load();
    } catch (e) {
      setErr(e instanceof ApiError ? `${e.code}: ${e.message}` : String(e));
    }
  };

  return (
    <main>
      <header className="row" style={{ justifyContent: "space-between" }}>
        <h1>{stats?.mode === MAINNET_NC ? "Order tracking" : "Testnet order tracking"}</h1>
        <Link href="/">← Comparison and routing</Link>
      </header>
      {err && <div className="banner error" role="alert">{err} — retrying every 5 s.</div>}

      <section className="panel" aria-labelledby="stats-h">
        <h2 id="stats-h">Execution statistics by venue {stats && <Badge kind="info">{stats.mode}</Badge>}</h2>
        <p className="small muted">{stats?.note}</p>
        {stats && stats.venues.length === 0 && <p className="small">No testnet orders yet.</p>}
        {stats && stats.venues.length > 0 && (
          <div className="table-wrap">
            <table>
              <thead><tr><th>Venue</th><th>Orders</th><th>Filled</th><th>Partial</th><th>Rejected</th><th>Cancelled</th><th>Not final</th><th>Fill ratio</th><th>Mean slippage (bp)</th><th>Max slippage (bp)</th><th>Ack p50 / max (ms)</th><th>Router penalty (bp)</th></tr></thead>
              <tbody>
                {stats.venues.map((v) => (
                  <tr key={v.platform}>
                    <td><strong>{v.platform}</strong><div className="small muted">{v.sample_note}</div></td>
                    <td className="num">{v.orders}</td><td className="num">{v.filled}</td><td className="num">{v.partially_filled}</td>
                    <td className="num">{v.rejected}</td><td className="num">{v.cancelled}</td><td className="num">{v.unknown_or_open}</td>
                    <td className="num">{v.fill_ratio ?? "—"}</td><td className="num">{v.slippage_bps_mean ?? "—"}</td><td className="num">{v.slippage_bps_max ?? "—"}</td>
                    <td className="num">{v.submit_to_ack_ms_p50 ?? "—"} / {v.submit_to_ack_ms_max ?? "—"}</td>
                    <td className="num">{v.router_penalty_bps}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {stats && stats.execution_venues.length > 0 && (
          <p className="small">Execution venues: {stats.execution_venues.map((v) => JSON.stringify(v)).join(" · ")}</p>
        )}
      </section>

      <MyAccount />

      {stats?.mode !== MAINNET_NC && (
        <section className="panel" aria-labelledby="orders-h">
          <h2 id="orders-h">All server orders ({orders?.length ?? "…"})</h2>
          {orders && orders.length === 0 && <p className="small">No orders. Orders only exist in TESTNET mode.</p>}
          {orders && orders.length > 0 && <OrdersTable orders={orders} onCancel={cancel} />}
        </section>
      )}
    </main>
  );
}
