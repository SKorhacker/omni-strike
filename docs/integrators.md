# Omni Strike — Integrator guide (API v1)

One API to get the best price for any crypto option across every venue (onchain, CEX, prediction
markets), for single options and multi-leg strategies, and to execute it. SDKs: `sdk/typescript`,
`sdk/python`. Machine-readable spec: `GET /openapi.json`.

## 1. Environments
| | Base URL | Execution |
|---|---|---|
| **Sandbox** | a `--features testnet` deployment | real orders on venue test networks only (test funds) |
| **Read-only** | a default deployment | quotes, routes, simulations |
| **Mainnet non-custodial** | a `--features mainnet-nc` deployment | real orders signed by the end user (section 6b) |
Same API, same keys format; keys are per environment.

## 2. Authentication & limits
- Header `Authorization: Bearer os_…` (your integrator key, shown once at creation — store it safely).
- Defaults ON for every key (change them yourself within the caps, `PUT /v1/integrator/settings`):
  max notional per order 250,000 USD / per day 2,000,000 USD, max slippage 50 bps, unwind cost cap 30 bps,
  single-venue tolerance 10 bps, simulation first, 20 req/s (burst 40), naked sells OFF, allowed venues /
  underlyings = all. Self-service caps: 5M / 50M USD, 500 bps, 300 bps, 200 bps, 200 req/s; above → the operator (admin API).
- Enforced on `/v1/routes/quote`, `/v1/strategies/*`, `/v1/stream/quotes`: allowed underlyings and venues, max
  notional per order (single option: quantity x strike; strategy: quantity x sum of ratios x index). The daily
  notional is charged when a route you quoted is executed or prepared (per server instance, UTC day); a route
  quoted with another key is refused.
- `GET /v1/integrator/me` returns your settings. Errors: `401 UNAUTHORIZED`, `429 RATE_LIMITED`,
  `422 LIMIT_EXCEEDED | INVALID_REQUEST`, `503 SERVICE_BUSY` (retry with backoff).

## 3. Price one option
`POST /v1/quotes/compare`
```json
{"underlying":"BTC","option_kind":"call","strike":"86000","expiry":"2026-12-25T08:00:00Z",
 "side":"buy","quantity":"1",
 "accepted_differences":["SETTLEMENT_INDEX_DIFFERS","SETTLEMENT_WINDOW_UNKNOWN","SETTLEMENT_WINDOW_DIFFERS",
                         "SETTLEMENT_ASSET_DIFFERS","PREMIUM_CURRENCY_DIFFERS"]}
```
Response: `ranked` (eligible venues, best first, total cost incl. fees), `incomplete`, `ineligible`
(with reasons), `unavailable` (venue issues, e.g. `SLOW_VENUE` = joins the next answer). Decimals are
strings. `latency_budget_ms` (default 220) bounds the wait for uncached venues. Underlyings: BTC, ETH,
SOL, XRP, HYPE. `family: "binary"` for prediction-market binaries (quantity = payout).

