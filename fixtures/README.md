# Test fixtures

- `recorded-<date>/` — real responses from the venues' **public** market-data endpoints, recorded
  once and replayed by the parser tests and the offline `FIXTURE` engine.
- `*_signing_vectors.json`, `*_vector.json` — reference signatures produced with the venues' own
  libraries (`eth_account`, the Hyperliquid Python SDK) so our encoders are tested against an
  independent implementation. Every key used is a public dummy key that never holds funds:
  `keccak("cow")` (Aevo, Derive — `scripts/gen_*_vectors.py`), `0x15…15` (Hyperliquid),
  `0x17…17` (Limitless).
