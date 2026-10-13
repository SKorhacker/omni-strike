"use client";

import { useEffect, useState } from "react";
import { api, ApiError } from "@/lib/api";
import type { OrderRow } from "@/lib/routing";
import { connect, discoverWallets, personalSign, type WalletInfo } from "@/lib/wallet";
import { Badge } from "../components";
import { OrdersTable } from "./orders-table";

const errText = (e: unknown) => (e instanceof ApiError ? `${e.code}: ${e.message}` : e instanceof Error ? e.message : String(e));
const short = (a: string) => `${a.slice(0, 6)}…${a.slice(-4)}`;
type Row = Record<string, unknown>;
const cell = (v: unknown) => (v === undefined || v === null || v === "" ? "—" : String(v));
const rows = (v: unknown): Row[] => (Array.isArray(v) ? (v as Row[]) : []);

/**
 * The connected user's own orders (any non-custodial venue) and Derive account (balances,
 * positions). Two read-only signatures valid 2 minutes: Omni Strike's "read my orders" message, then
 * Derive's login (a timestamp). Neither can sign an order or move funds.
 */
export function MyAccount() {
  const [wallets, setWallets] = useState<WalletInfo[]>([]);
  const [wallet, setWallet] = useState<WalletInfo | null>(null);
  const [address, setAddress] = useState<string | null>(null);
  const [subaccount, setSubaccount] = useState("");
  const [orders, setOrders] = useState<OrderRow[] | null>(null);
  const [account, setAccount] = useState<Row | null>(null);
  const [accountErr, setAccountErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => discoverWallets(setWallets), []);

  useEffect(() => {
    const p = wallet?.provider;
    if (!p?.on) return;
    const onAccounts = (...a: unknown[]) => {
      setAddress((a[0] as string[] | undefined)?.[0]?.toLowerCase() ?? null);
      setOrders(null);
      setAccount(null);
    };
    p.on("accountsChanged", onAccounts);
    return () => p.removeListener?.("accountsChanged", onAccounts);
  }, [wallet]);

  const doConnect = async (w: WalletInfo) => {
    setErr(null);
    try {
      setAddress(await connect(w.provider));
      setWallet(w);
    } catch (e) {
      setErr(errText(e));
    }
  };

  const load = async () => {
    if (!wallet || !address) return;
    setBusy(true);
    setErr(null);
    setAccountErr(null);
    try {
      const ts = Date.now();
      const message = `Omni Strike: read my orders\nWallet: ${address}\nTimestamp: ${ts}`;
      const signature = await personalSign(wallet.provider, address, message);
      const mine = await api<{ orders: OrderRow[] }>("/v1/noncustodial/orders", {
        method: "POST",
        body: { wallet: address, timestamp_ms: ts, signature, limit: 200 },
      });
      setOrders(mine.orders);
      // Derive account: Derive's own login (personal_sign of a timestamp), the given subaccount,
      // else the first one Derive lists for this wallet.
      try {
        const dts = Date.now();
        const auth = { wallet: address, timestamp_ms: dts, signature: await personalSign(wallet.provider, address, String(dts)) };
        let sub = Number(subaccount);
        if (!subaccount) {
          const s = await api<{ subaccount_ids: number[] }>("/v1/noncustodial/derive/subaccounts", { method: "POST", body: auth });
          sub = s.subaccount_ids[0] ?? 0;
          if (sub) setSubaccount(String(sub));
        }
        if (sub > 0) {
          const a = await api<{ account: Row }>("/v1/noncustodial/derive/account", { method: "POST", body: { ...auth, subaccount_id: sub } });
          setAccount(a.account);
        } else {
          setAccount(null);
          setAccountErr("No Derive subaccount for this wallet.");
        }
      } catch (e) {
        setAccount(null);
        setAccountErr(errText(e));
      }
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="panel" aria-labelledby="mine-h">
      <h2 id="mine-h">My account <Badge kind="info">wallet</Badge></h2>
      <p className="small muted">
        Your orders placed through Omni Strike and your Derive account (balance, positions). Two read-only signatures valid for 2 minutes (“read my orders”, then the Derive login): they can neither sign an order nor move funds.
      </p>
      {!address ? (
        <div className="row">
          {wallets.length === 0 && <span className="small muted">No wallet detected.</span>}
          {wallets.map((w) => (
            <button key={w.id} onClick={() => doConnect(w)}>
              {/* eslint-disable-next-line @next/next/no-img-element */}
              {w.icon && <img src={w.icon} alt="" width={16} height={16} style={{ verticalAlign: "middle", marginRight: 6 }} />}
              Connect {w.name}
            </button>
          ))}
        </div>
      ) : (
        <div className="row">
          <span className="small">Wallet: <code>{short(address)}</code> ({wallet?.name})</span>
          <label>Derive subaccount (optional)
            <input value={subaccount} onChange={(e) => setSubaccount(e.target.value.replace(/\D/g, ""))} size={10} inputMode="numeric" placeholder="auto" />
          </label>
          <button className="primary" onClick={load} disabled={busy}>{busy ? "Reading…" : "Sign and show"}</button>
        </div>
      )}
      {err && <div className="banner error" role="alert">{err}</div>}

      {account && (
        <>
          <h3>Derive · subaccount {cell(account.subaccount_id)}</h3>
          <p className="small">
            Value: <strong>{cell(account.subaccount_value)}</strong> · initial margin {cell(account.initial_margin)} · maintenance margin {cell(account.maintenance_margin)}
            {Array.isArray(account.open_orders) && <> · open orders {account.open_orders.length}</>}
          </p>
          <div className="table-wrap">
            <table>
              <thead><tr><th>Collateral</th><th>Quantity</th><th>Value</th></tr></thead>
              <tbody>
                {rows(account.collaterals).map((c, i) => (
                  <tr key={i}><td>{cell(c.asset_name)}</td><td className="num">{cell(c.amount)}</td><td className="num">{cell(c.mark_value)}</td></tr>
                ))}
                {rows(account.collaterals).length === 0 && <tr><td colSpan={3} className="small muted">No collateral.</td></tr>}
              </tbody>
            </table>
          </div>
          <div className="table-wrap">
            <table>
              <thead><tr><th>Position</th><th>Quantity</th><th>Avg price</th><th>Mark price</th><th>Unrealized PnL</th></tr></thead>
              <tbody>
                {rows(account.positions).map((p, i) => (
                  <tr key={i}>
                    <td>{cell(p.instrument_name)}</td><td className="num">{cell(p.amount)}</td><td className="num">{cell(p.average_price)}</td>
                    <td className="num">{cell(p.mark_price)}</td><td className="num">{cell(p.unrealized_pnl)}</td>
                  </tr>
                ))}
                {rows(account.positions).length === 0 && <tr><td colSpan={5} className="small muted">No positions.</td></tr>}
              </tbody>
            </table>
          </div>
        </>
      )}
      {accountErr && <p className="small muted">Derive account: {accountErr}</p>}

      {orders && (
        <>
          <h3>My orders ({orders.length})</h3>
          {orders.length === 0 ? <p className="small">No orders for this wallet.</p> : <OrdersTable orders={orders} />}
          <p className="small muted">Orders are IOC: filled immediately or cancelled by the venue, so there is never a pending order left to cancel.</p>
        </>
      )}
    </section>
  );
}
