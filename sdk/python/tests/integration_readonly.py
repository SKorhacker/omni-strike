"""READ-ONLY integration check against a running Omni Strike server.

Never calls execute / cancel / admin-write endpoints. Not collected by ``python -m unittest`` (file name).

    python tests/integration_readonly.py
    OMNI_STRIKE_BASE_URL=http://127.0.0.1:8080 OMNI_STRIKE_API_KEY=... python tests/integration_readonly.py
"""

from __future__ import annotations

import datetime as dt
import os
import sys
import time
import traceback
from decimal import Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from omni_strike import AdminClient, OmniStrike, OmniStrikeError  # noqa: E402

client = OmniStrike(
    base_url=os.environ.get("OMNI_STRIKE_BASE_URL", "http://127.0.0.1:8080"),
    api_key=os.environ.get("OMNI_STRIKE_API_KEY"),
)
failures = 0
target = None


def step(name, fn):
    global failures
    t = time.perf_counter()
    try:
        info = fn()
        print(f"PASS {name} ({(time.perf_counter() - t) * 1000:.0f} ms){' - ' + info if info else ''}")
    except OmniStrikeError as e:
        failures += 1
        print(f"FAIL {name}: {e.status} {e.code} {e.message}")
    except Exception:  # noqa: BLE001
        failures += 1
        print(f"FAIL {name}:\n{traceback.format_exc()}")


def ready():
    r = client.health_ready()
    return f"ready={r['ready']} mode={r['mode']} venues={len(r['venues'])}"


def platforms():
    r = client.platforms()
    assert r["platforms"], "no platforms"
    integrated = [p["id"] for p in r["platforms"] if p["implemented"]]
    return f"mode={r['mode']} platforms={len(r['platforms'])} integrated={integrated}"


def instruments():
    global target
    r = client.instruments("BTC", option_kind="call", limit=1000)
    for g in r["groups"][:20]:
        assert isinstance(g["strike"], Decimal), g["strike"]
    soon = dt.datetime.now(dt.timezone.utc) + dt.timedelta(days=1)
    cands = [g for g in r["groups"] if dt.datetime.fromisoformat(g["expiry"].replace("Z", "+00:00")) > soon]
    cands.sort(key=lambda g: (-len(g["listings"]), g["expiry"]))
    target = cands[0] if cands else None
    desc = f"{target['strike']} {target['expiry']} x{len(target['listings'])}" if target else "none"
    return f"total_groups={r['total_groups']} returned={len(r['groups'])} target={desc}"


def compare():
    assert target, "no instrument to compare"
    r = client.compare_quotes(
        {
            "underlying": "BTC",
            "option_kind": "call",
            "strike": target["strike"],
            "expiry": target["expiry"],
            "side": "buy",
            "quantity": Decimal("0.1"),
            "latency_budget_ms": 3000,
        }
    )
    for q in r["ranked"] + r["incomplete"]:
        assert isinstance(q["estimate"]["premium"], Decimal), q["estimate"]["premium"]
    best = r["ranked"][0]["estimate"] if r["ranked"] else None
    s = (f"ranked={len(r['ranked'])} incomplete={len(r['incomplete'])} "
         f"ineligible={len(r['ineligible'])} unavailable={len(r['unavailable'])}")
    if best:
        s += f" best={best['instrument']['platform']} total={best['total_estimated']} {best['premium_currency']}"
    return s


def compare_invalid():
    try:
        client.compare_quotes({"underlying": "DOGE", "option_kind": "call", "strike": "1",
                               "expiry": "2026-12-25T08:00:00Z", "side": "buy", "quantity": "1"})
    except OmniStrikeError as e:
        assert e.status == 422, e
        return f"{e.status} {e.code}: {e.message}"
    raise AssertionError("expected a 422")


def live():
    try:
        r = client.live_savings()
    except OmniStrikeError as e:
        if e.code == "LIVE_TRACKER_OFF":
            return "tracker off on this server (404, typed)"
        if e.code == "HTTP_404":
            return "SKIPPED: route not deployed on this server build (plain 404)"
        raise
    return f"basket={r['basket_size']} observations={r['summary']['observations']} win_rate={r['summary']['win_rate']}"


