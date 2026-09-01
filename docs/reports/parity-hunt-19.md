# Parity Hunt #19 — drive the undriven directions, then free-roam

Date: 2026-09-01  
Base: `22464bf25db24c4037f5efda72c8bb02d64baf51`  
Standing protocol: `#15150` (incoming consecutive-CLEAN counter `0/3`)

## Verdict and fence

**FINDINGS — NOT CLEAN.** Hunt 19 exhausted the requested deletion matrix with executed tests and convicted two independently observable lifecycle defects: **H19-01 RUST-DELETE-ORPHAN-SWEEP** and **H19-02 RUST-DELETE-INFLIGHT-REPOPULATION**. Both are fixed. The consecutive-CLEAN counter remains **0/3**.

TypeScript remains canonical. No serializer, schema, migration, provider, authority, deployment, or project fence moved. Live SQLite access was read-only through the standing audit. This report contains only counts, fixed vocabulary, byte counts, and short hash prefixes; it contains no session content, full session identifiers, project paths, RPC credentials, or database rows.

## Deletion direction matrix

Every row below was driven through an executed test. “PASS” means the pre-existing contract survived that direction; it is not a finding.

| Cell | Driven interleaving | Executable evidence | Result |
|---|---|---|---|
| DF-TS-R-TS | An existing failed Rust marker is delivered in TS mode, delivered again in Rust mode with module failure, then replayed in TS mode | `event-handler.test.ts` — `preserves a failed Rust cleanup across TS → Rust → TS double flips` | PASS |
| DF-R-TS-R | Rust deletion fails, the event replays in TS mode, then Rust deletion fails again before project retry succeeds | `event-handler.test.ts` — `preserves a failed Rust cleanup across Rust → TS → Rust double flips` | PASS |
| CON-DUP | Two `session.deleted` deliveries enter the same handler, both stop at deferred `onSessionDeleted` promises, one module deletion fails and the other succeeds | `event-handler.test.ts` — `serializes the durable outcome of two concurrent session.deleted deliveries` | PASS |
| CON-TIMER | Project-scoped Rust retry is in flight while a TS-mode duplicate delete is delivered | `event-handler.test.ts` — `keeps host rows while a delete races the project-scoped Rust retry` | PASS |
| CON-ROUTE-LANE | A real `SubcModuleTransport` transform occupies the per-session correctness lane while `session.delete` is submitted | `module-transport.test.ts` — `queues session.delete behind an in-flight transform on the same route` | PASS |
| CON-TRANSFORM-CACHE | Rust `clearSession` begins while a transform is in flight; the transform finishes before queued module deletion acknowledges | `rust-mode-transform.test.ts` — `does not let an in-flight transform repopulate a deleted session route` | **H19-02** |
| MARK-EVENT | Duplicate event delivery after a mode flip cannot downgrade `opencode:rust` | H18 fixture retained; both double-flip tests above extend it through the second transition | PASS |
| MARK-HOST-RETRY | Ordinary host cleanup excludes Rust-class markers | Existing H17/H18 event-handler fixture rerun in the 144-test focused lifecycle batch | PASS |
| MARK-ORPHAN | OpenCode orphan sweep runs while a Rust marker and its project coordinate are pending | `message-index-maintenance.test.ts` — `preserves every host row and coordinate for an orphan with pending Rust cleanup` | **H19-01** |
| MARK-MIGRATION | Direct `migration_pending` rollback executes while the source session has a Rust marker and project coordinate | `migrate.test.ts` — `leaves a pending Rust cleanup marker and project coordinate untouched` | PASS |
| MARK-DOCTOR | Unified doctor’s migration-recovery mutator is the same `sweepPendingMigrations` function driven by MARK-MIGRATION; doctor adds no session-table deletion after it | The MARK-MIGRATION test plus CLI typecheck and the complete focused migration suite | PASS |
| RESTART-COLD | Marker is written before timer registration and no marker-specific in-memory state exists; the cold-boot startup callback performs project-scoped retry | `dream-timer.test.ts` — `picks up a durable Rust deletion from the cold-boot startup tick` | PASS |

The doctor and direct migration rows are listed separately because they are distinct mandated entry directions. They converge before mutation: unified doctor invokes `sweepPendingMigrations` and performs no later session-row cleanup. The executable fixture drives that exact mutator with both marker and coordinate present.

## H19-01 RUST-DELETE-ORPHAN-SWEEP — fixed

### Conviction

