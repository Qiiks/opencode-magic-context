# Council Synthesis — Magic Context v0.31.0 Release-Readiness Audit

**Question**: Blind multi-model adversarial audit of the v0.31.0 delta (`git diff v0.30.7..HEAD`, ~119 files) of the Magic Context OpenCode/Pi plugins. Focus: cross-feature interactions, multi-process interleavings, restart/crash recovery, migration/config edges. Per-finding severity + per-member SHIP/HOLD verdict.

**Members consulted (6 valid)**: Opus 4.8, GPT 5.4 high, GPT 5.5 xhigh, XAI Composer 2.5, Ollama Minimax M3, Gemini Flash 3.5 high
**Members failed (1)**: Ollama GLM 5.2 — completed at 62m but fell into a degenerate repetition loop ("Let me now compile my final findings…" ×hundreds) and never emitted a structured response. Excluded.

## Verdict tally

| Member | Verdict | Sharpest finding |
|---|---|---|
| Opus 4.8 | **SHIP (conditional)** | Only P2s: Pi caveman gate is architectural-only; renewal timers can throw |
| GPT 5.4 high | **HOLD** | **P0**: Pi restart rehydration forces materialization (pending vs deferred) |
| GPT 5.5 xhigh | **HOLD (soft)** | P1: expired wrapup marker not reclaimed inside nested txn |
| XAI Composer 2.5 | **HOLD** | 4× P1: m0 cache identity invalidation, LKG identity keyed by cwd, TOCTOU, timer throw |
| Ollama Minimax M3 | **HOLD** | 3× P1: no subagent gate on /ctx-wrapup, rehydration parity, missing test |
| Gemini Flash 3.5 | **HOLD** | **critical/P0**: Pi advances compaction marker under m0 contention |

**5 HOLD / 1 SHIP-conditional.** No two members independently confirmed the *same* P0, but two members raised *different* source-verified P0-class breaches in the Pi harness, and multiple P1s cluster tightly. **Consolidated council recommendation: HOLD** pending resolution or explicit waiver of the Pi-harness materialization/marker-advance divergences and the subagent-gate gap.

---

## Findings

### #1: Pi restart rehydration forces materialization instead of deferred consumption
- **Severity**: High (P0 per GPT 5.4; P2 "documented recovery" per XAI/GPT 5.5 — contested)
- **Confidence**: Majority (3 members: GPT 5.4, XAI Composer, GPT 5.5) — **Athena source-verified**
- **Members Reported**: GPT 5.4 high (F1, P0), XAI Composer 2.5 (F3, P2), GPT 5.5 xhigh (F3, P2)
- **Issue**: Live wrapup publish signals `signalPiDeferredHistoryRefresh` + `signalPiDeferred**Materialization**`, which routes through the mid-turn-aware `canConsumeDeferredLate` gate. But **restart rehydration** signals `signalPiDeferredHistoryRefresh` + `signalPiPending**Materialization**`, which feeds `baseShouldApplyPendingOps` *directly* and bypasses that gate — an "explicit_flush"-strength bust for what was a manual/background publish.
- **Evidence** (Athena-verified in source):
  - Live wrapup: `pi-plugin/src/commands/ctx-wrapup.ts:382-383` → `signalPiDeferredMaterialization`.
  - Restart: `pi-plugin/src/index.ts:535-536` → `signalPiPendingMaterialization`.
  - `context-handler.ts:3705` `hasPendingMaterializeSignal` → `:3720-3724` `baseShouldApplyPendingOps` (no gate) vs `:3732-3733` `deferredMaterialize = canConsumeDeferredLate && …` (gated). `:3739-3740` labels the forced path `explicit_flush`.
- **Impact**: Touches the ship-blocker "background/manual publishes never force a materialization." A restart-rehydrated wrapup remnant can force a cache-busting materialization on the next defer pass rather than riding the next natural bust. Counter-view (XAI, GPT 5.5): this is intentional *wedged-marker recovery* (AUDIT-KNOWN-ISSUES A6), only fires post-restart, and is arguably the correct way to unstick a marker left by a crash. The disagreement is about *intent*, not *mechanism* — the mechanism is confirmed.
- **Fix Direction**: Rehydrate with `signalPiDeferredMaterialization` (gated), OR introduce a restart-only deferred signal that still requires `canConsumeDeferredLate`/natural-bust. If the current pending-materialization behavior is intended stuck-marker recovery, add an explicit code comment + test asserting it, and confirm it cannot fire mid-turn.

