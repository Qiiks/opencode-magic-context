# BUILD BRIEF: implement /ctx-wrapup per the v2 spec below

You are implementing a new Magic Context command in this repo (branch base: subc-migration HEAD). The spec below is council-reviewed and final — every council-tagged requirement (#1-#11) is load-bearing; do not simplify them away. Read the referenced modules before writing code. Where the spec cites approximate line numbers, re-locate by symbol.

Wiring checklist (per STRUCTURE.md conventions):
- Command definition in packages/plugin/src/features/builtin-commands/commands.ts; execution in packages/plugin/src/hooks/magic-context/command-handler.ts; the orchestrator itself as a new module packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts.
- Progress via the existing recomp RPC/TUI channel with a new kind; TUI rendering in packages/plugin/src/tui/ only if the existing progress renderer needs the new kind label.
- Pi command + orchestrator in packages/pi-plugin/src (command registration mirrors /ctx-recomp's pattern); PARITY.md entry for the toast-based progress divergence.
- The wrapup_in_progress marker: durable, session-scoped, TTL-stale-reclaimed — follow the existing lease patterns (BEGIN IMMEDIATE; see compartment-lease.ts). If a new column/table is needed, it is session-scoped state: prefer a session_meta scalar via storage-meta-persisted.ts over a new table; NO schema migration unless truly unavoidable (if unavoidable: new migration version, bump LATEST_SUPPORTED_VERSION, fresh-DB schema, ensureColumn, clearSession — per STRUCTURE.md).
- Docs: add the command to packages/docs commands page + root README command list if present. Keep wording per the spec's WORDING CONSTRAINT.
- Gates: plugin + pi-plugin bun test && typecheck, cli bun test, root bun run lint, check_comments. Commit with a clear message.

--- SPEC (v2, council-revised) ---

# /ctx-wrapup — forced historian drain of the live tail (v2, council-revised)

> v2 folds in the council findings (archive: .cortexkit/alfonso/athena/council-ctx-wrapup-spec-review-1ae92a8b2ee8d2c6). Verdict was REVISE with the reactive model-switch arm BLOCKED: the deliberate command ships v1; the reactive arm is a separate follow-up spec gated on this command's drain orchestrator existing. Council verified the three cache-safety self-nominations (multi-publish burst, boundary-floor leakage, growing-tail fingerprint) as BENIGN on OpenCode — no changes needed there.

## Problem

Two user-facing gaps, one engine:

1. **Large→small model switches still overflow.** Reported on Discord: switching a 553k-token session from a 1M-context model to a 272k model yields "Input exceeds context window" on the next message. The #188 unified-trigger fix arms recovery when `lastInputTokens > new model's trusted limit`, but the recovery it triggers is sized for incremental pressure, not for closing a ~280k gap: folding that much raw tail requires MULTIPLE sequential historian runs (each an LLM call over a bounded chunk), and the blocking arm waits for at most one. (Hypothesis to CONFIRM from a reporter's logs/`transform_decisions` before council treats it as fact — but the command below is justified independently.)
2. **No deliberate "compact now" control.** Users who KNOW they're about to switch models (or want to shrink a bloated session) have no way to pre-drain history. `/ctx-recomp` rebuilds existing compartments; nothing forces the historian forward over the live tail.

## Command contract

`/ctx-wrapup [messages_to_keep]`

- `messages_to_keep`: optional positive integer, default 20. The newest N meaningful messages stay raw; everything older gets compartmentalized.
- Runs the historian in a **blocking drain loop** over the eligible tail until coverage reaches the keep watermark.
- **Publishes deferred — never busts the cache itself.** Output on completion: "Wrapped up N messages into M compartments. The compacted history is queued and materializes on your next message." Then, only when NOT already pending a natural bust, append: "If you want it applied on the very next message, run /ctx-flush first." WORDING CONSTRAINT (council #8): never imply flush reduces context synchronously — flush only marks the next pass as busting; the reduction lands when the next message goes out. The model-switch case needs no flush at all (the switch's HARD fold materializes it for free) — say so in the docs page, not the command output.
- No-op guard: tail already within keep-N MEANINGFUL messages (same classifier as the watermark; council #9 — do not count structural noise) → "Nothing to wrap up — only N messages above the last compartment." No LLM run.
- Concurrency (council #2 — load-bearing): the loop runs under a WHOLE-LOOP guard, not per-iteration leases alone:
  - A durable cross-process `wrapup_in_progress` marker (session-scoped, TTL-stale-reclaimed like other leases) taken at loop start and released in a finally. Second /ctx-wrapup on the same session → rejected with current progress. /ctx-recomp and /ctx-session-upgrade must REFUSE while the marker is live (recomp DELETES compartments — interleaving corrupts state), and wrapup must refuse while a recomp is running (check the existing recomp-in-progress state).
  - Per-iteration, the existing historian DB lease + activeRuns machinery still serializes against the trigger-fired historian; if a trigger-fired run is in flight at loop start, await its completion (existing completion machinery), then proceed.
  - The ≥95% emergency arm firing mid-loop: the emergency path's own lease acquisition will block against the in-flight iteration; between iterations it could steal a run — that is ACCEPTABLE (it advances the same coverage the loop wants); the loop re-reads lastCompartmentEnd each iteration so a foreign publish just advances the loop.
- Failure honesty: chunks that published stay published (partial progress is real). On a chunk failure, stop, report coverage reached ("wrapped up through message X; run /ctx-wrapup again to continue"), and leave the standard historian failure notice/backoff machinery intact.

## Mechanics

### Drain loop (reuse, don't rebuild)
Loop the existing incremental pipeline (`compartment-runner-incremental.ts`) exactly as the emergency drain does, with one difference: the boundary given to each iteration comes from a **wrapup boundary override**, not `resolveProtectedTailBoundary`'s normal pressure math:

- Anchor: `lastCompartmentEnd + 1` (unchanged).
- Eligible end: the message just above the keep watermark (instead of the token-target / live-prompt-floor / 0.40×usable math).
- Chunking WITHIN the eligible range stays the runner's normal chunk budget (`trigger_budget`-derived) so per-run prompt sizes stay calibrated — the override widens the eligible range, not the per-run chunk.
- Open tool arcs at the cut: snap the watermark OUTWARD (keep more) so a live arc is never split — reuse the existing arc-fencing helpers from `protected-tail-boundary.ts` / `read-session-true-raw-tokens.ts`. Stale/interrupted arcs older than the watermark stay compactable (existing staleness rule).
- The keep watermark counts MEANINGFUL messages (the same meaningful-message classifier the live-prompt floor uses), newest-first; then snaps outward per the arc rule, and prefers a user-message boundary when one is within the snap distance.

### Final-chunk coverage: forceKeepLastCompartment (council #3 — NOT a blanket discard-last-off)
The runner's discard-last heuristic (don't persist the last compartment when lookahead is weak) contradicts wrapup's coverage contract — but `discardedLast` ALSO gates durable fact/event/primer promotion (compartment-runner-incremental.ts ~495-509, 632): promotion from a weak-lookahead boundary is exactly what the heuristic exists to prevent. So the final iteration passes a new `forceKeepLastCompartment` dep that (a) PERSISTS the final compartment for coverage, but (b) SKIPS its fact/event/primer/user-observation promotion (the compartment renders in history; nothing durable is extracted from it). Earlier iterations keep discard-last unchanged (their lookahead is the next chunk; re-reading the discarded head is designed behavior).

### Boundary staleness across the loop (council #5)
The runner's stale-snapshot self-heal (compartment-runner-incremental.ts ~252-274) re-resolves a stale boundary with NORMAL pressure math — which silently discards the wrapup override. Thread a `refreshBoundarySnapshot` callback into the runner deps: when the snapshot is stale, the runner calls it instead of re-resolving internally; wrapup's callback re-cuts from the CURRENT store state honoring the keep watermark (anchored at invocation). Normal trigger-fired runs keep today's behavior (callback absent → existing self-heal).

### Quota bypass (council #7)
`reserveProtectedTailDrainTokens` (~325-343) meters drain work by pressure window and would stop a low-pressure manual wrapup mid-loop despite the no-iteration-cap contract. The wrapup loop passes a forced-drain flag that bypasses the quota (same shape as the emergency_drain_active bypass). The no-progress circuit breaker (coverage unchanged after an iteration) remains the only loop terminator besides the watermark.

### Emergency-recovery flag (council #6)
`clearEmergencyRecovery` fires on every publish (~663-669). For the deliberate command this is benign today (recovery isn't armed), but guard it anyway: the loop must not clear an armed overflow-recovery flag before coverage reaches the watermark — gate the clear on "not inside a wrapup loop" (the wrapup_in_progress marker) so a concurrently-armed recovery survives until the drain actually finishes.

### Blocking + progress
- The incremental runner emits NO progress today (council #11) — progress comes from the WRAPUP ORCHESTRATOR, not the runner: emit a new `wrapup` progress kind (chunk i/j, message range, tokens) through the same RPC/TUI progress channel `/ctx-recomp` uses (recompProgressBySession or a sibling map; reuse the rendering plumbing, distinct kind label). Print the upfront estimate: eligible tokens, expected chunk count.
- Respect the historian model resolution + fallback chain as-is. Each iteration is a normal historian run (steps cap, timeout abort, failure notice) — wrapup adds ONLY the loop + boundary override.
- Budget: no hard cap on iterations by default (a 500k tail is the use case), but abort the loop if an iteration makes NO forward progress (coverage unchanged) to prevent infinite loops — that's the wrapup-level circuit breaker.

### Cache safety (the load-bearing part)
- Publishes ride the standard deferred-history-refresh path (historian publishes NEVER bust — existing invariant). Between wrapup and the next bust, every pass replays byte-identical.
- NO forced materialization, NO flush inside the command. The user message after wrapup materializes via the normal bust taxonomy: model switch → HARD (free, the intended flow); same-model next-execute/flush → SOFT/HARD per existing rules.
- The compaction-marker move stays deferred exactly as for normal publishes.
- Council should specifically attack: (a) does a multi-publish burst (say 8 publishes in one wrapup) violate any single-publish assumption in the deferred-refresh / marker-move / m1-revision plumbing? (b) protected-tail invariants that other consumers derive from the boundary — does bypassing the pressure math for the wrapup boundary leak anywhere shared state is written (e.g. does the runner persist boundary-derived state that later normal passes read)? (c) the trigger's fingerprint validation — wrapup iterations hand the runner explicit boundary snapshots; confirm the content-stable fingerprint contract holds across the loop when the tail is growing mid-wrapup (user keeps chatting — decide: block new turns? No: OpenCode can't; instead each iteration re-resolves from the CURRENT store state, and the keep watermark re-anchors to the tail as of command invocation, so a growing tail only ever ADDS protected messages).

### Reactive model-switch arm: BLOCKED, split out (council #1 — unanimous)
The reactive downswitch reuse is NOT part of this build. The only blocking primitive today (`awaitCompartmentRun`, transform-compartment-phase.ts ~224-262) awaits ONE run with a per-run 120s timeout then ships the prompt — it mechanically cannot close a multi-hundred-k gap, and extending it in place was rejected. After this command lands, a SEPARATE spec designs the reactive arm on top of the wrapup orchestrator (total-budget blocking drain, token-derived watermark from the new model's trusted limit, transform-path progress surfacing). Do not touch the #188 arm or awaitCompartmentRun in this build.

### Pi parity (council #4 + #10 — NOT naive loop-wrapping)
Pi ships the same command in this build, but via a proper multi-run primitive, not by wrapping the existing single-await path: Pi's current machinery has a 30s single-await cap (context-handler.ts ~2074), a single-slot inFlightHistorian map, and fire-and-forget spawnPiHistorianRun. Build a Pi wrapup orchestrator that (a) spawns runs SEQUENTIALLY, awaiting each subprocess to exit (no fire-and-forget), with a per-run budget matching the OpenCode historian budget (not 30s), (b) holds the same durable wrapup_in_progress marker (shared DB — the marker also mutually excludes an OpenCode process wrapping the same session), (c) re-reads lastCompartmentEnd per iteration. Council #10 (GLM): Pi's PENDING publication blob carries summary+tokens — the loop must drain/consume the pending blob between iterations the way Pi's normal pass does, or a later iteration clobbers an unconsumed one; verify against context-handler's deferred-publication consumption and make the orchestrator flush it per iteration. Progress: /ctx-status-style toast per chunk (no live sidebar rows on Pi; PARITY.md note).

## Config
None. No new knobs — the parameter is per-invocation. (The reactive-arm watermark math is derived, not configurable.)

## Tests
- Boundary override: keep-20 watermark computation over a synthetic session (meaningful-message counting, arc snap-outward, user-boundary preference).
- Loop: 3-chunk drain publishes 3× and stops at watermark; no-op guard; no-progress circuit breaker.
- forceKeepLastCompartment: final iteration persists to the edge AND skips promotion (assert no memory/event/primer rows from the final compartment); earlier iterations keep the heuristic.
- Concurrency: second wrapup rejected; recomp refused during wrapup and vice versa; trigger-fired publish between iterations advances the loop instead of breaking it.
- Quota bypass: a low-pressure session drains past where reserveProtectedTailDrainTokens would have stopped.
- Boundary refresh: stale snapshot mid-loop re-cuts honoring the watermark (not normal pressure math).
- Cache: E2E-style defer-replay test — wrapup publishes, next pass is defer → byte-identical; next execute materializes all new compartments in one bust. Model-key change after wrapup → single HARD fold containing all wrapup compartments.
- Mid-wrapup tail growth: messages appended during the loop stay raw (watermark anchored at invocation).
- Failure mid-loop: chunk 2 of 3 fails → chunks 0-1 published, coverage reported, re-run resumes from new lastCompartmentEnd.
- Pi mirrors of the loop/no-op/failure tests + the pending-blob-drain-per-iteration test.

## Out of scope (v1)
- The reactive model-switch arm (council-blocked; separate follow-up spec on top of this orchestrator).
- Auto-wrapup on idle or on a schedule.
- A wrapup that also drops tool outputs beyond what the historian chunk absorbs (the existing reclaim machinery keeps owning that).
- Any change to trigger math for normal (non-wrapup) passes.
