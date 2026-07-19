## Finding 1: Pi Compaction-Marker Drain Invariant Violation
- **Severity**: critical
- **Location**: `packages/pi-plugin/src/context-handler.ts` (lines 4369-4419)
- **Confidence**: high
- **Issue**: Pi drains the deferred compaction marker and advances it in the sessionManager (via `appendCompaction`) even when `injectM0M1Pi` failed to materialize the new compartment (e.g. due to database contention, where it falls back to replaying the cached m[0]/m[1] pair). This violates the ship-blocker invariant: *"One bust must cover BOTH a history rebuild AND its compaction-marker advance."*
- **Evidence**:
  In `packages/pi-plugin/src/context-handler.ts`, `deferredHistoryDrainEligible` is defined as:
  ```typescript
  const deferredHistoryDrainEligible =
      historyWasConsumedThisPass &&
      materializationSatisfiedThisPass &&
      (deferredHistoryWasPendingAtPassStart || hasPendingMaterializeSignal) &&
      !suppressDeferredHistoryDrain &&
      !casLost;
  ```
  `materializationSatisfiedThisPass` is set to `true` inside the `shouldApplyPendingOps` block (line 3775) when pending operations are applied, completely ignoring whether `injectM0M1Pi` actually materialized the new compartment or hit contention.
  If `injectM0M1Pi` hits contention, it catches `PiMaterializeContentionError` and falls back to replaying the cached m[0]/m[1] pair (which does NOT contain the new compartment).
  Yet, `deferredHistoryDrainEligible` remains `true`, so `applyDeferredPiCompactionMarker` is called, which calls `appendCompaction` and advances the compaction marker in the sessionManager past history that was NOT actually injected in the current prompt.
  In contrast, OpenCode's `transform-postprocess-phase.ts` correctly gates the drain on `m0RematerializedThisPass` (which is set to `result.m0Materialized` returned by `injectM0M1`).
- **Suggested Fix**:
  Modify `packages/pi-plugin/src/context-handler.ts` to set `materializationSatisfiedThisPass` based on the actual materialization outcome returned by `injectM0M1Pi`. Specifically, update `materializationSatisfiedThisPass` to `injectionResult?.m0Materialized === true` after `injectM0M1Pi` runs, matching OpenCode's `m0RematerializedThisPass` logic.

## Finding 2: Cross-Harness Prompt Divergence on History-Refresh-Only Passes
- **Severity**: high
- **Location**: `packages/pi-plugin/src/context-handler.ts` (line 2380) vs `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts` (line 1056)
- **Confidence**: high
- **Issue**: On a history-refresh-only pass (where a new compartment was published, but no pending ops or heuristics ran), OpenCode and Pi diverge on whether they inject the fresh todo state or replay the persisted anchor. This causes prompt cache misses and potential prompt divergence when switching between harnesses.
- **Evidence**:
  In OpenCode's `transform-postprocess-phase.ts`, the fresh todo state is injected only when `isCacheBustingPass` is `true`:
  ```typescript
  const isCacheBustingPass = shouldApplyPendingOps || shouldRunHeuristics;
  ```
  On a history-refresh-only pass, `shouldApplyPendingOps` and `shouldRunHeuristics` are both `false`, so `isCacheBustingPass` is `false`. Thus, OpenCode replays the persisted anchor (using the old todo state).
  In Pi's `context-handler.ts`, the fresh todo state is injected when `isCacheBustingForTodo` is `true`:
  ```typescript
  const isCacheBustingForTodo = isCacheBusting || result.executedWorkThisPass;
  ```
  Since `isCacheBusting` is `true` on a history-refresh-only pass, `isCacheBustingForTodo` is `true`. Thus, Pi injects the fresh todo state and updates the anchor.
  This causes the prompts to diverge on history-refresh-only passes.
- **Suggested Fix**:
  Align the cache-busting gate for todo injection in Pi to match OpenCode. Specifically, in `packages/pi-plugin/src/context-handler.ts`, `isCacheBustingForTodo` should be set to `result.executedWorkThisPass` (which is equivalent to OpenCode's `isCacheBustingPass`), rather than including `isCacheBusting`.

## Finding 3: Git Cooldown Usability Delay on Manual Fixes
- **Severity**: low
- **Location**: `packages/plugin/src/features/magic-context/memory/project-identity.ts` (lines 400-415)
- **Confidence**: high
- **Issue**: When git identity resolution fails (e.g. due to `dubious_ownership`), the directory is placed on a 5-minute cooldown. If the user immediately runs the suggested fix (`git config --global --add safe.directory <dir>`), Magic Context will still ignore the repository and return the `dir:` fallback for the remainder of the 5-minute window.
- **Evidence**:
  ```typescript
  if (getActiveCooldown(canonical) !== undefined) {
      if (hasGitDir(canonical)) {
          const cachedGitIdentity = reuseLastKnownGitIdentity(canonical);
          if (cachedGitIdentity !== undefined) {
              return cachedGitIdentity;
          }
      }
      return directoryFallback(canonical);
  }
  ```
  On cold start, `reuseLastKnownGitIdentity` returns `undefined`, so it immediately returns `directoryFallback(canonical)` without re-probing git until the cooldown expires.
- **Suggested Fix**:
  This is a minor usability tradeoff to prevent slow git probes from stalling the transform path. However, we could clear the cooldown for a directory if a command like `/ctx-status` or `/ctx-recomp` is manually run, or simply document that a restart/wait is required.

## Summary
- **Total findings**: 3 (1 critical, 1 high, 1 low)
- **Overall Verdict**: **HOLD**
- **Rationale**: The release must be held because **Finding 1** violates a ship-blocker invariant ("One bust must cover BOTH a history rebuild AND its compaction-marker advance") on the Pi harness. Under database contention, Pi will advance the compaction marker in the sessionManager past history that was not actually injected, leading to potential prompt cache issues or incorrect state. Additionally, **Finding 2** introduces an undocumented prompt divergence between OpenCode and Pi on history-refresh-only passes.