"""Unit tests with a fake transport (no network). Run: python -m unittest  (from sdk/python)."""

from __future__ import annotations

import datetime as dt
import json
import unittest
from decimal import Decimal
from typing import Any, List, Optional
from urllib.parse import parse_qs, urlsplit

from omni_strike import (
    AdminClient,
    HttpResponse,
    OmniStrike,
    OmniStrikeAdmin,
    OmniStrikeError,
    parse_retry_after,
)


class FakeTransport:
    def __init__(self, responses: List[Any]) -> None:
        self.responses = list(responses)
        self.calls: List[dict] = []

    def request(self, method, url, headers, body, timeout):  # noqa: D401
        self.calls.append(
            {"method": method, "url": url, "headers": dict(headers), "body": body, "timeout": timeout}
        )
        nxt = self.responses.pop(0)
        if isinstance(nxt, BaseException):
            raise nxt
        status, payload, hdrs = (nxt + ({},))[:3] if isinstance(nxt, tuple) else (200, nxt, {})
        raw = payload if isinstance(payload, (bytes, str)) else json.dumps(payload)
        if isinstance(raw, str):
            raw = raw.encode()
        return HttpResponse(status, {k.lower(): v for k, v in hdrs.items()}, raw)

    def json_body(self, i: int = 0) -> Any:
        return json.loads(self.calls[i]["body"])


def make(responses, **kw):
    t = FakeTransport(responses)
    sleeps: List[float] = []
    c = OmniStrike(
        base_url="http://api.test/", api_key="key_123", transport=t, sleep=sleeps.append, **kw
    )
    return c, t, sleeps


COMPARE = {
    "underlying": "BTC",
    "option_kind": "call",
    "strike": Decimal("120000"),
    "expiry": "2026-12-25T08:00:00Z",
    "side": "buy",
    "quantity": Decimal("0.5"),
}


class RequestTests(unittest.TestCase):
    def test_auth_header_and_query(self):
        c, t, _ = make([{"underlying": "BTC", "groups": []}])
        c.instruments(
            "BTC", option_kind="call", limit=5, expiry=dt.datetime(2026, 12, 25, 8, tzinfo=dt.timezone.utc)
        )
        call = t.calls[0]
        self.assertEqual(call["method"], "GET")
        self.assertEqual(call["headers"]["Authorization"], "Bearer key_123")
        u = urlsplit(call["url"])
        self.assertEqual(u.path, "/v1/instruments")
        q = parse_qs(u.query)
        self.assertEqual(q["underlying"], ["BTC"])
        self.assertEqual(q["limit"], ["5"])
        self.assertEqual(q["expiry"], ["2026-12-25T08:00:00Z"])
        self.assertNotIn("offset", q)

    def test_no_auth_without_key(self):
        t = FakeTransport([{"mode": "READ_ONLY", "platforms": []}])
        OmniStrike(base_url="http://api.test", transport=t).platforms()
        self.assertNotIn("Authorization", t.calls[0]["headers"])

    def test_compare_body_decimals_are_strings_and_none_dropped(self):
        c, t, _ = make([{"ranked": []}])
        req = dict(COMPARE, baseline_platform=None, accepted_differences=["SETTLEMENT_INDEX_DIFFERS"],
                   simulated_balances={"derive": Decimal("1000.50")})
        c.compare_quotes(req)
        body = t.json_body()
        self.assertEqual(t.calls[0]["url"], "http://api.test/v1/quotes/compare")
        self.assertEqual(t.calls[0]["headers"]["Content-Type"], "application/json")
        self.assertEqual(body["strike"], "120000")
        self.assertEqual(body["quantity"], "0.5")
        self.assertEqual(body["simulated_balances"], {"derive": "1000.50"})
        self.assertNotIn("baseline_platform", body)

    def test_decimal_exponent_is_written_plain(self):
        c, t, _ = make([{}])
        c.compare_quotes(dict(COMPARE, strike=Decimal("1.2E+5")))
        self.assertEqual(t.json_body()["strike"], "120000")

    def test_float_refused(self):
        c, t, _ = make([])
        with self.assertRaises(TypeError) as cm:
            c.compare_quotes(dict(COMPARE, quantity=0.5))
        self.assertIn("quantity", str(cm.exception))
        self.assertEqual(t.calls, [])

    def test_route_quote_split_nested(self):
        c, t, _ = make([{"route_id": "r", "route_hash": "h"}])
        c.quote_route(dict(COMPARE, split={"allow_split": True, "max_legs": 3, "min_saving_bps": Decimal("5")}))
        body = t.json_body()
        self.assertEqual(body["underlying"], "BTC")
        self.assertEqual(body["split"], {"allow_split": True, "max_legs": 3, "min_saving_bps": "5"})

    def test_ids_are_url_encoded_and_cancel_is_post(self):
        c, t, _ = make([{"id": "x"}, {"id": "y"}])
        c.get_order("a/b")
        c.cancel_order("00000000-0000-0000-0000-000000000000")
        self.assertEqual(t.calls[0]["url"], "http://api.test/v1/orders/a%2Fb")
        self.assertEqual(t.calls[1]["method"], "POST")
        self.assertTrue(t.calls[1]["url"].endswith("/v1/orders/00000000-0000-0000-0000-000000000000/cancel"))


