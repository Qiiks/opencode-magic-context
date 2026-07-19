---
task_id: bg_3d76f0be
agent: council-member
session_id: ses_0cc3a038fffeQzcOSzVzEdOyDU
parent_session_id: ses_0cc3ce25bffe2y8Lk3y9CGqpWL
status: completed
completed_at: 2026-07-05T19:43:26Z
---

[user] 2026-07-05T19:34:01Z
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

[assistant] 2026-07-05T19:34:01Z
**Planning code audit**

I need to audit the actual code in the repo's subc-migration branch. It's clear that I’ll be using read and search tools instead of task-based ones. I want to independently verify key modules and potential attack vectors with a focus on delivering findings about any defects in the /ctx-wrapup based on how the code currently behaves. I should inspect code paths related to both /ctx-wrapup and /ctx-flush, gathering evidence for my conclusions. Overall, I need to end with concrete verdicts for each finding and a final judgment of SHIP, REVISE, or BLOCK.

[assistant] 2026-07-05T19:34:16Z
**Inspecting Files**

I need to inspect the files directly. I'm thinking it might be best to outline the relevant files, as there could be multiple to look through. This way, I can keep track of what I’m examining and ensure I’m not missing anything important. I want to make sure I take a systematic approach to this inspection, so everything stays organized and I can address any issues that might arise. Let's get started!

[assistant] 2026-07-05T19:34:23Z
**Inspecting code sections**

I need to take a closer look at specific sections of the code. I'll consider using aft_zoom for a more detailed, symbol-level inspection. I want to read the lines surrounding the relevant portions, particularly focusing on areas related to the incremental runner, boundary, storage, transform, compaction marker, and Pi. Using aft_zoom for the symbols with context could help. I should aim to get these targeted sections analyzed in parallel for efficiency.

[assistant] 2026-07-05T19:34:35Z

[assistant] 2026-07-05T19:34:45Z
**Reading and outlining fingerprint processes**

I need to dive into the fingerprint definition and possibly outline the `computeRawRangeFingerprint` function. It might also be useful to look into building the `buildTrueRawTokenIndex` for ordinals. I wonder if the `pi-historian-runner` function should discard the last identical entries—I'll need to inspect that more closely. So, I’ll focus on reading specific ranges to gather the information I need!

[assistant] 2026-07-05T19:34:54Z
**Inspecting message interactions**

I think I ought to check out the rest of the readSessionChunk, particularly for messageCount, but maybe that’s not necessary. I need to dig into the transform-compartment-phase in its entirety around runCompartmentPhaseImpl. My focus will be on how the one-run await connects with lease interplay. I'll be looking closely around lines 159 to 420 to get a clearer understanding. This should help clarify things!

[assistant] 2026-07-05T19:34:58Z

[assistant] 2026-07-05T19:35:01Z
**Inspecting variables and functions**

I think I need to inspect the Pi and maybe check out FireHistorian regarding the trigger and the no-op. This might be relevant for wrapping the loop properly. I should read around the lines 3190 to 3269 to see if I can find anything useful. It's all about understanding how these elements fit together, so I can ensure everything functions as expected! Let's see what I can uncover in that section.

[assistant] 2026-07-05T19:35:05Z
**Inspecting paths and parameters**

I think I need to check the runPiHistorian to discard the last entry and see how publishing paths work. It might be similar to what OpenCode does. I wonder if I should search around BOUNDARY_HEALING_SLACK for relevant information or maybe related guidance. There’s definitely some investigation to be done here! It feels a little complex, but I'll get through it step by step. Let's keep it straightforward and clear!

[assistant] 2026-07-05T19:35:09Z

[assistant] 2026-07-05T19:35:11Z

[assistant] 2026-07-05T19:35:13Z

[assistant] 2026-07-05T19:35:19Z
**Inspecting command flush methods**

I need to look into the command-handler and execute-flush functions to understand how they work, especially the flush mechanism. There’s not much wrap-up here, so I’ll focus on reading the relevant sections to gain clarity on these processes. It’s important for me to ensure I grasp the details effectively and figure out the specifics of how the flush operates. That should help in achieving better functionality!

[assistant] 2026-07-05T19:35:25Z
**Investigating the process**

I need to inspect the findBoundaryUserMessage or validatePendingTarget to check the last pending blob for safety. I should take a closer look at the compaction-marker manager at the top. This feels a bit technical, but it's important to ensure everything is functioning properly and safely. Let's take it step by step to figure out what's going on!

[assistant] 2026-07-05T19:35:31Z
**Inspecting compartment interplay**

I think I need to focus on the relationship between activeCompartmentRun and startCompartmentAgent lease. There’s something here about how they interact with each other that I need to figure out. Maybe I should break down what each component does and how they depend on one another. It seems I’ll have to look closely at their roles to ensure everything functions smoothly. There’s definitely some analysis required!

