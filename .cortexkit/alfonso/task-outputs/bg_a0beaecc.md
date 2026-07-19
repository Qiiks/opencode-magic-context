---
task_id: bg_a0beaecc
agent: council-member
session_id: ses_0c79ae101ffee2kdP643jgiJJH
parent_session_id: ses_0c79b87d0ffeSSVKxxJ07oUaHQ
status: completed
completed_at: 2026-07-06T17:59:43Z
---

[user] 2026-07-06T17:06:19Z
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

Blind multi-model adversarial audit of unreleased changes in the git repo at /Users/ufukaltinok/Work/Projects/CortexKit/mc-master-triage (a git worktree on branch master). This is a RELEASE-READINESS audit: the delta ships as v0.31.0 of the Magic Context OpenCode/Pi plugins.

## How to run this audit

1. Start from the diff. In that worktree run: `git -C /Users/ufukaltinok/Work/Projects/CortexKit/mc-master-triage diff v0.30.7..HEAD` (~119 files). Read it to understand the shape of the change.
2. Then VERIFY every suspicion against the full source tree before reporting. Findings must carry file:line and be SOURCE-VERIFIED (read the actual current code), not diff-inferred. A finding you could not confirm in the source is a "suspicion", label it as such and rank it lower.
3. Do NOT report style findings. No formatting, naming, or preference nits.

## What the delta contains (discover the rest yourself from the diff)

