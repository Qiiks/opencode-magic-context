# Council Synthesis — Blind Design Review of `/ctx-wrapup` spec

**Intent:** AUDIT · **Members:** 6 (Opus 4.8, GPT 5.4 high, GPT 5.5 xhigh, XAI Composer 2.5, Ollama GLM 5.2, Gemini Flash 3.5 high) · **Agreement:** strong · **Confidence:** high

**Overall verdict: REVISE — with the phase-2 reactive downswitch arm BLOCKED until its blockers are resolved.**

Vote tally: 3× BLOCK (GPT 5.4, GPT 5.5, XAI Composer), 3× REVISE (Opus, GLM 5.2, Gemini). The split is not disagreement about the code — it is disagreement about scoping. Every member that said REVISE explicitly BLOCKS the phase-2 reactive arm and clears the *deliberate* command only after the same fixes the BLOCK voters demand. Every member that said BLOCK agrees the deliberate command is salvageable once those fixes land. **The unified reading: the deliberate `/ctx-wrapup [messages_to_keep]` command is a sound v1 after ~5 runner/orchestration fixes; the phase-2 reactive model-switch reuse is NOT mechanically realizable as specified and must not ship in v1.**

The spec's own three "attack this" cache-safety questions (multi-publish burst, boundary floor leakage, fingerprint across the loop) were all cleared as **non-issues on OpenCode** by unanimous independent verification — the spec's instincts there were correct. The real defects are everywhere else: the blocking surface, the concurrency model, the discard-last coupling, the drain quota, the emergency-recovery flag, and Pi.

---

## Findings (grouped by confidence)

### UNANIMOUS (6/6)

