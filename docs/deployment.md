# Deployment

Container deployment of Omni Strike: `oa-server` (Rust API + market-data ingestion), the `web`
demo UI (Next.js) and PostgreSQL 16, with an optional Caddy reverse proxy (automatic HTTPS) and
Prometheus/Grafana stubs.

> CI builds both images on every push. The full Compose stack (Caddy certificates, read-only
> filesystems, monitoring) has not been exercised end to end yet: do a first run on a staging host
> before relying on it, and see "To verify on the first run" at the end.

| File | Purpose |
|------|---------|
| `Dockerfile` | `oa-server` image: cargo-chef dependency caching, `debian:bookworm-slim` runtime, non-root (uid 10001), tini, HEALTHCHECK on `/health/ready` |
| `.dockerignore` | keeps `.env*`, `target/`, `node_modules/`, `.git/` out of the build context |
| `deploy/web.Dockerfile` | `web` image (Node 24, pnpm 10 via corepack, `next start`) |
| `deploy/docker-compose.yml` | single-node stack; profiles `proxy` and `monitoring` |
| `deploy/Caddyfile` | HTTPS, gzip/zstd, security headers, `/v1/admin` blocked publicly |
| `deploy/prometheus.yml` | Prometheus stub (Postgres exporter; oa-server job commented) |
| `deploy/.env.example` | every variable the server reads, with safe defaults |
| `deploy/docker-compose.mainnet-nc.yml`, `deploy/Caddyfile.mainnet-nc`, `deploy/.env.mainnet-nc.example` | mainnet non-custodial stack (section below) |
| `deploy/vps-bootstrap.sh` | one-time hardening + Docker install on a fresh VPS |

## Build

From the repository root (BuildKit is required for the cache mounts; it is the default in
current Docker):

```sh
# mainnet read-only API (the production image)
docker build -t omni-strike/oa-server:latest .
# testnet build: separate tag, separate database, NEVER deployed as production
docker build --build-arg CARGO_FEATURES=testnet -t omni-strike/oa-server:testnet .
# demo UI; OA_BACKEND_URL is baked into the Next.js rewrites at build time
docker build -f deploy/web.Dockerfile --build-arg OA_BACKEND_URL=http://oa-server:8080 \
  -t omni-strike/web:latest .
```

`--features live` does not compile, by design; there is no live image.

Release tags: tag images with the git SHA as well as `latest`
(`-t omni-strike/oa-server:$(git rev-parse --short HEAD)`) and set `OA_IMAGE_TAG` to the SHA in
production so a rollback is a one-line change.

## Run

```sh
cd deploy
cp .env.example .env && chmod 600 .env
# fill OA_PG_PASSWORD, OA_API_TOKEN, OA_ADMIN_TOKEN (openssl rand -hex 32), OA_DOMAIN, ACME_EMAIL
docker compose build
docker compose up -d                       # db + oa-server + web (ports on 127.0.0.1 only)
docker compose --profile proxy up -d       # + Caddy on 80/443 for OA_DOMAIN
docker compose --profile monitoring up -d  # + Prometheus (127.0.0.1:9090) / Grafana (127.0.0.1:3001)
docker compose ps                          # oa-server must become "healthy"
docker compose logs -f oa-server
```

Notes:

- `deploy/.env` is used both for compose interpolation and as the oa-server `env_file`.
  `DATABASE_URL`, `OA_BIND` and `OA_API_TOKEN` are set by compose and override the file.
- `OA_PG_PASSWORD` must be hex/alphanumeric: it is embedded unescaped in `DATABASE_URL`.
- Compose refuses to start if `OA_PG_PASSWORD`, `OA_API_TOKEN`, `OA_DOMAIN`, `ACME_EMAIL` or
  `GRAFANA_ADMIN_PASSWORD` are empty (the example file contains placeholders for the last three).
- The server binds `0.0.0.0:8080` in the container, which **requires `OA_API_TOKEN`** (startup
  is refused otherwise).
- **Known limitation, demo UI:** the browser calls `/api/v1/*` without a token, so with a
  token-protected API those calls return 401. Either run the UI only for operators who reach it
  through an SSH tunnel and a local backend, or enable the commented "PUBLIC DEMO MODE" block in
  the Caddyfile, which injects a token for `/api/*` and therefore makes the read-only API usable
  by anyone through that path. Fixing this properly needs a code change (server-side token in the
  Next.js proxy), out of scope here.
- `/health/ready` is 200 only when the database answers **and** at least one venue catalog is
  READY; the HEALTHCHECK allows a 60 s start period for the first catalogs. An unhealthy
  container is not restarted automatically by Docker; alert on it.
- Graceful shutdown: oa-server drains on SIGINT only, so the image and compose set
  `STOPSIGNAL SIGINT` / `stop_signal: SIGINT` (tini forwards it). SIGTERM would kill it
  without draining.
