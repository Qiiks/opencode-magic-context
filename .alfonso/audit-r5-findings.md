# Rust ↔ TypeScript Transform Parity Audit — Round 5

Audit base: `101efbd34e06a7f0ec6082dd177dd59e31437466`

Contract: the shipped TypeScript behavior is the specification. Rust behavior that differs is reported even when the Rust behavior appears preferable.

## Findings

### R5-01 — P0 — Compaction-off rewrites frozen m0 on additive changes

**TypeScript (spec):**

- `packages/plugin/src/hooks/magic-context/inject-compartments.ts:1488-1502` defines new memories and user-profile entries as m1 deltas, not m0/HARD triggers.
- `packages/plugin/src/hooks/magic-context/inject-compartments.ts:1617-1625` explicitly excludes `maxMemoryId` and project-doc changes from HARD materialization.
- `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1421-1444` routes compaction-off through the ordinary frozen m0/m1 injection discipline while suppressing historical compartments.

**Rust:**

- `crates/mc-module/src/transform.rs:2464-2487` recomposes the complete additive m0 from the live store on every request and sets `materialized` whenever those bytes differ from the stored m0.
- `crates/mc-module/src/transform.rs:2491-2504` turns that byte difference into a HARD rewrite and resets m1 to the placeholder.

**Divergence / executable check:** Bootstrap the same compaction-off session with memory A, then add memory B (or edit project docs/user profile) without changing model, system prompt, TTL state, request messages, or render config. The next TypeScript defer replays the old m0/m1 prefix; B waits for an independently authorized m1 refresh/HARD fold. Rust immediately emits a different m0 and reports `HARD`. This is an unauthorized provider-prefix rewrite on a pass that TypeScript keeps cache-stable.

**Spec side:** TypeScript.

### R5-02 — P1 — Compaction-off Rust drops the enabled mural surface

**TypeScript (spec):**

- `packages/plugin/src/hooks/magic-context/inject-compartments.ts:2132-2138` empties only the compartment input in compaction-off mode.
- `packages/plugin/src/hooks/magic-context/inject-compartments.ts:2191-2210` still resolves and renders the capability-gated mural into m0.
- `packages/plugin/src/hooks/magic-context/inject-compartments.ts:2231-2237` freezes the mural payload/hash for replay.

**Rust:**

- `crates/mc-module/src/transform.rs:2384-2401` composes additive m0 with docs, profile, memories, and no mural input.
- `crates/mc-module/src/transform.rs:2497-2500` stores only m0 text plus the m1 placeholder; the request's `mural` is never consumed by `apply_additive_only`.

**Divergence / executable check:** Enable compaction-off and supply the same valid OpenCode mural (`enabled`, vision-capable, data URL, content hash). TypeScript prepends the mural text/image and freezes its hash. Rust serves no mural. The rest of the raw history remains additive in both lanes.

**Spec side:** TypeScript.

### R5-03 — P1 — Rust wrapup stops after five chunks; TypeScript drains to the watermark

**TypeScript (spec):**

- `packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts:400-492` loops until the fixed target watermark is reached or a real failure/no-progress condition occurs; there is no round-count cap.
- `packages/pi-plugin/src/commands/ctx-wrapup.ts:307-462` has the same drain-until-target behavior.

**Rust:**

- `crates/mc-module/src/historian.rs:953-966` fixes `MAX_WRAPUP_ROUNDS` at 5.
- `crates/mc-module/src/lib.rs:6103-6109` bounds the producer loop by that cap.
- `crates/mc-module/src/lib.rs:6353-6359` returns a retryable partial result when history remains after five rounds.

**Divergence / executable check:** Use a backlog requiring six historian chunks with every producer round succeeding and advancing. One TypeScript `/ctx-wrapup` reaches the keep watermark. One Rust `session.wrapup` stops after five and requires another user command.

**Spec side:** TypeScript.

### R5-04 — P1 — Rust-mode `/ctx-wrapup` changes the requested keep watermark

**TypeScript (spec):**

- `packages/plugin/src/hooks/magic-context/command-handler.ts:110-126` accepts every positive safe integer and imposes no 5/100 limits.
- `packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts:263-269` applies only a floor of 1.

