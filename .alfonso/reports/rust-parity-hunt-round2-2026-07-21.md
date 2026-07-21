# Rust transform parity hunt — round 2 (2026-07-21)

## Scope and method

This is an unprimed, source-first comparison of the current Rust authority path (`crates/mc-core`, `crates/mc-store`, `crates/mc-module`) against the TypeScript OpenCode transform path under `packages/plugin/src/hooks/magic-context/` and its feature services. I started with the newest implementation commits (`ca2c5d37`, `876751fc`, `fefed182`, `6c86badd`, `0c04f838`, `13b6d1b8`), then walked cross-store seams, the protected pass lifecycle in `ARCHITECTURE.md`, and the live scheduler/historian/decay/nudge/emergency formulas. I did not inspect prior reports or plans.

Severity follows the requested scale. “Cost” is a source-derived bound unless explicitly assigned to a live beat.

## Findings

### 1. P0 — module-to-context memory adoption can move another project's row

**Rust anchor:** `crates/mc-store/src/lib.rs:7703-7724` deduplicates a memory on `(project_path, category, normalized_hash)`. The source-identity path is likewise namespaced: `crates/mc-store/src/lib.rs:11417-11432` resolves `(context_store_uuid, context_row_id)`.

**TypeScript anchor:** `packages/plugin/src/features/magic-context/context-authority.ts:1074-1087` falls back to a `category + normalized_hash` query with no `project_path` predicate and adopts the sole global candidate. `packages/plugin/src/features/magic-context/context-authority.ts:1241-1287` then updates that adopted row, including its `project_path`, from the module snapshot.

**Divergent sequence:** project A has the only context-store memory `(PROJECT_RULES, hash H)`. The module historian creates the same category/content under project B; as a module-native row it has no context row identity. Mirror-back for B finds the sole A candidate by category/hash, adopts A's id, and updates that row to project B. Rust treats A and B as distinct; TypeScript moves A's row to B.

**Cost:** one durable context memory is removed from its original project and its row identity is reassigned per collision. Any row-id-bound embeddings or references now point at the wrong ownership. This is normal-use data loss whenever the same fact first appears in another project, not a malformed-wire case. The global content-adoption fallback was introduced in `c3356f9cb` and remains unfenced after the current complete-row fixes.

---

### 2. P0 — Rust mutates the tail while the historian is running instead of honoring the veto

**Rust anchor:** the module commits `transform_with_projection` before checking or starting the historian (`crates/mc-module/src/lib.rs:5881-5927`, historian handling starts at `:5946`). Selection reads pending drops and runs on Execute/force/hard without any active-historian input (`crates/mc-module/src/transform.rs:1206`, `:1369-1461`).

**TypeScript anchor:** `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:548-554` derives `compartmentRunning`; pending operations are vetoed at `:617-622` and heuristics at `:647-663`, except for force pressure or a known m[0] hard fold.

**Divergent sequence:** a historian is already awaiting output, or the current pass starts one, while an ordinary scheduler Execute has an eligible two-pass/agent drop. Rust selects and freezes the reduction before it observes the live historian, returning a byte-changing SOFT. TypeScript sees the active run and leaves the operation pending. The historian publish then creates another deferred m[1] change, so Rust can pay for a tail rewrite now and another refresh later instead of coalescing them.

**Cost:** one avoidable provider cache miss from the earliest changed tail block per historian/Execute overlap; the uncached suffix can approach the full near-threshold tail. The exact billed-token loss depends on the provider breakpoint and is assigned a live beat below. Hard folds and force/emergency passes are not affected: both implementations deliberately bypass the veto there.

---

### 3. P1 — ordinary Execute does not consume pending m[1] work in Rust

**Rust anchor:** `crates/mc-module/src/transform.rs:1349-1363` explicitly excludes ordinary zero-reduction Execute from `pass_already_busting`; `:1482-1498` opens `bust_opportunity` only for an existing bust or a newly selected reduction. The regression is locked in by `:7657-7689`, which expects repeated Execute passes to hide a memory update until `arm_soft_refresh`.

**TypeScript anchor:** `packages/plugin/src/hooks/magic-context/cache-busting-signals.ts:12-20` allows deferred consumption whenever `schedulerDecision === "execute"` (unless an active run vetoes it), and the m[1] materializer rerenders on that cache-busting pass in `packages/plugin/src/hooks/magic-context/inject-compartments.ts:2523-2528`.

