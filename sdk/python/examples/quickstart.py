"""Quickstart: compare -> route quote -> (optional) execute on TESTNET.

    cd sdk/python
    python examples/quickstart.py                       # read-only: compare + route quote
    OMNI_STRIKE_EXECUTE=1 python examples/quickstart.py  # also executes IF the server is TESTNET

Env: OMNI_STRIKE_BASE_URL (default http://127.0.0.1:8080), OMNI_STRIKE_API_KEY.
"""

from __future__ import annotations

import datetime as dt
import os
import sys
from decimal import Decimal
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))  # run from a checkout

from omni_strike import OmniStrike, OmniStrikeError  # noqa: E402

client = OmniStrike(
    base_url=os.environ.get("OMNI_STRIKE_BASE_URL", "http://127.0.0.1:8080"),
    api_key=os.environ.get("OMNI_STRIKE_API_KEY"),
)


def main() -> int:
    # 1. Pick a contract: the most cross-listed BTC call expiring in more than a day.
    groups = client.instruments("BTC", option_kind="call", limit=1000)["groups"]
    soon = dt.datetime.now(dt.timezone.utc) + dt.timedelta(days=1)
    groups = [g for g in groups if dt.datetime.fromisoformat(g["expiry"].replace("Z", "+00:00")) > soon]
    if not groups:
        print("no contract listed")
        return 1
    g = max(groups, key=lambda x: len(x["listings"]))
    intent = {
        "underlying": "BTC",
        "option_kind": "call",
        "strike": g["strike"],  # already a Decimal
        "expiry": g["expiry"],
        "side": "buy",
        "quantity": Decimal("0.1"),  # never a float
    }

    # 2. Compare every covered venue for that exact contract and size.
    cmp = client.compare_quotes({**intent, "latency_budget_ms": 1000})
    print(f"BTC {g['strike']} C {g['expiry']} x0.1 - {cmp['wording']}")
    for i, q in enumerate(cmp["ranked"], 1):
        e = q["estimate"]
        print(f"  #{i} {e['instrument']['platform']:<9} total {e['total_estimated']} {e['premium_currency']}")
    for q in cmp["ineligible"]:
        e = q["estimate"]
        print(f"  -- {e['instrument']['platform']:<9} ineligible: {', '.join(e['ineligible'])}")

    # 3. Executable route (opt-in split across up to 3 venues).
    route = client.quote_route({**intent, "split": {"allow_split": True, "max_legs": 3}})
    plan = route["plan"]
    if plan:
        print(f"route {plan['kind']}: expected total {plan['expected_total']}, legs:")
        for leg in plan["legs"]:
            print(f"  {leg['instrument']['platform']} {leg['qty_underlying']} @ limit {leg['limit_price_native']} (native)")
    else:
        print(f"no plan: {route['no_plan_reason']}")
    print(f"executable={route['executable']} {'; '.join(route['not_executable_reasons'])} (expires {route['expires_at']})")

    # 4. Execute on TESTNET only, and only when explicitly asked.
    if os.environ.get("OMNI_STRIKE_EXECUTE") == "1" and route["executable"] and route["mode"] == "TESTNET":
        report = client.execute_testnet(route["route_id"], route["route_hash"])
        print(f"filled {report['filled_underlying']}/{report['requested_underlying']}")
        for o in report["orders"]:
            print(f"  {o['platform']} {o['state']} filled {o['filled_native']}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except OmniStrikeError as e:
        print(f"API error {e.status} {e.code}: {e.message}", file=sys.stderr)
        sys.exit(1)
