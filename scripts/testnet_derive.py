"""Derive v3 TESTNET helper — readiness check and measured order series.

  python scripts/testnet_derive.py check
  python scripts/testnet_derive.py run --orders 10 --i-confirm-testnet-orders

Reads OA_DERIVE_TESTNET_{WALLET,SUBACCOUNT_ID,SIGNER_KEY} from .env.testnet; never prints them
(only set/missing and PUBLIC addresses). Talks to testnet.api.derive.xyz/v3 only.
"""
import argparse
import json
import re
import sys
import time
from datetime import datetime, timezone
from decimal import Decimal

import testnet_aevo as common  # shared: load_env, http, pick_liquid, write_report, SERVER

DERIVE_TESTNET = "https://testnet.api.derive.xyz/v3"  # constant: never mainnet
REQUIRED = ["OA_DERIVE_TESTNET_WALLET", "OA_DERIVE_TESTNET_SUBACCOUNT_ID", "OA_DERIVE_TESTNET_SIGNER_KEY"]
HOW_TO = """
Derive testnet fields are not filled (details: docs/testnet.md):
  1. NEW wallet only for testnet -> https://testnet.app.derive.xyz -> connect.
  2. Deposit -> Mint test USDC -> deposit it into a subaccount (you do these steps in the app).
  3. Fill locally in .env.testnet: OA_DERIVE_TESTNET_WALLET, OA_DERIVE_TESTNET_SUBACCOUNT_ID,
     OA_DERIVE_TESTNET_SIGNER_KEY (session key with trade scope, or the throwaway test wallet key).
"""


def validate(env):
    problems, info = [], {}
    for k in REQUIRED:
        if not env.get(k):
            problems.append(f"{k}: missing")
    w = env.get("OA_DERIVE_TESTNET_WALLET", "")
    if w and not re.fullmatch(r"0x[0-9a-fA-F]{40}", w):
        problems.append("OA_DERIVE_TESTNET_WALLET: malformed (0x + 40 hex)")
    sid = env.get("OA_DERIVE_TESTNET_SUBACCOUNT_ID", "")
    if sid and not sid.isdigit():
        problems.append("OA_DERIVE_TESTNET_SUBACCOUNT_ID: must be an integer")
    k = env.get("OA_DERIVE_TESTNET_SIGNER_KEY", "")
    if k:
        if not re.fullmatch(r"(0x)?[0-9a-fA-F]{64}", k):
            problems.append("OA_DERIVE_TESTNET_SIGNER_KEY: malformed (0x + 64 hex PRIVATE key)")
        else:
            from eth_account import Account
            info["signer_address"] = Account.from_key(k).address
            info["session_key"] = bool(w) and info["signer_address"].lower() != w.lower()
    return problems, info


def derive_private(env, method, params):
    from eth_account import Account
    from eth_account.messages import encode_defunct
    ts = str(int(time.time() * 1000))
    sig = Account.sign_message(encode_defunct(text=ts), env["OA_DERIVE_TESTNET_SIGNER_KEY"]).signature.hex()
    hdr = {"X-DeriveWallet": env["OA_DERIVE_TESTNET_WALLET"], "X-DeriveTimestamp": ts, "X-DeriveSignature": "0x" + sig.removeprefix("0x")}
    return common.http("POST", f"{DERIVE_TESTNET}/{method}", params, hdr)


