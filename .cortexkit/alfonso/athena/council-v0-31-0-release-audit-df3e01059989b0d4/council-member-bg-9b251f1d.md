## Finding 1: Expired `wrapup_in_progress` blob not reclaimed when read inside an outer SQLite transaction
- **Severity**: P1 (should-fix)
- **Location**: `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:434-469`
- **Confidence**: high
- **Issue**: `getWrapupInProgressState` treats an expired marker as “not in progress” (`return null` at 441) but, if `BEGIN IMMEDIATE` fails because the caller is already in a write transaction (445-448), it **does not** NULL the expired JSON. `isWrapupInProgress` uses the same path (`472-474`). Any code that only consults these helpers will behave as if wrapup is idle while `session_meta.wrapup_in_progress_state` still holds a stale blob until some later standalone read succeeds in reclaiming it.
- **Evidence**: Comment at 445-447 explicitly documents returning null without cleanup; reclaim only runs when `BEGIN IMMEDIATE` succeeds (452-457). `compartment-runner.ts:112-118` and `pi-plugin/src/context-handler.ts:2989-2997` gate trigger-fired historian on `isWrapupInProgress`.
- **Why it matters**: After TTL expiry, trigger-fired historian / incremental compartment runs can resume **before** the durable row is cleared. That weakens the documented mutual-exclusion story (wrapup vs trigger historian) in multi-statement write paths (common in transform/storage). A new `/ctx-wrapup` can still acquire (494-498 checks `expiresAt > now`), so you can get overlapping “logical” states across processes until reclaim runs.
- **Suggested Fix**: On expired state, always attempt reclaim via a savepoint/nested transaction, or schedule async reclaim; alternatively treat “expired but row present” as inactive for triggers but run a best-effort `UPDATE ... NULL` even when outer txn is open (document SQLite nesting policy).

## Finding 2: OpenCode hook rehydrates pending compaction markers into **history** signals only, not **deferred materialization**
- **Severity**: P2 (nit / suspicion — needs multi-process scenario validation)
- **Location**: `packages/plugin/src/hooks/magic-context/hook.ts:243-258` vs `wrapup-orchestrator.ts:198-205`
- **Confidence**: medium
- **Issue**: Wrapup publication sets both `deferredHistoryRefreshSessions` and `deferredMaterializationSessions` in-process. After plugin restart, only `getSessionsWithPendingMarker` → `deferredHistoryRefreshSessions.add` is rehydrated; there is no DB-backed re-seed of `deferredMaterializationSessions` (contrast `storage-meta-persisted.ts:2198-2203` comment mentioning both sets).
- **Evidence**: Hook init loop 251-255 adds only `deferredHistoryRefreshSessions`. `getSessionsWithPendingMarker` at 2209-2217 reads `pending_compaction_marker_state` only.
- **Why it matters**: `transform-postprocess-phase.ts` ties deferred materialization to `deferredMaterializationSessions` (239-266). `materializationSatisfied` can still be true when that set is empty (`1150-1153`), so marker drain may work; suspicion is **queued drop / heuristic materialization** after wrapup+crash may follow a different path than in-process wrapup until an execute/flush pass, diverging from “ride the next natural bust” intent for Pi/OpenCode parity.
- **Suggested Fix**: On hook init, also add sessions with pending markers to `deferredMaterializationSessions`, or document that OpenCode relies on pending_ops + history rehydration alone post-restart.

## Finding 3: Pi plugin restart rehydration uses **pending** materialization, not **deferred** (stronger bust than wrapup’s in-flight publish)
- **Severity**: P2 (cross-harness / upgrade path)
- **Location**: `packages/pi-plugin/src/index.ts:532-537`, `packages/pi-plugin/src/commands/ctx-wrapup.ts:380-384`
- **Confidence**: high
- **Issue**: Wrapup `onPublished` signals `signalPiDeferredHistoryRefresh` + `signalPiDeferredMaterialization` (deferred sets). Startup rehydration for pending Pi markers calls `signalPiDeferredHistoryRefresh` + `signalPiPendingMaterialization` (pending = flush-like execute intent per `context-handler.ts:392-398`).
- **Evidence**: `ctx-wrapup.ts:382-383` vs `index.ts:535-536`. `historian-publish-signals.test.ts` expects wrapup path **not** to use `signalPiPendingMaterialization`.
- **Why it matters**: Users upgrading mid-session or restarting Pi after a partial wrapup may get a **cache-busting materialization** on next activation instead of mid-turn-gated deferred consumption (`AUDIT-KNOWN-ISSUES.md` A6). Usually correct for draining wedged markers (`index.ts:1035-1049`), but it is a behavioral delta from live wrapup completion.
- **Suggested Fix**: Split rehydration: use deferred signals when marker came from deferred publish (wrapup/historian), pending only for explicit `/ctx-flush` or document as intentional “stuck marker recovery” bust.

