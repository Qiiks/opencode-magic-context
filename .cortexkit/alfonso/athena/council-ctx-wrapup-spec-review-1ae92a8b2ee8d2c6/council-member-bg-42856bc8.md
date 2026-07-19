# Blind Design Review: `/ctx-wrapup` spec

Verified independently against the real code on branch `subc-migration`. `/ctx-wrapup` does not yet exist (grep for `wrapup` → 0 matches), so this reviews the spec against the engine it proposes to reuse.

---

## Finding 1: Runner's stale-snapshot self-heal silently discards the WRAPUP BOUNDARY OVERRIDE
- **Severity**: HIGH
- **Location**: `compartment-runner-incremental.ts:252-274`
- **Confidence**: high
- **Issue**: The spec's load-bearing premise is that each iteration is handed an explicit "WRAPUP BOUNDARY OVERRIDE … not resolveProtectedTailBoundary's normal pressure math." But the runner validates the passed snapshot (`validateBoundarySnapshot`, line 230-238) and, on a `stale_snapshot` result, **re-resolves the boundary itself** by calling `resolveOpenCodeProtectedTailBoundary(...)` (line 253) — i.e. normal token-target/live-prompt-floor/0.40× pressure math. If it produces a runnable window, the runner adopts it (`boundarySnapshot = refreshed`, line 271), throwing away the wrapup's widened eligible-end. The drain then compacts an incremental-sized head, not the keep-watermark head — quietly defeating the whole command.
- **Evidence**: line 253 `resolveOpenCodeProtectedTailBoundary({... mode:"incremental-runner" ...})`; line 271 `boundarySnapshot = refreshed`. There is no override plumbed through this path.
- **Suggested Fix**: Add a `deps.boundaryOverride`/`skipReResolve` flag the wrapup sets, so the runner either fails closed or re-applies the wrapup watermark (not pressure math) when a wrapup snapshot goes stale.

## Finding 2: Reactive arm no-ops every iteration — `model_or_limit_changed` isn't the healed reason
- **Severity**: HIGH
- **Location**: `protected-tail-boundary.ts:695-701` + `compartment-runner-incremental.ts:230-283`
- **Confidence**: high
- **Issue**: The reactive (phase-2) arm runs precisely *after* a model/limit change and must size to the NEW limit. `validateBoundarySnapshot` returns `{reason:"model_or_limit_changed"}` whenever `currentContextLimit !== snapshot.contextLimit` (line 695). The runner's self-heal only fires for `reason === "stale_snapshot"` (line 252). So a `model_or_limit_changed` result falls straight through to line 275 and the runner **no-ops** (`telemetry.status="noop"`). Unless the wrapup builds its snapshot with `contextLimit = new-model limit` AND passes `currentContextLimit` equal to it, every reactive iteration is a no-op → coverage never advances → the loop's own no-progress abort (spec) fires → oversized prompt still sent → provider 400. The spec never calls out this limit-equality requirement.
- **Evidence**: line 695 branch returns non-`stale_snapshot`; line 252 guard `validation.reason === "stale_snapshot"`; line 275-283 no-op.
- **Suggested Fix**: Wrapup must construct the snapshot with the new limit as `contextLimit` and pass matching `currentContextLimit`; add a test asserting a downswitch snapshot validates rather than no-ops.