**Rust:**

- `packages/plugin/src/hooks/magic-context/command-handler.ts:808-832` clamps the Rust-mode request to `[5, 100]` before transport.
- `crates/mc-module/src/lib.rs:479-480` and `crates/mc-module/src/lib.rs:5947-5961` enforce the same clamp again.

**Divergence / executable check:** Run `/ctx-wrapup 1` and `/ctx-wrapup 250` against otherwise identical histories. TypeScript keeps exactly 1 or 250 raw messages (subject only to safety snapping). Rust keeps 5 or 100. Existing Rust-mode adapter coverage at `packages/plugin/src/hooks/magic-context/command-handler.test.ts:850-895` explicitly asserts the 250 → 100 behavior.

**Spec side:** TypeScript.

### R5-05 — P1 — Rust wrapup user-boundary snapping ignores session geometry

**TypeScript (spec):**

- `packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts:103-121` passes the session's context limit and effective execute threshold into the wrapup boundary resolver.
- `packages/plugin/src/hooks/magic-context/protected-tail-boundary.ts:784-790` snaps with the resulting `ctx.triggerBudget`.
- `packages/plugin/src/hooks/magic-context/protected-tail-boundary.ts:290-307` clamps that real trigger budget only to the 2k–48k snap window.

**Rust:**

- `crates/mc-module/src/boundary.rs:610-617` derives wrapup snapping from hard-coded `128_000, 65.0`, regardless of request geometry or effective threshold.
- `crates/mc-module/src/boundary.rs:1378-1404` then applies that synthetic budget to the user snap.

**Divergence / executable check:** On a 1M-token model at threshold 65, place a meaningful user message 10k tokens before the raw keep candidate. TypeScript's trigger budget is about 32.5k, so it snaps to the user message. Rust's hard-coded budget bottoms at 5k, so it retains the later candidate. Tool-arc fencing is identical; the protected-tail start is not.

**Spec side:** TypeScript.

### R5-06 — P1 — OpenCode TypeScript wrapup suppresses user-memory observations that Rust persists

**TypeScript (spec):**

- `packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts:218-241` forwards memory and auto-promotion controls but omits `experimentalUserMemories` when invoking the compartment runner.
- `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:830-840` persists user observations only when `deps.experimentalUserMemories === true`, so OpenCode wrapup suppresses them on every chunk.

**Rust:**

- `crates/mc-module/src/lib.rs:4595-4618` forwards `cfg.user_memory_collection_enabled` into every wrapup firing.
- `crates/mc-module/src/historian_validate.rs:613-624` retains observations on non-final chunks, and `crates/mc-module/src/historian.rs:1617-1635` publishes those candidates when the forwarded config is enabled.

**Divergence / executable check:** Enable user-memory collection and run a multi-chunk wrapup whose first (non-final) chunk emits one user observation. OpenCode TypeScript stores no candidate; Rust stores it. The Pi TypeScript wrapup does forward its equivalent at `packages/pi-plugin/src/commands/ctx-wrapup.ts:425-444`, so this is specifically a shipped OpenCode-TS/Rust divergence, not a final-chunk promotion-skip difference.

**Spec side:** OpenCode TypeScript.

### R5-07 — P2 — Retryable Rust wrapup is presented as terminal “Failed,” not TypeScript “Partial”

**TypeScript (spec):**

- `packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts:499-509` labels a progressed-but-incomplete run `Magic Wrapup — Partial` and instructs the user to continue.

**Rust:**

- `crates/mc-module/src/lib.rs:5744-5753` returns the machine disposition `retryable` for transient/incomplete outcomes.
- `packages/plugin/src/hooks/magic-context/command-handler.ts:211-228` recognizes only `completed`, `nothing_to_compact`, and `already_in_progress`; `retryable` falls through to `Magic Wrapup — Failed`.

**Divergence / executable check:** Force no forward progress after one successful chunk. TypeScript presents a partial result. Rust returns `retryable`, but the OpenCode adapter presents a failed result even though retry is the prescribed continuation.

**Spec side:** TypeScript. The producer behavior remains retryable; this finding is disposition vocabulary/presentation only.

### R5-08 — P1 — D5 lineage descent does not inherit session notes