[assistant] 2026-07-05T19:35:35Z
**Inspecting recomp progress**

I need to check the recomp/orchestrator to see if there's a single entry per session. It might be straightforward, but there's a chance of conflict when using the same map, which could affect recomp progress. I should gather evidence on this. If I reuse recompProgressBySession for wrap-up progress, starting the wrap-up might overwrite ongoing progress entries. That might not be an issue if they’re mutually exclusive, but I need to examine whether any code assumes only a certain set for kinds. I’ll search for recomp progress usage.

[assistant] 2026-07-05T19:35:40Z

[assistant] 2026-07-05T19:35:43Z

[assistant] 2026-07-05T19:35:53Z

[assistant] 2026-07-05T19:35:56Z

[assistant] 2026-07-05T19:36:00Z
**Inspecting transform functions**

I need to check the comments in transform.ts, particularly if there's something about awaited calls. There's also a comment in runCompartmentPhase that might help. It’s important to look at model limit resolution functions too, specifically comparing resolveTrustedContextLimit and getSdkContextLimit. I might need to check how outgoing model limits are resolved, especially if it's only from the SDK catalog, with no detected limit until after the provider. I'll search for definitions as well.

[assistant] 2026-07-05T19:36:06Z

[assistant] 2026-07-05T19:36:10Z

[assistant] 2026-07-05T19:36:18Z

[assistant] 2026-07-05T19:36:20Z

[assistant] 2026-07-05T19:36:25Z

[assistant] 2026-07-05T19:36:35Z

[assistant] 2026-07-05T19:36:38Z

[assistant] 2026-07-05T19:36:49Z
**Analyzing session clearing**

I’m inspecting the event-handler to see if it's clearing the pending items in session.compacted. It might be worth considering whether there's a flush happening as well. I want to read through the relevant lines to get a better understanding of what's going on. It's all about making sure that the event-handler is functioning properly and isn't leaving anything lingering. Let's keep going and see if there's more to note here!

[assistant] 2026-07-05T19:36:53Z

[assistant] 2026-07-05T19:36:57Z
**Inspecting boundaries and timeouts**

I’m focusing on boundaries now, making sure that normal passes are aligned properly with the last compartment end. I need to check model-switch detection and per-pass timeouts too. I saw the getSdkContextLimit earlier, but I wonder if there's a hard transform timeout I need to account for. I should also search for historianTimeoutMs since it might only be linked to await and runner processes. There’s definitely more to dig into with other timeouts.

[assistant] 2026-07-05T19:37:03Z
**Inspecting the pi context handler**

I need to take a closer look at the hook around comment 27 in the pi context handler. It seems important, and the top lines might give me some insights. Let’s see if I can figure out what’s going on there! I want to ensure everything is working smoothly, so I'll be thorough and check for any potential issues. I'll have to pay attention to the details to understand fully.

[assistant] 2026-07-05T19:37:07Z

[assistant] 2026-07-05T19:37:11Z

[assistant] 2026-07-05T19:37:13Z
**Assessing implementation issues**

I need to identify the discrepancies between the spec for the new `/ctx-wrapup` command and the existing code. Since the implementation isn't done yet, I'll audit it to find defects and highlight any design mismatches or omissions. There could be some attack vectors that turn out to be non-issues, but it’s crucial to check these thoroughly to ensure everything aligns properly. Let's carefully evaluate this to get it right!
**Considering iteration management**

If each iteration runs as a separate historian run, there could be gaps in which other processes, like /ctx-recomp or emergency transforms, might disrupt the state. The specifications indicate that each iteration should re-resolve the state, but they don't specify if the entire loop is locked, which raises concerns. The current lease only protects a single run and not multi-pass loops, which might allow overlapping operations. I'll need to verify this from the code to ensure everything works correctly.
**Evaluating wrapup command process**

The specification mentions rejecting a second wrapup if one is already running, but it doesn't address whether /ctx-recomp or emergency paths can activate during the loop. Since each iteration acts as a separate historian run, `compartmentInProgress` might be false and activeRuns could be empty between loops. This allows a transform trigger to potentially initiate a new background run, which feels like a design flaw. I need to consider if the wrapup command can utilize the lease machinery more effectively before proceeding with each iteration.
**Identifying design defects**

I'm noticing that the existing activeRuns infrastructure isn't really set up to handle nested whole-loop and per-iteration runs. That's a significant flaw in the design. The spec mentions reusing the compartment-runner-incremental like it's done with the emergency drain, but the emergency drain actually depends on the `startCompartmentAgent` registering activeRuns for each run. It seems like that inconsistency could lead to issues in functionality.
**Evaluating publication specs**

