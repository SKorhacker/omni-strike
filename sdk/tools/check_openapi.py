"""SDK drift check against a server's OpenAPI document (stdlib only, read-only).

Reports:
  * API paths/methods not wrapped by the SDKs (new endpoints to add);
  * decimal-typed schema properties missing from the Python codec allow-list
    (they would stay ``str`` instead of ``Decimal``);
  * decimal property names that ALSO appear as non-decimal elsewhere (allow-list collision).

    python sdk/tools/check_openapi.py [http://127.0.0.1:8080]
Exit code 1 when something needs attention.
"""

from __future__ import annotations

import json
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))
from omni_strike._codec import DECIMAL_FIELDS, DECIMAL_MAPS  # noqa: E402

#: (method, path) wrapped by BOTH SDKs. Keep in sync with client.ts / client.py.
COVERED = {
    ("get", "/health/ready"), ("get", "/openapi.json"),
    ("get", "/v1/platforms"), ("get", "/v1/instruments"),
    ("post", "/v1/quotes/compare"), ("post", "/v1/comparability/explain"),
    ("post", "/v1/routes/preview"), ("post", "/v1/orders/simulate"),
    ("get", "/v1/simulations/{id}"),
    ("post", "/v1/routes/quote"), ("post", "/v1/routes/execute-testnet"),
    ("get", "/v1/orders"), ("get", "/v1/orders/{id}"), ("post", "/v1/orders/{id}/cancel"),
    ("get", "/v1/balances"), ("get", "/v1/exec-stats"), ("get", "/v1/live/savings"),
    ("post", "/v1/strategies/quote"),
    ("post", "/v1/noncustodial/prepare"), ("post", "/v1/noncustodial/submit"),
    ("post", "/v1/noncustodial/derive/subaccounts"),
    ("post", "/v1/noncustodial/orders"),
    ("post", "/v1/noncustodial/derive/account"),
    ("get", "/v1/integrator/me"), ("put", "/v1/integrator/settings"),
    # admin client (OmniStrikeAdmin / AdminClient)
    ("get", "/v1/admin/integrators"), ("post", "/v1/admin/integrators"),
    ("put", "/v1/admin/integrators/{id}/settings"), ("put", "/v1/admin/integrators/{id}/active"),
}
#: Deliberately not wrapped (disabled, internal or unsupported).
IGNORED = {
    ("get", "/health/live"), ("get", "/v1/metrics"), ("post", "/v1/orders"),
    ("post", "/v1/orders/prepare"), ("get", "/v1/positions"),
    ("post", "/v1/simulations/{id}/replay"),
    # Server-Sent Events: consumed with EventSource / an SSE client, not a request/response wrapper.
    ("get", "/v1/stream/quotes"),
}
DECIMAL_PATTERN = r"^-?\d+(\.\d+)?([eE]\d+)?$"


def main() -> int:
    base = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8080").rstrip("/")
    with urllib.request.urlopen(base + "/openapi.json", timeout=15) as r:
        doc = json.loads(r.read().decode("utf-8"))
    problems = 0

    ops = {(m, p) for p, item in doc.get("paths", {}).items() for m in item if m in
           ("get", "post", "put", "patch", "delete")}
    missing = sorted(ops - COVERED - IGNORED)
    stale = sorted((COVERED | IGNORED) - ops - {("get", "/openapi.json")})
    for m, p in missing:
        print(f"NOT WRAPPED   {m.upper():6} {p}")
    for m, p in stale:
        print(f"not in spec   {m.upper():6} {p}  (route not documented or not deployed)")
    problems += len(missing)

    schemas = doc.get("components", {}).get("schemas", {})
    dec, other = set(), set()

    def is_dec(v):
        return isinstance(v, dict) and v.get("pattern") == DECIMAL_PATTERN

    def is_plain_str(v):
        # `#[schemars(with = "String")]` decimals are documented as bare strings (no pattern):
        # not a collision with a patterned decimal of the same name.
        t = v.get("type") if isinstance(v, dict) else None
        return (t == "string" or t == ["string", "null"]) and not (
            {"format", "enum", "$ref", "pattern"} & set(v))

    def walk(n):
        if isinstance(n, dict):
            for k, v in (n.get("properties") or {}).items():
                if is_dec(v):
                    dec.add(k)
                elif isinstance(v, dict) and is_dec(v.get("additionalProperties")):
                    if k not in DECIMAL_MAPS:
                        print(f"DECIMAL MAP   {k}: add to DECIMAL_MAPS")
                        nonlocal_problems.append(k)
                elif not is_plain_str(v):
                    other.add(k)
            for v in n.values():
                walk(v)
        elif isinstance(n, list):
            for v in n:
                walk(v)

    nonlocal_problems: list = []
    walk(schemas)
    platform_ids = set(schemas.get("PlatformId", {}).get("enum", []))
    for k in sorted(dec - DECIMAL_FIELDS - platform_ids):
        print(f"DECIMAL FIELD {k}: add to DECIMAL_FIELDS (python/omni_strike/_codec.py) and TS types")
        problems += 1
    for k in sorted((DECIMAL_FIELDS & other)):
        print(f"COLLISION     {k}: decimal in one schema, non-decimal in another; codec needs a path rule")
        problems += 1
    problems += len(nonlocal_problems)

    print(f"\n{len(ops)} operations in spec, {len(ops & COVERED)} wrapped, "
          f"{len(ops & IGNORED)} ignored; {len(dec)} decimal property names.")
    print("OK" if not problems else f"{problems} item(s) need attention")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