**TypeScript (spec):**

- `packages/plugin/src/features/magic-context/storage-clone.ts:152-188` copies session notes into a fork when their anchor remains in the inherited branch, remapping anchor id and ordinal.
- `packages/plugin/src/features/magic-context/storage-clone.ts:426-493` performs that note copy in the same fenced state-copy transaction.

**Rust:**

- `crates/mc-store/src/lib.rs:712-724` stores native notes under a session id.
- `crates/mc-store/src/lib.rs:10229-10254` clones source core/meta for D5 descent.
- `crates/mc-store/src/lib.rs:10265-10332` copies transcripts, compartments, tags, temporal marks, user hints, Channel-1 appends, and overlay frontier, but omits both `mc_notes` and a new-session note copy.

**Divergence / executable check:** Create an active session note anchored before the fork/descent seam, then inherit the same retained history. TypeScript Pi clone reads the note under the destination session. Rust D5 has no target-session `mc_notes` row, so target-session note reads/delivery cannot see it.

**Spec side:** TypeScript.

**INFERRED — not runtime-verified:** The table-copy omission and session-keyed schema are direct, but this audit did not execute a D5 producer fixture with a note row.

### R5-09 — P1 — Rust `ctx_expand(start,end)` returns whole overlapping chunk transcripts

**TypeScript (spec):**

- `packages/plugin/src/tools/ctx-expand/tools.ts:108-129` calls `readSessionChunk` with exactly `[start, effectiveEnd + 1)` and returns only that requested ordinal slice (plus a continuation when the token budget cuts it).

**Rust:**

- `crates/mc-module/src/lib.rs:10168-10187` loads up to 64 transcript rows overlapping the requested range.
- `crates/mc-module/src/lib.rs:12502-12543` matches an overlapping compartment, then appends its entire persisted transcript without clipping transcript lines to `start..end`.

**Divergence / executable check:** Persist one transcript covering ordinals 1–100 and call `ctx_expand(start=50,end=55)`. TypeScript returns 50–55. Rust returns the full overlapping transcript (potentially 1–100) until its byte cap.

**Spec side:** TypeScript.

### R5-10 — P1 — Rust cannot honor durable full-message/verbose recovery after snapshot loss

**TypeScript (spec):**

- `packages/plugin/src/tools/ctx-expand/render.ts:1-21` defines message mode as full, untruncated recovery from harness-stored history and verbose mode as actual per-message previews.
- `packages/plugin/src/tools/ctx-expand/render.ts:202-225` reads one raw stored message by ordinal and renders every recoverable part.
- `packages/plugin/src/tools/ctx-expand/render.ts:236-271` builds verbose output from actual raw messages.

**Rust:**

- `crates/mc-module/src/lib.rs:10119-10139` can return an exact message only while a process-local cached request snapshot survives; otherwise it loads the transcript containing that ordinal.
- `crates/mc-module/src/lib.rs:12396-12413` returns the entire chunk-builder transcript and explicitly warns that tool calls/text may already be summarized or truncated.
- `crates/mc-module/src/lib.rs:12705-12773` manufactures verbose per-ordinal entries by expanding transcript spans, rather than recovering raw message parts.

**Divergence / executable check:** Publish a chunk containing a large tool output, restart/evict the module snapshot, then call `ctx_expand(message=<tool-result ordinal>)` and a verbose range around it. TypeScript still returns the stored full tool output and exact per-message previews. Rust returns a bounded whole-chunk transcript; verbose mode may repeat one summarized span for multiple ordinals.

**Spec side:** TypeScript.

### R5-11 — P1 — Native zero-based ordinal 0 is unexpandable

**TypeScript (spec):**

- `packages/plugin/src/tools/ctx-expand/tools.ts:62-68` requires a positive ordinal because the TypeScript raw-history providers expose the same positive numbering printed in their compartment headings/search hits.

**Rust:**

- `crates/mc-store/src/lib.rs:10177-10186` explicitly accepts zero-based fresh-lineage anchors for native/Claude Code input.
- `crates/mc-module/src/lib.rs:10119-10120` ignores `message=0`.
- `crates/mc-module/src/lib.rs:10141-10154` rejects a range beginning at 0.
- `crates/mc-module/src/lib.rs:13770-13779` advertises schema minima of 1 for every `ctx_expand` ordinal argument.