### #2: Pi advances the compaction marker even when m[0] materialization fell back to cached replay under contention
- **Severity**: High (critical/P0 per Gemini) — **Athena source-verified as a real OpenCode/Pi asymmetry**
- **Confidence**: Solo (1 member: Gemini) — but independently verified by Athena and consistent with the Pi-materialization theme
- **Members Reported**: Gemini Flash 3.5 high (F1, critical)
- **Issue**: The Pi deferred-history/marker drain gates on `materializationSatisfiedThisPass`, which is set `true` whenever pending-ops apply — *regardless of whether `injectM0M1Pi` actually re-materialized m[0]*. If injection hits `PiMaterializeContentionError` and replays the cached m[0]/m[1] pair (which lacks the new compartment), the drain still fires and `appendCompaction` advances the marker past history not present in the rendered prompt.
- **Evidence** (Athena-verified): `context-handler.ts:3775` sets `materializationSatisfiedThisPass = true` right after `applyPendingOperations` success. `:4369-4374` gates `deferredHistoryDrainEligible` on that flag, then `:4390-4398 → appendCompaction` advances the marker. OpenCode gates the equivalent drain on `m0RematerializedThisPass` (the actual `injectM0M1` result), not on pending-ops success.
- **Impact**: Directly touches the ship-blocker "one bust must cover BOTH a history rebuild AND its compaction-marker advance." Under DB contention (the exact multi-process interleaving the audit targets), the marker can advance past un-injected history → later replay drops/duplicates that span.
- **Fix Direction**: Set the drain-eligibility flag from the *actual* `injectM0M1Pi` outcome (`injectionResult?.m0Materialized === true`), mirroring OpenCode's `m0RematerializedThisPass`. This is the single highest-value verification for release: confirm whether the drain can fire on a contention-fallback pass. If `historyWasConsumedThisPass` is already false on a contention fallback, this collapses to a no-op — **release owner must confirm this on the contention path specifically.**

### #3: `/ctx-wrapup` has no subagent-skip gate
- **Severity**: P1
- **Confidence**: Minority (2 members: Minimax explicit P1; Opus F1 raises the parallel Pi caveman-gate gap as P2) — **Athena-plausible, not fully verified**
- **Members Reported**: Ollama Minimax M3 (F2, P1), Opus 4.8 (F1, P2 — adjacent: Pi caveman subagent exclusion is architectural-only)
- **Issue**: Neither wrapup entry point (`command-handler.ts:598-610` OpenCode, `commands/ctx-wrapup.ts:146-156` Pi) checks `sessionMeta.isSubagent` before invoking the historian, whereas the sibling paths (`recomp-orchestrator.ts:350`, `system-prompt-hash.ts:305-306`) do gate on subagent/wrapup state. A subagent that reaches the command would run the historian and write durable compartment state that can conflict with the parent session — and Opus notes the Pi caveman path shares the "single architectural assumption away from violating a stated ship-blocker" property.
- **Impact**: Bounded by whether a subagent can actually reach the command surface today (likely no — hence not P0). If reachable, wasted/wrong historian work + durable-state conflict. Defense-in-depth against the "subagents never get caveman / never do historian work" invariants.
- **Fix Direction**: At both entry points, load `getOrCreateSessionMeta` and bail if `meta.isSubagent === true`. Add an explicit `isSubagent` short-circuit on the Pi caveman branch (Opus F1) to match OpenCode's `!reducedMode` gate at `transform.ts:1761`.

