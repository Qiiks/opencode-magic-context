## Finding 1: Lack of Race-Free Guard for Concurrent Wrapup Commands
- **Severity**: blocker
- **Location**: `packages/plugin/src/hooks/magic-context/command-handler.ts` (where `/ctx-wrapup` will be registered)
- **Confidence**: high
- **Issue**: The spec states: *"if a wrapup is already running for the session, reject the second invocation."* However, there is no persistent or session-scoped flag in the codebase to track whether a wrapup is currently running. Because Node.js/Bun is single-threaded, the command handler can easily receive concurrent `/ctx-wrapup` invocations (e.g., double-clicks or rapid command entries). Without a race-free guard, multiple wrapup loops will run concurrently. Although the first run of the first loop will hold the lease, once it completes and releases the lease, the second loop's run can acquire it. This will cause the two wrapup loops to interleave their runs, leading to duplicate compartments, sequence number collisions, and database corruption.
- **Evidence**: In `packages/plugin/src/hooks/magic-context/compartment-runner.ts` (lines 145-150), the lease is released and the active run is deleted from `activeRuns` immediately upon completion of a single run. The wrapup loop runs multiple runs sequentially, meaning the lease is released and re-acquired between iterations, leaving a window for concurrent commands to interleave.
- **Suggested Fix**: Maintain an in-memory `activeWrapups = new Set<string>()` in the command handler. Reject the command immediately if the session ID is in the set, and ensure the session ID is added before the loop and removed in a `finally` block.

## Finding 2: Stale-Snapshot Re-resolution Fallback Bypasses Wrapup Boundary Override
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts` (lines 252-274)
- **Confidence**: high
- **Issue**: When a wrapup iteration runs, it hands an explicit boundary snapshot targeting the keep watermark. However, if the snapshot is validated and fails (e.g., because the user sent a message mid-wrapup, changing the raw message count), the runner's stale-snapshot fallback unconditionally calls `resolveOpenCodeProtectedTailBoundary` using the normal pressure math. This completely discards the wrapup's keep watermark and boundary override, reverting the run to normal incremental compaction and defeating the wrapup command's contract.
- **Evidence**: In `compartment-runner-incremental.ts` (lines 252-274), the stale-snapshot recovery path is hardcoded to call `resolveOpenCodeProtectedTailBoundary` with the default pressure parameters, with no way to preserve the wrapup boundary override.
- **Suggested Fix**: Add a `refreshBoundarySnapshot` callback to `CompartmentRunnerDeps` (similar to Pi's implementation in `spawnPiHistorianRun`) and use it in `runCompartmentAgent` to re-resolve the boundary snapshot using the wrapup-specific logic when it goes stale.

## Finding 3: Pi 30-Second Wait Cap Causes Guaranteed Provider 400 on Model Switch
- **Severity**: high
- **Location**: `packages/pi-plugin/src/context-handler.ts` (lines 2071-2080)
- **Confidence**: high
- **Issue**: Pi's emergency drain caps the wait for an in-flight historian at 30 seconds to avoid stalling the user's turn. However, for the reactive phase-2 switch (e.g., draining 280k tokens from a 553k session to fit a 272k model), the drain loop will require multiple sequential historian runs and will easily exceed 30 seconds. Because Pi cannot abort the outgoing request, timing out after 30 seconds will cause Pi to send the oversized prompt anyway, resulting in a guaranteed provider 400 (context overflow) and failing the reactive switch contract.
- **Evidence**: In `context-handler.ts` (lines 2071-2080), the wait is hard-capped at 30 seconds: `await withTimeout(histPromise, 30_000)`.
- **Suggested Fix**: For the reactive model-switch drain case, increase the timeout cap (e.g., to 5 minutes) or remove it entirely, prioritizing correctness (avoiding the provider 400) over the 30-second responsiveness limit.

## Finding 4: Discard-Last Disabled on Final Chunk Risks Boundary Corruption and Fact Double-Promotion
- **Severity**: medium
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts` (lines 495-509, 566, 632)
- **Confidence**: high
- **Issue**: The spec proposes disabling discard-last for the final iteration of the wrapup loop to ensure coverage reaches the keep watermark. However, the runner's discard-last heuristic exists to drop provisional compartments that lack lookahead (which makes their boundaries structurally unreliable). Disabling it means: (a) the final compartment will have a weak, structurally unreliable boundary, and (b) its facts, events, and primers will be promoted to durable project memory despite the weak boundary. This can lead to duplicate or poorly-extracted facts and corrupted boundaries that cannot be healed.
- **Evidence**: In `compartment-runner-incremental.ts` (lines 495-509), the discard-last logic drops the last compartment when lookahead is weak. Lines 566, 632, and 589 show that fact/event/primer promotion is skipped on discard-last runs to prevent double-promotion. Disabling discard-last forces these promotions to run on the weak-lookahead chunk.
- **Suggested Fix**: Keep discard-last enabled for all iterations, including the final one. The quality and consistency of the boundaries and project memories are more important than compacting the last 2-3 messages, which are already protected by the keep watermark anyway.