class DecodeTests(unittest.TestCase):
    def test_decimal_fields_parsed_ids_untouched_stats_float(self):
        payload = {
            "ranked": [
                {
                    "estimate": {
                        "instrument": {"platform": "aevo", "network": "aevo-mainnet", "native_id": "12345"},
                        "premium": "123.4500",
                        "avg_price_per_underlying": None,
                        "fee": {"status": "known", "amount": "0.31"},
                        "age_ms": 120,
                    },
                    "terms": {"strike": "120000", "spec_version": "1", "contract_multiplier": "0.01"},
                }
            ],
            "request": {"simulated_balances": {"derive": "50.5"}},
            "plan": {"best_single": [{"platform": "derive", "network": "n", "native_id": "1"}, "99.9"]},
            "summary": {"win_rate": 0.62, "median_saving_pct": 1.5},
        }
        c, _, _ = make([payload])
        r = c.request("GET", "/x")
        est = r["ranked"][0]["estimate"]
        self.assertEqual(est["premium"], Decimal("123.4500"))
        self.assertIsInstance(est["premium"], Decimal)
        self.assertEqual(est["instrument"]["native_id"], "12345")  # id stays a string
        self.assertIsNone(est["avg_price_per_underlying"])
        self.assertEqual(est["fee"]["amount"], Decimal("0.31"))
        self.assertEqual(r["ranked"][0]["terms"]["contract_multiplier"], Decimal("0.01"))
        self.assertEqual(r["ranked"][0]["terms"]["spec_version"], "1")
        self.assertEqual(r["request"]["simulated_balances"]["derive"], Decimal("50.5"))
        self.assertEqual(r["plan"]["best_single"][1], Decimal("99.9"))
        self.assertEqual(r["summary"]["win_rate"], 0.62)

    def test_openapi_is_raw(self):
        c, _, _ = make([{"components": {"schemas": {"x": {"properties": {"strike": "string"}}}}}])
        self.assertEqual(c.openapi()["components"]["schemas"]["x"]["properties"]["strike"], "string")


