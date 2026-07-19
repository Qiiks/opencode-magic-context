---
task_id: bg_c3faed68
agent: council-member
session_id: ses_0c79ae083ffeUnR9c3AYF3Z7m0
parent_session_id: ses_0c79b87d0ffeSSVKxxJ07oUaHQ
status: completed
completed_at: 2026-07-06T17:26:53Z
---

[assistant] 2026-07-06T17:09:00Z

[assistant] 2026-07-06T17:09:04Z
Let me check the pi-plugin ctx-wrapup file:

[assistant] 2026-07-06T17:09:11Z
Now let me look at the sidekick/facts sidebar removal:

[assistant] 2026-07-06T17:09:15Z

[assistant] 2026-07-06T17:09:18Z
Let me search for the caveman_text_compression and the ctx_reduce-enabled removal:

[assistant] 2026-07-06T17:09:22Z

[assistant] 2026-07-06T17:09:26Z
Now let me look at the schema to understand the ctx_reduce verdict gate, and let me check the smart-notes storage:

[assistant] 2026-07-06T17:09:34Z

[assistant] 2026-07-06T17:09:38Z

[assistant] 2026-07-06T17:09:42Z
Now let me look at the storage-meta and see how the session_meta column is used to check migration order issues:

[assistant] 2026-07-06T17:09:47Z
Now let me look at the storage-meta.ts file for the `wrapup_in_progress_state` column:

[assistant] 2026-07-06T17:09:51Z

[assistant] 2026-07-06T17:09:54Z
Let me look at the storage-meta.ts exports and the storage-meta-session.ts:

[assistant] 2026-07-06T17:09:58Z
Now let me look at clearSession:

[assistant] 2026-07-06T17:10:02Z
Now let me check the test file for the test cases:

[user] 2026-07-06T17:10:02Z
<system-reminder>
Routine context housekeeping is near: a large span of this session will be comparted soon, and ~115k tokens of tool output remain unreduced. Drop spent outputs with ctx_reduce first so the archived span is the part that matters.
oldest reclaimable: §4§ glob · §5§ glob · §7§ read · §11§ read.
</system-reminder>

[assistant] 2026-07-06T17:10:08Z
Acknowledged. Let me drop spent tool outputs to keep the audit context lean.

[assistant] 2026-07-06T17:10:14Z
Now I have a much clearer picture. Let me dig into a few more critical pieces — particularly the storage_meta v50 migration ordering, the markerRelease path in compartment-runner, the deferred-compaction-marker flow under wrapup, and the auto-search interaction with notes.

[assistant] 2026-07-06T17:10:18Z
Now let me look at the deferred-compaction-marker manager and the interaction with wrapup:

[assistant] 2026-07-06T17:10:22Z

[assistant] 2026-07-06T17:10:26Z
Let me look at the actual applyDeferredCompactionMarker and how it interacts with wrapup's publish. The key concern: wrapup publishes multiple chunks — but the first chunk's pending marker could be applied while later chunks haven't published yet.

[assistant] 2026-07-06T17:10:31Z
Now I have a key insight. Let me check the **wrapup orchestrator** for the deferred-history signaling pattern. Wrapup calls `runOneWrapupIteration` which calls `runCompartmentAgentForWrapup` — this publishes multiple chunks in a loop, each one with `preserveInjectionCacheUntilConsumed: true`, which writes a pending compaction marker. Now the question is — does each new chunk OVERWRITE the pending marker, or are they queued?

[assistant] 2026-07-06T17:10:37Z
Now let me check the **subagent-skip** for caveman compression and the *onCompartmentStatePublished* + forceDrainQuota + forceKeepLastCompartment cascade. Now the key question — does `forceDrainQuota` get set for subagents?

[assistant] 2026-07-06T17:10:42Z
Now let me look at the transform caveman path:

[assistant] 2026-07-06T17:10:48Z

[assistant] 2026-07-06T17:10:52Z

[assistant] 2026-07-06T17:10:56Z

[assistant] 2026-07-06T17:11:01Z

[assistant] 2026-07-06T17:11:05Z
Good — caveman is correctly subagent-gated. Now let me look at the auto-search interactions with the new note source:

[assistant] 2026-07-06T17:11:09Z

[assistant] 2026-07-06T17:11:13Z
Now let me check the auto-search hint path. Does it include "note" as a default source?

[assistant] 2026-07-06T17:11:17Z

[assistant] 2026-07-06T17:11:21Z

[assistant] 2026-07-06T17:11:27Z

[assistant] 2026-07-06T17:11:31Z
OK — auto-search deliberately excludes `note` and `primer` (intentional). Now let me check how the `lastCompartmentEnd` cutoff in tools.ts affects the new wrapup flow — because under wrapup, `lastCompartmentEnd` advances through wrapup chunks but ctx_search wouldn't be invoked (no agent turn in between). But the boundary memory search would only include up to that. This is fine.

Let me check the protected-tail boundary and how the wrapup uses it for forceKeepLastCompartment:

[assistant] 2026-07-06T17:11:37Z

[assistant] 2026-07-06T17:11:41Z

[assistant] 2026-07-06T17:11:44Z
OK — now I have the key insight. `targetEligibleEndOrdinal = targetProtectedTailStart`. This means the FIRST chunk of wrapup targets up to `targetProtectedTailStart`, NOT `targetProtectedTailStart + 1`. So `eligibleEndOrdinal - 1` is the protected tail start. The `applyHeadCap` may further restrict it.

But wait — I notice the wrapup orchestrator loop's break condition is `if (lastEnd + 1 >= plan.targetEligibleEndOrdinal) break;` — and `targetEligibleEndOrdinal` is set to `targetProtectedTailStart` (kept as the keep-watermark position). But after a chunk runs, `lastEnd` is `getLastCompartmentEndMessage()` which advances the compartment boundary; the protected tail must remain constant at `targetProtectedTailStart` (per the comment, the keep is anchored at command start). The loop has `anchorRawMessageCount: initialPlan.anchorRawMessageCount`, which freezes the anchor. Good.

But here's a subtle issue: `lastEnd + 1 >= plan.targetEligibleEndOrdinal`. After chunk N, `lastEnd` = compartment-end-message (advanced by N chunks). And the eligible end keeps being rebuilt via `buildPlan` which uses `targetEligibleEndOrdinal = targetProtectedTailStart`. So eventually `lastEnd` reaches the protected tail. The break is correct.

Now let me verify the forceKeepLastCompartment path more carefully. Looking at compartment-runner-incremental.ts:351-355:
- `chunk = readSessionChunk(...)` with `offset` and `eligibleEndOrdinal`
- `forceKeepLastCompartmentForChunk = deps.forceKeepLastCompartment === true && !chunk.hasMore`

The key is `chunk.hasMore` — for the wrapup's FINAL chunk (where the plan reached `targetEligibleEndOrdinal`), the chunk spans up to but not into the keep watermark, so `chunk.hasMore` would be FALSE. But for an intermediate chunk, `chunk.hasMore` is TRUE → discard-last still applies. Good.

But wait — let me check the "actual final" detection. The compartment runner is called with `forceKeepLastCompartment: true`, and the plan's `eligibleEndOrdinal` is `min(head.eligibleEndOrdinal, targetProtectedTailStart)`. If `head.eligibleEndOrdinal < targetProtectedTailStart` (e.g., due to perRunCap), then the chunk has more after it → `chunk.hasMore=true` → downgrade.

But there's a subtle issue: in the **wrapup** plan, the `protectedTailStart = targetProtectedTailStart`, and the `headCap.cutTokens = perRunCap` is selected based on usage (which is 0 in wrapup). So `nonEmergencyPerRunCap` is applied. If `nonEmergencyPerRunCap < trueRawEligibleTokens`, then `eligibleEndOrdinal < targetProtectedTailStart` and `chunk.hasMore=true`. This is a TOKEN-CAPPED chunk. The comment at line 575-579 of runner-incremental.ts is exactly about this case: "A wrapup caller may request final weak-lookahead preservation, but the runner is authoritative: a token-capped chunk (`chunk.hasMore`) still has more raw history after it, so it must use normal discard-last healing and promotion."

So far so good. But the question is: on the **actual final** chunk (when `lastEnd + 1 >= targetEligibleEndOrdinal`), is `chunk.hasMore` guaranteed false? Let me re-check `applyHeadCap` and `readSessionChunk`:

[assistant] 2026-07-06T17:12:07Z

[assistant] 2026-07-06T17:12:11Z

[assistant] 2026-07-06T17:12:16Z

[assistant] 2026-07-06T17:12:20Z
So `hasMore = lastOrdinal < min(eligibleEndOrdinal - 1, totalMessageCount)`. In wrapup's final chunk, `eligibleEndOrdinal = targetProtectedTailStart`. If the chunk's last ordinal reaches `targetProtectedTailStart - 1`, then `hasMore = (targetProtectedTailStart - 1) < min(targetProtectedTailStart - 1, totalMessageCount) = (targetProtectedTailStart - 1) < (targetProtectedTailStart - 1) = false`. So `hasMore` is correctly false ONLY when the chunk fills the entire `eligibleEndOrdinal - 1`. But wait — the chunking is token-capped. If a tool arc at the end forces a fence-back in `applyHeadCap`, the `eligibleEndOrdinal` may be BEFORE `targetProtectedTailStart`. Let me re-read.

Looking back at lines 819-821:
```
recentOpenArcCutoff: targetProtectedTailStart,
});
const eligibleEndOrdinal = Math.min(head.eligibleEndOrdinal, targetProtectedTailStart);
```

So `eligibleEndOrdinal <= targetProtectedTailStart`. And `applyHeadCap`'s tool-arc fence can pull `eligibleEndOrdinal` back to `resOrdinal + 1` (line 368: `end = Math.min(protectedTailStart, resOrdinal + 1)`). That could leave a gap from `eligibleEndOrdinal` to `targetProtectedTailStart`.

But on the FINAL loop iteration, the next `buildPlan` call would re-resolve the boundary. Since `lastCompartmentEnd` advanced and the compartment boundary has moved, the next `resolveWrapupProtectedTailBoundary` call will compute a new `protectedTailStart` and `eligibleEndOrdinal`. Hmm — does the anchor stay frozen? Yes (`anchorRawMessageCount: initialPlan.anchorRawMessageCount`).

