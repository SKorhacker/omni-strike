"""Aevo TESTNET helper — readiness check, signing-key registration, measured order series.

  python scripts/testnet_aevo.py check
  python scripts/testnet_aevo.py register          # path B (you run it; asks for the wallet key, hidden)
  python scripts/testnet_aevo.py run --orders 10 --i-confirm-testnet-orders

Secrets are read from .env.testnet and NEVER printed (only "set / missing / malformed" and public
addresses). Everything talks to TESTNET endpoints only (constants below). Nothing here can reach
mainnet, and the server refuses LIVE at startup.
"""

import argparse
import getpass
import hashlib
import hmac
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from decimal import Decimal

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
ENV_FILE = os.path.join(ROOT, ".env.testnet")
AEVO_TESTNET_REST = "https://api-testnet.aevo.xyz"   # constant: never mainnet
SERVER = "http://127.0.0.1:8081"                     # testnet oa-server
REQUIRED = ["OA_AEVO_TESTNET_WALLET", "OA_AEVO_TESTNET_API_KEY", "OA_AEVO_TESTNET_API_SECRET", "OA_AEVO_TESTNET_SIGNING_KEY"]
ACCEPT = ["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"]
HOW_TO = """
.env.testnet is not filled. Do this (details: docs/testnet.md):
  1. Create a NEW wallet used only for the Aevo testnet (never a wallet holding real funds).
  2. Either path A (UI) or path B (`python scripts/testnet_aevo.py register`) to obtain:
       OA_AEVO_TESTNET_WALLET, OA_AEVO_TESTNET_API_KEY, OA_AEVO_TESTNET_API_SECRET,
       OA_AEVO_TESTNET_SIGNING_KEY (private key of the SIGNING key, not of the wallet)
  3. Edit .env.testnet locally (never paste these values in a chat), then re-run `check`.
"""


def load_env():
    if not os.path.exists(ENV_FILE):
        sys.exit(f"{ENV_FILE} not found. Copy .env.testnet.example to .env.testnet first.\n{HOW_TO}")
    env = {}
    with open(ENV_FILE, encoding="utf-8-sig") as f:
        for line in f:
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, v = line.split("=", 1)
                env[k.strip()] = v.strip()
    return env


def validate(env):
    """Returns (problems, info) without ever exposing secret values."""
    problems, info = [], {}
    for k in REQUIRED:
        if not env.get(k):
            problems.append(f"{k}: missing")
    w = env.get("OA_AEVO_TESTNET_WALLET", "")
    if w and not re.fullmatch(r"0x[0-9a-fA-F]{40}", w):
        problems.append("OA_AEVO_TESTNET_WALLET: malformed (expected 0x + 40 hex)")
    sk = env.get("OA_AEVO_TESTNET_SIGNING_KEY", "")
    if sk:
        if not re.fullmatch(r"(0x)?[0-9a-fA-F]{64}", sk):
            problems.append("OA_AEVO_TESTNET_SIGNING_KEY: malformed (expected 0x + 64 hex PRIVATE key)")
        else:
            from eth_account import Account
            info["signing_key_address"] = Account.from_key(sk).address
            if w and info["signing_key_address"].lower() == w.lower():
                info["warning"] = "the signing key is the wallet key itself: allowed by EIP-712 but use a separate signing key"
    info["kill_switch"] = env.get("OA_KILL_SWITCH", "true")
    info["database_is_testnet"] = "testnet" in env.get("DATABASE_URL", "").rsplit("/", 1)[-1]
    if not info["database_is_testnet"]:
        problems.append("DATABASE_URL must point to a database whose name contains 'testnet'")
    return problems, info


def http(method, url, body=None, headers=None, timeout=30):
    data = body.encode() if isinstance(body, str) else (json.dumps(body).encode() if body is not None else None)
    h = {"content-type": "application/json", "user-agent": "omni-strike-testnet/0.1"}
    h.update(headers or {})
    req = urllib.request.Request(url, data=data, method=method, headers=h)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw or b"null")
        except json.JSONDecodeError:
            return e.code, {"raw": raw[:200].decode(errors="replace")}
    except urllib.error.URLError as e:
        return 0, {"error": str(e.reason)}


