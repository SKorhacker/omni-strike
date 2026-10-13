"use client";

import { useEffect, useMemo, useState } from "react";
import { api, ApiError } from "@/lib/api";
import type { ExecutionReport, RouteQuote } from "@/lib/routing";
import { accountsFor, collectSignatures, isBrowserVenue, isTxVenue, maxCost, type PreparedExecution } from "@/lib/noncustodial";
import { connect, discoverWallets, personalSign, WalletError, type WalletInfo } from "@/lib/wallet";
import { Badge } from "./components";

const errText = (e: unknown) => (e instanceof ApiError ? `${e.code}: ${e.message}` : e instanceof Error ? e.message : String(e));
const short = (a: string) => `${a.slice(0, 6)}…${a.slice(-4)}`;

/**
 * Non-custodial execution with the user's own wallet: prepare on the server, sign / send in the
 * wallet, submit. The page never holds a key; every step is approved in the wallet.
 */
export function WalletExecution({ quote, mode, onReport }: { quote: RouteQuote; mode: string; onReport: (r: ExecutionReport) => void }) {
  const mainnet = mode === "MAINNET_NONCUSTODIAL";
  const platforms = useMemo(() => quote.plan?.legs.map((l) => l.instrument.platform) ?? [], [quote]);
  const unsupported = platforms.filter((p) => !isBrowserVenue(p));
  const needsDerive = platforms.includes("derive");
  const sendsTx = platforms.some(isTxVenue);
  const estimate = Number(quote.plan?.expected_total ?? 0);

  const [wallets, setWallets] = useState<WalletInfo[]>([]);
  const [wallet, setWallet] = useState<WalletInfo | null>(null);
  const [address, setAddress] = useState<string | null>(null);
  const [subaccounts, setSubaccounts] = useState<number[] | null>(null);
  const [subaccount, setSubaccount] = useState("");
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const [steps, setSteps] = useState<string[]>([]);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => discoverWallets(setWallets), []);

  // Account switched in the wallet: forget the previous address.
  useEffect(() => {
    const p = wallet?.provider;
    if (!p?.on) return;
    const onAccounts = (...a: unknown[]) => {
      const list = a[0] as string[] | undefined;
      setAddress(list?.[0]?.toLowerCase() ?? null);
      setSubaccounts(null);
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

  const findSubaccounts = async () => {
    if (!wallet || !address) return;
    setErr(null);
    try {
      const ts = Date.now();
      const signature = await personalSign(wallet.provider, address, String(ts));
      const r = await api<{ subaccount_ids: number[] }>("/v1/noncustodial/derive/subaccounts", { method: "POST", body: { wallet: address, timestamp_ms: ts, signature } });
      setSubaccounts(r.subaccount_ids);
      if (r.subaccount_ids.length > 0) setSubaccount(String(r.subaccount_ids[0]));
    } catch (e) {
      setErr(errText(e));
    }
  };

  const run = async () => {
    if (!wallet || !address) return;
    setBusy(true);
    setErr(null);
    setSteps([]);
    const step = (t: string) => setSteps((s) => [...s, t]);
    try {
      const sub = needsDerive ? Number(subaccount) : null;
      if (needsDerive && !(Number.isInteger(sub) && (sub as number) > 0)) throw new WalletError("Invalid Derive subaccount");
      step("Server is preparing the orders (the route is consumed)");
      const prep = await api<PreparedExecution>("/v1/noncustodial/prepare", {
        method: "POST",
        body: { route_id: quote.route_id, route_hash: quote.route_hash, accounts: accountsFor(platforms, address, sub) },
      });
      step(`Maximum signed cost: ${maxCost(prep.legs).toFixed(4)} (premium + fee ceiling)`);
      const signed = await collectSignatures(wallet.provider, address, prep, step);
      step("Sending signatures to the server, verifying, then submitting to the venues");
      const report = await api<ExecutionReport>("/v1/noncustodial/submit", { method: "POST", body: signed });
      step(`Done: ${report.filled_underlying} / ${report.requested_underlying} filled`);
      onReport(report);
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(false);
      setConfirm(false);
    }
  };

  if (unsupported.length > 0) {
    return (
      <p className="small">
        <Badge kind="muted">wallet</Badge> {unsupported.join(", ")}: signing from the browser not available yet (API key or Starknet required) — use the SDK.
      </p>
    );
  }

  return (
    <div className="banner" role="group" aria-label="Non-custodial execution">
      <p>
        <Badge kind={mainnet ? "bad" : "warn"}>{mainnet ? "MAINNET — REAL FUNDS" : "TESTNET"}</Badge>{" "}
        <strong>Non-custodial</strong>: your wallet signs every order{sendsTx ? " and sends the transaction itself" : ""}. Omni Strike holds no key and cannot move your funds.
      </p>

      {!address ? (
        <div className="row">
          {wallets.length === 0 && <span className="small muted">No wallet detected (install MetaMask, Rabby, Coinbase Wallet…).</span>}
          {wallets.map((w) => (
            <button key={w.id} onClick={() => doConnect(w)}>
              {/* EIP-6963 icons are data: URIs supplied by the wallet: next/image adds nothing here. */}
              {/* eslint-disable-next-line @next/next/no-img-element */}
              {w.icon && <img src={w.icon} alt="" width={16} height={16} style={{ verticalAlign: "middle", marginRight: 6 }} />}
              Connect {w.name}
            </button>
          ))}
        </div>
      ) : (
        <>
          <p className="small">Wallet: <code>{short(address)}</code> ({wallet?.name})</p>
          {needsDerive && (
            <div className="row">
              <label>Derive subaccount
                {subaccounts && subaccounts.length > 0 ? (
                  <select value={subaccount} onChange={(e) => setSubaccount(e.target.value)}>
                    {subaccounts.map((s) => <option key={s} value={s}>{s}</option>)}
                  </select>
                ) : (
                  <input value={subaccount} onChange={(e) => setSubaccount(e.target.value.trim())} size={10} inputMode="numeric" placeholder="e.g. 12345" />
                )}
              </label>
              <button onClick={findSubaccounts} disabled={busy}>Find my subaccounts</button>
              {subaccounts && subaccounts.length === 0 && <span className="small muted">No subaccount: create one and deposit on app.derive.xyz.</span>}
            </div>
          )}
          <label style={{ flexDirection: "row", alignItems: "center", marginTop: 6 }}>
            <input type="checkbox" checked={confirm} onChange={(e) => setConfirm(e.target.checked)} />
            {mainnet
              ? `I am executing this exact route with REAL funds (estimate ${estimate.toFixed(2)}, IOC limit orders${sendsTx ? ", irreversible transaction" : ""})`
              : "I am executing this exact route on testnet with my wallet"}
          </label>
          <button className="primary" disabled={!confirm || busy || (needsDerive && !subaccount)} onClick={run} style={{ marginTop: 6 }}>
            {busy ? "In progress…" : "Sign with my wallet and execute"}
          </button>
        </>
      )}
      {steps.length > 0 && <ol className="tight small" style={{ marginTop: 6 }}>{steps.map((s, i) => <li key={i}>{s}</li>)}</ol>}
      {err && <div className="banner error" role="alert">{err}</div>}
    </div>
  );
}
