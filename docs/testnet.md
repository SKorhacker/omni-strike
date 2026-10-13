# TESTNET mode

Real orders on venue **test networks**, with test API keys held by the server. Use it to measure
quoted-vs-filled prices and to exercise the execution path end to end without real funds.

## Isolation from mainnet

| Gate | How |
|---|---|
| Build | `OA_TRADING_MODE=TESTNET` is accepted only by a binary built with `--features testnet` (cannot be combined with `mainnet-nc`) |
| Config | its own env file: `OA_ENV_FILE=.env.testnet` (template `.env.testnet.example`, git-ignored) |
| Database | name must contain `testnet`, otherwise startup is refused |
| Market data | testnet / demo endpoints of the same venues the orders go to |
| Networks | instruments are tagged with their test network; the risk check refuses anything else |
| Executors | each testnet executor has a testnet-only constructor; endpoints and EIP-712 domains are constants |
| Kill switch | on unless `OA_KILL_SWITCH=false` |
| Caps | 1 000 USDC per order and per venue, 5 000 per session, buy only |
| MCP | `execute_testnet_route` refused unless the MCP process sets `OA_MCP_ALLOW_TESTNET_EXECUTION=true` |
| Logs | every line carries `profile="testnet"`; startup and executions go to `audit_log`; keys are never logged (names and public addresses only) |

## Accounts (one per venue, all optional)

Use **new wallets and accounts that will never hold real funds**. Keys go in `.env.testnet` on
your machine only. A venue whose variables are missing is simply not attached (its name is logged).

**Derive v3** (Ethereum Sepolia, chain 11155111) — the easiest: test USDC is mintable.
1. Connect a new wallet on `testnet.app.derive.xyz`, **Deposit → Mint** test USDC, deposit it into a
   subaccount (credited ~2 min after mining).
2. Signer: preferably a session key with trade scope only (`trade:orderbook:*`); a throwaway
   wallet's own key also works (`signer == owner`).
3. `OA_DERIVE_TESTNET_WALLET`, `OA_DERIVE_TESTNET_SUBACCOUNT_ID`, `OA_DERIVE_TESTNET_SIGNER_KEY`.

**Aevo** — `testnet.aevo.xyz`, "Enable trading", testnet USDC from the team (the docs ask you to
contact them). Create an API key/secret in settings; register a signing key either in the app or
with `python scripts/testnet_aevo.py register` (asks for the test wallet key with hidden input,
generates a new signing key, writes `.env.testnet`). Variables: `OA_AEVO_TESTNET_WALLET`,
`OA_AEVO_TESTNET_API_KEY`, `OA_AEVO_TESTNET_API_SECRET`, `OA_AEVO_TESTNET_SIGNING_KEY`.

**OKX demo trading** — API key in *Demo Trading* with Trade + Read, no withdrawal:
`OA_OKX_DEMO_API_KEY`, `OA_OKX_DEMO_API_SECRET`, `OA_OKX_DEMO_PASSPHRASE`.

**Bybit testnet** — separate account on `testnet.bybit.com`, Unified Trading Account, test coins,
HMAC key with Options Trade + Read: `OA_BYBIT_TESTNET_API_KEY`, `OA_BYBIT_TESTNET_API_SECRET`.

**Deribit testnet** — account on `test.deribit.com` holding USDC, key with `trade:read_write` +
`account:read`, no `wallet` scope: `OA_DERIBIT_TESTNET_CLIENT_ID`, `OA_DERIBIT_TESTNET_CLIENT_SECRET`.

Non-custodial on testnet: `OA_DERIVE_NONCUSTODIAL=true` (or `AEVO` / `PARADEX`) replaces the
custodial Derive executor with the user-signed one, so the web UI's wallet flow can be tried on
Sepolia.

## Run

```sh
cargo build --release -p oa-app --features testnet --bin oa-server --target-dir target/testnet
OA_ENV_FILE=.env.testnet ./target/testnet/release/oa-server          # OA_BIND=127.0.0.1:8081 in the template
cd web && OA_BACKEND_URL=http://127.0.0.1:8081 pnpm dev --port 3001
```

Readiness and measured series (read-only check first; orders only with the explicit flag):

```sh
python scripts/testnet_derive.py check
python scripts/testnet_derive.py run --orders 10 --i-confirm-testnet-orders
python scripts/testnet_aevo.py check
```

## Order lifecycle

`VALIDATED → SUBMITTING → ACKNOWLEDGED | PARTIALLY_FILLED | FILLED | REJECTED | CANCELLED`, or
`UNKNOWN` when the outcome is not known. Every transition is journaled before the next step.

A transport error after sending gives `UNKNOWN`: the order is looked up by its precomputed venue id
(Derive: the nonce; Aevo: the EIP-712 digest), three quick attempts then every 10 s by the
`OrderTracker`, also after a restart. **It is never resubmitted.** Definite 4xx rejections are
final; unknown venue status strings are never treated as final.

## Common Derive errors

| Symptom | Cause |
|---|---|
| auth error | signer is neither the wallet nor a session key registered for it |
| subaccount error | the id does not belong to the wallet |
| USDC 0 | minted but not deposited into the subaccount |
| `insufficient_margin` | not enough USDC for premium + fees |
| invalid signature | wrong key, or the session key lacks the trade scope (`private/order_debug` shows the exchange's hashes) |