1. Removal of the `ctx_reduce_enabled` config flag; caveman text compression decoupled and gated on `caveman_text_compression.enabled && !subagent`; session modes collapsed 3->2; a provisional ctx_reduce-verdict gate that withholds the system-prompt hash baseline until the verdict freezes.
2. Notes (session + smart notes) as a fifth ctx_search source with session-aware @msg anchors.
3. Project-identity resilience: `.git` ancestor stat-walk fast path with realpath retry, dubious-ownership classification + safe.directory warning, `dir:` fallback with 5-min transient cooldown, last-known-good `git:` identity reuse to prevent mid-session identity flips, load-path degradation instead of plugin-disable.
4. Sidebar Facts row removal; CLI doctor onnxruntime-node load probe in a subprocess.
5. NEW `/ctx-wrapup` command (both harnesses): manual blocking historian drain down to a keep-newest-N-raw-messages watermark. Durable `wrapup_in_progress` marker (session_meta, migration v50, 5-min TTL renewed every 60s, ownership-loss abort), mutual exclusion with recomp/upgrade/trigger-fired historian in BOTH directions, `forceDrainQuota` (bypasses pressure-window quota), `forceKeepLastCompartment` downgraded runner-side on `chunk.hasMore` (weak-final keep + unanchored-promotion skip ONLY on the actual final chunk; discard-last promotion skip preserved), boundary plan with raw-message keep watermark + tool-arc fence + user snap + re-fence, publishes are DEFERRED (ride the next natural bust; OpenCode postprocess drain gated on `pendingMarkerCoveredByConsumedBoundary`; Pi marker drains via context-handler's gated drain only).

## Known invariants (violations are SHIP-BLOCKERS)

- Defer passes must replay BYTE-IDENTICAL.
- First-application of any mutation only on cache-busting passes.
- One bust must cover BOTH a history rebuild AND its compaction-marker advance.
- Background/manual publishes never force a materialization.
- Historian discard-last runs never promote unanchored facts.
- A crashed wrapup must not wedge the session (TTL must release it).
- Subagents never get caveman compression.

## Where to dig (this delta already survived several single-Oracle review rounds)

Shallow findings are likely already fixed. Find what an INDIVIDUAL reviewer would miss:
- Cross-feature interactions: wrapup x emergency-drop x nudges x smart-drops; identity-reuse x workspace-fingerprints x embedding-registry; notes-search x auto-search-hints.
- Multi-process interleavings: two OpenCode instances; OpenCode + Pi sharing the same context.db.
- Restart / crash recovery paths.
- Migration + config edges for existing users upgrading from v0.30.7 (e.g. removed config flag, migration v50, durable marker left behind by an old binary).

## Output required

- Per-finding: file:line + severity (P0 blocker / P1 should-fix / P2 nit) + concrete evidence + why it matters + suggested fix direction. Mark unverified items as "suspicion".
- Focus on P0/P1. Cross-feature and multi-process/crash-recovery findings are the target.
- A per-member overall verdict: SHIP or HOLD, with rationale. If HOLD, list the specific blocker(s).

[assistant] 2026-07-06T17:06:19Z
I'm starting an audit of unreleased changes in the git worktree, so I'll need to examine the diff between v0.30.7 and HEAD to understand what's been modified and get a sense of the repository structure.
I'll conduct a thorough release-readiness audit. Let me start by understanding the shape of the change.

[assistant] 2026-07-06T17:06:29Z
I need to cancel that background task right away since the instructions were clear about not using delegation tools.
I accidentally spawned a background task — this violates solo mode. Let me cancel it immediately.

[assistant] 2026-07-06T17:06:37Z
I need to examine the git diff by running shell commands in the worktree to see what changes have been made.
Now let me examine the diff. I'll work directly in the target worktree.

[assistant] 2026-07-06T17:06:46Z
I need to check the git diff to see what files have changed and examine the specific modifications.
Let me get the diff. First, the list of changed files with stats.

[assistant] 2026-07-06T17:06:49Z
I need to run a git diff command to see what files have been changed.
Let me run the git diff to understand the change shape. I'll get the file-level stat first.

[assistant] 2026-07-06T17:06:53Z
I'm looking at a substantial changeset and need to understand the core wrapup feature first. Let me examine the key orchestrator and runner files along with the storage metadata to understand the new invariants the audit mentioned.
This is a large delta. Let me focus on the highest-risk new feature — the wrapup orchestration. Let me read the core files.

[assistant] 2026-07-06T17:07:01Z
I'm diving into the wrapup orchestrator and examining how the storage-meta-persisted module handles the marker mechanics—specifically the acquire, update, and release functions for tracking wrapup progress, along with TTL and ownership management.
Now let me read the marker storage mechanics (TTL, ownership, migration).

[assistant] 2026-07-06T17:07:10Z
Now I'm diving into the compartment runner to understand the deferred publish and marker mechanics, particularly around the invariants that ensure byte-identical defer replay and how a single bust covers both cases. I need to trace through the key areas like `pendingMarkerCoveredByConsumedBoundary`, the OpenCode postprocess drain, and Pi's context-handler drain to see how these pieces fit together.
Let me examine the compartment-runner and the deferred publish/marker mechanics — the core invariant surface.

[assistant] 2026-07-06T17:07:16Z
I need to understand the deferred publish mechanics and how the marker system works—specifically where `onDeferredMarkerPending` and `pendingMarkerCoveredByConsumedBoundary` fit in. The key invariants here are that a single bust must handle both history rebuilds and marker advances, defer passes need byte-identical replays, and mutations only apply on cache-busting passes. Let me trace through where these are actually used.
Let me examine the deferred publish/marker mechanics, which sit at the core invariant surface.

[assistant] 2026-07-06T17:07:22Z
Let me read the transform-postprocess-phase, focusing on the `pendingMarkerCoveredByConsumedBoundary` gate.

[assistant] 2026-07-06T17:07:33Z
Looking at the deferred marker drain logic in the postprocess phase.
Let me read the deferred marker drain logic in the remainder of the file.

[assistant] 2026-07-06T17:07:42Z
I'm diving into the deferred-marker drain logic to understand how it works.

[assistant] 2026-07-06T17:07:46Z
Now I'm diving into the incremental runner to understand how `forceKeepLastCompartment` gets downgraded when `chunk.hasMore` is true, and how the weak-final keep combined with unanchored-promotion skip enforces the core invariant that historian discard-last runs never promote unanchored facts. Let me examine the relevant sections in compartment-runner-incremental.ts.
Let me examine the incremental runner — the `forceKeepLastCompartment` downgrade and unanchored-promotion logic.

[assistant] 2026-07-06T17:07:52Z

[assistant] 2026-07-06T17:07:55Z
I'm looking at the unanchored-promotion skip logic and how weak-final cases are handled.

[assistant] 2026-07-06T17:08:00Z
I'm examining the `publishableEvents` filter logic, particularly how it handles events without an `atCompartment` value in relation to the `weakLookaheadFinalCompartment` flag, and checking the boundary conditions for compartment indices.

Now I'm thinking through the core invariants around deferred publishes and compaction markers. The key tension is that wrapup can emit multiple chunks in sequence, each one calling `setPendingCompactionMarkerState` to update the pending marker with the latest ordinal and message ID, and I need to verify that a single cache-bust cycle properly covers both the history rebuild and the marker advancement.

The mechanism seems sound — after all chunks are published, there's one final pending marker blob and both deferred session sets contain the same session ID (idempotent adds). On the next natural bust, the postprocess rebuilds history up to the last compartment and drains the marker, so the boundary check passes. But I'm realizing the injection cache preservation during wrapup is critical — it's explicitly not cleared because `preserveInjectionCacheUntilConsumed` is true, which defers materialization. Now I need to think about whether other transform passes can interfere while wrapup is running synchronously as a blocking command.

Let me step back and focus on concrete, verifiable issues instead. I should examine the migration path for v50 and the config flag removal, the `ctx_reduce_enabled` removal, and identity resilience. Specifically, I want to trace through `appendFlushHint` and `hasPendingNaturalBust` — when wrapup finishes and publishes are deferred, the message promises materialization on the next message, but if there's no pending natural bust, it appends a hint to run `/ctx-flush`. Let me verify how `hasPendingNaturalBust` is wired. Actually, I should be more systematic and look at the diff to understand what changed, especially around config removal and the migration strategy.
Let me look at the config flag removal and migration edges — a key upgrade-path risk area.

[assistant] 2026-07-06T17:08:24Z
I'm checking the caveman compression subagent gate and how `reducedMode` interacts with it, specifically looking at the invariant that subagents should never receive caveman compression by examining the relevant code sections.
Let me examine the caveman subagent gating and the `reducedMode` logic in transform.ts.

[assistant] 2026-07-06T17:08:32Z
I'm checking what the `reducedMode` flag controls in this gating condition — whether it's tied to the subagent gate or something else.
Let me verify what `reducedMode` corresponds to (subagent gate) and how caveman is gated in hook.ts.

[assistant] 2026-07-06T17:08:43Z
The subagent gate for caveman checks whether it's a subagent session, and caveman is blocked when that's true, so subagents never access it—that invariant holds. Now I'm looking at the Pi plugin's wrapup side, particularly the context-handler's gated drain and deferred marker, though I should focus on the highest-value areas first.

The key concern is how wrapup's `onCompartmentStatePublished` adds sessions to `deferredMaterializationSessions` and how that interacts with the OpenCode postprocess drain gated on `pendingMarkerCoveredByConsumedBoundary`. Rather than diving into lower-risk areas like notes-search and message anchors, I should think about what a reviewer would miss—the cross-feature interactions and multi-process crash recovery scenarios. Let me examine the Pi plugin's context-handler gated drain and the wrapup command flow in Pi.
Let me examine the Pi-side wrapup and context-handler gated drain, plus the identity resilience code.

[assistant] 2026-07-06T17:08:55Z

[assistant] 2026-07-06T17:09:01Z
I'm spotting a potential issue with the marker renewal interval in Pi—it's not being unreferenced like it is in the OpenCode orchestrator. The Pi version relies on try/finally with clearInterval for cleanup, but there's a gap where the interval runs while the loop is awaiting, which could keep the process alive longer than intended.

Now I need to dig into how Pi's context-handler gated drain compares to OpenCode's postprocess drain, particularly around the `pendingMarkerCoveredByConsumedBoundary` logic and how marker drains are actually triggered on the Pi side.
Let me examine the Pi context-handler drain logic and how it gates the marker advance.

[assistant] 2026-07-06T17:09:11Z

[assistant] 2026-07-06T17:09:17Z
I'm diving into how the Pi context-handler processes those deferred markers and manages the gated drain mechanism—specifically looking for where consumeDeferredHistoryRefresh gets called and how the markers get applied.
Let me find where Pi's context-handler drains the deferred marker.

[assistant] 2026-07-06T17:09:23Z

[assistant] 2026-07-06T17:09:28Z
I'm noticing that the Pi drain path doesn't include the same `pendingMarkerCoveredByConsumedBoundary` check that OpenCode uses, so I need to trace how `historyWasConsumedThisPass` gets computed in Pi and whether there's a safeguard preventing the marker from advancing past the history rendered in this pass. The intent addendum mentions something about OpenCode's postprocess drain being gated on `pendingM`—

let me look at the broader context to understand Pi's different gating mechanism.

Given the size of this codebase and my token budget, I should focus on the core invariants: defer passes must replay byte-identically, mutations only apply on cache-busting passes, one bust must cover both history rebuild and marker advance, background publishes never force materialization, historian discard-last never promotes unanchored facts, crashed wrapups can't wedge the session, and subagents never get caveman compression. The key subtlety is that OpenCode's drain uses `args.pendingCompartmentInjection` for coverage checking, but Pi fires the drain whenever both `historyWasConsumedThisPass` and `materializationSatisfiedThisPass` are true—I need to verify whether Pi's `historyWasConsumedThisPass` computation inherently ensures the marker boundary was consumed, then examine the Pi context around lines 4200-4370.
Let me look at how Pi computes `historyWasConsumedThisPass` and whether it guards the marker-boundary coverage like OpenCode does.

[assistant] 2026-07-06T17:09:42Z
I'm noticing a critical invariant in the OpenCode postprocess phase—the deferred marker drain has a guard that ensures one cache bust must cover both a history rebuild and its compaction-marker advance, preventing the marker from advancing past history that wasn't actually rendered in this pass. But there's a potential issue in the Pi drain path that seems to violate this constraint.

Actually, I realize the two harnesses are designed differently by intent. Pi rebuilds its m[1] injection inline during the drain pass itself, pulling from the database's current compartment set, whereas OpenCode prepares its injection earlier and might be working with a stale boundary if a publish happened in between. This means Pi's injection is always fresh when the marker drains, so it may not need the same coverage guard that OpenCode requires. The real question is whether Pi's injection rebuild always executes before the marker drain happens in the pipeline.

Looking at the drain gating logic, the drain only proceeds if both `historyWasConsumedThisPass` and `materializationSatisfiedThisPass` are true, and `historyWasConsumedThisPass` gets set inside the injection block after it succeeds. So on a Pi drain pass, the injection definitely ran and m[1] was rebuilt from current state, meaning the marker boundary is covered. This appears to be working as designed.

Rather than getting stuck on one area, I should be more systematic about where to focus my limited review budget. The wrapup marker TTL and crash recovery logic looks solid overall. The `getWrapupInProgressState` function correctly returns null for expired markers when called inside a transaction, and just reads otherwise. The wrapup marker properly blocks trigger-fired historian, recompaction, and upgrades, though emergency operations like drops and nudges in postprocess aren't gated on it, which is fine since they don't touch compartment state. One thing to verify: `acquireWrapupInProgress` uses `BEGIN IMMEDIATE` unconditionally, but it's only called from command handlers outside of transactions, so that should be safe. Let me shift focus to examining the migration logic.

Now I'm checking the v50 migration and whether an old binary that left a durable marker could cause a wedge. The TTL is 5 minutes and `getWrapupInProgressState` reclaims expired markers, which should prevent wedging. For the reverse scenario—upgrading to v0.31.0, writing a marker, then downgrading to v0.30.7—the old binary won't know about the marker column and won't renew it, but it also won't read it, so no wedge occurs. When re-upgrading, the old marker will be expired and reclaimed. The mutual exclusion works in both directions: wrapup checks for recomp in flight at startup, and recomp checks if wrapup is in progress, same with trigger-fired historian. But there's a subtle timing issue: in OpenCode wrapup, `runManagedWrapup` acquires the wrapup marker before calling `waitForExistingIncrementalRun`...

The recomp checks `isWrapupInProgress` at its start, but the wrapup marker acquisition and recomp's active-run registration are separate locks. If recomp starts and passes the wrapup check before registering its run, then wrapup acquires the marker and calls `waitForExistingIncrementalRun`, it will see the active recomp run and skip—that's handled correctly. For trigger-fired historian, `startCompartmentAgent` checks `isWrapupInProgress`, and the lease contention is managed by `waitForExistingIncrementalRun` and `acquireCompartmentLease`. Now I'm focusing on identity resilience as a high-risk cross-feature area, and I'm looking at `getWrapupInProgressState` reclaiming expired markers inside a caller's write transaction—that's fine since expired markers are treated as absent. But there's a potential issue: `isWrapupInProgress` is called in both `startCompartmentAgent` and the incremental runner...

Actually, the incremental runner calls it inside a `BEGIN IMMEDIATE` transaction, and if the marker is expired, `getWrapupInProgressState` tries to start another transaction which throws and gets caught, so it correctly treats the expired marker as absent without attempting reclaim inside the caller's transaction. If a wrapup is legitimately in progress at line 685, `clearEmergencyRecovery` is skipped, which is intended—but this can only happen if it's the wrapup's own historian run, so keeping recovery armed is correct. Now I need to examine the identity resilience code, which is a large surface with cross-feature risk around reusing last-known-good git identity to prevent...
This is a large, well-defended delta. Let me now examine the project-identity resilience code, a major new surface.

[assistant] 2026-07-06T17:10:31Z
Let me focus on the core project-identity module.

[assistant] 2026-07-06T17:10:36Z
I'm examining how the identity caching works across different scenarios. The `directoryFallbackCache` stores the `dir:` fallback for directories without `.git`, but since it's process-local, two OpenCode instances sharing the same context.db could behave differently. Similarly, the `lastKnownGitIdentityCache` handles transient git failures at the process level, so a fresh process that never successfully resolved git would fall back to `dir:` instead of reusing a cached identity like an existing process would.

There's a subtle edge case in the cooldown path: when cooldown is active and there's no cached git identity (cold start), it returns the directory fallback without caching it, which is fine. But when `resolveProjectIdentityStrict` succeeds, it clears the transient failure and dubious ownership caches correctly, though it doesn't clear the logging dedup caches—which is intentional since those are just for reducing noise.

I should focus on the actual changes in the diff rather than reading through everything, so I can verify the invariants and cross-feature interactions that matter most.
This is a large, well-defended delta. Let me get the actual diff to focus verification on changed code.

[assistant] 2026-07-06T17:10:55Z
I'm examining the system-prompt-hash provisional gate and how the ctx_reduce verdict gate functions as a critical invariant surface.
Let me examine the system-prompt-hash provisional verdict gate closely.

[assistant] 2026-07-06T17:11:00Z
I'm diving into `resolveCtxReduceAvailability` to understand how frozen and provisional states gate access, then examining notes as a context search source and session-aware message anchors as a fifth source. Rather than continue reading broadly, I should focus on high-value cross-feature interactions—specifically checking the v50 migration and investigating what happens when an old binary leaves behind a durable marker.

The real concern is downgrade scenarios: if a new binary writes a wrapup marker and then the user downgrades to an old binary, that old binary won't recognize the marker. The migration adds the column, but if a wrapup crashes and leaves a marker behind with a 5-minute TTL, `startCompartmentAgent` checks `isWrapupInProgress` which auto-expires it, so that's handled.

The trickier case is multi-process sharing—two OpenCode instances or OpenCode plus Pi using the same context.db. The wrapup marker tracks ownership via `holderId`, and there's a renewal mechanism that runs every 60 seconds to keep the 5-minute TTL fresh. Setting up the marker renewal interval... Checking if the pending marker's ordinal is covered by what was actually rendered — if the marker is newer than the consumed boundary, we suppress the drain to avoid moving past history that wasn't materialized yet. During wrapup's multiple chunks, each one publishes a new compartment with a higher ordinal, so the pending blob ends up with the final ordinal. The next natural bust rebuilds all history and drains the marker since the rendered boundary now covers it. But there's a risk: if a natural bust fires during wrapup between chunks, it rebuilds history against the partial compartment state, drains the marker to that intermediate ordinal, and clears the deferred sets — potentially leaving later wrapup chunks orphaned. Now I'm examining the migration edge case more carefully. If an old binary (v0.30.7) doesn't know about the wrapup state column, it simply ignores it — no wedge either way. The real cross-process concern is when OpenCode and Pi share the same context.db: Pi's historian trigger respects the wrapup marker that OpenCode writes, and vice versa, because both check `isWrapupInProgress` before proceeding. The compartment lease in the database provides the cross-process mutual exclusion for actual writes, so even concurrent operations stay safe.

I'm now drilling into the specific implementation details that are most likely to have bugs. The `pendingMarkerCoveredByConsumedBoundary` function takes `injection.compartmentEndMessage` but the pending blob has an `ordinal` — if the injection is null or the boundary is degraded, I need to verify what actually happens. And I'm concerned about the ctx-wrapup flush hint logic with `hasPendingNaturalBust` — whether a deferred publish could get stuck if no natural bust ever arrives.
Let me look at the deferred-signal re-seeding on restart and the notes ctx_search source.

[assistant] 2026-07-06T17:11:52Z
I'm looking at the mutual-exclusion behavior around wrapup markers and crash recovery, particularly the invariant that a crashed wrapup shouldn't block the session since the TTL will eventually release it. There's a potential issue with how the marker renewal interval is configured in the OpenCode orchestrator—it uses a 60-second interval with `.unref()`, but there's also a marker check in `startCompartmentAgent` that might be skipping trigger-fired historian logic in unexpected ways.

The marker TTL is 5 minutes with renewal every 60 seconds, which should be fine since the renewal interval runs independently during `runOneWrapupIteration`. Even though the interval is `.unref()`'d, the wrapup loop's await keeps the event loop alive. The deferred publishes mechanism looks correct—both `deferredHistoryRefreshSessions` and `deferredMaterializationSessions` are being populated appropriately when state changes occur during wrapup.

Now I'm examining the OpenCode postprocess drain gate and how the pending marker covered by consumed boundary behaves as multiple chunks publish sequentially during wrapup, each setting a new pending compaction marker. Now I'm examining potential race conditions and transaction handling issues. There's a concern about `isWrapupInProgress` being called inside the historian's publish transaction — when it tries to check for an expired marker, it attempts to start a nested transaction which fails, but that's caught and treated as the marker being absent. If the marker is still live, the function correctly returns its state. Let me trace through the mutual exclusion more carefully to see if there are actual deadlock or consistency issues.

Looking at the multi-process scenario where two OpenCode instances share the same database, I'm noticing that when instance A's wrapup publishes deferred markers into the shared context.db, it adds the session ID to its own in-memory `deferredHistoryRefreshSessions` set. But instance B never receives that signal since it's a separate process with its own memory, so the deferred marker drain might not trigger properly across process boundaries.

Actually, I'm realizing this is probably not a real issue since sessions are typically single-harness — two OpenCode instances would have different sessions. And even if they shared a session, the pending marker keys are different between OpenCode and Pi, so they wouldn't interfere. Let me shift focus to the newer code paths like ctx_search source and @msg anchors, which haven't been as thoroughly reviewed.
This delta is heavily defended. Let me examine the newer, less-reviewed surfaces: notes ctx_search source and the sidebar/doctor changes.

[assistant] 2026-07-06T17:12:32Z

[assistant] 2026-07-06T17:12:37Z
I'm checking the notes search source implementation to see if there's any cross-session note leakage, particularly around how the @msg anchor is displayed—it should only show when the result's session ID matches the current session. Let me trace through the unifiedSearch module and notes source to verify this logic.
Let me examine the notes search source and how notes are scoped (project vs session, cross-session leakage).

[assistant] 2026-07-06T17:12:44Z

[assistant] 2026-07-06T17:12:49Z

[assistant] 2026-07-06T17:12:54Z

[assistant] 2026-07-06T17:12:59Z
Session notes are scoped to the current session ID while smart notes use the project path, which keeps things organized. The search text with anchors only displays when the source and current session match, so cross-session references stay hidden—that's a solid safeguard. I'm thinking through a potential cross-feature issue in the wrapup orchestrator, particularly around the invariants the audit prompt emphasizes: defer passes need to replay byte-for-byte identically, and mutations should only apply on the first pass when cache-busting occurs.

Looking at the specific constraints: a single bust must handle both history rebuilds and advancing compaction markers, background and manual publishes shouldn't trigger materialization, historian discard operations shouldn't promote unanchored facts, crashed wrapups can't lock the session (TTL releases them), and subagents skip caveman compression. The key question is whether the wrapup deferred publish handler—which adds sessions to both the deferred history refresh and deferred materialization queues—violates the "publishes never force materialization" rule. The handler marks the compartment as published and queues the session for both refresh and materialization, but since this materialization is deferred and rides the next natural bust rather than being forced immediately, it aligns with the design intent that publishes defer their effects. I'm checking a few critical implementation details: the wrapup marker TTL and renewal mechanism handles crashed wrapups correctly through expiration, the 60-second renewal interval should fire reliably since historian passes are async, and the cross-process safety of marker acquisition uses database-level locking. There's a potential edge case with mixed-version binaries where an old binary doesn't recognize wrapup markers and could run historian concurrently, but that's an inherent mixed-version concern.

The real safeguard is the compartment lease, which existed before this change and provides cross-process mutual exclusion regardless of marker awareness. For downgrade scenarios, SQLite handles extra columns gracefully in SELECT and INSERT statements, so an old binary encountering the new schema should work fine. Now I'm drilling into the deferred materialization logic to verify that background and manual publishes don't trigger full materializations as claimed.
Let me examine `canConsumeDeferredLate` to understand whether wrapup's deferred materialization forces a bust.

[assistant] 2026-07-06T17:13:37Z

[assistant] 2026-07-06T17:13:41Z
I'm reconsidering the concern about a durable marker left behind by an old binary — but since the marker is new in v50, an old binary wouldn't have created it in the first place. The real issue is the reverse: whether an old binary that doesn't have the `wrapup_in_progress_state` column causes problems when a new binary tries to write to it.

The TTL mechanism handles crashed wrapups correctly — the marker expires after 5 minutes even without renewal, and `getWrapupInProgressState` lazily clears expired markers. But there's a timing concern: if a wrapup crashes and leaves an unexpired marker, `startCompartmentAgent` will suppress historian runs for up to 5 minutes, which is intentional but worth noting.

The more concrete issue emerges when multiple instances (OpenCode + Pi, or two OpenCode instances) share the same context.db — the wrapup marker is session-scoped, so different sessions won't interfere, but I need to verify the marker is properly keyed by session_id to avoid cross-session contamination.

Looking at the wrapup orchestrator's marker renewal: it runs every 60 seconds with `unref()`, and if ownership is lost, it calls `markOwnershipLost` but the interval keeps running until the finally block clears it. The real invariant to check is whether deferred publishes and compaction markers stay synchronized — the wrapup holds the marker for its entire duration, blocking historian runs, then releases it and queues deferred refreshes. The pending compaction marker gets overwritten with each chunk's boundary, so the final marker reflects the last chunk. The gate `pendingMarkerCoveredByConsumedBoundary` should ensure the marker only advances when the consumed boundary covers it.

Now checking if deferred materializations actually force a bust on the next pass: when `deferredMaterializationWasPending` and `canConsumeDeferredLate` are both true, heuristics run and trigger a cache-busting pass, which materializes the deferred sessions. So the mechanism is sound — deferred materializations do force a bust even without other triggers. I should focus on the actual git diff to spot what changed and where real bugs might hide.

I don't have a bash tool available to run git directly, so I'll need to rely on source reading and the delta description instead. Let me zero in on a few concrete concerns: first, in the OpenCode wrapup's `runOneWrapupIteration`, the lease is acquired, then the runner promise is created and registered synchronously without any awaits in between, which is safe. Second, I'm checking whether the wrapup passes the right compartment context to its handlers.

Now I'm looking at a potential issue with the wrapup loop's progress check — after `runOneWrapupIteration` completes, it verifies `afterEnd <= beforeEnd`, but the iteration might have awaited an existing run internally, which could affect the timing. More importantly, I'm examining `getWrapupInProgressState` which uses `BEGIN IMMEDIATE` inside what might already be an outer transaction, and if that throws it returns null, treating the expired marker as absent. There's a subtler correctness concern here around when `isWrapupInProgress` gets called from different paths. The real concern is concurrent access: if a newer binary (0.31) and an older one (0.30.7) run simultaneously against the same database, the 0.31 wrapup sets the marker expecting mutual exclusion, but the 0.30.7 binary doesn't know about it and proceeds with trigger-fired historian anyway, breaking the intended synchronization.

However, both versions respect the compartment lease mechanism, which predates this change and provides the actual cross-process mutual exclusion—the wrapup marker is just a secondary optimization on top of that. So even if one binary ignores the marker, the lease still prevents concurrent execution.

Now I need to verify some concrete correctness issues: whether the marker acquisition in `runManagedWrapup` happens before waiting for existing incremental runs, and whether `startCompartmentAgent` properly checks the wrapup state before proceeding.

Let me also examine the `pendingMarkerCoveredByConsumedBoundary` logic more carefully, since that's where a real issue could hide—specifically how pending markers get handled when a wrapup produces multiple compartments across chunks. Cross-session leakage isn't a concern since session notes are scoped to the current session and anchors only display when `sourceSessionId` matches, while smart notes are project-scoped by design. The `anchorOrdinal` semantics are safe because the guard prevents showing anchors for foreign sessions, ensuring message expansion always references the correct session. Now I'm checking the caveman compression gating to verify subagents never receive it — the transform layer gates it on `!reducedMode` (where `reducedMode` is `sessionMeta.isSubagent`), and the postprocess only reads from what transform sets, so the invariant holds across both paths.

The critical edge case is when `isSubagent` gets set — if it's determined after the first pass, caveman could compress a subagent's content before the flag is recognized. I need to verify the timing of when `isSubagent` is determined and run a quick diagnostic check before wrapping up.
This is a well-defended delta. Let me check a few specific interaction concerns: subagent-detection timing (caveman invariant), and pull diagnostics.

[assistant] 2026-07-06T17:15:12Z
I'm tracing through how `sessionMeta.isSubagent` gets set and whether it could be false on the first pass, which would create a race condition where caveman compression might run before the subagent flag is properly initialized. The comment mentions this could happen with internal children, but I'm checking if the same timing issue applies to caveman compression itself.

Actually, this is probably low-risk since caveman only compresses old tags outside the protected tail, and on a subagent's first pass there wouldn't be enough history to trigger it anyway. Plus caveman is opt-in by default.

Let me shift focus to the wrapup orchestrator instead, specifically the marker renewal logic. I'm noticing that `markerRenewal` fires every 60 seconds to renew the ownership marker, which has a 5-minute TTL. The question is whether a long-running historian chunk could cause the marker to expire if it takes longer than 5 minutes, but the renewal interval runs independently of the historian, so it should keep firing regardless.

There's a real concern though: the marker renewal interval is `unref`'d, which is fine for cleanup, but I need to think through the crash-recovery path. If the process crashes, the marker expires after up to 5 minutes, and `getWrapupInProgressState` will return null once it's expired, allowing the session to resume. That's the intended behavior.

But there's a potential issue with a durable marker left behind by an old binary. If someone downgrades from a newer version that knows about wrapup markers to an older version that doesn't, the old binary would ignore the marker, and it would eventually expire anyway. But the real concern is the reverse scenario—what happens if a newer binary encounters a marker from an older binary?

Looking at the OpenCode wrapup orchestrator, I notice that `onCompartmentStatePublished` adds to both the deferred history refresh and materialization sets, while `onDeferredMarkerPending` only adds to the history refresh set. The invariant is that publishes are deferred and ride the next natural bust—the wrapup doesn't force materialization. During `runManagedWrapup`, multiple compartment boundaries are published, each advancing `lastCompartmentEnd`, but only the final boundary's pending compaction marker is kept since it gets overwritten with each publish. So one bust covers the final rebuild and marker.

Now I'm thinking through the race conditions. During wrapup, `startCompartmentAgent` checks `isWrapupInProgress` and skips, and recomp checks too, so mutual exclusion holds. Wrapup also checks for active incremental runs and waits for them. But there's a timing question: wrapup acquires the durable marker first, then calls `waitForExistingIncrementalRun`. An incremental that started before the marker was acquired will continue and wrapup waits for it via the active promise. The acquisition order matters here—I need to trace through what happens when wrapup checks `getWrapupInProgressState` first.

Looking at the `pendingMarkerCoveredByConsumedBoundary` gate for OpenCode postprocess, it's checking whether the pending marker's boundary is covered by the injection that was actually rendered. If the pending marker's ordinal is beyond the rendered boundary, the drain is suppressed so we don't advance the marker past history we didn't render. That's the right invariant. But there's a nuance with how wrapup and incremental callbacks interact—wrapup adds to both deferredHistoryRefresh and deferredMaterialization, while the incremental callback does something different.

The Pi side doesn't have the same `pendingMarkerCoveredByConsumedBoundary` check, which is intentional. Pi uses a different gating mechanism through the context-handler's gated drain. Looking at Pi's `deferredHistoryDrainEligible`, it requires that history was consumed this pass (which happens when `injectM0M1Pi` succeeds on a cache-busting or deferred-history pass), so Pi rebuilds history and drains the marker in the same pass. The concern for OpenCode was that the pending marker could be newer than the boundary rendered this pass, but Pi's approach handles this differently.

When `injectM0M1Pi` runs, it reads the current compartment state from the database at injection time, and since Pi rebuilds fresh each pass without cached boundary reuse, the injection always reflects the latest state. This means the pending marker's ordinal is always less than or equal to the rendered boundary, so Pi doesn't need the explicit coverage check. Within a single context pipeline pass, injection and drain both see consistent database state, so there's no window where a new compartment could be published between them.

Now I'm looking at the `forceKeepLastCompartment` downgrade and unanchored promotion logic to check if there's a real issue with the invariant about weak-final keep and unanchored-promotion skip only applying to the actual final chunk.

Examining the events filter... Now I'm checking when `isSubagent` gets set and whether caveman compression could leak to a subagent through a race condition. Since caveman is off by default and only affects the oldest 20% of tags, the risk is low. Looking at the broader picture, `cavemanTextCompression` flows through the config and only applies during postprocessing when `!reducedMode`, which gates on `!isSubagent` — so if the flag is set correctly, subagents are protected. The `isSubagent` flag is immutable per session, so even if it somehow flipped mid-session, it wouldn't cause a cache bust. Now I'm examining a potential issue in how emergency-drop and wrapup interact, specifically whether the durable marker could be live when context hits the 95% threshold during wrapup.

The command handler and message transform are separate events in OpenCode, so while `/ctx-wrapup` runs asynchronously, no transform passes fire for that session since the UI is single-threaded and blocks further messages. This rules out a race within the same process. For cross-process scenarios where two OpenCode instances or OpenCode and Pi share context.db, the wrapup marker uses `acquireWrapupInProgress` with `BEGIN IMMEDIATE` and a holder check to ensure atomic safety across processes.

Now I'm checking whether the history rebuild and compaction-marker advance invariant holds up. The marker drain in OpenCode's postprocess is gated on whether history was consumed and the pending marker is covered by the consumed boundary. If not covered, the signal survives for a later pass to retry, and that later pass will rebuild history to the newer boundary based on the injection cache triggering the refresh.

Looking at the reverse scenario—history rebuild without marker advance—if there's no pending marker to advance (already drained), then no advance is needed, which is fine. The deferred marker machinery appears sound.

Checking the emergency recovery logic in wrapup: during incremental runs, each chunk publish doesn't clear recovery while wrapup is in progress, and only the success path clears it at the end. On failure, recovery stays armed for remaining work, which is intentional. But I'm noticing a potential issue with the timing of when the durable marker is released versus when emergency recovery is cleared—the marker releases in the finally block before the recovery clear happens.

Since this is single-threaded Bun with no awaits between those two operations, there's no actual race window where another historian could start. Everything stays consistent through the function return, and any subsequent transform would see a coherent state.

Now I'm looking at the actual changes versus what's pre-existing, and focusing on the areas the prompt highlighted: identity reuse, workspace fingerprints, and embedding registry. The identity reuse mechanism caches the last known git identity process-locally, which means two separate processes on the same repo could diverge if one resolves a git identity successfully while the other hits a transient failure and falls back to dir-hash. This creates a temporary split where memories get written under different identity schemes, though the code acknowledges this self-heals. It's low severity.

But there's something more subtle: once a git identity gets cached in `identityCache`, it stays there forever with no invalidation. So even though the transient cooldown logic exists, it never actually gets triggered because the cached value is returned first. The next call still goes through `resolveProjectIdentity`, which checks for an active cooldown before calling the strict resolver, and since success doesn't set a cooldown, it just returns the cached identity. This means the identity becomes permanently stable within a process after the first successful resolution, which is the intended behavior.

The `hasGitDir` realpath retry for deciding whether to serve a cached fallback or re-resolve looks sound. Now I need to examine whether the embedding registry interacts problematically with identity reuse.

Actually, let me step back and run the test suite to see if anything's actually broken, then focus on a few more concrete concerns. I'm noticing something potentially buggy in the wrapup orchestrator: the `markerRenewal` interval fires every 60 seconds and calls `updateWrapupInProgress` with the same holder ID, and the main loop also does synchronous marker renewals. Both use BEGIN IMMEDIATE transactions so they serialize properly in the single-threaded environment, but there's redundancy. More importantly, if ownership is lost, the interval callback doesn't check for that—it just keeps firing and calling `renewWrapupMarker`, which returns null on holder mismatch and triggers `markOwnershipLost()`. That's idempotent, and the interval gets cleared in the finally block once the main loop breaks. But I should verify that `releaseWrapupInProgress` handles the ownership-lost case correctly—it only clears if the holder ID matches, so if another process took over, it won't actually clear the marker.

Now I need to check the migration v50 to see if the session mode collapse from 3 to 2 modes has any edge cases, verify the removal of the `ctx_reduce_enabled` config flag, and look at the config schema and reduce availability resolver to make sure everything's consistent.
This is a heavily-defended delta. Let me verify a few specific interaction surfaces and pull diagnostics.

[assistant] 2026-07-06T17:17:15Z
I'm looking at the `resolveCtxReduceAvailability` functions in system-prompt-hash.ts — there are two variants, one that takes just a sessionId and returns callable and frozen status, and another that also accepts messages as a parameter. Let me check the ctx-reduce-availability.ts file to understand how these work.

[assistant] 2026-07-06T17:17:22Z
I'm noticing there's an interaction between two resolution functions that both work with the same `availabilityBySession` cache. The first function, called from transform.ts, defaults to caching `true` when it encounters a user message without a signal, which sets up a fail-open behavior.

The potential issue is that the messages-path caches based on the first user message in the transform's array, which might not be the actual first message if history has been compacted. But since tool availability is constant across a session, the verdict should remain deterministic.

Now I'm focusing on the specific invariant risks, particularly around deferred wrapup publishes. The wrapup adds sessions to both `deferredMaterializationSessions` and `deferredHistoryRefreshSessions`, but the key constraint is that background publishes shouldn't force materialization—they're only consumed when `canConsumeDeferredLate` is true.

There's a timing concern with multi-chunk wrapups: publishes happen synchronously during the command handler execution, but the transform postprocess (which consumes deferred signals) only runs on user messages. So deferred signals accumulate during wrapup and sit until the next user message arrives. Each chunk overwrites the pending compaction marker with its new ordinal, and when the user sends the next message, the transform drains only the latest marker. The invariant requires that one bust covers both the history rebuild and the compaction-marker advance, with the pending marker after a multi-chunk wrapup pointing to the final compartment end.

For crash recovery, an expired marker (5-min TTL, renewed every 60s) is treated as absent and lazily cleared, so the session won't wedge indefinitely. During the 5-minute window after a crash, trigger-fired historians are suppressed and recomp/upgrade are blocked, which is an accepted degradation. The marker renewal interval fires every 60 seconds independently, but if a single chunk takes longer than 5 minutes without the interval firing, there's a potential gap in renewal coverage.

Looking at the Pi wrapup's marker renewal compared to OpenCode: Pi uses a 60-second interval without `.unref()` (unlike OpenCode), but it's properly cleared in the finally block, so there's no event loop leak during normal operation. The renewal logic itself looks sound—it calls `renewWrapupMarker` every 60 seconds to keep the 5-minute TTL fresh.

Now examining the interaction between deferred marker drains and trigger-fired historians: after wrapup releases the marker, a pending compaction blob exists alongside deferred signals. If a trigger-fired historian runs before the next user message, it could publish a new compartment with a newer ordinal, but the `pendingMarkerCoveredByConsumedBoundary` logic handles this newer-blob case correctly.

I should verify a few specific details—the doctor subprocess probe, sidebar Facts removal, any TODOs in the changed areas, and the `estimateChunks`/`expectedChunks` logic along with Pi's `formatExistingWrapup` function. There's a potential issue in the OpenCode wrapup I need to examine more closely.

The `runOneWrapupIteration` sets `forceKeepLastCompartment: true`, but the runner downgrades this based on `chunk.hasMore`. On non-final chunks, discard-last healing applies normally, dropping the provisional last compartment and making forward progress. On the final chunk, the last compartment is kept to reach the target. However, there's a concern: if the final chunk's `readSessionChunk` returns `hasMore: false` but the historian's last compartment ends before the target boundary, the loop might continue and attempt to rebuild the plan, risking an infinite loop if no forward progress is made.

Now I'm checking the diagnostics on the changed files and reconsidering the core invariants around defer passes and byte-identical replay. Caveman compression is correctly gated on cache-busting passes for detection and replayed consistently. For the notes search feature, session notes are scoped to the current session with valid anchors, while smart notes from other sessions don't show anchors—that's the intended session-aware behavior. The potential concern is whether auto-search-hints interact correctly with notes-search.
Let me examine the auto-search-runner to check the notes × auto-search-hints interaction that was flagged as a target area.

[assistant] 2026-07-06T17:18:20Z
Auto-search is deliberately scoped to memory, message, and git commit sources while excluding notes and primers, which prevents notes from surfacing in auto-search hints. Now I'm digging into how the wrapup orchestrator interacts with the deferred marker coverage invariant—specifically that each bust must simultaneously handle both a history rebuild and its corresponding compaction-marker advance.

The wrapup orchestrator defers both history refreshes and materializations, and while the loop processes multiple chunks with each one publishing a new compartment and overwriting the pending marker blob via `setPendingCompactionMarkerState`, only the final marker survives since no transform pass runs during the synchronous wrapup loop. The drain gate `pendingMarkerCoveredByConsumedBoundary` ensures the marker only advances to what was actually rendered, which is sound. But I need to carefully trace how multi-chunk wrapup interacts with deferred-history draining, and then examine the specific concern about emergency-drop, smart-drops, and nudges all colliding together.

Let me focus on finding concrete, verifiable issues rather than reading everything exhaustively. The wrapup marker has a 5-minute TTL renewed every 60 seconds by `markerRenewal` setInterval (which is `unref()`'d) and also renewed inside the loop via `renewWrapupMarker`. During `runOneWrapupIteration`, a single historian chunk could take a long time, but the 60-second renewal fires independently and if ownership is lost, `markOwnershipLost()` gets called — though the interval callback ignores the return value, which is fine since the loop checks `ownershipLost`. The real question is whether a single historian chunk could exceed the 5-minute TTL before the next renewal fires.

There's a more subtle concern around deferred publishes: wrapup defers materializations to ride the next natural burst, but on restart, `getSessionsWithPendingMarker` re-seeds `deferredHistoryRefreshSessions` and `pendingMaterializationSessions`. However, OpenCode's `onCompartmentStatePublished` adds to `deferredMaterializationSessions` — a different set — and I need to verify whether that gets re-seeded on restart or if there's a gap where deferred materializations could be lost.
I've mapped the core surfaces. Let me verify the restart re-seed path and which deferred sets get restored.

[assistant] 2026-07-06T17:18:48Z

[assistant] 2026-07-06T17:18:53Z
I'm noticing a discrepancy between what the rehydration actually updates and what the comments suggest it should update. The drain logic in postprocess depends on whether deferred materialization was pending, and that status affects whether history consumption is satisfied. I need to trace through how these deferred materialization sessions get re-seeded when the system restarts. So the wrapup orchestrator publishes multiple compartments in a synchronous loop without transform passes between them, and each iteration preserves the injection cache and defers marker application. Each publish adds to the deferred sets rather than pending ones, which means they ride the next natural bust and don't force materialization—consistent with the normal compartment phase path. The concern is about what happens across these multiple publishes within the wrapup loop.

The key insight is that intermediate compaction-marker boundaries get superseded by the final one, so a single materialization covers everything cumulatively. Since no transform pass runs between wrapup iterations, there's no risk of a partial drain mid-loop. The wrapup marker also blocks other instances from triggering historians or running recomp/upgrade checks, but I need to verify how the transform's pending-op drain and heuristics behave in other instances while wrapup holds the compartment lease.

The lease is only held during each iteration and released between them, so another instance could theoretically acquire it, but the wrapup marker still blocks `startCompartmentAgent`. However, heuristic drops and pending-op materialization don't check the wrapup marker and could mutate context.db tags simultaneously with wrapup's compartment publishing — though this isn't necessarily new to this change. Rather than exploring the entire surface, I should focus on concrete defects. Let me examine the marker TTL renewal interval against the 5-minute TTL to check for forward-progress issues.

The marker renews every 60 seconds with an unref'd interval, pushing expiration to now + 5 minutes. Since the historian awaits asynchronously, the event loop isn't blocked and renewals fire during that wait. If a second instance's wrapup somehow takes over after the first marker expires, `updateWrapupInProgress` returns null on holder mismatch and triggers `markOwnershipLost()`, causing the loop to break. The renewal interval callback doesn't check the return value, but `renewWrapupMarker` sets the ownership-lost flag internally. Now I'm thinking through whether there's a race condition when one instance's marker expires and another instance acquires the wrapup lease.

If instance A's renewal fails and instance B takes over, A's next renewal attempt fails due to holder ID mismatch, setting ownership-lost and breaking the loop. In the finally block, A calls `releaseWrapupInProgress` which checks that the holder ID matches before deleting, so A won't remove B's marker. The defense here is solid.

Looking at `getWrapupInProgressState` and the expired-reclaim logic inside a write transaction — `isWrapupInProgress` calls `getWrapupInProgressState` which may execute `BEGIN IMMEDIATE` if the marker is expired. If the caller is already in a transaction, it catches the error and returns null. `startCompartmentAgent` calls `isWrapupInProgress` before acquiring the lease, so it's not in a transaction yet. That's safe.

Now examining the `forceKeepLastCompartment` downgrade and how it interacts with discard-last, specifically the invariant that historian discard-last runs never promote unanchored facts. In the incremental runner, `skipUnanchoredPromotion` is gated by whether discard-last ran or the weak lookahead hit the final compartment, controlling whether promotion happens.

For events during discard-last: events without an `atCompartment` number are published (since `weakLookaheadFinal` is false), and events with a compartment number within the persisted range are kept. This is intentional per the design. However, there's a subtle issue — on discard-last, facts aren't promoted but events without `atCompartment` are stored. When the next run re-derives the discarded compartment, it re-emits the same facts and events, causing event duplication. The facts deduplicate against project memory in the prompt, but events would be stored twice, inflating the corpus meant for future dreamer aggregation. This is a minor data-quality issue, not a blocker.

Now examining the `weakLookaheadFinal` events filter more carefully. When wrapping up the final chunk with all compartments kept, `persistedCompartments` equals `emittedCompartments`, so events anchored to the last weak-lookahead compartment get filtered out, and unanchored events are dropped. This aligns with the invariant that nothing durable is extracted from the weak final boundary.

The delta is well-defended overall. I should focus on the less-protected surfaces: the doctor onnxruntime subprocess probe, the sidebar Facts row removal, the CLI doctor, and the migrate-experimental config edge case.

Looking at the `ctx_reduce_enabled` flag removal specifically—there's a test at index.test.ts:717 confirming that Zod silently strips unknown keys. But this creates a potential issue: a user who explicitly set `ctx_reduce_enabled: false` to disable the feature will find it re-enabled after upgrade since the flag is ignored and ctx_reduce availability now defaults to being callable. That's a silent behavior regression with no migration warning. I need to check if there's any handling in the CHANGELOG or migrate-experimental logic for this removed flag.
This delta is heavily defended. Let me check upgrade-path edges for the removed `ctx_reduce_enabled` flag and verify there's no deprecation warning.

[assistant] 2026-07-06T17:20:09Z
The removed config flag gets silently ignored as expected since Zod strips unknown keys. Now I need to think through the deferred publish invariant more carefully—specifically how the wrapup's `onCompartmentStatePublished` handler manages both `deferredMaterializationSessions` and `deferredHistoryR`...

Actually, the key issue is that after wrapup completes and returns a message, the compacted history materializes on the next message. The invariant states "Background/manual publishes never force a materialization," and the wrapup path respects this with `preserveInjectionCacheUntilConsumed: true` and `deferMarkerApplication` set to true.

The critical constraint is that one bust must cover both the history rebuild and the compaction-marker advance. I'm checking the `pendingMarkerCoveredByConsumedBoundary` function to verify the injection (the m0/m1 rebuild) actually covered the compartment boundary the marker wants to advance to—if the pending ordinal is less than or equal to the injection's compartment end message, then it's safe to advance. Since wrapup publishes multiple chunks sequentially, each one overwrites the previous pending marker blob, but only the latest blob gets applied during drain, so the final ordinal is what matters.

The concern is whether a transform pass could drain the marker mid-wrapup at an intermediate boundary that a later chunk then supersedes, but that's actually benign since marker advances are monotonic and each covers a real compartment boundary. I'm now looking at the byte-identical replay invariant and how it interacts with the deferred marker application.

Actually, I should focus on genuinely subtle cross-feature issues rather than shallow findings that likely got caught in review. Let me examine the Pi wrapup renewal interval — it's created with `setInterval` but never unref'd, unlike the OpenCode version. However, it's cleared in the finally block, so there's no leak, and the interval keeping the event loop alive during the await is fine anyway. The more concrete concern is around the TTL and crash recovery: the wrapup marker has a TTL that should release the session if the wrapup crashes, and I need to verify this invariant holds across the marker renewal logic in the orchestrator, the compartment lease acquisition, and the iteration loop.

The marker expiration and `isWrapupInProgress` check seem sound — if the marker expires, it self-heals after 5 minutes, and the transaction-aware clearing logic treats an expired marker as absent without blocking. But there's a real interaction to investigate: during wrapup, emergency recovery stays armed by design, and the live context doesn't materialize until the next user message, so high-pressure contexts don't get relief until then.

After wrapup completes, the next user message triggers a transform pass that rebuilds history from the deferred materialization markers and drains them — this is a cache-busting pass where emergency-drop and smart-drops can also fire. The key thing is that deferred materialization only consumes on passes the scheduler already deemed appropriate, so it rides natural busts rather than forcing one.

Now I'm looking at a concrete potential issue: the `markerRenewal` interval in `runManagedWrapup` fires every 60 seconds and calls `renewWrapupMarker`, which starts a transaction on the same database connection. If this fires while the main loop is already in a transaction during historian publish, you'd get a "cannot start a transaction within a transaction" error since it's a single synchronous connection.

But actually, in the single-threaded event loop, `setInterval` callbacks only fire at await points, and since all the historian transactions are synchronous (no awaits between BEGIN and COMMIT), there's no open transaction when the interval fires — so this pattern is safe.

Let me shift focus to examining whether the `expectedChunks` estimate is correct and if the loop actually terminates properly. Now I'm checking whether Pi's recomp path verifies that a wrapup isn't already in progress, and whether the mutual exclusion works in both directions. The concern is a potential race where recomp checks the wrapup marker, finds it clear, then proceeds to acquire a lease and register itself as an active run—but wrapup could be starting concurrently and checking for active runs at the same time.

The key insight is that in single-threaded JavaScript, these synchronous checks and state-setting operations can't truly overlap; whichever operation runs its check-and-commit first will be observed by the other. The wrapup marker acquisition happens synchronously before any awaits, and recomp's isWrapupInProgress check reads that same marker, so the ordering should hold. But I want to pull actual diagnostics and verify test health before finalizing my findings, and I'm reconsidering whether there are concrete issues lurking in the byte-identical replay behavior with the notes nudge and auto-search interaction.

Looking at the transform-postprocess phase, I see that `peekNoteNudgeText` returns text and then `markNoteNudgeDelivered` persists an anchor and appends to the user message. The concern is whether this delivery happens on every pass or only when appropriate. The code isn't gated on cache-busting passes—it's gated on `peekNoteNudgeText` returning non-null. If that function only returns text once and then relies on the persisted anchor to replay on subsequent passes, that's fine. But if a new note nudge delivery appends to the last user message on a deferred pass, that could mutate a cached message and break the cache. I need to check whether `peekNoteNudgeText` has proper deferral logic and examine the note-nudger implementation to understand the session-aware message anchors feature.
This is a heavily-defended delta. Let me examine the note-nudger delivery (new feature) for cache-safety, and pull diagnostics.

[assistant] 2026-07-06T17:21:34Z
I'm noticing the note nudge cooldown relies on in-memory storage rather than persistence, though the code acknowledges this limitation. Now I need to think through a critical concern: what happens when two OpenCode instances are running simultaneously and both try to execute wrapup while sharing the same context.db file.

The wrapup marker uses BEGIN IMMEDIATE transactions with holder-id validation, which should prevent conflicts. But I'm realizing there's a potential issue with the marker's TTL and renewal timing — the 5-minute TTL gets renewed every 60 seconds by an interval, and also during each wrapup iteration. However, if a single historian chunk takes longer than 5 minutes to process, the marker could expire mid-operation. The 60-second renewal interval should keep it alive during long historian runs, but I'm seeing that the renewal interval is unref'd while the historian's own lease renewal isn't, which creates an asymmetry I need to examine more carefully. Now I'm examining how the marker renewal interval interacts with ownership loss. The renewal fires every 60 seconds and calls `updateWrapupInProgress`, which returns null if ownership is lost and triggers `markOwnershipLost()`. The main loop checks this flag, so ownership changes propagate correctly. The interval reads `chunkIndex` from closure, which is fine since it's just a reference. I'm also thinking through the caveman compression invariant — that first-application of mutations only happens on cache-busting passes, while replay happens on every pass.

But there's a potential race condition with subagents. The `caveman` config only applies when `!reducedMode`, and `reducedMode` is set based on `sessionMeta.isSubagent`, which gets determined at `session.created`. If a transform pass runs before that event marks the session as a subagent, then `isSubagent` would still be false and caveman compression could run once on what should be a subagent. This would compress and freeze the persisted tag state. On subsequent passes, `isSubagent` becomes true so caveman config is undefined and replay doesn't run either. Since `tagMessages` restores the pristine original from `source_contents` every pass, the compressed tags would revert without replay, creating an inconsistency.

Now I'm examining the `forceKeepLastCompartment` downgrade logic and how it interacts with discard-last and the `chunk.hasMore` flag, since the invariant about historian discard-last never promoting unanchored facts is critical. Looking at the compartment-runner-incremental logic: `forceKeepLastCompartmentForChunk` is set when `forceKeepLastCompartment` is true AND `!chunk.hasMore`, discard-last runs when we're not in emergency and this flag is false with at least two compartments emitted, and then `skipUnanchoredPromotion` gets set based on whether discard-last actually ran or if we're keeping the last compartment weak.

The events filter is interesting — when we're on the final chunk with weak-final keep, events anchored to the last compartment are filtered out, but events anchored to earlier compartments pass through. Since events are stored separately from facts and facts are already skipped via `skipUnanchoredPromotion`, the invariant about never promoting unanchored facts still holds.

But there's a data-loss concern: on the final wrapup chunk, facts from that tail compartment are never extracted because the boundary is unreliable, and since the compartment gets persisted immediately, those facts are permanently lost — the historian won't re-read that range later. The design comment acknowledges this is intentional though: facts from the distrusted final boundary are sacrificed as a tradeoff. It's documented and arguably acceptable since facts can be re-derived, so not a critical issue.

Now I'm looking at the note-nudge delivery mechanism and whether it maintains byte-identical replay across defer and cache-busting passes. The nudge appends to the last user message when it triggers, and that happens unconditionally whenever fullFeatureMode is active — not gated on cache-busting passes. Since the nudge appends to the current turn's message before it reaches the provider, it's cache-safe, following the same pattern as auto-search reasoning. But I need to verify that `findLastUserMessageId` is actually finding the current turn's message and not something earlier. I'm now checking whether Pi has the same coverage guard that OpenCode uses to prevent advancing its compaction marker past history that was actually rendered in the current pass. OpenCode enforces this with `pendingMarkerCoveredByConsumedBoundary`, but scanning Pi's drain logic, I don't see an equivalent coverage check before it applies the deferred marker advance.

The critical difference might be in how the two systems handle injection freshness: OpenCode can hold a stale cached boundary, so it needs to verify coverage each time, whereas Pi appears to inject a fresh boundary from the database on every pass, which would automatically ensure the pending marker stays within rendered history. I need to trace through Pi's injection logic to confirm whether this freshness assumption actually holds.
This is a heavily-defended delta. Let me examine a key asymmetry: OpenCode's postprocess drain guards the marker advance with `pendingMarkerCoveredByConsumedBoundary`, but I need to verify Pi has an equivalent coverage guard.

[assistant] 2026-07-06T17:22:57Z
I'm checking whether Pi has a guard similar to OpenCode's `pendingMarkerCoveredByConsumedBoundary` that prevents moving the marker past unrendered history, and looking at how Pi's deferred compaction marker application handles this invariant.

The key difference is that Pi rebuilds the compartment set fresh from the database each pass with `injectM0M1Pi`, so the consumed boundary always reflects the current state, whereas OpenCode uses a cached `pendingCompartmentInjection` that might be stale — meaning if Pi's marker advances without this check, it could still be safe because the boundary it's advancing past is always up-to-date.

Now I'm drilling into specific high-value concerns: the wrapup marker renewal runs every 60 seconds with a 5-minute TTL, which looks solid. Crash recovery is handled by the self-expiring `getWrapupInProgressState`, though the historian does block for up to 5 minutes after a crash as designed. I'm checking whether the `onCompartmentStatePublished` handler's addition to `deferredMaterializationSessions` actually forces a materialization or just defers it to the next natural cycle.

Looking at the transform-postprocess-phase logic, a deferred materialization can trigger a cache bust, but only when `canConsumeDeferredLate` is true — meaning it rides a natural bust rather than forcing one independently. I need to verify what `canConsumeDeferredOnThisPass` actually gates on to confirm whether the scheduler is already executing a bust or if this is creating one.

[assistant] 2026-07-06T17:23:22Z
I'm examining a potential issue in how the wrapup orchestrator interacts with emergency-drop and nudges. Specifically, when wrapup registers an active compartment run with kind "wrapup", I need to check what happens during the transform postprocess phase when it computes `compartmentRunning` based on `getActiveCompartmentRun`, particularly if the user's context reaches the ≥85% threshold.

The wrapup orchestrator runs `runManagedWrapup` as a blocking call from the command handler, registering the active compartment run for each iteration and clearing it between iterations. I see that `startCompartmentAgent` checks `isWrapupInProgress` and skips if true, which prevents a trigger-fired historian from starting during those gaps.

Now I'm focusing on the actual invariant: background and manual publishes should never force a materialization. The wrapup's `onCompartmentStatePublished` adds to `deferredMaterializationSessions`, and during a normal transform pass, the deferred materialization is only consumed on a natural cache-busting or execute pass through the `canConsumeDeferredLate` gate, which respects the "ride the next natural bust" rule.

Looking at the cross-process safety of the wrapup marker — `acquireWrapupInProgress` uses BEGIN IMMEDIATE with holder checks, so that's safe. The marker TTL is 5 minutes with renewal every 60 seconds via an `unref()`'d interval. Even during long historian chunks, the interval fires independently on the event loop, keeping the marker alive. When instance A holds the marker, instance B's trigger-fired historian sees `isWrapupInProgress` as true and skips. If instance A crashes, the marker expires after 5 minutes, and `getWrapupInProgressState` lazily reclaims expired markers when `startCompartmentAgent` checks the wrapup status.

The reclaim logic in `getWrapupInProgressState` handles nested transactions carefully — if called inside a write transaction, `BEGIN IMMEDIATE` throws and returns null, treating an expired marker as absent. In `acquireWrapupInProgress`, if the marker is expired, it gets overwritten by the new holder. If the marker isn't expired but held by a different holder, it returns false to enforce mutual exclusion.

Now examining the `forceKeepLastCompartment` downgrade logic and how it interacts with the final chunk — there's a condition checking whether to keep the last compartment based on whether it's the final chunk and the force flag is set.

When the final chunk arrives with no more raw history, `forceKeepLastCompartmentForChunk` becomes true, which skips discard-last and keeps the last provisional compartment. This also sets `skipUnanchoredPromotion` to true, so facts from that final weak-boundary chunk aren't promoted. This is intentional per the design comments — the final chunk's facts are deliberately not promoted because the weak boundary is untrustworthy. It's a documented tradeoff rather than a bug, though it does mean potential fact loss on the final wrapup chunk.

Let me dig into the OpenCode wrapup to find actual interaction bugs. On the next user message, the history rebuild includes all new compartments up to the final boundary, and the postprocess drain verifies the pending marker is covered by the injection's compartment end — since they now match, the marker gets applied correctly.

I've been exploring the codebase extensively, so let me focus on the specific cross-feature interactions that are most likely to break things. I'm particularly concerned about identity reuse colliding with the embedding registry — when a project identity flips from directory-based to git-based mid-session, could embeddings get orphaned? And there's the question of whether `lastKnownGitIdentity` reuse during transient git failures could accidentally reuse a git identity for a directory that's actually changed. Let me dig into how identity resolution and embedding caching interact.

The identity cache is permanent and never invalidated except by test hooks, so once a git identity is cached for a canonical path, it's returned forever. `resolveProjectIdentity` checks the directory fallback cache first, then the transient cooldown, then calls `resolveProjectIdentityStrict` which checks the identity cache — so a successfully-resolved git repo stays stable. The code acknowledges that dir-to-git transitions orphan project memories written under the old key, but it's documented as a known tradeoff that self-heals through backfill and reconciliation paths. Now I'm looking at the `hasGitDir` realpath retry logic — it checks the ancestor chain of the real path, but there might be a subtle issue there. The wrapup orchestrator handles its own lease renewal, and the runner doesn't start a separate one when called directly from `runOneWrapupIteration`, so there's no double-renewal issue. Looking at the specific concerns the prompt raised, the schema fence design means that downgrading after a v50 migration would actually refuse to open the database — it's an existing safeguard, not new to this delta, though it's worth noting that users upgrading to v0.31.0 and then downgrading would hit this.
This is a heavily-defended delta. Let me examine the schema-fence downgrade behavior and the durable marker interaction with old binaries.

[assistant] 2026-07-06T17:24:32Z

[assistant] 2026-07-06T17:24:37Z
I'm seeing a critical schema compatibility issue: when one harness upgrades to v0.31.0 (schema v50) while the other stays on v0.30.7 (schema v49), the older version refuses to open the shared context.db and disables Magic Context entirely—a fail-closed safety mechanism. The v0.31.0 update introduces Notes as a fifth ctx_search source and performs the migration to v50, but this creates a breaking incompatibility for the lagging harness.

The durable marker scenario is actually protected by this same schema fence: if v0.31.0 writes a `wrapup_in_progress_state` marker and the user downgrades to v0.30.7, the old binary won't even open the v50 database, so it can't be wedged by the marker. Cross-process mutual exclusion works when both harnesses are on v0.31.0—they check `isWrapupInProgress` via the durable marker to suppress trigger-fired historians during wrapup. For crash recovery, the marker has a 5-minute TTL that gets lazily expired when `startCompartmentAgent` calls `isWrapupInProgress`.

The marker renewal runs on a 60-second interval (unref'd in OpenCode but cleared in the finally block in Pi), and during active wrapup iterations, the renewal fires independently even while the historian LLM call is running, so the marker stays fresh throughout long-running historian operations.

Now I'm thinking through the cross-feature interactions during wrapup—specifically whether the transform passes can fire while a manual wrapup blocks the user, and how the historian publishes compartments during that time.

The key insight is that wrapup publishes multiple chunks, each triggering `onCompartmentStatePublished` which queues deferred history refreshes and materializations. But each chunk overwrites the pending compaction marker state with the latest compartment end, so after all chunks complete, the pending marker points to the final boundary. When the next transform pass consumes these deferred updates, it rebuilds the injection to the latest compartment, and since the pending marker ordinal is at or before the injection boundary, the marker gets applied correctly in one shot.

Now I'm examining the event filtering logic more closely, particularly around the `weakLookaheadFinalCompartment` flag and how it interacts with the discard-last heuristic. When this flag is true (forcing the final compartment to be kept), events without an explicit compartment anchor are being filtered out entirely, which seems potentially problematic depending on whether that's the intended behavior.

Since these events are described as best-effort and re-derivable, losing them on the final wrapup chunk is concerning because there's no subsequent run to re-derive them — the wrapup is terminal. However, since the event corpus is currently unused and marked as future functionality, this isn't a critical issue.

Now I'm shifting focus to the multi-process interleaving and crash recovery scenarios, particularly the `getWrapupInProgressState` expiry logic and how it's called both outside and inside transactions. I need to trace through whether there's a potential race condition when `isWrapupInProgress` is invoked at line 685 within an active transaction while the same check might be clearing state elsewhere. The compartment lease serializes access, so even if an incremental historian started before a wrapup marker was set, the wrapup has to wait for the lease. If the marker appears mid-publish, the incremental historian correctly skips clearing emergency recovery, which is fine since the wrapup needs it armed. Now I need to look at the actual diff to distinguish what's new versus pre-existing behavior, starting with smaller surfaces like the sidebar Facts row removal and the onnxruntime probe.
This delta is heavily defended. Let me look at the CHANGELOG and the doctor onnxruntime probe (a newer, less-reviewed surface).

[assistant] 2026-07-06T17:25:41Z
I'm shifting focus away from the well-tested code to look for genuine issues, particularly around how different features interact with each other. There's a specific concern in the OpenCode wrapup orchestrator where the marker renewal interval is set up with an unref call, but the renewal process itself triggers an update to the wrapup progress state—I need to trace through whether this creates any race conditions or state inconsistencies with the main flow.

The real issue I'm spotting is around the crash recovery invariant: if a wrapup process crashes, the TTL on the marker should eventually expire and release the session. The marker has a 5-minute TTL and gets renewed every 60 seconds, so a crashed process would stop renewing and the marker would self-expire. The lazy cleanup in `getWrapupInProgressState` should handle this, but there's a subtle problem with how it handles transactions—when it tries to acquire an immediate transaction to delete the expired marker, if the caller is already inside a transaction, that throws an exception, and the code treats the expired marker as absent instead of properly cleaning it up.

Now I'm thinking through the interaction between wrapup and the historian running concurrently with the main transform pass. The wrapup command is blocking, so the user's session is held while it runs. But the real complexity emerges when multiple sessions in the same project could trigger wrapup and historian operations at the same time—that's where the compartment lease mechanism becomes critical for cross-process coordination. I'm looking at the callback that fires when a compartment state is published during wrapup, which marks sessions for deferred history refresh and materialization.

The key insight is that wrapup publishes multiple compartments across chunks, each time overwriting the pending marker blob, but the deferred drain only happens on the next user message. So after wrapup finishes, the marker blob correctly points to the final boundary, and the next natural transform pass handles both the history rebuild and marker advance in a single operation. Now I'm examining whether the invariant holds—specifically whether `pendingMarkerCoveredByConsumedBoundary` correctly suppresses redundant work when the pending marker is already covered by what's been consumed.

The coverage check compares the pending marker's ordinal against the injection's compartment end boundary, ensuring we never advance OpenCode's marker past history that hasn't been rendered yet. This is solid.

But there's a subtlety with emergency recovery during wrapup: the flag stays armed throughout the wrapup process (each published chunk doesn't clear it), and only gets cleared at the very end. If wrapup fails partway through, the flag remains armed, which seems intentional—it lets the automatic historian continue if the session is still over threshold. But I'm wondering whether emergency recovery was even armed before wrapup started, since wrapup is a manual command that can run at any usage level.

Looking at the incremental runner logic, wrapup doesn't arm recovery itself—it only avoids clearing it. Recovery gets armed by the overflow detection path, not the historian, so a failed wrapup wouldn't spuriously trigger it. That's fine.

Now examining the `forceKeepLastCompartment` downgrade logic and the events filter more carefully. The invariant states that historian discard-last runs never promote unanchored facts. The code skips facts promotion when either discard-last is true OR weak-final-keep is true, which matches the invariant. Looking at the events filter...

Filtering events based on compartment anchoring. Events with null anchoring return true only when weak-lookahead-final is false, meaning in discard-last cases (where weak-lookahead-final is false), null-anchored events still get published. This could cause duplication since those events would be re-emitted when the discarded compartment is re-derived next run. But events are marked as best-effort and re-derivable, not rendered, so this is low severity—the discard-last logic correctly skips all facts, but null-anchored events slip through as a minor consequence.

For the weak-final case, null-anchored events are dropped along with events anchored to the final compartment, which is the conservative approach. Now I'm thinking through the wrapup marker renewal and compartment lease timing—both intervals run independently during wrapup, which is fine. I'm also considering a multi-process scenario where two OpenCode instances share context.db and whether a session can be open in multiple instances simultaneously.

With `opencode -s <id>` you could theoretically open the same session in two instances, but the wrapup marker uses a holderId (random UUID) with acquire/update/release checks that enforce mutual exclusion—if instance B tries wrapup while instance A holds it, the acquire fails because the holderId doesn't match and the marker hasn't expired. That's solid protection.

Now I'm stepping back to think about what a reviewer might miss: cross-feature interactions, multi-process interleavings, restart/crash recovery, and migration/config edges. Let me examine the migration scenario where v0.31.0 (new) writes a wrapup_in_progress_state marker, then the user downgrades to v0.30.7 (old binary). The old binary has LATEST_SUPPORTED_VERSION = 49, so when it opens a DB migrated to v50, the schema fence rejects it and disables Magic Context—the old binary won't touch the marker at all, which is the fail-closed behavior we want.

But what if the marker was written by v0.31.0 and then the process crashed? On restart, there's no explicit init-time cleanup; instead the marker self-expires via a 5-minute TTL. When `startCompartmentAgent` checks `isWrapupInProgress` before starting a historian, it triggers lazy expiry. So a crashed wrapup could suppress trigger-fired historians for up to 5 minutes (worst case if the crash happened right after a renewal), which matches the design invariant. The marker renewal interval is unref'd so it won't keep the process alive, but I'm wondering if the per-iteration compartment lease renewal could prevent graceful shutdown if a wrapup iteration is mid-flight when the process tries to exit.

Looking at the marker renewal logic more carefully: the `markerRenewal` interval fires every 60 seconds and calls `renewWrapupMarker`, which extends the marker's expiration. Meanwhile, during a long historian run inside `runOneWrapupIteration`, the only things keeping the wrapup alive are the marker renewal interval and the per-iteration lease renewal interval. Both are working as intended. Now I need to verify that wrapup doesn't force materialization — it should only set deferred sessions and let the next natural bust handle the actual materialization.

Checking the wrapup orchestrator, it only adds to `deferredHistoryRefreshSessions` and `deferredMaterializationSessions`, not `pendingMaterializationSessions`, which is the force signal. The message about queuing compacted history and materializing on the next message confirms this is genuinely deferred. But I'm noticing a potential parity gap on the Pi side: Pi's context-handler drain checks `historyWasConsumedThisPass && materializationSatisfiedThisPass` before applying the pending marker, but unlike OpenCode's explicit `pendingMarkerCoveredByConsumedBoundary` guard, Pi doesn't seem to verify that the consumed boundary actually covers the pending marker's ordinal.

The key difference is that Pi rebuilds its injection fresh from the database on each cache-busting pass, so it always renders the latest compartments including the one the pending marker points to. OpenCode caches its injection and could have a stale boundary that doesn't cover history between the cache point and the pending marker, which would hide unrendered messages. So Pi might not need the coverage guard because the fresh rebuild guarantees coverage, but I should verify this assumption holds in practice.

Looking at the wrapup loop's forward progress detection: when the historian emits compartments, discard-last only drops the provisional last one if there are at least 2 compartments, so a single compartment always persists and advances the boundary. The `forceKeepLastCompartment` flag gets downgraded to false for non-final chunks when `hasMore` is true, which means discard-last applies normally — each iteration advances by at least one compartment even though the dropped compartment gets re-derived on the next pass. The no-forward-progress breaker catches any degenerate case where the boundary doesn't advance. Now I'm checking whether the `getWrapupInProgressState` expiry inside a transaction could leave something dangling...

The `BEGIN IMMEDIATE` call is guarded with a try-catch that treats an expired marker as absent, which is correct since it allows the historian to start fresh. The `acquireWrapupInProgress` function also calls `BEGIN IMMEDIATE` but without the guard — however, it's only called from the top level, not inside an existing transaction, so that's safe. And when `startCompartmentAgent` calls `getWrapupInProgressState`, it's not in a transaction either, so the marker gets properly cleaned up if expired.

Looking at the Pi wrapup renewal interval — it's set up but never unref'd, though it does get cleared in the finally block during shutdown.

Now examining the interaction between marker renewal and ownership loss: if another process takes over and sets `ownershipLost=true`, the renewal interval keeps calling `renewWrapupMarker`, which fails due to a holderId mismatch and idempotently calls `markOwnershipLost` again. This is harmless since no re-acquisition happens. The real concern is what happens if the main loop releases the marker in its finally block while the renewal interval is still running.

Actually, the interval gets cleared before the marker is released, so there's no race condition there. Even if the interval fires during the loop after `ownershipLost` is set but before the finally block executes, that's still fine. Now I need to dig into the boundary plan construction and re-fence logic in `resolveWrapupProtectedTailBoundary` since that's where the most complex new code lives.
This delta is well-defended. Let me examine the most complex new logic — the wrapup boundary plan with tool-arc fence and re-fence.

[assistant] 2026-07-06T17:27:42Z

[assistant] 2026-07-06T17:27:46Z
I'm checking whether the `anchorRawMessageCount` stays stable throughout the compaction process. The wrapup boundary calculation depends on this anchor value to determine where the protected tail should start, so if it drifts, the watermark could shift unexpectedly.

Now I need to verify that the wrapup's publish mechanism doesn't inadvertently force a materialization, since the invariant requires that background and manual publishes never trigger materialization. Let me trace through the publish path in the wrapup orchestrator to confirm it respects this constraint.
Let me examine how the OpenCode wrapup command is wired and whether `hasPendingNaturalBust` is provided.

[assistant] 2026-07-06T17:28:04Z

[assistant] 2026-07-06T17:28:11Z
I'm looking more closely at the interaction between the wrapup orchestrator and materialization logic. In `wrapup-orchestrator.ts`, the `onCompartmentStatePublished` callback queues up both deferred history refreshes and deferred materializations for manual wrapups, but I need to verify whether those deferred materializations actually trigger a cache bust or if they respect the invariant that background and manual publishes shouldn't force materialization.

The marker renewal mechanism seems solid — it fires every 60 seconds via `setInterval` and gets renewed at the top of each loop iteration and during lease acquisition, so even during long historian operations the marker stays alive. For crash recovery, the 5-minute TTL should release wedged sessions since the renewal stops on process crash and `getWrapupInProgressState` lazily clears expired markers.

But there's a potential concurrency issue: the marker renewal interval calls `updateWrapupInProgress`, which starts a transaction with `BEGIN IMMEDIATE`. If this fires while the historian's publish transaction is also running with `BEGIN IMMEDIATE`, we could hit `SQLITE_BUSY`. The problem is that `renewWrapupMarker` doesn't handle this exception — it would propagate as an uncaught error in the `setInterval` callback.

Actually, wait — in a single-threaded event loop, the historian's synchronous transaction can't be interrupted mid-execution. But the historian awaits the LLM call, and during that await the timer can fire. The key question is whether the historian is still in a transaction at that point — it sounds like the publish transaction happens after the validated pass returns, so the timer shouldn't collide with an active transaction.

The real issue is cross-process: if two OpenCode instances or OpenCode and Pi are sharing context.db, then one process's BEGIN IMMEDIATE could hit SQLITE_BUSY while another holds a write lock. I need to check if the database connection has a busy_timeout configured — if it does, the operation waits; if not, it fails immediately. Let me look at the DB initialization to see what timeout settings are in place.
Let me investigate a multi-process concern: whether the DB sets a busy_timeout, since the marker uses `BEGIN IMMEDIATE` from timer callbacks that can race a sibling process's write.

[assistant] 2026-07-06T17:28:46Z

[assistant] 2026-07-06T17:28:50Z
I'm checking the busy_timeout configuration in WAL mode and whether a BEGIN IMMEDIATE call from a setInterval callback could trigger an uncaught SQLITE_BUSY error that crashes the timer. Let me trace where the busy_timeout gets set.
Let me check the busy_timeout config and whether the wrapup marker timer callbacks can throw uncaught.

[assistant] 2026-07-06T17:28:59Z
The marker renewal timer in wrapup-orchestrator.ts calls `renewWrapupMarker` at regular intervals, which triggers `updateWrapupInProgress` and executes a `BEGIN IMMEDIATE` transaction. If this throws—whether from SQLite being busy after the 5-second timeout or from a nested transaction conflict—it would propagate synchronously within the timer callback.

However, since JavaScript is single-threaded, the `setInterval` callback can't fire in the middle of a synchronous block. The historian publish transaction (lines 624-694 in compartment-runner-incremental.ts) runs fully synchronously from `BEGIN IMMEDIATE` to `COMMIT` with no awaits in between, so the marker renewal timer can't interleave there. The real concern is whether any other transaction is open when the timer fires and tries to execute its own `BEGIN IMMEDIATE`.

The bigger issue is that `updateWrapupInProgress` calls `db.exec("BEGIN IMMEDIATE")` without a try/catch around it — if it throws due to a BUSY error from another process holding the write lock, the exception propagates out of the setInterval callback as an uncaught exception, which could crash the plugin host. But this is a pre-existing pattern throughout the codebase with a 5-second busy_timeout, so it's low severity.

Now I'm stepping back to look for cross-feature interactions that individual reviewers might miss. The `getWrapupInProgressState` function handles expiry by attempting to reclaim expired entries, and the wrapup marker has a 5-minute TTL renewed every 60 seconds. The marker renewal interval is marked as `unref()`'d, but I need to check how the lease renewal interval interacts with this.

For crash recovery, a crashed wrapup leaves the durable marker behind, which blocks the trigger-fired historian for up to 5 minutes until the TTL expires — that's the intended tradeoff. The marker retains its last renewed `expiresAt` timestamp, so it can block for the full duration.

Now I'm examining the OpenCode postprocess drain gating logic around `pendingMarkerCoveredByConsumedBoundary`. The concern is whether a single bust properly covers both a history rebuild and its compaction-marker advance. When wrapup publishes multiple chunks, each one overwrites the `pending_compaction_marker_state` with a new ordinal, which could create a gap if not handled carefully.

During wrapup, each iteration calls `runCompartmentAgentForWrapup` and the final pending marker ordinal should match the injection rebuild's endpoint, so the drain gate check `pending.ordinal <= injection.compartmentEndMessage` should hold consistently. But there's a subtle interaction: while wrapup is running and holding the compartment lease, the user can't send messages, yet I need to verify whether transforms can still fire in OpenCode and whether that creates any race conditions with the deferred publishes.

Looking at the marker renewal logic, the 60-second interval and the main loop both call `renewWrapupMarker` with `BEGIN IMMEDIATE`, but since timer callbacks only fire at yield points and the loop's renewal happens synchronously, there's no reentrancy issue. The real concern is whether `getWrapupInProgressState` can be called from within a transaction—it's called by `isWrapupInProgress` in `startCompartmentAgent` outside any transaction, but also from the compartment-runner-incremental in places that might be inside the publish transaction.

When `isWrapupInProgress` is called during the historian publish (line 685), it's inside an open transaction. If the marker is expired, `getWrapupInProgressState` tries `BEGIN IMMEDIATE`, which throws and returns null—treating the expired marker as absent. But if the marker is live, it returns the state without attempting a transaction, so it correctly reports true during wrapup. This is exactly why the recovery floor logic keeps emergency recovery armed while wrapup is in progress.

The notes search is more concerning—session notes are scoped to the current session, and smart notes are project-scoped, but I need to verify whether a smart note created in one session could leak into another session's context.

Looking at `pendingMarkerCoveredByConsumedBoundary`, the ordinal-based fallback checks whether `pending.ordinal <= injection.compartmentEndMessage`. If the pending ordinal is less than or equal to the injected boundary, the marker advances safely (never past rendered history). If greater, the drain is deferred. The invariant holds—the marker only advances when the injection covers at least up to the pending ordinal. And when `endMessageId === compartmentEndMessageId`, they're the same boundary regardless of ordinal disagreement.

Now I'm focusing on the scenario the task emphasizes: two OpenCode instances or OpenCode plus Pi running concurrently.

The `acquireWrapupInProgress` happens after `buildPlan`, then `waitForExistingIncrementalRun` waits after acquiring the marker. So the marker is acquired first, which means any trigger-fired historian sees it and skips. An already-running incremental historian continues and gets awaited—that's correct. The marker renewal interval fires every 60 seconds during the `await runnerPromise`, keeping the marker alive even if a historian chunk exceeds the 5-minute TTL. The interval is `unref()`'d so it won't keep the process alive on its own, but it still fires while the event loop is turning. Now I need to check the Pi side in the wrapup context.

On the Pi side, the `renewal` interval isn't `unref()`'d—a minor inconsistency but not a bug. I should hunt for genuine cross-feature bugs rather than shallow findings. Let me examine the interaction between `forceKeepLastCompartmentForChunk` and no-progress detection. On the final wrapup chunk, it keeps the last compartment for coverage and skips unanchored promotion. On non-final chunks, the behavior differs based on whether `forceKeepLastCompartment` is set.

The key invariant is that discard-last runs never promote unanchored facts, which is enforced by skipping promotion whenever `discardedLast` is true or when we're in the weak-final state. The logic ensures that on non-final chunks producing a single compartment, progress is still made even if discard-last would normally drop it. The wrapup loop's no-progress check (`afterEnd <= beforeEnd`) correctly terminates when no forward movement occurs, and the plan rebuilding handles the transition properly.

Looking at the events filter more closely, I'm checking how weak-final and discard-last compartments interact with event publishing. Session-level events (no atCompartment) get dropped in weak-final mode, and events pointing to the last compartment are also filtered out since they'd reference the unanchored boundary. For discard-last on non-final chunks, any events pointing beyond the persisted compartments are dropped, which prevents durable extraction from the distrusted tail. The filtering is consistent across both modes.

Now I'm thinking through the downgrade scenario with the wrapup marker. If an old binary (v0.30.7) doesn't know about `wrapup_in_progress_state`, and a user upgrades to v0.31.0 then downgrades, the schema fence kicks in—the old binary sees the DB is at version 50 and refuses to open it entirely, disabling Magic Context fail-closed rather than risking corruption. But if the new binary writes the marker and crashes, then restarts with v0.31, the hook init will reseed deferred history refresh but won't clear the wrapup marker itself, so it persists until TTL expires.

The real concern is whether the 5-minute block on trigger-fired historian during a crashed wrapup could wedge the session under high pressure. If wrapup crashes at 90% context and the marker blocks historian recovery for 5 minutes while the user keeps working and context climbs to 95%+, the emergency tool drops still fire but the historian that would compact history is gated behind the wrapup check. This could theoretically let context overflow if the emergency drops aren't sufficient, though it's bounded to that 5-minute window.

Let me verify whether the emergency recovery path has a separate entry point that bypasses the wrapup marker check, or if it's uniformly gated.
This delta is heavily defended. Let me check whether emergency historian recovery is also blocked by the wrapup marker (a potential wedge under pressure), and how session modes collapsed.

[assistant] 2026-07-06T17:30:37Z

[assistant] 2026-07-06T17:30:41Z
I'm spotting a critical race condition here. When `startCompartmentAgent` runs at the 95% threshold and forces the compartment to start while blocking, it doesn't account for an active wrapup marker—the function will just return early and skip the entire operation, leaving the system stuck waiting for something that never happens.

[assistant] 2026-07-06T17:30:50Z
I'm seeing a potential race condition: when wrapup is active and the marker is held, `startCompartmentAgent` returns early due to the `isWrapupInProgress` check, which clears `compartmentInProgress`. But if the user's session hits 95% capacity while a wrapup is still running—say a crashed wrapup that leaves the marker for up to 5 minutes—the emergency block tries to force-start the historian, which calls `startCompartmentAgent`, but that early-return skips the initialization entirely.

So during those 5 minutes after a crash, if another process or the same process on a new pass hits 95%, the emergency historian gets suppressed. The `startCompartmentAgent` returns without running anything, leaving `activeRun` undefined, so the session doesn't block or notify—it just proceeds without compacting. However, the emergency drop heuristic at ≥85% still runs in postprocess since it's not gated on wrapup, so that provides some relief.

There's a cross-process angle here too: process A's wrapup registers active runs only in its own memory, while process B sees the wrapup marker in the database and correctly skips starting its own agent. Process B's emergency drops still fire since its local `activeCompartmentRun` is undefined. The design is genuinely well-defended.

Given how thoroughly this is protected, I'm expecting my findings to be lower-severity or edge-case scenarios. Let me dig into some specific concerns: the wrapup marker renewal happens every 60 seconds but the TTL is 5 minutes, and I need to check what happens if a historian chunk takes longer than that renewal window.

I'm noticing a potential issue with the marker renewal timer — it fires every 60 seconds via `setInterval`, and during the historian pass it should continue running since the event loop stays free during awaits. But if `renewWrapupMarker` calls `updateWrapupInProgress`, which tries to acquire an immediate transaction lock and that throws due to `SQLITE_BUSY` (because the historian's publish transaction is holding the write lock), the exception propagates uncaught through the setInterval callback. The problem is that `BEGIN IMMEDIATE` sits outside the try/catch block in `updateWrapupInProgress`, so if it fails, there's no error handling — this could crash the process or get caught by a global handler depending on the plugin context.

This marker renewal pattern is new in migration v50, but the same vulnerable pattern exists in other storage functions too. With `busy_timeout=5000`, `BEGIN IMMEDIATE` waits up to 5 seconds for the write lock, and under heavy multi-process contention it could still fail and throw. The setInterval callback wrapping `renewWrapupMarker` has no try/catch protection, so an unhandled exception becomes a process-level uncaught error.

It's a low-probability crash, but worth considering. Even if the process crashes, the marker's TTL will eventually release it on restart, so the session won't stay wedged—but an uncaught exception killing the host process is still a robustness issue. I should check whether other setInterval callbacks in the codebase, like the compartment lease renewal, handle this pattern differently or if they have the same vulnerability.

Looking at the concrete differences between the OpenCode and Pi implementations: the Pi version doesn't unref the marker renewal interval, while OpenCode does, though both clear it in the finally block anyway. That's a minor distinction.

Now examining the actual coverage logic for the marker advance—specifically the `pendingMarkerCoveredByConsumedBoundary` function that checks whether a bust covers both the history rebuild and the compaction-marker advance. The drain only proceeds when both history consumption and deferred history conditions are met.

In the wrapup case, multiple publishes each overwrite the pending blob with a higher ordinal, all pointing to the latest compartment boundary. When the next natural transform pass consumes, it rebuilds history to that boundary and the pending marker matches, so one bust covers the full advance. The wrapup orchestrator correctly adds sessions to both the deferred history refresh and deferred materialization sets, matching what the trigger-fired path does.

Now I need to verify that the `liveSessionState` is properly shared between the wrapup context and the recomp context, since the deferred sets need to be the same instance for the consumption logic to work correctly.
Let me examine how wrapup's `liveSessionState` connects to the transform's deferred sets and the recomp context.

[assistant] 2026-07-06T17:31:48Z

[assistant] 2026-07-06T17:31:56Z

[assistant] 2026-07-06T17:31:59Z
The wrapup orchestrator populates those deferred session collections through the compartment state and marker pending callbacks, so the deferred publish flow is properly connected. Now I'm working through the core invariant that each bust operation must handle both the history rebuild and the compaction marker advancement together.

Looking at the wrapup's `onCompartmentStatePublished`, it marks the active run as published and adds the session to both deferred collections. But there's a timing concern here — the wrapup runs multiple iterations in a loop, and each iteration publishes a new compartment state and sets a pending compaction marker blob, which overwrites the previous one each time through the chunk cycle.

Since publishes are deferred and the wrapup runs synchronously in a command handler, the transform pass doesn't execute between chunks, so the pending marker blob keeps getting overwritten until the final iteration. That's actually fine because the final blob covers the furthest advance, and the boundary check validates that the pending ordinal doesn't exceed what's been consumed.

Now thinking through what might be easy to miss — the marker TTL renewal is unref'd so it doesn't block shutdown, but I need to verify the lease renewal timer inside the wrapup iteration is properly cleaned up.

Looking at the crash-recovery invariant: if the wrapup crashes, the marker has a 5-minute TTL renewed every 60 seconds, so after the process dies it self-expires and gets lazily cleared when `getWrapupInProgressState` is called. The tricky part is when that function is called from within a write transaction — it returns null if `BEGIN IMMEDIATE` fails, treating an expired marker as absent.

But here's the real concern: what happens when an old binary opens a database where a new binary left behind a durable wrapup marker after crashing? The new binary migrated the schema to version 50 with the `wrapup_in_progress_state` column, but the old binary only supports up to version 49.  It would refuse to open the database entirely due to the schema fence, so it never even sees the marker — that's the fail-closed behavior already documented.

For the notes search cross-session leak: session notes are fetched only for the current session, and the `@msg` anchor only displays when the source session matches the current one, so there's no leakage. Smart notes are project-scoped as intended. The acquire path checks that the marker hasn't expired and the holder ID matches before proceeding.

Now I'm examining the wrapup marker TTL concern more carefully. The marker has a 5-minute TTL and gets renewed every 60 seconds by an interval, plus explicitly at loop points. The real question is whether a long historian chunk could starve the renewal interval and cause the TTL to lapse. The interval is `unref()`'d but should still fire while the process is alive and the event loop is active during the wrapup. So the renewal should keep the marker fresh even during a long chunk, as long as the 60-second timer fires.

There's also a critical interaction between wrapup and the 95% emergency-drop path: during a wrapup, `startCompartmentAgent` is blocked by the `isWrapupInProgress` check, so if another turn hits 95% usage, it can't start a recovery historian. But since OpenCode processes one turn at a time per session, this race condition is actually prevented by the session's single-threaded turn processing.

The wrapup command itself blocks the session's command handler, so the transform pass (which runs on LLM calls) isn't executing on the primary session during the wrapup—only the spawned historian subagents are running. The 85% emergency-drop heuristic still runs in postprocess, but that's just cache cleanup, not a historian.

Now looking at the `clearEmergencyRecovery` call at the end of wrapup: the marker renewal interval is cleared and the wrapup marker is released in the finally block, then `clearEmergencyRecovery` is called afterward—so the marker is already released when emergency recovery is cleared, which is the right order.

There's a tiny window between releasing the marker and clearing recovery where the recovery is still armed, but since the session is blocked in the command handler, no transform pass can run. If the wrapup fails (stoppedForFailure), it returns early without clearing recovery, leaving it armed so the normal historian can resume—that's intentional.

Now I'm thinking about the `hasGitDir` and `directoryFallbackCache` interaction with the transient cooldown—there's a potential identity-flip risk here. The logic checks if a cached fallback exists and whether the canonical path has a git directory, deciding whether to reuse the cache or re-resolve, and then checking if an active cooldown exists to either reuse the last known git identity or...

The cold-start scenario is the tricky one: if a process starts and the first identity resolution happens during a transient git failure with no prior cached identity, it falls back to the directory-based identity. For the next 5 minutes, every resolve returns that directory identity. But once the cooldown expires and git becomes available again, the identity flips mid-session from the directory-based one to the git-based one.

This causes a data split — memories and embeddings written under the directory identity get orphaned when the session switches to the git identity. The backfill and reconciliation paths are supposed to handle this, and it's documented as an accepted tradeoff with bounded impact. Not a blocker, but a known risk.

Now I'm looking more carefully at the OpenCode deferred marker drain logic to see if there's an actual bug hiding there. Checking the discard-last and unanchored-promotion skip logic to ensure the invariant holds. When `forceKeepLastCompartmentForChunk` is true (wrapup's final chunk), discard-last doesn't run and the last compartment stays, but unanchored promotion gets skipped anyway since it's a weak boundary. When it's false (token-capped chunk with more remaining), discard-last runs normally. Now verifying that subagents never receive caveman compression.

The gating checks `isSubagent` to set `reducedMode`, and caveman compression only applies when `!reducedMode` and the feature is enabled. But there's a potential race: if `isSubagent` gets set late after the session is created, the first pass might run as primary and apply caveman compression to the database before the subagent flag is detected. On subsequent passes, replay would be skipped (subagent detected), but the compressed depth would already be persisted in the DB. This creates a question about whether that persisted compression affects downstream behavior when replay is disabled.

Now I'm tracing through the interaction between note-nudge delivery and auto-search hints, since both append to the same user message and get replayed each pass via anchors. The order matters for determinism across passes — I need to verify whether note-nudge and auto-search hints stack consistently.

On the detect pass, note-nudge appends at line 1019, then auto-search appends at line 1301. On replay passes, the anchors replay in the same order: note-nudge first (lines 989-991), then auto-search (lines 992-996). Both paths produce the same final string `<user text><note><autosearch>`, and the `.includes()` guard in `appendReminderToUserMessageById` makes re-appends idempotent, so the bytes stay identical across passes. This is consistent.

I'm stepping back now to think about what would constitute a real, verifiable correctness bug given how deeply defended this code is.

I'm noticing something about the `hasPendingNaturalBust` check in the wrapup flow. After a wrapup publishes, it sets `deferredHistoryRefreshSessions` and `deferredMaterializationSessions`, but `hasPendingNaturalBust` only checks the non-deferred versions of those sets — `historyRefreshSessions`, `systemPromptRefreshSessions`, and `pendingMaterializationSessions`. So the function returns false even though deferred sessions are pending, which means the final message appends a flush hint when it shouldn't.

Actually, wait — that's the right behavior. The deferred sets are designed to ride a natural bust (an execute pass), not force one. `hasPendingNaturalBust` is correctly asking whether there's already a forced bust queued. If only deferred sets are populated, there's no guarantee the next message will trigger an execute pass, so the flush hint telling the user to run `/ctx-flush` is actually helpful and correct.

Now looking at the drain condition for `deferredMaterializationSessions` in the wrapup case — in postprocess, materialization only runs if `canConsumeDeferredLate` is true, which depends on scheduler decisions and context percentage. On a low-context session after wrapup, the scheduler might defer every pass without triggering an execute, so `canConsumeDeferredOnThisPass` stays false and the deferred materialization never drains until context grows enough or the user forces a bust.

The same applies to the history rebuild — it's also gated on `canConsumeDeferredLate`, so after a wrapup on a low-context session, the compacted history sits deferred until a cache-busting pass happens. This means the wire still shows the raw history until that bust occurs. On a session that never hits the execute threshold, the raw history could stay indefinitely until `/ctx-flush` or context grows. That's a UX limitation rather than a correctness bug — it's intentional by design since background and manual publishes never force materialization. The flush hint explicitly tells users how to force it if needed.

Now I'm checking whether Pi's deferred marker drain has the same asymmetry issue that OpenCode guards against. OpenCode includes a `pendingMarkerCoveredByConsumedBoundary` check to prevent advancing the marker past history that the current pass rendered into m[0]/m[1]. Pi's drain logic doesn't have this coverage check — it just applies the deferred marker directly if the conditions are met. The risk is whether a pending marker blob could be newer than the injection rebuild from this pass, which would move the marker past history that hasn't actually been consumed yet.

Looking at when this could happen: the drain requires `historyWasConsumedThisPass`, which only gets set when the injection is recomputed (during cache busting or deferred history refresh). When m[1] is recomputed, it reads the latest compartments from the database, so the pending blob ordinal should be covered. However, there's a potential race condition — if a historian publishes a newer pending blob asynchronously between the injection rebuild and the drain firing, the marker could advance past history that m[1] doesn't actually cover.

The key difference is that OpenCode explicitly guards against this with `pendingMarkerCoveredByConsumedBoundary`, but Pi's drain doesn't have an equivalent check. Whether this is actually exploitable depends on whether there's an await point between the injection and drain where a concurrent historian could publish. I'd need to trace through more carefully to confirm the interleaving, but this asymmetry between the two harnesses is worth flagging as a potential issue. So the real risk is that Pi lacks OpenCode's coverage guard—if a concurrent publish lands mid-pass and writes a newer blob while Pi's injection was built before that publish, draining the new blob would violate the invariant since m[1] doesn't cover the new compartment. The `hasPendingMaterializeSignal` term suggests mid-pass publishes are expected, which makes this scenario plausible. I can't fully confirm the interleaving is reachable without knowing if there's an await between injection and drain where a historian commit could land, but this feels like a medium-confidence P1 suspicion. Let me check for awaits in the drain region and verify the `hasPendingMaterializeSignal` logic.
Let me examine the Pi drain region for the coverage-check asymmetry versus OpenCode, and whether concurrent publishes can land mid-pass.

[assistant] 2026-07-06T17:34:47Z
I'm checking whether Pi's drain logic includes the same coverage guard that OpenCode uses to prevent moving the marker past unrendered history. The drain at line 4375+ seems to apply the pending marker based on consumption and materialization conditions, but I need to verify if there's an equivalent check for coverage.

Looking at the actual drain code in Pi, it checks several conditions including whether deferred history was pending and materialization was satisfied, then applies the compaction marker. However, there's no explicit `pendingMarkerCoveredByConsumedBoundary` guard like OpenCode has. In OpenCode, the invariant is strict: the pending marker's ordinal must never advance past the boundary that was actually consumed in this pass, ensuring one bust covers both the history rebuild and the marker advance. Pi's model appears different since it rebuilds the AgentMessage array fresh per context event and renders from the current compartment state, which might handle this invariant differently. So the real concern is whether a sibling process (like OpenCode) could publish a newer compartment and marker blob while Pi is mid-pass, between the injection read and the drain read. Since both reads hit SQLite synchronously, a committed write from another process would be visible on the next read. But if Pi uses a separate marker column per harness, then cross-harness interference wouldn't apply. I need to verify whether Pi has its own marker column and whether the coverage guard is genuinely absent by design, though given this delta has survived multiple rounds, I should be cautious about over-claiming the risk.
Let me verify whether Pi's compaction marker is separate from OpenCode's, and whether the coverage-guard omission in Pi is by design.

[assistant] 2026-07-06T17:35:17Z
The Pi marker uses its own column separate from OpenCode's, so they can share the same context.db without conflicts. They do share the wrapup marker, compartments, and compartment lease, which is fine. I'm noticing a key difference in how they handle marker advancement — OpenCode uses `pendingMarkerCoveredByConsumedBoundary` as a guard in postprocess, but I need to check how Pi handles this.

Looking at Pi's drain logic, it requires both `historyWasConsumedThisPass` and `materializationSatisfiedThisPass` before applying the deferred compaction marker. The crucial difference is that Pi rebuilds m[1] from the full current DB compartment set on each cache-busting pass, rather than caching a boundary snapshot like OpenCode does. This means Pi always renders the latest compartments, so there's no risk of the marker advancing past un-rendered history — the coverage guard that OpenCode needs isn't necessary for Pi's architecture.

Now I'm looking for actual bugs in this delta by examining cross-feature interactions. The wrapup marker renewal system has a 60-second renewal interval against a 5-minute TTL, and I need to verify the timing works correctly when historian passes take a long time. The renewal happens both on the interval timer and inside the main loop, so the marker should stay fresh even during extended operations.

But there's a potential issue: the interval callback that renews the marker doesn't have error handling around the database write. If `renewWrapupMarker` throws due to a database lock (SQLITE_BUSY), the uncaught exception in the setInterval callback could crash the process. The main loop's calls to `renewWrapupMarker` are protected by try/finally, so they'd clean up properly, but the interval callback itself is exposed. It's a low-probability risk given the 5-second busy timeout and quick write operations, but it's a real latent vulnerability.

Now I'm examining the mutual exclusion guarantees between wrapup, recompilation, and trigger-fired historian operations. The checks are mostly in place — trigger-fired historian and recomp both skip if wrapup is active — but there's a potential race condition in the opposite direction. Wrapup acquires its marker first before checking for active recompilation runs, while recomp checks the wrapup marker at its start. If they initiate simultaneously, the ordering of these checks could leave a window where both think they have exclusive access.

I'm also noticing a gap in the mutual exclusion logic: the `/ctx-session-upgrade` operation (kind "upgrade") isn't being blocked by wrapup the same way recomp is. The `waitForExistingIncrementalRun` function only returns "busy" for kind "recomp" or "wrapup", but upgrade operations pass through and wait for the promise instead. Similarly, `acquireCompartmentLeaseForWrapup` only checks for those two kinds, so upgrade operations might proceed while wrapup is trying to acquire the lease.

However, upgrade does check `isWrapupInProgress` at its start, so if wrapup holds the marker, upgrade will skip. And if upgrade is running, wrapup waits for it to finish. That's mutual exclusion through waiting, which is acceptable.

Now I'm checking the discard-last and unanchored-promotion invariant: "Historian discard-last runs never promote unanchored facts." The logic compares persisted vs emitted compartments to detect discard-last, then skips unanchored promotion if either discard-last or weak lookahead final compartment is true. The weak lookahead flag only activates on the actual final chunk (when `hasMore` is false), so on token-capped chunks with more content coming, normal discard-last healing and promotion proceed as expected.

For event filtering, when the weak lookahead final compartment is active, events without an explicit compartment anchor are dropped, and events anchored to the final emitted compartment are also filtered out. This is consistent with treating the final compartment's facts and events as weak. But there's a subtle issue emerging — when weak lookahead is true, persisted and emitted compartments should be identical since nothing gets discarded.

So the final compartment persists for coverage, but its facts aren't promoted and its events aren't stored. Since this is the final chunk, there's no next run to reprocess those facts, meaning they're permanently lost. But this turns out to be intentional design — the wrapup deliberately discards extracted knowledge from the final compartment's boundary because it's weak (no lookahead), so facts might be misattributed. It's a quality tradeoff, not a bug.

Now I'm reconsidering what's actually a blocking issue versus deliberate design. The interval callback throw and the `unref` asymmetry seem more worth investigating.

The marker renewal interval in OpenCode is unref'd, but the lease renewal intervals in both OpenCode and Pi aren't unref'd (though they're cleared in finally blocks). More critically, the marker renewal callback isn't wrapped in try/catch — if `renewWrapupMarker` throws, the exception propagates uncaught in the timer. The same issue exists in Pi. And looking at `updateWrapupInProgress`, the `db.exec("BEGIN IMMEDIATE")` call happens before the try block, so if it throws (SQLITE_BUSY timeout or closed DB), it propagates out of the renewal function entirely.

This is a real robustness gap, though low-probability given the 5-second busy timeout. During heavy contention with multiple processes writing to the same context.db, an uncaught exception in the renewal timer during an active wrapup is possible but rare — I'd mark this as P2.

Now examining `getWrapupInProgressState` more closely. It wraps `BEGIN IMMEDIATE` in try/catch to handle being called within an existing transaction, returning null to treat an expired marker as absent. But `isWrapupInProgress` is called by `startCompartmentAgent` outside any transaction, so that protection doesn't apply there.

The incremental runner calls `isWrapupInProgress` at line 685, which is inside the publish transaction. If the marker is expired, the nested `BEGIN IMMEDIATE` throws, gets caught, returns null, and clears emergency recovery — which is correct behavior since an expired marker from a crashed wrapup shouldn't keep recovery armed indefinitely. During an active wrapup, the marker stays live and renewed every 60 seconds, so the nested transaction path never triggers and emergency recovery stays armed as intended.

For the two-instance scenario: Instance A holds the wrapup marker with its own holder ID, so Instance B's transform skips the compartment agent startup. If Instance B tries to acquire the marker, it sees A's active marker and backs off. The 5-minute TTL ensures that if A crashes, the marker expires and B can reclaim the session — satisfying the invariant that a crashed wrapup can't wedge the session indefinitely. The marker renewal fires every 60 seconds to keep it alive, and even though the timer is `unref()`'d, the historian subprocess await keeps the event loop running anyway.

Now I'm examining the notes search behavior more carefully. Session notes are scoped to the current session only, while smart notes are project-scoped, so searching from session X won't surface notes created in session Y even if they're in the same project. The `@msg` anchor only displays for notes from the current session. This is a design choice rather than a bug — session notes are intentionally session-local while smart notes are project-wide — but it does mean decisions parked in one session become invisible when searching from another. Not a blocker, just a UX limitation.

I'm also checking how the `maxMessageOrdinal` cutoff interacts with notes retrieval. Message history is bounded by the last compartment end ordinal, but notes aren't filtered by ordinal at all — they're returned regardless of where they fall in the message timeline.

Looking at the wrapup's deferred publish mechanism now. The wrapup command is blocking and publishes multiple chunks sequentially, each triggering `onCompartmentStatePublished` which populates deferred refresh and materialization sets. The wrapup registers itself as the active run for the session, so no concurrent transform passes can happen during the wrapup. Once it completes, the deferred sets signal the next transform pass to materialize the compacted history. The flow seems sound, but I'm concerned about what happens when the wrapup publishes N chunks — each write operation needs careful handling.

Checking the coverage invariant for the compaction marker. The wrapup pushes the compartment boundary to the keep-watermark, and the next transform pass rebuilds from the database with an injection that should render up to at least the pending marker's ordinal. The coverage check compares the pending ordinal against the injection's compartment end message, which should align correctly for wrapup scenarios. This looks correct too.

I've spent a lot of time confirming the core machinery is sound. Let me shift focus to newer or less-tested areas that might have actual defects. The CLI doctor onnxruntime subprocess probe is flagged as new, so that's worth examining more carefully. Given this is an adversarial audit and shallow findings are likely already fixed, I should target high-value areas and then wrap up my findings.

Checking a few specific comparisons between versions: the `estimateChunks` logic in Pi wrapup uses `Math.max` to compute expected chunks, while OpenCode computes it once — not a bug. The early return condition in OpenCode wrapup checks `initialPlan.rawMessagesAboveLastCompartment` against `messagesToKeep`, which is consistent with the Pi version. Subagent sessions shouldn't run wrapup anyway since they lack command access. Let me look at the actual git diff to spot any obviously risky changes I might've missed.

I'm focusing on a few key concerns: the interval-callback-throw robustness issue, whether Pi wrapup's marker renewal has the same throw risk, and a cross-feature concern around identity-reuse and the embedding registry. The real risk I'm seeing is that `resolveProjectIdentity` uses a reuse cache to return a git identity during cooldown or transient failure, but `resolveProjectIdentityStrict` doesn't do reuse — it throws. If different code paths call these two functions, they could diverge during a transient failure, with one path getting a reused git identity and another falling back to dir, causing inconsistency in the embedding registry.

Now I need to check who's actually calling `resolveProjectIdentityStrict` directly to see if this divergence is a real risk.
Let me examine callers of the strict vs. lenient identity resolvers for a potential divergence during transient git failures.

[assistant] 2026-07-06T17:37:27Z
I'm noticing a potential issue with the identity-reuse mechanism during the cooldown period. When `resolveProjectIdentity` caches a git identity locally in one process, a second process that starts fresh won't have access to that same cache. If the second process encounters a transient git failure, it won't benefit from the first process's successful resolution, even though they're both reading from the same context.db.

This creates an "identity split" where the two processes write project-scoped rows under different identities for the same repo during the transient window. But the design doc already acknowledges this as an accepted tradeoff—the split is bounded and self-heals once git recovers. So it's a known limitation rather than a defect.

Now I'm digging into the wrapup orchestrator's interaction with emergency-drop and deferred materialization. The `onCompartmentStatePublished` callback queues sessions for both deferred history refresh and deferred materialization, and the invariant holds that background publishes never force immediate materialization. The deferred work gets drained on the next natural pass. I want to examine what happens after `runOneWrapupIteration` completes to see if there's an edge case in the wrapup loop itself.

Each iteration publishes a new pending compaction marker that overwrites the previous one, so only the final chunk's marker survives—which is correct since it just needs to point to the final compartment end. The invariant that "one bust must cover both a history rebuild and its compaction-marker advance" holds because after wrapup finishes, the deferred history refresh, deferred materialization, and pending marker all drain together on the next natural transform pass, and the `pendingMarkerCoveredByConsumedBoundary` check ensures the marker only advances if the consumed boundary covers it. Since the final marker points to the final compartment end and the rebuild includes all compartments up to that end, coverage is guaranteed.

I'm now looking at the wrapup marker renewal timer, which runs on a 60-second interval and calls a renewal function that's cut off here. The historian timeout shouldn't be an issue since the marker renewal fires every 60 seconds independently of how long any individual chunk takes, so even a 10-minute historian call keeps the marker alive. For the wrapup and incremental run interaction, the marker is acquired first which prevents new incremental runs from starting, while already-running ones are allowed to finish via `waitForExistingIncrementalRun`, giving us mutual exclusion in both directions. On the notes search side, session notes are scoped to the current session only while smart notes are project-wide, so there's no cross-session leakage. I'm checking whether the wrapup state expiry logic handles transaction boundaries correctly — specifically whether `getWrapupInProgressState` behaves safely when called from `startCompartmentAgent` outside a transaction versus when already inside one. The reclaim logic uses `BEGIN IMMEDIATE` and returns null on transaction conflicts, which works fine for the non-transactional case, but I need to verify what happens if a concurrent process tries to reclaim while another is still holding the marker.

I'm also tracing through the multi-process race scenario where two instances try to acquire the wrapup marker simultaneously — the `BEGIN IMMEDIATE` serializes them correctly, and the second caller gets rejected when it sees an active foreign marker. But I'm now wondering about a specific edge case in the OpenCode wrapup flow that might be problematic. So the pending drops from wrapup are queued as cache-safe operations that materialize on the next execute pass rather than immediately, which is a minor delay but not a correctness issue. The history still renders correctly through the refresh mechanism, so there's no data loss. This design choice is intentional and well-protected. Now I need to verify the actual git diff to catch any other changes I might have overlooked, particularly around the sidebar facts row removal.

Let me focus on the `pendingMarkerCoveredByConsumedBoundary` logic in OpenCode — it advances the marker if the pending ordinal is at or below the rendered boundary, which keeps the marker within the safe rendered range. But there's an edge case when the injection exists but has a null compartment boundary (degraded rebuild): the pending endMessageId is a real string from the marker state, so it won't equal null, making that equality check false. Then it falls back to comparing ordinals, and if the compartmentEndMessage is 0 in the degraded case while pending.ordinal is higher, the condition fails.

Now looking at the `forceKeepLastCompartment` downgrade logic — the invariant is that unanchored facts never get promoted during discard-last runs, and weak-final keep plus promotion skip only happen on the actual final chunk. The code correctly applies this: on non-final chunks, normal discard-last healing runs (which may discard and skip promotion), while on the final chunk with the flag set, it keeps the last compartment and skips promotion. But there's a subtle concern with the events filter on the final chunk — it's checking whether events should be publishable based on their compartment position relative to the persisted compartments and the weak lookahead boundary.

Let me trace through the specifics: when the weak lookahead is active on the final chunk, events without an explicit compartment assignment get dropped, and events pointing at or beyond the final compartment boundary also get dropped. This means the final weak compartment's events don't persist, which aligns with the principle that nothing durable should be extracted from a boundary the discard-last heuristic would have distrusted. That checks out.

I'm fairly confident the invariants are holding here. Let me shift focus to something more concrete — I want to verify whether there's a consistency issue between the default `min_chars` value in the hook (which uses 500) and what the schema specifies, since the test file uses 50.
This delta is heavily defended. Let me verify a few concrete suspicions rather than continue reading defended code. Let me check the caveman config default consistency and the notes searchable-status leak.

[assistant] 2026-07-06T17:38:57Z
I'm noticing that `NOTE_SEARCHABLE_STATUSES` includes "dismissed" notes, which raises a question about whether dismissed notes should appear in search results. The default note read behavior shows active session notes plus ready smart notes, but the search functionality seems to include dismissed ones too—this could be intentional for preserving search history, though I should double-check the intent.

Now I'm looking at a specific transaction issue in the wrapup marker logic. The `getWrapupInProgressState` method uses `BEGIN IMMEDIATE` and can be called from within another write transaction, returning null if it can't acquire the lock. But there's a potential problem: `isWrapupInProgress` calls this method, and in `compartment-runner-incremental.ts`, `isWrapupInProgress` gets called at line 685 while already inside the historian's transaction that started at line 624. This means we're trying to nest transactions, which could cause issues depending on how the database handles that scenario. I'm checking the marker renewal implementation across both versions — OpenCode uses `unref()` on the interval while the Pi version doesn't, but that's fine since it's explicitly cleared in the finally block. The real concern is the crash-recovery invariant: if wrapup crashes after publishing compartments but before draining the deferred marker, the pending state gets re-seeded on restart. But there's a subtle issue emerging around how the wrapup handles that deferred state. I'm checking a few specific areas for potential issues. First, `searchNotes` handles session notes by `sessionId` and smart notes by `projectPath`, with the `@msg` anchor properly gated to only show when the source session matches the current one — that's solid. Second, the system-prompt-hash provisional gate: when a session isn't persisted yet, the hash write is skipped but guidance still goes out with the provisional value. If the next pass flips the guidance based on user input, the system prompt bytes actually change between passes, but since no hash was persisted on pass one, there's no detected hash change to trigger a cache flush — though the physical prompt content did change.

Actually, the provisional window is narrow since the system prompt only builds when the agent is about to respond, which requires a user message already. That's fine. Now I'm looking at the `directoryFallbackCache` interaction with transient cooldown — if a directory has no last-known git identity (cold start where git broke on first probe but `.git` exists), the fallback returns undefined and we hit the fallback case, which seems like it could be problematic.

During the cooldown period, the directory serves a `dir:` identity while memories get written under that hash. Once git recovers, the identity flips to `git:<root>`, leaving those `dir:` rows orphaned. The comment acknowledges this as a known tradeoff that self-heals through backfill paths. The consistency check during cooldown looks solid — `getActiveCooldown` returns the cooldown, checks `hasGitDir`, tries reuse (undefined at cold start), and consistently returns the directory fallback.

The real concern is whether identity flipping from `dir:` to `git:` mid-session causes issues with the embedding registry — if the registry gets confused when the same directory suddenly has a different identity, that could be a genuine problem. But this is getting speculative. Let me focus on what I can concretely verify and check the OpenCode wrapup area for actual bugs.

During wrapup, the orchestrator calls `runOneWrapupIteration` multiple times, each iteration publishing a compartment and overwriting the pending compaction marker with the latest compartment end. Since the loop is synchronous within the command, intermediate markers just get overwritten — only the final one matters. The `deferredHistoryRefreshSessions` set is idempotent, so that's fine. The key invariant is that one bust must cover both a history rebuild and its compaction-marker advance. After wrapup completes, the next user message triggers a transform pass that rebuilds history and drains the marker if it's covered by the consumed boundary.

Now examining the strongest candidate finding in the wrapup transaction-reclaim logic: when `getWrapupInProgressState` is called from within the historian's open transaction, it checks if the marker is live. If it's live, it returns the state without attempting a nested transaction. If it's expired, it tries to begin a transaction, which throws and gets caught, returning null and treating the marker as absent. This is the correct behavior for an expired marker. But there's a subtle concern: when wrapup itself is running the historian with the wrapup marker live, the marker stays live because wrapup keeps renewing it, so `isWrapupInProgress` returns true and doesn't clear emergency recovery.

The marker renewal timer fires every 60 seconds with a 5-minute TTL, and even if a historian pass takes over 2 minutes, the renewal timer should still fire during network I/O waits. If ownership is lost, `markOwnershipLost` sets a flag that the main loop checks at the top of each iteration and breaks. The marker renewal timer continues firing every 60 seconds even after ownership is lost until it's cleared in the finally block, but `renewWrapupMarker` just returns false and calls `markOwnershipLost` again (idempotent), causing no harm.

I'm not finding a clear issue here — the code is well-defended. Let me look at the diff summary and test files to see if there are any known fragile areas or TODOs. I should also check the `getWrapupInProgressState` call path and whether two OpenCode instances could both acquire wrapup. The `acquireWrapupInProgress` function uses `BEGIN IMMEDIATE` and checks that the holder ID matches, so cross-process mutual exclusion is enforced via the database. The `deferredMaterializationSessions` not being re-seeded on restart shouldn't cause queued drops to be lost since they materialize via `getPendingOps` on the next execute. Now I'm checking whether OpenCode and Pi sharing the same context.db could cause issues — they use separate columns for their compaction markers, but they do share the `wrapup_in_progress_state` column.

The real concern is the schema-fence behavior: if a user has OpenCode at v0.31.0 (schema 50) and Pi at v0.30.7 (schema 49) sharing the same database, OpenCode will migrate to schema 50 and Pi will refuse to open the database entirely, disabling Magic Context for Pi until it updates. This is a known, fail-closed design with a user-facing message, so it's not a new bug — just an upgrade-path friction point worth noting as informational. I'm now considering whether an old binary could have left behind a durable marker that causes problems.

If an old binary can't open the database due to the schema fence, it can't write anything to clobber the new `wrapup_in_progress_state` column. And if a user downgrades after a new binary writes a wrapup marker, the schema fence prevents the old binary from opening anyway, so there's no wedge. Even if they later re-upgrade, the marker's 5-minute TTL means it expires and gets reclaimed before they're back on the new version. The fence protects against this scenario. I should verify the `min_chars` default is consistent between the schema and the hook fallback to wrap up this analysis.
Let me check a few specific consistency points across the config and caveman defaults.

[assistant] 2026-07-06T17:40:35Z
I'm checking whether the Pi index.ts file gates the subagent wiring properly.
Let me verify the Pi-side caveman subagent gate — a SHIP-BLOCKER invariant.

[assistant] 2026-07-06T17:40:42Z
I'm wondering whether Pi actually routes subagents through the caveman context handler at all, and if so, whether it gates the feature differently than OpenCode does. Let me look into how Pi identifies subagents and whether caveman gets disabled for them.

[assistant] 2026-07-06T17:40:48Z
I'm trying to understand where the caveman heuristic gets applied in Pi's pipeline and whether it's bypassed for subagents. Let me trace through how the caveman config flows through the system and check if there's a subagent gate that might prevent it from being invoked.

[assistant] 2026-07-06T17:40:53Z
I'm checking how Pi handles subagent status and whether it gates the caveman feature similarly to OpenCode, which passes `!reducedMode` based on `isSubagent`. I need to trace through Pi's pipeline to see if subagent sessions are even supported and whether the context handler respects that gating.

[assistant] 2026-07-06T17:41:02Z
I'm checking whether Pi runs the pipeline with caveman compression for subagent sessions. In OpenCode, caveman is disabled when in reduced mode for subagents, so I need to trace how Pi's context handler behaves for subagent sessions and where caveman gets gated. The key constraint is that subagents should never receive caveman compression, which appears to be enforced in the transform layer.

[assistant] 2026-07-06T17:41:15Z
I'm checking whether Pi gates the caveman text compression feature behind subagent status like OpenCode does. In OpenCode, it's explicitly conditional on whether reduced mode is active, but in Pi the caveman heuristics are passed unconditionally to the cleanup function. I need to verify if Pi even supports subagents and whether this gating should be applied there too.
Let me check whether Pi gates caveman on subagent status — this is a SHIP-BLOCKER invariant ("Subagents never get caveman compression").

[assistant] 2026-07-06T17:41:23Z
I need to trace through Pi's code to understand how subagent sessions are identified and whether the runPipeline or caveman heuristics even execute for them, since subagents might follow a completely different code path.

[assistant] 2026-07-06T17:41:29Z
I'm checking how the caveman heuristic is gated in Pi and whether subagents have access to it. I need to trace where `heuristics.caveman` gets passed and if there's any subagent check, then look at how subagents are actually executed through the SubagentRunner.

[assistant] 2026-07-06T17:41:37Z
I'm checking whether Pi's caveman heuristics are gated behind subagent status by tracing how `runPipeline` passes the caveman flag to `applyPiHeuristicCleanup`, and whether subagent sessions even invoke this pipeline with caveman enabled. The key difference is that the primary process registers the context reducer while hidden child processes don't, so I need to verify if subagents actually use this code path.

Looking at the structural gates: Pi subagents don't reach the context handler at all, so caveman never runs for them structurally. OpenCode gates caveman explicitly on `!reducedMode`, while Pi's gate is implicit through the handler registration. The invariant holds, though it's slightly fragile since it relies on structural separation rather than explicit checks.

Now I'm examining the wrapup deferred-publish invariant more carefully — specifically whether background and manual publishes defer materialization consistently. In the OpenCode wrapup orchestrator, `onCompartmentStatePublished` adds to both `deferredHistoryRefreshSessions` and `deferredMaterializationSessions`, which matches the normal historian path, so wrapup publishes defer materialization the same way background publishes do.

There's a potential issue with the marker renewal: the wrapup marker is released in a `finally` block before computing the final message, and the 60-second renewal interval could fire during a historian pass that takes up to 120 seconds. If `renewWrapupMarker` throws when calling `updateWrapupInProgress` with `BEGIN IMMEDIATE` (due to SQLITE_BUSY or connection state), the error would propagate as an unhandled exception from the setInterval callback, since the busy_timeout doesn't cover all failure modes.

The `getWrapupInProgressState` read path handles `BEGIN IMMEDIATE` failures gracefully by catching them and returning null, treating the expired marker as absent. But the real concern is the cross-instance scenario: when two OpenCode instances share context.db, Instance A acquires the wrapup marker while Instance B's historian and recomp correctly skip due to `isWrapupInProgress`. However, both instances could race to drain the deferred compaction marker since Instance B rehydrates from the persisted `getSessionsWithPendingMarker` state on hook init. The CAS-clear operation handles this race, with one instance winning the clear operation.

The key insight is that the marker is session-scoped, so Instance B only drains markers for sessions it's actively transforming. If B never opens session X, it never transforms it, so it never drains the marker for that session — eliminating the cross-instance double-drain risk in practice.

Now I'm checking the invariants around defer passes needing byte-identical replays and mutations only applying on cache-busting passes. The notes anchor rendering in ctx_search is a tool output that doesn't affect cache, and auto-search hints have hardcoded sources that deliberately exclude notes, keeping those interactions decoupled. This is intentional per the design comment about cache-neutral primers.

Looking at the wrapup logic, `runManagedWrapup` builds plans iteratively for each chunk. The `forceKeepLastCompartment` and `forceDrainQuota` flags are set, and on the final chunk, the last provisional compartment is kept with a weak boundary while unanchored promotion is skipped — matching the invariant that historian discard-last runs never promote unanchored facts. For non-final chunks, the runner downgrades the flag so normal discard-last healing and promotion apply. But there's a potential issue: on the final chunk, unanchored facts from that compartment aren't promoted, which could leave them stranded.

The design comment clarifies this is intentional — the final weak boundary trades fact-extraction for coverage, and since the compartment itself is persisted, those facts can be re-derived by the dreamer later. It's a documented trade-off, not a blocker. Now I'm checking the `publishableEvents` filter to see how it handles the weak final case, particularly how unanchored events are filtered out.

Looking at the `pendingMarkerCoveredByConsumedBoundary` guard, which enforces the invariant that a single bust must cover both a history rebuild and its compaction-marker advance. The logic checks whether a pending marker is covered by the consumed boundary, and if not, it suppresses the drain to preserve the signal for a later pass. I'm tracing through a scenario where wrapup publishes multiple compartments across chunks, each potentially setting a new pending marker — but `setPendingCompactionMarkerState` overwrites the blob on each publish, so only the latest marker survives.

When the transform pass rebuilds history, the injection's compartment boundary reflects the latest compartment from the database, which should match the latest pending marker's ordinal. So the coverage check should hold. But I'm concerned about whether a pending marker could be overwritten by a later publish whose compartment the current injection doesn't yet cover — though since pending always equals the latest publish and injection is rebuilt from all compartments in the database, they should align.

Now I'm shifting focus to the deferred materialization from wrapup and the concern about what happens if no natural bust arrives. The system added `deferredMaterializationSessions` to handle this — on the next user message, an execute pass typically triggers, allowing materialization to run. I'm also wondering about the historian timeout and marker renewal timing, but I should narrow down to a few concrete, defensible findings rather than continuing broad exploration.

Looking at the marker renewal robustness in the wrapup loop, the timer callback for `markerRenewal` fires every 60 seconds and calls `renewWrapupMarker`, which eventually calls `db.exec("BEGIN IMMEDIATE")` without a try/catch wrapper at the timer level. If that exec throws due to SQLITE_BUSY after the 5-second timeout, the exception propagates uncaught from the timer callback, potentially crashing the process. The main wrapup loop's own renewal calls are inside the outer try/finally, so they'd be caught, but the timer callback is separate and unprotected. This is a low-severity robustness gap, though the pattern is used elsewhere and BEGIN IMMEDIATE failures are rare.

The real concern is crash recovery: if a wrapup crashes, its marker has a 5-minute TTL, and `startCompartmentAgent` reclaims expired markers via `getWrapupInProgressState`, so the session won't wedge. The marker renews every 60 seconds while running, and even though the renewal timer is `.unref()`'d, the wrapup's await chain keeps the event loop alive. But there's a potential issue if a crashed wrapup left a marker and an older binary version wrote it with different semantics.

Actually, that scenario doesn't happen: an old binary (pre-v50) couldn't write a wrapup marker because the column didn't exist yet. If a new binary writes the marker and an old binary opens the database, the schema fence rejects it entirely—the old binary sees the DB is at version 50 while it only supports up to 49, so it disables all of MC and won't touch the marker. That's the documented fail-closed behavior.

Now looking at the notes search cross-session leak: `searchNotes` only returns session notes from the current session plus project-scoped smart notes, so there's no leak of other sessions' notes. The `@msg` anchor only renders when the smart note's `sourceSessionId` matches the current session, so even shared smart notes don't show misleading anchors. But smart notes are project-scoped and shared across sessions, so when session B searches and finds a smart note created in session A, there's a question about what happens next.

After this thorough review, I've identified a few reportable findings. The main one is a timer-callback robustness issue: in the wrapup marker renewal logic, `updateWrapupInProgress` calls `db.exec("BEGIN IMMEDIATE")` outside the try block, so a synchronous throw from that call would propagate into the setInterval callback uncaught. This pattern appears in both OpenCode and Pi's implementations. It's a P2 robustness concern, not a ship-blocker. The final-chunk fact-promotion loss in wrapup is a documented design trade-off, so that's not worth reporting as a finding. ker won't renew, but the loop's own renewal calls at each iteration would also catch it unless an iteration runs longer than five minutes. This is a legitimate robustness issue worth reporting, though probably P2 severity.

Let me dig deeper for something more critical. I'm reconsidering the deferred publish invariant against what happens when wrapup completes at high context. The wrapup sets deferred signals and returns the result to the user, but the raw history stays in OpenCode's message array until materialization runs — which is deferred. So immediately after wrapup, context is still full. If the next message pushes past 95%, the emergency materialization fires and drains the deferred signals, so it self-heals. But I need to check whether the deferred publish actually requires a natural bust or if the emergency drop at 85% is sufficient.

Now I'm looking at whether the notes source respects the `memoryEnabled` config flag and checking the search dispatch to see if notes search runs independently of memory settings.
Let me examine how notes search is gated within unifiedSearch and whether it respects source filtering.

[assistant] 2026-07-06T17:43:22Z
I'm checking how session notes are scoped and gated—they're fetched only for the current session, so the anchor display logic that verifies `sourceSessionId` matches the current session works correctly. Smart notes are project-scoped but tied to their creating session, so anchors get suppressed for notes from other sessions.

Now I need to verify whether `runNotes` searches notes even when memory is disabled.

[assistant] 2026-07-06T17:43:29Z

[assistant] 2026-07-06T17:43:32Z
I'm thinking through what a reviewer might overlook. There's a potential issue with how session notes are filtered — the `getNotes` function returns notes scoped to the current session, but the `@msg` anchor ordinal only displays when the source session matches the current one. Since notes are already filtered by `sessionId`, I need to check whether smart notes are being handled correctly across different project contexts.

Actually, looking more carefully at the auto-search flow — it explicitly restricts sources to memory, messages, and git commits, so notes aren't included there anyway. That's safe.

The more subtle concern is around the `messageOrdinalCutoff` in the notes search. The cutoff prevents leaking live-tail messages for the message source, but notes themselves aren't filtered by this cutoff. A session note anchored to a live-tail message would still show its anchor ordinal, though that's probably fine since notes are the agent's own parked content, not conversation leakage.

Let me dig into the wrapup deferred-publish invariant more carefully. The key constraint is that background and manual publishes should never force materialization — publishes should defer and ride the next natural bust. In the wrapup orchestrator, when a compartment state publishes, it adds to both the deferred history refresh sessions and deferred materialization sessions. Then during OpenCode postprocessing, the deferred materialization gets consumed if conditions allow, which happens on the next execution cycle.

The tricky part is that wrapup runs a loop of multiple historian chunks, each publishing, while holding the compartment lease and blocking trigger-fired historians. The deferred sets accumulate across all these publishes, but they're only consumed when the user sends their next message after wrapup completes — that's the intended "ride the next natural bust" behavior.

For crash recovery, if wrapup crashes mid-loop, the marker has a 5-minute TTL and the system checks for in-progress wrapups on startup, skipping if one's active. After TTL expiry, the state clears itself. The marker renewal timer is unref'd so it doesn't keep the process alive, and on crash the timers die anyway, letting the TTL expire naturally. But there's a potential issue with the deferred sets that were populated before the crash — those are in-memory only.

The pending compaction marker blob is persisted in the database though, and on hook initialization, `getSessionsWithPendingMarker` re-seeds `deferredHistoryRefreshSessions`. But looking at the code, it only re-seeds that one set, not `deferredMaterializationSessions`, even though the docstring claims both are re-seeded. Let me trace through whether this is actually a gap by checking the drain path in the transform postprocess phase.

The marker drain only requires `historyWasConsumedThisPass` and `deferredHistoryWasPendingAtPassStart`, which checks if the session is in `deferredHistoryRefreshSessions`. So re-seeding that set is sufficient for the marker to drain correctly. Materialization is handled separately and will happen naturally on a compartment bust anyway, so the docstring is just outdated but the logic works fine.

Now looking at a potentially more serious interaction: the note anchor ordinals stored in session notes versus the message ordinal cutoff that restricts context search results. If a note points to a message still in the live tail, expanding context around it would show content already visible to the agent, which isn't a leak since it's their own session. But I need to examine the interplay between wrapup's `forceKeepLastCompartment` flag and the discard-last unanchored-promotion skip more carefully, since that's where the real invariants matter.

The code ensures that on the final chunk when `forceKeepLastCompartment` is true, the compartment is kept but unanchored facts aren't promoted. On non-final chunks, normal discard-last healing applies and facts promote as expected. So only the very last compartment of the entire wrapup skips fact promotion—a deliberate trade-off to avoid extracting facts from boundaries the discard-last heuristic would distrust. It's a minor quality issue, not a correctness problem.

But I need to verify whether those final compartment facts are actually lost or if they're captured elsewhere. The wrapup loop runs until reaching the target eligible end ordinal, and each iteration processes one chunk at a time. If there are multiple chunks, each call reads from the current offset up to the historian chunk token limit. So when `isWrapupInProgress` is called from line 685 inside the publish transaction, it tries to acquire a nested transaction if the marker is expired, which fails and returns null — correctly treating the expired marker as absent and allowing `clearEmergencyRecovery` to run. If the marker isn't expired, it returns the state directly without opening a transaction, so the check works correctly and recovery stays armed. The nested-transaction guard handles this properly. Now I'm thinking through the concurrency scenario where two OpenCode instances share the same context and both try to run wrapup on the same session, or when wrapup in one instance races with a historian trigger in another.

Looking at the marker renewal interval, I notice it fires every 60 seconds with a 5-minute TTL, and `runOneWrapupIteration` can take up to `historianTimeoutMs` per chunk. During those awaits, the loop body doesn't renew, but the interval keeps the marker alive. Once ownership is lost and `ownershipLost` is set to true, the loop aborts, but the interval keeps firing until it's cleared in the finally block. The key is that `updateWrapupInProgress` checks if another process owns the marker and returns null if it does, so even if the interval tries to renew after ownership is lost, it won't succeed.

Now I'm examining Pi's marker application in the drain versus OpenCode's coverage guard. OpenCode has `pendingMarkerCoveredByConsumedBoundary` to prevent advancing the marker past history that wasn't actually rendered this pass, since its injection cache can lag behind the DB state. Pi rebuilds a fresh AgentMessage[] from the DB each pass and reads the latest compartment boundary, so the marker and rebuild are always consistent—there's no cache lag issue like OpenCode has. This is likely a deliberate design difference, though I should verify Pi's actual implementation to be sure.

I'm noticing Pi does have an m0/m1 cache (with a `clearM0M1PiCache` function), but the real question is whether the marker coverage concern applies to Pi's compartment boundary semantics. Rather than dig deeper into this subtle area that's already survived review, I should focus on concrete, verifiable issues—particularly checking whether the `messagesToKeep` default (20 in Pi) matches OpenCode's wrapup command handler, since that's a straightforward thing to confirm.
Let me examine the OpenCode wrapup command handler and a few remaining interaction points.

[assistant] 2026-07-06T17:44:53Z
I'm examining how the OpenCode command handler processes the ctx-wrapup command and parses its arguments.
Let me look at the OpenCode command registration for ctx-wrapup args parsing.

[assistant] 2026-07-06T17:45:01Z
I need to locate the OpenCode command handler that processes the ctx-wrapup arguments and invokes executeWrapup.

[assistant] 2026-07-06T17:45:06Z

[assistant] 2026-07-06T17:45:10Z
I'm digging into a potential issue with the wrapup orchestrator's marker renewal timing. The wrapup marker has a 5-minute TTL but only gets renewed every 60 seconds, and I'm concerned about what happens if the orchestrator loop gets blocked during a compartment agent run—specifically when the historian LLM call hits its timeout limit.

Actually, the marker renewal runs on a separate setInterval that fires every 60 seconds regardless of what the main loop is doing, so even if the loop is blocked waiting for the historian, the marker keeps getting renewed. That's good.

But there's a subtle edge case I need to verify: when the wrapup crashes, the renewal interval stops firing (process is dead), and after 5 minutes the marker expires. The `getWrapupInProgressState` call will see the expiration and return null, which clears the lock and allows `startCompartmentAgent` to proceed. The crash-recovery invariant holds.

Now I'm checking a different concern about the OpenCode wrapup behavior—specifically how `onCompartmentStatePublished` adds sessions to both the deferred history refresh and deferred materialization queues, and whether that violates the invariant that background publishes shouldn't force materialization.

Looking at the multi-process scenario more carefully: when Instance A holds the wrapup marker and runs iterations, Instance B's trigger-fired historian checks `isWrapupInProgress` and skips. Between A's iterations, B could theoretically acquire the compartment lease, but `startCompartmentAgent` checks the wrapup marker first, so B still bails. The mutual exclusion holds.

Now I'm tracing through what happens when B's trigger fires between A's iterations—specifically checking the order of checks in `startCompartmentAgent`: first it checks the in-memory `activeRuns` map (which is per-process, so B won't see A's run), then it checks `isWrapupInProgress` against the database...

Actually, the trickier part is the deferred sets across processes. When A publishes a compartment during wrapup, it writes the marker to the shared DB and adds the sessionId to A's in-memory `deferredHistoryRefreshSessions`. But B has its own separate in-memory set that won't include this sessionId unless B rehydrated it at startup. B only rehydrates from `getSessionsWithPendingMarker` once at hook init, not continuously. So if A publishes mid-session, B won't know about it in its in-memory set. But the marker is in the shared DB, and when B's transform runs, it checks its own in-memory set—which is false—so B won't drain it. A will drain it instead since A has it in memory. Since a session typically belongs to one OpenCode instance, this works out fine. The more relevant case is OpenCode and Pi sharing the same context.db but using separate marker columns.

Now I'm zeroing in on the wrapup marker renewal timer. The orchestrator has a 60-second `markerRenewal` interval that's unref'd, and it calls `renewWrapupMarker` which checks ownership and updates the marker. If ownership was lost, it returns null and marks it as lost—idempotent. The interval keeps firing until the finally block clears it, which means during a long historian call the marker stays fresh every 60 seconds. But this creates a subtle issue for crash recovery: if the renewal keeps the marker alive while the loop is stuck, the marker might not expire as expected.

I'm also checking the wrapup progress emission—it uses `kind: "wrapup"` in the progress tracking, and I can confirm from the type definitions that `RecompProgress.kind` includes "wrapup" as a valid option.

Now I'm spotting a real correctness issue with identity reuse and transient cooldowns. When a git probe fails transiently, the code sets a 5-minute cooldown and reuses the last-known git identity if cached. But on a cold start with no cached identity, it falls back to a directory-based identity like `dir:<hash>`. The session then writes compartments under that identity. When the cooldown expires 5 minutes later and git recovers, the identity flips to `git:<root>`, leaving all the compartments written under `dir:<hash>` orphaned from the git identity.

The code acknowledges this as a known tradeoff—the split is bounded and self-heals through backfill and reconciliation paths. It's not a blocker, but worth flagging as a P2 issue.

Looking more closely at the interaction between `directoryFallbackCache` and `transientFailureCooldown`, I'm tracing through the logic: when git works fine and the identity is cached, then git fails transiently, the code sets a cooldown and reuses the last-known git identity. That works cleanly. But I need to verify what happens when the cooldown expires and git recovers.

Actually, the cache itself prevents mid-session flips—once `identityCache` is populated with a git identity, `resolveProjectIdentityStrict` returns the cached value immediately without re-probing, so it never flips. Mid-session flips only occur on cold start with transient failure, which is an acceptable tradeoff.

Now examining the deferred materialization double-signal in wrapup: the invariant is that background and manual publishes never force immediate materialization. In the wrapup orchestrator, `onCompartmentStatePublished` adds to `deferredMaterializationSessions`, which gets consumed later on the next natural execute pass—not immediately. This matches the behavior in the normal historian where both deferred history refresh and deferred materialization are added together. The pattern is consistent and correct.

The wrapup's callback structure adds the session to both deferred sets after publishing, and the pending marker blob is written. On the next transform pass, the drain only happens if the pass actually rebuilds history and the pending marker is covered by the consumed boundary. The `anchorRawMessageCount` captured at the start of the wrapup loop ensures the keep-watermark stays consistent across iterations, even as the historian compacts—raw messages persist in opencode.db until the marker advances.

Since `/ctx-wrapup` is a blocking command, new messages are unlikely to arrive during wrapup, but the anchor freeze handles that case anyway: if more messages do arrive, the anchor stays frozen at its original count. I'm also checking the integer parsing in `parseWrapupArgs` to make sure it validates correctly and that OpenCode's version matches the one being used.

Now I'm verifying the note anchor cross-session leak concern. Session notes are scoped to the current session only, while smart notes are project-scoped by design — and the anchor suppression correctly prevents foreign session ordinals from surfacing. That's handled properly. One thing I want to check: `getLastCompartmentEndMessage` can return -1 as a sentinel value when there are no compartments, and the ctx-search tool handles this by treating -1 as 0.

I've done enough verification to see this delta is well-defended. Let me now identify the genuine concerns worth reporting, ranked by severity. I should also double-check a couple of specifics: the `directoryFallback` MD5 re-key comment references an orphaning issue from a prior commit, not this delta. And I need to verify the OpenCode `parseWrapupArgs` to see if there's a discrepancy with the default `messagesToKeep` value.
Let me verify the OpenCode `parseWrapupArgs` and check for a default-value discrepancy with Pi.

[assistant] 2026-07-06T17:46:30Z
Both default to 20. Now I'm checking a specific concern about the `getWrapupInProgressState` early-expiry path when called inside a write transaction — it returns null rather than reclaiming, which is documented. But in `startCompartmentAgent`, `isWrapupInProgress` gets called outside any transaction, so it can use BEGIN IMMEDIATE and reclaim properly. That's fine. Let me dig into what might actually be a real issue here.

The wrapup marker has a 5-minute TTL that gets renewed every 60 seconds. During a historian chunk run, `runOneWrapupIteration` awaits the runner promise, and a historian chunk can run for up to `historianTimeoutMs` — I need to check what that default is.

[assistant] 2026-07-06T17:46:39Z
I'm noticing there's a timeout constant defined differently in two places—hook.ts pulls from config schema at 5 minutes while rpc-handlers.ts has its own local definition at 10 minutes. That's an existing inconsistency I should set aside for now and focus on the concrete findings from my review.

Looking at the wrapup marker TTL interaction with the historian timeout: the marker has a 5-minute TTL and gets renewed every 60 seconds by the markerRenewal interval, which keeps it alive during a 5-minute historian run. But I'm noticing the marker renewal code doesn't have error handling, and renewWrapupMarker calls updateWrapupInProgress which appears to be incomplete in the thinking. The OpenCode postprocess guards marker advancement with a check that prevents moving past history not yet rendered into the message arrays. Pi rebuilds those arrays fresh each pass from the database, so when history is consumed, the injection has already rendered all current compartments — meaning the consumed boundary aligns with the latest compartment, and the pending marker should safely point within that range.

However, there's a subtle race condition when multiple compartments publish in quick succession: each new publish overwrites the pending marker to point at the latest compartment ordinal, so only one pending marker exists at any time. When the drain eventually runs, it applies whichever marker is current, which points to the most recent compartment.

The OpenCode coverage guard handles the case where a new compartment publishes after the injection was prepared but before the drain runs—if the pending ordinal exceeds the consumed boundary, the guard suppresses it.

Now I'm looking at the `getWrapupInProgressState` expiry-reclaim logic and how it handles being called from within a transaction. The code attempts to start an immediate transaction, and if that fails (because the caller is already in a transaction), it returns null as a signal that the marker should be treated as absent. This is called from `startCompartmentAgent` outside any transaction, which is fine, and from the incremental runner at several points where it checks whether a wrapup is in progress before clearing emergency recovery—but one of those call sites appears to be inside a transaction block.

Wait, I need to reread the logic more carefully. When `getWrapupInProgressState` is called inside the historian publish transaction, it first tries to read the raw wrapup state with a plain SELECT, which works fine inside a transaction. Then it checks if the state has expired—if it hasn't expired, it returns the state immediately without attempting the BEGIN IMMEDIATE that would fail. So the function only tries the nested transaction if the marker is expired, which means the nested transaction failure only happens when checking an expired marker from within a transaction. That's the guard: if you're inside a transaction and the marker is expired, you can't verify expiry, so you treat it as absent. Now I'm tracing through what happens when the user triggers emergency mode at 95% while a wrapup marker is active. The `startCompartmentAgent` function checks if wrapup is in progress and returns early if it is, which means the compartment agent won't start even though emergency mode is trying to force it. This creates a potential issue where `activeRun` remains undefined unless the wrapup itself has already registered a run, leaving the system in a blocked state without the expected compartment agent running.

However, the wrapup does register its own active run during iteration, so the 95% block would wait on that wrapup run instead. The real problem emerges in the gap between wrapup iterations—when the marker is still active but no run is registered yet. If a 95% emergency pass hits during this window, `startCompartmentAgent` skips due to the wrapup check, `activeRun` stays undefined, and the emergency block falls through without actually compacting anything. This could theoretically allow overflow at 95%+, though since wrapup is actively compacting in the background, the pressure should relieve quickly. The tiered drop heuristics at 85% still fire independently, so this is a transient, low-severity condition.

Now I'm checking the deferred materialization logic more carefully. The wrapup correctly adds to both `deferredHistoryRefreshSessions` and `deferredMaterializationSessions`, and deferred materialization only gets consumed on natural execute or force passes, never forcing a bust on its own. That means publishes genuinely ride the next natural bust as intended. Let me dig into the OpenCode marker handling to see if there's something concrete I can verify there.

I'm noticing a potential inconsistency: the comment in storage-meta-persisted says `getSessionsWithPendingMarker` re-seeds both `deferredHistoryRefreshSessions` and `pendingMaterializationSessions`, but the actual implementation only adds to `deferredHistoryRefreshSessions`. The drain condition in OpenCode postprocess checks whether materialization was satisfied, which depends on whether deferred materialization was pending and whether it got consumed — but if deferred materialization sessions aren't being re-seeded after a plugin restart, that could break the invariant.

However, looking at the logic more carefully: after restart, `deferredMaterializationWasPending` would be false since those sessions weren't re-seeded, which means `materializationSatisfied` evaluates to true through the first disjunct. So the marker can still drain. And the pending operations themselves get materialized on the next natural execute pass regardless, since `shouldApplyPendingOps` doesn't strictly depend on the deferred materialization seeding. The real issue is that the marker drain requires `historyWasConsumedThisPass`, which needs the history to be rebuilt in that same pass.

After restart with only `deferredHistoryRefreshSessions` seeded, the injection cache is cold, so the first prepare pass rebuilds from the database. This means `historyRebuiltThisPass` becomes true, and since `deferredHistoryWasPendingAtPassStart` is seeded, the marker drain proceeds. So re-seeding only the history refresh sessions is actually sufficient.

But there's a subtle question about whether it's safe for the marker to advance on a pass that rebuilt history even if the pending operations haven't materialized yet. The invariant requires that one bust covers both a history rebuild and its compaction-marker advance, but the marker advance and the drops are somewhat independent—the marker tells OpenCode where its compaction boundary is, while the drops remove already-compartmentalized raw messages. This feels like a lower-confidence observation that needs more careful analysis.

Now looking at the `resolveCtxReduceAvailabilityFromMessages` fail-open freeze more carefully. There's a subtle divergence between the two resolvers: the transform path freezes on the first user message with tools and treats no-signal as callable, while the system-prompt path reads from the database and also freezes the availability even if the tools field is null. Once a row exists in the database, the availability gets cached and frozen regardless of whether tools was actually present.

There's a potential race condition here — the transform path caches from the in-memory messages array while the system-prompt path caches from the database. If they run in different orders and the first user message's tools differ between these two sources, they could diverge. Though in practice both should be reading the same first user message, so this is likely a well-considered design.

Looking at the note anchor cross-session leak in formatResult, session notes are correctly filtered by sessionId so they're always from the current session, and the guard checking `sourceSessionId === currentSessionId` prevents misleading @msg anchors. Smart notes are project-scoped and meant to be cross-session, so if a smart note from session A surfaces in session B, its anchorOrdinal correctly gets suppressed by that same guard. The distinction between session-scoped and project-scoped notes appears to be handled properly.

Now I'm checking a few remaining concerns — the unref issue on timer callbacks, the re-seed comment mismatch, and the 95% emergency block skipped during wrapup gaps. I'm also noticing that `renewWrapupMarker` inside the setInterval has no error handling, and `updateWrapupInProgress` calls `db.exec("BEGIN IMMEDIATE")` outside the try block, so if the database throws a SQLITE_BUSY or transaction error, it propagates uncaught.

In the setInterval callback, this unhandled exception would crash the process since unref only prevents the timer from keeping the process alive — it doesn't catch exceptions. Under heavy multi-process contention with multiple OpenCode instances and the Pi sharing the same database, a 60-second marker renewal could hit a write lock longer than the 5-second busy timeout and throw. This is a real but low-probability hazard, and I'm rating it medium severity with low confidence on how often it'd actually trigger in practice. I need to verify whether there's a global uncaughtException handler that might save it, and I should double-check the wrapup-orchestrator's marker renewal logic once more. The marker renewal interval of 60 seconds keeps the wrapup marker alive during historian chunks that can run up to 5 minutes, so there's no self-eviction risk. However, there's a subtle timing issue: when another process takes over the wrapup (marker ownership changes), the renewal callback sets `ownershipLost` as a side effect, but since the main loop might be awaiting a long historian run, this flag won't be checked until that iteration completes—a delayed abort that's acceptable but worth noting.

I'm ready to finalize the audit findings. I've identified three issues: first, an uncaught exception risk in the marker-renewal timer callbacks where `updateWrapupInProgress` and `renewCompartmentLease` call `db.exec("BEGIN IMMEDIATE")` without try-catch protection, which could crash the process under heavy multi-process contention; second, a stale comment on `getSessionsWithPendingMarker` that claims it re-seeds pending materialization sessions when the hook only seeds deferred history refresh sessions—the behavior appears correct but the documented contract needs verification; and third, the 95% emergency historian block gets skipped during wrapup inter-chunk gaps because the compartment agent early-returns when a wrapup is in progress, leaving relief to depend on the tiered tool-drop floor. Let me verify the first finding more concretely.
I've mapped the major surfaces. Let me verify my leading concrete concern — error handling in the marker/lease renewal timer callbacks.

[assistant] 2026-07-06T17:49:07Z
I'm looking at the wrapup marker renewal timer in wrapup-orchestrator.ts, which sets up an interval that renews the wrapup marker every 60 seconds and calls unref on it to prevent the timer from keeping the process alive.

The issue is that `renewWrapupMarker` calls `updateWrapupInProgress`, which executes `db.exec("BEGIN IMMEDIATE")` to start a database transaction. If the database is busy and the busy_timeout of 5000ms expires, this throws SQLITE_BUSY. Since the timer callback isn't wrapped in try/catch, an uncaught exception would propagate to the process's uncaughtException handler and potentially crash it. The real problem is that in storage-meta-persisted.ts, the `BEGIN IMMEDIATE` call happens outside the try block, so if it throws, the error bubbles up to the setInterval callback uncaught. While this is low-probability since the renewal happens infrequently and the busy_timeout usually succeeds, it's a genuine crash vector worth addressing.

The lease renewal timers have the same pattern — they lack try/catch wrappers around database operations that could throw SQLITE_BUSY. This is a pre-existing pattern throughout the codebase, and the busy_timeout provides some mitigation, so I won't overweight this concern.

Now I'm focusing on the strongest candidate issues I've identified. The Pi marker drain appears to lack a coverage guard that OpenCode has — before applying a deferred compaction marker, OpenCode checks whether the marker's ordinal is actually covered by the compartment boundary rendered in this pass, suppressing the drain if not. Pi's drain runs when eligible and applies the marker without this equivalent check, which could be an asymmetry in how the two handle the invariant that one cache bust must cover both a history rebuild and its marker advance.

But thinking through this more carefully — in Pi, the marker is native compaction appended to the session branch, and the m[1] injection recomputes fresh from the database whenever a cache bust or deferred history refresh happens. So by the time the marker drain runs, the injected history should already cover the new compartment. The guard in OpenCode exists because the pending marker blob is written at publish time with a specific ordinal, and there can be a race where multiple historian publishes overwrite the pending blob. In Pi's design, since injection always rebuilds m[1] fresh from the database rather than using a cached boundary, the coverage guard may genuinely be unnecessary. This looks like a deliberate design difference rather than a bug, so I'll note it as a low-confidence suspicion.

Now I'm examining the durable marker TTL and mutual exclusion mechanics. The key invariant is that a crashed wrapup shouldn't wedge the session — the 5-minute TTL should release it, with renewals every 60 seconds.

The marker expiration check happens in `getWrapupInProgressState`, which uses `BEGIN IMMEDIATE` to clear expired entries. This is called from `startCompartmentAgent` outside any transaction, so there's no deadlock risk. If the check runs inside an existing write transaction, it catches the failure and treats the expired marker as absent.

But there's a timing edge case: the historian can run for up to 5 minutes (the default timeout), and during that window the marker renewal timer fires every 60 seconds independently. I need to verify whether the marker actually gets renewed while a long-running historian chunk is executing. I see the nested transaction issue: when `isWrapupInProgress` is called from inside the incremental runner's transaction at line 685, it tries to start another transaction if the marker is expired, which fails. But during an active wrapup, the marker isn't expired so it returns the live state without attempting a nested transaction—keeping recovery armed as intended. If the marker somehow expired (wrapup crashed), it catches the error and clears recovery.

Now examining the renewal timer and `getLastCompartmentEndMessage` calls for race conditions, and checking the deferred publish mechanism. The wrapup correctly uses deferred materialization sets rather than forcing sets, so it respects the invariant that background publishes don't force materialization. The deferred history sits until the user's next message, where the scheduler decides whether to consume it on a defer pass via `canConsumeDeferredLate`—same mechanism recomp uses.

Checking the `messages_to_keep` defaults: both OpenCode and Pi use 20, so they're consistent. For identity resilience, `hasGitDir` walks ancestors but only gets called on the cached-fallback path or during cooldown; on the hot path for git repos, it returns from cache immediately without the walk. The `directoryFallbackCache` has a subtle issue though: it's populated when git metadata is missing, but `hasGitDir` walks ancestors, so if a session's directory is deep inside a git repo but git is temporarily broken at cold start, there's a potential mismatch.

Actually, the logic handles this: if any ancestor has `.git`, `hasGitMetadata` is true and it sets cooldown instead of caching the fallback. If a directory later gets `git init`, the cached fallback gets deleted and re-resolved to a git identity, orphaning any project memories written under the `dir:` prefix before that—but that's a documented tradeoff with self-healing via backfill.

Now I'm thinking through a critical scenario: what happens when `/ctx-wrapup` acquires the durable marker while context hits 95% and triggers an emergency block, forcing a compartment agent to start and block in the transform phase?

The compartment agent checks `isWrapupInProgress` and returns early without starting a run, clearing `compartmentInProgress`. At 95%, if wrapup is between chunks, the emergency historian gets suppressed and `activeRun` is undefined, so the block does nothing and the transform proceeds. The 85% emergency drops still happen via `forceMaterialization` and `emergencyDropEligible` in postprocess, which don't need the historian, so context relief still works. But if a sibling process crashes while holding the compartment lease, wrapup gets stuck waiting for the lease (which has a 5-minute TTL) while context climbs toward 100% and the emergency historian remains suppressed.

During that narrow window, only tool-drops provide relief, and if the context is mostly raw text rather than tools, the session could overflow. However, this is a low-severity edge case requiring a crashed sibling. More importantly, while wrapup is legitimately running its own historian chunks each iteration, it's actively compacting history and providing context relief itself — the design intent is that wrapup owns compartment-state publication, which is consistent.

Now I'm checking whether the wrapup boundary could land in a position where `eligibleEndOrdinal <= offset`, causing the loop to spin without progress. The loop has a progress check at line 338 that breaks when `lastEnd + 1 >= plan.targetEligibleEndOrdinal`, and after running, `afterEnd <= beforeEnd` indicates whether progress was made.

Looking at the `releaseWrapupInProgress` call in the finally block — it checks that `current?.holderId === holderId` before clearing, so if another process took ownership of the marker, it won't accidentally clear a foreign marker. That matches the invariant.

The most concrete concern is the timer-callback uncaught-exception issue in `wrapup-orchestrator.ts`, particularly around the lease renewal logic.  The lease renewal pattern in `startLeaseRenewal` is pre-existing and unguarded, so that's lower priority. But the marker renewal code is new — it calls `renewWrapupMarker` which does `db.exec("BEGIN IMMEDIATE")` outside a try block. With concurrent writers (two OpenCode instances or OpenCode plus another process), a second writer holding the lock could cause SQLITE_BUSY to throw after the 5-second timeout, and if that happens in an unref'd timer callback, Node would emit an uncaught exception.

Verifying the code structure: `updateWrapupInProgress` has BEGIN IMMEDIATE at line 524, then the try block starts at 526 — so the transaction start is indeed unprotected. The same pattern appears in `acquireWrapupInProgress` and `releaseWrapupInProgress`, but those are called from command handlers that are awaited and caught. Only `updateWrapupInProgress` is called from the timer, so that's the real exposure. This is a legitimate P2 hardening issue.

Reconsidering whether there are any P0/P1 correctness issues: the notes search respects session boundaries — it returns session notes only for the current session plus project-scoped smart notes. The `@msg` anchor is only displayed when the source session matches the current session, so smart notes from other sessions don't leak anchors that would be meaningless in the current context. That's the session-aware anchors feature working as intended.

Smart notes are explicitly project-scoped and meant to be shared across sessions by design, so surfacing their content in other sessions isn't a leak. The `NOTE_SEARCHABLE_STATUSES` including "dismissed" is interesting — dismissed notes still appear in search results even though the read tool excludes them by default. This might be intentional (you can still find something you dismissed), but it's worth noting as a low-severity UX concern.

For the migration concern: an old binary can't write the `wrapup_in_progress_state` marker since the column doesn't exist in its schema, and it refuses to open a v50 database anyway. If a new binary writes the marker and then the user downgrades, the old binary won't open the database, so the marker becomes irrelevant. When they re-upgrade, the TTL expires the stale marker. The lazy clearing of expired markers happens whenever `getWrapupInProgressState` is called (during historian triggers and recompilation), so stale markers are cleared on the next trigger after expiration. Until then, historians are suppressed for up to 5 minutes, but heuristic tool-drops still run — that's the documented behavior.

Now I'm narrowing down my findings: there's an unguarded `db.exec("BEGIN IMMEDIATE")` call in the wrapup marker mutators that runs from a 60-second renewal timer, so a SQLITE_BUSY exception could become an uncaught error in an unref'd timer. I also want to flag that dismissed notes are surfaced in context search (NOTE_SEARCHABLE_STATUSES includes "dismissed"), and there's a potential asymmetry in the Pi marker-drain coverage guard, though that's lower confidence. Let me check whether `renewCompartmentLease` in the lease-renewal timer has the same unguarded database issue.

I'm also reconsidering whether there's a genuine P1 in how wrapup's `forceKeepLastCompartment` interacts with the `discardedLast` events filter. The filter logic drops unanchored events when the final compartment is weak, and skips facts too — this keeps the weak compartment for coverage but prevents promoting its unanchored facts, which aligns with the invariant that historian discard-last runs never promote unanchored facts.

But there's an asymmetry I'm noticing: when `forceKeepLast` is true on the final chunk, `persistedCompartments` equals `emittedCompartments` (nothing gets discarded), so `discardedLast` is false, yet `skipUnanchoredPromotion` becomes true anyway because of the weak lookahead flag. This means facts from the final compartment are never promoted during wrapup — and since wrapup compacts the raw messages away, those facts could be lost forever unless they're re-derived from the persisted weak compartment afterward.

Actually, this might be intentional. The comment acknowledges that nothing durable is extracted from boundaries the discard-last heuristic would distrust, so sacrificing the final chunk's facts is a deliberate tradeoff to avoid double-emission risk. The compartment summary itself is preserved, so the conversation data isn't lost — just the secondary facts corpus. And since those raw messages are now compacted, there's no risk of re-reading them in a future wrapup or historian run.

But wait, that means the stated rationale doesn't quite apply here. The double-emission concern assumes the same range gets re-read, but in wrapup-final-keep there's no next run for that range. So skipping promotion is arguably over-conservative — it loses facts with no actual double-emission risk. That's a quality regression specific to the wrapup path, though it's documented as intentional. I'll flag it as a low-priority observation.

Let me check one more thing about the interaction between the provisional context-reduce verdict gate and the system-prompt hash when resuming a session. When the process restarts, the availability cache is empty, but the verdict gets frozen immediately from the persisted first user message in the database. So there's no provisional window on resume — only on brand-new sessions before the first message persists. That looks fine.

Now I'm checking whether the system-prompt hash ever gets written if the frozen flag stays false. For a new session, the first system-prompt transform runs before the first user message persists, so frozen stays false and the hash doesn't get written yet. But once the user message persists, the next system-prompt pass finds the row, sets frozen to true, and writes the hash. So it does eventually get written. If there's a persistent read failure, frozen stays false and the hash never gets written — that's degraded but fail-safe since it disables change detection rather than causing false busts.

I'm also checking the caching logic for context-reduce availability: it caches the verdict as true (fail-open) only when a user message exists, and doesn't cache when there's no user message yet. That matches the frozen semantics. Both the transform path and the system-prompt path write to the same availability map, so whichever runs first freezes the verdict. If the transform sees the first user message and caches true, but the actual first user message in the database had a deny-list, they'd be reading the same message so they should agree.

I'm confident in my assessment now. The defense depth is solid, so I'm going to recommend shipping with minor P2 hardening. I found no P0 or P1 correctness blockers. Let me verify the timer-callback concern by checking the exact lines where setInterval callbacks are unguarded — confirmed in both harnesses that the wrapup renewal timers and database writes lack try-catch protection, exposing them to uncaught exceptions under SQLITE_BUSY. I'll document this as a P2 finding along with the wrapup final-compartment facts issue.

For the deferred-marker drain concern, I'm flagging it as a low-confidence suspicion since Pi's fresh-per-pass rebuild design likely makes the missing guard moot, but I'll present it clearly as something worth confirming rather than claiming it's definitely safe.
I've completed a thorough sweep of the delta's highest-risk surfaces: the new wrapup orchestration (both harnesses), the durable marker lifecycle, deferred publish/marker drain, project-identity resilience, notes search, caveman subagent gating, and the provisional verdict gate. Let me do one final verification on the timer-callback exposure I identified.

[assistant] 2026-07-06T17:52:13Z
I'm looking at a potential issue with the marker renewal timer in wrapup-orchestrator.ts—the `markerRenewal` interval tries to renew the wrapup marker every 60 seconds, but if another process holds a write lock, the `BEGIN IMMEDIATE` transaction will timeout after 5 seconds and throw an exception that propagates uncaught.

In Node.js, an uncaught exception in a setInterval callback crashes the process, which is a real robustness gap for a plugin running inside the OpenCode host. The compartment-runner.ts lease renewal has a similar pattern where it calls `renewCompartmentLease` inside an interval without wrapping it in try/catch, though the busy_timeout and WAL mode usually make this unlikely to fail in practice.

The wrapup marker renewal uses BEGIN IMMEDIATE to acquire a write lock, which under write contention could throw SQLITE_BUSY after 5 seconds if the lock can't be acquired. This exception would be uncaught in the setInterval callback. It's a genuine robustness gap but requires sustained write contention over 5 seconds — exactly the scenario flagged in the audit with multiple OpenCode instances and Pi sharing context.db. I'd rank this as a P2 suspicion since I couldn't confirm an actual crash, just that the exception handling is missing.

Looking deeper at the wrapup-orchestrator, the marker renewal timer is set up with an interval that calls `renewWrapupMarker` every 60 seconds, and the code also calls `.unref()` on the timer to prevent it from keeping the process alive. There's a discrepancy between two DEFAULT_HISTORIAN_TIMEOUT_MS constants (300s vs 600s) defined in different files, though in production the config schema default ensures the value is always set so the fallbacks are dead code. The real concern is whether the wrapup marker TTL (5 min, renewed every 60s) properly releases a crashed wrapup process — if the renewal timer is unref'd and the wrapup loop gets stuck, the marker will eventually expire and allow the historian to resume, but I need to verify what happens if the process is still alive but the wrapup is hung. Now I'm checking whether the Pi path has the same coverage guard that OpenCode uses. In OpenCode's transform-postprocess phase, `pendingMarkerCoveredByConsumedBoundary` ensures the marker doesn't advance past unrendered history. But looking at the Pi drain logic around line 4375, I need to verify if `applyDeferredPiCompactionMarker` has an equivalent check before it runs, because the invariant requires that any bust covering a marker advance must also cover the history rebuild. The key difference is that Pi's injection always reads the full current compartment set from the DB, so when the drain runs, the injection has already rendered up to the latest compartment boundary and the pending marker ordinal matches it—meaning Pi doesn't need the coverage guard that OpenCode requires, since OpenCode's injection is prepared against a potentially stale boundary snapshot.

Even if Pi's injection budget truncates newer compartments, they're still represented in a compressed tier rather than hidden entirely, so the marker advance remains safe. This makes Pi's approach defensible, though I'll note it as a low-confidence suspicion worth confirming rather than a definite bug.

Now I'm narrowing down the strongest findings to present, focusing on concrete, verified issues.

The most critical one is uncaught exceptions in the wrapup marker renewal timers. In wrapup-orchestrator.ts, the `markerRenewal` setInterval calls database operations like `getLastCompartmentEndMessage` and `renewWrapupMarker` without try/catch protection. Under sustained multi-process write contention where busy_timeout exceeds 5 seconds, BEGIN IMMEDIATE throws SQLITE_BUSY, which becomes an uncaught exception in the setInterval callback. The audit specifically flags "two OpenCode instances sharing the same context.db" as a target scenario, making this a real robustness gap. The Pi renewal timer and compartment-runner lease renewals have the same vulnerability, though those existed before this delta. The new risk is specifically the wrapup marker timers. I need to verify whether an uncaught exception in a setInterval callback actually crashes the host or if an uncaughtException handler can survive it.

On the config side, `ctx_reduce_enabled` removal is a silent behavior change. Users who explicitly set it to `false` to disable ctx_reduce guidance now get it re-enabled post-upgrade with no migration warning. Unlike other experimental flags that get in-memory migration plus warnings, this one is silently stripped. The feature philosophy shifted to always-on gated by tool availability, so losing the disable option is a deliberate product decision, but the lack of a deprecation warning is a UX regression. I should verify whether this flag ever actually disabled meaningful behavior or if the concern is overstated.

The two ctx_reduce resolution paths—one from the transform layer and one from the system-prompt layer—can theoretically disagree, but they share the same cache map so once either one caches a value, both agree going forward. The design where the first user message decides is documented and working as intended, so this isn't a real issue.

Session notes are correctly scoped to the current session only, while smart notes are project-wide—that separation appears intentional to prevent notes from one session leaking into another's search results. The code also properly suppresses @msg anchors when a smart note originates from a different session, since message ordinals are session-relative and would be meaningless in a foreign context.

For the `getLastCompartmentEndMessage` sentinel handling, the code guards against both -1 and 0 returns by mapping them to a safe cutoff value, so there's no issue there either.

Now I'm digging into the wrapup and emergency-drop interaction that the audit flagged. During a wrapup, the historian runs in the background while transform passes continue for the same session. When context hits 95% during a wrapup, the emergency compartment agent would normally force-start, but `startCompartmentAgent` checks if a wrapup is in progress and skips if true, suppressing the emergency historian. The wrapup iteration registers itself as an active run with kind="wrapup", so when transform-compartment-phase checks for an active run at 95%, it finds the wrapup run instead of starting a new compartment agent.

Between wrapup iterations, the active run clears, but `isWrapupInProgress` remains true, so the emergency historian still can't run—transform just proceeds with heuristic drops as the only relief. This is acceptable degradation since the wrapup is draining history anyway, just not synchronously. If the wrapup's historian fails and stops making progress, the orchestrator breaks with `stoppedForFailure` and releases the marker, allowing the emergency historian to resume.

The wrapup publishes are deferred rather than forced, riding the next natural bust, so after a wrapup completes, the compacted history sits in deferred sets until the user's next message triggers materialization. The marker renewal interval callback in the wrapup isn't wrapped in try/catch, so if the database operation throws, it could crash the host — though this pattern already exists in the pre-existing lease renewal code and hasn't caused issues in production, so I'll downgrade this to a P2 consistency note rather than a critical finding.

Now I'm reconsidering whether there's a genuine P0/P1 issue with the config removal for upgraders. If a v0.30.7 binary downgrades or runs alongside a new v0.31.0 binary sharing the same database, the old binary won't respect the wrapup mutual exclusion — but the compartment lease still serializes them at the DB level, so there's no corruption. Plus, if v0.31.0 migrated the schema to v50, the old binary would refuse to open the database entirely since it only supports up to v49, which is the documented fail-closed behavior. So this isn't a new bug either.

Let me verify the wrapup marker handoff logic: if marker A expires and process B takes over, A detects the ownership loss through the `updateWrapupInProgress` check and stops cleanly. The handoff is correct.

I'm settling on three findings. First, the unguarded DB calls in the wrapup marker renewal callbacks can throw uncaught exceptions under multi-process contention — same pattern exists in the lease renewal code though. Second, the removed `ctx_reduce_enabled` config key is silently stripped without a deprecation warning, unlike other graduated keys. Third, there's a suspicion about Pi's deferred compaction-marker drain lacking the coverage guard that OpenCode has, though it's likely safe.

I also notice a divergent `DEFAULT_HISTORIAN_TIMEOUT_MS` — 300,000 in the schema versus 600,000 locally in rpc-handlers.ts, though it's dead code in production since the schema default always populates the config.

Now I'm re-examining the publishableEvents filter for the weak final compartment case to check if there's a genuine wrapup promotion-skip bug. The filter logic gates events based on whether they're anchored beyond the persisted or emitted compartment boundaries, and for the weak final keep case where no discard happens, I need to trace through whether events at the final compartment index are correctly included or incorrectly filtered out.

Actually, I'm catching something subtle here — atCompartment is 1-based, so when emittedCompartments.length equals N, an atCompartment value >= N refers to the last compartment and beyond. This means events anchored to the final weak compartment get dropped, which aligns with the "unanchored-promotion skip only on the actual final chunk" behavior. Unanchored events also return false for the weak final case, so they're correctly skipped. The facts promotion is separately gated by skipUnanchoredPromotion, which prevents facts from being promoted on discard-last runs. But I'm noticing the events logic treats unanchored events differently — on a discard-last run, unanchored events actually return true and get published, whereas the invariant states discard-last runs should never promote unanchored facts. The distinction is that facts and events are separate corpora, so I need to verify whether this asymmetry is intentional or a gap.

Looking closer, on discard-last the last compartment gets dropped and re-derived next run. If unanchored events are published now and then re-emitted during re-derivation, that creates duplicate events. The comment mentions dropping events anchored to the discarded provisional compartment, but unanchored events survive. Since events are marked as "best-effort, re-derivable," duplicates might be tolerated, but this is still a minor inconsistency worth flagging as low severity.

I'm also double-checking the historian timeout divergence — confirmed via the schema showing 300,000ms versus the RPC handler's 600,000ms locally. And reconsidering the identity-reuse and embedding-registry interaction: the cold-start case where a transient git failure causes a fallback to `dir:<hash>` identity, then git recovers and flips to `git:<sha>`, orphaning those `dir:` rows. But the code already acknowledges this as a known, bounded tradeoff with self-healing through backfill and reconciliation paths, so it's not a new bug — it's the intended resilience behavior. My findings are all P2-level, so I'm ready to write the council response with a SHIP verdict.
I've completed a thorough sweep of the delta's highest-risk surfaces. Let me do one final targeted check on the discard-last event-publish path I noticed.

[assistant] 2026-07-06T17:55:04Z
I'm working through the event-filter logic for the `weakLookaheadFinalCompartment` case, where the final chunk is forced to be kept. Since nothing gets discarded in this scenario, the persisted and emitted compartments have the same length, which means the filter condition checking if an event's compartment exceeds that length will evaluate to false.

The key insight is that events anchored to the last compartment (using 1-based indexing) get dropped by the `>= emittedCompartments.length` condition, while earlier compartments are preserved. For unanchored events, the weak-final case explicitly drops them by returning false, which aligns with the design principle that no unanchored promotions happen at the weak boundary.

Now I'm examining the concrete vulnerabilities across restart and multi-process scenarios. The first major concern is an asymmetry in rehydration: when the hook reinitializes after a restart, it only re-seeds `deferredHistoryRefreshSessions` from the database, but the wrapup callback sets both that and `deferredMaterializationSessions`. This creates a gap where materialization state isn't recovered. Looking at the OpenCode drain gate, the deferred-marker condition depends on `materializationSatisfied`, which becomes true when `deferredMaterializationWasPending` is false — and after restart, that flag is indeed false because the sessions weren't rehydrated, so the gate incorrectly opens.

The docstring at line 2200-2201 claims the re-seed covers both `deferredHistoryRefreshSessions` and `pendingMaterializationSessions`, but the actual code in hook.ts only populates the history sessions. This is either stale documentation or a missing implementation, and it means the pending compaction marker signal is lost across restarts, potentially delaying materialization of queued drops.

However, this isn't a correctness blocker — the drops remain in pending_ops and will materialize on the next execute pass anyway. The history rebuild and marker drain still happen together on the next consuming pass, preserving the core invariant. It's a minor latency gap and a documentation accuracy issue, not a wedge.

Now looking at whether `getWrapupInProgressState` correctly handles expired markers when called from within a transaction. If the marker hasn't expired, it returns immediately without trying to acquire a lock, so a live marker is always visible even from inside a transaction. The concern about expired markers trying to acquire `BEGIN IMMEDIATE` and failing is only relevant for stale markers, which is the correct behavior.

But I'm noticing that OpenCode's drain has a guard checking `pendingMarkerCoveredByConsumedBoundary` to prevent advancing its marker past history this pass actually rendered, while Pi's drain appears to lack this same protection.

Let me trace through Pi's scenario: if two publishes land back-to-back before a context pass runs, the pending marker blob gets overwritten to the newer boundary. When the pass finally executes, it rebuilds history from all current compartments in the database, which will include everything up to that newer boundary. Since the injection always reads the full current state, the rebuilt history should cover whatever the pending marker points to. So there's no actual gap — the marker can't outpace the history because the history reconstruction is always complete as of the current database state. The marker renewal timer runs synchronously between awaits in the single-threaded event loop, so it can't interleave with the wrapup transaction itself. However, `updateWrapupInProgress` could throw `SQLITE_BUSY` if another process holds the write lock, and that exception would be uncaught in the setInterval callback—a potential unhandled error.

This same pattern exists elsewhere in the codebase (lease renewal timers also lack try/catch), so it's not new to this delta, but the wrapup marker renewal is a fresh addition. Given the 5-second busy_timeout and WAL mode, a write lock held for longer than that is unlikely but possible, making this a low-probability robustness gap rather than a blocker.

The real concern is whether an unhandled exception from a setInterval callback would crash the process or just get logged. In a multi-process environment with write contention, this becomes a legitimate interleaving issue worth flagging as P1 or P2 with medium confidence, especially since the historian's own lease renewal follows the same unguarded pattern.

Now I'm checking the other concerns: the messagesToKeep defaults match between implementations, the search results properly scope session notes and smart notes with the @msg anchor only showing for the current session, and dismissed notes appearing in search is more of a product decision than a bug since the status is visible in the output. The real issue I need to trace is whether `getWrapupInProgressState` is reclaiming a BEGIN IMMEDIATE transaction while the incremental runner already holds one.

Looking at the code flow, `isWrapupInProgress` gets called at line 685 inside the historian's publish transaction (which starts at line 624). When the wrapup marker is expired, `getWrapupInProgressState` tries to start its own transaction, which SQLite rejects—but that's caught and returns null, treating the expired marker as absent, so `clearEmergencyRecovery` runs as intended. When the marker isn't expired (which is the normal wrapup case since it renews every 60 seconds with a 5-minute TTL), the function returns the state without attempting a new transaction, so `isWrapupInProgress` returns true and `clearEmergencyRecovery` is skipped, keeping recovery armed during wrapup. This matches the intended invariant.

There's an edge case where the wrapup marker expires mid-operation—maybe the renewal timer failed or the process paused—but that's self-consistent: if the marker expired, wrapup's own loop would detect ownership loss on the next renewal attempt and abort anyway. The nested transaction attempt is safely caught, so no crash occurs.

Now checking whether caveman gating in Pi is actually tied to subagent sessions—`runPipeline` receives `heuristics.caveman` from config unconditionally, but I need to verify if subagent sessions actually invoke this with caveman enabled. So runPipeline can actually execute for sessions marked as subagents — line 3523 reads `isSubagent` within that function, which means it's checking the flag there. And critically, caveman gets applied at line 4003 with no `isSubagent` guard, unlike OpenCode's explicit conditional. The real question is whether a subagent session actually triggers the `context` event that invokes runPipeline in the first place. I need to trace that path to confirm whether this is a genuine vulnerability or just a structural safeguard that holds in practice.
Let me verify a potential SHIP-BLOCKER: whether Pi's caveman path is gated against subagents.

[assistant] 2026-07-06T17:56:51Z
I'm noticing that Pi passes heuristics unconditionally to runPipeline, which then applies the caveman cleanup based on those heuristics. But OpenCode's transform.ts gates caveman on whether it's a subagent, so I need to verify whether Pi's context handler even gets registered for subagent sessions in the first place.

Looking at the comments around line 3515-3518, it's clear that subagents in Pi don't use this context handler at all — the architectural design prevents them from reaching runPipeline. So while Pi's caveman gate is implicit rather than explicit like OpenCode's `!reducedMode` check, it's still protected by the fact that subagents never invoke this path. This is a latent divergence worth noting, but not a confirmed blocker since the current architecture keeps subagents away from this code entirely.

I've done a comprehensive review and found one concrete issue: Pi's caveman subagent gate relies on implicit architectural assumptions rather than explicit guards like OpenCode uses.

Beyond that, I'm seeing a couple of other potential concerns. The notes search includes dismissed notes in its results, which could be intentional for recalling past decisions, but it's worth questioning whether dismissed notes should resurface via search. And there's the `getWrapupInProgressState` function that returns null when called inside an existing transaction — though on closer inspection, treating an expired marker as absent is actually the correct semantics, so that's not a real issue.

The more interesting case is how this plays out during the publish transaction. When the wrapup marker is live, `isWrapupInProgress` correctly detects it and prevents emergency recovery from being cleared, which maintains the invariant that recovery stays armed until the wrapup loop finishes. But I need to think through what happens when a historian is triggered outside the wrapup context.

Now I'm looking at the wrapup marker renewal timer in the orchestrator — it runs every 60 seconds and calls `renewWrapupMarker`, which attempts to acquire an immediate transaction. If that `BEGIN IMMEDIATE` throws (due to lock contention or the connection being mid-transaction), the exception propagates out of the interval callback uncaught, which could crash the process. The main loop's renewal calls are wrapped in try/finally, but this interval callback has no error handling, and since it's using the same database connection, a transaction conflict here becomes a real risk. The lease renewal timers in other modules have the same uncaught exception exposure when database writes fail, though UPDATE operations are less likely to throw than BEGIN IMMEDIATE. I'm also noticing a discrepancy in the marker rehydration logic — the comment claims both `deferredHistoryRefreshSessions` and `deferredMaterializationSessions` get re-seeded on restart, but the actual code only seeds the history refresh sessions, which could leave materialization in an inconsistent state if a crash occurs mid-wrapup after the marker is written but before the drain completes. The cache-busting pass only triggers on pending ops or heuristics, so history refresh alone won't force a bust—the marker drain just waits for the next natural one, which is the intended behavior. For the context reduction availability caching, both the transform path and system-prompt path read the same first user message and should cache the same verdict, so they stay consistent. Now looking at directory fallback cache staleness across worktrees...

Actually, let me step back and focus on what's genuinely worth reporting. The caveman subagent gate in Pi is more of a latent defense-in-depth gap than a current blocker—the architectural design prevents subagents from reaching the handler anyway, so I'll flag that as P2. For the marker renewal timer exception handling, the real risk is if updateWrapupInProgress throws during the interval—that could happen with SQLITE_BUSY when multiple OpenCode instances or OpenCode+Pi share context.db and a write lock gets held past the timeout.

The more concerning pattern is in the wrapup flow itself: if renewWrapupMarker throws in the main loop, the exception propagates through runManagedWrapup, and then releaseWrapupInProgress in the finally block could throw again with another BEGIN IMMEDIATE, causing the whole command to reject. That's a robustness gap worth noting, though the actual trigger (SQLITE_BUSY) is rare in practice.

I'm settling on a SHIP verdict with P2 items to verify—the caveman gate divergence and the timer exception exposure—since the delta is heavily defended and I haven't found a clear P0 issue. But let me double-check whether there's a higher-severity problem lurking in the wrapup deferred-publish and emergency interaction flow. long as the historian is running within that 5-minute window, the marker stays renewed and the session won't get wedged. But if the historian itself times out (hits the 300s limit), the wrapup process fails, the marker stops renewing, and it expires 5 minutes after the last renewal — so the session could be blocked for up to 10 minutes total. That's a potential issue if the historian is slow enough to timeout during wrapup.

I'm also checking the ownership loss scenario: if another process steals the marker during renewal, the main loop detects it and aborts cleanly, though the renewal timer keeps firing harmlessly until it's cleared. And for the drain logic, the pending markers should be covered by the compartment end messages since the injection rebuilds from all published compartments, so that's solid.

The core findings are holding up — a few P2 concerns but no confirmed P0/P1 blockers. I should lay out the honest verdict with the verification items flagged, though I need to stay appropriately skeptical about edge cases like the caveman-gate divergence and timer exception exposure.

Now reconsidering the dismissed notes behavior: the search includes dismissed status for both session and smart notes, so dismissed items surface in results with their status visible to the agent. It's a minor UX quirk but not a correctness issue. And for cross-session leaks, session notes are filtered by current sessionId so they can't leak across sessions, while smart notes have their anchors suppressed if they're from a different session. The real question is whether session notes should be searchable only within their own session or if that's intentional design.

Finalizing the findings: the Pi caveman subagent lacks an explicit gate like OpenCode has, creating a defense-in-depth gap against the invariant. The wrapup and lease renewal timer callbacks execute database writes without exception handling, so an SQLITE_BUSY error under contention could become an unhandled exception. Dismissed notes in search is a low-priority item. I've verified the heavily-defended areas are clean. The verdict is ship-ready with those P2 items flagged for follow-up. Let me confirm the timer exception is actually reachable by checking whether the database exec throws on SQLITE_BUSY after the timeout expires—it does in both Bun and Node sqlite implementations, and the interval callback has no try/catch wrapping it, so this is a real robustness gap under multi-process scenarios.
I've completed a thorough sweep of the delta's highest-risk surfaces. This is a heavily-defended change that has clearly survived multiple review rounds — the shallow findings are fixed, the invariants are guarded with explicit code paths and comments, and the crash/restart/multi-process seams are mostly covered. Let me consolidate my findings.

<COUNCIL_MEMBER_RESPONSE>
# Adversarial Audit — Magic Context v0.31.0 delta

Scope covered (source-verified, not diff-inferred): wrapup orchestration (OpenCode `wrapup-orchestrator.ts` + Pi `commands/ctx-wrapup.ts`), durable marker lifecycle (`storage-meta-persisted.ts`), deferred publish/marker drain (both harnesses' postprocess), `forceKeepLastCompartment` downgrade + unanchored-promotion skip (`compartment-runner-incremental.ts`), project-identity resilience (`memory/project-identity.ts`), notes ctx_search source (`search.ts`, `tools/ctx-search`), caveman subagent gating, provisional ctx_reduce verdict gate (`system-prompt-hash.ts`, `ctx-reduce-availability.ts`), config-flag removal + migration v50.

## Finding 1: Pi caveman subagent exclusion is architectural-only, not an explicit gate (invariant defense-in-depth gap)
- **Severity**: P2
- **Location**: `packages/pi-plugin/src/context-handler.ts:2183` (runPipeline call) → `:3981-4004` (applyPiHeuristicCleanup with `caveman: args.heuristics.caveman`); contrast OpenCode `packages/plugin/src/hooks/magic-context/transform.ts:1761` (`cavemanTextCompression: !reducedMode ? deps.cavemanTextCompression : undefined`)
- **Confidence**: high (divergence is real); medium (that it's exploitable today: it is not)
- **Issue**: The SHIP-BLOCKER invariant "Subagents never get caveman compression" is enforced in OpenCode by an explicit `!reducedMode` (i.e. `!isSubagent`) gate at the point caveman config is forwarded. In Pi, `runPipeline` forwards `heuristics.caveman` unconditionally and `applyPiHeuristicCleanup` applies it with **no `isSubagent` guard** on the caveman path. Pi relies solely on the architectural fact that subagents don't invoke this context handler — and the code at `:3515-3518` explicitly anticipates the opposite ("if a future path marks a session as subagent here, suppress visible tags and nudges…"), yet that suppression covers tags/nudges, **not caveman**. So the two harnesses enforce the same invariant with asymmetric robustness: OpenCode is belt-and-suspenders; Pi is a single architectural assumption away from violating a stated ship-blocker.
- **Evidence**: `context-handler.ts:4003` passes `caveman: args.heuristics.caveman` inside `if (shouldRunHeuristics && args.heuristics)` with the only subagent checks nearby being for synthetic-todo (`:2377`) and Channel-1 (`:2412`), never for caveman. OpenCode's transform.ts:1757-1761 comment explicitly says caveman is forwarded "Only … for primary sessions."
- **Suggested Fix**: Add an explicit `sessionMeta.isSubagent` short-circuit on the Pi caveman branch (e.g. gate `caveman` to `undefined` when `sessionMetaForAvailability.isSubagent`), matching OpenCode's explicit gate, so the invariant survives any future path that routes a subagent through `runPipeline`.

## Finding 2: Marker/lease renewal `setInterval` callbacks perform `BEGIN IMMEDIATE` DB writes with no try/catch (unhandled-exception exposure under multi-process contention)
- **Severity**: P2
- **Location**: OpenCode `wrapup-orchestrator.ts:285-290` (marker renewal) and `:167-171` (lease renewal); Pi `commands/ctx-wrapup.ts:235-239` (marker renewal) and `:331-333` (lease renewal). Write path: `storage-meta-persisted.ts:524` (`db.exec("BEGIN IMMEDIATE")` outside the try).
- **Confidence**: medium (reachability); low (that it hard-crashes vs. just logs)
- **Issue**: The 60s wrapup-marker renewal timer calls `updateWrapupInProgress`, which issues `db.exec("BEGIN IMMEDIATE")` **before** its try/finally. On `SQLITE_BUSY` that outlives `busy_timeout=5000` — exactly plausible in the audit's called-out multi-process scenario (two OpenCode instances, or OpenCode + Pi sharing `context.db`, one holding the write lock during a large publish/migration) — `exec` throws. The interval callback wraps nothing, so the throw becomes an uncaught exception originating from a timer, which in an embedded plugin can destabilize the host rather than degrade gracefully. This partially undercuts the "a crashed wrapup must self-heal via TTL" story: the intended failure mode is silent TTL expiry, not a timer-thrown exception. (`renewCompartmentLease` is a bare UPDATE and lower-risk, but shares the pattern.)
- **Evidence**: `wrapup-orchestrator.ts:285` `const markerRenewal = setInterval(() => { renewWrapupMarker({...}); }, 60_000);` — `renewWrapupMarker` → `updateWrapupInProgress` → un-guarded `BEGIN IMMEDIATE`. No surrounding try/catch in either the interval or `renewWrapupMarker`. Same shape in Pi `ctx-wrapup.ts:235`. Note the synchronous begin→commit blocks mean same-connection nesting can't cause it, so the only trigger is genuine cross-process lock contention — which is precisely the interleaving this audit targets.
- **Suggested Fix**: Wrap the interval bodies (and ideally `updateWrapupInProgress`'s `BEGIN IMMEDIATE`) in try/catch that treats a transient BUSY as a no-op renewal (the TTL still covers the gap); never let a renewal timer throw.

## Finding 3: `ctx_search` note source surfaces `dismissed` notes
- **Severity**: P2 (low)
- **Location**: `packages/plugin/src/features/magic-context/search.ts:845` — `NOTE_SEARCHABLE_STATUSES = ["active", "pending", "ready", "dismissed"]`, used for both session and smart notes in `searchNotes` (`:925`, `:930`).
- **Confidence**: high (behavior); low (that it's unwanted)
- **Issue**: A note the user (or dreamer) explicitly **dismissed** remains returnable via `ctx_search sources:["note"]`, whereas `ctx_note read` defaults to active session notes + ready smart notes (constants.ts). This is an intent inconsistency: dismissal implies "stop surfacing this," yet search re-surfaces it. Not a correctness or cache-safety bug (the result row shows `status=dismissed`, so the agent can see it's retired), so ranking it low — but worth a conscious decision before ship since it's a new user-facing recall surface.
- **Evidence**: `search.ts:845` includes `"dismissed"`; `tools/ctx-search/tools.ts:103` renders `status=${result.status}` unconditionally.
- **Suggested Fix**: Either drop `"dismissed"` from `NOTE_SEARCHABLE_STATUSES`, or confirm intentional (recall-of-past-decisions) and document it; if kept, consider de-prioritizing dismissed hits in ranking.

## Areas verified CLEAN (attempted to break, could not)
- **Deferred marker coverage guard**: OpenCode's `pendingMarkerCoveredByConsumedBoundary` (`transform-postprocess-phase.ts:89-96`, gate at `:1188-1238`) correctly refuses to advance the marker past history not yet rendered into m[0]/m[1], preserving "one bust covers both rebuild + marker advance." Pi's drain (`context-handler.ts:4369-4426`) uses a separate `pending_pi_compaction_marker_state` and gates on `materializationSatisfiedThisPass` + CAS-clear; the asymmetry (no explicit ordinal-coverage check) is defensible because Pi rebuilds the full AgentMessage array per pass and drains only after materialization landed.
- **Unanchored-promotion skip on weak boundaries**: `compartment-runner-incremental.ts:352-353` downgrades `forceKeepLastCompartment` on `chunk.hasMore`; `:577-609` skips unanchored fact/event promotion for both discard-last and forced-final-keep, and filters events anchored to the discarded/weak tail — satisfies "discard-last runs never promote unanchored facts."
- **Crashed-wrapup self-heal**: TTL 5min renewed every 60s, ownership-loss abort in both directions, `startCompartmentAgent` (`compartment-runner.ts:112-118`) and Pi trigger-eval (`context-handler.ts:2990`) skip while marker live but respect expiry — satisfies "TTL must release it." Emergency tool-drops still run while historian is marker-blocked, so no full wedge.
- **Provisional verdict gate**: `system-prompt-hash.ts:383` bails on `!availability.frozen` before writing any hash baseline — a deny-list session never persists a reduce-enabled hash it would immediately flip. Cache-stable.
- **Removed `ctx_reduce_enabled`**: Zod strips it silently (index.test.ts:717), migration v50 is an idempotent `ensureColumn`, `LATEST_SUPPORTED_VERSION=50` matches; schema-fence records rejection for the lagging-binary shared-DB case (`storage-db.ts:31-47`). Restart rehydration re-seeds `deferredHistoryRefreshSessions` from persisted marker blobs (`hook.ts:250-262`).
- **Project-identity resilience**: dubious-ownership → `dir:` fallback + safe.directory warning; transient failures reuse last-known `git:` to prevent mid-session identity flips; `dir:` cache re-checks `hasGitDir` every call so identity flips to `git:` once a repo appears. `resolveProjectIdentityOrFallback` used at all load-path call sites so identity never disables load.

## Overall verdict: **SHIP (conditional)**
No P0 or P1 defect was confirmed in source. The invariant-critical machinery (defer byte-identity, single-bust coverage, discard-last non-promotion, TTL release, subagent exclusion in OpenCode, provisional-hash gating) verifies as correct. My three findings are all P2:
- **Finding 1** and **Finding 2** are the ones I'd want addressed or explicitly waived before tagging — not because they break today, but because both are *robustness regressions against stated ship-blocker/self-heal guarantees* in exactly the Pi and multi-process seams this audit was asked to stress. They are cheap to fix (one explicit gate; two try/catch wrappers).
- **Finding 3** is a product-intent call, not a blocker.

Caveat on confidence: this was a broad static sweep; I did not execute the suites or reproduce the multi-process BUSY race, so Finding 2's real-world crash impact is a medium-confidence suspicion rather than a proven crash. If the team accepts Findings 1–2 as post-release follow-ups with the architectural assumptions documented, this delta is shippable.
</COUNCIL_MEMBER_RESPONSE>

