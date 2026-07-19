# Mechanical fixes: multi-process DB storm fallout (3 items)

Repo: this worktree (branch from HEAD of `subc-migration`). All three items are independent of each other; land as one branch, separate commits.

## Background (evidence, 2026-07-17 12:02-12:11 UTC, magic-context.log)

A second OpenCode process (TUI) booted in `~/Work/Projects/CortexKit/benchmarks` while the long-running serve process was active and a 30K-row clone transaction ran against context.db. Result: a SQLITE_BUSY storm past the 5s busy_timeout. Observed in the log:

- `transform tag persistence failed; continuing without tagging: database is locked` (multiple sessions)
- `[migrations] FAILED v1: acquire migration write lock — database is locked` followed by `storage fatal` and `hook failed to open storage; disabling feature` — the TUI process disabled MC entirely at boot.
- Simultaneous per-project background work from the fresh process: git-commit sweeps, smart-note sweeps, backfills for ~28 projects immediately at hook init, while the serve ran the same lanes.

## Item 1 — boot quiet period for background lanes (plugin)

Problem: a freshly booted process fires all background maintenance immediately at hook init for every registered project (git sweeps, smart-note sweeps, session-project backfill, message-index reconcile). When another MC process is already live on the same DB, this creates an avoidable write storm exactly at the moment the new process is also running its own migrations/boot reads.

Fix, in `packages/plugin/src/plugin/dream-timer.ts` (and any other boot-time kick sites found by tracing hook init):

- Add a boot quiet period: background lanes (dream scheduler tick, git sweeps, smart-note sweeps, backfill, message-index reconcile kicks) do not start until `BOOT_QUIET_MS` after plugin construction. Default 120_000 (2 min), config override NOT added (no new knob; constant with a doc comment).
- Stagger: after the quiet period, per-project first-run work must be jittered (existing patterns may already jitter; verify and extend to cover the first tick) so 28 projects do not fire within the same second.
- The TRANSFORM hot path is untouched. Channel nudges, tagging, marker drains are NOT background lanes and must not be delayed.
- Tests: fake-timer test proving no background lane runs before the quiet period elapses, and that the transform path is not gated.

## Item 2 — migration-lock boot must not insta-disable (plugin)

Problem: `storage-db.ts` boot path treats `database is locked` during the migration write-lock acquisition as fatal, disabling MC for the process lifetime. A concurrent process holding the write lock for a few seconds (giant transaction, checkpoint) should not permanently kill a fresh process's MC.

Fix, in `packages/plugin/src/features/magic-context/storage-db.ts` (+ `migrations.ts` if the lock acquisition lives there):

- On `SQLITE_BUSY`/`database is locked` during the migration lock/check phase ONLY: bounded retry — up to 5 attempts with backoff (1s, 2s, 4s, 8s, 15s; total ~30s), each attempt logged at info level.
- If all retries exhaust: keep today's fail-closed disable (correct: schema state unknown).
- Genuine migration FAILURES (SQL errors, fence violations) keep today's behavior — no retry.
- The retry must be async (no event-loop blocking sleeps).
- Tests: lock held by a second connection released after N seconds → boot succeeds on retry; lock held past the budget → disables as today; non-lock migration error → no retry.

## Item 3 — dashboard cause misclassification on transform-failure passes (Rust, dashboard)

Problem: when the plugin's transform fails open (returns the raw array), the dashboard cache page labels the resulting bust `Cause: Provider-side (not Magic Context)` because no transform_decisions row exists for that assistant message. Pointing AWAY from MC during an MC failure is worse than no label.

Fix, in `packages/dashboard/src-tauri/src/db.rs` (cause attribution query/logic; frontend label map in the cache components):

- Distinguish "no decision row + prompt size >> last known compacted size" from genuine provider-side causes. Concretely: when no transform_decisions row matches the bust message AND the session has transform_decisions rows for earlier messages (MC was active) — classify as `mc_transform_missing` with label "Magic Context transform did not run (fail-open pass)". Keep true provider-side classification only when MC decisions exist for the message.
- Frontend: render the new cause with the same severity styling as MC-caused busts (it IS ours), tooltip explaining fail-open.
- Tests: Rust unit test for the attribution branch (decision rows present for older messages, absent for bust message → mc_transform_missing).

## Gates

- `bun test` in packages/plugin (scoped suites for storage-db, dream-timer).
- `cargo test` in packages/dashboard/src-tauri + `bun run build` frontend.
- Comment hygiene: comments explain invariants, never reference this incident/plan; no em-dashes.

## Report

Per item: files touched, test names, and for item 1 the exact list of boot-time kick sites found and gated.
