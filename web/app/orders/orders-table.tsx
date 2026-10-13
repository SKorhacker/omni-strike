import type { OrderRow } from "@/lib/routing";
import { Badge, fmtTime } from "../components";

const stateKind = (s: string) =>
  s === "FILLED" ? "ok" : s === "PARTIALLY_FILLED" ? "warn" : s === "REJECTED" || s === "UNKNOWN" ? "bad" : "muted";

export function OrdersTable({ orders, onCancel }: { orders: OrderRow[]; onCancel?: (id: string) => void }) {
  return (
    <div className="table-wrap">
      <table>
        <thead><tr><th>Created</th><th>Route / leg</th><th>Venue / contract</th><th>State</th><th>Quantity</th><th>Filled</th><th>Limit</th><th>Expected / actual price</th><th>Ack (ms)</th><th>Reconciliations</th><th>Error</th>{onCancel && <th></th>}</tr></thead>
        <tbody>
          {orders.map((o) => (
            <tr key={o.id}>
              <td className="small">{fmtTime(o.created_at)}</td>
              <td className="small">{o.route_id.slice(0, 8)} / {o.leg_index}</td>
              <td><strong>{o.platform}</strong><div className="small muted">{o.native_id} · {o.network}</div></td>
              <td><Badge kind={stateKind(o.state)}>{o.state}</Badge>{!o.is_final && <> <Badge kind="muted">not final</Badge></>}</td>
              <td className="num">{o.qty_native}</td>
              <td className="num">{o.filled_native}</td>
              <td className="num">{o.limit_price_native}</td>
              <td className="num">{o.expected_avg_per_underlying} / {o.avg_fill_price_native ?? "—"}</td>
              <td className="num">{o.submit_to_ack_ms ?? "—"}</td>
              <td className="num">{o.reconcile_attempts}</td>
              <td className="small">{o.last_error ?? ""}</td>
              {onCancel && <td>{!o.is_final && <button onClick={() => onCancel(o.id)}>Cancel</button>}</td>}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