**Divergent sequence:** after an m[0] exists, a compartment, memory mutation, new memory, note, or profile-version signal arrives. The scheduler later returns Execute with no tail reduction. TypeScript renders the pending m[1] delta; Rust replays SOFT+ indefinitely until a reduction, flush, force, or hard condition supplies a different bust opportunity.

**Cost:** user-visible state can remain stale for an unbounded number of ordinary Execute cycles. The 15%-of-m[0], 20%-of-history-budget, and `>40` mutation pressure-refold checks (`crates/mc-module/src/transform.rs:1908-1914`) also cannot run until that unrelated opportunity occurs. This exact behavior was introduced by `13b6d1b8`; it contradicts the protected contract that Execute is a genuine bust cycle for deferred work.

---

### 4. P1 — user-profile changes are acknowledged without being rendered

**Rust anchor:** `crates/mc-module/src/m1_compose.rs:112-159` hashes `user_profile_version` into the in-session revision, but `:303-325` explicitly omits `<new-user-profile>` from the composed body. A successful SOFT then advances the m[1] revision in `crates/mc-module/src/transform.rs:1975-2005`.

**TypeScript anchor:** `packages/plugin/src/hooks/magic-context/inject-compartments.ts:2006-2021` renders a budgeted `<new-user-profile>` whenever the current version differs from the m[0] marker. The current state-sync payload supplies both profile rows and version (`packages/plugin/src/hooks/magic-context/module-state-sync.ts:1310-1336`).

**Divergent sequence:** m[0] contains profile V; the global user profile changes to V+1 and state sync updates the module store. On the next Rust SOFT, the revision mismatch is consumed but the body has no profile block. Every later pass sees the new revision as already applied; the profile remains invisible until an unrelated HARD rebuilds m[0]. TypeScript surfaces V+1 in m[1] on the consuming pass.

**Cost:** 100% of the changed profile delta is omitted for the remainder of the m[0] epoch. This is a merge interaction: `13b6d1b8` added the version to the revision signal, while the post-fix tree now has the profile/version source, but the old “no source yet” composer branch remained.

---

### 5. P1 — fresh subagent pressure is demoted out of the emergency selector

**Rust anchor:** `crates/mc-module/src/transform.rs:1264-1273` rewrites subagent `Force85` and `Emergency95` decisions to ordinary `Execute` and clears the drain latch. `:2442-2449` maps only the two force decisions to `PassClass::EmergencyForce`.

**TypeScript anchor:** `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:539-547` makes tiered emergency drop eligible at ≥85% for both primary and subagent sessions; `:647-663` runs heuristics on that pressure path, and `packages/plugin/src/hooks/magic-context/heuristic-cleanup.ts:90-155` invokes the target-headroom planner.

**Divergent sequence:** a subagent reaches 85% with active old tool arcs, no prior two-pass watermark, and smart drops disabled. TypeScript walks T3→T2→T1 toward the 30%-of-working-span target. Rust changes the pass to Execute; the ordinary selectors can select nothing, so the subagent continues toward overflow without the only guaranteed pressure floor.

**Cost:** up to the entire emergency reclaim target is missed. On a tool-heavy subagent this is the difference between returning below the execute ceiling and exhausting context. The demotion was introduced by `6c86badd`; avoiding primary m[0] materialization does not require discarding the emergency selection class.

---

### 6. P1 — Rust's default historian producer window is four times smaller

**Rust anchor:** `crates/mc-module/src/config.rs:28-41` defaults the historian context limit to 32,000 and derives `round(limit × 0.25)`, clamped to 8,000–50,000; the default therefore yields an 8,000-token chunk.

**TypeScript anchor:** `packages/plugin/src/hooks/magic-context/derive-budgets.ts:30-34` uses a 128,000 fallback, and `:68-73` applies the same 25%/clamp derivation; the default yields 32,000 tokens.

**Divergent sequence:** no historian context-limit override is configured and the module has no model-catalog value. Rust asks the historian to process at most 8k formatted tokens per pass; TypeScript processes 32k.

**Cost:** four times less source per producer invocation and potentially roughly four times as many producer rounds for the same eligible history, with correspondingly weaker cross-message lookahead. `0c04f838` introduced the Rust fallback while claiming the TypeScript derivation; the formula matches, the default input does not.

---

### 7. P1 — nudge channels do not use the TypeScript eligibility surface

**Rust anchor:** Channel 1 applies a `reclaimable >= usable/3` gate and bands on reclaimable/working-window pressure in `crates/mc-module/src/transform.rs:4659-4697`. Channel 2 sums only `tag.token_count` at `:4556-4572`, and the response suppresses Channel 2 for subagents at `:2227-2242`.

