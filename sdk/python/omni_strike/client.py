"""Omni Strike API client (stdlib only)."""

from __future__ import annotations

import datetime as _dt
import email.utils
import random
import socket
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, List, Mapping, Optional, Protocol, Sequence, cast

from . import _codec
from .errors import OmniStrikeError
from .models import (
    BalancesResponse,
    CompareRequest,
    CompareResponse,
    CreatedIntegrator,
    ExecStatsResponse,
    ExecutionReport,
    InstrumentsResponse,
    Integrator,
    IntegratorSettings,
    LiveSavingsSnapshot,
    Order,
    PlatformsResponse,
    Readiness,
    RouteQuote,
    RouteQuoteRequest,
    StrategyQuote,
    StrategyRequest,
)

__version__ = "0.1.0"
DEFAULT_BASE_URL = "http://127.0.0.1:8080"
_RETRYABLE = (429, 503)


@dataclass
class HttpResponse:
    status: int
    headers: Mapping[str, str]
    body: bytes


class Transport(Protocol):
    """Pluggable HTTP layer (inject a fake one in tests).

    Must return an :class:`HttpResponse` for ANY HTTP status (including 4xx/5xx), raise
    ``TimeoutError`` on timeout and ``OSError`` on connection failure.
    """

    def request(
        self,
        method: str,
        url: str,
        headers: Mapping[str, str],
        body: Optional[bytes],
        timeout: float,
    ) -> HttpResponse: ...


class UrllibTransport:
    """Default transport based on ``urllib.request``."""

    def request(
        self,
        method: str,
        url: str,
        headers: Mapping[str, str],
        body: Optional[bytes],
        timeout: float,
    ) -> HttpResponse:
        req = urllib.request.Request(url, data=body, headers=dict(headers), method=method)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return HttpResponse(resp.status, _lower(resp.headers), resp.read())
        except urllib.error.HTTPError as e:
            try:
                data = e.read()
            finally:
                e.close()
            return HttpResponse(e.code, _lower(e.headers), data)
        except urllib.error.URLError as e:
            if isinstance(e.reason, (socket.timeout, TimeoutError)):
                raise TimeoutError(str(e.reason)) from e
            raise OSError(str(e.reason)) from e
        except socket.timeout as e:  # read timeout after headers
            raise TimeoutError(str(e)) from e


def _lower(headers: Any) -> Dict[str, str]:
    return {k.lower(): v for k, v in (headers.items() if headers else [])}


def orders_message(wallet: str, timestamp_ms: int) -> str:
    """The text a wallet signs (``personal_sign``) to read its own orders with ``my_orders``."""
    return f"Omni Strike: read my orders\nWallet: {wallet.lower()}\nTimestamp: {timestamp_ms}"


def parse_retry_after(value: Optional[str], now: Optional[float] = None) -> Optional[float]:
    """``Retry-After`` (delta-seconds or HTTP-date) -> seconds."""
    if value is None or not value.strip():
        return None
    v = value.strip()
    try:
        return max(0.0, float(v))
    except ValueError:
        pass
    try:
        at = email.utils.parsedate_to_datetime(v)
    except (TypeError, ValueError):
        return None
    if at is None:
        return None
    if at.tzinfo is None:
        at = at.replace(tzinfo=_dt.timezone.utc)
    ref = now if now is not None else time.time()
    return max(0.0, at.timestamp() - ref)