def aevo_private(env, method, path, body=""):
    """HMAC-authenticated call to the Aevo TESTNET REST API (docs: urls/authentication)."""
    ts = str(time.time_ns())
    msg = f"{env['OA_AEVO_TESTNET_API_KEY']},{ts},{method.upper()},{path},{body}"
    sig = hmac.new(env["OA_AEVO_TESTNET_API_SECRET"].encode(), msg.encode(), hashlib.sha256).hexdigest()
    hdr = {"AEVO-KEY": env["OA_AEVO_TESTNET_API_KEY"], "AEVO-TIMESTAMP": ts, "AEVO-SIGNATURE": sig}
    return http(method, AEVO_TESTNET_REST + path, body if body else None, hdr)


def cmd_check(_a):
    env = load_env()
    problems, info = validate(env)
    print("== .env.testnet")
    for k in REQUIRED:
        print(f"   {k}: {'set' if env.get(k) else 'MISSING'}")
    print(f"   OA_KILL_SWITCH={info['kill_switch']}  (orders need exactly 'false')")
    if info.get("signing_key_address"):
        print(f"   signing key ADDRESS (public): {info['signing_key_address']}")
    if info.get("warning"):
        print(f"   note: {info['warning']}")
    ready = not problems
    for p in problems:
        print(f"   PROBLEM: {p}")
    if problems:
        print(HOW_TO)
    else:
        print("== Aevo testnet authentication (GET /account, read-only)")
        s, acc = aevo_private(env, "GET", "/account")
        if s != 200:
            ready = False
            err = (acc or {}).get("error", acc)
            print(f"   FAILED HTTP {s}: {err}")
            print("   - UNAUTHORIZED / INVALID_SIGNATURE: API key/secret wrong, or created on MAINNET (app.aevo.xyz) instead of testnet.aevo.xyz")
            print("   - clock skew: the timestamp is in nanoseconds; check the PC clock is synchronised")
        else:
            acct = acc.get("account", "")
            same = acct.lower() == env["OA_AEVO_TESTNET_WALLET"].lower()
            print(f"   OK account {acct} (matches OA_AEVO_TESTNET_WALLET: {same})")
            print(f"   balance {acc.get('balance')}  available {acc.get('available_balance')}  equity {acc.get('equity')}")
            keys = json.dumps(acc.get("signing_keys", [])).lower()
            sk_ok = info.get("signing_key_address", "x").lower() in keys
            print(f"   signing key registered on the account: {sk_ok}")
            if not same:
                ready = False
                print("   PROBLEM: the API key belongs to another account than OA_AEVO_TESTNET_WALLET")
            if not sk_ok:
                ready = False
                print("   PROBLEM: signing key not registered -> orders would fail with an invalid signature (use path B: register)")
            try:
                available = Decimal(acc.get("available_balance") or "0")
            except Exception:
                available = Decimal(0)
            if available <= 0:
                ready = False
                print("   PROBLEM: no available testnet USDC -> every order would be rejected. Fund the account first (docs/testnet.md step 4)")
    print("== testnet oa-server (8081)")
    s, rd = http("GET", SERVER + "/health/ready")
    if s == 0:
        print("   not running. Start: $env:OA_ENV_FILE='.env.testnet'; .\\target\\testnet\\release\\oa-server.exe")
        ready = False
    else:
        print(f"   ready={rd.get('ready')} database={rd.get('database')} mode={rd.get('mode')}")
        s, st = http("GET", SERVER + "/v1/exec-stats")
        venues = st.get("execution_venues", []) if s == 200 else []
        print(f"   execution venues attached: {venues if venues else 'NONE (restart the server after filling .env.testnet)'}")
        if not venues:
            ready = False
    print("\nREADY FOR TESTNET ORDERS" if ready and info["kill_switch"] == "false" else
          "\nNOT READY" + (" (everything else OK: set OA_KILL_SWITCH=false and restart when you want to trade)" if ready else ""))
    return 0 if ready else 1