## Finding 4: Dubious-ownership git failure returns `dir:` even when `lastKnownGitIdentity` exists
- **Severity**: P2 (suspicion — identity / memory split under shared-repo Docker)
- **Location**: `packages/plugin/src/features/magic-context/memory/project-identity.ts:397-413`
- **Confidence**: medium
- **Issue**: For `dubious_ownership`, code sets transient cooldown and calls `reuseLastKnownGitIdentity` (405-408), but if that returns a value it still **returns `fallback` (`dir:`)** at 413 after `recordDubiousOwnershipFallback` — wait, let me re-read.

Actually lines 405-408:
```
const cachedGitIdentity = reuseLastKnownGitIdentity(canonical);
if (cachedGitIdentity !== undefined) {
    return cachedGitIdentity;
}
```
Then 410-413 dubious ownership records and returns fallback.

So if last known git exists, it returns git identity before dubious ownership fallback path... Order is: 405-408 return cached git; only if no cached git, 410-413 return dir fallback for dubious ownership.

I'll withdraw finding 4 as incorrect.

## Finding 5: `getWrapupInProgressState` during active wrapup + nested txn: false negative enables parallel historian (same as Finding 1)

Merged into Finding 1.

## Finding 6: Wrapup partial failure always releases marker in `finally` — good; crash mid-chunk leaves TTL — verified
- **Severity**: informational (invariant holds)
- **Location**: `wrapup-orchestrator.ts:404-407`, `WRAPUP_IN_PROGRESS_TTL_MS` at `storage-meta-persisted.ts:397`
- **Confidence**: high
- **Evidence**: `storage-wrapup-marker.test.ts:57-75` stale reclaim test.

## Finding 7: Compaction marker drain gated on consumed boundary — ship-blocker invariant protected
- **Severity**: informational
- **Location**: `transform-postprocess-phase.ts:89-96`, `1188-1202`
- **Confidence**: high
- **Evidence**: `pendingMarkerCoveredByConsumedBoundary` prevents advancing marker past rendered boundary; aligns with “one bust must cover history rebuild AND marker advance.”

## Finding 8: Subagent caveman exclusion — invariant holds
- **Severity**: informational
- **Location**: `transform.ts:1529`, `1761` (`!reducedMode`); `hook.ts:667-668` comment
- **Confidence**: high

## Finding 9: Provisional ctx_reduce verdict withholds system-prompt hash baseline
- **Severity**: informational (invariant holds)
- **Location**: `system-prompt-hash.ts:295-301`, `ctx-reduce-availability.ts:64-71`
- **Confidence**: high
- **Evidence**: `system-prompt-hash.test.ts:774-804`

## Finding 10: Notes as ctx_search source with session-scoped anchors
- **Severity**: informational
- **Location**: `search.ts:909-951`, `tools/ctx-search/tools.ts:68-100`
- **Confidence**: high
- **Evidence**: Session + smart notes merged; `anchorOrdinal` on results; tests filter foreign session anchors.

## Summary

| Severity | Count |
|----------|-------|
| P0 | 0 confirmed |
| P1 | 1 |
| P2 | 2 |
| Suspicion / parity | 1 (Finding 2) |

**Overall verdict: HOLD** (soft hold — no confirmed P0 ship-blocker in source reviewed)

**Rationale**: Core v0.31.0 invariants checked in tree look intentionally implemented: wrapup TTL (5 min) + 60s renewal (`wrapup-orchestrator.ts:285-290`), mutual exclusion with recomp/upgrade/trigger historian (`compartment-runner.ts:112-118`, `recomp-orchestrator.ts:352`), deferred marker publication with boundary-gated drain (`compartment-runner-incremental.ts:554-693`, `transform-postprocess-phase.ts:1188-1202`), `forceKeepLastCompartment` downgraded on `chunk.hasMore` (`compartment-runner-incremental.ts:352-353, 574-579`), project-identity reuse on transient git failure (`project-identity.ts:384-408`), removed `ctx_reduce_enabled` (config test at `index.test.ts:717-724`), migration v50 wrapup column (`migrations-v50.test.ts`).

**Blocker for SHIP**: **Finding 1** — expired wrapup marker cleanup vs `isWrapupInProgress` false-negative inside nested DB transactions can allow trigger-fired historian to run while a stale wrapup row remains and mutual-exclusion semantics are fuzzy across processes. Recommend fixing or documenting with an integration test (two processes / nested txn + expired marker) before calling release green.

**Not raised as defects**: Pi vs OpenCode deferred-signal differences largely documented in `PARITY.md` and `AUDIT-KNOWN-ISSUES.md` A6; emergency-drop + smart-drops + wrapup interaction appears gated on execute/materialization passes (`transform-postprocess-phase.ts:494-652`).