- The binary is compiled **without sqlx TLS**: Postgres must be reachable over a private network
  (the compose `backend` network). A managed database over the internet needs the sqlx `tls-*`
  feature added first.

## Single-node sizing

Memory and CPU are small (tens of MB, near-idle CPU under light load); the heavy part is network:
one set of WebSocket/REST connections per venue.

| Component | Start with | Compose limit |
|-----------|-----------|---------------|
| oa-server | 1 vCPU, 512 MB | 2 CPU, 1 GB |
| Postgres 16 | 1 vCPU, 1 GB, 20 GB SSD | 1 CPU, 1 GB |
| web | 0.5 vCPU, 256 MB | 1 CPU, 512 MB |
| Caddy | small | 1 CPU, 256 MB |

A 2 vCPU / 4 GB VM is plenty for a single node. Pick a region with
low latency to the venues that matter most (Derive, Deribit, OKX, Bybit); latency to venues,
not CPU, dominates quote freshness. Re-measure RSS after a day of uptime before shrinking limits.

## Upgrades and migrations

- Migrations live in `crates/app/migrations/` and are **embedded in the binary**
  (`sqlx::migrate!`), applied automatically at startup before the server listens. There is no
  separate migration job.
- Migrations are forward-only. Before any upgrade that adds a migration: take a backup (below).
  A rollback to an older image after a schema change works only if the change is
  backward-compatible; otherwise restore the backup.
- Procedure:
  ```sh
  docker compose build oa-server          # or pull the new tag; set OA_IMAGE_TAG
  ./backup.sh                             # see below
  docker compose up -d oa-server          # SIGINT drain, new container migrates then listens
  docker compose logs oa-server | grep -i "migrations applied"
  ```
- Single node = a few seconds of API downtime during the restart (catalog warm-up afterwards).
  Zero-downtime needs two API nodes behind a load balancer (see Scaling).
- Postgres major upgrades (16 -> 17) need `pg_upgrade` or dump/restore, never just a tag change.

## Postgres backup

Logical dump (small database: accounts, simulations, audit, execution journal):

```sh
# backup (custom format, compressed)
docker compose exec -T db pg_dump -U oa -d options_aggregator -Fc > backup-$(date +%F-%H%M).dump
# restore into an empty database
docker compose exec -T db pg_restore -U oa -d options_aggregator --clean --if-exists < backup-XXXX.dump
```

- Run it from cron at least daily, keep 7 daily + 4 weekly, copy off-host (S3/Backblaze with
  server-side encryption). A backup that has never been restored is not a backup: test a restore
  into a scratch container monthly.
- For point-in-time recovery later, add WAL archiving (pgBackRest or WAL-G).
- The volume `omni-strike_oa-pg` holds the data; `docker compose down -v` **deletes it**.

## Security checklist

- [ ] `OA_API_TOKEN` and `OA_ADMIN_TOKEN` are random, >= 32 bytes (`openssl rand -hex 32`).
      An admin token shorter than 24 characters is silently ignored (admin endpoints disabled).
- [ ] `deploy/.env` is `chmod 600`, owned by the deploy user, never committed (git-ignored by
      `.env.*`; the `.dockerignore` also keeps it out of images). Rotate tokens on staff changes.
- [ ] TLS everywhere public: Caddy in front, HSTS on. No plain HTTP API exposed.
- [ ] Firewall: inbound 22 (restricted to admin IPs / VPN), 80 and 443 only. Ports 8080, 3000,
      3001, 9090 are bound to 127.0.0.1 by compose; Postgres is not published at all. Note that
      Docker-published ports bypass ufw: keep them on 127.0.0.1.
- [ ] `/v1/admin/*` is blocked by Caddy; call it through `ssh -L 8080:127.0.0.1:8080`.
- [ ] Production runs the `latest` (mainnet, read-only) image with `OA_TRADING_MODE=READ_ONLY`
      or `SIMULATION`. **No testnet image, no `OA_*_TESTNET_*` / `OA_OKX_DEMO_*` keys, no
      `OA_KILL_SWITCH=false` in production.** The testnet image runs on a separate host or
      project with its own database (name containing `testnet`).
- [ ] Images contain no secrets (everything comes from env at run time). Verify with
      `docker history --no-trunc` and a secret scan (gitleaks/trivy) in CI.
- [ ] Containers run non-root, `no-new-privileges`, all capabilities dropped, read-only root
      filesystem for oa-server and web.
- [ ] `OA_REQUIRE_API_KEY=true` once integrators have their own keys; per-integrator rate limits
      set. Edge rate limiting (Caddy has none built in) via the caddy-ratelimit plugin or a CDN.
- [ ] Logs never contain secrets (the server never logs `DATABASE_URL` or keys); still ship logs
      to a store with restricted access.
- [ ] Image updates: rebuild monthly for Debian/Node security patches; pin base image digests
      for reproducibility once the pipeline is stable.

## Mainnet non-custodial on a VPS

