## What and why

## How it was tested

- [ ] `cargo fmt`, `cargo clippy` (default, `--features testnet`, `--features mainnet-nc`), `cargo test`
- [ ] Web: `pnpm lint`, `tsc --noEmit` (if `web/` changed)
- [ ] New venue parsing / signing comes with a recorded fixture or a reference vector
- [ ] Execution changes include a test of the refusal path
