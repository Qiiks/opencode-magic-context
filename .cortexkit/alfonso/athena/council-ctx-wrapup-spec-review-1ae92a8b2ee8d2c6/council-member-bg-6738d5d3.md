## Finding 1: Existing drain quota can stop `/ctx-wrapup` before the keep watermark
- **Severity**: BLOCKER
- **Location**: `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:425-498`; `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:325-343`
- **Confidence**: high
- **Issue**: The spec promises “no hard iteration cap,” but the incremental runner always calls `reserveProtectedTailDrainTokens`. Once the 10-minute window budget is exhausted, the runner returns a quota no-op instead of running another chunk.
- **Evidence**: `protectedTailWindowBudget` caps non-emergency drains at up to 500k tokens (`storage-meta-persisted.ts:380-390`), and `reserveProtectedTailDrainTokens` returns without `ok` when `reserved <= 0` (`:484-498`). The runner treats that as a no-op and exits (`compartment-runner-incremental.ts:335-343`).
- **Suggested Fix**: Add an explicit manual/reactive-drain bypass or separate budget class for `/ctx-wrapup`; do not fake emergency usage unless all emergency side effects are audited.
- **Verdict**: BLOCK until fixed.

## Finding 2: Successful first chunk clears emergency recovery even if a multi-run reactive drain is incomplete
- **Severity**: HIGH
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:663-669`; `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:1431-1438`
- **Confidence**: high
- **Issue**: The current runner assumes one successful publication completes overflow recovery. In a multi-chunk downswitch drain, the first chunk can clear `needs_emergency_recovery`; if a later chunk fails/stops before the new-model target, the next request can still overflow without the recovery flag.
- **Evidence**: The publish path comments “overflow recovery is complete” and calls `clearEmergencyRecovery` on every successful incremental publish (`compartment-runner-incremental.ts:663-669`). That function clears `needs_emergency_recovery` and the no-head counter (`storage-meta-persisted.ts:1431-1438`).
- **Suggested Fix**: In wrapup/reactive-drain mode, defer clearing emergency recovery until target coverage is reached; on partial reactive failure, abort the user send or re-arm recovery.
- **Verdict**: REVISE.

## Finding 3: No loop-level race-free guard exists for sequential wrapup runs
- **Severity**: HIGH
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner.ts:30-34,100-120,145-152,187-240`
- **Confidence**: high
- **Issue**: Existing serialization protects one historian/recomp run, not an entire multi-iteration drain loop. A wrapup implemented as “normal historian run” per iteration can release the active run/DB lease between chunks, allowing `/ctx-recomp`, another wrapup, or the 95% transform arm to start in the gap.
- **Evidence**: `activeRuns` is a process-local map (`:30-34`); `startCompartmentAgent` acquires a lease for one run (`:109-120`) and releases it in `finally` (`:145-152`). By contrast, recomp holds the active run/lease around its whole multi-pass operation (`:204-240`).
- **Suggested Fix**: Add a session-scoped wrapup/drain lease covering the whole loop, or run the full loop under one active-run registration and one renewable DB lease.
- **Verdict**: BLOCK for concurrency.