#### #1: The blocking surface awaits ONE run with a per-run timeout, then ships the oversized prompt — phase-2 reactive arm cannot mechanically close the gap
- **Severity**: Critical (BLOCKER for phase-2)
- **Confidence**: Unanimous (6 members)
- **Members Reported**: Opus, GPT 5.4, GPT 5.5, XAI Composer, GLM 5.2, Gemini
- **Issue**: The spec's phase-2 requires a *blocking drain loop* (many sequential historian runs, "minutes for 553k→272k") to complete before the outgoing request. The only blocking primitive that exists, `awaitCompartmentRun`, races **one** `activeRun.promise` against **one** `historianTimeoutMs` (default 120s) and, on timeout, returns `"timed_out"` and *proceeds without waiting*. It does not loop, re-resolve, or bound a total budget. A ~280k-token gap needs many runs; on timeout the transform continues and sends the un-compacted prompt → the exact provider 400 the arm exists to prevent. Multiple members note the loop's own "abort on no forward progress" rule (spec) will fire on a *timed-out-but-still-running* iteration because coverage hasn't advanced yet — conflating "slow" with "stuck."
- **Evidence**: `transform-compartment-phase.ts:224-262` (`awaitCompartmentRun` = `Promise.race([activeRun.promise, timeout])`, `historianTimeoutMs ?? 120_000` at :229); `:390-406` (on `"timed_out"` logs and proceeds); `:319-322` comment explicitly limits blocking to 95% one run; `transform.ts:716-742` (proactive arm only sets a flag + bumps % to 95, drives no loop); `compartment-runner-incremental.ts:346,449` (one chunk, one pass per run).
- **Impact**: Phase-2 reactive downswitch is non-functional as specified — it will either freeze the user unboundedly or truncate and 400. This is the primary justification for the entire command (gap #1), so failing it guts half the spec.
- **Fix Direction**: Build a first-class blocking drain-loop orchestrator on the send path with an explicit **total** deadline (separate from per-run `historianTimeoutMs`) that, per iteration: awaits the in-flight run's *publication* (not a race), re-resolves the boundary from current state, re-starts the next run, and only aborts on a *completed* run that advanced nothing. Do not extend `awaitCompartmentRun` in place.

#### #2: No loop-wide concurrency guard — the lease and `activeRuns` are released between iterations, so wrapup-vs-wrapup / wrapup-vs-recomp / wrapup-vs-95%-arm can interleave and corrupt state
- **Severity**: Critical
- **Confidence**: Unanimous (6 members)
- **Members Reported**: Opus, GPT 5.4, GPT 5.5, XAI Composer, GLM 5.2, Gemini
- **Issue**: Existing serialization protects a *single historian run*, not a multi-iteration loop. `startCompartmentAgent` acquires the DB lease and registers in `activeRuns` per run, then releases both in `.finally` when each run settles. Between wrapup iterations the map is momentarily empty and the lease is free, opening a window where: (a) a second `/ctx-wrapup` slips in (spec wants a *rejection*, but `startCompartmentAgent` only silently no-ops on an active run — not a user-facing reject, and process-local `activeRuns` doesn't cover a second process); (b) `/ctx-recomp` acquires the lease and runs a **full rebuild from message 1** (`DELETE FROM compartments`), destroying wrapup's in-progress compartments, eagerly clearing the injection cache, and moving the marker — wrapup's next iteration re-resolves from a wiped state; (c) a concurrent transform pass at ≥95% force-starts its own run in the gap.
- **Evidence**: `compartment-runner.ts:104-107` (`startCompartmentAgent` no-ops if `activeRuns.get` set — silent, not a reject; process-local map at :30); `:145-152` (lease + activeRuns cleared per-run in finally); `:192-202` (recomp only checks `activeRuns.has` at command start); `compartment-runner-recomp.ts:110` (`DELETE FROM compartments`), `:265-267` (eager cache clear); recomp holds its run/lease across its whole multi-pass op (contrast :204-240). Lease itself is a correct cross-process authority (`compartment-lease.ts:13-38`, re-checked under `BEGIN IMMEDIATE` at `compartment-runner-incremental.ts:606-616`) — but only per run.
- **Impact**: Duplicate compartments, sequence collisions ("UNIQUE constraint failed"), destroyed coverage, marker desync — genuine DB corruption under realistic double-invocation / concurrent-command timing.
- **Fix Direction**: Introduce a session-scoped `wrapup_in_progress` guard set under `BEGIN IMMEDIATE` (cross-process, race-free) for clean rejection of a second wrapup; hold the compartment lease for the ENTIRE loop (or register a placeholder in `activeRuns` for the loop's duration) so recomp/upgrade/second-wrapup/95%-arm cannot start between iterations; make recomp and wrapup each check the other's guard.

#### #3: "Discard-last OFF for the final chunk" has no runner mechanism AND would durably promote facts/events/primers off a boundary the runner itself calls "structurally unreliable"
- **Severity**: High
- **Confidence**: Unanimous (6 members)
- **Members Reported**: Opus, GPT 5.4, GPT 5.5, XAI Composer, GLM 5.2, Gemini
- **Issue**: Two coupled problems. (1) **No plumbing**: discard-last is decided internally by `!inEmergency && emittedCompartments.length >= 2 && lookaheadMargin <= BOUNDARY_HEALING_SLACK`; `CompartmentRunnerDeps` has no field to force it off. (2) **`discardedLast` is the gate that suppresses durable promotion** — facts (`promotionActive && !discardedLast`), events (filtered by persisted range), user observations, and primers are all skipped/filtered when discard-last drops the tail. Forcing discard-last OFF on the final chunk keeps a compartment whose boundary the runner's own comments call "structurally unreliable" (no lookahead) AND durably promotes its facts/events/primers — precisely the double-promotion the heuristic exists to prevent. Opus and GPT 5.5 add a sharper corollary for the **reactive path**: it runs while `needsEmergencyRecovery` is armed, so `inEmergency` is true and discard-last is *already OFF for every iteration* — the spec's "earlier iterations keep discard-last" is already false there.
- **Evidence**: `compartment-runner-incremental.ts:495-509` (discard-last gated on `inEmergency` at :496 + lookahead; no dep); `:566` (`discardedLast` computed), `:632` (`promotionActive && !discardedLast` fact gate), `:589-591` (event filter), `:795-799` (observation gate), `:824-829` (primer gate); `:481-494,558-566` comments ("structurally unreliable," double-up risk); `compartment-runner-types.ts` deps have no discard-last override.
- **Impact**: Corrupted/duplicated project memories and primers; a weak final boundary shifts where the next run re-derives (`offset = lastCompartmentEnd+1`). Gemini's minority position: the quality cost isn't worth it — keep discard-last on for all iterations since the last 2-3 messages are within the keep watermark anyway.
- **Fix Direction**: Add an explicit `forceKeepLastCompartment` / `disableDiscardLast` dep, and **separate "persist final coverage" from "promote durable artifacts"** — the safer choice most members converge on is: keep the final compartment for coverage but STILL skip fact/event/primer promotion for it. On the reactive path, decide `inEmergency` semantics deliberately rather than inheriting them.

#### #4: Pi "day-one parity" via loop-wrapping is mechanically unsound — 30s single-await cap, single-slot in-flight map, no abort surface, fire-and-forget spawn
- **Severity**: High (BLOCKER for Pi reactive parity)
- **Confidence**: Unanimous (6 members)
- **Members Reported**: Opus, GPT 5.4, GPT 5.5, XAI Composer, GLM 5.2, Gemini
- **Issue**: The spec says Pi uses "the same loop, same boundary override, day one," wrapping `runPiHistorian` "the same way its emergency drain does." But Pi's emergency drain awaits ONE in-flight historian with a hard `withTimeout(histPromise, 30_000)` cap and does not loop; `inFlightHistorian` is a **single-slot** map (iteration N+1 overwrites N's promise); `spawnPiHistorianRun` is fire-and-forget (stores the promise, doesn't await); and Pi has **no `session.abort` surface** ("We can't `client.session.abort()` mid-pass"). A multi-run loop with a 30s-per-run cap on a 280k gap times out after run 1 and sends the oversized prompt → guaranteed provider 400. GPT 5.5 adds Pi *clears* emergency recovery on model change and has *no* proactive `lastInputTokens > limit` arm at all, so phase-2 on Pi is unspecified in code. (Members confirm Pi's `pi.on("context")` handler IS async and awaited before the LLM call, so a blocking loop is *possible* — but needs a new primitive, not the emergency drain.)
- **Evidence**: `context-handler.ts:2056-2082` (30s cap `withTimeout(histPromise, 30_000)` at :2074, one in-flight wait); `:2044-2050` ("can't session.abort mid-pass"); `:2818-2926` (`spawnPiHistorianRun` fire-and-forget, `inFlightHistorian.set`); single-slot `inFlightHistorian` map.
- **Impact**: Pi reactive downswitch guaranteed to 400; even the deliberate Pi command needs a new blocking-loop primitive.
- **Fix Direction**: Do not ship Pi reactive parity in v1. Build a dedicated Pi blocking drain-loop primitive with a multi-run total budget (not 30s), per-run tracking (not single-slot), and awaited `runPiHistorian` re-spawns; or gate Pi to the deliberate command only with a bounded iteration count under Pi's pipeline timeout.

### MAJORITY

#### #5: The runner's stale-snapshot self-heal silently DISCARDS the wrapup boundary override and re-resolves with normal pressure math
- **Severity**: High
- **Confidence**: Majority (4 members: Opus, Gemini explicit; GPT 5.5, GPT 5.4 via the reactive limit-change variant)
- **Members Reported**: Opus, Gemini, GPT 5.5, GPT 5.4
- **Issue**: The spec's load-bearing premise is that each iteration is handed an explicit widened boundary. But when the passed snapshot validates as `stale_snapshot` (e.g. the user chatted mid-wrapup and the `last`-ordinal id moved), the runner re-resolves the boundary *itself* by calling `resolveOpenCodeProtectedTailBoundary(mode:"incremental-runner")` — normal token-target/live-prompt-floor/0.40×usable math — and, if runnable, adopts it (`boundarySnapshot = refreshed`). This throws away the keep-watermark eligible-end and compacts an incremental-sized head instead, silently defeating the command. GPT 5.4/Opus surface the reactive-arm variant: a limit change returns `model_or_limit_changed` (NOT `stale_snapshot`), which isn't the healed reason, so the reactive iteration falls straight through to a **no-op** unless the wrapup builds its snapshot with `contextLimit = new-model limit` and passes a matching `currentContextLimit`.
- **Evidence**: `compartment-runner-incremental.ts:252-274` (self-heal re-resolves with `mode:"incremental-runner"`, adopts `refreshed`); `:230-238,275-283` (non-`stale_snapshot` → no-op); `protected-tail-boundary.ts:695-701` (`model_or_limit_changed` branch).
- **Impact**: Deliberate wrapup silently under-compacts on any mid-wrapup edit/chat that busts the `last`-id; reactive wrapup no-ops on the limit change unless snapshot limits are set exactly.
- **Fix Direction**: Thread a `refreshBoundarySnapshot` callback into `CompartmentRunnerDeps` (Pi already has this pattern in `spawnPiHistorianRun`) so the self-heal re-applies the *wrapup watermark*, not pressure math; build reactive snapshots with the new limit as `contextLimit` + matching `currentContextLimit`; add a test asserting a downswitch snapshot validates rather than no-ops.

#### #6: `clearEmergencyRecovery` fires on EVERY successful publish — the first chunk clears the overflow flag mid-drain
- **Severity**: High
- **Confidence**: Majority (4 members: GPT 5.4, GPT 5.5, XAI Composer, GLM 5.2)
- **Members Reported**: GPT 5.4, GPT 5.5, XAI Composer, GLM 5.2
- **Issue**: The runner assumes one successful publish completes overflow recovery and unconditionally calls `clearEmergencyRecovery` inside every publish transaction. In a multi-run reactive downswitch drain, chunk 1 publishing clears `needs_emergency_recovery`; if chunk 2 later fails/times out before the new-model target, the next user send won't auto-block/retry even though the session is still too large → overflow. XAI/GPT 5.5 add the coupling to Finding #3: because `inEmergency` is read per-run, clearing it after iteration 1 means iteration 2+ silently re-enables discard-last mid-drain.
- **Evidence**: `compartment-runner-incremental.ts:663-669` (`clearEmergencyRecovery` in the publish tx, comment "overflow recovery is complete"); `:496-498` (`inEmergency` read once per run gates discard-last).
- **Impact**: Reactive drain can leave the session overflowing with recovery disarmed; discard-last policy flips unpredictably between iterations.
- **Fix Direction**: Add a `wrapupDrainMode` dep that suppresses `clearEmergencyRecovery` until the orchestrator reaches the target watermark; re-arm on partial failure; drive discard-last policy from the orchestrator (off only on the true final iteration), not from `inEmergency`.

#### #7: The normal protected-tail drain quota can stop a low-pressure manual `/ctx-wrapup` before the keep watermark
- **Severity**: High (BLOCKER per GPT 5.4/GLM)
- **Confidence**: Majority (2 members strongly; consistent with the reserve path all members read)
- **Members Reported**: GPT 5.4, GLM 5.2
- **Issue**: The spec promises "no hard iteration cap," but every incremental run passes through `reserveProtectedTailDrainTokens`. Outside the ≥95% emergency latch the per-10-minute-window budget is finite, so a low-pressure manual wrapup (the deliberate "compact now" use case) can no-op with "protected-tail drain quota exhausted" long before reaching the watermark on a large tail.
- **Evidence**: `compartment-runner-incremental.ts:325-343` (`reserveProtectedTailDrainTokens`, `!reserve.ok` → no-op exit); `storage-meta-persisted.ts:380-390` (`protectedTailWindowBudget` caps non-emergency drains), `:484-498` (returns not-ok when reserved ≤ 0). Bypass only when emergency latch active.
- **Impact**: The deliberate command silently stalls mid-drain on exactly the large sessions it targets.
- **Fix Direction**: Add an explicit forced-drain mode bypassing the normal window quota (or a separate quota class for wrapup) — without faking emergency usage unless all emergency side effects are audited.

#### #8: `/ctx-flush` does not itself apply wrapup output — it drops pending tag ops and sets signals; materialization needs a consuming transform pass
- **Severity**: Medium
- **Confidence**: Majority (4 members: Opus, GPT 5.5, XAI Composer, Gemini)
- **Members Reported**: Opus, GPT 5.5, XAI Composer, Gemini
- **Issue**: The spec suggests users run `/ctx-flush` to apply the wrapup immediately. But `executeFlush` only marks pending-op tags dropped ("takes effect on next message") — it does NOT move the compaction marker or re-fold m0. The deferred marker drain runs in the transform postprocess phase, gated on `historyWasConsumedThisPass && deferredHistoryWasPendingAtPassStart`. `onFlush` sets the refresh/materialization signals so the *next* transform pass drains the marker — so flush + a subsequent message works, but nothing reduces context synchronously, and if the user sends a normal non-busting message the marker may not move (Gemini's sharper claim). Members split on whether this "works eventually" (Opus/GPT 5.4 verified the signals wire up) vs "misleading UX" — the honest reading: flush ≠ immediate reduction.
- **Evidence**: `execute-flush.ts:10-36` (tag drops only); `hook.ts:752-758` (`onFlush` sets `historyRefreshSessions`+`pendingMaterializationSessions`); `transform-postprocess-phase.ts:1179-1214` (marker drain gated on `historyWasConsumedThisPass`); publish defers marker via `onDeferredMarkerPending` (`compartment-runner-incremental.ts:708-709`).
- **Impact**: A "compact now" control that shows no context-% drop until a later bust; the flush suggestion overpromises.
- **Fix Direction**: Report coverage as "queued; materializes on your next message" — never as an achieved reduction. Either drop the flush suggestion (materialization rides the next natural bust, per the spec's own model-switch-HARD-fold path) or document that flush applies on the *next* message, not immediately. Optionally have wrapup seed the materialization signals itself.

#### #9: The no-op guard must count MEANINGFUL messages, not the raw-ordinal delta from `getRawHistoryEligibility`
- **Severity**: Medium/Low
- **Confidence**: Majority (3 members: Opus, GPT 5.5, XAI Composer)
- **Members Reported**: Opus, GPT 5.5, XAI Composer
- **Issue**: The spec's watermark and no-op guard both count meaningful messages newest-first, but `getRawHistoryEligibility` exposes only raw-ordinal counts (tool-only, empty, system-directive messages included). Using the raw delta for the guard while the watermark uses meaningful counting makes them disagree at the boundary: a tool-heavy tail can falsely no-op, a user-heavy tail can trigger an empty LLM pass. No existing helper counts "meaningful messages above ordinal X" — `hasMeaningfulUserText` exists but isn't exposed that way.
- **Evidence**: `protected-tail-boundary.ts:657-672` (`getRawHistoryEligibility` returns raw `rawMessageCount`/`offset`, no meaningful filter); contrast the meaningful-user scan at `:497-509`; `read-session-formatting.ts` `hasMeaningfulUserText`.
- **Fix Direction**: Implement a dedicated `countMeaningfulMessagesAboveOrdinal` helper reusing `hasMeaningfulUserText` + `fenceBoundaryForToolArcs` for the outward snap; use it for BOTH the watermark and the no-op guard.

### SOLO (single-member, verified against code — lower confidence but concrete)

#### #10: Pi's deferred-marker clobber is NOT benign — Pi's pending blob carries chunk-local `summary` + `tokensBefore`, not just an ordinal
- **Severity**: Medium
- **Confidence**: Solo (GLM 5.2)
- **Members Reported**: GLM 5.2
- **Issue**: Unlike OpenCode's ordinal/id-only pending blob (where single-slot overwrite is benign because the marker only advances monotonically), Pi's `PendingPiCompactionMarker` also stores `summary` and `tokensBefore`. Sequential deferred wrapup publishes overwrite the single Pi blob, so the final native Pi compaction covers all earlier chunks but uses only the LAST chunk's summary/tokens — a content mismatch, not just a position collapse.
- **Evidence**: `storage-meta-persisted.ts:1863-1869` (`PendingPiCompactionMarker` includes `summary`,`tokensBefore`), `:1910-1919` (single-slot overwrite); `pi-historian-runner.ts:865-996` (`markerSummary` from current run's compartments only); `compaction-marker-manager-pi.ts:71-80` (drain passes `pending.summary` to `appendCompaction`).
- **Fix Direction**: For Pi wrapup, aggregate summaries/tokens across the loop or recompute the pending marker from all compartments since the previous native boundary.

#### #11: `recompProgressBySession` is single-slot and the incremental runner never emits progress — reusing the recomp surface needs new plumbing + a `wrapup` kind
- **Severity**: Medium/High
- **Confidence**: Solo (GPT 5.5; Opus/others noted the reuse without the emit gap)
- **Members Reported**: GPT 5.5
- **Issue**: The spec reuses the recomp progress surface for per-chunk wrapup rows, but `runCompartmentAgent` / the incremental path never calls `onRecompProgress` (only the recomp runner wires it). The map is a single entry per session keyed by sessionId with kind `recomp|upgrade|embed` — no `wrapup` — so a concurrent recomp and wrapup overwrite each other's row.
- **Evidence**: `recomp-orchestrator.ts:145-162` (single-slot `recompProgressBySession`); `compartment-runner-recomp.ts:225-237` (only recomp wires `onRecompProgress`); `compartment-runner-types.ts:105`.
- **Fix Direction**: Add `kind:"wrapup"`, emit progress from the loop orchestrator (not inside a single runner call), and reject a second wrapup/recomp while `phase !== done|failed`.

---

## Summary table

| # | Finding | Severity | Agreement | Members |
|---|---------|----------|-----------|---------|
| 1 | Single-run+timeout blocking surface → phase-2 ships oversized prompt | Critical | Unanimous | all 6 |
| 2 | No loop-wide concurrency guard (lease released between iterations) | Critical | Unanimous | all 6 |
| 3 | Discard-last OFF: no mechanism + promotes weak-boundary durable facts | High | Unanimous | all 6 |
| 4 | Pi loop-wrap unsound (30s cap, single-slot, no abort) | High | Unanimous | all 6 |
| 5 | Stale-snapshot self-heal discards the wrapup boundary override | High | Majority | Opus, Gemini, GPT5.5, GPT5.4 |
| 6 | `clearEmergencyRecovery` on every publish clears flag mid-drain | High | Majority | GPT5.4, GPT5.5, XAI, GLM |
| 7 | Normal drain quota stops low-pressure manual wrapup | High | Majority | GPT5.4, GLM |
| 8 | `/ctx-flush` doesn't apply output synchronously | Medium | Majority | Opus, GPT5.5, XAI, Gemini |
| 9 | No-op guard must count meaningful messages | Medium | Majority | Opus, GPT5.5, XAI |
| 10 | Pi pending blob carries summary+tokens — clobber not benign | Medium | Solo | GLM |
| 11 | `recompProgressBySession` single-slot; incremental never emits | Medium | Solo | GPT5.5 |

---

## Dismissed as non-issues (the spec's three "attack this" cache questions — verified SAFE on OpenCode by all members who checked)

- **Multi-publish burst / single-slot pending marker (spec Q-a)**: The whole wrapup loop is synchronous within one call — no transform pass drains between iterations, so 8 blobs collapse to the last. The marker advances monotonically (`existingMarkerAlreadyCoversTarget`), the last blob's ordinal matches the last published compartment, and `validatePendingTarget` passes. Intermediate compartments ARE persisted (coverage not lost). **Benign on OpenCode** (unanimous). *Caveat:* NOT benign on Pi — see Finding #10.
- **Boundary-floor leakage (spec Q-b)**: `recordProtectedTailPublicationFloor` writes `prior_boundary_ordinal = MAX(existing, lastCompartmentEnd+1)`; later normal passes read `runtimeFloor = max(offset, priorBoundaryOrdinal)` and `offset` already equals `lastCompartmentEnd+1`, so the floor never over-protects. **Benign** (unanimous). Minor: the same call resets `recovery_no_eligible_head_count=0` per publish — a narrow, low-severity race that could suppress a concurrent no-head-escape notification (GLM/reviewer note), correct behavior for a successful publish.
- **Fingerprint across a growing tail (spec Q-c)**: `computeRawRangeFingerprint` hashes `[offset, eligibleEnd)`; appended messages land ABOVE `eligibleEnd`, so the range hash is unchanged, and `validateBoundarySnapshot` rejects only on *shrink*, not growth. **Safe as long as** the wrapup override recomputes `offset = lastCompartmentEnd+1` from CURRENT store state each iteration and does NOT cache offset across iterations, and the eligible range's `protectedTailStart`/`eligibleEndOrdinal` are pinned at invocation. A mid-range message *edit* busts the fingerprint and triggers the stale re-resolve — which is Finding #5's real risk, not a fingerprint failure.
- **Trusted-limit availability for the reactive arm (spec Q-5)**: `getSdkContextLimit` is a synchronous catalog read already used by the #188 proactive arm, so the new model's cap is resolvable at drain time without a provider round-trip, and `messages.transform` is genuinely awaited by OpenCode. *But* GPT 5.4 flags a real gap: for UNKNOWN/new models the trusted limit is intentionally `undefined` until an overflow is observed — phase-2 must specify fallback behavior (disable proactive wrapup with an honest message, or use a configured limit).

---

## Priority recommendations

**BLOCK the phase-2 reactive model-switch arm** until a first-class blocking drain-loop orchestrator exists (Finding #1) with a total budget, publication-await-not-race progress, correct `model_or_limit_changed` snapshot handling (#5), deferred `clearEmergencyRecovery` (#6), and a defined unknown-model fallback. Pi reactive parity (#4, #10) is a separate substantial design — do not claim "day one."

**Before shipping even the deliberate `/ctx-wrapup` command, land:**
1. Loop-wide concurrency guard + whole-loop lease (#2) — corruption risk, non-negotiable.
2. Forced-drain quota bypass (#7) — else the command stalls on its target sessions.
3. `forceKeepLastCompartment` dep that keeps coverage but skips promotion for the weak final compartment (#3).
4. `refreshBoundarySnapshot` callback so the stale re-resolve honors the wrapup watermark (#5).
5. Await in-flight run's promise before each iteration; don't treat "a run was active" as no-progress (#1/#2 corollary).
6. Meaningful-message no-op guard (#9); honest deferred-reduction reporting, drop/clarify the flush suggestion (#8).
7. `wrapup` progress kind emitted from the orchestrator (#11).

**Net:** the engine reuse is a good instinct and the cache-safety analysis was sound, but "reuse, don't rebuild" understates the work — the blocking loop, the loop-wide guard, the discard-last/promotion split, the quota bypass, and the boundary-override-survives-restale are all NEW surfaces, not reuse. Ship the deliberate command after the fixes above; block the reactive arm and Pi parity for a dedicated follow-up.
