# Contributing

Thanks for helping. Keep changes small, tested and focused. Code map, commands and rules (also
for AI coding agents): [AGENTS.md](AGENTS.md). How each mechanism works: [docs/diagrams.md](docs/diagrams.md).

## Setup

See [README › Quick start](README.md#quick-start). The default test suite needs no network, no
database and no keys:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features testnet -- -D warnings
cargo clippy --workspace --all-targets --features mainnet-nc -- -D warnings
cargo test --workspace && cargo test -p oa-app --features mainnet-nc
cd web && pnpm lint && pnpm exec tsc --noEmit
```

CI runs the same checks plus `cargo deny`, the SDK tests and a secret scan.

## Rules

- **`crates/core` stays pure**: no I/O, no async runtime, no HTTP.
- **Unknown is not zero.** A fee, a cost or a contract term that is not known must surface as
  unknown and exclude the venue from routing — never default to `0`.
- **Decimals never go through floats** (Rust `Decimal`, strings on the wire).
- **Every venue parser change comes with a recorded fixture** (`fixtures/recorded-*`) and a test.
  Signing changes need a reference vector produced with the venue's own library.
- **Migrations are forward-only and non-destructive**: never `DROP` or `TRUNCATE`, never edit an
  applied migration; add a new numbered file.
- **No secrets in code, fixtures, logs or tests.** Tests use public test keys (e.g. `keccak("cow")`)
  or throwaway keys generated at runtime.
- Anything touching execution (`execution.rs`, `*_user.rs`, `*_exec.rs`, risk limits) needs a
  test that proves the refusal path, not only the happy path.

## Adding a venue

1. `crates/adapters/src/<venue>.rs`: implement `Connector` (catalog, book, fees). Map native
   instruments to `InstrumentKey` with every contract term you can source; leave unknown terms
   unknown.
2. Record real public responses under `fixtures/recorded-<date>/` and test the parser against them.
3. Add the venue to `crates/adapters/src/registry.rs` (with official sources) and attach it in
   `crates/app/src/bin/server.rs`.
4. Optional execution: `<venue>_user.rs` (non-custodial: build the payload, verify the user's
   signature, forward) behind an `OA_<VENUE>_NONCUSTODIAL` switch.

## Commits and pull requests

One logical change per PR, with a description of what changed, why, and how it was tested.
By contributing you agree that your contribution is licensed under the MIT license.