### 3b. User country
Add `"user_country": "BR"` (ISO 3166-1 alpha-2) to compare / route / strategy / binary requests. Venues whose
terms exclude that country are never compared or routed (`unavailable[].kind = GEO_RESTRICTED`, with the source
of the rule). Policy: `config/geo-policy.json` (compiled from each venue's terms; review it for your deployment). Server switch `OA_REQUIRE_USER_COUNTRY=true` makes it mandatory for executable routes.
Non-custodial does not remove these rules: orders reach venues from the server.

### 3c. Binaries (prediction markets) vs options
`POST /v1/binaries/compare {"underlying","direction":"above|below","strike","expiry","payout":"1000","user_country"}`
returns the best binary (Kalshi, Hyperliquid HIP-4, …) and the cheapest options vertical spread that pays AT
LEAST the payout in every scenario at the same expiry (super-replication), plus `cheapest` =
`BINARY | OPTIONS_SPREAD | UNDECIDED_UNKNOWN_FEES`. Binaries with unpublished fees are shown as a lower bound
(`binary_premium_only`); a verdict is only given when it holds whatever those fees are.

## 4. Live prices (push)
`GET /v1/stream/quotes?underlying=BTC&option_kind=call&strike=86000&expiry=2026-12-25T08:00:00Z&side=buy&quantity=1&interval_ms=500`
Server-Sent Events, event `quote`: `{t, best:{platform,total}, venues:[…], derive_total, saving_vs_derive_pct, unavailable}`.
One computation per distinct contract shared by all subscribers.

## 5. Multi-leg strategies
`POST /v1/strategies/quote` — 1-6 legs `{option_kind, strike, expiry, side, ratio?}` + `quantity`.
Returns `plan`: `SINGLE_VENUE` (all legs on one venue, atomic where supported, chosen when within
`single_venue_tolerance_bps` of the best split) or `SPLIT` (best venue per leg); `net_cost` (positive = debit,
negative = credit); `saving_vs_baseline` (vs doing it all on Derive).
`POST /v1/strategies/route` — same body (+ `limit_tolerance_bps`, `unwind_cost_cap_bps`): returns an
executable, single-use route (30 s) with IOC limits per leg, `max_loss` (null = unbounded → refused unless
`allow_naked_sells`), `route_id` + `route_hash`.

## 6. Execute (testnet)
`POST /v1/routes/execute-testnet {"route_id","route_hash"}` (single routes from `/v1/routes/quote` and
strategy routes). Strategy legs are sent **in parallel**; **all-or-nothing**: if a leg does not fill
completely, filled legs are unwound within the cap; beyond it the residual is kept and flagged.
Report: `orders`, `unwind_orders`, `strategy_outcome` (`COMPLETE | UNWOUND | UNWIND_INCOMPLETE | PENDING`),
`notes`. Orders: `GET /v1/orders`, `GET /v1/orders/{id}`, `POST /v1/orders/{id}/cancel`.

## 6b. Non-custodial execution (the user signs — recommended for onchain venues)
Omni Strike never holds your users' keys. Venues: Derive, Aevo, Paradex, Hypercall, Hyperliquid HIP-4, Limitless (signatures) and Thetanuts, CallPut (transactions sent by the wallet).
1. Get a route: `POST /v1/routes/quote` or `/v1/strategies/route`.
2. `POST /v1/noncustodial/prepare {"route_id","route_hash","account":{"owner":"0x…","signer":"0x…"(optional session key),"subaccount_id":123}}`
   → `legs[]` with `typed_data` (EIP-712) and `digest`, valid 5 minutes.
3. In your app, the user's wallet (or a trading-only session key) signs each `typed_data` with
   `eth_signTypedData_v4`, and signs the current time in ms with `personal_sign` (`auth_signature`).
4. `POST /v1/noncustodial/submit {"route_id","auth_timestamp_ms","auth_signature","signatures":[{"order_id","signature"}]}`
   → execution report (+ `execution.report` webhook). We recover every signer and refuse anything that
   does not match before sending; a submission is single use.
Strategy legs are sent in parallel; if one does not fill, `strategy_outcome = UNWIND_REQUIRES_USER_SIGNATURE`
(an unwind needs new user signatures: quote the opposite legs and sign them).
Server switch (operators): `OA_DERIVE_NONCUSTODIAL=true`. Reference clients: `scripts/noncustodial_derive.py`, and the
web UI (`web/lib/noncustodial.ts`: browser wallet via EIP-6963, Derive + Thetanuts + CallPut).
Subaccount id unknown? `POST /v1/noncustodial/derive/subaccounts {"wallet","timestamp_ms","signature"}` (signature =
`personal_sign(String(timestamp_ms))` by the wallet, < 2 min) returns `subaccount_ids` — read-only, checked locally,
forwarded to Derive, never stored. A prepared execution survives a server restart (kept in the database for 4.5 min).

**Following the user's account** (`{"wallet","timestamp_ms","signature"}`, valid 2 min, cannot trade or move funds):
- `POST /v1/noncustodial/orders` (+ optional `limit`): the orders of THAT wallet that were actually sent (any
  non-custodial venue whose account is an EVM address). `signature` = `personal_sign` of
  `"Omni Strike: read my orders\nWallet: <lowercased address>\nTimestamp: <timestamp_ms>"` — bound to this service,
  so it is useless as a venue login. `GET /v1/orders` and `GET /v1/orders/{id}` are private (403 `PRIVATE`) in
  MAINNET_NONCUSTODIAL.
- `POST /v1/noncustodial/derive/account` (+ `subaccount_id`): the Derive subaccount as Derive returns it
  (collaterals, positions, open orders, value, margins). `signature` = Derive's own login,
  `personal_sign(String(timestamp_ms))`, forwarded to Derive.
- No cancel endpoint: every order is IOC, filled or cancelled by the venue at once; nothing rests.

### 6b-quinquies. Limitless binaries (non-custodial)
`accounts.limitless = {"maker": "0x…" (EOA, profile in eoa mode), "owner_id", "fee_rate_bps" (rank.feeRateBps),
"api_token_id"}`; the user signs `typed_data` (CTF `Order`); the integrator substitutes the signature into
`body_template` and computes `venue_auth = {timestamp (ISO-8601), hmac}` = base64(HMAC-SHA256(secret,
"<ts>\nPOST\n/orders\n<body>")) — the secret never reaches us. FAK buys. `OA_LIMITLESS_NONCUSTODIAL=true`.

### 6b-quater. Hyperliquid HIP-4 outcomes (non-custodial)
`accounts.hyperliquid = {"wallet": "0x…", "signer": "0x…" (API agent, optional)}`; each leg's `typed_data` is the
phantom `Agent{source "a", connectionId}` of the msgpack order action (byte-identical to the official SDK, tested);
the wallet or its agent signs; we verify the signer and POST /exchange. `OA_HYPERLIQUID_NONCUSTODIAL=true`.

### 6b-ter. Hypercall (non-custodial, EIP-712)
`accounts.hypercall = {"wallet": "0x…", "signer": "0x…" (approved agent, optional)}`; each leg's `typed_data` is a
`PlaceOrder` (route `best_execution` = book + RFQ auction, tif `ioc`) signed by the wallet or its agent; we verify the
signer and forward. Hypercall has no testnet (production chain 999). `OA_HYPERCALL_NONCUSTODIAL=true`.

### 6b-bis. Paradex (non-custodial, Starknet)
`accounts.paradex = {"account": "0x…"}`; each leg's `typed_data` is a SNIP-12 `Order` signed by the user's
Starknet account; submit `signature` (as the Paradex SDK flattens it) and `venue_auth: {"jwt": "…"}` — a
short-lived JWT the user's client obtained with its own key. We use it for that order only and never store it.
Caveat: API-reachable option books are often empty (retail-only RPI liquidity), so IOC orders may not fill. Server switch `OA_PARADEX_NONCUSTODIAL=true` (testnet).

### 6c. Transaction venues (Thetanuts, CallPut) — the user's wallet sends the transaction
For these venues `prepare` returns `legs[].transaction` = `{chain_id, to, data, value, num_contracts, maker_price,
approval}` (Base mainnet `OptionBook.fillOrder` against the market maker's best signed order within your limit).
The user's wallet sends it (and the USDC `approval` first if its allowance is lower); then
`submit {"route_id", "signatures": [{"order_id", "tx_hash"}]}`. We never send anything: we read the public receipt,
check it is exactly the prepared call (same contract, same calldata hash) and report the fill from the `OrderFilled`
event. Not mined yet = `UNKNOWN`, reconciled automatically from the public receipt. Account: `{"address": "0x…"}`.
CallPut (buys): `createOpenPosition` on Base with the ETH execution fee as `value`; a keeper executes or cancels the
request (budget refunded if cancelled); status read from `openPositionRequests`. Server switches
`OA_CALLPUT_NONCUSTODIAL=true`, `OA_THETANUTS_NONCUSTODIAL=true`; `OA_THETANUTS_REFERRER` = the operator's referrer address for the
OptionBook referral fee split.

## 7. Webhooks
Set `webhook_url` in your settings (https to a public host; `http://localhost` only when the server sets `OA_WEBHOOK_ALLOW_LOCAL=true`, for development). Redirects are not followed. Events: `execution.report`.
Headers: `X-OmniStrike-Event`, `X-OmniStrike-Timestamp`, `X-OmniStrike-Signature: v1=<hex HMAC-SHA256>`
over `"<timestamp>.<raw body>"` with your `webhook_secret` (returned once at key creation).
Verify (Python):
```python
import hmac, hashlib, time
def verify(secret, ts, body, sig):
    if abs(time.time() - int(ts)) > 300: return False          # replay protection
    mac = hmac.new(secret.encode(), f"{ts}.{body}".encode(), hashlib.sha256).hexdigest()
    return hmac.compare_digest(sig, "v1=" + mac)
```
Delivery: up to 4 attempts (0 s, 1 s, 4 s, 16 s); answer 2xx quickly.

## 8. Admin (Omni Strike operators)
`OA_ADMIN_TOKEN` (>= 24 chars) enables `/v1/admin/integrators` (create → key + webhook secret once; list;
`PUT …/{id}/settings` beyond caps; `PUT …/{id}/active`). `OA_REQUIRE_API_KEY=true` makes keys mandatory.
