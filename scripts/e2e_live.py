"""End-to-end check against a RUNNING stack with REAL public data (read-only).

  python scripts/e2e_live.py [--api http://127.0.0.1:8080] [--web http://127.0.0.1:3000] [--mcp target/release/oa-mcp.exe]

Journey: health -> platforms -> search -> compare (no acceptance) -> explain -> compare with
explicit acceptance -> route preview -> simulate -> fetch record -> replay -> metrics,
then the same engine through the MCP server (stdio) and the web proxy.
Exits non-zero on any failed check. Prints what it observed; never places an order.
"""

import argparse
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from decimal import Decimal

FAILS = []


def check(cond, msg):
    print(("  ok   " if cond else "  FAIL ") + msg)
    if not cond:
        FAILS.append(msg)


def http(method, url, body=None, timeout=90):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method, headers={"content-type": "application/json"})
    t = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read() or b"null"), time.perf_counter() - t
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null"), time.perf_counter() - t


def pick_contract(api, underlying):
    """Nearest expiry >= 2 days listed on both platforms, strike listed on both and closest to the
    middle of the common strikes (a proxy for at-the-money)."""
    s, r, _ = http("GET", f"{api}/v1/instruments?underlying={underlying}&option_kind=call&limit=1000")
    check(s == 200, f"search {underlying}: HTTP {s}")
    for c in r["catalogs"]:
        check(c["ok"], f"catalog {c['platform']}: {c['instruments']} instruments ({c['error']})")
    now = datetime.now(timezone.utc).timestamp()
    common = [e for e in r["expiries"] if len(e["platforms"]) >= 2
              and datetime.fromisoformat(e["expiry"].replace("Z", "+00:00")).timestamp() > now + 2 * 86400]
    for e in common:
        groups = [g for g in r["groups"] if g["expiry"] == e["expiry"] and len(g["listings"]) >= 2]
        if len(groups) >= 3:
            g = groups[len(groups) // 2]
            return g["expiry"], g["strike"], g["listings"]
    return None, None, None


def mcp_session(exe, api):
    p = subprocess.Popen([exe], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         text=True, env={**__import__("os").environ, "OA_API_URL": api})
    def rpc(i, method, params=None):
        p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params or {}}) + "\n")
        p.stdin.flush()
        return json.loads(p.stdout.readline())
    try:
        init = rpc(0, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "0"}})
        p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        p.stdin.flush()
        tools = rpc(1, "tools/list")
        return p, rpc, init, tools
    except Exception:
        p.kill()
        raise


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--api", default="http://127.0.0.1:8080")
    ap.add_argument("--web", default="http://127.0.0.1:3000")
    ap.add_argument("--mcp", default="target/release/oa-mcp.exe")
    ap.add_argument("--underlying", default="BTC")
    ap.add_argument("--repeat", type=int, default=0, help="extra live compares for latency stats")
    a = ap.parse_args()
    api = a.api

    print("[1] health")
    s, live, _ = http("GET", f"{api}/health/live")
    check(s == 200, "live")
    s, ready, _ = http("GET", f"{api}/health/ready")
    check(s == 200 and ready["ready"], f"ready (database={ready.get('database')})")

    print("[2] platforms")
    s, p, _ = http("GET", f"{api}/v1/platforms")
    check(len(p["platforms"]) == 12, "12 platforms in inventory")
    check(p["mode"] in ("READ_ONLY", "SIMULATION"), f"mode {p['mode']}")
    verified = sorted(x["id"] for x in p["platforms"] if x["integration_level"] == "VERIFIED_READ_ONLY")
    check(verified == ["aevo", "derive"], f"verified read-only = {verified}")

    print("[3] search")
    expiry, strike, listings = pick_contract(api, a.underlying)
    check(expiry is not None, f"common contract found: {a.underlying} call {strike} {expiry}")
    if expiry is None:
        return finish()

    base = {"underlying": a.underlying, "option_kind": "call", "strike": strike, "expiry": expiry,
            "side": "buy", "quantity": "0.1", "accepted_differences": [], "baseline_platform": "derive"}

    print("[4] compare without accepting differences")
    s, c, dt = http("POST", f"{api}/v1/quotes/compare", base)
    check(s == 200, f"compare HTTP {s} in {dt*1000:.0f} ms")
    quotes = c["ranked"] + c["incomplete"] + c["ineligible"]
    for q in quotes:
        e = q["estimate"]
        print(f"       {e['instrument']['platform']:7} {e['instrument']['native_id']:24} prov={e['provenance']} age={e['age_ms']}ms "
              f"covered={e['covered_underlying']}/{e['requested_underlying']} avg={e['avg_price_per_underlying']} "
              f"fee={e['fee']} total={e['total_estimated']} inel={e['ineligible']}")
        check(e["provenance"] == "LIVE_DATA", f"{e['instrument']['platform']} quote is LIVE_DATA")
        check(e["age_ms"] <= 10000, f"{e['instrument']['platform']} quote fresh ({e['age_ms']} ms)")
    for u in c["unavailable"]:
        print(f"       unavailable: {u}")
    pair = c["comparability"][0]["report"] if c["comparability"] else None
    check(pair is not None and pair["verdict"] != "STRICTLY_COMPARABLE",
          f"cross-platform verdict = {pair and pair['verdict']} {pair and pair['reasons']}")
    check(all(q["estimate"]["comparability"] is None or "DIFFERENCES_NOT_ACCEPTED" in q["estimate"]["ineligible"]
              for q in quotes if not q["is_reference"]), "non-reference platform not ranked without acceptance")

    print("[5] explain")
    keys = [l["key"] for l in listings]
    s, ex, _ = http("POST", f"{api}/v1/comparability/explain", {"a": keys[0], "b": keys[1]})
    check(s == 200, f"explain: {ex.get('report', {}).get('verdict')} accept={ex.get('codes_to_accept_for_economic_comparison')}")
    codes = ex.get("codes_to_accept_for_economic_comparison", [])

    print("[6] compare WITH explicit acceptance")
    acc = {**base, "accepted_differences": codes}
    s, c2, dt = http("POST", f"{api}/v1/quotes/compare", acc)
    check(s == 200, f"compare(accepted) HTTP {s} in {dt*1000:.0f} ms")
    print(f"       ranked={[q['estimate']['instrument']['platform'] for q in c2['ranked']]} "
          f"incomplete={[q['estimate']['instrument']['platform'] for q in c2['incomplete']]} "
          f"ineligible={[(q['estimate']['instrument']['platform'], q['estimate']['ineligible']) for q in c2['ineligible']]}")
    totals = [Decimal(q["estimate"]["total_estimated"]) for q in c2["ranked"]]
    check(totals == sorted(totals), "ranked by ascending estimated total")
    check(all(q["estimate"]["total_estimated"] is not None for q in c2["ranked"]), "ranked rows have complete totals")
    check(all(q["estimate"]["total_estimated"] is None for q in c2["incomplete"] if q["estimate"]["unknown_costs"]),
          "unknown costs never produce a total")

    print("[7] route preview + simulation")
    s, pv, _ = http("POST", f"{api}/v1/routes/preview", {**acc, "price_tolerance_bps": 50})
    check(s == 200, f"preview HTTP {s}: route={'yes -> ' + pv['route']['destination']['platform'] if pv.get('route') else 'none'}")
    if not pv.get("route"):
        for r in pv.get("no_route_reasons", []):
            print("       reason:", r)
    s, sim, _ = http("POST", f"{api}/v1/orders/simulate", {**acc, "price_tolerance_bps": 50})
    check(s == 200, f"simulate HTTP {s}: status={sim.get('status')} result={sim.get('result')}")
    check(sim.get("accepted_differences") == codes, "acceptance scope kept in the simulation record")
    s, got, _ = http("GET", f"{api}/v1/simulations/{sim['id']}")
    check(s == 200 and got["id"] == sim["id"], "simulation persisted and readable (PostgreSQL)")
    s, rp, _ = http("POST", f"{api}/v1/simulations/{sim['id']}/replay")
    check(s == 200 and rp["identical"], f"deterministic replay identical={rp.get('identical')}")

    print("[8] execution disabled")
    s, e, _ = http("POST", f"{api}/v1/orders", {})
    check(s == 403 and e["error"]["code"] == "EXECUTION_DISABLED", "POST /v1/orders -> 403 EXECUTION_DISABLED")

    if a.repeat:
        print(f"[9] {a.repeat} more live compares (cache allowed within max_age)")
        lat = []
        for _ in range(a.repeat):
            _, _, dt = http("POST", f"{api}/v1/quotes/compare", acc)
            lat.append(dt * 1000)
        lat.sort()
        print(f"       client-side latency ms: n={len(lat)} p50={lat[len(lat)//2]:.1f} max={lat[-1]:.1f}")

    print("[10] MCP (stdio) through the same API")
    try:
        p, rpc, init, tools = mcp_session(a.mcp, api)
        names = [t["name"] for t in tools["result"]["tools"]]
        check(init["result"]["protocolVersion"] == "2025-06-18", "initialize")
        check(names == ["list_platforms", "search_instruments", "compare_quotes", "explain_comparability", "preview_route", "simulate_order"], f"tools {names}")
        r = rpc(2, "tools/call", {"name": "compare_quotes", "arguments": acc})
        txt = r["result"]["content"][0]["text"]
        check(not r["result"]["isError"] and "untrusted" in txt, "compare_quotes via MCP")
        r = rpc(3, "tools/call", {"name": "simulate_order", "arguments": {**acc, "price_tolerance_bps": 50}})
        check(not r["result"]["isError"], "simulate_order via MCP")
        r = rpc(4, "tools/call", {"name": "execute_approved_order", "arguments": {}})
        check(r["result"]["isError"], "no execution tool via MCP")
        p.stdin.close()
        p.wait(timeout=5)
    except FileNotFoundError:
        check(False, f"MCP binary not found at {a.mcp}")

    print("[11] web UI + proxy")
    try:
        with urllib.request.urlopen(a.web, timeout=20) as r:
            html = r.read().decode()
        check(r.status == 200 and "Agrégateur" in html, "web page served")
        s, pp, _ = http("GET", f"{a.web}/api/v1/platforms")
        check(s == 200 and len(pp["platforms"]) == 12, "web proxy -> API")
    except Exception as e:
        check(False, f"web not reachable: {e}")

    print("[12] metrics")
    s, m, _ = http("GET", f"{api}/v1/metrics")
    for k, v in sorted(m["timings"].items()):
        print(f"       {k:42} n={v['count']:5} p50={v['p50']} p95={v['p95']} p99={v['p99']} max={v['max']} sufficient={v['sufficient']}")
    return finish()


def finish():
    print()
    print("RESULT:", "PASS" if not FAILS else f"FAIL ({len(FAILS)})")
    for f in FAILS:
        print("  -", f)
    sys.exit(1 if FAILS else 0)


if __name__ == "__main__":
    main()
