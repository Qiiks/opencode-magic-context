## Finding 1: Phase-2 reactive drain cannot close a large→small gap with current blocking mechanics
- **Severity**: BLOCKER
- **Location**: `packages/plugin/src/hooks/magic-context/transform.ts:754-766` (bumps to 95%, does not run a multi-iteration drain); `transform-compartment-phase.ts:224-262,324-407` (`awaitCompartmentRun` races **one** run, default `historianTimeoutMs` **120s**); `transform.ts:707-741` (proactive arm uses `getSdkContextLimit` + `lastInputTokens`, not a watermark from `executeThreshold × newLimit`)
- **Confidence**: high
- **Issue**: The spec’s phase-2 “same drain loop, blocking before the request” replaces one-shot recovery, but the only blocking path today waits for **at most one** historian completion (then proceeds on timeout). A ~280k-token gap needs **many** sequential LLM historian passes (minutes), which this path cannot guarantee before OpenCode sends the outgoing call.
- **Evidence**: `awaitCompartmentRun` is `Promise.race([activeRun.promise, timeout])` with no loop; comment at `transform-compartment-phase.ts:319-322` explicitly limits blocking to 95% **one** run; proactive switch only sets `needsEmergencyRecovery` and percentage bump, not a computed keep watermark.
- **Suggested Fix**: Implement the drain loop on the **send path** (new orchestrator) with unbounded or user-visible multi-run budget; do **not** extend only `awaitCompartmentRun` without removing the 120s single-run cap for downswitch.

