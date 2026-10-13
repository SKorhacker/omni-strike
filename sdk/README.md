# Omni Strike SDKs

Official clients for the Omni Strike API (crypto options best execution for integrators).

| SDK | Path | Runtime deps |
|---|---|---|
| TypeScript (`@omni-strike/sdk`) | [`typescript/`](typescript/README.md) | none (global `fetch`, Node >= 18 / browsers) |
| Python (`omni-strike`) | [`python/`](python/README.md) | none (stdlib `urllib`, Python >= 3.9) |

Both share the same contract:

- API key passed at construction, sent as `Authorization: Bearer <key>`.
- Decimals never become floats (TS: `string`; Python: `decimal.Decimal`); float inputs are refused.
- Errors are typed (`OmniStrikeError`: `status`, `code`, `message`) from the server envelope
  `{"error":{"code","message"}}`.
- Retries with backoff only for idempotent GETs on 429/503, honouring `Retry-After`;
  POSTs/PUTs (compare, route/strategy quote, execute, cancel, settings) are never retried.
- Integrator self-service (`me`, `update settings`) lives on the main client; integrator
  provisioning (`/v1/admin/*`) is a separate admin client (`OmniStrikeAdmin` / `AdminClient`)
  built with the operator admin token.

## Keeping the SDKs in sync with the server

```bash
python sdk/tools/check_openapi.py http://127.0.0.1:8080
```

Read-only. Lists operations in `/openapi.json` that the SDKs do not wrap, and decimal schema
fields missing from the Python decimal allow-list (`python/omni_strike/_codec.py`). When an
endpoint is added: add the types (`typescript/src/types.ts`, `python/omni_strike/models.py`),
the method (`client.ts`, `client.py`), a fake-transport unit test in each SDK, and the
`(method, path)` to `COVERED` in the checker.

Decimals declared with `#[schemars(with = "String")]` appear in the spec as bare strings (no
decimal pattern), so the checker cannot detect them: add them to `DECIMAL_FIELDS` by hand (e.g.
`ratio`, `single_venue_tolerance`, `max_notional_per_*`). Routes the server serves but does not
document (currently `/v1/integrator/*`, `/v1/admin/*`, `/v1/balances`) are listed as
`not in spec` for information only.
