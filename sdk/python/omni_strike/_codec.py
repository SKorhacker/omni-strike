"""JSON codec: API decimals (JSON strings) <-> ``decimal.Decimal``, never ``float``.

The server serialises every rust_decimal value as a JSON string. Responses are decoded with a
field-name allow-list (the decimal fields of the OpenAPI schemas): a string under one of these
keys that parses as a decimal becomes ``Decimal``. Identifiers (``native_id``, ``route_hash``...)
are never touched, and JSON numbers that are statistics (``saving_pct``, ``win_rate``) stay
``float``/``int`` exactly as the server sends them.
"""

from __future__ import annotations

import json
from decimal import Decimal, InvalidOperation
from typing import Any

#: Keys whose values are decimal strings in the v1 schemas.
DECIMAL_FIELDS = frozenset(
    {
        # instruments / terms
        "strike", "contract_multiplier", "tick_size", "min_amount", "amount_step",
        # compare / estimates
        "quantity", "amount", "requested_underlying", "covered_underlying", "covered_native",
        "premium", "avg_price_per_underlying", "book_impact", "index_price", "total_estimated",
        "total_per_underlying", "required_funds", "declared_balance", "required",
        # routing
        "quality_penalty_bps", "min_saving_abs", "min_saving_bps", "min_leg_underlying",
        "limit_tolerance_bps", "qty_underlying", "qty_native", "expected_premium",
        "expected_avg_per_underlying", "worst_level_price_native", "limit_price_native",
        "expected_total", "ranking_total", "estimated_saving_vs_best_single",
        # orders / execution / stats
        "max_fee_per_unit", "expected_fee", "filled_native", "avg_fill_price_native",
        "filled_underlying", "fill_ratio", "slippage_bps_mean", "slippage_bps_max",
        "router_penalty_bps", "available", "equity",
        # simulation
        "quantity_underlying", "quantity_native", "expected_avg_price_per_underlying",
        "limit_price_per_underlying", "estimated_total", "total", "quoted_avg", "fresh_avg",
        "drift_bps",
        # live savings
        "best_total", "derive_total",
        # recorded books / fee schedules (simulation records)
        "price", "size", "mark_price", "rate", "round_up_to",
        # multi-leg strategies (best_single_venue is a [platform, decimal] pair, see _walk)
        "ratio", "net_cost", "best_split_net_cost", "saving_vs_baseline", "single_venue_tolerance",
        # integrator settings
        "max_notional_per_order_usd", "max_notional_per_day_usd",
    }
)

#: Keys whose value is a map of name -> decimal string.
DECIMAL_MAPS = frozenset({"simulated_balances", "by_strategy"})


def _dec(v: Any) -> Any:
    if isinstance(v, str):
        try:
            d = Decimal(v)
        except InvalidOperation:
            return v
        return d if d.is_finite() else v
    return v


def _walk(node: Any, key: str = "") -> Any:
    if isinstance(node, dict):
        out = {}
        for k, v in node.items():
            if k in DECIMAL_FIELDS and isinstance(v, str):
                out[k] = _dec(v)
            elif k in DECIMAL_MAPS and isinstance(v, dict):
                out[k] = {mk: _dec(mv) for mk, mv in v.items()}
            elif k == "best_single" and isinstance(v, list) and len(v) == 2:
                # (InstrumentKey, Decimal) tuple serialised as a 2-element array.
                out[k] = [_walk(v[0]), _dec(v[1])]
            elif k == "best_single_venue" and isinstance(v, list) and len(v) == 2:
                # (PlatformId, Decimal) tuple serialised as a 2-element array.
                out[k] = [v[0], _dec(v[1])]
            else:
                out[k] = _walk(v, k)
        return out
    if isinstance(node, list):
        return [_walk(x, key) for x in node]
    return node


def loads(text: str) -> Any:
    """Parse a response body. Non-integer JSON numbers stay ``float`` (statistics only)."""
    return _walk(json.loads(text))


class _Encoder(json.JSONEncoder):
    def default(self, o: Any) -> Any:
        if isinstance(o, Decimal):
            return format(o, "f") if o.is_finite() else str(o)
        try:
            import datetime as _dt

            if isinstance(o, _dt.datetime):
                return to_rfc3339(o)
        except ImportError:  # pragma: no cover
            pass
        return super().default(o)


def to_rfc3339(dt: Any) -> str:
    """Datetime -> RFC 3339 UTC string ("Z"). Naive datetimes are assumed to be UTC."""
    import datetime as _dt

    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=_dt.timezone.utc)
    return dt.astimezone(_dt.timezone.utc).isoformat().replace("+00:00", "Z")


def _check_no_float(node: Any, path: str = "$") -> None:
    if isinstance(node, float):
        raise TypeError(
            f"OmniStrike: {path} is a float ({node!r}); pass decimals as Decimal or str, "
            f"e.g. Decimal({str(node)!r})"
        )
    if isinstance(node, dict):
        for k, v in node.items():
            _check_no_float(v, f"{path}.{k}")
    elif isinstance(node, (list, tuple)):
        for i, v in enumerate(node):
            _check_no_float(v, f"{path}[{i}]")


def _drop_none(node: Any) -> Any:
    if isinstance(node, dict):
        return {k: _drop_none(v) for k, v in node.items() if v is not None}
    if isinstance(node, (list, tuple)):
        return [_drop_none(v) for v in node]
    return node


def dumps(payload: Any) -> str:
    """Serialise a request body: ``None`` fields dropped (the server rejects unknown/null
    extras on strict schemas), ``Decimal`` -> plain string, floats refused."""
    payload = _drop_none(payload)
    _check_no_float(payload)
    return json.dumps(payload, cls=_Encoder, separators=(",", ":"))