### #4: Expired `wrapup_in_progress` blob not reclaimed when read inside an outer SQLite transaction
- **Severity**: P1
- **Confidence**: Solo (1 member: GPT 5.5) — high-confidence, source-cited
- **Members Reported**: GPT 5.5 xhigh (F1)
- **Issue**: `getWrapupInProgressState` returns `null` for an expired marker but, if `BEGIN IMMEDIATE` fails because the caller is already in a write txn, it does **not** NULL the stale JSON. `isWrapupInProgress` shares this path. Trigger-fired historian gating on `isWrapupInProgress` can therefore resume while the durable row still holds a stale blob until a later standalone read reclaims it.
- **Evidence**: `storage-meta-persisted.ts:434-469` (comment at 445-447 documents returning null without cleanup; reclaim only on `BEGIN IMMEDIATE` success at 452-457). Gated consumers: `compartment-runner.ts:112-118`, `pi-plugin/context-handler.ts:2989-2997`.
- **Impact**: Weakens the wrapup-vs-trigger-historian mutual-exclusion story in multi-statement write paths and across processes (a new `/ctx-wrapup` can still acquire on `expiresAt > now`). Degraded, not a proven data-loss P0.
- **Fix Direction**: Reclaim expired state via savepoint/nested txn, or schedule async reclaim; treat "expired but row present" as inactive for triggers while still attempting best-effort NULL even with an outer txn open.

### #5: Crash-recovery rehydration restores only `deferredHistoryRefreshSessions`, not `deferredMaterializationSessions` (OpenCode)
- **Severity**: P1 (Minimax) / P2 suspicion (GPT 5.5)
- **Confidence**: Minority (2 members) — source-cited
- **Members Reported**: Ollama Minimax M3 (F1, P1), GPT 5.5 xhigh (F2, P2 suspicion)
- **Issue**: The wrapup `onCompartmentStatePublished` callback adds the session to BOTH `deferredHistoryRefreshSessions` and `deferredMaterializationSessions`, but hook-init rehydration re-seeds only the history set from `getSessionsWithPendingMarker`. After a crash, the materialization set is empty, so the drop-ops materialization path skips the first pass (marker drain still fires — degraded, not wedged).
- **Evidence**: `hook.ts:250-262` (one set), `wrapup-orchestrator.ts:198-205` (both sets), `transform-postprocess-phase.ts:1247-1252` (asymmetric consume).
- **Impact**: A crashed wrapup is not *transparently* resumed on OpenCode — first pass after restart misses materialization until the next `/ctx-flush` or natural pressure bust. Violates the intent's symmetry, not a hard invariant.
- **Fix Direction**: Extend the rehydration loop to also add pending-marker sessions to `deferredMaterializationSessions` (and `pendingMaterializationSessions` for safety). Note: this is the OpenCode-side mirror of Finding #1's Pi-side concern — resolve the two together to keep the harnesses symmetric.

### #6: Wrapup marker/lease renewal timers can throw uncaught from `setInterval` callbacks
- **Severity**: P2 (Opus) / P1 (XAI) — contested
- **Confidence**: Minority (2 members) — high on mechanism, medium on crash impact
- **Members Reported**: Opus 4.8 (F2, P2), XAI Composer 2.5 (F4, P1)
- **Issue**: The 60s renewal timers call `updateWrapupInProgress` (which does an unguarded `BEGIN IMMEDIATE`) with no try/catch. On `SQLITE_BUSY` outlasting `busy_timeout` — plausible under the two-instance / OpenCode+Pi shared-`context.db` scenario — the timer throws, which in an embedded plugin can destabilize the host and undercuts the "crashed wrapup self-heals via TTL" story (intended failure mode is silent TTL expiry, not a timer-thrown exception).
- **Evidence**: OpenCode `wrapup-orchestrator.ts:285-290` (marker), `:167-171` (lease); Pi `commands/ctx-wrapup.ts:235-239`, `:331-333`; write path `storage-meta-persisted.ts:524` (`BEGIN IMMEDIATE` outside try).
- **Fix Direction**: Wrap renewal/release bodies in try/catch; treat transient BUSY as a no-op renewal (TTL covers the gap); distinguish ownership-loss (`null`) from transient errors; never let a renewal timer throw.

