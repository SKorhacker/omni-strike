# Executable routing ("Jumper" for options)

Question answered: for this BUY intent (underlying, call/put, strike, expiry UTC, quantity), which
covered venue(s) give the best **estimated** total cost that can actually be executed — one venue,
or a split across 2–3 venues — and with which IOC limit per leg.

Code: `crates/core/src/routing.rs` (pure), `Engine::route_quote` (`crates/app/src/engine.rs`).
API: `POST /v1/routes/quote`. MCP: `quote_route`. UI: "Routage exécutable" panel.

## Which venues are considered
From the comparison of the exact contract (same kind, strike, expiry instant):
- **Routable** = every ineligibility reason is `PARTIAL_COVERAGE` (insufficient depth is fine: a
  split may complete it). Anything else excludes the venue and is listed in `candidates[].reasons`:
  `DifferencesNotAccepted`, `Stale`, `ClockSkew`, `UnknownCosts`, `PlatformUnhealthy`,
  `NO_VISIBLE_DEPTH`, and in TESTNET `NO_TESTNET_EXECUTION_FOR_THIS_VENUE`.
- Contractual differences (Derive ↔ Aevo: settlement index, Aevo window) must be **explicitly
  accepted** in `accepted_differences`; otherwise the other venue is never routed.
- Unknown costs exclude the venue; they are never priced as zero.
- Budgets: `simulated_balances` (declared, never presented as real) cap each venue's spend, fees included.

## Algorithm
1. Enumerate every subset of routable venues of size ≤ `max_legs` (1 when split is off).
2. For a subset, merge the ask levels of all its venues, cheapest price **per underlying unit**
   first; allocate in units of the strictest amount step of the subset, within budgets.
3. Drop legs below the venue minimum or `min_leg_underlying` (the smaller subset is evaluated anyway).
4. Re-cost each leg **exactly** on its own venue: book walk + fee formula. Fixed fees (Derive $0.5
   per order) are paid once **per leg** — the main reason a split rarely pays for small sizes.
   A leg exceeding its budget is shrunk and the freed quantity re-allocated (≤ 4 rounds).
5. Rank full-coverage plans by `expected_total + Σ premium × quality_penalty_bps`.
6. Choose:
   - `SINGLE`: cheapest full-coverage single venue (default; split off).
   - `SPLIT_COST`: a split saving ≥ `min_saving_abs` **and** ≥ `min_saving_bps` vs the best single venue.
   - `SPLIT_COVERAGE`: no single venue covers the quantity, a split does.
   - `PARTIAL`: nothing covers it → best partial plan, **not executable**.
7. Each leg's IOC limit = worst consumed level × (1 + `limit_tolerance_bps`), rounded **up** to the
   tick, never below the worst level.

## Split policy (all optional)
| Field | Default | Meaning |
|---|---|---|
| `allow_split` | false | Splitting is opt-in |
| `max_legs` | 2 | 1..=3 |
| `min_saving_abs` | 1 | premium currency |
| `min_saving_bps` | 10 | of the best single total |
| `min_leg_underlying` | 0 | besides venue minimums |
| `limit_tolerance_bps` | 50 | 0..=500 |

## Partial fills and leg risk
Legs are **not atomic** across venues. Each leg is an IOC limit order: what is not filled at once
is cancelled by the venue, so nothing rests. In this version a partially executed split is
**accepted and reported** (execution report note + order states); there is no automatic
re-routing of the remainder (backlog). Cancel exists for any non-final order (`/v1/orders/{id}/cancel`).

## Execution quality feedback
`/v1/exec-stats` computes per venue the mean slippage vs quote and the reject rate. With ≥ 10
testnet orders, `router_penalty_bps = max(0, mean adverse slippage) + 100 × reject rate` is added
to that venue's ranking (never to the displayed expected total). Below 10 orders: no penalty.

## Route quote contract
`route_id` + `route_hash` (SHA-256 over mode, expiry and every leg's venue/network/instrument/
quantity/limit), valid **30 s**, **single use** (consumed atomically in PostgreSQL). Any change
needs a new quote. `executable` is true only in TESTNET, kill switch off, full coverage.

## Measured (live mainnet data, 2026-10-05, `scripts/measure_routing.py`)
30 liquid (≤ 30 d, ±5 % of spot) + 30 random contracts listed on both venues, BUY 0.1 and 1:
- Full coverage by one venue: 30/30 liquid, 29/30 random (1 contract: no venue had depth).
- **Split never chosen** at these sizes (one venue — mostly Derive — always covered them; a split
  would add a fixed fee and Aevo's books are shallow).
- On replayed real books, BUY 100 ETH → `SPLIT_COST` (Aevo 55 @ ~101.7 + Derive 45) — test
  `split_is_chosen_on_real_books_when_it_saves`.
- Route quote latency: p50 200–670 ms, p95 0.3–2.7 s (first subscription to a new Derive book is the slow part).

## Update 2026-10-05: 3 venues, real balances
- **Paradex** added read-only (third venue). Live coverage, 40 liquid contracts (<= 30 d, ±5 % of
  spot), BUY 0.1, differences accepted (`scripts/measure_venues.py`):

  | Venue | Lists the contract | Any API-reachable depth | Full coverage | Ranked |
  |---|---|---|---|---|
  | Aevo | 27 | 24 | 24 | 24 |
  | Derive | 20 | 20 | 20 | 20 |
  | **Paradex** | **40** | **0** | **0** | **0** |

  Paradex lists every liquid contract but its standard (API) books were empty: its visible liquidity
  is retail-only RPI orders, which API orders cannot take. It is shown honestly as incomplete
  (`NO_VISIBLE_DEPTH`) and never priced from RPI. It widens the catalogue, not the executable depth.
- **Real balances** (TESTNET): the execution venue's available balance (authenticated API) is the
  leg budget (a declared simulated balance can only lower it). Balance 0 -> `INSUFFICIENT_FUNDS`;
  balance read error -> `BALANCE_UNAVAILABLE` (never treated as 0 or unlimited). `GET /v1/balances`.
