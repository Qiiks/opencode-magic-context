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