## Finding 5: Recomp Can Interleave and Corrupt Wrapup State
- **Severity**: medium
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner.ts` (lines 192-202)
- **Confidence**: high
- **Issue**: While a wrapup run is active, it is registered in `activeRuns` and holds the lease, preventing recomp from starting. However, between the wrapup loop's sequential runs, `activeRuns` is temporarily empty and the lease is released. If a user triggers `/ctx-recomp` (or an upgrade) during this brief window, recomp will acquire the lease and start a full rebuild. This will interleave with the wrapup loop, clearing the injection cache eagerly and corrupting the compaction marker and compartment sequence state.
- **Evidence**: In `compartment-runner.ts` (lines 192-202), recomp checks `activeRuns.has(sessionId)` and bails if true. But since wrapup releases the lease and deletes itself from `activeRuns` between iterations, recomp can acquire the lease in the gap.
- **Suggested Fix**: Use a session-scoped `wrapupInProgress` flag (or register a persistent placeholder promise in `activeRuns` for the duration of the entire wrapup loop) to prevent recomp or other runs from starting until the wrapup loop completes.

## Finding 6: Deferred Compaction Marker Move Prevents Immediate Context Reduction
- **Severity**: medium
- **Location**: `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts` (lines 1179-1181)
- **Confidence**: high
- **Issue**: The spec states that compaction-marker moves stay deferred. However, the deferred marker is only applied in the postprocess phase when `historyWasConsumedThisPass` is true (which requires a cache-busting pass or forced materialization). If the user runs `/ctx-wrapup` and then sends a normal message that does not bust the cache, `historyWasConsumedThisPass` will be false. The compaction marker will not be moved, meaning OpenCode will not filter out the compacted messages, and the next prompt sent to the provider will still contain the full uncompacted tail, defeating the purpose of the wrapup.
- **Evidence**: In `transform-postprocess-phase.ts` (lines 1179-1181), the deferred marker is only applied if `historyWasConsumedThisPass` is true.
- **Suggested Fix**: Apply the compaction marker immediately in the command handler (or force `historyWasConsumedThisPass` to be true on the next pass) so the context reduction takes effect on the very next message.

## Summary
- **Blocker**: 1
- **High**: 2
- **Medium**: 3
- **Low**: 0

**Overall Risk Assessment**: High risk of database corruption and functional failure due to concurrent command execution, stale-snapshot re-resolution fallback, and Pi timeout limitations.

**Verdict**: **REVISE**
The design is conceptually sound but requires revision to address the concurrency race condition (Finding 1), the stale-snapshot re-resolution fallback (Finding 2), and the Pi timeout limitation (Finding 3) before it can be safely shipped.