def cmd_register(a):
    """Path B: register a NEW locally generated signing key for the testnet account (POST /register).
    Per docs (api-key-setup/api-key-setup-api, urls/private-api/postregister). The docs disagree on
    the expiry unit; we sign and send the same value in seconds. If Aevo answers an error, use path A."""
    from eth_account import Account
    from eth_account.messages import encode_typed_data
    env = load_env()
    if all(env.get(k) for k in REQUIRED) and not a.force:
        sys.exit("All Aevo fields are already set in .env.testnet; refusing to overwrite (use --force).")
    print("This registers a new SIGNING key on the AEVO TESTNET account of your test wallet.")
    wallet_pk = getpass.getpass("Test wallet PRIVATE key (input hidden, used once, never stored): ").strip()
    wallet = Account.from_key(wallet_pk)
    signer = Account.create()
    domain = {"name": "Aevo Testnet", "version": "1", "chainId": 11155111}
    expiry = int(time.time()) + 365 * 86400
    reg = encode_typed_data(domain_data=domain, message_types={"Register": [{"name": "key", "type": "address"}, {"name": "expiry", "type": "uint256"}]},
                            message_data={"key": signer.address, "expiry": expiry})
    account_sig = wallet.sign_message(reg).signature.hex()
    sk = encode_typed_data(domain_data=domain, message_types={"SignKey": [{"name": "account", "type": "address"}]},
                           message_data={"account": wallet.address})
    signing_key_sig = signer.sign_message(sk).signature.hex()
    del wallet_pk
    body = {"account": wallet.address, "signing_key": signer.address, "expiry": str(expiry),
            "account_signature": "0x" + account_sig.removeprefix("0x"), "signing_key_signature": "0x" + signing_key_sig.removeprefix("0x")}
    s, r = http("POST", AEVO_TESTNET_REST + "/register", body)
    if s != 200 or not r.get("api_key"):
        print(f"Aevo answered HTTP {s}: {(r or {}).get('error', r)}")
        print("Registration not confirmed. Use path A (UI) in docs/testnet.md. Nothing was written.")
        return 1
    values = {"OA_AEVO_TESTNET_WALLET": wallet.address, "OA_AEVO_TESTNET_API_KEY": r["api_key"],
              "OA_AEVO_TESTNET_API_SECRET": r["api_secret"], "OA_AEVO_TESTNET_SIGNING_KEY": "0x" + signer.key.hex().removeprefix("0x")}
    lines = open(ENV_FILE, encoding="utf-8-sig").read().splitlines()
    out, done = [], set()
    for line in lines:
        k = line.split("=", 1)[0].strip() if "=" in line and not line.lstrip().startswith("#") else None
        if k in values:
            out.append(f"{k}={values[k]}")
            done.add(k)
        else:
            out.append(line)
    out += [f"{k}={v}" for k, v in values.items() if k not in done]
    with open(ENV_FILE, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(out) + "\n")
    print(f"OK: signing key {signer.address} registered for {wallet.address}; read_only={r.get('read_only')}.")
    print("Wallet, API key/secret and signing key written to .env.testnet (values not shown). Next: check.")
    return 0


def pick_liquid(qty, platform="aevo"):
    """Contracts listed on `platform`, near the money, 2-30 days; via the server's own routing."""
    out = []
    now = datetime.now(timezone.utc).timestamp()
    for u in ("BTC", "ETH"):
        for kind in ("call", "put"):
            s, r = http("GET", f"{SERVER}/v1/instruments?underlying={u}&option_kind={kind}&limit=1000", timeout=60)
            if s != 200:
                continue
            groups = [g for g in r["groups"] if any(l["key"]["platform"] == platform for l in g["listings"])
                      and 2 * 86400 < datetime.fromisoformat(g["expiry"].replace("Z", "+00:00")).timestamp() - now < 30 * 86400]
            groups.sort(key=lambda g: (g["expiry"], float(g["strike"])))
            # middle strikes of each expiry first (closest to the money)
            by_exp = {}
            for g in groups:
                by_exp.setdefault(g["expiry"], []).append(g)
            for exp, gs in sorted(by_exp.items())[:3]:
                mid = len(gs) // 2
                for g in sorted(gs, key=lambda g: abs(gs.index(g) - mid))[:6]:
                    out.append({"underlying": u, "option_kind": kind, "strike": g["strike"], "expiry": exp, "qty": qty[u]})
    return out