class ErrorTests(unittest.TestCase):
    def test_envelope(self):
        c, _, _ = make([(422, {"error": {"code": "INVALID_REQUEST", "message": "strike must be > 0"}})])
        with self.assertRaises(OmniStrikeError) as cm:
            c.compare_quotes(COMPARE)
        e = cm.exception
        self.assertEqual((e.status, e.code, e.message), (422, "INVALID_REQUEST", "strike must be > 0"))
        self.assertFalse(e.is_retryable)

    def test_non_envelope(self):
        c, t, _ = make([(502, "Bad Gateway")])
        with self.assertRaises(OmniStrikeError) as cm:
            c.platforms()
        self.assertEqual(cm.exception.code, "HTTP_502")
        self.assertEqual(len(t.calls), 1)

    def test_timeout_and_network(self):
        c, _, _ = make([TimeoutError("slow"), ConnectionRefusedError("refused")])
        with self.assertRaises(OmniStrikeError) as cm:
            c.platforms()
        self.assertEqual((cm.exception.status, cm.exception.code), (0, "TIMEOUT"))
        with self.assertRaises(OmniStrikeError) as cm:
            c.platforms()
        self.assertEqual(cm.exception.code, "NETWORK_ERROR")

    def test_health_ready_503_returns_body(self):
        c, t, _ = make([(503, {"ready": False, "database": "down", "venues": [], "mode": "READ_ONLY"})])
        self.assertFalse(c.health_ready()["ready"])
        self.assertEqual(len(t.calls), 1)


class RetryTests(unittest.TestCase):
    BUSY = (503, {"error": {"code": "SERVICE_BUSY", "message": "busy"}})

    def test_get_retries_with_retry_after_then_succeeds(self):
        c, t, sleeps = make(
            [(429, {"error": {"code": "RATE_LIMITED", "message": "x"}}, {"Retry-After": "2"}), self.BUSY,
             {"mode": "READ_ONLY", "platforms": []}],
            retry_base_delay=0.1,
        )
        self.assertEqual(c.platforms()["mode"], "READ_ONLY")
        self.assertEqual(len(t.calls), 3)
        self.assertEqual(sleeps[0], 2.0)
        self.assertTrue(0.1 <= sleeps[1] <= 0.2, sleeps)

    def test_gives_up_after_max_retries(self):
        c, t, sleeps = make([self.BUSY] * 3, max_retries=2)
        with self.assertRaises(OmniStrikeError) as cm:
            c.live_savings()
        self.assertEqual(cm.exception.code, "SERVICE_BUSY")
        self.assertEqual((len(t.calls), len(sleeps)), (3, 2))

    def test_retry_after_capped(self):
        c, _, sleeps = make([(429, {}, {"Retry-After": "3600"}), []], max_retry_delay=1.5)
        c.list_orders(limit=10)
        self.assertEqual(sleeps, [1.5])

    def test_posts_never_retried(self):
        c, t, sleeps = make([self.BUSY, self.BUSY])
        with self.assertRaises(OmniStrikeError):
            c.compare_quotes(COMPARE)
        with self.assertRaises(OmniStrikeError):
            c.execute_testnet("rid", "hash")
        self.assertEqual((len(t.calls), sleeps), (2, []))
        self.assertEqual(t.json_body(1), {"route_id": "rid", "route_hash": "hash"})

    def test_parse_retry_after(self):
        self.assertEqual(parse_retry_after("1.5"), 1.5)
        self.assertIsNone(parse_retry_after(None))
        self.assertIsNone(parse_retry_after("garbage"))
        now = dt.datetime(2026, 10, 7, tzinfo=dt.timezone.utc).timestamp()
        self.assertEqual(parse_retry_after("Wed, 07 Oct 2026 00:00:03 GMT", now=now), 3.0)


