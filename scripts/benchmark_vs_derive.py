"""Benchmark: our router vs "just use Derive" (read-only, live public data, running stack).

  python scripts/benchmark_vs_derive.py [--api URL] [--n 40] [--seed 11] [--json out.json]

Universe: BTC/ETH options listed on Derive AND at least one other venue, --min-hours (default 1 h) to 60 days, |K/S-1| <= 15 %.
For each contract and size:
- BUY: /v1/routes/quote (split allowed, settlement differences accepted) = OUR cost, vs Derive's own
  estimate for the same size (from the route's `compare` block) = DERIVE-ONLY cost.
- SELL: /v1/quotes/compare = best eligible single venue proceeds vs Derive's proceeds.
- POTENTIAL: same, also counting Paradex RPI prices (today shown but not routable: Retail-only).
Reports: % of cases where we beat Derive, saving in USDC and in bps of premium, cases where Derive
alone cannot fill the size but we can. All figures are ESTIMATES from visible books + documented fees.
"""
import argparse
import json
import random
import time
import urllib.error
import urllib.request
from collections import defaultdict
from datetime import datetime, timezone

ACCEPT = ["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN", "SETTLEMENT_WINDOW_DIFFERS"]
SIZES = {"BTC": ["0.1", "1", "5"], "ETH": ["1", "10", "50"]}


def http(method, url, body=None, timeout=120):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method, headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null")