A second, separate stack: `deploy/docker-compose.mainnet-nc.yml` (override, own compose project,
own volume and database `options_aggregator_mainnet_nc`), `deploy/Caddyfile.mainnet-nc`,
`deploy/.env.mainnet-nc.example`. End users trade from the web UI with **their own wallet**
(MetaMask, Rabby, Coinbase Wallet… via EIP-6963): Derive orders are signed in the wallet
(`eth_signTypedData_v4`), Thetanuts / CallPut transactions are sent by the wallet (exact USDC
approval only when the allowance is lower). The server holds no key and cannot move funds.

1. **VPS** (you): Ubuntu 22.04/24.04 or Debian 12, 2 vCPU / 4 GB is enough. Put your SSH public
   key in `/root/.ssh/authorized_keys`, then as root: `bash deploy/vps-bootstrap.sh deploy`
   (Docker + Compose, firewall 22/80/443, fail2ban, unattended upgrades, key-only SSH).
2. **DNS** (you): an A/AAAA record `OA_DOMAIN` -> the VPS IP.
3. **Secrets, on the VPS only** (as `deploy`):
   ```sh
   git clone https://github.com/SKorhacker/omni-strike.git && cd omni-strike/deploy
   cp .env.mainnet-nc.example .env.mainnet-nc && chmod 600 .env.mainnet-nc
   for v in OA_PG_PASSWORD OA_API_TOKEN OA_ADMIN_TOKEN; do sed -i "s/^$v=.*/$v=$(openssl rand -hex 32)/" .env.mainnet-nc; done
   $EDITOR .env.mainnet-nc        # OA_DOMAIN, ACME_EMAIL, venues; keep OA_KILL_SWITCH=true
   ```
4. **Start with the kill switch ON**:
   ```sh
   NC="docker compose -p omni-strike-nc -f docker-compose.yml -f docker-compose.mainnet-nc.yml --env-file .env.mainnet-nc"
   $NC build && $NC up -d && $NC --profile proxy up -d
   $NC ps                                   # oa-server "healthy"
   curl -s https://$OA_DOMAIN/api/v1/platforms | head -c 300   # mode MAINNET_NONCUSTODIAL
   ```
   Open `https://OA_DOMAIN`, compare a contract, "Proposer une route": the wallet block appears;
   prepare is refused with "kill switch is ON" — the whole path works up to the last gate.
5. **Open trading**: set `OA_KILL_SWITCH=false`, `$NC up -d oa-server`. First order with a small
   size from your own wallet (see docs/mainnet-noncustodial.md).

Notes:
- Caddy injects `OA_API_TOKEN` into the UI's `/api/*` calls: the non-admin API is public through
  the UI path, which a public non-custodial app needs. `/v1/admin` stays blocked.
- Caps are **server-wide** (all users together) over a rolling window
  (`OA_SESSION_WINDOW_HOURS`, default 24 h), computed from the order journal: a restart does not
  reset them and every instance sees the same spend. Raise them deliberately.
- Prepared executions (waiting for signatures) live in `prepared_executions`: a restart between
  "prepare" and "submit" no longer loses them, and two instances can share the load (submit is
  single-use across instances).
- Backups: same as above with `-p omni-strike-nc` and `-d options_aggregator_mainnet_nc`.

## Scaling later

Today **ingestion and the API run in the same `oa-server` process**: each instance opens its own
WebSocket/REST connections to every venue and keeps its own in-memory books. Consequences:

- Running N replicas = N times the venue connections and request rates. That is fine for 2
  instances (HA) but quickly hits venue rate limits beyond that.
- Per-integrator rate limits are in-process, so with N replicas the effective limit is N times
  the configured value unless the LB uses sticky routing by API key.

Short term (2 nodes, HA): two identical VMs, each with oa-server + web, a managed or replicated
Postgres (requires sqlx TLS if not on a private network), and an L7 load balancer health-checking
`/health/ready`. Accept double venue connections.

Target architecture:

1. Split the binary into **market-data ingestors** (one connection set per venue, single active
   instance + standby) and **stateless quote/route API nodes**.
2. Fan out books via a bus (NATS JetStream) from ingestors to API nodes.
3. Scale API nodes horizontally behind the load balancer; move rate limits and sessions to Redis.
4. Postgres for accounts/orders/journal, ClickHouse for book/quote history.
5. Move from Compose to k3s/Kubernetes beyond ~2 nodes; Prometheus + Grafana + Loki + alerting,
   with a native Prometheus `/metrics` endpoint replacing the JSON `/v1/metrics`.

## To verify on the first run

- Exact `cargo chef cook` flags (`-p`, `--bin`, `--features`) with the pinned cargo-chef version.
- `next start` with a read-only root filesystem (tmpfs on `/web/.next/cache` and `/tmp`).
- Postgres with `cap_drop: ALL` plus the re-added capabilities.
- Caddy `{$OA_DOMAIN}` certificate issuance and the security headers against the UI (CSP).
