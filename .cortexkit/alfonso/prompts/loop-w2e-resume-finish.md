# Resume + finish: W2-E background-lanes fixes from WIP snapshot

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. A previous mason implementing this task froze mid-flight (infrastructure, not correctness); its work was checkpoint-committed from outside as a WIP snapshot on branch **wip/w2e-background-lanes** (commit 5b448fcd, 12 files: message-index.ts + message-index-async.ts + session-project-backfill.ts + project-embedding-registry.ts + read-session-raw.ts/chunk.ts + index.ts + tests). The snapshot is MID-IMPLEMENTATION: not gate-verified, possibly incomplete, possibly containing half-written logic. Your job: adopt it, audit it against the original brief below, finish what's missing, fix what's wrong, gate it, commit clean.

STEP 1: `git merge wip/w2e-background-lanes` into your worktree branch (it's based on the same subc-migration lineage; resolve trivially if needed). Review EVERY file of the snapshot as if reviewing a stranger's PR — do not assume any of it is finished or correct. The previous worker got no chance to run tests.

STEP 2: audit + finish against the ORIGINAL BRIEF (verbatim below, between the markers). Every fix it demands must be present and correct; every test it demands must exist and pass. Where the snapshot diverges from the brief, the brief wins unless the snapshot's approach is demonstrably better (justify in report).

STEP 3: gates. cd packages/plugin && bun test (full suite), typecheck, lint, check_comments. Squash-or-keep the WIP commit as you see fit but the final history must be clean commits with honest messages (no "wip").

=== ORIGINAL BRIEF START ===
(see .cortexkit/alfonso/prompts/loop-w2e-bg-lanes-fix.md in the repo — read it in full; it specifies: Fix 1 HIGH FTS dirty-floor contiguity — watermark never advances past an uncovered ordinal, floor preserved unless demonstrably covered, failed-mark recovery, two-connection tests; Fix 2 HIGH bounded/yielding background work — FTS reconciler paged with time/row budget + yields between transactions, backfill pages discovery + yields by elapsed-time/rows not new-directories + lease renewal across yields + bounded blocking git calls, tests proving the yield; design-review mediums — backfill holder-id fencing, embedding drain abort on failed renewal + backlog lease renewal, batched GC, capped session chunk repair. And the constraint: no cache-core files, no materialization-trigger changes.)
=== ORIGINAL BRIEF END ===

Report: per-fix status (was-in-snapshot-correct / was-in-snapshot-fixed-by-you / added-by-you), gate evidence, and anything from the brief you deliberately did not do with why.