### #7: OpenCode m[0]/m[1] cache does not invalidate on project-identity change
- **Severity**: P1
- **Confidence**: Solo (1 member: XAI) — high, with a Pi-parity anchor
- **Members Reported**: XAI Composer 2.5 (F1)
- **Issue**: OpenCode resolves a per-pass `projectIdentity` but the cached m[0] snapshot neither stores nor compares it. A session that materialized under a cold `dir:` fallback and later resolves to `git:…` can keep serving the old project-memory baseline until an unrelated hard bust. **Pi already has this guard** (`inject-compartments-pi.ts:926-934` returns `project_change`); OpenCode omits it.
- **Evidence**: `inject-compartments.ts:629-648` (`M0M1State` lacks `cachedM0ProjectIdentity`), `:1026-1116` (`mustMaterialize` compares model/system/TTL/epoch/mutation/upgrade but not identity), `:1713-1729` (materialize omits `projectIdentity` though `storage-meta-shared.ts:460-474` supports it).
- **Impact**: Directly interacts with the new project-identity-resilience feature: the `dir:`→`git:` recovery this release adds can leave OpenCode serving stale project memory. Cross-feature (identity-reuse × m0 cache).
- **Fix Direction**: Thread `projectIdentity` through OpenCode `M0SnapshotMarkers`/`M0M1State`, persist it, hard-fold on mismatch; treat legacy null as "unknown" for one lazy adoption, matching Pi.

### #8: Last-known-good git identity reuse is keyed by exact cwd, not repo root
- **Severity**: P1
- **Confidence**: Solo (1 member: XAI) — high
- **Members Reported**: XAI Composer 2.5 (F2)
- **Issue**: LKG identity caches are keyed by `path.resolve(directory)`. During transient git failure, resolving `/repo/subdir` looks up only `/repo/subdir` (not `/repo`) and falls back to `dir:`, splitting one live repo into distinct `dir:` identities when cwd moves within it.
- **Evidence**: `memory/project-identity.ts:262-304` (cwd-keyed caches), `:384-407` (`reuseLastKnownGitIdentity(canonical)` exact-dir only; no ancestor/root walk despite `hasGitDir` walking ancestors).
- **Impact**: The mid-session identity-flip prevention this release advertises can still flip within a repo on cwd change under transient failure → memory/embedding split. Cross-feature (identity-reuse × workspace-fingerprints).
- **Fix Direction**: Key LKG reuse by resolved git root/worktree gitdir; or when `hasGitDir(cwd)` is true, walk ancestor/realpath cache entries before returning `dir:`.

### #9: Cross-process TOCTOU between `/ctx-wrapup` marker and trigger-fired historian lease
- **Severity**: P1
- **Confidence**: Solo (1 member: XAI) — medium
- **Members Reported**: XAI Composer 2.5 (F3)
- **Issue**: Trigger-fired historian checks `isWrapupInProgress` *then* acquires the compartment lease, but lease acquisition itself doesn't check the wrapup marker. A peer process can pass the marker check before wrapup commits its marker, then acquire the lease after the marker exists and publish during a manual wrapup window.
- **Evidence**: OpenCode `compartment-runner.ts:112-118` (marker check) then `:121-122` (lease); `compartment-lease.ts:13-31` (SQL arbitrates only `compartment_state_lease`). Pi same split: `context-handler.ts:2989-2997` then `:2841-2843`. Compounds with Finding #4 (stale marker false-negative).
- **Fix Direction**: Make lease acquisition atomically fail on an unexpired wrapup marker, or re-check the marker immediately after acquiring the lease and abort/release before any historian work.

### #10: Pi auto-search permanently caches retryable failures as "no hint"
- **Severity**: P1
- **Confidence**: Solo (1 member: GPT 5.4) — high, with an OpenCode contrast + test
- **Members Reported**: GPT 5.4 high (F3)
- **Issue**: In Pi, a transient auto-search timeout/error is persisted as a permanent `no-hint` decision; later passes short-circuit and never retry, suppressing hinting for the whole turn. OpenCode treats timeout/error as retryable and does not persist them (with a test asserting the retry).
- **Evidence**: Pi `auto-search-pi.ts:312-320` (replay+exit), `:402-416` (persists `error`/`timeout`). OpenCode `auto-search-runner.ts:349-365` + `auto-search-runner.test.ts:170-181`.
- **Impact**: Cross-feature (notes-search × auto-search-hints): a brief embedding/runtime blip degrades the new notes-inclusive auto-search for the rest of the turn on Pi only.
- **Fix Direction**: Match OpenCode — persist only stable outcomes (`empty`, `below-threshold`, `too-short`, `stacked`); leave timeout/error unpersisted/retryable.

