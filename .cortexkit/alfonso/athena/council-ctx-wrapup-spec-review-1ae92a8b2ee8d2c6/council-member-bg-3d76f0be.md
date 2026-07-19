## Finding 1: Manual `/ctx-wrapup` would hit the normal drain quota and stop early
- **Severity**: BLOCKER
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:325-343`; `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:380-389, 484-498`; Pi mirror in `packages/pi-plugin/src/pi-historian-runner.ts:421-437`
- **Confidence**: high
- **Issue**: The spec says `/ctx-wrapup` is a forced drain loop with no iteration cap. But the incremental runner always goes through `reserveProtectedTailDrainTokens()`. Outside the 95% emergency latch, that quota is finite per 10-minute window, so a low-pressure manual wrapup can no-op with quota exhaustion before reaching the keep watermark.
- **Evidence**: `runCompartmentAgent()` exits on `!reserve.ok` with `"protected-tail drain quota exhausted"`. The budget helper only bypasses quota when the emergency latch is active (`usagePercentage >= 95`), not for ordinary manual drains.
- **Suggested Fix**: Add an explicit wrapup/forced-drain mode that bypasses the normal protected-tail quota, or a separate quota contract for manual drains.
- **Verdict**: BLOCK

## Finding 2: Multi-run reactive recovery would clear `needsEmergencyRecovery` after the first successful chunk
- **Severity**: BLOCKER
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:663-669`; Pi mirror in `packages/pi-plugin/src/pi-historian-runner.ts:982-983`
- **Confidence**: high
- **Issue**: The current runner assumes one successful publish completes overflow recovery. In a multi-run downswitch drain, that is false. If chunk 1 publishes and chunk 2 later fails/times out, the recovery flag is already gone, so the next user send will not auto-block/retry even though the session may still be too large for the new model.
- **Evidence**: The runner unconditionally calls `clearEmergencyRecovery(...)` immediately after every successful incremental publish, with a comment saying successful publication means recovery is complete.
- **Suggested Fix**: Suppress `clearEmergencyRecovery()` during loop iterations and clear it only once the loop reaches the target watermark; re-arm on partial completion/failure.
- **Verdict**: BLOCK

## Finding 3: The current blocking path only waits for one run, with a hard timeout, then proceeds
- **Severity**: BLOCKER
- **Location**: `packages/plugin/src/hooks/magic-context/transform-compartment-phase.ts:224-239, 390-405`; Pi current wait in `packages/pi-plugin/src/context-handler.ts:2051-2075`
- **Confidence**: high
- **Issue**: The spec wants the send to block until the drain loop finishes, potentially for minutes. The current machinery does not do that: it waits for one active run, races it against a timeout, and then continues without waiting.
- **Evidence**: `awaitCompartmentRun()` uses `Promise.race(...)` against `historianTimeoutMs` (default 120s) and returns `"timed_out"`; the caller logs and proceeds. Pi is stricter: it waits only an existing in-flight historian and caps that wait at 30s.
- **Suggested Fix**: Introduce a dedicated loop-level blocking budget/await path for wrapup and model-downswitch recovery; do not reuse the existing one-run timeout semantics unchanged.
- **Verdict**: BLOCK

## Finding 4: There is no loop-wide guard; per-run lease/flags allow interleaving between iterations
- **Severity**: BLOCKER
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner.ts:100-167`; `packages/plugin/src/hooks/magic-context/transform-compartment-phase.ts:273-315, 324-357`; `/ctx-recomp` gate in `packages/plugin/src/hooks/magic-context/compartment-runner.ts:191-224`
- **Confidence**: high
- **Issue**: Existing serialization is per historian run, not per multi-iteration loop. Between iterations, the DB lease is released and `activeRuns` is cleared. Another transform pass or `/ctx-recomp` can acquire the same session and mutate compartment state mid-wrapup.
- **Evidence**: `startCompartmentAgent()` acquires/releases the lease around a single run. `runCompartmentPhase()` treats `compartmentInProgress && !activeRun` as a signal to start a new run, so that flag is not a safe loop-wide blocker. `/ctx-recomp` only checks `activeRuns`/lease at command start.
- **Suggested Fix**: Add a dedicated session-wide wrapup/forced-drain state (or higher-level lease) and make transform, recomp, and wrapup all respect it.
- **Verdict**: BLOCK

## Finding 5: Turning discard-last off for the final chunk would durably persist weak-lookahead artifacts
- **Severity**: HIGH
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:481-509, 585-591, 632-638, 795-811, 824-876`; Pi mirror in `packages/pi-plugin/src/pi-historian-runner.ts:822-907, 950-957, 1032-1100`
- **Confidence**: high
- **Issue**: The final-chunk override is not just a boundary-quality tradeoff. In current code, `discardedLast` is the gate that suppresses or filters durable side effects. If the final weak-lookahead compartment is kept, facts, events, user-memory candidates, and primer candidates become durable immediately.
- **Evidence**: Facts are promoted only when `!discardedLast`; events are filtered only when `discardedLast` dropped a tail compartment; user observations and primers are also gated on `!discardedLast`.
- **Suggested Fix**: Separate “persist final coverage” from “promote durable artifacts.” If final coverage must keep the last compartment, add a no-promotion/no-event mode for that last weak-boundary chunk.
- **Verdict**: REVISE