**TypeScript anchor:** Channel 1 uses undropped/estimated-input pressure with a 20% gentle floor in `packages/plugin/src/hooks/magic-context/ctx-reduce-nudge.ts:293-321`. TypeScript's usable total includes conversation, tool input/output, and reasoning components (`packages/plugin/src/features/magic-context/storage-tags.ts:175-190`, `packages/plugin/src/hooks/magic-context/transform.ts:2082-2107`), and the channel gate explicitly includes subagents (`packages/plugin/src/hooks/magic-context/transform.ts:2059-2066`, `:2140-2145`).

**Divergent sequence:** a large live tail has reclaimable tool output equal to 20% of estimated input but less than one third of Rust's derived working window: TypeScript emits a gentle Channel-1 nudge while Rust stays quiet. Separately, large tool arguments/reasoning can cross TypeScript's Channel-2 one-third gate while Rust undercounts them. Even if it crosses, a subagent receives no Rust Channel-2 directive while TypeScript allows one.

**Cost:** a complete missed nudge cycle, not only a severity-label mismatch. This can leave all eligible `ctx_reduce` work unrequested until a later pressure/drop path. The Channel-2 10,000-token floor, one-third predicate after aggregation, and reminder text otherwise match.

---

### 8. P1 — temporal compartment dates are persisted instead of rendered under the flag

**Rust anchor:** module-native historian publication writes `start_date: None` and `end_date: None` (`crates/mc-module/src/historian.rs:34-59`), while `crates/mc-module/src/decay_render.rs:44-63` renders only those stored values. The current `temporal_awareness` flag is used for tail-gap overlays at `crates/mc-module/src/transform.rs:1062`, not m[0]/m[1] composition.

**TypeScript anchor:** `packages/plugin/src/hooks/magic-context/inject-compartments.ts:1272-1300` resolves boundary timestamps in one OpenCode DB query on each materializing render and adds dates only when `temporalAwareness` is enabled. Conversely, the context→module seed always serializes available dates at `packages/plugin/src/hooks/magic-context/module-state-sync.ts:451-503`.

**Divergent sequences:** with the default flag enabled, every Rust-native compartment lacks the date that TypeScript derives from its boundary messages. With the flag disabled after a seeded compartment has dates, TypeScript omits dates but Rust continues to render the persisted values.

**Cost:** all native compartment headings lose temporal evidence under the default, while disabled configurations still pay and expose those bytes. `ca2c5d37` freshly routed `temporal_awareness` into Rust but only applied it to gap overlays, leaving both halves of compartment-date behavior divergent.

---

### 9. P1 — Rust applies workspace floors to m[1] new memories; TypeScript does not

**Rust anchor:** `crates/mc-module/src/m1_compose.rs:238-300` resolves workspace membership and calls the shared `trim_memories_to_budget` with membership at 25% of the memory budget. That helper reserves equal member floors (`crates/mc-module/src/m0_compose.rs:167-239`).

**TypeScript anchor:** `packages/plugin/src/hooks/magic-context/inject-compartments.ts:1964-1998` reads union new memories but calls non-workspace `trimMemoriesToBudgetV2` with the 25% budget, so selection is global permanent/importance/id order with no per-member floor.

**Divergent sequence:** workspace A has enough high-priority post-watermark memories to fill the quarter-budget and workspace B has lower-priority additions. TypeScript emits only A. Rust first reserves B's equal share, then fills globally, producing different membership and ordering.

**Cost:** up to the full m[1] new-memory quarter-budget can contain a different memory set; every affected SOFT has different provider-visible bytes. This m[1] reuse of the m[0] floor helper was introduced by `6c86badd`. The m[0] workspace floor itself matches TypeScript.

---

### 10. P2 — sparse tag maps clear untagged reasoning in Rust

**Rust anchor:** `crates/mc-module/src/transform.rs:4991-4996` returns zero for a message missing from a nonempty tag map. Replay then treats `0 <= reasoning_watermark` as aged at `:5155-5174`; cutoff detection likewise accepts the zero at `:5705-5719`. Native clearing uses ordinal fallback for an individually missing tag at `:5811-5816`.

**TypeScript anchor:** both first clearing and replay explicitly skip `msgTag === 0` in `packages/plugin/src/hooks/magic-context/strip-content.ts:308-340` and `:243-277`.

