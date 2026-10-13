---
name: omni-strike-api
description: Use a running Omni Strike server (HTTP API or the oa-mcp MCP tools) to price a crypto option across venues, explain why venues differ, build a route and execute it correctly — including the non-custodial wallet flow. Use when asked to find the best price or venue for a BTC, ETH, SOL, XRP or HYPE option or strategy, compare a binary with options, or prepare an order for a user to sign.
---

# Using the Omni Strike API

Base URL: the operator's server (local default `http://127.0.0.1:8080`). Every endpoint is in
`GET /openapi.json`; the full guide is `docs/integrators.md` (also `GET /docs`). Auth: header
`Authorization: Bearer <key>` when the server requires it. Errors:
`{"error": {"code", "message"}}`. **All numbers are decimal strings** — never send floats.

MCP alternative: the same operations as tools (`list_platforms`, `search_instruments`,
`compare_quotes`, `explain_comparability`, `preview_route`, `simulate_order`, `quote_route`,
`get_order_status`, `get_exec_stats`; `execute_testnet_route` only if the operator enabled it).

## 1. Find a listed contract

`GET /v1/instruments?underlying=BTC` (optional `option_kind`, `expiry`, `family=binary`). Use an
expiry **instant** (RFC 3339, UTC) and a strike that exist; never invent them.

## 2. Compare

```json
POST /v1/quotes/compare
{"underlying":"BTC","option_kind":"call","strike":"120000","expiry":"2026-12-25T08:00:00Z",
 "side":"buy","quantity":"1","accepted_differences":[]}
```

Read the answer in this order:

- `ranked`: comparable venues, cheapest total (premium + fees for THIS size) first.
- `ineligible`, with reasons. `DifferencesNotAccepted` means the contracts are similar but not
  identical (for example `SETTLEMENT_INDEX_DIFFERS`). Explain the difference to the user
  (`POST /v1/comparability/explain`) and **only add codes to `accepted_differences` if the user
  agrees**: they change the payoff.
- `incomplete` (not enough visible depth), `unavailable` (venue issue; `SLOW_VENUE` joins the next
  answer), `estimated_saving_vs_baseline`.
- An unknown fee is never zero: such venues are excluded, with the reason.

Optional `"user_country": "FR"` (ISO alpha-2) excludes venues whose terms forbid that country.

## 3. Route

`POST /v1/routes/quote` = the compare body + `"split": {"allow_split": true, "max_legs": 2}`.
It returns `plan` (`kind` SINGLE or SPLIT, `reason` starting with SINGLE, SPLIT_COST,
SPLIT_COVERAGE or PARTIAL), legs with an IOC `limit_price_native`, `executable`,
`not_executable_reasons`, and a single-use `route_id` + `route_hash` valid **30 seconds**.
PARTIAL plans are not executable.

Strategies (1 to 6 legs): `POST /v1/strategies/quote`, then `POST /v1/strategies/route`.
Binaries vs options: `POST /v1/binaries/compare`.

## 4. Execute

**Mainnet is non-custodial: you never sign for the user.** Your job ends at producing what the
user's wallet must sign.

1. `POST /v1/noncustodial/prepare`
   `{"route_id","route_hash","accounts":{"derive":{"owner":"0x…","subaccount_id":123}}}`
   (Thetanuts and CallPut: `{"address":"0x…"}`). Each leg returns either `typed_data` (EIP-712,
   signed with `eth_signTypedData_v4`) or a `transaction` the wallet sends. Valid about 4.5 minutes.
2. The user signs in their wallet. Derive also needs `auth_signature` = `personal_sign` of
   `String(auth_timestamp_ms)`.
3. `POST /v1/noncustodial/submit`
   `{"route_id","auth_timestamp_ms","auth_signature","signatures":[{"order_id","signature" or "tx_hash"}]}`.
   Single use; every signature is verified before anything is sent.

Orders are IOC: filled at once or cancelled by the venue, so there is nothing to cancel afterwards.
On a testnet server, `POST /v1/routes/execute-testnet {"route_id","route_hash"}` executes with the
server's test accounts.

Follow-up: `POST /v1/noncustodial/orders` (wallet signature of
`"Omni Strike: read my orders\nWallet: <lowercased address>\nTimestamp: <ms>"`) lists the user's
orders; `POST /v1/noncustodial/derive/account` returns Derive balances and positions.

## Common mistakes

| Mistake | Fix |
|---|---|
| Floats in JSON | Send `"1.5"`, not `1.5` |
| Accepting every difference code silently | Ask the user; explain with `/v1/comparability/explain` |
| Reusing a `route_id` | Single use, 30 s: quote again |
| Treating `incomplete` or `PARTIAL` as executable | Only `ranked` venues and full-coverage plans execute |
| `403 EXECUTION_DISABLED` | The server is read-only or the kill switch is on: report it, do not work around it |