### #11: Foreign publishes are invisible to already-running peer processes
- **Severity**: P1 (conditional on multi-instance being supported)
- **Confidence**: Solo (1 member: GPT 5.4) — high on mechanism
- **Members Reported**: GPT 5.4 high (F2)
- **Issue**: Deferred history/marker consumption is rehydrated from durable state only at process startup. If process A publishes while process B is already running on the same `context.db`, B never sees the foreign deferred signal and can serve stale history/marker until restart or an unrelated local bust.
- **Evidence**: publish adds only to in-memory sets (`transform.ts:1049-1052`, `1648-1650`); consumers read only local `Set.has` (`transform.ts:916-930`, `context-handler.ts:3444-3445`, `4369-4383`); boot-only rehydration (`hook.ts:243-255`, `index.ts:533-537`).
- **Impact**: Only matters if two live instances sharing one `context.db` is a supported v0.31.0 scenario — the release owner must decide. If unsupported, downgrade to a documented limitation.
- **Fix Direction**: Make persisted pending-marker state a pass-start trigger so any running process can notice + safely consume a foreign publish.

### #12: `forceKeepLastCompartmentForChunk` downgrade has no dedicated test
- **Severity**: P1 (test-coverage)
- **Confidence**: Solo (1 member: Minimax) — high
- **Members Reported**: Ollama Minimax M3 (F3)
- **Issue**: The runner computes `forceKeepLastCompartmentForChunk = forceKeepLastCompartment && !chunk.hasMore` and two consumer gates flip on it, but the only test (`wrapup-orchestrator.test.ts:110-126`) asserts the *dep value passed in* (`[true,true,true]`), not the runner's `!chunk.hasMore` downgrade. A refactor flipping `&&`→`||` passes the suite while breaking the documented contract. Pi mirror (`pi-historian-runner.ts:460`) also untested.
- **Evidence**: `compartment-runner-incremental.ts:352-355` (downgrade), consumers `:505`/`:579`; `wrapup-orchestrator.test.ts:108-126`.
- **Fix Direction**: Add a runner-level test with `chunk.hasMore=true` for intermediates and `false` for the last, asserting discard-last fires for intermediates and is suppressed only on the final chunk. Mirror in Pi.

### #13: Pi/OpenCode prompt divergence on history-refresh-only passes (todo injection)
- **Severity**: High (Gemini) — **suspicion, needs confirmation**
- **Confidence**: Solo (1 member: Gemini) — NOT independently Athena-verified
- **Members Reported**: Gemini Flash 3.5 high (F2)
- **Issue**: On a history-refresh-only pass, OpenCode gates fresh-todo injection on `isCacheBustingPass = shouldApplyPendingOps || shouldRunHeuristics` (false → replays persisted anchor), while Pi uses `isCacheBustingForTodo = isCacheBusting || result.executedWorkThisPass` (true → injects fresh todo + updates anchor). Claimed to cause prompt-cache misses / cross-harness divergence.
- **Impact**: If real, touches "defer passes must replay byte-identical." But Gemini's line refs (`context-handler.ts:2380`, `transform-postprocess-phase.ts:1056`) were not re-verified by Athena, and this conflicts with GPT 5.5/Opus's finding that the byte-identity invariant holds. **Treat as a suspicion to confirm, not a blocker on its own.**
- **Fix Direction**: If confirmed, align Pi's todo-injection gate to OpenCode's `isCacheBustingPass` (i.e. drop the `isCacheBusting` disjunct, keep `executedWorkThisPass`).

### #14 (P2 cluster): defensive / UX cleanups
- **Severity**: P2
- **Members Reported**: Opus 4.8, Minimax M3, GPT 5.5, XAI, Gemini
- Items: `ctx_search` surfaces `dismissed` notes (Opus F3 — product-intent call); `wrapup_in_progress_state` absent from `healNullTextColumns` (Minimax F4 — consistency, no current bug); no partial index on `pending_compaction_marker_state` → full `session_meta` scan on every startup (Minimax F5 — perf for large histories); wrapup busy-waits up to 5 min on foreign lease with no cap (Minimax F6/F8 — UX); per-chunk `onNoteTrigger("historian_complete")` re-arms note-nudge (Minimax F7 — misleading persistence, benign); git cooldown ignores an immediate manual `safe.directory` fix for 5 min (Gemini F3 — UX).

---

## Convergence & disagreement