## Finding 4: Forcing discard-last off on the final chunk promotes weak-boundary facts/observations/primers
- **Severity**: MEDIUM
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:481-509,558-566,632-638,795-825`
- **Confidence**: high
- **Issue**: The spec’s “discard-last OFF for final chunk” satisfies coverage but bypasses the runner’s quality guard. The code explicitly says the last greedy compartment lacks lookahead; when it is not discarded, unanchored facts, user observations, and primers become durable.
- **Evidence**: The runner drops the final provisional compartment when lookahead is weak (`:481-509`). It skips fact promotion only when `discardedLast` is true (`:558-566`, `:632-638`) and similarly gates user observations/primers (`:795-825`).
- **Suggested Fix**: Either include kept-tail lookahead for the final pass, or add a “persist final but suppress unanchored promotions” mode.
- **Verdict**: REVISE.

## Finding 5: Pi deferred marker clobber is not benign because the pending blob contains chunk-local summary data
- **Severity**: MEDIUM
- **Location**: `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:1863-1919`; `packages/pi-plugin/src/pi-historian-runner.ts:865-996`; `packages/pi-plugin/src/compaction-marker-manager-pi.ts:71-80`
- **Confidence**: high
- **Issue**: OpenCode’s pending marker is mostly an ordinal/id target, but Pi’s pending marker also stores `summary` and `tokensBefore`. Sequential deferred wrapup publishes overwrite the single pending Pi blob, so the final native Pi compaction can cover all earlier chunks while using only the last chunk’s summary/tokens.
- **Evidence**: `PendingPiCompactionMarker` includes `summary` and `tokensBefore` (`storage-meta-persisted.ts:1863-1869`) and `setPendingPiCompactionMarkerState` overwrites one slot (`:1910-1919`). The Pi runner builds `markerSummary` from only `newCompartments` for the current run and stores it (`pi-historian-runner.ts:865-996`). The drain passes `pending.summary` to `appendCompaction` (`compaction-marker-manager-pi.ts:71-80`).
- **Suggested Fix**: For Pi wrapup, aggregate summaries/tokens across the loop or recompute the pending marker from all compartments since the previous native boundary.
- **Verdict**: REVISE.

## Finding 6: Pi emergency machinery cannot deliver the reactive blocking drain as specified
- **Severity**: HIGH
- **Location**: `packages/pi-plugin/src/context-handler.ts:2036-2075,2818-2925,2983-2985`
- **Confidence**: high
- **Issue**: The spec says Pi should use the same loop day one, but existing Pi emergency handling waits for at most one already-running historian and caps that wait at 30s. It does not start or await a multi-run drain before the outgoing request.
- **Evidence**: The emergency block explicitly caps waiting at 30s (`context-handler.ts:2051-2054`) and only waits `inFlightHistorian.get(sessionId)` (`:2071-2075`). `spawnPiHistorianRun` is fire-and-forget and stores one promise (`:2818-2925`), while trigger evaluation skips if one is already in flight (`:2983-2985`).
- **Suggested Fix**: Add a dedicated awaited Pi drain-loop primitive with its own timeout/cancellation semantics; do not reuse the current 30s emergency wait.
- **Verdict**: BLOCK for Pi parity/reactive correctness.

## Finding 7: Runner APIs do not return enough structured outcome for “failure honesty”
- **Severity**: MEDIUM
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:96,335-343,464-475,513-531`; `packages/plugin/src/hooks/magic-context/compartment-runner.ts:100`
- **Confidence**: high
- **Issue**: `/ctx-wrapup` must report coverage reached and stop honestly on chunk failure/no-progress. The current incremental runner and starter return `void`; failures/no-ops are logged/telemetry-only, so a drain loop must infer outcomes by rereading DB state and cannot reliably surface the specific stop reason.
- **Evidence**: `runCompartmentAgent` returns `Promise<void>` (`:96`), quota no-op returns without structured status (`:335-343`), validation failure returns after notification (`:464-475`), and no-progress returns similarly (`:513-531`). `startCompartmentAgent` also returns `void` (`compartment-runner.ts:100`).
- **Suggested Fix**: Refactor the runner or add a wrapup wrapper that returns `{status, priorEnd, newEnd, failureReason, published}` per iteration.
- **Verdict**: REVISE.

## Verified non-issues / safe points
- **OpenCode pending marker single-slot clobber appears safe**: the pending blob is ordinal/id only (`storage-meta-persisted.ts:1818-1827`), `applyDeferredCompactionMarker` validates the final target (`compaction-marker-manager.ts:84-118`), and marker advancement is monotonic (`:139-183`).
- **Publication floor is consistent with normal passes**: publish records `lastCompartmentEnd + 1` (`compartment-runner-incremental.ts:657-669`), while normal boundary resolution uses `runtimeFloor = max(offset, priorBoundaryOrdinal)` (`protected-tail-boundary.ts:479-481`).
- **Tail growth by append does not invalidate snapshots**: validation allows current raw count to be larger and checks the old last ordinal/id plus the `[offset, eligibleEnd)` fingerprint (`protected-tail-boundary.ts:710-763`; fingerprint definition `read-session-true-raw-tokens.ts:661-673`).
- **`/ctx-flush` can materialize wrapup output**: command handling calls `onFlush` after `executeFlush` (`command-handler.ts:537-539`), and OpenCode wires that to history refresh + materialization signals (`hook.ts:752-758`).

## Summary
Findings: **2 BLOCKER/HIGH blockers**, **3 HIGH/MEDIUM design risks**, **2 MEDIUM implementation gaps**. The core OpenCode idea is plausible, but the spec as written conflicts with real quota, recovery, concurrency, and Pi mechanics.

**Overall verdict: BLOCK** — do not ship until Findings 1, 3, and 6 are fixed; revise Findings 2, 4, 5, and 7 before implementation freeze.