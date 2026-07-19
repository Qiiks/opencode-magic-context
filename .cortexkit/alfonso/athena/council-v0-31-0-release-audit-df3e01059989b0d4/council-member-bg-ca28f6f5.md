## Finding 1: Pi restart rehydration upgrades deferred publishes into explicit materialization
- **Severity**: P0 blocker
- **Location**: `packages/pi-plugin/src/index.ts:533-537`; `packages/pi-plugin/src/context-handler.ts:3705-3724`, `3736-3780`; `packages/pi-plugin/src/context-handler.ts:2889-2903`
- **Confidence**: high
- **Issue**: After restart, Pi rehydrates pending publish state with the **explicit flush** signal (`pendingMaterialization`) instead of the normal **deferred publish** signal (`deferredMaterialization`). That can make a background/manual publish materialize on the very next defer pass, violating the invariant that background/manual publishes must not force materialization.
- **Evidence**: Startup calls `signalPiDeferredHistoryRefresh(sid)` **and** `signalPiPendingMaterialization(sid)` (`index.ts:533-537`), but normal historian publication uses `signalPiDeferredHistoryRefresh(sessionId)` + `signalPiDeferredMaterialization(sessionId)` (`context-handler.ts:2902-2903`). In `runPipeline`, `hasPendingMaterialization()` feeds `baseShouldApplyPendingOps` directly (`context-handler.ts:3705-3724`), so `applyPendingOperations(...)` can run even when the scheduler is not executing (`3736-3754`).
- **Suggested Fix**: Rehydrate with `signalPiDeferredMaterialization(...)`, not `signalPiPendingMaterialization(...)`, or add a separate restart-only deferred signal that still requires the normal `canConsumeDeferredLate` / natural-bust gate.

## Finding 2: Foreign publishes are invisible to already-running peer processes
- **Severity**: P1 should-fix
- **Location**: OpenCode `packages/plugin/src/hooks/magic-context/hook.ts:243-255`, `packages/plugin/src/hooks/magic-context/transform.ts:916-930`, `1049-1052`, `1648-1650`; Pi `packages/pi-plugin/src/index.ts:533-537`, `packages/pi-plugin/src/context-handler.ts:2140`, `3444-3445`, `4369-4383`; also `packages/plugin/src/hooks/magic-context/restart-history-omission.test.ts:3-20`
- **Confidence**: high
- **Issue**: Deferred history/marker consumption is only rehydrated from durable state at **process startup**. If OpenCode/Pi process A publishes and process B is already running on the same `context.db`, process B never sees the foreign deferred-history signal, so it can continue using stale history/marker state until restart or some unrelated local bust.
- **Evidence**: Publish paths add only to in-memory sets (`transform.ts:1049-1052`, `1648-1650`); consumers read only local `Set.has(sessionId)` (`transform.ts:916-930`; `context-handler.ts:3444-3445`, `4369-4383`). Boot-time rehydration exists (`hook.ts:243-255`, `index.ts:533-537`), but there is no per-pass durable peek for already-running peers. The restart regression test explicitly describes the underlying assumption: publishes set only in-memory deferred-refresh signals (`restart-history-omission.test.ts:3-20`).
- **Suggested Fix**: Make persisted pending-marker state (or a dedicated durable deferred-history flag) a pass-start trigger, so any running process can notice and safely consume a foreign publish.

## Finding 3: Pi auto-search permanently caches retryable failures as “no hint”
- **Severity**: P1 should-fix
- **Location**: `packages/pi-plugin/src/auto-search-pi.ts:312-320`, `402-416`; contrast `packages/plugin/src/hooks/magic-context/auto-search-runner.ts:349-365` and `packages/plugin/src/hooks/magic-context/auto-search-runner.test.ts:141-181`
- **Confidence**: high
- **Issue**: In Pi, a transient auto-search timeout/error is stored as a permanent `no-hint` decision for that message. Later passes short-circuit on the stored decision and never retry, so brief embedding/runtime failures suppress hinting for the entire turn.
- **Evidence**: Pi replays any existing decision and exits (`auto-search-pi.ts:312-320`); its catch path writes `"error"` and its timeout path writes `"timeout"` via `writeNoHintAndReconcile(...)` (`402-416`). OpenCode does the opposite: timeout/error are treated as retryable and intentionally not persisted (`auto-search-runner.ts:349-365`), with a test that asserts the second pass retries (`auto-search-runner.test.ts:170-181`).
- **Suggested Fix**: Match OpenCode semantics in Pi: do not persist `no-hint` on timeout/error; only persist stable outcomes like `empty`, `below-threshold`, `too-short`, or `stacked`.

## Summary
- **Findings**: 1×P0, 2×P1
- **Overall verdict**: **HOLD**
- **Blockers**: Finding 1 is a direct invariant breach. Finding 2 is also high-risk if concurrent OpenCode/Pi instances sharing one `context.db` are a supported scenario for this release.
- **Overall confidence**: high