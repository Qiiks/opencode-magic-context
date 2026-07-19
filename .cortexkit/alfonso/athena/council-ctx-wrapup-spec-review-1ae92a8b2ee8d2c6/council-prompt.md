
## Solo Analysis Mode
You MUST do ALL exploration yourself using your available read/search tools.
- Do NOT use task or any delegation tool under any circumstances
- Do NOT delegate to explore, librarian, or any other subagent
- Do NOT spawn background tasks
- Search the codebase directly — you have full read-only access to every file
- This mode produces the most thorough analysis because you see every result firsthand


## Analysis Intent: AUDIT

You are conducting an **audit** — your goal is to find discrete issues, risks, or violations.

**Focus:**
- Search for problems, anti-patterns, security risks, correctness issues, or violations of stated requirements
- Each finding must be a distinct, actionable item with concrete evidence
- Severity determines priority: critical (blocks/breaks), high (significant risk), medium (should fix), low (nice to fix)
- For each finding, provide the specific location (reference, section, or component where it occurs)
- State your confidence: high (clear evidence), medium (likely but needs verification), low (suspicion, investigate further)
- **This is a broad sweep, not a targeted trace.**

**Analytical standards:** Support claims with concrete evidence. State confidence (high/medium/low) for key assertions. Note caveats and limitations.

**Structure your response as:**
```
<COUNCIL_MEMBER_RESPONSE>
## Finding 1: [Title]
- **Severity**: critical/high/medium/low
- **Location**: [specific reference — e.g. component, section, endpoint, rule]
- **Confidence**: high/medium/low
- **Issue**: [what is wrong and why it matters]
- **Evidence**: [concrete reference, snippet, or observation that proves the issue]
- **Suggested Fix**: [actionable recommendation]

## Finding 2: [Title]
...

## Summary
[Total findings by severity. Overall risk assessment with confidence levels.]
</COUNCIL_MEMBER_RESPONSE>
```

## Analysis Question

Blind design review of a spec for a new Magic Context command, `/ctx-wrapup`, in the repo at ~/Work/Projects/CortexKit/magic-context (branch subc-migration). This is an AUDIT: find real defects with severity + file:line evidence from the ACTUAL code. Do NOT report style nits or hypotheticals without a concrete code path. End with a per-finding verdict and an overall SHIP / REVISE / BLOCK.