## Finding 2: `clearEmergencyRecovery` on every incremental publish breaks a multi-iteration wrapup loop
- **Severity**: BLOCKER
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:495-509,496-498,663-669`
- **Confidence**: high
- **Issue**: Discard-last is gated on `!getOverflowState(...).needsEmergencyRecovery`. Each successful publish calls `clearEmergencyRecovery` **before** the next loop iteration. Mid-wrapup (and mid reactive drain), iteration 2+ therefore runs **with** discard-last and **without** emergency semantics—contradicting “discard-last OFF on final chunk only” and reintroducing boundary-healing drops on non-final iterations when pressure is still high.
- **Evidence**: `inEmergency` read once per run; `clearEmergencyRecovery` at lines 669 in same publish transaction as floor/marker.
- **Suggested Fix**: Add an explicit runner dep (e.g. `wrapupDrainMode: true`) that skips `clearEmergencyRecovery` until the **orchestrator** finishes, and forces discard-last policy per iteration (off only on final).

## Finding 3: No mechanism to force discard-last off; final weak-boundary compartment gets full promotion
- **Severity**: HIGH
- **Location**: `compartment-runner-incremental.ts:495-509,566,632-638,589-591`
- **Confidence**: high
- **Issue**: Discard-last is purely `inEmergency` + lookahead margin; there is no dep to disable it. If wrapup disables discard-last on the final iteration, that compartment was chosen without lookahead (`BOUNDARY_HEALING_SLACK`), yet `promotionActive && !discardedLast` promotes facts and filtered events persist—opposite of the runner’s safety model for provisional tails.
- **Evidence**: Comments at 481-493 state last compartment is “structurally unreliable”; discard-last skips fact promotion (558-565, 632).
- **Suggested Fix**: Add `forcePersistAllCompartments?: boolean` **or** `disableDiscardLast?: boolean` on `CompartmentRunnerDeps`; document accepted tradeoff or gate final-chunk promotion when lookahead &lt; slack.

## Finding 4: `recompProgressBySession` is single-slot and incremental runner does not emit progress
- **Severity**: HIGH
- **Location**: `recomp-orchestrator.ts:145-162`; `compartment-runner-types.ts:105`; `compartment-runner-recomp.ts:225-237` (only recomp wires `onRecompProgress`)
- **Confidence**: high
- **Issue**: Spec reuses recomp progress for per-chunk wrapup rows, but `runCompartmentAgent` / incremental path never calls `onRecompProgress`. Concurrent `/ctx-recomp` and wrapup would overwrite the same `Map` entry; kind is only `recomp|upgrade|embed` with no `wrapup`.
- **Suggested Fix**: Add `kind: "wrapup"`, emit progress from the **loop orchestrator** (not only inside one runner call), and reject second wrapup/recomp while `phase !== done|failed` for that session.

## Finding 5: Wrapup vs ≥95% transform pass — lease serializes, but no shared “drain owner” and timeout asymmetry
- **Severity**: HIGH
- **Location**: `compartment-runner.ts:100-121,608`; `transform-compartment-phase.ts:330-407`; `compartment-runner-incremental.ts:606-616`
- **Confidence**: high
- **Issue**: Spec says wrapup waits for in-flight historian like the 95% arm, but a concurrent transform at ≥95% can **start** another run after wrapup’s wait if timing overlaps; only DB lease + `activeRuns` prevent double publish. Worse: transform **stops blocking after 120s** while wrapup may need many minutes—user send can proceed with incomplete drain unless wrapup holds the send path exclusively.
- **Evidence**: `startCompartmentAgent` no-ops if `activeRuns` has entry; lease `BEGIN IMMEDIATE` at publish; timeout proceeds at 397-405.
- **Suggested Fix**: Process-local `wrapupDrainInProgress` (or extend lease metadata) + block transform from **starting** new historians during wrapup; reactive phase-2 must run on same gate as user message send.

## Finding 6: Pi parity: emergency path awaits one historian, 30s cap, no proactive downswitch drain
- **Severity**: HIGH
- **Location**: `packages/pi-plugin/src/context-handler.ts:2036-2082,2818-2852`; contrast `packages/plugin/src/hooks/magic-context/transform.ts:684-741`
- **Confidence**: high
- **Issue**: Pi mirrors OpenCode’s **single** in-flight wait (`withTimeout(histPromise, 30_000)`), not a loop. `spawnPiHistorianRun` fires one background run. Pi **clears** emergency recovery on model change (`1738`) and has **no** proactive `lastInputTokens > catalog limit` arm—phase-2 reactive drain on Pi is unspecified in code and cannot match spec without new Pi orchestration and longer blocking than 30s.
- **Suggested Fix**: Pi-specific wrapup loop wrapper with explicit multi-run budget; add proactive switch arming or document Pi as v1 OpenCode-only; do not assume `runPiHistorian` loop equals OpenCode `startCompartmentAgent` loop without abort/session.send gating.

## Finding 7: `/ctx-flush` suggestion is misleading for deferred wrapup output
- **Severity**: MEDIUM
- **Location**: `execute-flush.ts:10-31`; `hook.ts:752-757`; `transform-postprocess-phase.ts:1179-1223`
- **Confidence**: high
- **Issue**: `/ctx-flush` only drops **pending tag ops** and sets refresh/materialize signals—it does **not** apply deferred compaction markers or inject `<session-history>` from new compartments alone. Wrapup publishes compartments + pending marker blob; until a **materializing** transform pass drains `applyDeferredCompactionMarker`, OpenCode’s `filterCompacted` boundary may not advance—flush does not fix “I wrapped up but still see raw tail.”
- **Evidence**: `onFlush` adds `historyRefreshSessions` + `pendingMaterializationSessions`; marker drain is conditional on `historyWasConsumedThisPass && deferredHistoryWasPendingAtPassStart` with pending blob.
- **Suggested Fix**: User messaging: suggest “send another message” or document that flush ≠ marker drain; optionally add wrapup completion hook that seeds deferred sets without busting cache against spec.

## Finding 8: Single-slot pending compaction marker — benign for ordinal advance, fragile for concurrent consumers
- **Severity**: MEDIUM
- **Location**: `storage-meta-persisted.ts:1818-1827`; `compaction-marker-manager.ts:139-183,204-221`; `transform-postprocess-phase.ts:1192-1200` (CAS-lost-newer-pending)
- **Confidence**: medium
- **Issue**: Eight wrapup publishes overwrite one pending blob; **last** publish’s `(ordinal, endMessageId)` should subsume earlier targets if monotonic. **Risk**: if a materializing pass reads blob N and publish N+1 overwrites before CAS-clear, code intentionally preserves newer pending (`cas-lost-newer-pending`)—fine. No mid-loop reader of pending between publishes in current code. `validatePendingTarget` still passes for last target if compartments appended in order.
- **Evidence**: `existingMarkerAlreadyCoversTarget`; incremental only writes pending in publish tx (`671-676`).
- **Suggested Fix**: Non-issue for v1 if wrapup blocks other publishers; document invariant; optional queue if recomp interleaves (recomp **direct-applies** marker and CAS-clears stale pending at `compartment-runner-recomp.ts:326-330`).

## Finding 9: `recordProtectedTailPublicationFloor` + shared meta — consistent for wrapup, recomp collision differs
- **Severity**: MEDIUM
- **Location**: `storage-meta-persisted.ts:341-354`; `protected-tail-boundary.ts:479-481`; `compartment-runner-recomp.ts:314-330`
- **Confidence**: medium
- **Issue**: Floor is `MAX(existing, floor)` and reset `recovery_no_eligible_head_count`—wrapup advancing floor each publish aligns with normal `migrationFloorActive`. **Interleave risk**: `/ctx-recomp` replaces compartments, direct marker update, clears stale pending—running recomp during wrapup can invalidate deferred targets (`validatePendingTarget` → stale-skip) while wrapup compartments partially exist.
- **Suggested Fix**: Hard reject `/ctx-recomp` and incremental trigger while wrapup drain flag set; mirror recomp’s lease skip messaging.

## Finding 10: Fingerprint / growing tail — safe if keep watermark fixed at invocation
- **Severity**: LOW (design constraint)
- **Location**: `read-session-true-raw-tokens.ts:661-672`; `protected-tail-boundary.ts:689-765`; `compartment-runner-incremental.ts:252-274`
- **Confidence**: high
- **Issue**: Fingerprint hashes `[offset, eligibleEndOrdinal)`. Appends **above** fixed `eligibleEndOrdinal` do not change the range hash. **Failure mode**: if wrapup recomputes a **wider** eligible end each iteration without fixing invocation anchor, or user edits messages **inside** the eligible range, `validateBoundarySnapshot` fails (`stale_snapshot` / fingerprint).
- **Suggested Fix**: Spec’s “keep watermark re-anchored at invocation” must pin `protectedTailStart` / `eligibleEndOrdinal` for all iterations; refresh only `offset` via DB `lastCompartmentEnd+1`.

## Finding 11: No-op guard must not use raw `getRawHistoryEligibility` alone
- **Severity**: MEDIUM
- **Location**: `protected-tail-boundary.ts:657-671`; spec “meaningful messages newest-first”
- **Confidence**: high
- **Issue**: `hasRawBeyondLastCompartment` is `rawMessageCount >= offset`—counts **all** messages including noise/empty user rows. Spec no-op is “tail already within keep-N **meaningful**”—needs same counting as watermark builder (`hasMeaningfulUserText`, arc snap), not eligibility helper alone.
- **Suggested Fix**: Implement no-op via wrapup boundary resolver comparing meaningful count above last compartment to `messages_to_keep`.

## Finding 12: `startCompartmentAgent` silently no-ops if `activeRuns` already set — wrapup “wait then continue” must await promise, not call start
- **Severity**: MEDIUM
- **Location**: `compartment-runner.ts:104-107`
- **Confidence**: high
- **Issue**: If wrapup calls `startCompartmentAgent` while a run is active, it **returns without starting**. Loop must `await getActiveCompartmentRun().promise` then start next iteration with updated `boundarySnapshot` override.
- **Suggested Fix**: Orchestrator pattern: wait → re-resolve offset → build new snapshot → start (or direct `runCompartmentAgent` with lease like recomp).

---

## Summary
| Severity | Count |
|----------|-------|
| BLOCKER   | 2 |
| HIGH      | 4 |
| MEDIUM    | 5 |
| LOW       | 1 |

**Per-finding verdict (short):**  
1 BLOCK · 2 BLOCK · 3 HIGH · 4 HIGH · 5 HIGH · 6 HIGH · 7 MEDIUM · 8 MEDIUM (mostly safe) · 9 MEDIUM · 10 LOW (constraint) · 11 MEDIUM · 12 MEDIUM  

**Overall verdict: BLOCK**

**Drivers:** (1) Phase-2 / large→small cannot be made correct by reusing the 95% **single-await, 120s-capped** path—needs a first-class multi-run blocking orchestrator on the send path. (2) Incremental publish **`clearEmergencyRecovery`** and discard-last coupling will corrupt multi-iteration wrapup behavior unless the runner gains explicit wrapup/drain mode. (3) Pi **30s / one-shot** emergency path and missing proactive switch arm make “day one Pi parity” for the same loop **not** achievable without additional Pi design in the spec.