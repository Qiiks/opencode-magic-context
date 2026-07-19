# Build: Pi /clone state inheritance (issue #225 durable fix)

Implement the design at `.alfonso/plans/pi-clone-inheritance-v1.md` (v2, Oracle-reviewed — the "Oracle-resolved specifics" section is authoritative and supersedes anything below it that conflicts). Repo: ~/Work/Projects/CortexKit/magic-context, branch subc-migration.

## Shape

New module `packages/pi-plugin/src/clone-inheritance.ts` + wiring in `packages/pi-plugin/src/index.ts`'s `session_start` handler (gate strictly on `reason === "fork"` && `previousSessionFile` present; the whole handler is try/catch fail-open with a single structured log line on failure: source id, dest id, stage, and the user remedy "run /ctx-wrapup to rebuild, or re-clone").

Flow: read source session id from previousSessionFile's JSONL header → resolve clone branch entries via ctx.sessionManager.getBranch() → build the clone RawMessage.id→ordinal map with convertEntriesToRawMessages → single BEGIN IMMEDIATE transaction: destination-empty guard INSIDE it (no compartments AND no tags rows for the clone session; skip+log if any), then copy per the design's items 1-9 → after commit, if a pending marker was migrated call signalPiDeferredCompactionMarkerDrain().

Every numbered item in "Oracle-resolved specifics" (1-11) is a requirement. Item 11 is the minimum test list — implement every listed test; they go in packages/pi-plugin/src/clone-inheritance.test.ts using the existing test DB helpers (never the live DB; MAGIC_CONTEXT_TEST_DATA_DIR discipline as in sibling tests).

## Storage helpers

Add what you need in packages/plugin/src/features/magic-context/ storage modules (e.g. a copySessionStateForClone(db, sourceId, destId, filter) or granular copy helpers) — keep them harness-neutral in shared storage, with the Pi-specific filtering/validation living in the pi-plugin module. SQLite binds: SPREAD positional args, never array-form. Use runLeaseGuardedWrite-style BEGIN IMMEDIATE discipline (see packages/plugin/src/features/magic-context/dreamer/lease.ts for the shared helper) for the transaction.

## Cache discipline (the part that gets review scrutiny)

- Do NOT migrate any cached_m0_*/cached_m1_*/memory-block-cache session_meta fields. The clone's first transform pass must take mustMaterializePi() === first_render and render m0 from the migrated compartments. Add the test proving it (item 11 last bullet).
- Copied tags must replay byte-identically: source_contents rows migrate with them (item 2), statuses/drop_mode/accounting verbatim, tag numbers verbatim (row autoincrement ids re-mint naturally on insert).

## Documentation

- PARITY.md: add the divergence entry — Pi has clone/fork state inheritance; OpenCode /fork does not (OpenCode re-mints message ids on fork, entry-id-keyed migration impossible there; its fix is a separate future design).
- One line in the plugin CHANGELOG-worthy commit message; no user-facing docs changes in this task (docs ride the release).

## Gates

- cd packages/pi-plugin && bun test — green (including your new suite).
- cd packages/plugin && bun test — green (shared storage helpers must not disturb OpenCode paths).
- bun run typecheck both packages; lint clean.
- check_comments clean — comments explain invariants (why the guard is inside the transaction, why counter = max copied, why todo fields are all-or-nothing), never reference reviews/plans/issue numbers except a single top-of-module pointer comment is fine WITHOUT process jargon.
- Commit in logical units (shared storage helpers → pi module → wiring → tests is fine as 1-2 commits; do not squash into one blob with unrelated formatting).