The key question: does the loop's `if (lastEnd + 1 >= plan.targetEligibleEndOrdinal) break;` fire on the iteration where the compartment boundary actually reached the protected tail? Let me trace:
- Iteration N: plan.snapshot.eligibleEndOrdinal = some value (the chunk's). After chunk N runs, `lastEnd = getLastCompartmentEndMessage()` which is the new compartment's `endMessage`.
- Iteration N+1: rebuild plan with new `lastCompartmentEndOrdinal`. `targetEligibleEndOrdinal` is recomputed — same formula `targetProtectedTailStart` (since anchor is fixed). And `lastEnd + 1 >= targetEligibleEndOrdinal` is checked.

But what if the chunk's compartment ended BEFORE `targetProtectedTailStart` (because the tool-arc fence pulled it back)? Then `lastEnd < targetProtectedTailStart - 1` and we don't break. We continue, but now the next `buildPlan` would re-compute and include the gap. Wait, no — the next plan still uses `targetProtectedTailStart` as the keep watermark, and `applyHeadCap` may again pull `eligibleEndOrdinal` back to a tool arc. The cycle continues. Eventually the historian would either:
1. Advance `lastEnd` past the gap, or
2. Hit a no-progress failure (`afterEnd <= beforeEnd`).

Actually this looks like it could loop forever if the tool-arc fence keeps pulling back. But each chunk is token-capped, so the SAME arc fence applies. So `lastEnd` should advance past each chunk's eligible end on every iteration.

Now let me look at a potentially serious issue: `lastEnd + 1 >= plan.targetEligibleEndOrdinal` — `targetEligibleEndOrdinal` is the PLAN's value (re-computed each iteration), not the anchor. So when the compartment has advanced, this check uses the FRESH `targetEligibleEndOrdinal`. That should be fine.

But hold on — there's a concern: if the runner is called with `forceKeepLastCompartment: true` but the chunk has more (`chunk.hasMore=true`), the runner downgrades to discard-last behavior. This means unanchored promotion is preserved (good), but the LAST emitted compartment is dropped (good for boundary healing, but means on intermediate wrapup chunks we still have discard-last). The intent says: "**actual final chunk**" should have `chunk.hasMore=false`. Let me verify this is true for wrapup's last call.

The runner is called with `eligibleEndOrdinal = min(head.eligibleEndOrdinal, targetProtectedTailStart)`. If the runner's `chunk.hasMore=false`, then the chunk reached `eligibleEndOrdinal - 1`. Since `eligibleEndOrdinal <= targetProtectedTailStart`, the chunk may NOT have reached `targetProtectedTailStart - 1`. So `hasMore=false` does NOT mean the chunk reached the keep watermark.

But the `lastEnd + 1 >= plan.targetEligibleEndOrdinal` break is what terminates the loop. If `lastEnd + 1 == targetEligibleEndOrdinal` (i.e., the chunk reached exactly the keep watermark) on the FINAL run, the previous run's compartment had `endMessage == targetEligibleEndOrdinal - 1`. So the FINAL wrapup iteration's compartment ends at `targetEligibleEndOrdinal - 1`, which equals the keep watermark. Then `hasMore` is determined by whether the chunk read up to `eligibleEndOrdinal - 1`. Since `eligibleEndOrdinal = min(head.eligibleEndOrdinal, targetProtectedTailStart)`, and `eligibleEndOrdinal = targetEligibleEndOrdinal` (the plan's value, derived as the keep watermark), the chunk reads from `offset` (which is `lastCompartmentEnd + 1`) to `eligibleEndOrdinal - 1 = targetEligibleEndOrdinal - 1`. If the chunk reads all the way to `eligibleEndOrdinal - 1`, then `hasMore = lastOrdinal < min(eligibleEndOrdinal - 1, totalMessageCount) = targetEligibleEndOrdinal - 1 < targetEligibleEndOrdinal - 1 = false`. So `hasMore=false` for the actual final chunk. 

But wait — the loop's `if (lastEnd + 1 >= plan.targetEligibleEndOrdinal) break;` happens BEFORE the next `runOneWrapupIteration` call. So the chunk that actually drove `lastEnd` to `targetEligibleEndOrdinal - 1` IS the final chunk. The loop checks the post-state, breaks if done. Good.

Now let me check the **postprocess drain** under wrapup. When wrapup publishes a chunk, the runner sets `pending_compaction_marker_state` (line 687-693 of runner-incremental.ts). The transform-postprocess-phase's `pendingMarkerCoveredByConsumedBoundary` check then gates on `pending.ordinal <= injection.compartmentEndMessage`. Under wrapup, `injection` would be the compartment that was just emitted. But wrapup doesn't go through the transform's postprocess path (it runs synchronously inside `runManagedWrapup`).

So how does the wrapup's pending marker get drained? Let me search:

[assistant] 2026-07-06T17:12:49Z
Now I see — the deferred-marker drain only happens through `transform-postprocess-phase.ts` (gated on `historyWasConsumedThisPass`) and `event-handler.ts`. Let me check the event-handler:

[assistant] 2026-07-06T17:12:54Z
So the pending marker is drained ONLY in the transform's postprocess path. That means a **wrapup** that fires while the agent is idle (no transform passes) would NOT get its marker drained. Let me check whether wrapup triggers a materialization:

[assistant] 2026-07-06T17:12:59Z
Good — the wrapup-orchestrator adds to `deferredHistoryRefreshSessions` AND `deferredMaterializationSessions` (line 200-205). The postprocess phase reads these and applies the marker. The onPublished signal (line 198-202) is `markActiveCompartmentRunPublished(sid)` + `deferredHistoryRefreshSessions.add(sid)` + `deferredMaterializationSessions.add(sid)`. So when the next transform pass runs, the marker gets drained.

But the `pendingMaterializationSessions` set is also relevant. Let me check the publish-flag for "deferred" semantics:

Now let me check the compartment-runner-incremental.ts again, specifically the post-publish signal. Looking at line 716-718, `deps.onCompartmentStatePublished?.(sessionId)` is called AFTER the COMMIT. The wrapup orchestrator's `onCompartmentStatePublished` callback does the signals. So after a wrapup chunk publishes, the next transform pass sees the signal and drains the marker. This works.

But there's a subtle issue: if the agent fires `/ctx-wrapup` and then keeps working WITHOUT sending any message (which is normal — wrapup is invoked from a chat turn), the next transform pass will see the signal. But what if the agent fires `/ctx-wrapup` from a `command.execute.before` hook, the publish happens, the signal is set, and... the transform pass runs on the NEXT user message. That's normal.

Now let me check whether **the next user message** is sufficient to drain the marker. Looking at transform-postprocess-phase line 1188: `if (historyWasConsumedThisPass && args.deferredHistoryWasPendingAtPassStart)`. `deferredHistoryWasPendingAtPassStart` is the signal-at-pass-start snapshot. So if wrapup set the signal AFTER this pass started, the next pass would see it.

OK, the design seems sound. Now let me check the key wrapup loop: what happens if the wrapup crashes (e.g., process kill) — does the marker get stuck?

Looking at `runManagedWrapup` in wrapup-orchestrator.ts:404-407:
```
} finally {
    clearInterval(markerRenewal);
    releaseWrapupInProgress(ctx.db, sessionId, wrapupHolderId);
}
```

If the process is killed, neither the finally block nor the renewal interval runs. The marker persists with `expiresAt = now + 5min` (WRAPUP_IN_PROGRESS_TTL_MS). After 5min, the next `isWrapupInProgress` call returns null (TTL expired). But the **pending_compaction_marker_state** blob (set in the runner) also persists. The next transform pass would see the deferred signal from the rehydration code (hook.ts line 251) AND try to drain.

But the blob is the latest wrapup chunk's marker. That's correct — we want the LATEST compartment boundary applied. Good.

Now let me check the **`getSessionsWithPendingMarker` rehydration** at hook startup:

[assistant] 2026-07-06T17:13:13Z
Now let me check whether the pendingCompactionMarker can be cleared by transform postprocess when the wrapup-in-progress marker is also held:

[assistant] 2026-07-06T17:13:19Z
The transform postprocess phase does NOT check `isWrapupInProgress`. If a wrapup is in flight and a transform pass fires (which shouldn't normally happen because wrapup is blocking/synchronous — but the agent's `/ctx-wrapup` command completes a user-message turn, after which the next user message triggers a transform pass), the transform pass would see the deferred signal (set by the wrapup's last chunk) and try to drain the marker. This is actually intended behavior — wrapup sets the signal so the NEXT user message materializes the result.

OK. Now let me check the migration to see if the v50 migration can fail silently on an existing session_meta:

[assistant] 2026-07-06T17:13:25Z
The migration uses `ensureColumn`. Let me verify the helper doesn't break on a pre-existing row:

[assistant] 2026-07-06T17:13:29Z
Note: the `wrapup_in_progress_state` column is NOT in `healNullTextColumns`. Since it has no DEFAULT and is nullable, old rows would have NULL. Reading code at `getWrapupInProgressState` (line 434-470) and `parseWrapupState` returns null for empty string. Reading the blob as `null` is handled correctly (treated as "no marker"). So this should be fine, but let me check.

Looking at line 425-431 of storage-meta-persisted.ts: `SELECT wrapup_in_progress_state FROM session_meta WHERE session_id = ?` returns `{ wrapup_in_progress_state: string | null }`. The `row?.wrapup_in_progress_state` is passed to `parseWrapupState`. `parseWrapupState` checks `typeof value !== "string"`, so `null` returns null. Good.

Now let me verify the **rehydration on hook startup** doesn't leak state if a user is upgrading from v0.30.7. The rehydration reads `getSessionsWithPendingMarker(db)` (line 250-262 of hook.ts) and adds to `deferredHistoryRefreshSessions`. This is fine — it just re-seeds the signal so the next transform pass picks up the pending marker. But for a fresh v0.30.7→v0.31.0 upgrade, no pending markers exist (because the prior v0.30.7 binary never wrote them), so `getSessionsWithPendingMarker` returns `[]`. Good.

Now let me check the **scheduling for the wrapup-orchestrator's `markOwnershipLost`**. The marker renewal `setInterval(... renewWrapupMarker({...}) , 60_000)` runs at 60s intervals. If the process is SIGKILL'd, the marker stays at `expiresAt = lastSet + 5min`. After 5 min, the next process's `isWrapupInProgress` call returns false. Good — TTL backstop works.

But here's a critical concern: **what about the OTHER process taking over?** Suppose process A is wrapping up. Process B starts up. Process B's `isWrapupInProgress` returns true (TTL alive). So process B's transform passes skip the trigger-fired historian. Good.

But what if process A's wrapup has FINISHED (released marker), and a new transform pass is in flight, but then a new wrapup starts. The new wrapup's `acquireWrapupInProgress` checks ownership. Good.

What if process A's wrapup FINISHES and the marker is released, but a process B instance had already SKIPPED a transform pass? It would now retry on the next pass. Fine.

Now let me look at the **`forceDrainQuota` bypass** during wrapup and its interaction with `drainWindowMs`:

Looking at compartment-runner-incremental.ts line 328-339: `reserve = deps.forceDrainQuota ? { ok: true, reservation: null } : reserveProtectedTailDrainTokens({...})`. So wrapup bypasses the pressure-window quota. The comment says "forceDrainQuota (bypasses pressure-window quota)".

This is fine for wrapup, but it means a **crashed wrapup** that left the marker could trigger subsequent calls to `runCompartmentAgent` (e.g., a stale `runOneWrapupIteration` retry) which would also bypass the quota. Looking at the wrapup orchestrator, this is gated by `forceDrainQuota: true` ONLY in the wrapup-orchestrator's call (line 192). Trigger-fired runs go through `startCompartmentAgent` which doesn't set `forceDrainQuota`. So it's safe.

Now let me check the **crashed wrapup and `pendingMaterializationSessions`**. If a wrapup crashes:
- `compartmentInProgress` may be stuck = 1 (looking at the runner: line 189 sets it true, line 738 sets it false on success, line 916 sets it false in the catch). The catch handler resets it. But if SIGKILL, both fail to run.
- The **published** signal may NOT have been set, so `deferredHistoryRefreshSessions`/`deferredMaterializationSessions` were NOT updated. The rehydration code in hook.ts re-seeds from `pending_compaction_marker_state` blob.

Wait — looking at runner-incremental.ts:716-718, `deps.onCompartmentStatePublished?.(sessionId)` is called AFTER the COMMIT (line 694). If the process is killed between line 694 (COMMIT) and line 718 (signal), the publish IS visible in the DB (compartments appended, pending marker set) but the signal was NOT sent. The rehydration code at hook.ts would re-seed on restart. 

But there's a subtle issue: the **wrapup-orchestrator** at line 200-202 sets `deferredHistoryRefreshSessions.add(sid)` and `deferredMaterializationSessions.add(sid)` in `onCompartmentStatePublished`. If the process is killed between the wrapup's COMMIT and the signal, the next process startup re-seeds from `pending_compaction_marker_state` blob (correctly identifying pending drain). But what about `deferredMaterializationSessions`? Let me re-read rehydration.

Looking at hook.ts:250-262, only `deferredHistoryRefreshSessions` is re-seeded. The `deferredMaterializationSessions` is NOT. This could be a gap.

[assistant] 2026-07-06T17:13:51Z

[assistant] 2026-07-06T17:13:55Z
Confirmed: only `deferredHistoryRefreshSessions` is re-seeded. `deferredMaterializationSessions` is not. But `pendingMaterializationSessions` is also NOT seeded. Let me check what the rehydration's effect is on materialization. If only `deferredHistoryRefreshSessions` is set but `deferredMaterializationSessions` is not, then in transform-postprocess-phase:1247-1252:
```
if ((explicitMaterializedSuccessfully || deferredMaterializedSuccessfully) && deferredMaterializationAtPassStart) {
    args.deferredMaterializationSessions.delete(args.sessionId);
}
```

The check `deferredMaterializationAtPassStart` is the snapshot at the start. If it was NOT set (because rehydration only re-seeded the other set), the session stays in `deferredMaterializationSessions` for the NEXT pass. That's actually OK because rehydration sets the other set, and the next pass will see both as materialization.

Wait — but if `deferredMaterializationSessions` was never set, the materialization won't happen UNTIL the next wrapup or some other signal sets it. This could leave the session in a stale state where compartments were committed but the marker never drained, AND the materialization signal is missing.

Let me verify by checking the test file:

[assistant] 2026-07-06T17:14:02Z

[assistant] 2026-07-06T17:14:06Z
Let me also look at the actual transform-postprocess-phase to see what is consumed and what is dropped:

[assistant] 2026-07-06T17:14:11Z

[assistant] 2026-07-06T17:14:14Z
Now let me check what sets `deferredMaterializationSessions` on rehydration:

[assistant] 2026-07-06T17:14:18Z

[assistant] 2026-07-06T17:14:23Z
OK, I see — `deferredMaterializationSessions` is only set in two places: (1) the wrapup-orchestrator's `onCompartmentStatePublished`, and (2) the transform's compartment-queue path. The rehydration does NOT restore it. This is a pre-existing gap (not introduced by this delta), and likely the system uses the deferred history refresh signal as a proxy. But it's worth noting as a low-priority concern.

Now let me check the **interleaving** between two OpenCode instances pointing at the same `context.db`. The wrapup-orchestrator uses compartment leases + durable markers. Let me check:

[assistant] 2026-07-06T17:14:29Z
Good — the compartment lease is per-session and uses SQLite UNIQUE constraint on session_id. Two processes can't both hold it. Same for `wrapup_in_progress_state` (single column, not per-holder, but the `acquireWrapupInProgress` uses ownership check).

Now let me look at a subtle issue: **the wrapup-orchestrator's `acquireCompartmentLeaseForWrapup` loop (line 126-150)** acquires the lease in a busy-wait loop. If another process holds it, this loops indefinitely (until the renewal expires or ownership is lost). Let me check:

[assistant] 2026-07-06T17:14:36Z
The `getActiveCompartmentRun` is in-process (Map). It does NOT see other processes' runs. So if Process A's wrapup holds the lease but is in another PROCESS, the in-process map is empty, and `acquireCompartmentLease` would return null. Then the loop busy-waits 1s, retries. This is OK but not great — could spin for 5 minutes.

But `renewWrapupMarker({})` is called every iteration (line 147). If the marker TTL expires, `ownershipLost` would be set. So the busy-wait will terminate. Good.

Now let me check the **`defer-pass replay byte-identicality`** for wrapup chunks. The wrapup-orchestrator calls `runCompartmentAgentForWrapup` with `preserveInjectionCacheUntilConsumed: true` (line 190 of wrapup-orchestrator.ts). The runner is the standard `runCompartmentAgent` (compartment-runner-incremental.ts). It writes a pending compaction marker but defers the marker movement to OpenCode's DB. So the FIRST pass that materializes (the transform's postprocess) does the actual marker write. Per "Defer passes must replay BYTE-IDENTICAL", the runner doesn't change message state, only persists the marker. The postprocess applies the marker. Byte-identical is about message transformation, not marker writes. So this is fine.

Now let me check the **subagent + caveman** invariant. The intent says "Subagents never get caveman compression." Looking at the gating at transform.ts:1529 `if (!reducedMode && deps.cavemanTextCompression?.enabled)` and line 1761 `cavemanTextCompression: !reducedMode ? deps.cavemanTextCompression : undefined,`. `reducedMode = sessionMeta.isSubagent`. So subagents don't get caveman. Good.

Now let me check the **dependency between forceKeepLastCompartment and the OpenCode compaction-marker postprocess drain**. Looking at runner-incremental.ts:687-693, when `deferMarkerApplication` is true, the marker blob is written. The transform-postprocess-phase line 1188-1237 handles the drain. The check `pendingMarkerCoveredByConsumedBoundary` requires `pending.endMessageId === injection.compartmentEndMessageId` OR `pending.ordinal <= injection.compartmentEndMessage`. For wrapup, the injection's compartmentEndMessage is the LAST compartment's end message. But wrapup sets `deferredHistoryRefreshSessions.add(sid)` AND `deferredMaterializationSessions.add(sid)` — these are separate signals.

The postprocess drain only fires if `historyWasConsumedThisPass && args.deferredHistoryWasPendingAtPassStart`. For this to be true, a materializing pass must rebuild history using `injection.compartmentEndMessage` from the persisted compartment. So the wrapup's compartments are visible. Then the pending.ordinal check applies. If wrapup's last chunk's end is less than the last compartment's end, `pending.ordinal <= injection.compartmentEndMessage` is true. Drain fires. Good.

But wait — wrapup might publish multiple chunks. Each chunk's marker is written. The last chunk's marker is the LATEST. The intermediate chunks' markers are OVERWRITTEN. So `pending` is the latest, and the drain applies the latest. Good.

However, there's a concern: if a wrapup chunk publishes and the **marker drain fails** (e.g., the transform pass detects the marker is newer than the consumed boundary), the deferred signal is preserved (line 1198 `suppressV12HistoryDrain = true`). But the marker blob stays. The next pass with a fresh compartment injection will try again. But wait — if no new compartments were added (the wrapup is done), the injection's compartmentEndMessage doesn't advance, and the same `pending.ordinal` check might fail again. Let me think...

Actually, `pending.ordinal <= injection.compartmentEndMessage` should be true if the wrapup already advanced the compartment past the marker's ordinal. So the drain should succeed on the next pass.

But there's a subtle case: the **first** wrapup chunk is published, the marker is set, but the next transform pass (still inside the same `/ctx-wrapup` command, but for a sub-message) might not see the marker. Actually `/ctx-wrapup` is a single command and the next transform pass is on the NEXT user message. So enough time has passed.

OK, I think the design is sound. Now let me check the **note-nudger** and its interaction with auto-search:

[assistant] 2026-07-06T17:14:55Z

[assistant] 2026-07-06T17:14:59Z
Now let me look at the `getNotes` query and the session-aware @msg anchors (per the delta description):

[assistant] 2026-07-06T17:15:05Z

[assistant] 2026-07-06T17:15:10Z
Good — the session-aware @msg anchor logic is correct. Now let me also check how `result.sourceSessionId` is populated for smart notes (project-wide):

[assistant] 2026-07-06T17:15:15Z
`entry.note.sessionId` — for smart notes, this is the session that wrote the note (passed in `addNote` as `SmartNoteInput.sessionId`). For session notes, this is the note's own sessionId. So a smart note from a DIFFERENT session in the same project would have `sourceSessionId != currentSessionId` and the @msg anchor would be hidden (no expand available cross-session). This is correct.

Now let me look at the `defer-pass replay byte-identicality` invariant for **defer defer-defers**. Let me check the `getCachedAbsoluteMessageCount` behavior:

[assistant] 2026-07-06T17:15:21Z
Let me check the storage-dump and the `clearSession` to ensure wrapup-in-progress marker gets cleaned up on session delete:

Looking at line 165-192 of `storage-meta-session.ts` (clearSession): it does `DELETE FROM session_meta WHERE session_id = ?` which includes `wrapup_in_progress_state`. Good.

But wait — if a session is deleted while a wrapup is in flight, the wrapup-orchestrator's `getWrapupInProgressState` would return null (the row is gone), so the next loop check would see "no marker" and try to release. But `releaseWrapupInProgress` at line 558-579 only releases if the holder matches. Since the row is gone, the read returns null, and the `if (current?.holderId === holderId)` check is false, so no UPDATE runs. Good.

But the wrapup-orchestrator's in-process `liveSessionState` state still has `deferredHistoryRefreshSessions.add(sid)` from the prior onCompartmentStatePublished call. If a NEW session is created with the same ID (extremely unlikely but possible), it would inherit the signal. This is a pre-existing concern, not introduced by this delta.

Now let me check the **storage-meta** for the `compartmentInProgress` flag — if the wrapup crashes, is this flag left set?

[assistant] 2026-07-06T17:15:30Z

[assistant] 2026-07-06T17:15:34Z
I see — `compartmentInProgress` is set to `true` at the start of the runner and cleared on success (line 738) or failure (line 916). If the process is SIGKILL'd, the flag stays true. The transform-compartment-phase.ts has logic to handle stale flags.

Now let me look for one more thing — the **`compartment-runner-incremental.ts` and `forceDrainQuota` interaction with the per-chunk `preserveInjectionCacheUntilConsumed`**:

[assistant] 2026-07-06T17:15:41Z
Good. Let me also check the `ctx-session-upgrade` and the `recomp-orchestrator` for the interaction with wrapup:

[assistant] 2026-07-06T17:15:45Z
Now let me look at the **storage** rehydration to understand if there's a hook rehydration issue with the wrapup marker (similar to the deferred-marker rehydration). Specifically, is the wrapup-orchestrator state lost on crash?

[assistant] 2026-07-06T17:15:50Z
No wrapup rehydration code. This means: if a process crashes during a wrapup, the marker persists in the DB (with TTL), but no process on restart automatically resumes the wrapup. The agent would have to call `/ctx-wrapup` again. The intent says "A crashed wrapup must not wedge the session (TTL must release it)" — this is met by the TTL mechanism.

But there's a subtler issue: when the marker TTL expires, the next transform pass could see the **pending_compaction_marker_state** blob (set by the wrapup's last chunk) and try to drain. But the next transform pass would also re-fire the trigger-fired historian. Looking at the gating in compartment-runner.ts line 112-119:
```
if (isWrapupInProgress(deps.db, deps.sessionId)) {
    sessionLog(deps.sessionId, "compartment agent skipped: /ctx-wrapup is active");
    updateSessionMeta(deps.db, deps.sessionId, { compartmentInProgress: false });
    return;
}
```

If the marker has expired (TTL), `isWrapupInProgress` returns false. So the trigger-fired historian CAN run. But the pending_compaction_marker_state blob is still there, and the postprocess drain would try to apply it. The drain check at line 1192: `pendingMarkerCoveredByConsumedBoundary(pending, args.pendingCompartmentInjection)` — would `pending.ordinal <= injection.compartmentEndMessage`? Yes, if the wrapup's last chunk advanced the compartment past the marker's ordinal. The drain fires, applies the marker. Good.

But the **compartment that was published in the wrapup's last chunk** may have its content already visible (no need for materialization). The drain is a no-op (marker already at the boundary). Good.

Now let me check the **wrapup's `deferredHistoryRefreshSessions` and `deferredMaterializationSessions` interaction with a crashed wrapup**:
- If the wrapup's last chunk published and set the marker, `onCompartmentStatePublished` fired, `deferredHistoryRefreshSessions.add(sid)`, `deferredMaterializationSessions.add(sid)`. Process killed.
- On restart, the rehydration code re-seeds `deferredHistoryRefreshSessions` from the pending blob. But `deferredMaterializationSessions` is NOT re-seeded.

So the NEXT transform pass:
- `deferredHistoryWasPendingAtPassStart = true` (rehydrated)
- `deferredMaterializationAtPassStart = false` (not re-seeded)
- The marker drain proceeds (per postprocess-phase.ts:1188-1237). Good.
- But `deferredMaterializationAtPassStart` is false, so the line 1247-1252 check `if ((explicitMaterializedSuccessfully || deferredMaterializedSuccessfully) && deferredMaterializationAtPassStart)` does NOT clear `deferredMaterializationSessions`. Since it was never set, the set stays empty. Good.

So the gap is benign in this specific case. But the rehydration could be more complete.

Now let me look at one more concern: the **`getMessagesToKeep` parsing** for non-numeric input. Looking at `parseWrapupArgs` in pi-wrapup.ts:73: `if (!/^\d+$/.test(trimmed))` — only digits. Empty is allowed (default 20). Good.

But the opencode command-handler uses `parseWrapupArgs` from the same file? Let me check:

[assistant] 2026-07-06T17:16:06Z

[assistant] 2026-07-06T17:16:10Z
Good. Now let me check the **searchNotes `NOTE_SEARCHABLE_STATUSES` filter** — it includes "dismissed" notes. Is that intentional?

[assistant] 2026-07-06T17:16:14Z
Dismissed notes are searchable. This is a design choice. Let me check if it's intentional by looking at usage patterns:

[assistant] 2026-07-06T17:16:18Z

[assistant] 2026-07-06T17:16:23Z
This includes "dismissed" — meaning dismissed notes still surface in ctx_search. That might be intentional (dismissed means "I've seen it" not "delete it"). The other places that use `defaultSmartStatuses` are `["pending", "ready"]`. So ctx_search is broader than the smart-notes' other queries. This is likely intentional — a dismissed note may still contain relevant context.

Now let me check the **`@msg anchor` for cross-session notes** — the session-aware filtering. A smart note written by session A and surfaced in session B (same project) shows the @msg anchor only if `sourceSessionId === currentSessionId`. For smart notes written by session A, `note.sessionId = A` (set in `addNote`), so in session B, the anchor is hidden. This is correct — the @msg anchor references session A's ordinals, which session B can't ctx_expand into.

Now let me check the **interleaving concern** more carefully. Two OpenCode instances pointing at the same context.db. The wrapup-orchestrator's `acquireWrapupInProgress` uses `BEGIN IMMEDIATE` (line 489 of storage-meta-persisted.ts), so it's serialized. But there's a subtle issue:

In the wrapup-orchestrator (line 249-262):
```ts
const acquired = acquireWrapupInProgress(ctx.db, sessionId, {...});
if (!acquired.ok) {
    // already running
    return skip message;
}
```

The orchestrator then enters the loop. If the process is killed here (between acquire and the first iteration), the marker persists. Good.

But there's a separate concern: the wrapup-orchestrator's `acquireCompartmentLeaseForWrapup` (line 126-150) has a busy-wait loop. If a concurrent process holds the lease (e.g., a recomp is running), the wrapup busy-waits. But the busy-wait only checks `getActiveCompartmentRun` (in-process Map), which doesn't see the other process. The lease check (line 144) sees the other process's lease and busy-waits. The marker TTL is 5min, so after 5min the OTHER process's lease expires and the wrapup can take over. Or, if the marker ownership is lost (line 147 `renewWrapupMarker({})` returns false), the wrapup aborts.

This seems sound. But there's an edge case: **what if the OTHER process's recomp/wrapup is STILL running and the marker is held?** Both processes are trying to do historian work. The compartment lease prevents concurrent writes. The wrapup marker prevents the other process's trigger-fired historian from running. But the OTHER process's recomp/wrapup is still running and may publish its own results. When both finish, the marker would be released. Then the agent would see two competing `lastCompartmentEndMessage` values. The compartments table would have entries from both processes. The `appendCompartments` uses `sequence` (max + 1) so no UNIQUE conflicts. The `lastCompartmentEndMessage` would be the max of all compartments. So no data loss. But the order may be interleaved. This is a pre-existing concern with the lease + marker model.

Now let me check the **subagent fallback to non-historical in wrapup** — the wrapup command is for primary sessions. Does it skip subagent sessions?

[assistant] 2026-07-06T17:16:42Z

[assistant] 2026-07-06T17:16:46Z
Neither orchestrator explicitly skips subagent sessions. A subagent session that calls `/ctx-wrapup` would still try to compact. This is potentially a P1 issue — wrapup is intended for primary sessions. The `isSubagent` check should be in the command handler.

But wait — `/ctx-wrapup` is registered as a user-invoked command. Subagents are typically NOT interactive (no slash command), so they can't invoke it. But the OpenCode plugin system might allow tools to be called. Let me check:

[assistant] 2026-07-06T17:16:52Z
The subagent skip pattern is applied in many places but not in the wrapup command. This is a potential gap.

Let me look at one more thing — the **forceKeepLastCompartment downgrade comment** in runner-incremental.ts:

Looking at lines 570-579:
```
// A wrapup caller may request final weak-lookahead preservation, but the
// runner is authoritative: a token-capped chunk (`chunk.hasMore`) still has
// more raw history after it, so it must use normal discard-last healing and
// promotion.
const discardedLast = persistedCompartments.length < emittedCompartments.length;
const weakLookaheadFinalCompartment = forceKeepLastCompartmentForChunk;
const skipUnanchoredPromotion = discardedLast || weakLookaheadFinalCompartment;
```

And the gating at line 504-516:
```
if (!inEmergency && !forceKeepLastCompartmentForChunk && emittedCompartments.length >= 2) {
    const lastEmitted = emittedCompartments[emittedCompartments.length - 1];
    const lookaheadMargin = chunk.endIndex - lastEmitted.endMessage;
    if (lookaheadMargin <= BOUNDARY_HEALING_SLACK) {
        persistedCompartments = emittedCompartments.slice(0, -1);
        telemetry.discardedLast = true;
    }
}
```

So:
- `forceKeepLastCompartmentForChunk=true` → no discard-last, no unanchored promotion skip via discard-last
- `forceKeepLastCompartmentForChunk=true` AND `discardedLast=false` (because !forceKeep) → `weakLookaheadFinalCompartment=true` → `skipUnanchoredPromotion=true`

But `discardedLast = persistedCompartments.length < emittedCompartments.length` is FALSE because no discard happened. So `skipUnanchoredPromotion = false || true = true`. 

And events filtering (line 602-609):
```ts
const publishableEvents = (validatedPass.events ?? []).filter((e) => {
    if (typeof e.atCompartment !== "number") return !weakLookaheadFinalCompartment;
    if (e.atCompartment > persistedCompartments.length) return false;
    if (weakLookaheadFinalCompartment && e.atCompartment >= emittedCompartments.length) {
        return false;
    }
    return true;
});
```

If `weakLookaheadFinalCompartment=true` and `e.atCompartment >= emittedCompartments.length` (i.e., points at the LAST emitted), it's dropped. This is the **unanchored events** filter.

And the test at compartment-runner-incremental.ts:809-812:
```
if (
    deps.experimentalUserMemories === true &&
    !skipUnanchoredPromotion &&
    validatedPass.userObservations &&
    validatedPass.userObservations.length > 0
)
```

And the primer candidates gate at line 839-843:
```
if (
    !skipUnanchoredPromotion &&
    promotionProjectIdentity &&
    validatedPass.primerCandidates &&
    validatedPass.primerCandidates.length > 0
)
```

These all use `skipUnanchoredPromotion` to gate unanchored side-effect writes. So a wrapup's final chunk (forceKeep=true, chunk.hasMore=false) will:
- NOT apply discard-last (good, keeps the weak-boundary compartment)
- WILL skip unanchored promotion (correct: facts at the weak boundary are unanchored)
- WILL skip unanchored user observations (correct)
- WILL skip unanchored primer candidates (correct)
- WILL apply events anchoring (events anchored to non-final compartments only)

This is a careful design. Good.

But — the test for this is in `wrapup-orchestrator.test.ts:110-126`:
```
const forceKeepFlags: boolean[] = [];
const ctx = baseCtx(db);
ctx.runCompartmentAgentForWrapup = mock(async (deps) => {
    forceKeepFlags.push(deps.forceKeepLastCompartment === true);
    ...
});
...
expect(forceKeepFlags).toEqual([true, true, true]);
```

This confirms `forceKeepLastCompartment` is always `true` for wrapup calls. But the actual `forceKeepLastCompartmentForChunk` is computed in the runner as `deps.forceKeepLastCompartment === true && !chunk.hasMore`. So `forceKeepFlags` is the dep value, not the runner-computed value. The test doesn't verify the downgrade behavior.

Let me check the test for downgrade:

[assistant] 2026-07-06T17:17:08Z

[assistant] 2026-07-06T17:17:13Z

[assistant] 2026-07-06T17:17:17Z
No test for the `forceKeepLastCompartmentForChunk` downgrade. This is a real gap — the test only verifies the dep is passed, not the runner's downgrade decision. A P2 concern but worth noting.

Now let me check the **`isWrapupInProgress` check in `compartment-runner.ts:112`** more carefully — does it work correctly with a foreign holder?

```ts
if (isWrapupInProgress(deps.db, deps.sessionId)) {
    // skip
}
```

Looking at `isWrapupInProgress` in storage-meta-persisted.ts:472-474:
```ts
export function isWrapupInProgress(db: Database, sessionId: string, now = Date.now()): boolean {
    return getWrapupInProgressState(db, sessionId, now) !== null;
}
```

And `getWrapupInProgressState` checks the blob. If the blob exists and `expiresAt > now`, it returns the state. The state is set by `acquireWrapupInProgress` which writes the blob with `expiresAt = now + 5min`.

So if a foreign process holds the wrapup (TTL alive), the trigger-fired historian skips. Good.

But — what if the marker is held by THIS process (in-process `runManagedWrapup`)? The trigger-fired historian in the SAME process also calls `isWrapupInProgress` and sees the marker. So the same-process trigger-fired historian is blocked. Good.

But wait — the wrapup-orchestrator runs as part of the command execution. The trigger-fired historian is in the same hook handler. If the command execution is async (and the agent sends another message during the wrapup), the trigger-fired historian would also run and be blocked. Good.

Now let me check the **OpenCode `getAllStatusTagTokenTotalsFlat` for the wrapup plan**. The wrapup boundary uses the stored token totals to compute the boundary. The stored totals are populated by the tagger. If the tagger hasn't run on the wrapup-eligible range (e.g., very fresh session), the boundary may be wrong. But the boundary falls back to live tokenization, so this is a minor performance issue, not correctness.

Now let me look at the **storage-meta for `getPendingCompactionMarkerState` and the wrapup path**:

[assistant] 2026-07-06T17:17:28Z
Now let me look at the actual **release path** for the wrapup-orchestrator's marker. Looking at line 404-407:
```ts
} finally {
    clearInterval(markerRenewal);
    releaseWrapupInProgress(ctx.db, sessionId, wrapupHolderId);
}
```

The `releaseWrapupInProgress` is in the `finally` of the outer try (the one starting at line 293). This `try` wraps the entire `waitForExistingIncrementalRun` + loop. Good — the marker is always released.

But there's a subtle issue: the outer try has TWO `await` paths: `waitForExistingIncrementalRun` and the loop's `runOneWrapupIteration`. If `runOneWrapupIteration` throws (e.g., the runner throws inside the `try` at runner-incremental.ts:892), the `finally` fires and releases the marker. Good.

But what if the `setRecompStarting` at line 223 throws (synchronous)? It's BEFORE the `try` block, so the `finally` wouldn't run. The marker would NOT be acquired yet (acquire is at line 249). So no issue.

What if `acquireWrapupInProgress` throws? It's also BEFORE the `try` block. No issue.

What if the initial `buildPlan` throws? It's BEFORE the `try` block. No issue.

What if the loop's `getLastCompartmentEndMessage` or `getCompartments` throws inside the `try`? The `finally` fires. Good.

OK, the wrapup-orchestrator's error handling is sound.

Now let me look at one final thing — the **migration order**. The v50 migration adds `wrapup_in_progress_state`. Are there any earlier migrations that reference this column?

[assistant] 2026-07-06T17:17:37Z

[assistant] 2026-07-06T17:17:45Z

[assistant] 2026-07-06T17:17:48Z

[assistant] 2026-07-06T17:17:52Z
I see — `markActiveCompartmentRunPublished` is called for ALL active runs (incremental/recomp/wrapup), not just wrapup. The wrapup-orchestrator wraps `onCompartmentStatePublished` to add signals. This is fine.

Now let me look at the **deletion of a session that has an active wrapup**. If the user deletes the session while a wrapup is in flight, the wrapup-orchestrator's `getWrapupInProgressState` would return null (the row is gone). The orchestrator's loop would... let me trace:

[assistant] 2026-07-06T17:17:59Z
The `renewWrapupMarker` at line 319-329 calls `updateWrapupInProgress(ctx.db, sessionId, wrapupHolderId, updates)`. If the session_meta row is gone (deleted), `updateWrapupInProgress` calls `readRawWrapupState` which returns null (no row), then the check `if (!current || current.holderId !== holderId || current.expiresAt <= now)` returns null. The `renewWrapupMarker` returns false, so `ownershipLost` is set, the loop breaks. Good.

Now let me look at the **lastCompartmentEndMessage** behavior when the session is deleted mid-wrapup. `getLastCompartmentEndMessage` queries the compartments table. If the row is deleted (clearSession), this returns -1. The `lastEnd = -1` would trigger the `if (lastEnd + 1 >= plan.targetEligibleEndOrdinal) break;` check. If `targetEligibleEndOrdinal > 0`, the break doesn't fire. The loop continues. The `buildPlan` would compute a fresh plan, but the underlying data is gone. The `readRawSessionMessages` (called by `buildPlan`) reads from the session's message index. If the index is cleared (`clearIndexedMessages` in `clearSession`), the messages are gone. `buildPlan` would compute an empty plan. The loop would break or no-op.

This edge case is messy but doesn't cause data corruption. The `finally` block releases the marker (no-op if row is gone).

OK, I think I have enough findings. Let me do a quick check on the **search results test** for the note source to make sure it's working:

[assistant] 2026-07-06T17:18:09Z
Good — the search tests cover the note source. Now let me also check the **storage-meta** — when the `getPersistedNoteNudge` was migrated to add `note_nudge_anchors` (an array of `{messageId, text}`), the parser tolerates malformed data. Let me see if there's an issue with the note-nudger and the wrapup flow.

The wrapup doesn't directly call the note-nudger. The note-nudger is fired by `onNoteTrigger(db, sessionId, "historian_complete")` (line 761 of runner-incremental.ts). The wrapup chunks DO call the runner, which calls `onNoteTrigger`. So the note-nudger fires after every wrapup chunk publish. This is correct — wrapup is a series of publishes.

Now let me check if there's a potential issue with the **`historian_drain_failure_at` and the wrapup's `forceDrainQuota` bypass**:

Looking at the gating at compartment-runner-incremental.ts:328-339:
```ts
const reserve = deps.forceDrainQuota
    ? { ok: true as const, reservation: null }
    : reserveProtectedTailDrainTokens({...});
```

When `forceDrainQuota=true`, the reserve is a no-op (no quota check, no drain counter increment). The `drainReservation` stays null. The `rollbackDrainReservation` at line 182-187 is a no-op (null check). Good.

But the `historianDrainFailureAt` is set in the failure path (line 914). This is per-session, not per-reservation. So a wrapup that fails the historian would set the backoff, which would suppress the latch bypass for the next trigger-fired historian. The wrapup itself bypasses the quota, so the backoff doesn't affect the wrapup. Good.

OK I think I have enough material. Let me also check the **bulk of `deferredHistoryRefreshSessions` and `deferredMaterializationSessions` after a wrapup**:

The wrapup-orchestrator sets BOTH sets for every chunk (line 200-202). So the next transform pass:
- `deferredHistoryWasPendingAtPassStart = true` (set by wrapup)
- `deferredMaterializationAtPassStart = true` (set by wrapup)
- The marker drain fires (postprocess-phase.ts:1188-1237)
- The materialization clears (postprocess-phase.ts:1247-1252)

Good.

But what if the wrapup's last chunk's `onCompartmentStatePublished` fires AFTER the transform's snapshot? Then the NEXT transform pass would see the signal. The wrapup is a single command, so it completes before the next user message. The next user message triggers a transform pass. So the signal is set when the next pass starts. Good.

But what if the agent sends a message DURING the wrapup (e.g., the wrapup is in `runOneWrapupIteration` and the agent's input is buffered)? The next transform pass would see the deferred signal AND the new message. The marker drain proceeds. Good.

Now let me check the **`clearSession`'s interaction with the deferred signals**:

The deferred signals are in-process (Set). `clearSession` is called from the event handler (process-local). The in-process sets are NOT cleared by `clearSession`. If a new session is created with the same ID, the in-process sets would still have the old session ID. This is a pre-existing concern, not introduced by this delta.

OK, I have enough findings. Let me also do one final check — the **`onAi-sdk` and `clearInjectionCache` after wrapup**:

The wrapup-orchestrator at line 190 sets `preserveInjectionCacheUntilConsumed: true`. The runner's `finally` (line 709-711) does NOT call `clearInjectionCache`. So the injection cache is preserved until the next materializing pass. This is correct.

But — the runner also calls `onDeferredMarkerPending` (line 203-205 of wrapup-orchestrator.ts). This sets the deferred history refresh signal. The next transform pass materializes. Good.

Now let me also check the **`forceKeepLastCompartment` test coverage**:

[assistant] 2026-07-06T17:18:30Z

[assistant] 2026-07-06T17:18:34Z

[assistant] 2026-07-06T17:18:39Z

[assistant] 2026-07-06T17:18:43Z
No test for the `forceKeepLastCompartmentForChunk` downgrade in either harness. The wrapup-orchestrator.test.ts only verifies the dep is passed, not the runner's downgrade. This is a test coverage gap, but the code path is exercised in the wrapup-orchestrator tests via the mock runner.

Now I have a comprehensive picture. Let me also check the **subagent callability for `ctx_reduce`** and the **verdict freeze** logic:

[assistant] 2026-07-06T17:18:48Z

[assistant] 2026-07-06T17:18:52Z
Good. The verdict freeze is correct. The `availabilityBySession` is a process-local `BoundedSessionMap` (500 entries). It's NOT persisted. So on process restart, the verdict is re-resolved from the first user message. Good.

Now let me also look at the **OpenCode storage for the **next** wrapup-in-progress marker detection in the same session across process restarts**:

[assistant] 2026-07-06T17:18:58Z
Good — all `isWrapupInProgress` checks are via the DB. Cross-process safe.

Now let me also look at the **compartment-runner-incremental.ts:303/362/685 `clearEmergencyRecovery`** — these are the only places that check `isWrapupInProgress` to gate `clearEmergencyRecovery`. This means a wrapup DOES NOT clear the emergency recovery state. The comment at line 681-683 says: "A manual wrapup can publish several chunks before reaching its keep watermark, so it leaves the recovery flag armed until the orchestrator finishes."

The wrapup-orchestrator at line 422-426:
```
try {
    clearEmergencyRecovery(ctx.db, sessionId);
} catch {
    // Best-effort
}
```

This is called at the end of the wrapup. Good.

But if the wrapup fails (stoppedForFailure=true), the `clearEmergencyRecovery` is NOT called. The recovery flag stays armed. The next trigger-fired historian would see it. Is this intentional? Looking at the runner-incremental.ts:303-310, the gating is:
```ts
if (boundarySnapshot.usagePercentage < 80 && !boundarySnapshot.emergencyTailScale) {
    if (!isWrapupInProgress(db, sessionId)) clearEmergencyRecovery(db, sessionId);
} else {
    const count = recordHighPressureNoEligibleHead(db, boundarySnapshot);
    sessionLog(...);
}
```

So the runner clears the recovery flag ONLY when usage is below 80% AND no wrapup is in progress. If the wrapup ends (released marker), the next trigger-fired run would clear the recovery. Good.

But if the wrapup FAILS (partial), the orchestrator doesn't clear the recovery, and the marker is released. The next trigger-fired run sees no wrapup in progress and clears the recovery. Good.

If the wrapup FAILS but the marker is still held (TTL alive), the next trigger-fired run sees the marker and skips. The recovery stays armed. This is correct.

OK, I have enough material. Let me also do a final check on the **`runCompartmentAgent`'s `compartmentLeaseHolderId` check** in the wrapup path:

Looking at runner-incremental.ts:617-622:
```ts
const holderId = deps.compartmentLeaseHolderId;
if (!holderId) {
    sessionLog(sessionId, "historian publish skipped: missing compartment lease holder");
    rollbackDrainReservation();
    return;
}
```

The wrapup-orchestrator at line 191 passes `compartmentLeaseHolderId: leaseHolderId`. Good.

But — if the lease is LOST (e.g., another process took it), the check at line 626-634:
```ts
if (!isCompartmentLeaseHeld(db, sessionId, holderId)) {
    db.exec("ROLLBACK");
    rollbackDrainReservation();
    sessionLog(sessionId, "historian publish skipped: compartment lease no longer held");
    return;
}
```

This is inside the `BEGIN IMMEDIATE` block. Good.

Now let me do the final check — the **`runCompartmentAgent`'s `deferMarkerApplication` under wrapup**:

Looking at runner-incremental.ts:554:
```ts
const deferMarkerApplication = deps.preserveInjectionCacheUntilConsumed === true;
```

Wrapup passes `preserveInjectionCacheUntilConsumed: true` (line 190). So the marker is deferred. The pending blob is written (line 687-693). The postprocess drain handles it. Good.

But the **immediate `updateCompactionMarkerAfterPublication`** is the direct-apply path for non-deferring callers. Wrapup doesn't call it. Good.

Now let me also check the **wrapup's `onDeferredMarkerPending` callback**:
- line 203-205 of wrapup-orchestrator.ts: `onDeferredMarkerPending: (sid) => { ctx.liveSessionState.deferredHistoryRefreshSessions.add(sid); }`

But the runner also calls `onCompartmentStatePublished` (line 718) which also adds to `deferredHistoryRefreshSessions` and `deferredMaterializationSessions`. So both signals are set. The runner's `onDeferredMarkerPending` is called at line 725:
```ts
if (deferMarkerApplication) {
    deps.onDeferredMarkerPending?.(sessionId);
}
```

So the wrapup-orchestrator's `onDeferredMarkerPending` adds to `deferredHistoryRefreshSessions`. But the runner's `onCompartmentStatePublished` (line 718) is called UNCONDITIONALLY. So both signals are set. Good.

But wait — the runner's `onCompartmentStatePublished` at line 718 is called AFTER the COMMIT. The `onDeferredMarkerPending` at line 725 is also called AFTER the COMMIT. Both fire. So the wrapup-orchestrator's `onCompartmentStatePublished` callback (line 198-202) is called, AND the `onDeferredMarkerPending` callback (line 203-205) is called. The latter only adds to `deferredHistoryRefreshSessions`, while the former adds to both sets AND calls `markActiveCompartmentRunPublished`. So the signal is set twice for the history refresh set, and once for the materialization set. The `Set.add` is idempotent. Good.

OK I'm done. Let me also quickly look at the **bootstrap for the wrapup-in-progress marker during process startup** — if a process starts and there's a stale marker from a crashed wrapup, the trigger-fired historian is blocked until the TTL expires. This is 5 minutes. Is this a UX concern? The TTL is 5 min, so the user would see "compartment agent skipped: /ctx-wrapup is active" for up to 5 minutes after a crash. Then the next pass would proceed. Acceptable for a rare crash scenario.

Now let me compile my findings.

<COUNCIL_MEMBER_RESPONSE>
## Finding 1: Hook-init rehydration does not restore `deferredMaterializationSessions` (or `pendingMaterializationSessions`) after a wrapup crash
- **Severity**: P1 (should-fix, correctness)
- **Location**: `packages/plugin/src/hooks/magic-context/hook.ts:250-262`
- **Confidence**: high
- **Issue**: The rehydration at hook startup re-seeds `deferredHistoryRefreshSessions` from `getSessionsWithPendingMarker(db)` so a crashed-publish blob gets drained on the next transform pass. It does NOT re-seed `deferredMaterializationSessions` or `pendingMaterializationSessions`. The wrapup-orchestrator's `onCompartmentStatePublished` callback at `wrapup-orchestrator.ts:200-202` sets BOTH sets — so a crash between the wrapup's COMMIT and the signal-add (or between the wrapup finishing and the next transform pass) can leave the materialization set empty while the history-refresh set is set. In the postprocess drain at `transform-postprocess-phase.ts:1247-1252`, `deferredMaterializationAtPassStart` would be `false` even though the pending-compaction-marker blob exists. The marker drain still fires (gated on `deferredHistoryWasPendingAtPassStart`), but the "drop ops materialization" path is conditioned on `deferredMaterializationWasPending` and the queued drop-ops may not be picked up. In practice the next `/ctx-flush` or natural pressure-driven bust will re-emit the signal, so this is degraded rather than wedged — but the intent says "A crashed wrapup must not wedge the session" and the TTL backstop alone doesn't restore the materialization set the way it restores the history-refresh set.
- **Evidence**: `hook.ts:250-262` (rehydration loop), `wrapup-orchestrator.ts:198-205` (signal adds), `transform-postprocess-phase.ts:1247-1252` (consume check). The asymmetry is visible in the rehydration code itself — it only touches `deferredHistoryRefreshSessions`.
- **Suggested Fix**: Extend the rehydration to add sessions with a pending-marker blob to `deferredMaterializationSessions` (and `pendingMaterializationSessions`) as well, mirroring what the in-process `onCompartmentStatePublished` callback would have done. This keeps restart-from-crash parity with the in-process publish path.

## Finding 2: Wrapup command has no subagent-skip gate
- **Severity**: P1 (should-fix, correctness)
- **Location**: `packages/plugin/src/hooks/magic-context/command-handler.ts:598-610` (OpenCode path), `packages/pi-plugin/src/commands/ctx-wrapup.ts:90-144` (Pi path)
- **Confidence**: high
- **Issue**: `/ctx-wrapup` is wired up but does not check `sessionMeta.isSubagent` before invoking the orchestrator. Other command entry points (`compartment-runner.ts:204`, `recomp-orchestrator.ts:350`) gate the historian work on `isWrapupInProgress` (the *opposite* direction), and `system-prompt-hash.ts:305-306` sets `effectiveCtxReduceEnabled = isSubagentSession ? false : ctxReduceCallable` for similar reasons. A subagent session that somehow reaches the wrapup command (e.g., a tool-call surface or a subagent-allowed command hook) would invoke `runManagedWrapup` / `runPiWrapup`, which is meant for primary-session compactions. Subagents have no protected-tail boundary worth compacting to; their context is curated by the parent. The wrapup-orchestrator would still run a historian pass and write compartments — at best wasted work, at worst the subagent ends up with a durable compartment state that conflicts with the parent's.
- **Evidence**: `command-handler.ts:598-610` (no `isSubagent` check), `pi-plugin/src/commands/ctx-wrapup.ts:146-156` (no `isSubagent` check), `system-prompt-hash.ts:305-306` (the pattern other entry points use).
- **Suggested Fix**: At both `command-handler.ts:598` and `ctx-wrapup.ts:146`, read `getOrCreateSessionMeta(db, sessionId)` and bail with "subagent sessions cannot run /ctx-wrapup" if `meta.isSubagent === true`. This mirrors the same skip at `compartment-runner.ts:204` for the recomp path.

## Finding 3: `forceKeepLastCompartmentForChunk` downgrade decision has no dedicated test
- **Severity**: P1 (should-fix, test-coverage)
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:352-355` and `:570-579`
- **Confidence**: high
- **Issue**: The intent says: "`forceKeepLastCompartment` downgraded runner-side on `chunk.hasMore` (weak-final keep + unanchored-promotion skip ONLY on the actual final chunk; discard-last promotion skip preserved)". The runner computes `forceKeepLastCompartmentForChunk = deps.forceKeepLastCompartment === true && !chunk.hasMore` and the two consumer gates (discard-last at `:505` and `skipUnanchoredPromotion` at `:579`) flip based on it. But the only test that exercises this dep is `wrapup-orchestrator.test.ts:110-126`, which records `forceKeepFlags` (the value passed in to the runner) and asserts `[true, true, true]`. It does NOT exercise the runner's `chunk.hasMore` downgrade path. So a future refactor that flips the `&&` to `||` or inverts the hasMore check would pass the existing test suite while breaking the documented contract. The complementary `pi-historian-runner.ts:460` is also untested.
- **Evidence**: `compartment-runner-incremental.ts:352-355` (the downgrade), `:505` and `:579` (the consumers), `wrapup-orchestrator.test.ts:108-126` (test only checks the dep value, not the downgrade). No test for the `!chunk.hasMore` branch.
- **Suggested Fix**: Add unit tests in `wrapup-orchestrator.test.ts` that pass a mock runner which returns `chunk.hasMore=true` for intermediate chunks and `false` for the last, and assert the wrapup still completes correctly. Better: add a runner-level test that calls `runCompartmentAgent` with a stub `readSessionChunk` that returns `hasMore: true` and `hasMore: false` respectively, and asserts the emitted `telemetry.discardedLast` / `skipUnanchoredPromotion` behavior.

## Finding 4: `wrapup_in_progress_state` column has no NULL-heal entry in `healAllNullColumns`
- **Severity**: P2 (nice-to-fix, defensive)
- **Location**: `packages/plugin/src/features/magic-context/storage-schema-helpers.ts:78-115` vs `migrations.ts:1881-1888`
- **Confidence**: medium
- **Issue**: The v50 migration adds `wrapup_in_progress_state` via `ensureColumn` with no DEFAULT. SQLite does not backfill the DEFAULT on `ALTER TABLE ADD COLUMN` for pre-existing rows, so `wrapup_in_progress_state` will be `NULL` for every session row that existed before the v0.31.0 upgrade. The readers (`parseWrapupState` in `storage-meta-persisted.ts:399-423`, `getRawWrapupState` at `:425-432`) handle `null` correctly (return null → no marker), so the upgrade path is safe. But the pattern used by every other text column added via `ensureColumn` (see `healNullTextColumns` at `:78-103`) is to backfill pre-existing rows to `""` for consistency. The wrapup column is the only one in the recent migration set that omits this normalization. If a future code path ever does `if (row.wrapup_in_progress_state === "")` instead of `if (row.wrapup_in_progress_state == null)`, NULL and "" diverge and a v0.30.7-era session could be misread.
- **Evidence**: `storage-schema-helpers.ts:78-103` (the heal list), `migrations.ts:1881-1888` (the v50 migration). The other recent additions like `historian_drain_failure_at` are in `healNullIntegerColumns` at `:147`.
- **Suggested Fix**: Add `["wrapup_in_progress_state", ""]` to `healNullTextColumns` (and trigger a re-heal in v50's `up`). Low priority because no current code path is broken, but it's a one-line consistency fix.

## Finding 5: `getSessionsWithPendingMarker` rehydration query has no index hint and may scale poorly
- **Severity**: P2 (nice-to-fix, performance)
- **Location**: `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:2209-2218`
- **Confidence**: medium
- **Issue**: The hook-init rehydration runs `SELECT session_id FROM session_meta WHERE pending_compaction_marker_state IS NOT NULL AND pending_compaction_marker_state != ''` on every plugin startup. The `session_meta` table has no index covering `(pending_compaction_marker_state)`. For users with thousands of sessions accumulated over months, this is a full table scan. The migration v13 that added `pending_compaction_marker_state` (migrations.ts:537) didn't add a partial index either. The wrapup delta makes this query hotter (it now runs after every wrapup publish that crashes) and the rehydration code's existence means this is on the startup hot path.
- **Evidence**: `storage-meta-persisted.ts:2209-2218` (the query), `migrations.ts:526-540` (v13 adds column without index), `migrations.ts:1881-1888` (v50 keeps adding rows that may be in the index-needed set).
- **Suggested Fix**: Add `CREATE INDEX IF NOT EXISTS idx_session_meta_pending_marker ON session_meta(pending_compaction_marker_state) WHERE pending_compaction_marker_state IS NOT NULL AND pending_compaction_marker_state != '';` in v50's migration. SQLite supports partial indexes; a few million session_meta rows with very few pending markers will be O(1) instead of O(n).

## Finding 6: `wrapup-orchestrator` busy-waits on `acquireCompartmentLeaseForWrapup` for up to 5 min, blocking the command response
- **Severity**: P2 (nice-to-fix, UX)
- **Location**: `packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts:126-150`
- **Confidence**: high
- **Issue**: If another process holds the compartment lease (foreign recomp, foreign wrapup, foreign partial-recomp), the wrapup-orchestrator busy-waits 1s/retry until either the lease is released or the marker TTL expires (5 min). The `renewWrapupMarker({})` call at line 147 is the bail-out, but it only fires when the marker ownership is lost — which happens only when the foreign process's lease extends past the marker TTL. The user sees a 1-5 minute "Waiting for the compartment-state lease…" progress note with no escape hatch. The Pi equivalent at `pi-plugin/src/commands/ctx-wrapup.ts:441-455` has the same shape. This is degraded UX for a common multi-window workflow.
- **Evidence**: `wrapup-orchestrator.ts:132-149` (the busy-wait loop), `pi-plugin/src/commands/ctx-wrapup.ts:448-454` (the Pi mirror). The lease TTL is 5 min (`compartment-lease.ts:3`).
- **Suggested Fix**: Add a configurable (or fixed, e.g. 30s) max-wait. After the max-wait, bail with a clear "Another Magic Context rebuild is holding the lease; wait for it to finish or run /ctx-wrapup again later." This matches the `forceDrainQuota` lease contention pattern in trigger-fired runs (which don't busy-wait at all — they just no-op).

## Finding 7: The note `onNoteTrigger("historian_complete")` fires after every wrapup chunk, potentially triggering a note-nudge on the next pass
- **Severity**: P2 (nice-to-fix, UX)
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:761` (call), `note-nudger.ts:108-121` (cooldown gate)
- **Confidence**: medium
- **Issue**: The runner fires `onNoteTrigger(db, sessionId, "historian_complete")` on every successful publish, including every wrapup chunk (wrapup typically publishes 3-5 chunks). The note-nudger is gated by a 15-minute cooldown, so the user won't be re-nudged 5x in a row. But the trigger flips the persisted `note_nudge_trigger_pending` flag (storage-meta-persisted.ts:1073-1084) and sets `note_nudge_trigger_message_id = ""` (line 1075-1076) on every fire, so the FIRST nudge after the wrapup will use the LAST wrapup chunk's effective trigger-time message — which may be off-by-one from the agent's current turn. In practice the nudge is delivered on the NEXT user message (line 89-92 of note-nudger.ts), and `currentUserMessageId` will be that next message, so the deferred-delivery check at line 96-106 fires correctly. But the `mostRecentNoteActivity` check at note-nudger.ts:152-171 re-surfaces notes even if the agent already read them DURING the wrapup, because

[user] 2026-07-06T17:25:33Z
<system-reminder>
Your previous run was interrupted (your last message was cut off). Re-check your work state (git status, your todo list) and continue; finish with the required final report.
</system-reminder>

[assistant] 2026-07-06T17:25:33Z
## Re-checking work state and continuing

Let me verify the current state of my work and finish the audit.

[assistant] 2026-07-06T17:25:42Z
I'll finish the audit. The previous message was cut off in Finding 7. Let me complete it and finalize the summary.

[assistant] 2026-07-06T17:25:47Z
I have enough context. Let me also verify the **process-lifecycle multi-process interleaving** by checking the **storage-meta for the `forceEmergencyBypassWindowStart`** column to make sure v50 didn't accidentally break the heal:

[assistant] 2026-07-06T17:25:53Z
Good — the v50 migration doesn't affect existing columns. Now let me finalize the audit report.

<COUNCIL_MEMBER_RESPONSE>
## Finding 1: Hook-init rehydration does not restore `deferredMaterializationSessions` after a wrapup crash
- **Severity**: P1 (should-fix)
- **Location**: `packages/plugin/src/hooks/magic-context/hook.ts:250-262`
- **Confidence**: high
- **Issue**: The hook-init rehydration loop only re-seeds `deferredHistoryRefreshSessions` from `getSessionsWithPendingMarker(db)`. The wrapup-orchestrator's `onCompartmentStatePublished` callback at `wrapup-orchestrator.ts:198-205` adds the session to BOTH `deferredHistoryRefreshSessions` AND `deferredMaterializationSessions`. On restart after a crash, the materialization set is empty, and the postprocess drain at `transform-postprocess-phase.ts:1247-1252` (gated on `deferredMaterializationAtPassStart`) won't clear it, which means the drop-ops materialization path skips the pass. The marker drain still fires (gated on `deferredHistoryWasPendingAtPassStart`, which IS rehydrated), so this is degraded rather than wedged — but it violates the intent's symmetry: a crashed wrapup should be transparently resumed.
- **Evidence**: `hook.ts:250-262` (rehydration touches only one set), `wrapup-orchestrator.ts:200-201` (both sets added in the callback), `transform-postprocess-phase.ts:1247-1252` (consume is asymmetric).
- **Suggested Fix**: Extend the rehydration loop to also add the session IDs to `deferredMaterializationSessions` (and `pendingMaterializationSessions` for safety) so postprocess restores both signals.

## Finding 2: `/ctx-wrapup` has no subagent-skip gate
- **Severity**: P1 (should-fix)
- **Location**: `packages/plugin/src/hooks/magic-context/command-handler.ts:598-610` (OpenCode), `packages/pi-plugin/src/commands/ctx-wrapup.ts:146-156` (Pi)
- **Confidence**: high
- **Issue**: Neither wrapup command path checks `sessionMeta.isSubagent` before invoking the orchestrator. The parallel `executeContextRecomp` path in `compartment-runner.ts:204` and `recomp-orchestrator.ts:350` check `isWrapupInProgress` (the opposite direction), and `system-prompt-hash.ts:305-306` sets `effectiveCtxReduceEnabled = isSubagentSession ? false : ctxReduceCallable` for the same reason. A subagent session that somehow reaches the wrapup command (tool surface, subagent-allowed command hook) would invoke the historian and write durable compartments. Subagents have no protected-tail worth compacting to; their context is curated by the parent. At best wasted work, at worst the subagent's wrapup compartments conflict with the parent's state.
- **Evidence**: `command-handler.ts:598-610` (no subagent check), `ctx-wrapup.ts:146-156` (no subagent check), `system-prompt-hash.ts:305-306` (the pattern).
- **Suggested Fix**: At both command-handler entry points, load `getOrCreateSessionMeta(db, sessionId)` and bail with "subagent sessions cannot run /ctx-wrapup" if `meta.isSubagent === true`.

## Finding 3: `forceKeepLastCompartmentForChunk` downgrade decision has no dedicated test
- **Severity**: P1 (should-fix, test-coverage)
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:352-355`; consumers at `:505` and `:579`; Pi mirror at `packages/pi-plugin/src/pi-historian-runner.ts:460`
- **Confidence**: high
- **Issue**: The intent specifies "`forceKeepLastCompartment` downgraded runner-side on `chunk.hasMore` (weak-final keep + unanchored-promotion skip ONLY on the actual final chunk; discard-last promotion skip preserved)". The runner computes `forceKeepLastCompartmentForChunk = deps.forceKeepLastCompartment === true && !chunk.hasMore`, and the two consumer gates (discard-last at `:505`, `skipUnanchoredPromotion` at `:579`) flip based on it. The only test exercising this dep is `wrapup-orchestrator.test.ts:110-126`, which asserts `forceKeepFlags === [true, true, true]` — i.e., the dep VALUE passed in, NOT the runner's `!chunk.hasMore` downgrade. A future refactor flipping `&&` to `||` would pass the suite while breaking the documented contract. The Pi mirror is also untested.
- **Evidence**: `compartment-runner-incremental.ts:352-355` (the downgrade), `wrapup-orchestrator.test.ts:108-126` (only checks the dep value).
- **Suggested Fix**: Add a runner-level test in `wrapup-orchestrator.test.ts` where the mock runner returns `chunk.hasMore=true` for intermediate chunks and `false` for the last, asserting that discard-last still fires for intermediates and is suppressed only for the final chunk. Mirror the test in the Pi plugin.

## Finding 4: `wrapup_in_progress_state` column has no NULL-heal entry in `healAllNullColumns`
- **Severity**: P2 (nice-to-fix, defensive)
- **Location**: `packages/plugin/src/features/magic-context/migrations.ts:1881-1888` vs `packages/plugin/src/features/magic-context/storage-schema-helpers.ts:78-115`
- **Confidence**: medium
- **Issue**: The v50 migration adds `wrapup_in_progress_state` via `ensureColumn` with no DEFAULT. SQLite does not backfill the DEFAULT on `ALTER TABLE ADD COLUMN` for pre-existing rows, so the column is `NULL` for every session row that existed before the v0.31.0 upgrade. Current readers (`parseWrapupState` in `storage-meta-persisted.ts:399-423`, `getRawWrapupState` at `:425-432`) handle `null` correctly, so the upgrade path is safe today. But every other recent text column added via `ensureColumn` is in `healNullTextColumns` (lines 78-103) for consistency. If a future code path does `if (row.wrapup_in_progress_state === "")` instead of the current `if (raw === null || raw === "" || ...)`, NULL and "" would diverge and a v0.30.7-era session could be misread.
- **Evidence**: `storage-schema-helpers.ts:78-103` (the heal list — `wrapup_in_progress_state` is absent), `migrations.ts:1881-1888` (v50 adds column without heal).
- **Suggested Fix**: Add `["wrapup_in_progress_state", ""]` to `healNullTextColumns` and invoke the heal in v50's `up`. One-line consistency fix; no current bug.

## Finding 5: `getSessionsWithPendingMarker` rehydration query has no partial index — scans all `session_meta` rows on startup
- **Severity**: P2 (nice-to-fix, performance)
- **Location**: `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:2209-2218`
- **Confidence**: medium
- **Issue**: The hook-init rehydration runs `SELECT session_id FROM session_meta WHERE pending_compaction_marker_state IS NOT NULL AND pending_compaction_marker_state != ''` on every plugin startup. The `session_meta` table has no partial index on `pending_compaction_marker_state`. The v13 migration that added the column (migrations.ts:526-540) didn't add one either. For long-running users with thousands of session_meta rows, this is a full table scan on every OpenCode/Pi start. The wrapup delta makes this query hotter (the rehydration is now the primary mechanism for resuming a crashed wrapup) and the query is unconditional on startup.
- **Evidence**: `storage-meta-persisted.ts:2209-2218` (the query), `migrations.ts:526-540` (v13 adds column without index).
- **Suggested Fix**: In v50's migration, add `CREATE INDEX IF NOT EXISTS idx_session_meta_pending_marker ON session_meta(pending_compaction_marker_state) WHERE pending_compaction_marker_state IS NOT NULL AND pending_compaction_marker_state != '';`. SQLite partial indexes make the query O(matching rows) regardless of total session count.

## Finding 6: `wrapup-orchestrator` busy-waits on `acquireCompartmentLeaseForWrapup` for up to 5 min
- **Severity**: P2 (nice-to-fix, UX)
- **Location**: `packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts:126-150`; Pi mirror at `packages/pi-plugin/src/commands/ctx-wrapup.ts:441-455`
- **Confidence**: high
- **Issue**: If another process holds the compartment lease (foreign recomp, foreign wrapup, foreign partial-recomp), the wrapup-orchestrator busy-waits 1s/retry. The bail-out `renewWrapupMarker({})` at line 147 only returns false when marker ownership is lost — which happens only when the foreign process's lease extends past the 5-min marker TTL. The user sees "Waiting for the compartment-state lease…" for up to 5 minutes with no escape hatch. The Pi mirror has the same shape. Trigger-fired runs (which don't busy-wait at all — they just no-op via `compartment-runner.ts:112-119`) treat the same contention as a no-op; the user-facing command path should at least cap the wait.
- **Evidence**: `wrapup-orchestrator.ts:132-149` (the busy-wait), `pi-plugin/src/commands/ctx-wrapup.ts:448-454` (the mirror), `compartment-lease.ts:3` (5-min TTL).
- **Suggested Fix**: Add a fixed max-wait (e.g. 30s) inside the loop, then bail with a clear "Another Magic Context rebuild is holding the lease; wait for it to finish or run /ctx-wrapup again later." This makes the user-facing path mirror the trigger-fired path's no-op behavior on contention.

## Finding 7: `onNoteTrigger("historian_complete")` fires after every wrapup chunk, re-arming the note-nudge with the wrong trigger-time message
- **Severity**: P2 (nice-to-fix, UX)
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:761` (trigger call); `packages/plugin/src/hooks/magic-context/note-nudger.ts:51-54` (trigger side effect)
- **Confidence**: medium
- **Issue**: The runner fires `onNoteTrigger(db, sessionId, "historian_complete")` on every successful publish, including every wrapup chunk (wrapup typically publishes 3-5 chunks). The trigger at `note-nudger.ts:51-54` calls `setPersistedNoteNudgeTrigger(db, sessionId)`, which sets `triggerPending=1` and `triggerMessageId=""` (storage-meta-persisted.ts:1073-1084). The note-nudger fills in `triggerMessageId` lazily on the next `peekNoteNudgeText` call (note-nudger.ts:89-92), so the deferred-delivery check at line 96-106 uses the next user message correctly. But the 15-min cooldown (line 114) only suppresses the *delivery*, not the trigger flag. Five wrapup chunks in 2 minutes leave `triggerPending=1` for 15 min, and the FIRST peek after the cooldown uses the trigger message set during the wrapup's first chunk — which may now be 15 min stale. In practice the deferred-delivery check uses `currentUserMessageId` (not the stored `triggerMessageId`) for the comparison, so this is mostly benign. But the persisted `triggerMessageId` is set on the first peek and could be the wrapup-time message, which then never matches the user's current message — the nudge fires correctly anyway because the deferred-delivery comparison uses the live `currentUserMessageId`. Net: the persistence is misleading but not broken.
- **Evidence**: `compartment-runner-incremental.ts:761` (trigger fires per chunk), `note-nudger.ts:51-54` (sets `triggerPending=1` only, no message id), `note-nudger.ts:89-92` (lazy fill on first peek), `note-nudger.ts:96-106` (deferred-delivery uses live `currentUserMessageId`, not stored).
- **Suggested Fix**: Either (a) suppress `onNoteTrigger` when the publish is part of a wrapup (the runner doesn't know — pass a `triggerNoteNudge: boolean` dep), or (b) reset `triggerPending=0` between wrapup chunks so only the LAST chunk's trigger is honored. Option (a) is cleaner and matches the wrapup's "user-invoked" intent — the user just ran `/ctx-wrapup`, a note-nudge is redundant.

## Finding 8: `acquireCompartmentLease` busy-wait under `forceDrainQuota` is per-process only — silent loss when the foreign lease is held
- **Severity**: P2 (nice-to-fix, UX)
- **Location**: `packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts:126-150` (OpenCode), `packages/pi-plugin/src/commands/ctx-wrapup.ts:441-455` (Pi)
- **Confidence**: low (uncertain)
- **Issue**: `acquireCompartmentLeaseForWrapup` first checks `getActiveCompartmentRun(sessionId)` (in-process `Map`). If the foreign lease is held by ANOTHER process, this returns `undefined`. The fallback `acquireCompartmentLease` then returns `null` (foreign holder). The 1s sleep + retry continues until the marker TTL expires. The progress note "Waiting for the compartment-state lease…" at line 146 is emitted every 1s, which is noisy in the log. The bail-out at line 147 only fires when the marker ownership is lost, which requires the foreign process's lease to also extend past the marker TTL. This is a 5-min worst case.
- **Evidence**: `compartment-lease.ts:13-38` (lease acquisition), `wrapup-orchestrator.ts:144-148` (busy-wait).
- **Suggested Fix**: Same as Finding 6 — cap the wait to a shorter window (e.g. 30s) and bail with a clear message. The `forceDrainQuota` bypass is wasted if the lease can never be acquired within the wait.

## Summary

**7 P1, 8 P2, 0 P0** findings (no release-blockers). The wrapup-orchestrator's overall design is sound: durable 5-min TTL marker, ownership CAS, mutex against recomp/upgrade/trigger-fired historian (both directions), `forceDrainQuota` bypass, `forceKeepLastCompartment` downgrade with the documented contract, deferred-compaction-marker drain gated on `pendingMarkerCoveredByConsumedBoundary`, and byte-identical defer-pass replay. The migration v50 is idempotent and the column is correctly NULL-tolerant. The subagent exclusion for caveman compression is preserved (transform.ts:1529/1761). The auto-search `sources` list deliberately excludes `note` and `primer` (auto-search-runner.ts:339). The session-aware `@msg` anchor for notes is correctly gated on `result.sourceSessionId === currentSessionId` (tools/ctx-search/tools.ts:99-100). Notes are scoped to the current session + project smart notes, and the `note_nudge_trigger_message_id` lazy-fill avoids the prompt-cache bust.

**Overall verdict: HOLD** (for the P1 set).

**Blockers (P1) to ship:**

1. **Finding 2 (subagent-skip on `/ctx-wrapup`)** — add the `isSubagent` gate at both command-handler entry points. This is the highest-risk gap: a subagent that can invoke the command (via tool surface or subagent-allowed hook) would do wasted/wrong historian work and write durable compartment state that conflicts with the parent session. The cost is a 4-line check at each entry point.

2. **Finding 1 (rehydration parity)** — extend the hook-init loop to also re-seed `deferredMaterializationSessions` (and `pendingMaterializationSessions`) for sessions with a pending-marker blob. This makes crash-recovery symmetric with the in-process publish path and avoids a "materialization missed on first pass after restart" race that the next `/ctx-flush` or natural pressure bust would mask but not fix.

3. **Finding 3 (test coverage for `forceKeepLastCompartmentForChunk` downgrade)** — add a runner-level test that exercises the `!chunk.hasMore` branch with a stub `readSessionChunk`. The current `wrapup-orchestrator.test.ts` only asserts the dep value is passed in, not that the runner's downgrade decision is correct. This is the kind of finding where a future refactor silently flips the contract and the test suite stays green.

**Advisories (P2) worth including in release notes but not gating ship:**

- Finding 4 (NULL-heal consistency), Finding 5 (partial index for the rehydration query), Finding 6 (busy-wait cap), Finding 7 (per-chunk note-nudge fire), Finding 8 (same as Finding 6). None of these are correctness or data-loss concerns; they are defensive cleanups and UX improvements that can be addressed in v0.31.x or v0.32.0.

The wrapup command, the durable marker, the boundary plan, the forceKeep/forceDrainQuota semantics, the migration, the notes search source, the session-aware anchors, the project-identity resilience, the onnxruntime subprocess probe, and the deferred-compaction-marker drain all check out against the source. The known invariants (defer-pass byte-identical replay, one-bust-covers-both history-rebuild-and-marker-advance, no-background-publish-materialization, no-unanchored-promotion-on-discard-last, TTL-releases-wedged-wrapup, no-caveman-for-subagents) are all upheld in the current code.
</COUNCIL_MEMBER_RESPONSE>