**Unanimously verified CLEAN** (multiple members independently tried to break and could not): provisional ctx_reduce-verdict gate withholds the system-prompt hash baseline until frozen (`system-prompt-hash.ts:383`); removed `ctx_reduce_enabled` handled by Zod strip + idempotent migration v50; `forceKeepLastCompartment` downgrade + unanchored-promotion skip logic; crashed-wrapup TTL self-heal (5 min, 60s renew, ownership-loss abort, `finally`-release); notes-as-fifth-source with session-scoped `@msg` anchors correctly gated on `sourceSessionId === currentSessionId`; auto-search deliberately excludes `note`/`primer`; OpenCode caveman subagent exclusion (`transform.ts:1761` `!reducedMode`); OpenCode `pendingMarkerCoveredByConsumedBoundary` deferred-marker gate.

**Genuine disagreement**: 
- The **Pi restart→materialization** behavior (#1) — GPT 5.4 reads it as a P0 invariant breach; XAI/GPT 5.5 read the same source as *intentional documented stuck-marker recovery* (AUDIT-KNOWN-ISSUES A6). Athena confirmed the mechanism (gate bypass is real) but the severity turns on undocumented intent.
- **Renewal-timer throw** (#6) — Opus P2 (won't hard-crash, just logs) vs XAI P1 (can destabilize embedded host).
- Overall verdict: Opus alone reads the residual as all-P2 → SHIP-conditional; the other five see enough P1-and-up in the Pi-harness / multi-process seams to HOLD.

**The two P0-class claims (#1, #2) are distinct mechanisms in the same subsystem** (Pi deferred-materialization / marker-advance), raised by *different* models, both source-verified by Athena. That convergence-by-theme — even without vote overlap on a single line — is the strongest signal in this audit and the core of the HOLD.

---

## Priority recommendations

**Must resolve or explicitly waive before tagging v0.31.0 (release owner decision required):**
1. **#2** — Confirm on the *m[0]-contention-fallback path* whether the Pi compaction-marker drain can advance past un-injected history (`materializationSatisfiedThisPass` vs OpenCode's `m0RematerializedThisPass`). This is the sharpest ship-blocker-invariant risk. If `historyWasConsumedThisPass` is already false on contention fallback, it's a no-op — verify and document.
2. **#1** — Decide whether Pi restart rehydration's `signalPiPendingMaterialization` (gate-bypassing) is intended recovery or a breach of "manual publishes never force materialization." Either switch to the gated deferred signal or add an intent-asserting test + comment. Resolve jointly with **#5** (the OpenCode-side rehydration asymmetry) to keep harnesses symmetric.

**Should-fix (P1) — batch for v0.31.0 if the window allows, else fast-follow:**
3. **#3** subagent-skip gate on `/ctx-wrapup` (+ Opus's Pi caveman explicit gate) — cheap defense-in-depth on stated invariants.
4. **#4** expired-marker reclaim inside nested txn (+ **#9** TOCTOU) — together they harden wrapup↔trigger-historian mutual exclusion across processes.
5. **#7 / #8** OpenCode m0-identity invalidation + LKG-keyed-by-cwd — these are the cross-feature interactions with the *new* identity-resilience feature and should not ship unguarded when Pi already guards #7.
6. **#10** Pi auto-search retryable-failure persistence; **#6** renewal-timer try/catch; **#12** add the downgrade test.

**Investigate:** **#13** (Pi/OpenCode todo-injection divergence) — confirm against source before acting; conflicts with the byte-identity "clean" verdict.

**Release-notes / fast-follow (P2):** #14 cluster.

## Dismissed / withdrawn
- **GPT 5.5 F4** (dubious-ownership returns `dir:` despite LKG existing) — the member re-read source mid-analysis and correctly withdrew it: `project-identity.ts:405-408` returns cached git identity *before* the dubious-ownership `dir:` fallback. Not a bug.
- **Ollama GLM 5.2** — excluded entirely (degenerate output, no structured findings).

## Confidence
**Medium-High.** Six models, broad static coverage, and the two invariant-critical findings (#1, #2) were re-verified by Athena directly in source. The residual uncertainty is severity, not existence: whether #1/#2 fire on the real contention/restart paths (vs. being no-ops guarded by an upstream condition) requires the release owner to trace the contention/restart execution once more — ideally with an integration test — before over- or under-calling them. No member executed the suites or reproduced a multi-process race, so the multi-process findings (#2, #6, #9, #11) are mechanism-verified but not runtime-reproduced.