SETTINGS = {
    "max_notional_per_order_usd": Decimal("250000"),
    "max_notional_per_day_usd": Decimal("2000000"),
    "allowed_platforms": ["derive", "aevo"],
    "allowed_underlyings": None,
    "max_slippage_bps": 50,
    "unwind_cost_cap_bps": 30,
    "single_venue_tolerance_bps": 10,
    "simulation_first": True,
    "rate_limit_rps": 20,
}
INTEGRATOR = {
    "id": "11111111-1111-1111-1111-111111111111", "name": "acme", "key_prefix": "os_1a2b3c4d",
    "settings": dict(SETTINGS, max_notional_per_order_usd="250000", max_notional_per_day_usd="2000000"),
    "active": True, "created_at": "2026-10-07T00:00:00Z", "updated_at": "2026-10-07T00:00:00Z",
}
EXP = dt.datetime(2026, 12, 25, 8, tzinfo=dt.timezone.utc)
STRATEGY_QUOTE = {
    "computed_at": "2026-10-07T00:00:00Z", "wording": "w", "notes": [], "no_plan_reason": None,
    "request": {"underlying": "BTC", "quantity": "0.5",
                "legs": [{"option_kind": "call", "strike": "60000", "expiry": "x", "side": "buy", "ratio": "2"}]},
    "index_price": "62000.5", "single_venue_tolerance": "12.4",
    "plan": {
        "kind": "SINGLE_VENUE",
        "legs": [{"leg": 0, "side": "buy", "quote": {"platform": "derive", "native_id": "BTC-C", "amount": "1200.5"}}],
        "net_cost": "-35.25", "best_split_net_cost": "-40.1", "best_single_venue": ["derive", "-35.25"],
        "saving_vs_baseline": None, "reason": "r",
    },
    "legs": [{"leg": 0, "request": {}, "quantity": "0.5",
              "quotes": [{"platform": "aevo", "native_id": "77", "amount": "1201"}], "unavailable": []}],
}


class StrategyTests(unittest.TestCase):
    def test_quote_strategy_body_and_decoding(self):
        c, t, _ = make([STRATEGY_QUOTE])
        q = c.quote_strategy({
            "underlying": "BTC", "quantity": Decimal("0.5"), "single_venue_tolerance_bps": 20,
            "baseline_platform": None,
            "legs": [
                {"option_kind": "call", "strike": Decimal("60000"), "expiry": EXP, "side": "buy"},
                {"option_kind": "call", "strike": "65000", "expiry": EXP, "side": "sell", "ratio": Decimal("1")},
            ],
        })
        call = t.calls[0]
        self.assertEqual((call["method"], call["url"]), ("POST", "http://api.test/v1/strategies/quote"))
        body = t.json_body()
        self.assertEqual(body["quantity"], "0.5")
        self.assertNotIn("baseline_platform", body)
        self.assertEqual(body["legs"][0], {"option_kind": "call", "strike": "60000",
                                           "expiry": "2026-12-25T08:00:00Z", "side": "buy"})
        self.assertEqual(body["legs"][1]["ratio"], "1")
        plan = q["plan"]
        self.assertEqual(plan["net_cost"], Decimal("-35.25"))
        self.assertEqual(plan["best_split_net_cost"], Decimal("-40.1"))
        self.assertEqual(plan["best_single_venue"], ["derive", Decimal("-35.25")])
        self.assertIsNone(plan["saving_vs_baseline"])
        self.assertEqual(plan["legs"][0]["quote"]["amount"], Decimal("1200.5"))
        self.assertEqual(plan["legs"][0]["quote"]["native_id"], "BTC-C")
        self.assertEqual(q["single_venue_tolerance"], Decimal("12.4"))
        self.assertEqual(q["index_price"], Decimal("62000.5"))
        self.assertEqual(q["request"]["legs"][0]["ratio"], Decimal("2"))
        self.assertEqual(q["legs"][0]["quotes"][0]["native_id"], "77")

    def test_limit_exceeded_typed_not_retried(self):
        c, t, sleeps = make([(422, {"error": {"code": "LIMIT_EXCEEDED", "message": "above cap"}})])
        with self.assertRaises(OmniStrikeError) as cm:
            c.quote_strategy({"underlying": "BTC", "quantity": "100", "legs": []})
        self.assertEqual((cm.exception.status, cm.exception.code), (422, "LIMIT_EXCEEDED"))
        self.assertEqual((len(t.calls), sleeps), (1, []))

    def test_float_ratio_refused(self):
        c, t, _ = make([])
        with self.assertRaises(TypeError):
            c.quote_strategy({"underlying": "BTC", "quantity": "1", "legs": [
                {"option_kind": "call", "strike": "1", "expiry": EXP, "side": "buy", "ratio": 1.5}]})
        self.assertEqual(t.calls, [])