I need to inspect the failure honesty spec that says published chunks should stay published, and if there's a failure, I should stop and report coverage reached. That seems aligned with the concept of publishing after each pass. However, there’s a potential issue with the default no-op guard, which might prevent running when "tail already within keep-N." I should explore how to count meaningful messages and create tool arcs while preferring boundary conditions. The spec might be okay overall, though.
**Considering multi-run loops**

I'm thinking about a multi-run loop where the user might have to wait 6-10 minutes if there are 3-5 runs, each taking 120 seconds. That feels like a considerable wait time, so I'd need to adjust the budget accordingly and perhaps notify the user about this timing. I want to make sure everything is clear and the user feels informed about what to expect!

[assistant] 2026-07-05T19:39:32Z

[assistant] 2026-07-05T19:39:38Z

[assistant] 2026-07-05T19:39:41Z

[assistant] 2026-07-05T19:39:43Z
**Addressing non-issues**

I’m thinking we should briefly mention any non-issues, possibly after presenting our findings or in the summary. The prompt suggests that if a vector is a non-issue, I should state that clearly, supported by evidence to ensure it's safe. I want to keep it concise without unnecessary padding. So, perhaps I could include "Verified non-issues" as bullet points after the findings or summary to clarify this effectively.
**Evaluating compaction safety**

I’m thinking about the single-slot pending compaction marker. It seems safe since only the transform postprocess consumes it. Plus, the last pending marker subsumes the earlier one due to the monotonic marker advance and target validation. I want to make sure that I'm clear on these details because it impacts how compaction operates. It’s interesting how these markers work together, ensuring safety and validity throughout the process!
**Considering command concurrency**

I’m thinking about whether the /ctx-recomp command can be invoked at the same time. I'm realizing that the real risk seems to be during the reactive phase 2 in transforms. If the user can't send another message until the first one returns, there might not be concurrent transforms for the same session. I wonder if background events could trigger the /ctx-recomp command, and maybe a second wrap-up command would only be rejected if it conflicts. The repo does allow multiprocess and shared sessions, though!
**Reviewing telemetry processes**

I’m examining how the incremental runner records telemetry and sends messages during failures. The wrapup command could catch and report these issues, which doesn't seem like a code defect. I think I need to inspect the `detectMidTurnBypassReason`, but that's not really relevant now. One potential issue could be with the wrapup command's default setting of `messages_to_keep=20` — it might miscount meaningful messages, but maybe there's a way to implement something new. It's not concrete yet.

[assistant] 2026-07-05T19:41:12Z
**Investigating phase test**

I’m considering whether I need to inspect the transform-compartment-phase test. I’m wondering if there’s an issue with phase 2. Specifically, does the `recordOverflowDetected` flag only allow the arm to reset the failure state? That seems irrelevant right now. It's also wise to check the `historyWasConsumedThisPass` and look into the /ctx-flush suggestion. I’ll search around the computation to see if anything else stands out. Let’s verify those aspects!

[assistant] 2026-07-05T19:41:17Z

[assistant] 2026-07-05T19:41:19Z
**Clarifying code documentation**

I want to include clear one-line fix directions and summarize non-issues separately. It’s vital that each finding has specific evidence from the code, citing multiple files and line numbers when possible. For example, I'd use a format like `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:663-669` for file ranges. I need to confirm exact line numbers by checking those regions thoroughly, especially verifying if `compartmentInProgress` triggers a start when there's no `activeRun`.

[assistant] 2026-07-05T19:41:42Z
**Evaluating settings and options**

I’m considering whether I can set the usage percentage artificially, but that might mess with other boundary calculations. It seems better to include an explicit bypass option instead. I also need to think about whether the per-window budget might impact the reactive phase 2 after the first run, especially if there's a usage bump to 95 and the latch activates. The over-quota bypass should happen on each pass if there hasn't been a recent failure. So, okay for the reactive path, but not for the explicit command.
**Evaluating implementation issues**

I'm looking into findings about the implementation of proposals. For finding 6, it might be high due to explicit code concerns, while finding 7 appears to be medium. I need to phrase things clearly to avoid hypotheticals. There’s a point about `reserveProtectedTailDrainTokens` possibly allowing enough runs under certain circumstances, but that’s not guaranteed because the command has no cap. So, relying on quotas seems incorrect. I feel confident about these assessments, but I should check for potential issues with `clearEmergencyRecovery` too.
**Finalizing documentation**

I think I should mention that "same engine phase 2 accepted v1" implies these reactive issues are still relevant, even if the explicit command functions properly. I’ll need to inspect the exact line regarding `recordHighPressureNoEligibleHead`, but maybe dropping some older outputs using `ctx_reduce` could help. Current context is fine, and while I considered dropping some details, they are necessary. I’ll draft a concise final document containing around 7 findings, a summary, and verified non-issues while ensuring to avoid raw tool output or full file contents.
<COUNCIL_MEMBER_RESPONSE>
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
</COUNCIL_MEMBER_RESPONSE>

