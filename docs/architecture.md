# Architecture

A modular monolith: one API binary (`oa-server`), one MCP binary (`oa-mcp`) that forwards to the
API, PostgreSQL, and a Next.js UI. No message bus, no cache server, no orchestrator.

## Crates

| Crate | Role | Rule |
|---|---|---|
| `oa-core` | Domain model and pure engines: comparability, book walk, fees, cost estimate and ranking, routing, strategies, risk checks | No I/O, no async, no HTTP: everything is a deterministic function of its inputs |
| `oa-adapters` | Venue connectors (native → canonical), execution venues, shared HTTP client, platform registry | Normalises; never decides routing |
| `oa-app` | Engine, venue actors, execution service, Axum API, OpenAPI, MCP, Postgres store | API, UI, SDKs and MCP all go through the same `Engine` |

## Connectors

`Connector` (`crates/adapters/src/lib.rs`) exposes `list_instruments`, `get_market_data` (book +
timings), `get_fees` and capability flags. Capabilities a venue does not implement return
`ConnectorError::Unsupported` — never an empty success.

Each connector maps native instruments to an `InstrumentKey` (underlying, kind, strike, expiry
instant, settlement asset/index/window, premium currency, multiplier, exercise style) and keeps the
native id. Terms that cannot be sourced stay `None` and surface as `INSUFFICIENT_INFORMATION` or a
difference to accept.

Wrappers: `UsdtAsUsdc` converts USDT-quoted venues at a live rate; `OnNetwork` tags a connector
with a test network so risk checks can tell testnet from mainnet instruments.

Streaming: Derive, Deribit, OKX, Bybit and Binance keep WebSocket books; others are polled.

## Venue actors (Kameo)

One `VenueActor` per venue (`crates/app/src/venue.rs`):

| Item | Value |
|---|---|
| Mailbox | bounded, 256 |
| Ask timeouts | mailbox 100 ms, reply 2 s → `Busy` |
| Network I/O | spawned tasks; results published in a per-key `watch` channel, then `FetchDone` |
| Dedup | one in-flight fetch per key; concurrent callers subscribe to the same channel |
| Cache | catalogs per underlying (TTL); books LRU-bounded (256), freshness by `max_age` |
| Failure | a failed fetch replaces the cached value: stale data is never served silently |
| Circuit breaker | 5 consecutive failures → open 30 s; HTTP 429 → open for `Retry-After` (≤ 60 s) |
| Hot books | recently requested books are refreshed in the background (max 4, 30 s window) |

The actor state is a cache only: restarting loses nothing order-related.

## Request path (compare)

1. Validate the request (underlying, quantity bounds, `max_age_ms`, accepted differences).
2. Catalogs from every venue in parallel → exact contract match per venue, or a `NO_LISTING` /
   `CATALOG_UNAVAILABLE` issue. Geo-restricted venues are dropped first.
3. Books in parallel, bounded by the latency budget; a slow venue is reported `SLOW_VENUE` and
   joins the next answer.
4. Pure core: comparability vs the reference contract → book walk for the size → fee formula →
   ranking → `ranked` / `incomplete` / `ineligible` / `unavailable`.
5. Observations are persisted asynchronously; persistence failures never block the response.

## Execution

`ExecutionService` (`crates/app/src/execution.rs`) is attached only in `TESTNET` and
`MAINNET_NONCUSTODIAL`.

- **Journal first.** Every order row is written to `exec_orders` + `order_events` before each
  step (`VALIDATED → SUBMITTING → ACKNOWLEDGED | PARTIALLY_FILLED | FILLED | REJECTED | CANCELLED`,
  or `UNKNOWN`). If the intent cannot be journaled, the leg is not sent.
- **Never resubmit.** A transport error after sending gives `UNKNOWN`; the `OrderTracker` actor
  reconciles it from the venue (or the public receipt for transaction venues) every 10 s, also
  after a restart. Unknown venue statuses are never treated as final.
- **IOC only.** Nothing rests on a venue; partial fills are reported.
- **Strategies** send legs in parallel; on testnet an incomplete strategy is unwound within a cost
  cap; non-custodially the unwind needs new user signatures and is reported as such.
- **Caps** are computed from the journal over a rolling window (`committed_spend_since`), under a
  lock that serialises read-spend → validate → journal.
- **Non-custodial**: `prepare` stores the prepared execution in `prepared_executions`; `submit`
  checks every signature against the declared signer, then takes it atomically (single use across
  instances).

## Storage

PostgreSQL via sqlx. Migrations in `crates/app/migrations/` are embedded and applied at startup,
forward-only and non-destructive. Tables: quote observations, simulations, route quotes
(single-use), execution journal, prepared executions, integrators, audit log. Tests use an
in-memory store with the same trait; `pg_tests` run the real store when `OA_TEST_DATABASE_URL` is
set.

## API layer

Axum 0.8 with: bearer token / integrator key auth, per-integrator token-bucket rate limits, a
global in-flight semaphore (`503 SERVICE_BUSY`), 64 KiB body limit, request timeout, typed error
envelope, OpenAPI document generated from the Rust types (`schemars`), SSE quote stream with one
computation per contract shared by all subscribers, HMAC-signed webhooks.