def cmd_check(_a):
    env = common.load_env()
    problems, info = validate(env)
    print("== .env.testnet (Derive)")
    for k in REQUIRED:
        print(f"   {k}: {'set' if env.get(k) else 'MISSING'}")
    print(f"   OA_KILL_SWITCH={env.get('OA_KILL_SWITCH', 'true')}")
    if info.get("signer_address"):
        print(f"   signer ADDRESS (public): {info['signer_address']}  ({'session key' if info['session_key'] else 'wallet signs directly'})")
    ready = not problems
    for p in problems:
        print(f"   PROBLEM: {p}")
    if problems and env.get("OA_DERIVE_TESTNET_WALLET") and info.get("signer_address") and not env.get("OA_DERIVE_TESTNET_SUBACCOUNT_ID"):
        # Helper: list the wallet's subaccount ids (read-only) so they can be copied into .env.testnet.
        s, r = derive_private(env, "private/get_subaccounts", {"wallet": env["OA_DERIVE_TESTNET_WALLET"]})
        ids = (r.get("result") or {}).get("subaccount_ids") if isinstance(r, dict) else None
        print(f"   subaccounts of this wallet: {ids if ids else f'none found (HTTP {s}: {r.get("error") if isinstance(r, dict) else r})'}")
    if problems:
        print(HOW_TO)
    else:
        print("== Derive testnet authentication (private/get_subaccount, read-only)")
        s, r = derive_private(env, "private/get_subaccount", {"subaccount_id": int(env["OA_DERIVE_TESTNET_SUBACCOUNT_ID"])})
        err = r.get("error") if isinstance(r, dict) else None
        if s != 200 or err:
            ready = False
            print(f"   FAILED HTTP {s}: {err or r}")
            print("   - auth error: wallet/signer mismatch, or the session key is not registered for this wallet")
            print("   - subaccount error: wrong OA_DERIVE_TESTNET_SUBACCOUNT_ID (must belong to the wallet)")
        else:
            res = r["result"]
            usdc = next((c for c in res.get("collaterals", []) if str(c.get("asset_name", "")).upper() == "USDC"), None)
            amount = Decimal(usdc["amount"]) if usdc else Decimal(0)
            print(f"   OK subaccount {res.get('subaccount_id')}  USDC {amount}  value {res.get('subaccount_value')}  initial margin {res.get('initial_margin')}")
            if amount <= 0:
                ready = False
                print("   PROBLEM: no USDC in the subaccount: mint test USDC in the app and deposit it (docs/testnet.md)")
    print("== testnet oa-server (8081)")
    s, st = common.http("GET", common.SERVER + "/v1/exec-stats")
    venues = [v for v in (st.get("execution_venues", []) if s == 200 else []) if "derive" in json.dumps(v)]
    print(f"   Derive execution venue attached: {venues if venues else 'NO (fill .env.testnet, restart the server)'}")
    if not venues:
        ready = False
    ks = env.get("OA_KILL_SWITCH", "true")
    print("\nREADY FOR DERIVE TESTNET ORDERS" if ready and ks == "false" else
          "\nNOT READY" + (" (set OA_KILL_SWITCH=false and restart to trade)" if ready else ""))
    return 0 if ready else 1


def cmd_run(a):
    if not a.i_confirm_testnet_orders:
        sys.exit("Refusing: add --i-confirm-testnet-orders to send TESTNET orders (test funds only).")
    env = common.load_env()
    if env.get("OA_KILL_SWITCH") != "false":
        sys.exit("Refusing: OA_KILL_SWITCH is not 'false' in .env.testnet.")
    if cmd_check(a) != 0:
        sys.exit("Refusing: Derive readiness check failed (see above).")
    s, p = common.http("GET", common.SERVER + "/v1/platforms")
    if s != 200 or p.get("mode") != "TESTNET":
        sys.exit("Refusing: server on 8081 is not in TESTNET mode.")
    qty = {"BTC": a.qty_btc, "ETH": a.qty_eth}
    started = datetime.now(timezone.utc)
    results, tried = [], 0
    for c in common.pick_liquid(qty, platform="derive"):
        if len(results) >= a.orders:
            break
        tried += 1
        body = {"underlying": c["underlying"], "option_kind": c["option_kind"], "strike": c["strike"], "expiry": c["expiry"],
                "side": "buy", "quantity": c["qty"], "accepted_differences": common.ACCEPT, "split": {"allow_split": False}}
        s, q = common.http("POST", common.SERVER + "/v1/routes/quote", body, timeout=60)
        if s != 200 or not q.get("executable") or not q["plan"] or q["plan"]["legs"][0]["instrument"]["platform"] != "derive":
            continue
        s, rep = common.http("POST", common.SERVER + "/v1/routes/execute-testnet", {"route_id": q["route_id"], "route_hash": q["route_hash"]}, timeout=60)
        leg = q["plan"]["legs"][0]
        results.append({"contract": c, "quote": {"avg": leg["expected_avg_per_underlying"], "total": leg["expected_total"], "limit": leg["limit_price_native"]}, "http": s, "report": rep})
        print(f"  #{len(results)} {leg['instrument']['native_id']} qty {c['qty']}: HTTP {s} "
              f"{[o['state'] for o in rep.get('orders', [])] if s == 200 else rep.get('error')}")
        time.sleep(2)
    print(f"\n{len(results)} orders sent ({tried} contracts examined). Waiting 20 s for reconciliation...")
    time.sleep(20 if results else 0)
    s, orders = common.http("GET", common.SERVER + "/v1/orders?limit=200")
    s2, stats = common.http("GET", common.SERVER + "/v1/exec-stats")
    ids = {o["id"] for r in results for o in r["report"].get("orders", [])}
    common.write_report(started, results, [o for o in (orders if s == 200 else []) if o["id"] in ids], stats if s2 == 200 else {}, tried)
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("check")
    x = sub.add_parser("run")
    x.add_argument("--orders", type=int, default=10)
    x.add_argument("--qty-btc", default="0.01")
    x.add_argument("--qty-eth", default="0.1")
    x.add_argument("--i-confirm-testnet-orders", action="store_true")
    a = ap.parse_args()
    sys.exit({"check": cmd_check, "run": cmd_run}[a.cmd](a))


if __name__ == "__main__":
    main()
