# Parity Hunt #18 — deletion replay across a config flip

Date: 2026-09-01  
Base: `4288973af2ce94ea708f757d3a38fe3272e2a787`  
Standing protocol: `#15150` (incoming consecutive-CLEAN counter `0/3`)

## Verdict and fence

**FINDINGS — NOT CLEAN.** Hunt 18 found and fixed one independently observable lifecycle defect, **H18-01 RUST-DELETE-FLIP-REPLAY**. The consecutive-CLEAN counter remains **0/3**.

TypeScript remains canonical. No serializer, schema, migration, provider, authority, deployment, or project fence moved. Live SQLite access was read-only through the standing audit. This report contains counts, fixed vocabulary, byte counts, and short hash prefixes only; it contains no session content, full session identifiers, project paths, RPC credentials, or database rows.

## H18-01 RUST-DELETE-FLIP-REPLAY — fixed

### Conviction

Hunt 17 made failed Rust deletion durable as an `opencode:rust` cleanup marker and supplied a lazy transport that survives a later flip to TypeScript. A duplicate `session.deleted` delivery after that flip defeated both protections:

1. `markSessionCleanupPending(..., false)` rewrote the existing `opencode:rust` marker to ordinary `opencode` through its upsert;
2. the TypeScript-mode handler then had no active Rust transform client, treated its no-op in-process cleanup as success, and called host `clearSession`;
3. `clearSession` removed both `pending_session_cleanup` and `session_projects`, leaving no durable coordinate for the lazy transport to delete module-owned tags, cache state, overlays, producer ledgers, or notes.

This is independently observable durable-state and session-lifecycle behavior. It is not a tooling gap: a retried lifecycle event could acknowledge deletion to the host while retaining the deleted session in the Rust module indefinitely.

### Executed red-first evidence

The lifecycle regression first creates host tags and a project binding, makes Rust module deletion fail, verifies the `opencode:rust` marker, then replays the same deletion through a TypeScript-mode handler before project-scoped retry. Against the pre-fix implementation it failed at `packages/plugin/src/hooks/magic-context/event-handler.test.ts:1010`:

```text
expect(received).toEqual(expected)
Expected: { "harness": "opencode:rust" }
Received: null
```

The host tag assertion immediately after that line would also have observed premature deletion. The fixed test proceeds through the existing project-scoped retry and proves that successful module acknowledgement removes the module coordinate, marker, and host tag together.

### Fix

Cleanup-marker classification is now monotonic: once a session requires Rust module cleanup, a later host-only upsert cannot downgrade it. `markSessionCleanupPending` returns the effective durable classification. When a TypeScript-mode handler sees an older Rust marker, it performs in-process cache cleanup but leaves durable host state for the lazy, project-scoped Rust retry. A current Rust handler still awaits module deletion and clears host state only after acknowledgement; a TypeScript-only deletion retains its original immediate semantics.

This preserves the H17 transport design without sharing or reviving a deleted session's live transform route. Both initial and timer-driven module calls close only that session's route. The shared session-table registry still removes `session_projects` and `pending_session_cleanup` in the same host transaction after acknowledgement.

### Non-vacuity mutation

After the fix, a temporary branch containing the exact token `NON-VACUITY BREAK` forced the handler to consume every marker as host-only. The focused suite went red at the same restored assertion, `event-handler.test.ts:1010`, with expected `opencode:rust` and received `null`. The mutation was restored and the suite passed 34/34. No mutation token remains.

## Audit of the rest of the H17 fix

The awaited initial deletion propagates module failure while closing its route; ordinary maintenance excludes Rust-class markers; project retry joins the marker to the matching harness/project binding; every retry closes its temporary route; and successful acknowledgement runs the existing all-session-table transaction. The lazy cleanup transport is registered while the plugin is enabled even when current `transform_mode` is TypeScript. Focused lifecycle coverage, the full plugin suite, and the complete Rust suite found no additional deletion, cache-route, or marker-table regression.

## Newest live corpus

The privacy-preserving live differ read both Darwin capture roots for the Aug 31 and Sep 1 slices at the Hunt-17 lower bound, `2026-08-31T07-31-31-335Z`, with RPC and the Rust oracle enabled.

| Corpus | Rust Anthropic | TS Anthropic | TS Responses | Unverified | Total |
|---|---:|---:|---:|---:|---:|
| Both date slices | 256 | 201 | 282 | 6,296 | 7,035 |

The coordinate join admitted 739 bodies with zero ambiguous capture hashes. The aggregate source-contract unexplained bucket was zero, as were decision-window, engine-truth, maintenance, operator-read, historian-producer, mural, wrapup, and Hunt-12 unexplained invariants. The older H13 Responses observation (`sha256=4506f17b…`, 26,464 bytes) remained in the TS lane. Fourteen newer Responses empty-content observations remained lane-unverified inventory with no adjacency violation; they are not same-input evidence and were not promoted into findings.

## Post-taxonomy scheduler vocabulary

The live decision window after the same cutoff contained two durable `scheduler_history` rows: one `defer`/`Defer` and one `execute`/`Execute`, both with canonical `none` defer reason. The corresponding live read contained 48 Rust transform decisions and about 3.5k TypeScript decisions. Rust's transport/lifecycle statuses (`error` and `parked`) remain intentionally additional operator outcomes, while the shared scheduler outcomes present in both stores were exactly `defer` and `execute`. The live audit reported zero vocabulary invariant. The two durable historian no-fire rows were still explicitly classified as legacy generic `trigger_false`; no post-taxonomy row was relabeled without evidence.

## Verification

- `bun install --frozen-lockfile` — passed; manifests and lockfile unchanged.
- Red-first focused lifecycle regression — failed at `event-handler.test.ts:1010` with the Rust marker missing, then passed after the fix.
- `bun run typecheck` — passed.
- Full plugin suite, `bun test --parallel --timeout 30000` — 4,262 passed, 0 failed.
- `cargo test --workspace` — passed: 1,171 tests passed, 4 ignored, 0 failed across unit, binary, integration, and doc-test legs.
- `python3 scripts/audit-transform-wire-parity.test.py` — 9 passed.
- Two newest-window live audits — completed read-only; 7,035 bodies, 739 admitted, zero aggregate unexplained invariants.
- Restored lifecycle suite after the `NON-VACUITY BREAK` mutation — 34 passed, 0 failed.
- `git diff --check` — passed.

CLEAN-OR-FINDINGS verdict: FINDINGS