**Divergent sequence:** at least one message is tagged, establishing a positive watermark, while another historical assistant message with typed reasoning has no tag. Rust clears the untagged reasoning (zero, or an ordinal from a different identity space); TypeScript leaves it intact because age cannot be established.

**Cost:** all typed/inline reasoning bytes in each affected untagged message can be removed prematurely. The condition is edge-shaped because ordinary tool/text messages are usually tagged. The tag/strip port that introduced this behavior is `876751fc`.

---

### 11. P2 — historian chunk termination differs on Unicode and filtered tails

**Rust anchor:** truncation searches Unicode scalar indices in `crates/mc-module/src/historian_chunk.rs:623-654`. Rust's `has_more` calculation uses the last included end ordinal at `:355-385`.

**TypeScript anchor:** `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:164-180` binary-searches UTF-16 `string.length`/`slice` indices. `packages/plugin/src/hooks/magic-context/read-session-chunk.ts:783-815` computes completion with `max(lastOrdinal, highestScannedOrdinal)` so trailing scanned-but-filtered messages count as consumed.

**Divergent sequences:** an astral character at the exact token-budget cut can leave the two prompts differing by a scalar (TypeScript can cut a surrogate pair). Separately, meaningful content followed only by filtered/noise messages makes TypeScript report the scan complete while Rust can report `has_more`, causing an extra empty/no-op round or a different final-chunk healing decision.

**Cost:** at most a boundary scalar in the prompt for the Unicode case; one extra producer/no-op iteration or different final-chunk disposition for the filtered-tail case.

---

### 12. P2 — scheduler TTL differs at the exact boundary and for overflowing durations

**Rust anchor:** `crates/mc-module/src/scheduler.rs:584-597` fires TTL Execute when `elapsed >= ttl`; its parser uses `u64` plus checked multiplication at `:283-305`.

**TypeScript anchor:** `packages/plugin/src/features/magic-context/scheduler.ts:108-113` requires `elapsed > ttl`; parsing uses JavaScript `Number` multiplication at `:30-46`.

**Divergent sequences:** exactly at the TTL millisecond, Rust executes while TypeScript defers. For a syntactically numeric duration outside Rust's `u64`/checked product, Rust rejects/falls back while TypeScript can retain a much larger finite duration.

**Cost:** one pass of scheduling skew at the ordinary boundary; pathological configured durations can produce a persistent policy difference. Percentage/token threshold resolution and the 65%/80% caps otherwise match.

---

### 13. P2 — emergency reclaim rounding can select a different final arc

**Rust anchor:** `crates/mc-module/src/selection.rs:658-663` compares the unrounded `current - target` to the 2,000-token rearm floor, and `:633-635`, `:704-715` rounds byte-to-token reclaim per grouped arc.

**TypeScript anchor:** `packages/plugin/src/hooks/magic-context/emergency-drop.ts:192-204` rounds `current - target` before the rearm comparison; `:98-100`, `:240-251` estimates and accumulates per tag.

**Divergent sequence:** a reclaim target within half a token of 2,000 can arm only one implementation. With several fractional-token tag/arc estimates, per-arc versus per-tag rounding can also cross the target after a different candidate, selecting one extra or one fewer tool arc.

**Cost:** zero or one extra emergency bust at the rearm edge, or normally one final arc of selection skew. Target fraction 0.30, T1/T2 `ceil(0.20 × n)` reserve, traversal order, and same-input rearm latch otherwise match.

## Intentional divergences

No additional documented intentional divergence was found. The expected subagent differences—no synthetic m[0]/m[1] prefix and no historian firing—match TypeScript's reduced feature mode; they do not explain the emergency-selector or Channel-2 gaps above.

## Ranked fix order

1. **Finding 1:** project-scope the content-adoption query and require the local store UUID for row-id adoption before any more authority mirroring.
2. **Finding 2:** thread the process-local historian-active state into Rust transform selection and apply the same ordinary-pass veto, retaining hard/force bypasses.
3. **Findings 3 and 4 together:** make scheduler Execute a deferred-work consumption opportunity, then actually compose/profile-version deltas before advancing their revision.
4. **Finding 5:** retain `EmergencyForce` selection for subagents while suppressing only primary-only materialization/blocking behavior.
5. **Finding 6:** align the no-catalog historian context fallback to 128k (or send a resolved value on every binding).
6. **Finding 7:** port the TypeScript Channel-1 denominator/bands, aggregate all token lanes for Channel 2, and remove the subagent suppression.
7. **Finding 8:** carry boundary timestamps or derive them at render time, and gate heading dates with `temporal_awareness` rather than persistence.
8. **Finding 9:** use non-workspace trimming for m[1] new-memory deltas; preserve workspace floors only in m[0].
9. **Finding 10:** treat tag zero as “age unknown” everywhere and never substitute ordinals into the tag-number space.
10. **Findings 11–13:** align chunk scan/truncation and the exact TTL/emergency numerical edges; add cross-language goldens around each boundary.

