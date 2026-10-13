# Comparability rules

Code: `crates/core/src/comparability.rs`.

| Verdict | When |
|---|---|
| NOT_COMPARABLE | family, underlying, call/put, strike or expiry instant differ |
| INSUFFICIENT_INFORMATION | one of those core terms is unknown |
| ECONOMICALLY_SIMILAR_WITH_DIFFERENCES | core terms equal; exercise style, settlement mechanism/asset/index/window or premium currency differ or are unverified |
| STRICTLY_COMPARABLE | everything known and equal (multiplier differences are normalised away) |

Cross-venue ranking/routing requires every reason code of a SIMILAR pair to be **explicitly
accepted** (`accepted_differences`); core codes can never be accepted (422).

Derive ↔ Aevo today: always SIMILAR (`SETTLEMENT_INDEX_DIFFERS`, `SETTLEMENT_WINDOW_UNKNOWN`) — 39/39
and 40/40 live pairs measured on 2026-10-05.

## What changed in the "freshness" iteration — no rule was relaxed
- **Common fillable quantity**: when a comparable venue only lacks depth, the response proposes the
  largest quantity every such venue covers (strictest step, above minimums). The user must re-run
  with it; the requested quantity is never changed silently. Not offered when another reason
  (e.g. differences not accepted) blocks the venue.
- **Side-by-side terms** table in the UI highlighting each differing term.
- **Acceptance memory** (opt-in, browser tab only): boxes are pre-checked visibly with a badge;
  nothing applies until the user clicks "Recomparer"; "Oublier" revokes.
- Every comparison and simulation records the accepted codes (`notes`, `accepted_differences`).