The orphan sweep selected an old OpenCode session missing from `opencode.db` and passed it to the shared session-table deleter with harness `opencode`. Harness-scoped host rows, including `session_projects`, were removed. The Rust marker itself had harness `opencode:rust`, so it survived the harness-filtered delete. The resulting state looked durable but was unreachable: project-scoped retry joins the marker to `session_projects`, and that coordinate had already been erased.

The red-first test failed at `packages/plugin/src/features/magic-context/message-index-maintenance.test.ts:149`: expected `deleted: 0`, received `deleted: 1`. Assertions immediately after that line pin every host row and the retry coordinate.

### Fix

The shared session-table deletion boundary now protects a session carrying the matching Rust marker unless its caller explicitly states that module deletion has acknowledged. Protection applies to the whole session, including unscoped tables and the session→project coordinate. The orphan sweep reports the number actually deleted rather than the number initially selected. Only the successful Rust event path and successful project-scoped retry set the acknowledgement capability.

This makes marker classification monotonic at the shared deletion boundary, not only in the event handler fixed by Hunt 18. No schema change was required.

## H19-02 RUST-DELETE-INFLIGHT-REPOPULATION — fixed

### Conviction

The module transport correctly serializes `session.delete` behind an in-flight transform on the same per-session correctness lane. Rust `clearSession`, however, cleared local state only before awaiting that queued deletion. While deletion waited, the transform completed and repopulated the session’s wire cache. After deletion acknowledged, `clearSession` closed the route but left that cache alive. A later pass for the same id emitted a stale tail delta instead of a full wire.

The red-first test failed at `packages/plugin/src/hooks/magic-context/rust-mode-transform.test.ts:2098`: expected no `tail_delta`, received a delta carrying the deleted route’s prior fingerprint.

### Fix

Rust `clearSession` now clears local route state both before module deletion and again in `finally`, immediately before closing the route. The first clear prevents new local consumers from adopting the session; the second removes state an already-running transform could publish while module deletion waited in the correctness lane. Failure propagation and route close semantics remain unchanged.

## Non-vacuity mutations

Every temporary mutation contained the exact token `NON-VACUITY BREAK` and was restored before final verification:

1. Disabling the Rust-marker guard in the shared session-table deleter made MARK-ORPHAN fail again at `message-index-maintenance.test.ts:149`, expected `deleted: 0`, received `deleted: 1`.
2. Disabling the post-await local-state clear made CON-TRANSFORM-CACHE fail again at `rust-mode-transform.test.ts:2098`, where the next pass received a stale `tail_delta`.

No mutation token remains.

## Free-roam

### Newest live corpus and replay arms

The read-only live audit used the standing Hunt-17 lower bound, `2026-08-31T07-31-31-335Z`, against both Darwin capture roots. The final Responses-root read contained **3,887 bodies**: 1,047 Anthropic and 2,840 OpenAI Responses. Privacy-safe coordinates admitted **312** bodies: 209 Rust Anthropic, 73 TypeScript Anthropic, and 30 TypeScript Responses; 3,575 remained unverified inventory. There were zero ambiguous capture hashes and the aggregate source-contract unexplained bucket was zero.

The Responses inventory grew by five bodies between the two read-only probes; the report uses the final count. The unlike-session Anthropic empty-text and signed-reasoning position spaces remain observations. The Responses empty-content inventory remained lane-unverified and had no adjacency violation. Prior adjudications were not reopened.

All three same-input replay arms ran. Real Anthropic retained three provider-native signed-thinking position differences already adjudicated as intentional and had zero unadjudicated divergences. Mock Anthropic and OpenAI Responses each had zero divergence.

### Outside the previous report’s frame

The transport correctness lane was driven directly with `session.delete`, rather than inferred from its generic ordering property. Direct migration recovery was run with a simultaneous Rust cleanup marker. The Pi lifecycle counterpart remains intentionally in-process on reversible session switch/shutdown; the full plugin and Rust suites found no additional deletion, transport, codec, scheduler, or store defect.

## Verification

- Frozen workspace install — passed; manifests and lockfile unchanged.
- Red-first deletion matrix — two failures recorded at the lines above; all other cells passed.
- Restored focused deletion matrix — 144 passed, 0 failed; direct transport lane cell passed; migration recovery suite 31 passed.
- Plugin typecheck — passed.
- CLI typecheck — passed.
- Full plugin suite — 4,269 passed, 0 failed.
- Complete Rust workspace suite — 1,171 passed, 4 ignored, 0 failed.
- Differ suite — 9 passed.
- Three same-input replay arms — zero unadjudicated divergences.
- Two newest-window live audits — completed read-only; final 3,887 bodies, 312 admitted, zero aggregate unexplained invariants.

CLEAN-OR-FINDINGS verdict: FINDINGS
