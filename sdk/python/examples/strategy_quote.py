"""Quickstart: quote a BTC bull call spread (buy the lower strike, sell the higher, same expiry).

Quote only, nothing is sent to any venue.

    cd sdk/python
    python examples/strategy_quote.py

Env: OMNI_STRIKE_BASE_URL (default http://127.0.0.1:8080), OMNI_STRIKE_API_KEY.
"""

from __future__ import annotations

import datetime as dt
import os
import sys
from collections import defaultdict
from decimal import Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))  # run from a checkout

from omni_strike import OmniStrike, OmniStrikeError  # noqa: E402

client = OmniStrike(
    base_url=os.environ.get("OMNI_STRIKE_BASE_URL", "http://127.0.0.1:8080"),
    api_key=os.environ.get("OMNI_STRIKE_API_KEY"),
)


def main() -> int:
    # 1. Expiry (> 1 day out) with the most strikes listed on >= 2 venues; two adjacent mid strikes.
    groups = client.instruments("BTC", option_kind="call", limit=1000)["groups"]
    soon = dt.datetime.now(dt.timezone.utc) + dt.timedelta(days=1)
    by_expiry = defaultdict(list)
    for g in groups:
        if len(g["listings"]) >= 2 and dt.datetime.fromisoformat(g["expiry"].replace("Z", "+00:00")) > soon:
            by_expiry[g["expiry"]].append(g)
    chain = max(by_expiry.values(), key=len, default=[])
    if len(chain) < 2:
        print("no expiry with two cross-listed strikes")
        return 1
    chain.sort(key=lambda g: g["strike"])  # Decimal strikes
    mid = (len(chain) - 1) // 2
    low, high = chain[mid], chain[mid + 1]

    # 2. Quote the spread: each leg priced on every venue; one venue preferred within tolerance.
    q = client.quote_strategy(
        {
            "underlying": "BTC",
            "quantity": Decimal("0.1"),  # each leg = quantity x ratio; never a float
            "legs": [
                {"option_kind": "call", "strike": low["strike"], "expiry": low["expiry"], "side": "buy"},
                {"option_kind": "call", "strike": high["strike"], "expiry": high["expiry"], "side": "sell"},
            ],
            "single_venue_tolerance_bps": 10,
            "latency_budget_ms": 3000,
        }
    )
    print(f"BTC {low['strike']}/{high['strike']} call spread {low['expiry']} x0.1 (index {q['index_price']})")
    for leg in q["legs"]:
        best = leg["quotes"][0] if leg["quotes"] else None
        desc = f"best {best['platform']} {best['amount']} ({len(leg['quotes'])} venues)" if best else "no covered quote"
        print(f"  leg {leg['leg']} {leg['request']['side']} {leg['request']['strike']}: {desc}")
    p = q["plan"]
    if p:
        kind = "credit" if p["net_cost"] < 0 else "debit"
        single = " ".join(map(str, p["best_single_venue"])) if p["best_single_venue"] else "none"
        print(f"plan {p['kind']}: net {p['net_cost']} ({kind}), best split {p['best_split_net_cost']}, "
              f"best single venue {single}, saving vs baseline {p['saving_vs_baseline'] if p['saving_vs_baseline'] is not None else 'n/a'}")
        for leg in p["legs"]:
            qt = leg["quote"]
            print(f"  leg {leg['leg']} {leg['side']} on {qt['platform']} ({qt['native_id']}) {qt['amount']}")
        print(f"  {p['reason']}")
    else:
        print(f"no plan: {q['no_plan_reason']}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except OmniStrikeError as e:
        print(f"API error {e.status} {e.code}: {e.message}", file=sys.stderr)
        sys.exit(1)