**Divergence / executable check:** On a zero-based native session whose first compacted message/heading begins at ordinal 0, call `ctx_expand(message=0)` or `ctx_expand(start=0,end=0)`. The module rejects/falls past both modes, so an ordinal printed by native history is not consumable by its own recovery tool. TypeScript headings and tool arguments remain in one numbering domain.

**Spec side:** TypeScript's externally self-consistent ordinal contract; Rust needs an adapter/normalization equivalent for its zero-based ingress.

## WHERE I LOOKED

### Fix-wave interaction seams — no additional finding

- `crates/mc-module/src/selection.rs:294-378,735-791,1091-1357`: frozen reduction targets exclude an arc before supersession/dedup/emergency accounting; protected tag/exempt-message sets are applied before the stable decision merge. I found no route by which smart-drops reselect an already frozen reduction.
- `crates/mc-module/src/transform.rs:3606-3660,3834-3887,4728-4790,9227-9385,9506-9538,10327-10411`: checked first-application/replay ordering for stale-reduce, image, reasoning-age, placeholder, and merged-assistant-reasoning strips. The R4 subagent B-arm uses `is_provider_prefix_mutation_pass` at `4769-4774`, not the primary-only `is_bust_pass`, and is covered by the subagent regression at `16379-16460`.
- `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1300-1419,1929-2011` and `supersession-reclaim.ts:27-136`: checked the TypeScript frozen-id and supersession interaction. No additional selection/freeze divergence found.
- `crates/mc-module/src/transform.rs:5199-5228`, `crates/mc-module/src/scheduler.rs:515-529,744-758`, and `crates/mc-module/src/lib.rs:13501-13528`: regular scheduling uses soft geometry, only the absolute wall uses hard geometry, and the historian trigger uses the same usage→soft-geometry→200k denominator order. No regular-lane denominator regression found; R5-05 is isolated to manual wrapup.

### Remaining model-key reads/comparisons

Meaningful TypeScript identity comparisons inspected are canonicalized: `transform.ts:983-989,1128-1133,1272-1278,2267-2276`; `event-resolvers.ts:140-151`; `rust-mode-transform.ts:1570-1572,1603-1607`; `inject-compartments.ts:1548-1553,2772-2778`; `packages/pi-plugin/src/inject-compartments-pi.ts:989-1002,2041-2057`; `lkg-replay.ts:519-523`; and `prompt-surface-runtime.ts:244-253`.

The Rust runtime still reads/compares raw `model_key` at:

- `crates/mc-module/src/transform.rs:3397-3402` — detected-overflow proof identity.
- `crates/mc-module/src/transform.rs:5274-5288` — render identity bytes.
- `crates/mc-module/src/lib.rs:6724-6742` — prompt-surface epoch freeze.
- `crates/mc-module/src/config.rs:154-199` — cache-TTL exact/provider/model lookup.
- `crates/mc-module/src/scheduler.rs:433-464` — execute-threshold lookup.

I found no shipped alias divergence at those Rust sites: the OpenCode Rust adapter canonicalizes its model identity before transport (`rust-mode-transform.ts:1570-1572`), forwards host-resolved threshold/cache controls, and canonicalizes prompt-surface selection at the TypeScript edge; the two aliases added by `b5e999ef` are Pi/OMP-native `openai-codex` and `google-antigravity`, neither of which is a Claude Code native provider id. A direct unsupported caller that bypasses those adapters remains spelling-sensitive, but that is not a shipped TS-vs-Rust path.

### Wrapup surfaces — examined with no further finding

- `packages/plugin/src/hooks/magic-context/protected-tail-boundary.ts:728-866` vs `crates/mc-module/src/boundary.rs:550-652,1356-1405`: raw-role keep counting, closed-tool-arc fencing/refencing, newest-result protection, and fixed target watermark.
- `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:573-617,830-865` vs `crates/mc-module/src/historian_validate.rs:536-624`: final weak-lookahead behavior matches for facts, anchored events, primers, and final-chunk observations. R5-06 is the OpenCode caller's missing user-memory enable flag, not the final promotion filter.
- `packages/pi-plugin/src/commands/ctx-wrapup.ts:189-490`, `packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts:193-521`, `crates/mc-module/src/lib.rs:4541-4651,5756-6405`: lease ownership, no-forward-progress detection, final force-keep downgrade on token-capped chunks, and queued-next-message materialization.