## Surfaces verified clean

- **Decay:** half-life constants, importance/pressure equation, tier boundaries, archive predicate, rendered-tier cap, and once-per-pass budget pressure match between `crates/mc-core/src/decay.rs:21-145` and `packages/plugin/src/hooks/magic-context/decay-curve.ts:26-155`.
- **Historian trigger core:** `round(usable × 5%)` clamped 5k–50k, tail scan `max(6k, budget × 3)`, and protected-tail threshold-minus-two arithmetic match (`crates/mc-module/src/boundary.rs:337-344`, `:705-706`, `:765-779`; `packages/plugin/src/hooks/magic-context/derive-budgets.ts:22-28`, `packages/plugin/src/hooks/magic-context/compartment-trigger.ts:373-387`, `:658-713`).
- **Scheduler pressure:** percentage/token threshold resolution and 65%/80% caps match; only the TTL edges in Finding 12 differ.
- **Emergency planner core:** fixed-floor equation, 0.30 target fraction, T3→T2→T1 order, T1/T2 20% reserve, protected set, and same-input idempotence latch match; only Finding 13's rounding and Finding 5's subagent routing differ.
- **m[1] pressure backstop:** `memory updates > 40`, absolute m[1] `> 20%` of history budget, and m[1] `> 15%` of m[0] with the 500-token m[0] floor match. The gate is stranded by Finding 3, but the constants/formulas are correct.
- **m[0] budget selection:** permanent-first/global ordering, category wrapper accounting, workspace equal floors, and global leftover fill match. The m[1]-only reuse is Finding 9.
- **Cross-store row shape:** complete memory snapshot fields now match in both directions; sparse feed updates preserve omitted fields; full state sync is transactional and replay-idempotent for compartments, memories, mutations, profile, workspace, drop/strip seeds, todo/nudge anchors, marker/deferred state, Channel-2 state, and watermarks. Identity adoption remains Finding 1.
- **Frozen namespaces:** `red:*` and `strip:*` are both accepted as current cache shape (`crates/mc-module/src/transform.rs:2522-2545`), survive/prune through separate filters (`:2878-2897`, `:5210-5227`), and first detection is bust-only while replay is pass-independent.
- **Provider-visible strip replay:** stale `ctx_reduce`, placeholder/system injection, processed-image, structural-noise, and persisted reasoning surfaces use frozen replay rather than live moving-window detection. The untagged-age exception is Finding 10.
- **Subagent seed/firing:** state sync still seeds authority data before a subagent transform, while Rust omits m[0]/m[1] and explicitly disables historian firing; this matches TypeScript reduced feature mode. Emergency and Channel-2 behavior are the listed exceptions.
- **Historian output gates:** model chain, 0.1 temperature, 32k output cap, 600s await, minimum-substance bypass for emergency/fold-only profiles, sparse ordinal validation, numeric discard-last slack, side-channel privacy/promotion gates, and reattach redrain behavior match current TypeScript source.
- **Project docs:** `inject_docs` now gates the m[0] docs block; docs changes do not self-trigger hard materialization, and the next natural hard fold incorporates them.

## Undetermined — needs a live beat

1. **Exact cache bill for Finding 2.** Hold a historian producer in Awaiting, queue one old tool drop, and send an ordinary Execute through TS and Rust with provider cache attribution enabled. Compare the earliest changed wire block plus `cache_read_input_tokens`/uncached input. Source establishes the extra Rust mutation; only its billed magnitude is provider-dependent.
2. **Forced-reconnect temporal backfill.** Publish a Rust-native compartment with temporal awareness on, render it, force a full authority rebind, then render again with the flag on and off. This establishes whether the TS mirror/full-seed loop later backfills dates and, if so, whether it causes an additional hard/cache byte transition.
3. **Store-UUID rollover fencing.** Keep a module store from context DB A, replace the context DB with B, and attempt the first authority transform/feed mirror. Verify whether the binding/state-sync fence rejects before `contextMemoryId` can adopt a same-number row. The local function ignores UUID equality, but the source alone does not prove whether the higher-level handshake makes that branch unreachable after rollover.