You have full read access to the repo. READ the actual code — the reviewer (Athena) has already read the key modules and pasted grounding below, but you MUST independently verify against the real files before asserting a finding. The referenced modules:
- packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts (the historian run: boundary snapshot handling, discard-last, publish transaction, recordProtectedTailPublicationFloor, setPendingCompactionMarkerState)
- packages/plugin/src/hooks/magic-context/protected-tail-boundary.ts (resolveProtectedTailBoundary, validateBoundarySnapshot, computeRawRangeFingerprint usage)
- packages/plugin/src/hooks/magic-context/read-session-true-raw-tokens.ts (computeRawRangeFingerprint definition — content-based over ordinal:id:parts.length:partContentFingerprint)
- packages/plugin/src/hooks/magic-context/transform.ts (findNewestUserModel / #188 model-switch detection, the >=95% blocking arm)
- packages/plugin/src/hooks/magic-context/transform-compartment-phase.ts (the >=95% blocking arm mechanics: awaitCompartmentRun — awaits ONE run with a per-run timeout, does NOT loop)
- packages/plugin/src/hooks/magic-context/compaction-marker-manager.ts (applyDeferredCompactionMarker, updateCompactionMarkerAfterPublication, existingMarkerAlreadyCoversTarget — monotonic boundary advance)
- packages/plugin/src/features/magic-context/storage-meta-persisted.ts (setPendingCompactionMarkerState — SINGLE-SLOT blob, overwritten per publish; recordProtectedTailPublicationFloor — prior_boundary_ordinal = MAX(existing, floor); clearPendingCompactionMarkerStateIf — CAS clear)
- packages/plugin/src/hooks/magic-context/recomp-orchestrator.ts (recompProgressBySession — single in-memory progress entry per session, setRecompStarting/setRecompTerminal)
- packages/pi-plugin/src/context-handler.ts (Pi emergency drain ~line 2036-2075: awaits ONE in-flight historian with 30s cap, does NOT loop; spawnPiHistorianRun ~2818 with refreshBoundarySnapshot support)
- packages/plugin/src/hooks/magic-context/compartment-trigger.ts (BLOCK_UNTIL_DONE_PERCENTAGE = 95)

=== THE SPEC (verbatim) ===
/ctx-wrapup [messages_to_keep] — forced historian drain of the live tail (v1).

Problem: two gaps, one engine. (1) Large->small model switches still overflow: switching a 553k-token session from a 1M model to a 272k model yields "Input exceeds context window" on the next message. The #188 unified-trigger fix arms recovery when lastInputTokens > new model's trusted limit, but that recovery is sized for incremental pressure, not for closing a ~280k gap — folding that much raw tail needs MULTIPLE sequential historian runs, and the blocking arm waits for at most one. (2) No deliberate "compact now" control.

Command contract: /ctx-wrapup [messages_to_keep], default 20. Runs the historian in a BLOCKING DRAIN LOOP over the eligible tail until coverage reaches the keep watermark. Publishes DEFERRED — never busts the cache itself; materialization rides the next natural bust (model switch -> HARD fold, free). No-op guard: tail already within keep-N -> no LLM run. Concurrency: if a historian run is already in flight, wait for it (same lease/completion machinery as the >=95% blocking arm), then continue the loop; if a wrapup is already running for the session, reject the second invocation. Failure honesty: published chunks stay published; on chunk failure, stop and report coverage reached.

Mechanics — Drain loop reuses compartment-runner-incremental.ts exactly as the emergency drain does, with one difference: the boundary given to each iteration comes from a WRAPUP BOUNDARY OVERRIDE, not resolveProtectedTailBoundary's normal pressure math. Anchor: lastCompartmentEnd+1. Eligible end: the message just above the keep watermark (instead of token-target/live-prompt-floor/0.40×usable math). Chunking WITHIN the eligible range stays the runner's normal chunk budget. Open tool arcs at the cut: snap watermark OUTWARD (keep more) reusing arc-fencing helpers. The keep watermark counts MEANINGFUL messages newest-first, then snaps outward per the arc rule, prefers a user-message boundary.

Discard-last OFF for the FINAL chunk: the runner's discard-last heuristic contradicts wrapup's coverage contract. For the FINAL loop iteration only, persist all validated compartments. Earlier iterations keep discard-last.

Blocking + progress: reuse the recomp progress surface (recompProgressBySession RPC/TUI channel) for per-chunk progress rows. Each iteration is a normal historian run. No hard iteration cap, but abort the loop if an iteration makes NO forward progress (coverage unchanged).

Cache safety (load-bearing): publishes ride the standard deferred-history-refresh path (historian publishes NEVER bust). NO forced materialization, NO flush inside the command. The compaction-marker move stays deferred exactly as for normal publishes. Growing tail mid-wrapup: each iteration re-resolves from CURRENT store state; the keep watermark re-anchors to the tail as of command invocation, so a growing tail only ever ADDS protected messages.

Reactive reuse (phase 2, same engine): upgrade the #188 arm — when lastInputTokens > trusted limit of the OUTGOING model, run the SAME drain loop to a watermark computed from the new limit (target: fold enough that m0+tail fits under executeThreshold × newLimit), BLOCKING, before the request goes out. Replaces the current one-shot blocking recovery for the downswitch case. The blocking wait is on the USER's message send (they see nothing until the drain finishes — potentially minutes for a 553k->272k switch). Accepted v1 (correct beats a provider 400). Show recomp-style progress during the wait.

Pi parity: same command, same loop, same boundary override, day one. Pi's runner is subprocess-based; the loop wraps runPiHistorian the same way its emergency drain does. Pi progress: /ctx-status-style toast updates per chunk.

=== ATHENA'S CODE GROUNDING (verify independently) ===
1. setPendingCompactionMarkerState (storage-meta-persisted.ts:1818) writes a SINGLE-SLOT blob per session — each publish OVERWRITES pending_compaction_marker_state. A wrapup doing 8 sequential publishes writes 8 blobs, but the deferred drain only runs on a LATER transform pass, so all 8 collapse to the LAST blob before any drain reads it. The marker only advances monotonically (existingMarkerAlreadyCoversTarget, compaction-marker-manager.ts:139), and the last pending has the highest ordinal subsuming all earlier ones — so the clobber MAY be benign. VERIFY: does anything between publishes read the pending blob and expect its own ordinal? Does applyDeferredCompactionMarker's validatePendingTarget (checks endMessageId still maps to a compartment with matching ordinal) still pass when only the last blob survives? Is there any m1-revision / decay-render / deferred-refresh state that assumes one-publish-per-trigger-cycle?

2. recordProtectedTailPublicationFloor (storage-meta-persisted.ts:341) writes prior_boundary_ordinal = MAX(existing, floorOrdinal) AND resets recovery_no_eligible_head_count=0. This is a MONOTONIC shared floor read by later NORMAL passes via migrationFloorActive (protected-tail-boundary.ts:480: runtimeFloor = max(offset, priorBoundaryOrdinal)). Wrapup pushes this floor to lastCompartmentEnd+1 each publish. Since normal passes' offset is already lastCompartmentEnd+1, this looks consistent — but VERIFY there is no OTHER boundary-derived shared/persisted state (trigger watermarks, fingerprints, emergency latches) the wrapup's widened boundary writes that a later NORMAL pass reads and would mis-derive from. The runner takes deps.boundarySnapshot as an explicit override (compartment-runner-incremental.ts:216) — trace what durable state the publish path writes from that snapshot.

3. computeRawRangeFingerprint (read-session-true-raw-tokens.ts:661) hashes ordinal:id:parts.length:partContentFingerprint over [offset, eligibleEndOrdinal). validateBoundarySnapshot (protected-tail-boundary.ts:689) recomputes it and rejects on mismatch. The runner re-resolves a stale snapshot ONCE from current state (compartment-runner-incremental.ts:252-274). For wrapup, each iteration hands an explicit widened boundary. VERIFY: when the user keeps chatting mid-wrapup (tail grows), does the growing tail only ADD messages ABOVE eligibleEndOrdinal (so the fingerprint of [offset, eligibleEnd) is unchanged), or can appended messages ever shift ordinals/ids within the eligible range and bust the fingerprint mid-loop? Does the keep-watermark re-anchoring interact correctly with validateBoundarySnapshot across iterations?

4. Discard-last OFF on the final iteration: the runner's discard-last (compartment-runner-incremental.ts:495-509, BOUNDARY_HEALING_SLACK=2) drops the provisional last compartment when lookahead is weak, AND SKIPS fact/event/observation/primer promotion on a discard-last run (lines 566, 632, 589). If wrapup forces discard-last OFF on the final chunk, that final compartment: (a) has structurally-unreliable boundary (no lookahead), (b) gets its facts/events/primers PROMOTED (durable) despite weak boundary. VERIFY the mechanism the spec proposes to force discard-last off, and whether persisting a weak-lookahead final compartment corrupts anything downstream that ASSUMES discard-last semantics (e.g., the offset=lastCompartmentEnd+1 re-read logic, marker endMessageId targeting, decay-render). Is the quality cost acceptable vs the coverage contract?

5. Reactive phase-2 watermark from the NEW model's limit: #188 detection uses findNewestUserModel (transform.ts:251) — the newest USER message carries the NEW model (OpenCode resolves outgoing model from lastUser.model). VERIFY the timing: is the new model's trusted limit resolvable at the point the drain would run, or is trusted-limit resolution deferred (detectedContextLimit only learns the real limit AFTER a provider response)? Can the blocking drain (multiple sequential LLM historian runs, minutes for 553k->272k) mechanically COMPLETE before OpenCode sends the request — i.e., is transform.ts (messages.transform) actually awaited by OpenCode before the outgoing call, and is there any per-pass timeout that would truncate the loop? The existing 95% arm awaits ONE run with historianTimeoutMs (transform-compartment-phase.ts:229, default 120s) — a multi-run loop needs a different budget.

6. Concurrency matrix — attack each: (a) wrapup vs in-flight background historian (lease: compartment-runner-incremental.ts:608 isCompartmentLeaseHeld under BEGIN IMMEDIATE); (b) wrapup vs wrapup (spec says reject second — verify the guard is race-free); (c) wrapup vs /ctx-recomp (recomp uses manual-full-recomp boundary + clears injection cache eagerly, preserveInjectionCacheUntilConsumed=false — wrapup preserves it; do they corrupt each other's marker/cache state?); (d) wrapup loop in progress when the >=95% emergency arm fires mid-loop on a concurrent transform pass (both want to drive the historian; both take the lease). VERIFY the lease + compartmentInProgress flag + activeCompartmentRun registration interplay.

7. UX honesty: (a) the no-op guard ("only N messages above the last compartment") — correct against getRawHistoryEligibility / meaningful-message counting? (b) partial-failure reporting; (c) the /ctx-flush suggestion — is there any case where the suggested flush would NOT apply the wrapup output (e.g., flush busts but re-resolves a boundary that no longer sees the deferred compartments, or the marker hasn't moved so filterCompacted still includes the raw tail)? Trace what /ctx-flush actually does vs what wrapup published.

8. Pi parity risks in the loop-wrapping approach: Pi's emergency drain (context-handler.ts:2056-2075) awaits ONE historian with a 30s cap and CANNOT abort mid-pass (no session.abort surface). spawnPiHistorianRun (2818) fires ONE background run. VERIFY whether wrapping runPiHistorian in a blocking loop is mechanically sound in Pi's subprocess model, whether Pi's transform/pipeline is awaited before the outgoing call the way OpenCode's is, and whether the 30s cap / lack-of-abort changes the reactive-arm correctness on Pi.

Deliverable: for EACH finding, give: a short title, severity (BLOCKER / HIGH / MEDIUM / LOW), the concrete file:line code path that proves it, and a one-line fix direction. Cover the 8 attack vectors above AND anything else you find in the real code. If a vector is a non-issue, say so briefly with the evidence that makes it safe (don't pad). End with an overall verdict: SHIP / REVISE / BLOCK, with the 1-3 findings that drive that verdict.