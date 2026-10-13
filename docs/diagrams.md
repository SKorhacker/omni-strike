# How Omni Strike works, in diagrams

Every diagram below describes the code as it is; the file that implements each step is named under
it. GitHub renders the Mermaid blocks directly.

1. [System overview](#1-system-overview)
2. [One comparison, end to end](#2-one-comparison-end-to-end)
3. [Comparability verdict](#3-comparability-verdict)
4. [Cost of an order on one venue](#4-cost-of-an-order-on-one-venue)
5. [Routing: single venue or split](#5-routing-single-venue-or-split)
6. [Risk checks](#6-risk-checks)
7. [Trading modes and gates](#7-trading-modes-and-gates)
8. [Non-custodial execution](#8-non-custodial-execution)
9. [What the web UI accepts to sign](#9-what-the-web-ui-accepts-to-sign)
10. [Order lifecycle](#10-order-lifecycle)
11. [Venue actor: cache and circuit breaker](#11-venue-actor-cache-and-circuit-breaker)
12. [Deployment](#12-deployment)

## 1. System overview

```mermaid
flowchart TB
  subgraph Clients
    UI["Web UI (Next.js)<br/>user wallet via EIP-6963"]
    SDK["TypeScript / Python SDK"]
    AG["AI agent via oa-mcp (stdio)"]
  end
  subgraph Server["oa-server (Rust)"]
    API["HTTP API (Axum)<br/>auth · rate limits · OpenAPI"]
    ENG["Engine<br/>compare · route · strategy · binaries"]
    CORE["oa-core (pure, no I/O)<br/>comparability · book walk · fees<br/>cost · routing · risk"]
    ACT["Venue actors (Kameo)<br/>one per venue: cache, dedup, breaker"]
    EXE["Execution service<br/>journal · caps · reconciliation"]
  end
  V[("20 venues<br/>REST · WebSocket · JSON-RPC")]
  DB[("PostgreSQL<br/>journal · quotes · integrators")]
  UI -->|"/api/* proxy"| API
  SDK --> API
  AG --> API
  API --> ENG
  ENG --> CORE
  ENG --> ACT
  ACT --> V
  ENG --> EXE
  EXE --> V
  EXE --> DB
  ENG --> DB
```

`crates/core` · `crates/adapters` · `crates/app` (`engine.rs`, `venue.rs`, `execution.rs`, `api.rs`)

## 2. One comparison, end to end

```mermaid
sequenceDiagram
  autonumber
  participant C as Client
  participant A as API
  participant E as Engine
  participant VA as Venue actors
  participant K as oa-core
  C->>A: POST /v1/quotes/compare (contract, side, size, accepted differences)
  A->>A: auth, rate limit, validate
  A->>E: compare
  E->>E: drop venues excluded by the geo policy
  par every venue
    E->>VA: catalog (cached)
    VA-->>E: listings
  end
  E->>E: exact contract match per venue or NO_LISTING
  par listed venues, bounded by the latency budget
    E->>VA: order book (cached / streamed)
    VA-->>E: book or failure
  end
  E->>K: comparability, book walk, fees, ranking
  K-->>E: ranked / incomplete / ineligible / unavailable
  E-->>A: response
  A-->>C: JSON (decimals as strings)
  E--)E: persist observations asynchronously
```

`engine.rs::compare`, `core/cost.rs`, `core/comparability.rs`

## 3. Comparability verdict

Two contracts are compared term by term. Ranking across venues only happens for
`STRICTLY_COMPARABLE`, or for `ECONOMICALLY_SIMILAR_WITH_DIFFERENCES` when the caller accepted
every difference code.

```mermaid
flowchart TD
  S([Contract A vs contract B]) --> F{Same family<br/>dated option or binary?}
  F -- no --> NC[NOT_COMPARABLE]
  F -- yes --> CORE{Underlying, call/put,<br/>strike, expiry instant}
  CORE -- one differs --> NC
  CORE -- one unknown --> II[INSUFFICIENT_INFORMATION]
  CORE -- all equal --> SEC{Exercise style, settlement<br/>mechanism / asset / index / window,<br/>premium currency}
  SEC -- all known and equal --> SC[STRICTLY_COMPARABLE]
  SEC -- any differs or unknown --> ES[ECONOMICALLY_SIMILAR_WITH_DIFFERENCES<br/>+ reason codes]
  ES --> ACC{Caller accepted<br/>every code?}
  ACC -- yes --> RANK[Ranked together]
  ACC -- no --> INEL[Shown, not ranked:<br/>DifferencesNotAccepted]
  SC --> RANK
```

Contract-size multipliers are normalised away, never a reason. `core/comparability.rs`

## 4. Cost of an order on one venue

```mermaid
flowchart TD
  B([Book snapshot + requested size]) --> FR{Fresh enough?<br/>max_age, clock skew}
  FR -- no --> X1[Ineligible: Stale / ClockSkew]
  FR -- yes --> W[Walk the asks level by level<br/>until the size is covered]
  W --> COV{Size covered?}
  COV -- no --> PART[PARTIAL_COVERAGE:<br/>average price of what is visible]
  COV -- yes --> AVG[Average price, total premium]
  PART --> FEE
  AVG --> FEE{Fee formula known<br/>for this venue and size?}
  FEE -- no --> X2[Ineligible: UnknownCosts<br/>never priced as zero]
  FEE -- yes --> TOT[Total = premium + fees<br/>per underlying unit]
  TOT --> R([Ranked by total cost])
```

`core/book.rs`, `core/fees.rs`, `core/cost.rs`

## 5. Routing: single venue or split

```mermaid
flowchart TD
  Q([Buy intent + split policy]) --> RV[Routable venues:<br/>only reason = PARTIAL_COVERAGE]
  RV --> SUB[Every subset of size ≤ max_legs<br/>1 when split is off]
  SUB --> MERGE[Merge ask levels, cheapest per<br/>underlying unit first, within budgets]
  MERGE --> RECOST[Re-cost each leg exactly on its venue<br/>fixed fees paid once per leg]
  RECOST --> FULL{Any plan covers<br/>the full size?}
  FULL -- no --> P[PARTIAL: best partial plan,<br/>not executable]
  FULL -- yes --> SINGLE{A single venue<br/>covers it?}
  SINGLE -- no --> SCOV[SPLIT_COVERAGE]
  SINGLE -- yes --> SAVE{Split saves ≥ min_saving_abs<br/>and ≥ min_saving_bps?}
  SAVE -- yes --> SCOST[SPLIT_COST]
  SAVE -- no --> ONE[SINGLE: cheapest venue]
  SCOV --> LIM
  SCOST --> LIM
  ONE --> LIM[IOC limit per leg = worst level<br/>× 1 + tolerance, rounded up to the tick]
  LIM --> RQ([route_id + route_hash, single use, 30 s])
```

`core/routing.rs`, `engine.rs::route_quote` — details in [routing.md](routing.md)

## 6. Risk checks

Every check runs and every violation is returned (no short-circuit), before anything is sent.

```mermaid
flowchart LR
  I([Order intent]) --> C1[Kill switch]
  C1 --> C2[Mode allows submission]
  C2 --> C3[Venue, network, underlying,<br/>family allowed]
  C3 --> C4[Sell allowed?<br/>off on mainnet]
  C4 --> C5[Quantity > 0, limit > 0]
  C5 --> C6[Worst cost ≤ per-order cap]
  C6 --> C7[Committed spend + this order<br/>≤ window cap]
  C7 --> C8[Limit within max deviation<br/>of the reference price]
  C8 --> C9[Strategy max loss bounded]
  C9 --> OK{Any violation?}
  OK -- yes --> REJ([Refused with the list])
  OK -- no --> GO([Journaled, then sent])
```

`core/risk.rs`, `engine.rs::risk_limits`; the window cap is checked again at submission under a
lock (`execution.rs::reserve_session_spend`).

## 7. Trading modes and gates

```mermaid
flowchart TD
  BUILD{Build features} -->|default| D[READ_ONLY · SIMULATION]
  BUILD -->|testnet| T[TESTNET]
  BUILD -->|mainnet-nc| M[MAINNET_NONCUSTODIAL]
  BUILD -->|live| L[compile error:<br/>custodial live does not exist]
  BUILD -->|testnet + mainnet-nc| L2[compile error]
  T --> TG{Startup: database name<br/>contains 'testnet'?}
  M --> MG{Startup: database name<br/>contains 'mainnet_nc'?}
  TG -- no --> STOP[refuse to start]
  MG -- no --> STOP
  TG -- yes --> TK[Testnet executors only<br/>test keys from .env.testnet]
  MG -- yes --> MK[User-signed executors only<br/>no key loader]
  TK --> KS{OA_KILL_SWITCH=false?}
  MK --> KS
  KS -- no --> RO[Every order refused]
  KS -- yes --> EX[Orders allowed within caps]
```

`crates/app/src/lib.rs`, `config.rs`, `bin/server.rs`

## 8. Non-custodial execution

```mermaid
sequenceDiagram
  autonumber
  participant U as User wallet
  participant C as Client
  participant S as oa-server
  participant DB as Postgres
  participant V as Venue / chain
  C->>S: POST /v1/routes/quote
  S-->>C: route_id, route_hash, legs + IOC limits
  C->>S: POST /v1/noncustodial/prepare (route, user account)
  S->>S: consume route (single use), risk checks
  S->>DB: VALIDATED rows + prepared execution (expires in 4.5 min)
  S-->>C: EIP-712 typed data per leg, or a transaction to send
  C->>U: eth_signTypedData_v4 / eth_sendTransaction
  U-->>C: signatures / transaction hashes
  C->>S: POST /v1/noncustodial/submit
  S->>S: recover every signer = declared account (before consuming)
  S->>DB: take prepared execution (atomic, once across instances)
  S->>DB: window cap re-checked, rows RESERVED / SUBMITTING
  S->>V: forward signed orders, or read the mined receipt
  V-->>S: fills
  S->>DB: final states
  S-->>C: execution report
```

`execution.rs::prepare_user_signed` / `submit_user_signed`, `adapters/src/*_user.rs`

## 9. What the web UI accepts to sign

Even if the server were compromised, the page refuses anything outside these rules.

```mermaid
flowchart TD
  P([Prepared execution from the server]) --> D{Derive leg?}
  D -- yes --> DC{chainId 1 or 11155111,<br/>verifyingContract = Derive Matching,<br/>message.owner = connected wallet?}
  DC -- no --> STOP([Refused])
  DC -- yes --> SIGN[eth_signTypedData_v4]
  D -- no --> TX{Transaction leg<br/>Thetanuts / CallPut}
  TX --> TC{Base chain, known contract,<br/>ETH value ≤ allowed,<br/>approval: USDC, known spender,<br/>amount ≤ 2 × signed worst cost?}
  TC -- no --> STOP
  TC -- yes --> AL{Current allowance<br/>lower than needed?}
  AL -- yes --> APP[Send exact approval, wait for receipt]
  AL -- no --> SEND
  APP --> SEND[Send transaction, wait for receipt]
  SIGN --> AUTH[Derive login: personal_sign timestamp]
  SEND --> SUB([Submit to the server])
  AUTH --> SUB
```

`web/lib/noncustodial.ts`, `web/lib/wallet.ts`

## 10. Order lifecycle

```mermaid
stateDiagram-v2
  [*] --> VALIDATED: risk checks passed, journaled
  VALIDATED --> CANCELLED: never signed (expires) / cap reached at submission
  VALIDATED --> SUBMITTING: cap reserved, journaled before sending
  SUBMITTING --> ACKNOWLEDGED
  SUBMITTING --> FILLED
  SUBMITTING --> PARTIALLY_FILLED
  SUBMITTING --> REJECTED: definite 4xx
  SUBMITTING --> UNKNOWN: timeout / transport error
  UNKNOWN --> FILLED: reconciled from the venue
  UNKNOWN --> PARTIALLY_FILLED
  UNKNOWN --> REJECTED
  UNKNOWN --> CANCELLED
  ACKNOWLEDGED --> FILLED
  ACKNOWLEDGED --> CANCELLED: IOC remainder
  PARTIALLY_FILLED --> FILLED
  PARTIALLY_FILLED --> CANCELLED: IOC remainder
  FILLED --> [*]
  REJECTED --> [*]
  CANCELLED --> [*]
```

An `UNKNOWN` order is looked up by its precomputed venue id (three quick attempts, then every 10 s,
also after a restart). It is **never resubmitted**. `execution.rs` (`submit_row`, `OrderTracker`)

## 11. Venue actor: cache and circuit breaker

```mermaid
stateDiagram-v2
  [*] --> Closed
  Closed --> Closed: fetch ok (cache updated)
  Closed --> Closed: failure (< 5 in a row, cache marked Failed)
  Closed --> Open: 5 consecutive failures → 30 s
  Closed --> Open: HTTP 429 → Retry-After (≤ 60 s)
  Open --> Closed: delay elapsed, next fetch allowed
  note right of Closed
    one in-flight fetch per key,
    concurrent callers share it;
    books LRU-bounded (256)
  end note
```

`crates/app/src/venue.rs`

## 12. Deployment

```mermaid
flowchart LR
  NET((Internet)) -->|443 / 80| CADDY[Caddy<br/>HTTPS · headers]
  CADDY -->|"/"| WEB[web :3000]
  CADDY -->|"/api/* allowlist + token"| OA[oa-server :8080]
  CADDY -->|"/v1/* (integrator key)"| OA
  CADDY -.->|"/v1/admin → 404"| X[blocked]
  OA --> PG[(Postgres<br/>not published)]
  PROM[Prometheus 127.0.0.1] -.-> PG
  ADMIN[Operator] -->|SSH tunnel| OA
```

`deploy/docker-compose.yml`, `deploy/Caddyfile*` — details in [deployment.md](deployment.md)
