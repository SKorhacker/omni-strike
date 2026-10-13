# Mainnet non-custodial execution (`MAINNET_NONCUSTODIAL`)

Real funds, but **the server holds no key**: every mainnet order is signed — or every transaction
sent — by the end user's own wallet. The server builds the exact payload, the user signs it, the
server checks the signature against the declared signer and forwards it. It cannot create, modify
or replay an order on its own.

## Guarantees enforced in code (and covered by tests)

| Guard | Where |
|---|---|
| Only in a binary built with `--features mainnet-nc`; cannot be combined with `testnet` (compile error) | `crates/app/src/lib.rs`, `config.rs` |
| Own database: its name must contain `mainnet_nc` | `config.rs` |
| Custodial venues are dropped at startup (only `custody = NON_CUSTODIAL` executors) | `ExecutionService::new` |
| The custodial path `/v1/routes/execute-testnet` is refused | `ExecutionService::execute` |
| The attach function has no access to any key loader | `bin/server.rs::attach_mainnet_noncustodial_execution` |
| Kill switch on unless `OA_KILL_SWITCH=false` | server, `take_route` |
| Caps: 100 USDC per order, 500 per window by default; env can raise them up to hard ceilings 25 000 / 100 000 | `engine::risk_limits` |
| The window cap is the committed spend read from the order journal over `OA_SESSION_WINDOW_HOURS` (default 24): survives restarts, shared by every instance; no journal = no order | `ExecutionService::build_rows`, `Store::committed_spend_since` |
| Unsigned preparations never count toward the cap; the cap is re-checked at submission under a lock and the rows are journaled before the lock is released (nobody can block trading by preparing without signing) | `ExecutionService::reserve_session_spend` |
| Signatures are verified before a prepared execution is consumed (a forged submission cannot burn it); transaction venues check that the transaction was sent by the declared address | `check_user_signatures`, `thetanuts_user` / `callput_user` |
| Buy only; only mainnet networks pass the risk check | `risk_limits` |
| Route quotes single-use and hash-bound (30 s); signatures valid 5 min; prepared executions stored in Postgres and taken once across all instances; an incomplete submission does not consume them | `prepare_user_signed`, `submit_user_signed` |
| Every signature is recovered and compared with the declared signer before anything is sent | each `*_user.rs` executor |
| EIP-712 domain from the attached venue (`identity().chain_id`), never a default | `prepare_user_signed` |
| Order journal private: `GET /v1/orders` is 403; users read their own orders with a wallet signature (`POST /v1/noncustodial/orders`) | `api::list_orders` |
| Short-lived credentials (Derive REST auth signature, Paradex JWT, Aevo HMAC) used once and never journaled | `ExecutionService::persist` |
| IOC only: nothing rests on a venue | `execution.rs` |

Custodial `LIVE` does not exist (`--features live` is a compile error).

## Executors

| Venue | User signs / sends | Notes |
|---|---|---|
| Derive v3 (Ethereum, chain 1) | EIP-712 `Action` + `personal_sign` of a timestamp (REST auth) | Owner wallet or a trade-scoped session key. Web UI supported. |
| Aevo | EIP-712 order (signing key) | The integrator computes the HMAC with its own API secret; the secret never reaches the server. |
| Paradex | SNIP-12 order (Starknet account) + short-lived JWT | Option books reachable through the API are often empty (retail-only liquidity). |
| Thetanuts V4 (Base) | Sends `OptionBook.fillOrder` | Server reads the public receipt and checks it is exactly the prepared call. Web UI supported. |
| CallPut (Base) | Sends `createOpenPosition` | Multi-leg requests only (debit spreads, one transaction). Web UI supported. |
| Hypercall | EIP-712 `PlaceOrder` (wallet or approved agent) | No testnet. |
| Hyperliquid HIP-4 | Agent signature of the msgpack order action | Byte-identical to the official SDK (tested). |
| Limitless (Base) | EIP-712 CTF order + integrator HMAC | |

Each is opt-in: `OA_<VENUE>_NONCUSTODIAL=true`. Request formats: [integrators.md](integrators.md#6b-non-custodial-execution-the-user-signs).

## Running it

1. Create the database (name containing `mainnet_nc`) and copy `.env.mainnet-nc.example` to
   `.env.mainnet-nc`. Keep `OA_KILL_SWITCH=true` until everything below works.
2. Build and run:
   ```sh
   cargo build --release -p oa-app --features mainnet-nc --bin oa-server --target-dir target/mainnet-nc
   OA_ENV_FILE=.env.mainnet-nc ./target/mainnet-nc/release/oa-server
   ```
3. Check the full path up to the last gate: get a route, call prepare — it must be refused with
   "kill switch is ON".
4. Dry run of a first order without spending anything: `python scripts/omni_onchain.py derive`
   (finds an executable route and shows it), then `--simulate` (throwaway key, full
   prepare/sign/submit against the server).
5. Set `OA_KILL_SWITCH=false`, restart, and place a small order from your own wallet — in the web
   UI, or with `python scripts/omni_onchain.py VENUE --i-confirm-mainnet-real-funds`. The script
   decodes the bytes it is about to sign and refuses anything other than the requested buy, for the
   declared account, under `--max-cost`; the key is typed with hidden input and kept in memory only.

On a server: set `OA_API_TOKEN` (required for a non-loopback bind). The container stack for a VPS
is described in [deployment.md](deployment.md#mainnet-non-custodial-on-a-vps).

## Derive v3 notes

- Hosts `https://api.derive.xyz/v3`, `wss://api.derive.xyz/v3/ws`; deposits are made on Ethereum L1.
- The wallet is the user's own EOA or multisig; module addresses are identical on testnet and
  mainnet, only the chain id (hence the domain separator) differs.
- Nonce = UTC nanoseconds-style (`timestamp × 1e9 + salt`): a lost acknowledgement is reconciled by
  nonce, never resubmitted.
- Source: <https://docs.derive.xyz/migrating/breaking-changes>.