## Finding 3: Per-run timeout + "abort on no forward progress" = oversized prompt still ships
- **Severity**: HIGH
- **Location**: `transform-compartment-phase.ts:224-240` (awaitCompartmentRun, `historianTimeoutMs ?? 120_000`) vs spec's "abort the loop if an iteration makes NO forward progress"
- **Confidence**: high
- **Issue**: The existing blocking machinery awaits ONE run with a per-run timeout; on timeout it returns `"timed_out"` and leaves the run going in the background (line 234-240, 397-406). If the wrapup loop reuses this and an iteration times out, the loop observes *coverage unchanged* (the background run hasn't published yet) and, per the spec's abort rule, **stops** — while the reactive arm's entire justification is "correct beats a provider 400." A slow historian thus produces exactly the 400 the arm exists to prevent, for the 553k→272k case that needs many minutes of sequential runs.
- **Evidence**: line 229 default 120s; line 234 `result==="timeout"` → `"timed_out"`; spec conflates a timed-out run with a genuinely stuck one.
- **Suggested Fix**: Distinguish "timed out (still running)" from "published no new compartment." Wait for the in-flight run's publication (await `activeRun.promise`) before evaluating progress; only abort on a *completed* run that advanced nothing. Give the loop a multi-run budget, not a per-run 120s cap.

## Finding 4: Pi loop-wrapping is mechanically unsound (30s cap, single-slot in-flight, no abort)
- **Severity**: HIGH
- **Location**: `context-handler.ts:2071-2082` (30s `withTimeout`), `:2613` `inFlightHistorian = new Map<string, Promise<unknown>>()`, `:2818` `spawnPiHistorianRun` (fires ONE background run)
- **Confidence**: high
- **Issue**: "Pi parity … day one" wraps `runPiHistorian` in a blocking loop, but (a) `inFlightHistorian` is a **single-slot** map — iteration N+1 overwrites N's promise, so any emergency wait awaits the wrong entry; (b) the emergency path caps waits at **30s** (`withTimeout(histPromise, 30_000)`), implying Pi's handler is not designed to block for the multi-minute drain the reactive arm requires; (c) Pi has **no `session.abort` surface** (comment at line 2043-2045), so a runaway loop can't be interrupted. A minutes-long blocking loop inside `pi.on("context")` risks stalling or being truncated by Pi's own pipeline budget.
- **Evidence**: lines 2044-2045 ("We can't `client.session.abort()`"), 2051-2054 (30s cap rationale), 2613 single-slot map, 2818-2828 one-shot spawn.
- **Suggested Fix**: Do not ship Pi reactive parity in v1. If wrapped, replace single-slot `inFlightHistorian` with per-run tracking and confirm Pi awaits the context handler with a budget that tolerates the loop; otherwise gate Pi to the deliberate command only, or a bounded iteration count well under any pipeline timeout.

## Finding 5: "discard-last OFF for the FINAL chunk" has no mechanism and conflicts with the emergency gate
- **Severity**: MEDIUM
- **Location**: `compartment-runner-incremental.ts:496-509` (`inEmergency = getOverflowState(...).needsEmergencyRecovery`; discard-last skipped when `inEmergency`), promotion gates at 632, 589, 795-800, 824
- **Confidence**: high
- **Issue**: Two problems. (1) There is **no deps flag** to force discard-last off — it's decided internally by `!inEmergency && emittedCompartments.length >= 2`. The spec's "for the FINAL loop iteration only, persist all validated compartments" cannot be expressed without new plumbing. (2) On the **reactive** path the loop runs while `needs_emergency_recovery` is armed, so `inEmergency` is true and discard-last is already OFF for **every** iteration (line 496-498), directly contradicting the spec's "Earlier iterations keep discard-last." Net effect: every reactive chunk promotes facts/events/primers off a weak final boundary, not just the final one.
- **Evidence**: line 496 `inEmergency = getOverflowState(...)`; line 498 gate; promotion gated by `!discardedLast` at 632/589/795/824.
- **Suggested Fix**: Add an explicit `deps.persistFinalCompartment` flag threaded to the discard-last block; on the reactive path decide deliberately (accept all-off, or clear the emergency flag for wrapup) rather than inheriting `inEmergency` semantics by accident.

## Finding 6: wrapup-vs-wrapup / vs-recomp guard can't rely on `activeRuns` — it's empty between iterations
- **Severity**: MEDIUM
- **Location**: `compartment-runner.ts:104-107` (startCompartmentAgent bail), `:192-201` (recomp `activeRuns.has` reject), `:145-151`/`:64-70` (activeRuns cleared on each run settle)
- **Confidence**: high
- **Issue**: `activeRuns` tracks a single historian RUN, cleared in `.finally` when each run settles (line 148, 67). Between wrapup iterations the map is momentarily empty, so it **cannot** serialize the LOOP. A second `/ctx-wrapup` (spec says "reject the second") or a concurrent `/ctx-recomp` can slip into that gap; recomp does a **full rebuild from message 1** (`executeContextRecompInternal`) that resequences/replaces the compartments the wrapup just published — invalidating its pending target (`validatePendingTarget` → `compartment-removed`) and its no-progress math mid-loop. The spec names no dedicated latch.
- **Evidence**: line 192 `if (activeRuns.has(sessionId))`; line 148/67 clear-on-settle; recomp replaces all compartments (compartment-runner.ts:216-218).
- **Suggested Fix**: Introduce a synchronous per-session `wrapupInProgress` Set set at command entry; make recomp/upgrade/second-wrapup check it, and have wrapup check the recomp guard, so neither starts while the other's multi-step sequence is live.

## Finding 7: Reusing `startCompartmentAgent` per iteration silently no-ops against an in-flight background historian
- **Severity**: MEDIUM
- **Location**: `compartment-runner.ts:100-107`
- **Confidence**: high
- **Issue**: The spec says "if a historian run is already in flight, wait for it … then continue the loop." But `startCompartmentAgent` returns silently when `activeRuns.get(sessionId)` is set (line 104-107). If the wrapup loop calls it while a background historian is mid-run, the iteration is a no-op, coverage is unchanged, and the spec's no-progress abort fires — the wrapup quits instead of waiting.
- **Evidence**: line 104 `const existing = activeRuns.get(...); if (existing) return;`.
- **Suggested Fix**: Wrapup must `await getActiveCompartmentRun(sessionId)?.promise` before each iteration and only then start/await its own run; do not treat "a run was already active" as no-progress.

## Finding 8: "Compact now" is deferred — no synchronous reduction, and /ctx-flush only applies on the next pass
- **Severity**: MEDIUM
- **Location**: `execute-flush.ts:10-31` (drops pending_ops only), `hook.ts:754-758` (onFlush sets 3 signals), deferred marker drain `transform-postprocess-phase.ts:1179-1215`
- **Confidence**: high
- **Issue**: `executeFlush` itself only marks pending-op tags dropped ("Changes take effect on next message") — it does **not** move the compaction marker or re-fold m0. The marker move happens in the transform postprocess drain (line 1180), which needs `historyWasConsumedThisPass && deferredHistoryWasPendingAtPassStart`. `onFlush` does set `historyRefreshSessions + pendingMaterializationSessions` (hook.ts:755-757), so the *next* transform pass will drive the drain — but nothing reduces context synchronously. For the deliberate command with no imminent model switch, the user runs a "compact now" control and sees **no context-% drop** until a later bust; the raw tail stays billed in the live prompt. This is a genuine honesty gap between the command's name/UX and its deferred effect.
- **Evidence**: execute-flush.ts:22-24 (tag drops only); marker drain gated at postprocess-phase.ts:1179; publish defers marker via `onDeferredMarkerPending` (compartment-runner-incremental.ts:708-709).
- **Suggested Fix**: Report coverage as "queued; materializes on next message / run `/ctx-flush`," never as an achieved reduction; consider having wrapup optionally set the flush signals itself so the reduction lands on the immediately-following pass.

## Finding 9: No-op guard must count meaningful messages consistently, not raw-ordinal delta
- **Severity**: LOW
- **Location**: `protected-tail-boundary.ts:657-671` (`getRawHistoryEligibility` returns raw-ordinal counts)
- **Confidence**: medium
- **Issue**: The spec's no-op guard ("tail already within keep-N") and watermark both count MEANINGFUL messages newest-first, but `getRawHistoryEligibility` exposes only raw-ordinal `rawMessageCount - lastCompartmentEnd` (gaps + tool-only messages included). If the guard uses the raw delta while the watermark uses meaningful counting, they disagree at the boundary and a session with many tool messages can either falsely no-op or run an empty LLM pass.
- **Evidence**: line 666-671 returns `rawMessageCount`/`offset`, no meaningful-message filter (contrast the meaningful-user scan at protected-tail-boundary.ts:498-505).
- **Suggested Fix**: Compute the no-op guard from the same meaningful-message counter the watermark uses.

---

## Vectors that are NON-ISSUES (verified safe)

- **Single-slot pending-marker clobber (Athena #1)**: `setPendingCompactionMarkerState` (storage-meta-persisted.ts:1818) overwrites per publish, but the whole wrapup loop is synchronous within one call — no transform pass drains between iterations, so 8 blobs collapse to the LAST. The marker advances monotonically (`existingMarkerAlreadyCoversTarget`, compaction-marker-manager.ts:139-183) and the last publish has the highest ordinal, subsuming all earlier ones. `validatePendingTarget` (manager.ts:84-117) passes because the final compartment exists at that ordinal. **Benign.**
- **Shared publication floor (Athena #2)**: `recordProtectedTailPublicationFloor` writes `prior_boundary_ordinal = MAX(existing, lastCompartmentEnd+1)` (storage-meta-persisted.ts:350). Later normal passes read it as `runtimeFloor = max(offset, priorBoundaryOrdinal)` (protected-tail-boundary.ts:480), and `offset` already equals `lastCompartmentEnd+1`, so the floor never over-protects. **Benign.**
- **Fingerprint vs growing tail (Athena #3)**: `computeRawRangeFingerprint` hashes `[offset, eligibleEnd)` (read-session-true-raw-tokens.ts:661-673). Appended messages land ABOVE `eligibleEnd`, leaving that range's fingerprint unchanged; `validateBoundarySnapshot`'s `last`/count checks tolerate growth (`rawMessageCountAtTrigger > current` only rejects on *shrink*, line 710). Safe **for the fingerprint** — but see Finding 1, where a stale `last`-id triggers the pressure-math re-resolve.
- **Trusted-limit availability for reactive arm (Athena #5)**: `getSdkContextLimit` is a synchronous catalog read (models-dev-cache.ts:329; already used by the #188 proactive arm at transform.ts:714), so the new model's cap IS resolvable at drain time without a provider round-trip. The transform is genuinely awaited (`experimental.chat.messages.transform`, hook.ts:883; the 95% arm already `await`s inside it), so a blocking loop is mechanically possible on OpenCode. (Caveat: OpenCode's own outer hook timeout is not verifiable from this repo.)

---

## Summary
- **HIGH: 4** (Findings 1-4) — 3 defeat the reactive phase-2 arm outright (boundary-override discarded, limit-change no-op, timeout→abort→400) and 1 makes Pi parity unsound.
- **MEDIUM: 4** (Findings 5-8) — discard-last mechanism/gate conflict, loop serialization gaps, silent no-op vs in-flight historian, deferred-reduction honesty.
- **LOW: 1** (Finding 9).
- **Verified non-issues: 5** of Athena's 8 vectors (single-slot clobber, floor, fingerprint, trusted-limit resolution, transform-await) are safe as-is.

**Overall verdict: REVISE** — with the **reactive phase-2 arm BLOCKED** until Findings 1, 2, and 3 are resolved. The deliberate `/ctx-wrapup [messages_to_keep]` command is a sound v1 given the boundary-override plumbing (Finding 1), a real wrapup latch (Finding 6), await-in-flight-first (Finding 7), and honest deferred-reduction reporting (Finding 8). The reactive downswitch reuse, as specified, will no-op or abort into the exact provider-400 it targets, and Pi "day-one parity" is not mechanically supported. Driving findings: **#2 (reactive no-op on limit change), #3 (timeout-abort ships oversized prompt), #4 (Pi loop unsound).**