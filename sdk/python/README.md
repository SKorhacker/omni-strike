# omni-strike (Python)

Official Python client for the **Omni Strike** crypto options best-execution API.
Standard library only (`urllib`), Python >= 3.9, typed (`py.typed`, `TypedDict` models).

## Install

```bash
pip install ./sdk/python        # or add sdk/python to PYTHONPATH
```

## Quickstart: compare -> route quote -> execute (TESTNET)

```python
from decimal import Decimal
from omni_strike import OmniStrike, OmniStrikeError

client = OmniStrike(base_url="http://127.0.0.1:8080", api_key="...")  # Bearer <api_key>

intent = {
    "underlying": "BTC", "option_kind": "call", "strike": Decimal("120000"),
    "expiry": "2026-12-25T08:00:00Z", "side": "buy", "quantity": Decimal("0.5"),
}

# 1. Compare every covered venue for this exact contract and size
cmp = client.compare_quotes({**intent, "latency_budget_ms": 500})
best = cmp["ranked"][0]["estimate"] if cmp["ranked"] else None
if best:
    print(best["instrument"]["platform"], best["total_estimated"])  # Decimal

# 2. Executable route (opt-in split over up to 3 venues), valid 30 s
route = client.quote_route({**intent, "split": {"allow_split": True, "max_legs": 3}})

# 3. Execute - TESTNET servers only (403 EXECUTION_DISABLED elsewhere), single use
if route["executable"]:
    try:
        report = client.execute_testnet(route["route_id"], route["route_hash"])
        print(report["filled_underlying"], [o["state"] for o in report["orders"]])
    except OmniStrikeError as e:
        if e.code != "ROUTE_INVALID":  # expired / already used: quote again
            raise
```

Runnable version: [`examples/quickstart.py`](examples/quickstart.py).

## Multi-leg strategies: bull call spread

```python
q = client.quote_strategy({
    "underlying": "BTC",
    "quantity": Decimal("0.1"),  # each leg = quantity x ratio (ratio defaults to 1)
    "legs": [
        {"option_kind": "call", "strike": Decimal("88000"), "expiry": "2026-12-25T08:00:00Z", "side": "buy"},
        {"option_kind": "call", "strike": Decimal("90000"), "expiry": "2026-12-25T08:00:00Z", "side": "sell"},
    ],
    "single_venue_tolerance_bps": 10,  # prefer ONE venue within 10 bps of notional of the best split
})
plan = q["plan"]
if plan:
    # net_cost (Decimal) > 0 = debit, < 0 = credit; kind "SINGLE_VENUE" | "SPLIT"
    print(plan["kind"], plan["net_cost"], plan["best_split_net_cost"], plan["best_single_venue"])
    for leg in plan["legs"]:
        print(leg["leg"], leg["side"], leg["quote"]["platform"], leg["quote"]["amount"])
else:
    print("no plan:", q["no_plan_reason"])
```

Quote only (nothing is sent). Runnable version: [`examples/strategy_quote.py`](examples/strategy_quote.py).

## Integrator settings and admin

```python
me = client.me()                                    # GET /v1/integrator/me (integrator key)
client.update_settings({**me["settings"], "max_slippage_bps": 25})  # PUT: always the FULL object

# Operator side: separate class + admin token (server OA_ADMIN_TOKEN); alias OmniStrikeAdmin
import os
from omni_strike import AdminClient
admin = AdminClient(os.environ["OMNI_STRIKE_ADMIN_TOKEN"], base_url="http://127.0.0.1:8080")
created = admin.create_integrator("acme")            # {"integrator", "api_key"}: key shown ONCE
admin.set_integrator_active(created["integrator"]["id"], False)
```

## API coverage