## Finding 6: Pi day-one parity cannot reuse the current fire-and-forget historian path
- **Severity**: HIGH
- **Location**: `packages/pi-plugin/src/context-handler.ts:2182-2235, 2265-2279`; `packages/pi-plugin/src/context-handler.ts:2818-2926`
- **Confidence**: high
- **Issue**: Pi currently runs the transform pipeline first, then *afterward* maybe fires historian in the background. That is the opposite of the spec’s reactive parity requirement (“drain before the request goes out”).
- **Evidence**: The `context` handler awaits `runPipeline(...)`, then calls `maybeFireHistorian(...)`; the comment explicitly says historian is fire-and-forget and never blocks the LLM call. `spawnPiHistorianRun()` always launches a detached promise.
- **Suggested Fix**: Add a synchronous `runPiHistorian` loop path inside the `context` handler for wrapup/reactive drain, with its own progress/budget handling; do not try to layer this onto `maybeFireHistorian()` unchanged.
- **Verdict**: REVISE

## Finding 7: Unknown-model downswitches still have no trusted pre-send limit
- **Severity**: MEDIUM
- **Location**: `packages/plugin/src/hooks/magic-context/transform.ts:713-727`; `packages/plugin/src/hooks/magic-context/event-resolvers.ts:66-95`
- **Confidence**: high
- **Issue**: The proposed phase-2 watermark depends on the new model’s trusted limit, but the current pre-send arm only works when `getSdkContextLimit()` returns a real catalog limit. For unknown/new models, the trusted limit is intentionally `undefined` until an overflow is observed.
- **Evidence**: The proactive arm requires a numeric `armCatalogLimit`; `resolveTrustedContextLimit()` returns `undefined` when neither models.dev/override nor a previously detected overflow limit exists.
- **Suggested Fix**: Specify fallback behavior for unknown models (e.g. disable proactive wrapup with an honest message, or use an explicit configured limit).
- **Verdict**: REVISE

## Summary
- **Findings**: 4 BLOCKER, 2 HIGH, 1 MEDIUM
- **Overall risk**: high
- **Overall confidence**: high

**Verified non-issues**
- **Single-slot pending marker overwrite looks safe**: only the deferred drain/recomp/session-compacted paths consume it, and marker movement is monotonic (`compaction-marker-manager.ts:84-117, 139-183`; call sites in `transform-postprocess-phase.ts:1179-1223`).
- **`prior_boundary_ordinal` write looks safe**: normal boundary resolution already clamps to `max(offset, priorBoundaryOrdinal)` and publish writes `lastCompartmentEnd + 1`, matching the next offset (`protected-tail-boundary.ts:479-481, 591-648`; `storage-meta-persisted.ts:341-355`).
- **Growing tail fingerprinting is safe on append-only chat**: OpenCode and Pi both assign stable existing ordinals and append newer messages after the current eligible range (`read-session-raw.ts:76-117`; `read-session-pi.ts:305-433`; `protected-tail-boundary.ts:758-764`).
- **`/ctx-flush` should apply wrapup output correctly**: historian publish raises deferred history/materialization signals, and `/ctx-flush` adds the explicit rebuild/materialization signals needed for the next pass (`transform.ts:1049-1052, 1648-1650`; `hook.ts:752-757`; `transform-postprocess-phase.ts:1179-1223`).

**Overall verdict: BLOCK**

Primary blockers:
1. manual wrapup would still be throttled by the normal drain quota,
2. reactive multi-run recovery would clear the overflow-recovery flag after the first successful chunk,
3. the current blocking machinery only waits one run and times out instead of truly draining to target.