### Compaction-off surfaces — examined with no further finding

- `packages/plugin/src/hooks/magic-context/transform.ts:741-825`, `transform-postprocess-phase.ts:820-870,1356-1444,1929-2011`, `inject-compartments.ts:2085-2260,3071-3303`.
- `crates/mc-module/src/transform.rs:2343-2590,14807-14867` (additive composer/producer and its byte-stability regression), plus `crates/mc-module/src/lib.rs:7526-7583` for config routing.
- Raw history, reductions, tags, stale strips, historian rows, todo synthesis, auto-search, and pressure reclaim stay non-mutating/hidden in both off-mode lanes. Findings R5-01/R5-02 are the remaining m0/m1 epoch and mural differences.

### Clone/fork and D5 descent — examined with no further finding

- `packages/plugin/src/features/magic-context/storage-clone.ts:228-508` and `packages/pi-plugin/src/clone-inheritance.ts:82-140,160-227`: compartment/tag/source-content/pending-op filtering, strip-id blobs, todo anchor remap, and marker migration.
- `crates/mc-store/src/lib.rs:9880-10426` and `crates/mc-module/src/transform.rs:2750-2816,4728-4790`: source selection, prior publish fence, copied core/meta, ordinal continuation, placeholder boundary, and post-copy materialization. R5-08 is the session-note omission; I found no second observable inherited-state mismatch.

### `ctx_expand` surfaces — examined with no further finding

- `packages/plugin/src/tools/ctx-expand/tools.ts:47-136`, `render.ts:1-271`, and `constants.ts:1-13`.
- `crates/mc-module/src/lib.rs:10102-10207,12259-12262,12360-12467,12502-12779` and `crates/mc-store/src/lib.rs:700-710`.
- Input validation, last-compacted-tail rejection, nominal 15k output budget, cached raw part rendering, and missing/evicted diagnostics were inspected. Findings R5-09 through R5-11 cover the transcript slicing, durability, and ordinal-domain differences.

### Recent single-sided commits

- `9cb94a2d` conflict detection: adapter/bootstrap-only. It chooses one transform owner before dispatch; the bound Rust module receives the resolved mode/config and has no competing plugin-registration surface. No Rust twin needed.
- `4a4b7b4b` whitespace sentinel invisibility: Rust twin is already present in `crates/mc-module/src/transform.rs:11304-11310` and feeds `apply_serializer_residual_to_message` at `10388-10405`.
- `b5e999ef` alias config keys: adapter-canonical/host-resolved for module traffic; raw Rust sites are enumerated above. No shipped gap found.
- `2525c6ca` ingress-keyed delta reuse: module-only optimization for native attachment/projection caches. TypeScript mode receives and transforms full harness message objects and has no equivalent delta cache to key.
- `af05a75c` degraded-core snapshots: module-only bounded native cache behavior. TypeScript mode has no deep native attachment snapshot or degraded-core fallback.
- `23e37040` trigger denominator: the TypeScript historian boundary receives its already resolved per-pass `contextLimit`; it has no separate module prepare path with a missing-usage fallback. No TS twin needed.

### Other files/classes inspected

- `crates/mc-module/src/config.rs`, `prompt_surface.rs`, `m0_compose.rs`, `m1_compose.rs`, `historian.rs`, `historian_chunk.rs`, `historian_validate.rs`, `codec/opencode.rs`, `codec/pi.rs`.
- `packages/plugin/src/hooks/magic-context/command-handler.ts`, `module-transport.ts`, `module-wire.ts`, `rust-mode-transform.ts`, `recomp-orchestrator.ts`, `compartment-runner-incremental.ts`, `compartment-runner-historian.ts`, `read-session-chunk.ts`.
- `packages/pi-plugin/src/context-handler.ts`, `ctx-wrapup.ts`, `pi-historian-runner.ts`, `clone-inheritance.ts`, and `inject-compartments-pi.ts`.