| Method | Endpoint |
|---|---|
| `platforms()` | `GET /v1/platforms` |
| `instruments(underlying, option_kind=, expiry=, family=, offset=, limit=)` | `GET /v1/instruments` |
| `compare_quotes(req)` | `POST /v1/quotes/compare` (`family` "dated_option"/"binary", `accepted_differences`, `latency_budget_ms`, `max_age_ms`, ...) |
| `quote_route(req)` | `POST /v1/routes/quote` (compare fields + `split` policy) |
| `execute_testnet(route_id, route_hash)` | `POST /v1/routes/execute-testnet` |
| `list_orders(limit=)`, `get_order(id)`, `cancel_order(id)` | `GET /v1/orders`, `GET /v1/orders/{id}`, `POST /v1/orders/{id}/cancel` |
| `balances()`, `exec_stats()`, `live_savings()` | `GET /v1/balances`, `/v1/exec-stats`, `/v1/live/savings` |
| `quote_strategy(req)` | `POST /v1/strategies/quote` (1..=6 legs, `single_venue_tolerance_bps`, `baseline_platform`, ...) |
| `me()`, `update_settings(settings)` | `GET /v1/integrator/me`, `PUT /v1/integrator/settings` |
| `AdminClient`: `create_integrator`, `list_integrators`, `update_integrator_settings`, `set_integrator_active` | `POST`/`GET /v1/admin/integrators`, `PUT /v1/admin/integrators/{id}/settings`, `PUT /v1/admin/integrators/{id}/active` |
| `health_ready()`, `openapi()` | `GET /health/ready` (returns on 503 too, check `ready`), `GET /openapi.json` |
| `explain_comparability`, `preview_route`, `simulate_order`, `get_simulation` | comparability / simulation endpoints |
| `request(method, path, query=, body=)` | escape hatch for endpoints not wrapped yet |

## Behaviour

- **Decimals**: responses are decoded so every decimal field (the schemas' decimal properties,
  listed in `omni_strike/_codec.py`) is a `decimal.Decimal`; identifiers stay `str`, statistics the
  server sends as JSON numbers (`saving_pct`, `win_rate`) stay numbers. Requests accept `Decimal`,
  `str` or `int`; a `float` anywhere in a body raises `TypeError` before sending. `None` fields are
  dropped; `datetime` values are sent as RFC 3339 UTC.
- **Errors**: every failure is `OmniStrikeError(status, code, message, body, retry_after)`. Server
  envelope `{"error":{"code","message"}}` -> its code (`INVALID_REQUEST`, `NOT_FOUND`,
  `SERVICE_BUSY`, `UNAUTHORIZED`, `EXECUTION_DISABLED`, `ROUTE_INVALID`, `RISK_REJECTED`,
  `LIVE_TRACKER_OFF`, `RATE_LIMITED` (429), `LIMIT_EXCEEDED` (422, your integrator limits), ...); other bodies -> `HTTP_<status>`; client-side -> status 0 with
  `TIMEOUT`, `NETWORK_ERROR`, `INVALID_RESPONSE`.
- **Timeouts**: `timeout` seconds per attempt (default 30).
- **Retries**: only idempotent `GET`s, only on HTTP 429/503, `max_retries` (default 2), exponential
  backoff with jitter from `retry_base_delay` (0.25 s), `Retry-After` honoured (seconds or HTTP
  date), capped by `max_retry_delay` (10 s). POSTs and PUTs are **never** retried automatically.
- **Transport**: pass `transport=` (any object with `request(method, url, headers, body, timeout)
  -> HttpResponse`) to plug in your own HTTP stack or a fake in tests.

## Tests

```bash
cd sdk/python
python -m unittest                                   # unit tests, fake transport
OMNI_STRIKE_BASE_URL=http://127.0.0.1:8080 python tests/integration_readonly.py   # READ-ONLY live calls
```

The integration script also quotes a bull call spread, checks `integrator/me` (typed 401 without
a key) and lists integrators only when `OMNI_STRIKE_ADMIN_TOKEN` is set. It never calls execute,
cancel or any admin write.
