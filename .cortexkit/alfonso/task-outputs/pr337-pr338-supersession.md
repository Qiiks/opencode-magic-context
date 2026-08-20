# PR #337 / #338 supersession comparison

**Scope:** read-only comparison only. No merge, comment, push, or other GitHub mutation was performed.

**Method:** read the complete patch series and metadata with `gh pr diff 337`, `gh pr view 337`, `gh pr diff 338`, and `gh pr view 338`; refreshed both check lists with `gh pr checks`; compared them to the merged master fixes below and to open issue [#312](https://github.com/cortexkit/magic-context/issues/312).

| PR | Filed by Cole Leavitt | Relevant master work merged independently | Check snapshot |
| --- | --- | --- | --- |
| [#337](https://github.com/cortexkit/magic-context/pull/337) `fix(historian): defer child session cleanup` | 2026-08-19 05:46 UTC | `edd6e06e8ff762a3133c25c3743d95b6e9ead465` — `fix(#336): historian children ride the age-gated orphan sweep...` at 20:30 UTC | Open; all listed CI, smoke, E2E, Greptile, Socket, and Cubic checks pass; only `[code]smith` is skipped. |
| [#338](https://github.com/cortexkit/magic-context/pull/338) `test: remove shared-DB and timer flakiness` | 2026-08-19 07:12 UTC | `16b91c876fdf1476c56b94b955f75f880d7fabda` / `5b7238c993754e6b34861992fd35d56a62eb3498` — `#330/#333` at 20:33–20:34 UTC | Open; all listed CI, smoke, E2E, Greptile, Socket, and Cubic checks pass; only `[code]smith` is skipped. |

Cole filed both first. The master changes were independently developed and merged later that night; they should not be represented as an earlier solution.

## PR #337 vs. merged #336

### What #337 actually does

* Changes the historian default from five to ten minutes in the schema, generated schema asset, user docs, and configuration test; exports `MAX_HISTORIAN_PROMPT_ATTEMPTS = 3`.
* Removes the OpenCode historian runner's success-path `session.delete`. Completed and failed `magic-context-compartment` children remain for later cleanup.
* Adds historian title matching to the orphan sweep and a stale budget of `timeout × 3 outer attempts × 2 primary/suggestion calls × (primary + fallback count)`, floored at one hour.
* Registers historian timeout/fallback/`keep_subagents` with the timer, runs child retirement when Dreamer is disabled, and, in its final commit, separates the historian and Dreamer/privacy sweeps so a long historian budget does not delay privacy cleanup. It also uses `resolveFallbackChain(...).length` rather than raw configuration length.
* Adds `empty-task-output.ts`, invoked from the tool-execute-after hook, to annotate an otherwise empty completed native `<task_result>` with a diagnostic sentinel. This is a diagnostic for the #270 symptom, not part of the #336 FK race repair.
* Adds focused static/unit coverage for those paths.

### Comparison

**The central design is the same, but merged #336 is stronger on the cleanup risk.** #337 uses no completion/quiescence marker and no separate post-completion grace delay: its only guard is the age-gated sweep. Master likewise does not pretend that prompt completion or `session.idle` is a writer barrier, but makes the age budget explicitly include detached-writer grace. Master additionally:

1. reproduces the actual FK failure: after historian publication it successfully inserts a late child `step-start` part, and proves no inline delete or `session.status` probe occurred (`compartment-runner-session-cleanup.test.ts`);
2. adds the 11-second maximum retry-backoff allowance **and** a 15-minute detached-writer grace to the same retry/suggestion/fallback budget. #337's formula has neither allowance;
3. performs the historian sweep before the Dreamer-enabled early return, and makes maintenance eligible when `historianChildSweep` is registered. Therefore it covers the Dreamer-disabled gap explicitly;
4. separates the historian title from the existing privacy-sensitive namespaces: retrospective, user memories, curate, maintain-docs, refresh-primers, and smart-note compile/confirm prefixes. Each class receives its own age budget rather than a shared maximum;
5. honors `keep_subagents` by suppressing only historian retirement, while preserving the privacy orphan backstop; and
6. carries the ten-minute default through the OpenCode schema/docs **and** Pi's historian default. #337 contains no Pi implementation change. The specific FK race is an OpenCode `session`/`part` database race; Pi's subprocess model does not have that child-session deletion path.

The current raw fallback count is deliberately conservative for blank, duplicate, or malformed entries: it can retain a child longer than necessary, never shorter. #337's `resolveFallbackChain(...).length` is a more exact count and is worth adopting as a small cleanup refinement, not a correctness prerequisite.

### Verdict: **COMPLEMENTARY**

The #336 cleanup fix supersedes #337's main arm and covers it more completely. Do not merge the branch as a competing repair. Extract these two self-contained follow-ups with credit:

1. `empty-task-output.ts` plus its `createToolExecuteAfterHook` registration/tests, to make a completed native Task with an empty `<task_result>` visible to the caller (#270 diagnostics).
2. The timer-registration refinement from raw fallback count to `resolveFallbackChain(pluginConfig.historian?.fallback_models).length`, with its normalization test. It narrows an otherwise safe over-retention window.

The ten-minute default, separate privacy/historian sweeps, Dreamer-disabled coverage, and `keep_subagents` behavior are already present on master; no extraction is needed for them.

### Draft public comment for #337

> Thank you, Cole — you diagnosed the important part of #336 early: prompt completion/idle is not a barrier for OpenCode's detached child writes, so deleting the historian child inline can race the FK-backed `part` insert. You filed this before we landed our independent work, and the overlap is real.
>
> In parallel we merged `edd6e06e8` for #336. It keeps the child and retires it through an age-gated sweep, but also adds the late-`part` FK regression reproduction, retry-backoff plus detached-writer grace, separate privacy/historian windows, the Dreamer-disabled maintenance path, and historian-only `keep_subagents` retention. It also carries the 10-minute historian default through the Pi path.
>
> We do not want to merge two overlapping implementations of that repair. We will close this with credit rather than merge the branch, and lift the two non-overlapping pieces into focused follow-ups: the empty completed native-Task diagnostic for #270, and normalized fallback-chain counting for the historian retention budget. Thanks again for getting the underlying lifecycle race on the board first.

## PR #338 vs. merged #330 + #333 and issue #312

### What #338 changes, by file

| File | PR change | Relationship to master / #312 |
| --- | --- | --- |
| `packages/plugin/package.json` | Changes the final test command from `bun test` to `bun test --timeout 30000`. (Its first commit briefly used `--isolate`; the final PR does not.) | Separates Bun's 5s test kill from SQLite's production 5s `busy_timeout`, so a lock can report `SQLITE_BUSY` first. Useful #312 observability, **not** contention removal. |
| `message-index-async.ts` | Replaces the scheduled-session `Set` with a `Map<string, Promise<void>>`; `scheduleReconciliation` and `scheduleClearAndReindex` return completion promises. | Overlaps #333's boot-quiet race test, but master fixes it by waiting for the observable state (`reads === 2 && isSessionReconciled`) rather than changing production scheduling API. No extraction recommended. |
| `message-index-async.test.ts` | Uses fake timers, awaits the new promises, and restores real timers. | Alternative test control for the same #333 case. The master state-wait is a direct behavioral assertion and has mutation evidence; do not layer both approaches without a separate API need. |
| `storage-embedding-measurements.test.ts` | Keeps the production-scale `2,005` setup inserts but wraps them in one SQLite transaction. | Overlaps #330. Master is stronger: it makes the cap an optional `recordEmbeddingMeasurement` parameter and tests `cap=25` plus five overflow rows, preserving the eviction invariant in milliseconds rather than merely accelerating a disk-heavy 2,005-row fixture. No extraction recommended. |
| `tui-compiled-runtime-imports.test.ts` | Imports `@opentui/core` before parallel imports of the TUI runtime specifiers, avoiding the `TreeSitterClient` initialization TDZ race. | Independent TUI test-stability fix; no corresponding master change. Worth extracting. |

### #312 assessment

#338 does **not** implement either actual shared-database remedy described in #312. It does not give each test file a temporary `context.db`, change `test-preload.ts`, change `resolveDatabasePath()`, or lower/change `PRAGMA busy_timeout=5000` under tests. The shared `context.db` contention therefore remains.

Its 30-second Bun timeout is nevertheless a good diagnostic arm: the same 5-second SQLite wait should become a visible lock error rather than an unattributable Bun timeout. It should be taken as observability work, clearly documented as such, while the per-file database isolation follow-up remains open. It is not evidence that #312 is fixed.

### Verdict: **COMPLEMENTARY**

Do not merge #338's overlapping #330/#333 implementation. Extract exactly these independent/useful pieces with credit:

1. `packages/plugin/package.json`: `bun test --timeout 30000`, to expose SQLite lock failures before the test runner kills them. Keep #312 open because it does not isolate databases or alter test `busy_timeout`.
2. `packages/plugin/src/tui/tui-compiled-runtime-imports.test.ts`: warm `@opentui/core` before the parallel runtime-specifier imports to eliminate the observed TDZ race.

Do not extract the full-scale fixture transaction or completion-promise/fake-timer scheduler rewrite: master already has smaller, invariant-preserving #330 coverage and state-based #333 coverage.

### Draft public comment for #338

> Thank you, Cole — you filed this before our independent #330/#333 changes landed, and your diagnosis of the timing signature was useful. In particular, #312 correctly distinguishes a real SQLite lock wait from a merely slow test: Bun's 5s test timeout and SQLite's 5s `busy_timeout` currently hide the lock error behind the same wall-clock cutoff.
>
> In parallel, master landed `16b91c876` / `5b7238c9`: #330 now exercises the corpus-eviction invariant with an injectable small cap instead of 2,005 disk-backed inserts, and #333 waits for the observable rebuild state instead of an 80ms sleep. We therefore do not want to merge a second implementation of those overlapping changes.
>
> Two pieces of this PR are still useful and we will extract them into focused follow-ups with credit: `bun test --timeout 30000` so a SQLite lock surfaces before Bun kills the test, and the `@opentui/core` warm-up before parallel runtime imports. The timeout change is diagnostic only — it does not provide per-file `context.db` isolation or change the test `busy_timeout`, so #312 remains open for the real contention fix. We will close this PR with credit rather than merge the duplicate branch.

## Verification record

* `gh pr diff 337 --patch` / `gh pr view 337 ...` — completed, read-only.
* `gh pr diff 338 --patch` / `gh pr view 338 ...` — completed, read-only.
* `gh issue view 312 ...` — completed, read-only.
* `gh pr checks 337 && gh pr checks 338` — completed: all displayed project checks pass; `[code]smith` is skipped on both.
* Local commit/source comparison against `edd6e06e8`, `16b91c876`, and `5b7238c9` — completed, read-only.
