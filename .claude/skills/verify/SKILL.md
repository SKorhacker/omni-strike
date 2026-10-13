---
name: verify
description: Run every check Omni Strike's CI runs (Rust fmt, clippy and tests on the three builds, web lint, types and build, SDK tests) and report the results precisely. Use before declaring any code change done, or when asked to verify, test or check the repository.
---

# Verify a change

Run what applies to the files you touched; for a cross-cutting change run everything. Report each
command as pass or fail with the first error. Never write "should work".

## Rust (always if `crates/` changed)

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features testnet -- -D warnings
cargo clippy --workspace --all-targets --features mainnet-nc -- -D warnings
cargo test --workspace
cargo test -p oa-app --features testnet
cargo test -p oa-app --features mainnet-nc
```

Changed `store.rs` or a migration and a Postgres is available: run the store tests on a
throwaway database (migrations are non-destructive):
`OA_TEST_DATABASE_URL=postgres://user:pw@127.0.0.1:5432/scratch_db cargo test -p oa-app pg_tests`.

Changed a connector that has `live-tests` code:
`cargo clippy --workspace --all-targets --features oa-adapters/live-tests,oa-app/live-tests -- -D warnings`.

## Web (if `web/` changed)

```sh
cd web
pnpm install --frozen-lockfile
pnpm exec next typegen
pnpm exec tsc --noEmit
pnpm lint
pnpm build
```

## SDKs (if `sdk/` or an endpoint changed)

```sh
cd sdk/typescript && npx -p typescript tsc -p . && npm test && rm -rf dist
cd sdk/python && python -m pytest -q
python sdk/tools/check_openapi.py http://127.0.0.1:8080   # needs a running server
```

## Before finishing

- `git status`: no `.env` file other than `*.example`, no build output, no `dist/`.
- The diagrams in `docs/diagrams*.md` still match any flow you changed.
- API change: `openapi.rs`, both SDKs and `sdk/tools/check_openapi.py` updated.