def pct(v, p):
    v = sorted(v)
    return v[max(1, -(-len(v) * p // 100)) - 1] if v else None


def universe(api, min_hours=1):
    out, now = [], datetime.now(timezone.utc).timestamp()
    for u in ("BTC", "ETH"):
        for kind in ("call", "put"):
            _, r = http("GET", f"{api}/v1/instruments?underlying={u}&option_kind={kind}&limit=2000")
            for g in r["groups"]:
                plats = {l["key"]["platform"] for l in g["listings"]}
                exp = datetime.fromisoformat(g["expiry"].replace("Z", "+00:00")).timestamp()
                if "derive" in plats and len(plats) >= 2 and now + min_hours * 3600 < exp < now + 60 * 86400:
                    out.append({"u": u, "kind": kind, "strike": g["strike"], "expiry": g["expiry"], "venues": sorted(plats)})
    return out


def estimates(cmp):
    """All single-venue estimates of a compare result, keyed by platform."""
    out = {}
    for group in ("ranked", "incomplete", "ineligible"):
        for x in cmp.get(group, []):
            e = x["estimate"]
            out[e["instrument"]["platform"]] = (group, e)
    return out


def full(e, qty):
    return e and e.get("total_estimated") is not None and float(e["covered_underlying"]) >= float(qty) - 1e-9


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--api", default="http://127.0.0.1:8080")
    ap.add_argument("--n", type=int, default=40)
    ap.add_argument("--seed", type=int, default=11)
    ap.add_argument("--json", default=None)
    ap.add_argument("--min-hours", type=float, default=1)
    ap.add_argument("--accept-settlement-asset", action="store_true", help="also accept SETTLEMENT_ASSET_DIFFERS + PREMIUM_CURRENCY_DIFFERS (USDT/coin-settled CEX venues, converted to USDC)")
    ap.add_argument("--per-underlying", action="store_true", help="sample n contracts PER underlying")
    a = ap.parse_args()
    if a.accept_settlement_asset:
        ACCEPT.extend(["SETTLEMENT_ASSET_DIFFERS", "PREMIUM_CURRENCY_DIFFERS"])
    cs = universe(a.api, a.min_hours)
    # spot per underlying from any estimate
    spots = {}
    for u in ("BTC", "ETH"):
        c = next(c for c in cs if c["u"] == u)
        _, r = http("POST", f"{a.api}/v1/quotes/compare", {"underlying": u, "option_kind": c["kind"], "strike": c["strike"], "expiry": c["expiry"], "side": "buy", "quantity": "0.1", "accepted_differences": ACCEPT})
        spots[u] = next(float(e["index_price"]) for _, e in estimates(r).values() if e.get("index_price"))
    cs = [c for c in cs if abs(float(c["strike"]) / spots[c["u"]] - 1) <= 0.15]
    random.seed(a.seed)
    if a.per_underlying:
        sample = [x for u in ("BTC", "ETH") for x in random.sample([c for c in cs if c["u"] == u], min(a.n, sum(c["u"] == u for c in cs)))]
    else:
        sample = random.sample(cs, min(a.n, len(cs)))
    print(f"universe {len(cs)} contracts on Derive + another venue (|K/S-1|<=15%, 1-60 d); sample {len(sample)}; spots {spots}")

    rows = []
    for i, c in enumerate(sample):
        for qty in SIZES[c["u"]]:
            base = {"underlying": c["u"], "option_kind": c["kind"], "strike": c["strike"], "expiry": c["expiry"], "quantity": qty, "accepted_differences": ACCEPT}
            # BUY via the router (split allowed)
            s, q = http("POST", f"{a.api}/v1/routes/quote", {**base, "side": "buy", "split": {"allow_split": True, "min_saving_abs": "1", "min_saving_bps": "10"}})
            if s == 200:
                est = estimates(q.get("compare") or {})
                d = est.get("derive", (None, None))[1]
                p = q.get("plan")
                ours = float(p["expected_total"]) if p and p["fully_covered"] else None
                der = float(d["total_estimated"]) if full(d, qty) else None
                rpi = est.get("paradex", (None, None))[1]
                pot = ours
                if rpi and full(rpi, qty) and "RETAIL_ONLY_LIQUIDITY" in rpi["ineligible"]:
                    pot = min(x for x in (ours, float(rpi["total_estimated"])) if x is not None)
                rows.append({**c, "qty": qty, "side": "buy", "ours": ours, "derive": der, "potential": pot,
                             "venue": (p["legs"][0]["instrument"]["platform"] if p and p["kind"] == "SINGLE" else (p["kind"] if p else None)),
                             "derive_covered": d and float(d["covered_underlying"])})
            # SELL via compare (best eligible single venue)
            s, r = http("POST", f"{a.api}/v1/quotes/compare", {**base, "side": "sell"})
            if s == 200:
                est = estimates(r)
                elig = [(pl, float(e["total_estimated"])) for pl, (g, e) in est.items() if g == "ranked" and full(e, qty)]
                d = est.get("derive", (None, None))[1]
                best = max(elig, key=lambda x: x[1]) if elig else None
                der = float(d["total_estimated"]) if full(d, qty) and est["derive"][0] == "ranked" else None
                rpi = est.get("paradex", (None, None))[1]
                pot = best[1] if best else None
                if rpi and full(rpi, qty) and "RETAIL_ONLY_LIQUIDITY" in rpi["ineligible"]:
                    pot = max(x for x in (pot, float(rpi["total_estimated"])) if x is not None)
                rows.append({**c, "qty": qty, "side": "sell", "ours": best[1] if best else None, "derive": der, "potential": pot,
                             "venue": best[0] if best else None, "derive_covered": d and float(d["covered_underlying"])})
            time.sleep(0.3)
        print(f"  {i + 1}/{len(sample)} {c['u']} {c['kind']} {c['strike']} {c['expiry'][:10]} venues {c['venues']}", flush=True)

    report = {"at": datetime.now(timezone.utc).isoformat(), "spots": spots, "n_contracts": len(sample), "groups": {}}
    print("\n=== Our router vs Derive alone (estimates) ===")
    groups = defaultdict(list)
    for r in rows:
        groups[(r["u"], r["side"], r["qty"])].append(r)
    for key in sorted(groups, key=lambda k: (k[0], k[1], float(k[2]))):
        rs = groups[key]
        u, side, qty = key
        sign = 1 if side == "buy" else -1  # buy: lower cost better; sell: higher proceeds better
        both = [r for r in rs if r["ours"] is not None and r["derive"] is not None]
        sav = [sign * (r["derive"] - r["ours"]) for r in both]
        bps = [sign * (r["derive"] - r["ours"]) / r["derive"] * 1e4 for r in both if r["derive"]]
        pot = [sign * (r["derive"] - r["potential"]) for r in rs if r["potential"] is not None and r["derive"] is not None]
        better = sum(1 for s in sav if s > 0.005)
        only_us = sum(1 for r in rs if r["ours"] is not None and r["derive"] is None)
        nobody = sum(1 for r in rs if r["ours"] is None)
        venues = defaultdict(int)
        for r in rs:
            if r["venue"]:
                venues[r["venue"]] += 1
        g = {"n": len(rs), "both_full": len(both), "we_beat_derive": better, "saving_usdc_sum": round(sum(sav), 2),
             "saving_usdc_median_when_better": round(pct([s for s in sav if s > 0.005], 50) or 0, 2),
             "saving_bps_mean": round(sum(bps) / len(bps), 1) if bps else None,
             "saving_bps_median_when_better": round(pct([b for b, s in zip(bps, sav) if s > 0.005], 50) or 0, 1),
             "potential_with_paradex_rpi_usdc_sum": round(sum(pot), 2),
             "filled_by_us_not_by_derive": only_us, "nobody_fills": nobody, "chosen_venue": dict(venues)}
        report["groups"][f"{u} {side} {qty}"] = g
        print(f"{u} {side:4} {qty:>4}: n={g['n']:3} | beat Derive {better:3}/{len(both):3} | saving total {g['saving_usdc_sum']:>9} USDC, "
              f"mean {g['saving_bps_mean']} bps, median-when-better {g['saving_bps_median_when_better']} bps | "
              f"+Paradex RPI {g['potential_with_paradex_rpi_usdc_sum']:>9} | only we fill {only_us} | nobody {nobody} | venues {dict(venues)}")
    report["rows"] = rows
    if a.json:
        with open(a.json, "w", encoding="utf-8") as f:
            json.dump(report, f, indent=1, default=str)
        print(f"\nraw rows -> {a.json}")


if __name__ == "__main__":
    main()