def cmd_run(a):
    if not a.i_confirm_testnet_orders:
        sys.exit("Refusing: add --i-confirm-testnet-orders to send TESTNET orders (test funds only).")
    env = load_env()
    problems, info = validate(env)
    if problems:
        print("\n".join("PROBLEM: " + p for p in problems))
        sys.exit(HOW_TO)
    if info["kill_switch"] != "false":
        sys.exit("Refusing: OA_KILL_SWITCH is not 'false' in .env.testnet (restart the server after changing it).")
    if cmd_check(a) != 0:
        sys.exit("Refusing: readiness check failed (see above).")
    s, p = http("GET", SERVER + "/v1/platforms")
    if s != 200 or p.get("mode") != "TESTNET":
        sys.exit(f"Refusing: server on {SERVER} is not in TESTNET mode ({p.get('mode') if s == 200 else s}).")
    qty = {"BTC": a.qty_btc, "ETH": a.qty_eth}
    started = datetime.now(timezone.utc)
    results, tried = [], 0
    for c in pick_liquid(qty):
        if len(results) >= a.orders:
            break
        tried += 1
        body = {"underlying": c["underlying"], "option_kind": c["option_kind"], "strike": c["strike"], "expiry": c["expiry"],
                "side": "buy", "quantity": c["qty"], "accepted_differences": ACCEPT, "split": {"allow_split": False}}
        s, q = http("POST", SERVER + "/v1/routes/quote", body, timeout=60)
        if s != 200 or not q.get("executable"):
            continue
        s, rep = http("POST", SERVER + "/v1/routes/execute-testnet", {"route_id": q["route_id"], "route_hash": q["route_hash"]}, timeout=60)
        leg = q["plan"]["legs"][0]
        results.append({"contract": c, "quote": {"avg": leg["expected_avg_per_underlying"], "total": leg["expected_total"], "limit": leg["limit_price_native"]},
                        "http": s, "report": rep})
        print(f"  #{len(results)} {leg['instrument']['native_id']} qty {c['qty']}: HTTP {s} "
              f"{[o['state'] for o in rep.get('orders', [])] if s == 200 else rep.get('error')}")
        time.sleep(2)
    print(f"\n{len(results)} orders sent ({tried} contracts examined). Waiting 30 s for reconciliation...")
    time.sleep(30 if results else 0)
    s, orders = http("GET", SERVER + "/v1/orders?limit=200")
    s2, stats = http("GET", SERVER + "/v1/exec-stats")
    ids = {o["id"] for r in results for o in r["report"].get("orders", [])} if results else set()
    mine = [o for o in (orders if s == 200 else []) if o["id"] in ids]
    write_report(started, results, mine, stats if s2 == 200 else {}, tried)
    return 0


def write_report(started, results, orders, stats, tried):
    stamp = started.strftime("%Y-%m-%dT%H%M%SZ")
    out = os.path.join(ROOT, "runs", f"testnet-aevo-{stamp}.json")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w") as f:
        json.dump({"started": started.isoformat(), "results": results, "orders": orders, "stats": stats}, f, indent=1, default=str)
    lines = [f"\n## Testnet run {started.isoformat()} (Aevo testnet, IOC BUY, split off)\n",
             f"{len(results)} orders sent, {tried} contracts examined (only those with an executable route).\n",
             "| Instrument | Qty | Limit | Quoted avg | Fill avg | Slippage bps | State | Ack ms | Reconciliations |",
             "|---|---|---|---|---|---|---|---|---|"]
    for o in orders:
        fill = o.get("avg_fill_price_native")
        slip = ""
        if fill and Decimal(o["filled_native"]) > 0:
            q = Decimal(o["expected_avg_per_underlying"])
            slip = str(((Decimal(fill) - q) / q * 10000).quantize(Decimal("0.01")))
        lines.append(f"| {o['native_id']} | {o['qty_native']} | {o['limit_price_native']} | {o['expected_avg_per_underlying']} | {fill or '-'} | {slip or '-'} | {o['state']} | {o.get('submit_to_ack_ms') or '-'} | {o.get('reconcile_attempts')} |")
    for v in stats.get("venues", []):
        lines.append(f"\nVenue {v['platform']}: {v['orders']} orders, filled {v['filled']}, partial {v['partially_filled']}, rejected {v['rejected']}, "
                     f"fill ratio {v['fill_ratio']}, slippage mean {v['slippage_bps_mean']} bps (max {v['slippage_bps_max']}), "
                     f"ack p50 {v['submit_to_ack_ms_p50']} ms. {v['sample_note']}")
    ids_note = [r for r in results for n in r["report"].get("notes", []) if "differs from the precomputed" in n]
    lines.append(f"\norder_id == EIP-712 digest hypothesis: {'REFUTED on some legs' if ids_note else ('consistent on all legs' if orders else 'not tested')}.\n")
    print("\n".join(lines))
    print(f"Raw data: {out}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("check", help="read-only readiness check")
    r = sub.add_parser("register", help="path B: register a new signing key (you run it)")
    r.add_argument("--force", action="store_true")
    x = sub.add_parser("run", help="send a measured series of TESTNET orders")
    x.add_argument("--orders", type=int, default=10)
    x.add_argument("--qty-btc", default="0.01")
    x.add_argument("--qty-eth", default="0.1")
    x.add_argument("--i-confirm-testnet-orders", action="store_true")
    a = ap.parse_args()
    sys.exit({"check": cmd_check, "register": cmd_register, "run": cmd_run}[a.cmd](a))


if __name__ == "__main__":
    main()