@dataclass
class OmniStrike:
    """Omni Strike API client: best execution for crypto options across venues.

    >>> os_ = OmniStrike(api_key="...")  # doctest: +SKIP
    >>> os_.compare_quotes({"underlying": "BTC", "option_kind": "call", "strike": "120000",
    ...     "expiry": "2026-12-25T08:00:00Z", "side": "buy", "quantity": "0.5"})  # doctest: +SKIP

    Args:
        base_url: API root.
        api_key: integrator API key, sent as ``Authorization: Bearer <api_key>``.
        timeout: per-attempt timeout in seconds (compare may wait up to 15 s for slow venues).
        max_retries: retries for idempotent GETs answered 429/503. POSTs are never retried.
        retry_base_delay: first backoff (s), doubled per attempt with jitter.
        max_retry_delay: cap for any single wait, including ``Retry-After``.
        transport: custom HTTP layer.
        sleep: custom sleep (testing hook).
    """

    base_url: str = DEFAULT_BASE_URL
    api_key: Optional[str] = None
    timeout: float = 30.0
    max_retries: int = 2
    retry_base_delay: float = 0.25
    max_retry_delay: float = 10.0
    headers: Dict[str, str] = field(default_factory=dict)
    transport: Transport = field(default_factory=UrllibTransport)
    sleep: Callable[[float], None] = time.sleep

    def __post_init__(self) -> None:
        self.base_url = self.base_url.rstrip("/")
        self.max_retries = max(0, self.max_retries)

    # ------------------------------------------------------------ discovery

    def platforms(self) -> PlatformsResponse:
        """``GET /v1/platforms``: platforms with status, capabilities and live health."""
        return cast(PlatformsResponse, self.request("GET", "/v1/platforms"))

    def instruments(
        self,
        underlying: str,
        *,
        option_kind: Optional[str] = None,
        expiry: Any = None,
        family: Optional[str] = None,
        offset: Optional[int] = None,
        limit: Optional[int] = None,
    ) -> InstrumentsResponse:
        """``GET /v1/instruments``: options (or binaries) grouped by kind/strike/expiry."""
        if isinstance(expiry, _dt.datetime):
            expiry = _codec.to_rfc3339(expiry)
        query = {
            "underlying": underlying,
            "option_kind": option_kind,
            "expiry": expiry,
            "family": family,
            "offset": offset,
            "limit": limit,
        }
        return cast(InstrumentsResponse, self.request("GET", "/v1/instruments", query=query))

    # ------------------------------------------------------------ pricing

    def compare_quotes(self, request: CompareRequest) -> CompareResponse:
        """``POST /v1/quotes/compare``: quantity-aware cross-venue comparison (never retried)."""
        return cast(CompareResponse, self.request("POST", "/v1/quotes/compare", body=request))

    def explain_comparability(self, a: Mapping[str, str], b: Mapping[str, str]) -> Dict[str, Any]:
        """``POST /v1/comparability/explain`` for two InstrumentKeys."""
        return self.request("POST", "/v1/comparability/explain", body={"a": a, "b": b})

    def preview_route(self, request: Mapping[str, Any]) -> Dict[str, Any]:
        """``POST /v1/routes/preview``: single-destination preview (nothing is sent)."""
        return self.request("POST", "/v1/routes/preview", body=request)

    def simulate_order(self, request: Mapping[str, Any]) -> Dict[str, Any]:
        """``POST /v1/orders/simulate``: persisted simulation record."""
        return self.request("POST", "/v1/orders/simulate", body=request)

    def get_simulation(self, simulation_id: str) -> Dict[str, Any]:
        """``GET /v1/simulations/{id}``."""
        return self.request("GET", f"/v1/simulations/{_seg(simulation_id)}")

    # ------------------------------------------------------------ routing & execution

    def quote_route(self, request: RouteQuoteRequest) -> RouteQuote:
        """``POST /v1/routes/quote``: executable BUY route; single-use id + hash valid 30 s."""
        return cast(RouteQuote, self.request("POST", "/v1/routes/quote", body=request))

    def execute_testnet(self, route_id: str, route_hash: str) -> ExecutionReport:
        """``POST /v1/routes/execute-testnet``: TESTNET only (403 EXECUTION_DISABLED otherwise).

        Never retried automatically: a route can be consumed only once.
        """
        body = {"route_id": route_id, "route_hash": route_hash}
        return cast(ExecutionReport, self.request("POST", "/v1/routes/execute-testnet", body=body))

    def prepare_noncustodial(self, route_id: str, route_hash: str, account: Dict[str, Any]) -> Dict[str, Any]:
        """``POST /v1/noncustodial/prepare``: NON-CUSTODIAL step 1. Returns per leg the EIP-712
        ``typed_data`` the end user signs (``eth_signTypedData_v4``). Omni Strike holds no key.

        ``account`` = ``{"owner": "0x…", "signer": "0x…" (optional), "subaccount_id": 123}``.
        """
        body = {"route_id": route_id, "route_hash": route_hash, "account": account}
        return cast(Dict[str, Any], self.request("POST", "/v1/noncustodial/prepare", body=body))

    def submit_noncustodial(
        self,
        route_id: str,
        auth_timestamp_ms: int,
        auth_signature: str,
        signatures: List[Dict[str, str]],
    ) -> ExecutionReport:
        """``POST /v1/noncustodial/submit``: NON-CUSTODIAL step 2 (single use, never retried).

        ``auth_signature`` = ``personal_sign(str(auth_timestamp_ms))`` by the same signer;
        ``signatures`` = ``[{"order_id": …, "signature": "0x…"}]`` (one per prepared leg).
        """
        body = {
            "route_id": route_id,
            "auth_timestamp_ms": auth_timestamp_ms,
            "auth_signature": auth_signature,
            "signatures": signatures,
        }
        return cast(ExecutionReport, self.request("POST", "/v1/noncustodial/submit", body=body))

    def derive_subaccounts(self, wallet: str, timestamp_ms: int, signature: str) -> Dict[str, Any]:
        """``POST /v1/noncustodial/derive/subaccounts``: READ-ONLY, the user's Derive subaccount ids.

        ``signature`` = ``personal_sign(str(timestamp_ms))`` by ``wallet`` (fresh, < 2 min).
        """
        body = {"wallet": wallet, "timestamp_ms": timestamp_ms, "signature": signature}
        return cast(Dict[str, Any], self.request("POST", "/v1/noncustodial/derive/subaccounts", body=body))

    def my_orders(self, wallet: str, timestamp_ms: int, signature: str, limit: Optional[int] = None) -> Dict[str, Any]:
        """``POST /v1/noncustodial/orders``: the orders of ONE end user, newest first.

        ``signature`` = ``personal_sign`` by ``wallet`` of :func:`orders_message` (fresh, < 2 min).
        """
        body: Dict[str, Any] = {"wallet": wallet, "timestamp_ms": timestamp_ms, "signature": signature}
        if limit is not None:
            body["limit"] = limit
        return cast(Dict[str, Any], self.request("POST", "/v1/noncustodial/orders", body=body))

    def derive_account(self, wallet: str, timestamp_ms: int, signature: str, subaccount_id: int) -> Dict[str, Any]:
        """``POST /v1/noncustodial/derive/account``: READ-ONLY, the user's Derive subaccount
        (collaterals, positions, open orders, value, margins) as Derive returns it. Same auth as
        :meth:`my_orders`.
        """
        body = {"wallet": wallet, "timestamp_ms": timestamp_ms, "signature": signature, "subaccount_id": subaccount_id}
        return cast(Dict[str, Any], self.request("POST", "/v1/noncustodial/derive/account", body=body))

    def list_orders(self, limit: Optional[int] = None) -> List[Order]:
        """``GET /v1/orders``: order journal, newest first (limit 1..=1000). TESTNET; private in
        MAINNET_NONCUSTODIAL (use :meth:`my_orders`)."""
        return cast(List[Order], self.request("GET", "/v1/orders", query={"limit": limit}))

    def get_order(self, order_id: str) -> Order:
        """``GET /v1/orders/{id}``."""
        return cast(Order, self.request("GET", f"/v1/orders/{_seg(order_id)}"))

    def cancel_order(self, order_id: str) -> Order:
        """``POST /v1/orders/{id}/cancel``: TESTNET, non-final orders."""
        return cast(Order, self.request("POST", f"/v1/orders/{_seg(order_id)}/cancel"))

    def balances(self) -> BalancesResponse:
        """``GET /v1/balances``: execution-venue balances (TESTNET); empty otherwise."""
        return cast(BalancesResponse, self.request("GET", "/v1/balances"))

    def exec_stats(self) -> ExecStatsResponse:
        """``GET /v1/exec-stats``: fill ratio, slippage and latency per venue."""
        return cast(ExecStatsResponse, self.request("GET", "/v1/exec-stats"))

    def live_savings(self) -> LiveSavingsSnapshot:
        """``GET /v1/live/savings`` (404 ``LIVE_TRACKER_OFF`` when not running)."""
        return cast(LiveSavingsSnapshot, self.request("GET", "/v1/live/savings"))

    # ------------------------------------------------------------ strategies

    def quote_strategy(self, request: StrategyRequest) -> StrategyQuote:
        """``POST /v1/strategies/quote``: multi-leg strategy (1..=6 legs) priced on every venue
        with an atomicity-aware plan (single venue within tolerance, else best split).

        Quote only (nothing is sent), never retried. 422 ``LIMIT_EXCEEDED`` when your integrator
        limits refuse it. ``plan.net_cost`` > 0 is a debit, < 0 a credit.
        """
        return cast(StrategyQuote, self.request("POST", "/v1/strategies/quote", body=request))

    # ------------------------------------------------------------ integrator self-service

    def me(self) -> Integrator:
        """``GET /v1/integrator/me``: the calling integrator (401 without an integrator key)."""
        return cast(Integrator, self.request("GET", "/v1/integrator/me"))

    def update_settings(self, settings: IntegratorSettings) -> Integrator:
        """``PUT /v1/integrator/settings``: replaces ALL settings (send the full object, e.g.
        ``me()["settings"]`` edited). Hard caps apply (422 ``INVALID_REQUEST``). Not retried."""
        return cast(Integrator, self.request("PUT", "/v1/integrator/settings", body=settings))

    # ------------------------------------------------------------ health

    def health_ready(self) -> Readiness:
        """``GET /health/ready``: returns the body for 200 AND 503 (check ``ready``)."""
        return cast(
            Readiness, self.request("GET", "/health/ready", accept_status=(503,), retry=False)
        )

    def openapi(self) -> Dict[str, Any]:
        """``GET /openapi.json`` (raw, decimals not converted)."""
        return cast(Dict[str, Any], self.request("GET", "/openapi.json", raw_json=True))

    # ------------------------------------------------------------ transport

    def request(
        self,
        method: str,
        path: str,
        *,
        query: Optional[Mapping[str, Any]] = None,
        body: Any = None,
        accept_status: Sequence[int] = (),
        retry: bool = True,
        timeout: Optional[float] = None,
        raw_json: bool = False,
    ) -> Any:
        """Low-level call for endpoints not wrapped yet. GETs get the retry policy."""
        url = self._url(path, query)
        headers = {
            "Accept": "application/json",
            "User-Agent": f"omni-strike-python/{__version__}",
            **self.headers,
        }
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"
        data: Optional[bytes] = None
        if body is not None:
            headers["Content-Type"] = "application/json"
            data = _codec.dumps(body).encode("utf-8")

        attempts = self.max_retries + 1 if (method == "GET" and retry) else 1
        t = self.timeout if timeout is None else timeout
        for attempt in range(attempts):
            try:
                resp = self.transport.request(method, url, headers, data, t)
            except TimeoutError as e:
                raise OmniStrikeError(0, "TIMEOUT", f"{method} {url} timed out after {t} s") from e
            except OSError as e:
                raise OmniStrikeError(0, "NETWORK_ERROR", f"{method} {url} failed: {e}") from e
            if 200 <= resp.status < 300 or resp.status in accept_status:
                return self._parse(resp, raw_json)
            err = _to_error(resp)
            if attempt + 1 < attempts and resp.status in _RETRYABLE:
                self.sleep(self._backoff(attempt, err.retry_after))
                continue
            raise err
        raise AssertionError("unreachable")  # pragma: no cover

    def _backoff(self, attempt: int, retry_after: Optional[float]) -> float:
        if retry_after is not None:
            return min(retry_after, self.max_retry_delay)
        exp = self.retry_base_delay * (2**attempt)
        return min(exp * (0.5 + random.random() * 0.5), self.max_retry_delay)

    def _url(self, path: str, query: Optional[Mapping[str, Any]]) -> str:
        url = self.base_url + path
        if query:
            items = [(k, _qv(v)) for k, v in query.items() if v is not None]
            if items:
                url += "?" + urllib.parse.urlencode(items)
        return url

    @staticmethod
    def _parse(resp: HttpResponse, raw_json: bool) -> Any:
        if not resp.body:
            return None
        text = resp.body.decode("utf-8")
        try:
            if raw_json:
                import json

                return json.loads(text)
            return _codec.loads(text)
        except ValueError as e:
            raise OmniStrikeError(
                resp.status, "INVALID_RESPONSE", f"response is not JSON: {text[:200]}", body=text
            ) from e


