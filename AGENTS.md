# AGENTS.md — working on Omni Strike

Instructions for AI coding agents (and humans in a hurry). Read this before changing code.
Product overview: [README.md](README.md). Visual explanations: [docs/diagrams.md](docs/diagrams.md).

## What this is

A best-execution router for crypto options: it normalises 20 venues into one contract model,
prices an order for a given size (book walk + fee formula), refuses to compare contracts that are
not the same unless each difference is accepted, routes to one venue or splits, and executes
**non-custodially** on mainnet (the end user's wallet signs; the server holds no key).

## Map: where things live

| Task | Files |
|---|---|
| Domain types (instrument, side, platform ids, modes) | `crates/core/src/domain.rs` |
| Comparability verdicts and reason codes | `crates/core/src/comparability.rs` |
| Book walk / fees / cost / ranking | `crates/core/src/{book,fees,cost}.rs` |
| Routing (single / split, IOC limits) | `crates/core/src/routing.rs` |
| Risk checks | `crates/core/src/risk.rs`; limits per mode: `crates/app/src/engine.rs::risk_limits` |
| A venue's market data | `crates/adapters/src/<venue>.rs` (`Connector` trait in `adapters/src/lib.rs`) |
| Custodial testnet executors | `crates/adapters/src/<venue>_exec.rs` |
| Non-custodial executors (user signs) | `crates/adapters/src/<venue>_user.rs` (`ExecutionVenue` trait in `adapters/src/exec.rs`) |
| Platform inventory (all 28, with sources) | `crates/adapters/src/registry.rs` |
| Which venues are attached per mode | `crates/app/src/bin/server.rs` |
| Engine (compare, route, strategies) | `crates/app/src/engine.rs`, `strategy.rs`, `binaries.rs` |
| Venue actors (cache, breaker) | `crates/app/src/venue.rs` |
| Execution journal, caps, reconciliation | `crates/app/src/execution.rs` |
| HTTP routes, auth, rate limits | `crates/app/src/api.rs`; OpenAPI paths: `openapi.rs` |
| Integrator keys and limits | `crates/app/src/integrators.rs` |
| Postgres + in-memory store | `crates/app/src/store.rs`; migrations `crates/app/migrations/` |
| Config / startup gates | `crates/app/src/config.rs`, `crates/app/src/lib.rs` |
| MCP tools | `crates/app/src/mcp.rs` |
| Web UI | `web/app/*`, wallet: `web/lib/wallet.ts`, `web/lib/noncustodial.ts` |
| SDKs | `sdk/typescript/src/`, `sdk/python/omni_strike/`; drift check `sdk/tools/check_openapi.py` |

## Commands

```sh
# Rust (offline: no network, no database, cannot trade)
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features testnet -- -D warnings
cargo clippy --workspace --all-targets --features mainnet-nc -- -D warnings
cargo test --workspace
cargo test -p oa-app --features mainnet-nc
cargo test -p oa-app <name>                    # one test / module

# Real Postgres store tests (optional)
OA_TEST_DATABASE_URL=postgres://… cargo test -p oa-app pg_tests

# Web
cd web && pnpm install --frozen-lockfile && pnpm exec next typegen && pnpm exec tsc --noEmit && pnpm lint && pnpm build

# SDKs
cd sdk/typescript && npx -p typescript tsc -p . && npm test
cd sdk/python && python -m pytest -q
```

A change is done when all of the above that apply are green. CI (`.github/workflows/ci.yml`) runs
the same checks plus cargo-deny, a Docker build and a secret scan.

## Rules (non-negotiable)

1. **`crates/core` stays pure**: no I/O, no async, no HTTP, no clock reads hidden in logic.
2. **Unknown is not zero.** A fee, cost or contract term that is not known stays `None` /
   `FeeAmount::Unknown` and excludes the venue from routing. Never default it to `0`.
3. **Decimals never go through floats.** `rust_decimal::Decimal` in Rust, strings on the wire.
4. **Execution safety is layered; do not remove a layer.** Feature gates, startup checks, kill
   switch, caps (checked at prepare and again at submission), single-use routes and prepared
   executions, signer recovery before consuming, IOC only, buy-only on mainnet. Any change to
   `execution.rs`, `*_user.rs`, `*_exec.rs`, `risk.rs` or `config.rs` needs a test that proves the
   **refusal** path.
5. **No custody.** Nothing in a `mainnet-nc` build may load or hold a user key. Custodial live
   trading must stay a compile error.
6. **Migrations are forward-only and non-destructive.** New numbered file; never `DROP`,
   `TRUNCATE` or edit an applied migration.
7. **No secrets anywhere** (code, fixtures, logs, tests). Tests use public dummy keys
   (`keccak("cow")`, `[5u8; 32]`…) or keys generated at runtime. Short-lived user credentials are
   redacted before journaling (`ExecutionService::persist`).
8. **Every parser change has a recorded fixture**; every signing change has a reference vector
   from the venue's own library (`fixtures/`, `scripts/gen_*_vectors.py`).
9. **Keep the API shape stable.** JSON field names are part of the contract with the SDKs and the
   UI; when you add or change an endpoint, update `openapi.rs`, both SDKs and
   `sdk/tools/check_openapi.py`.

## Safety rules for agents

- Never place a real order, send a transaction, approve a token or move funds. Mainnet orders are
  the end user's decision, signed in their wallet.
- Never ask for, print, store or paste private keys, seed phrases or API secrets. If you see one,
  stop and tell the user it must be rotated.
- Never set `OA_KILL_SWITCH=false` or raise caps on someone's behalf.
- Never commit `.env*` files other than the `*.example` templates.

## Testing patterns

- Offline engines: `FixtureConnector` (`crates/app/src/fixtures.rs`) replays recorded venue
  responses; `MemStore` replaces Postgres.
- Execution: `FakeVenue` / `FakeFill` (`fixtures.rs`) simulate fills, partial fills, rejections and
  lost acknowledgements; `e2e_tests.rs` has harnesses (`harness()`, `harness_admin()`, `testnet()`)
  and the non-custodial flow against local fake venues (`mod prepared_persistence`).
- HTTP-level tests call the router directly (`tower::ServiceExt::oneshot`), no port needed.
- Live endpoints only behind `--features live-tests`.

## Gotchas

- `docs/integrators.md` and `config/geo-policy.json` are compiled into the binary (`include_str!`):
  editing them changes the server, and the Dockerfile copies them.
- `cargo clippy --features testnet` and `--features mainnet-nc` compile different code paths:
  always lint all three.
- On Windows, build with the MSVC toolchain from PowerShell/CMD (Git Bash's `link` shadows MSVC's).
- User-facing strings (web UI, API messages) are in English.
- Mermaid diagrams in `docs/diagrams*.md` must match the code: update them when you change a flow.

## Skills

Step-by-step procedures live in [`.claude/skills/`](.claude/skills) (plain Markdown, usable by any
agent): `verify` (all checks), `add-venue` (new connector end to end), `omni-strike-api` (use the
running API / MCP to quote, route and execute correctly).