def strategy():
    groups = client.instruments("BTC", option_kind="call", limit=1000)["groups"]
    soon = dt.datetime.now(dt.timezone.utc) + dt.timedelta(days=1)
    by_expiry: dict = {}
    for g in groups:
        if len(g["listings"]) >= 2 and dt.datetime.fromisoformat(g["expiry"].replace("Z", "+00:00")) > soon:
            by_expiry.setdefault(g["expiry"], []).append(g)
    chain = max(by_expiry.values(), key=len, default=[])
    assert len(chain) >= 2, "no expiry with two cross-listed strikes"
    chain.sort(key=lambda g: g["strike"])
    mid = (len(chain) - 1) // 2
    lo, hi = chain[mid], chain[mid + 1]
    q = client.quote_strategy({
        "underlying": "BTC", "quantity": Decimal("0.1"), "latency_budget_ms": 3000,
        "legs": [
            {"option_kind": "call", "strike": lo["strike"], "expiry": lo["expiry"], "side": "buy"},
            {"option_kind": "call", "strike": hi["strike"], "expiry": hi["expiry"], "side": "sell"},
        ],
    })
    assert len(q["legs"]) == 2
    assert isinstance(q["single_venue_tolerance"], Decimal), q["single_venue_tolerance"]
    for leg in q["legs"]:
        assert isinstance(leg["quantity"], Decimal)
        for lq in leg["quotes"]:
            assert isinstance(lq["amount"], Decimal), lq
    p = q["plan"]
    if p:
        assert p["kind"] in ("SINGLE_VENUE", "SPLIT")
        assert isinstance(p["net_cost"], Decimal) and isinstance(p["best_split_net_cost"], Decimal)
        if p["best_single_venue"]:
            assert isinstance(p["best_single_venue"][1], Decimal)
    quotes = "/".join(str(len(leg["quotes"])) for leg in q["legs"])
    head = f"{lo['strike']}/{hi['strike']} {lo['expiry']} quotes={quotes} "
    if p:
        single = ":".join(map(str, p["best_single_venue"])) if p["best_single_venue"] else "none"
        return head + f"plan={p['kind']} net={p['net_cost']} split={p['best_split_net_cost']} single={single}"
    return head + f"no plan: {q['no_plan_reason']}"


def integrator_me():
    try:
        me = client.me()
    except OmniStrikeError as e:
        if e.status == 401 and e.code == "UNAUTHORIZED":
            return f"no integrator key -> typed 401 UNAUTHORIZED ({e.message})"
        raise
    assert isinstance(me["settings"]["max_notional_per_order_usd"], Decimal)
    return f"integrator={me['name']} active={me['active']} rps={me['settings']['rate_limit_rps']}"


def admin_list():
    token = os.environ.get("OMNI_STRIKE_ADMIN_TOKEN")
    if not token:
        try:
            OmniStrike(base_url=client.base_url).request("GET", "/v1/admin/integrators")
        except OmniStrikeError as e:
            assert e.status == 401, e
            return f"SKIPPED (no OMNI_STRIKE_ADMIN_TOKEN); unauthenticated -> typed 401 {e.code}"
        raise AssertionError("admin list answered without a token")
    return f"integrators={len(AdminClient(token, base_url=client.base_url).list_integrators())}"


step("health/ready", ready)
step("GET /v1/platforms", platforms)
step("GET /v1/instruments", instruments)
step("POST /v1/quotes/compare", compare)
step("POST /v1/quotes/compare invalid -> typed error", compare_invalid)
step("GET /v1/live/savings", live)
step("POST /v1/strategies/quote (bull call spread)", strategy)
step("GET /v1/integrator/me", integrator_me)
step("GET /v1/admin/integrators (read-only)", admin_list)
print(f"\n{failures} step(s) failed" if failures else "\nall read-only integration steps passed")
sys.exit(1 if failures else 0)