class AdminClient:
    """Operator client for ``/v1/admin/*`` (integrator provisioning).

    Separate from :class:`OmniStrike` so an integrator key and the admin token (server
    ``OA_ADMIN_TOKEN``) are never mixed. Same transport, errors and retry policy (only the GET
    list is retried). 401 ``UNAUTHORIZED`` when admin is disabled on the server.

    Keyword arguments other than ``admin_token`` are passed to :class:`OmniStrike`.
    """

    def __init__(self, admin_token: str, **kwargs: Any) -> None:
        if not admin_token:
            raise ValueError("AdminClient: admin_token is required")
        if "api_key" in kwargs:
            raise TypeError("AdminClient: pass admin_token, not api_key")
        self._client = OmniStrike(api_key=admin_token, **kwargs)

    @property
    def base_url(self) -> str:
        return self._client.base_url

    def create_integrator(
        self, name: str, settings: Optional[IntegratorSettings] = None
    ) -> CreatedIntegrator:
        """``POST /v1/admin/integrators``: the integrator AND its API key (shown once)."""
        body = {"name": name, "settings": settings}
        return cast(CreatedIntegrator, self._client.request("POST", "/v1/admin/integrators", body=body))

    def list_integrators(self) -> List[Integrator]:
        """``GET /v1/admin/integrators``."""
        return cast(List[Integrator], self._client.request("GET", "/v1/admin/integrators"))

    def update_integrator_settings(
        self, integrator_id: str, settings: IntegratorSettings
    ) -> Integrator:
        """``PUT /v1/admin/integrators/{id}/settings`` (full settings; admin may exceed caps)."""
        path = f"/v1/admin/integrators/{_seg(integrator_id)}/settings"
        return cast(Integrator, self._client.request("PUT", path, body=settings))

    def set_integrator_active(self, integrator_id: str, active: bool) -> Integrator:
        """``PUT /v1/admin/integrators/{id}/active`` (404 ``NOT_FOUND`` if unknown)."""
        path = f"/v1/admin/integrators/{_seg(integrator_id)}/active"
        return cast(Integrator, self._client.request("PUT", path, body={"active": bool(active)}))


#: Same class under the TypeScript SDK's name.
OmniStrikeAdmin = AdminClient


def _qv(v: Any) -> str:
    if isinstance(v, bool):
        return "true" if v else "false"
    return str(v)


def _seg(s: str) -> str:
    return urllib.parse.quote(str(s), safe="")


def _to_error(resp: HttpResponse) -> OmniStrikeError:
    retry_after = parse_retry_after(resp.headers.get("retry-after"))
    text = resp.body.decode("utf-8", errors="replace") if resp.body else ""
    body: Any = text
    try:
        import json

        body = json.loads(text) if text else None
    except ValueError:
        pass
    env = body.get("error") if isinstance(body, dict) else None
    if isinstance(env, dict) and isinstance(env.get("code"), str):
        msg = env.get("message")
        return OmniStrikeError(
            resp.status,
            env["code"],
            msg if isinstance(msg, str) else "",
            body=body,
            retry_after=retry_after,
        )
    return OmniStrikeError(
        resp.status, f"HTTP_{resp.status}", text[:500], body=body, retry_after=retry_after
    )