class IntegratorTests(unittest.TestCase):
    def test_me_retries_429_with_retry_after(self):
        c, t, sleeps = make([
            (429, {"error": {"code": "RATE_LIMITED", "message": "x"}}, {"Retry-After": "1"}), INTEGRATOR])
        me = c.me()
        self.assertEqual((t.calls[1]["method"], t.calls[1]["url"]), ("GET", "http://api.test/v1/integrator/me"))
        self.assertEqual(sleeps, [1.0])
        self.assertEqual(me["settings"]["max_notional_per_order_usd"], Decimal("250000"))
        self.assertEqual(me["settings"]["max_slippage_bps"], 50)

    def test_me_401_typed(self):
        c, t, _ = make([(401, {"error": {"code": "UNAUTHORIZED", "message": "integrator API key required"}})])
        with self.assertRaises(OmniStrikeError) as cm:
            c.me()
        self.assertEqual((cm.exception.status, cm.exception.code), (401, "UNAUTHORIZED"))
        self.assertEqual(len(t.calls), 1)

    def test_update_settings_put_not_retried(self):
        c, t, sleeps = make([INTEGRATOR, (429, {"error": {"code": "RATE_LIMITED", "message": "x"}},
                                          {"Retry-After": "1"})])
        c.update_settings(SETTINGS)
        self.assertEqual((t.calls[0]["method"], t.calls[0]["url"]),
                         ("PUT", "http://api.test/v1/integrator/settings"))
        body = t.json_body()
        self.assertEqual(body["max_notional_per_order_usd"], "250000")
        self.assertEqual(body["allowed_platforms"], ["derive", "aevo"])
        self.assertTrue(body["simulation_first"])
        with self.assertRaises(OmniStrikeError) as cm:
            c.update_settings(SETTINGS)
        self.assertEqual(cm.exception.code, "RATE_LIMITED")
        self.assertEqual((len(t.calls), sleeps), (2, []))


class AdminTests(unittest.TestCase):
    def test_admin_endpoints(self):
        t = FakeTransport([{"integrator": INTEGRATOR, "api_key": "os_secret"}, [INTEGRATOR], INTEGRATOR,
                           dict(INTEGRATOR, active=False)])
        a = AdminClient("adm", base_url="http://api.test", transport=t, sleep=lambda s: None)
        self.assertIs(OmniStrikeAdmin, AdminClient)
        created = a.create_integrator("acme")
        self.assertEqual(created["api_key"], "os_secret")
        self.assertEqual(len(a.list_integrators()), 1)
        upd = a.update_integrator_settings("id/1", SETTINGS)
        self.assertEqual(upd["settings"]["max_notional_per_day_usd"], Decimal("2000000"))
        self.assertFalse(a.set_integrator_active(INTEGRATOR["id"], False)["active"])
        self.assertEqual([(x["method"], x["url"]) for x in t.calls], [
            ("POST", "http://api.test/v1/admin/integrators"),
            ("GET", "http://api.test/v1/admin/integrators"),
            ("PUT", "http://api.test/v1/admin/integrators/id%2F1/settings"),
            ("PUT", f"http://api.test/v1/admin/integrators/{INTEGRATOR['id']}/active"),
        ])
        self.assertTrue(all(x["headers"]["Authorization"] == "Bearer adm" for x in t.calls))
        self.assertEqual(t.json_body(0), {"name": "acme"})
        self.assertEqual(t.json_body(3), {"active": False})

    def test_admin_token_required(self):
        with self.assertRaises(ValueError):
            AdminClient("")
        with self.assertRaises(TypeError):
            AdminClient("adm", api_key="x")



class OrdersMessageTest(unittest.TestCase):
    def test_matches_the_server_format(self) -> None:
        from omni_strike import orders_message

        self.assertEqual(
            orders_message("0xABCdef0000000000000000000000000000000001", 1791000000000),
            "Omni Strike: read my orders\nWallet: 0xabcdef0000000000000000000000000000000001\nTimestamp: 1791000000000",
        )


if __name__ == "__main__":
    unittest.main()
