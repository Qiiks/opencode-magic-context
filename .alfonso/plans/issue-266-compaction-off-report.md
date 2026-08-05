# Issue #266 — "Disable compaction" mode: implementation-path report

- **Status:** investigation/report only — no production code changed.
- **Issue:** `gh issue view 266`. User runs GPT models (no thinking blocks) and routes `/compact`
  through a separate plugin that hooks **OpenCode's native compaction**. They want Magic Context's
  **memory + dreamer + search** (the "knowledge layer") but want MC to stop managing/compacting the
  context window — no nudges, no compaction tools, no auto-compact, no tool-output pruning. Let
  OpenCode (or nothing) own the window.
- **Ufuk's concern (verbatim):** "I'm not sure if we can do this cleanly without breaking anything as
  so many parts are connected."
- **Source baseline:** all file/line citations are against HEAD `3fd7ee33` of this worktree
  (branch `alfonso/task/bg_8f56aad3-...`). Relative paths: TypeScript under `packages/plugin/`
  (OpenCode) and `packages/pi-plugin/` (Pi); Rust under `crates/`.
- **Hypothetical gate investigated:** a config flag `compaction: { enabled: false }` (name ours to
  propose; see §5). Both harnesses share ONE Zod schema
  (`packages/plugin/src/config/schema/magic-context.ts`, re-used by Pi at
  `packages/pi-plugin/src/config/index.ts:20-24`), so a single schema addition covers OpenCode+Pi.

---

## Executive summary / verdict

**It is buildable, and it is cleaner than the "so many parts are connected" fear suggests — but it is
NOT a one-line flag.** Three facts drive the whole design:

1. **Memory reaches the agent ONLY through the messages-transform (m[0]/m[1] injection), never
   through the system prompt.** The system-prompt path carries guidance + a sticky date and nothing
   else (`system-prompt-hash.ts:280-330`, `magic-context-prompt.ts:147-182`). Project memory is
   rendered into the `<project-memory>` block inside m[0]/m[1]
   (`inject-compartments.ts:371-421`, applied in `transform-postprocess-phase.ts:1184-1201`).
   ⇒ **"Register no `messages.transform` at all" would silently kill memory delivery** and thus fail
   the issue's core ask (memories WITHOUT compaction). The transform must keep running in a
   **reduced mode**.

2. **There is already a zero-compartment code path that injects memory WITHOUT trimming history.**
   When no compartments exist, `prepareCompartmentInjection` prepends the memory block and does no
   splice (`inject-compartments.ts:459-472`). With compaction off the historian never runs, so
   compartments stay empty forever and this path is always taken. **This is the exact "memory without
   compaction" behavior the issue wants — it already exists**, it just needs the surrounding
   pruning machinery gated off.

3. **MC and OpenCode native compaction are currently mutually exclusive at the plugin-enabled
   level.** `detectConflicts` treats `compaction.auto=true` (OpenCode's DEFAULT) as a blocking
   conflict and sets `pluginConfig.enabled=false` (`conflict-detector.ts:60-63`, `index.ts:147-155`),
   and the setup wizard writes `compaction.auto=false` into `opencode.jsonc`
   (`setup-opencode.ts:84-88`). Compaction-off mode REQUIRES native compaction to be available, so
   **the conflict detector and the setup/doctor writer are the two load-bearing couplings that must
   change first.** Everything else is gating.

**Recommended shape:** a **reduced transform mode** (keep `messages.transform` registered, gate off
every pruning/compaction stage, keep additive memory/docs injection + search indexing), plus
(a) teach `detectConflicts` that `compaction.auto=true` is not a conflict when compaction is off,
(b) stop setup/doctor from forcing `compaction.auto=false` in that mode,
(c) on Pi, stop cancelling `session_before_compact`,
(d) unregister the compaction-only tools (`ctx_reduce`, `ctx_expand`),
(e) clean up MC's OpenCode compaction markers so hidden history is not orphaned.

**Build/no-build leaning: BUILD.** The risk surface is real but enumerable (§9), the memory path
already degrades correctly, and there is strong precedent for subsystem toggles
(`dreamer.disable`, `sidekick.disable`, `historian.disable` in `config/agent-disable.ts:1-11`).
Estimated effort: **~5–7 mason-slices** (§10).

---

## 1. TRANSFORM PIPELINE

### 1.1 How the transform is registered and gated today

- Outer wrapper `createMessagesTransformHandler` (`plugin/messages-transform.ts:83-94`) is ALWAYS
  registered (`index.ts:538-545`). It delegates to `magicContextRuntime.magicContext`, which is
  `null` when the plugin is disabled (`create-session-hooks.ts:43-45` returns `magicContext: null`
  if `pluginConfig.enabled !== true`). A null hook = passthrough (messages returned unmodified).
- The real pipeline is `createTransform` (`hooks/magic-context/transform.ts:596`). Per pass it:
  1. resolves session id / meta (`transform.ts:623-650`);
  2. **exempts MC's own hidden children** (historian/dreamer/sidekick/migration) — full skip
     (`transform.ts:660-663`);
  3. **hands Rust-mode sessions to the module** and returns (`transform.ts:668-675`);
  4. computes `reducedMode = sessionMeta.isSubagent`, `fullFeatureMode = !reducedMode`
     (`transform.ts:681-682`). **This is the existing two-speed precedent** — subagents already run
     a reduced pipeline (tagging + ctx_reduce + heuristic drops, but NO historian/compartments/
     force-materialize; see gates below).
  5. schedules **message-index FTS reconciliation** (`transform.ts:633`).
  6. resolves model/usage, then runs the compaction stages below.

### 1.2 What must KEEP running with compaction off

| Stage | Why keep | Evidence |
|---|---|---|
| **Message-index FTS feed** | Feeds `ctx_search`'s `message` source from RAW messages. **Independent of compaction.** | `transform.ts:633` `scheduleReconciliation`; ALSO event-driven: `hook-handlers.ts:292-297` (`scheduleIncrementalIndex` on terminal `message.updated`), `hook-handlers.ts:368-370` (reconciliation on every non-delete event), Pi `index.ts:1950-1956` (`message_end`). Confirmed it indexes **raw** OpenCode/Pi messages, not compartments (`message-index-async.ts:145-189`, `message-index.ts:80-89`). |
| **Memory injection (m[0]/m[1])** | The issue's whole point. Memory rides m[0]/m[1], not the system prompt. | `transform.ts:1985-2001` builds the `m0M1` arg; `transform-postprocess-phase.ts:1184-1201` calls `injectM0M1`; memory block built in `inject-compartments.ts:371-421`. |
| **Project docs / user-profile / key-files adjuncts** | Ride m[0]/m[1] (`injectDocs`, `transform.ts:1995`). Keep — free value, no pruning. | `transform.ts:1985-2001`. |
| **System-prompt guidance** | Tells the agent about memory/search/notes. Keep, but switch to the **no-`ctx_reduce` variant** that already exists. | `system-prompt-hash.ts:280-330`; no-reduce variant at `magic-context-prompt.ts:178-179` (`BASE_INTRO_NO_REDUCE`). |
| **Session→project binding / identity** | Needed for memory + search scoping. | `transform.ts:1361-1390`. |

**Message-index independence — VERIFIED.** The FTS is fed from raw session messages via event hooks
that fire regardless of whether the transform mutates anything. Even a fully no-op transform still
yields a searchable `message` source. (This corrects a plausible-but-wrong assumption that the FTS
is built from compartment data — it is not.)

### 1.3 What must be SKIPPED (gated off) with compaction off

| Stage | What it does | Gate today | Evidence |
|---|---|---|---|
| **Historian / compartment trigger** | Fires the summarizer that turns history into compartments. | `fullFeatureMode && historianRunnable && !compartmentInProgress` | `transform.ts:1459-1496`; `historianRunnable` from `isHistorianRunnable` (`hook.ts:387`, `agent-disable.ts:9-11`). |
| **m[0]/m[1] session-history render + history trim** | Renders `<session-history>` and splices/trim raw tail to the compartment boundary. | `fullFeatureMode` | `transform.ts:1500-1542` (`prepareCompartmentInjection`), splice in `inject-compartments.ts:543-551`. With 0 compartments the trim is a no-op (`inject-compartments.ts:459-472`). |
| **Scheduler execute/defer (drop scheduling)** | Decides whether pruning passes run. | `resolveSchedulerDecision` | `transform-context-state.ts:61-87`. |
| **Heuristic cleanup / tool-output drops / pending ops** | The "tool output pruning" the issue explicitly hates. | `shouldRunHeuristics`, `shouldApplyPendingOps` | `transform-postprocess-phase.ts:677-756`. |
| **Force materialization + tiered emergency drop (≥85%)** | Aggressive context reduction. | `fullFeatureMode && pct ≥ forceMaterializationPercentage` | `transform-postprocess-phase.ts:610-620`. |
| **95% block (force-start historian + block transform)** | Hard ceiling. | `historianRunnable && canRunCompartments && pct ≥ 95` | `transform-compartment-phase.ts:324-329`; `BLOCK_UNTIL_DONE_PERCENTAGE=95` (`compartment-trigger.ts:42`). |
| **Channel 1 / Channel 2 nudges** | Nudge the agent to call `ctx_reduce`. Pointless once drops are off. | `ctxReduceCallable` | `transform.ts:2254-2372` (Channel 1 baseline + Channel 2 trigger `shouldTriggerChannel2`). |
| **Strips (reasoning/thinking, structural noise, placeholders)** | Mutate the message array to remove content. See decision note below. | inside transform/strip-content | `hooks/magic-context/strip-content.ts` (see STRUCTURE.md:131). |
| **Synthetic todowrite injection (B7)** | Context-management injection. | postprocess | `transform-postprocess-phase.ts:130-176` (STRUCTURE.md:129). |
| **Caveman text compression** | Age-tier text compression = compaction. | `cavemanTextCompression` config, primary-only | `transform.ts:1977`; `hooks/magic-context/caveman.ts`. |
| **Temporal markers** | Additive informational overlays. Judgment call (see below). | every pass | `transform.ts:1555-1569`. |

**Decision points the builder must make (recommend in brackets):**
- **Strips:** [SKIP them in compaction-off mode.] They are context management (they remove content
  from the wire). The issue asks MC to stop managing the window. Tradeoff: reasoning strips save
  tokens; keeping them is defensible, but the cleanest "hands off" semantics is additive-only. If
  kept, they must be provably non-pruning for tool output.
- **Temporal markers:** [SKIP] for a clean hands-off pass; low impact either way (they are additive).
- **`ctx_reduce` availability verdict:** must resolve to NOT callable so guidance + nudges +
  synthetic-todo all agree (`transform.ts:694-696`, `resolveCtxReduceAvailabilityFromMessages`).

### 1.4 Cleanest shape: "no transform" vs "reduced mode"

- **"Register no `messages.transform`" — REJECTED.** Kills memory injection (m[0]/m[1] lives in the
  transform). The system prompt cannot carry memory today (§1.2), so the agent would get zero project
  memories. Rebuilding a system-prompt memory path is a large, cache-sensitive new subsystem — more
  work and more risk than gating the existing transform.
- **"Transform runs in a reduced mode" — RECOMMENDED.** Keep registration; gate off every pruning
  stage (§1.3); keep additive memory/docs injection + FTS feed (§1.2). Model it on the existing
  `reducedMode`/`fullFeatureMode` split (`transform.ts:681-682`) by adding a third speed, e.g.
  `compactionOffMode`, that implies `fullFeatureMode=false` for all pruning gates but keeps the
  m[0]/m[1] memory path (which today is gated on `fullFeatureMode` and must be re-gated onto
  `memory.enabled && compactionOff`). The zero-compartment path then delivers memory with no trim.

**Pi parity:** Pi's pipeline mirrors this structure (`registerPiContextHandler`,
`packages/pi-plugin/src/context-handler.ts:1943`; scheduler decision, `injectM0M1Pi`,
`heuristic-cleanup-pi` all present — see imports at `context-handler.ts:190-194`). The same
gates must be applied there.

---

## 2. COMPACTION-ADJACENT MACHINERY (must not fire / must tolerate absence)

| Machinery | Compaction-off behavior | Evidence |
|---|---|---|
| **OpenCode `opencode.jsonc` `compaction.auto`** | **Must stay untouched (NOT forced false).** Setup/doctor currently write `compaction.auto=false` + `prune=false`; in compaction-off mode they must skip that write (or offer to restore `auto:true`). | `setup-opencode.ts:84-88`. |
| **Conflict detector** | **Must NOT treat `compaction.auto=true` as a conflict when compaction is off** — otherwise MC disables itself (`pluginConfig.enabled=false`) exactly when the user wants memory+native-compaction. This is the single most load-bearing change. | `conflict-detector.ts:47-104` (`compactionAuto` at `:60-63`; OpenCode default `auto:true` at `:122`); disable side `index.ts:147-155`. |
| **MC compaction markers in `opencode.db`** | **Must be removed on flip-off, and not written while off.** A marker makes OpenCode's `filterCompacted` hide pre-boundary messages; with MC no longer injecting `<session-history>`, hidden history would be orphaned (history loss). | Marker = 3 rows in opencode.db (`compaction-marker.ts:484-527`); consumed by `filterCompacted` (header `:1-28`); removal helper `removeCompactionMarker` (`:671`). |
| **Pi `session_before_compact` cancel** | **Must NOT cancel** — let Pi compact natively. Currently ALWAYS returns `{cancel:true}`. | `pi-plugin/src/index.ts:1886-1897`; also the fail-closed surface `fail-closed-pi.ts:66-68`. |
| **Overflow detection + emergency paths** | **Disarm.** `session.error`→`detectOverflow` arms `needs_emergency_recovery`; the 95% block + tiered drops consume it. With compaction off, do not arm recovery (or ignore it) and let the overflow propagate to native compaction. Subagent path already records-limit-only (`event-handler.ts:300-330`) — a good template. | `event-handler.ts:290-338` (`recordOverflowDetected` at `:333`); latch read/write `storage-meta-persisted.ts:1624-1712`; 95% block `transform-compartment-phase.ts:324-329`; emergency drop `hooks/magic-context/emergency-drop.ts`. |
| **`/ctx-wrapup`, `/ctx-recomp`, `/ctx-flush`, `/ctx-session-upgrade`** | **Refuse with a clear message** (they are compaction operations). Note `/ctx-wrapup` & `/ctx-recomp` & upgrade are ALREADY gated on `historianRunnable` (`hook.ts:1117-1127`) — so `historian.disable` precedent covers them; compaction-off should imply the same. `/ctx-flush` (drain pending ops) is meaningless with no drops. | Command defs `builtin-commands/commands.ts:3-41`; handler guards `command-handler.ts` flush `:661-689`, wrapup `:728-762`, recomp `:764-883`; historian gating `hook.ts:1117-1127`. |
| **TUI sidebar** | Shows `usagePercentage` from `session_meta.last_context_percentage` + `executeThreshold`. With compaction off it should show a "compaction off / native" state instead of a threshold MC will never enforce. | RPC `sidebar-snapshot`/`status-detail` (`tui/data/context-db.ts:146,209`), fields from `session_meta` (`plugin/rpc-handlers.ts`), render `tui/slots/sidebar-content.tsx:761`, `tui/index.tsx:225-228`. |
| **Historian + compartment storage** | **Dormant but tables exist — fine.** No schema change; historian simply never triggered, compartments stay empty. | trigger gate `transform.ts:1459`. |
| **Boot/config plumbing** | Minimal: `buildMagicContextHookConfig` spreads the WHOLE plugin config (`create-session-hooks.ts:27-32`), so a new field flows to the hook automatically; only the Zod schema + a resolver need the field. | `create-session-hooks.ts:18-33`. |

**Critical footgun — "nothing compacts":** compaction-off only makes sense if the HARNESS will
compact. If a user sets `compaction.enabled=false` while their `opencode.jsonc` still carries the
`compaction.auto=false` that MC setup wrote earlier (`setup-opencode.ts:84-88`), then MC won't
compact AND OpenCode won't either ⇒ the window grows unbounded until a provider overflow error.
**Required:** when compaction-off is configured, `doctor` must detect `compaction.auto=false` and
warn/offer to restore native compaction (set `auto:true` or remove the override). The setup wizard
must not write `compaction.auto=false` in this mode. Without this guard the feature's failure mode is
a confusing hard overflow, not a graceful degradation.

---

## 3. DREAMER + MEMORY LAYER independence

**Dreamer tasks** (12 canonical, `features/magic-context/dreamer/task-registry.ts`; schema
`DreamTasksSchema` `config/schema/magic-context.ts:144-185`). Per-task data source and
zero-compartments impact:

| Task | Reads | Zero-compartments impact |
|---|---|---|
| `retrospective` | Raw session transcripts (`dreamer/retrospective-raw-provider.ts`) + `compartment_events` | **Partial:** raw-message scan works; the "corroborating historian events" section goes empty. Still runs. |
| `curate`, `classify-memories`, `verify`, `verify-broad`, `map-memories`, `compress-cues` | `memories` table | **None** — independent of compartments. |
| `evaluate-smart-notes` | `smart_notes`/`notes` tables | **None.** |
| `review-user-memories` | `user_memories` table | **None.** |
| `promote-primers` | `primer_candidates`/`primers` | **None.** |
| `refresh-primers` | `primers` + raw sessions | **None** (falls back to closed-book orientation). |
| `maintain-docs` | filesystem docs + git log | **None.** |

**Conclusion:** the dreamer is effectively independent of the compaction pipeline. Only
`retrospective` loses an optional corroboration section. **However**, one dependency to flag:
`promote-primers`/`refresh-primers` and memory promotion (`memory.auto_promote`,
`config/schema/magic-context.ts:826-831`) can draw on historian-produced facts in some flows; with
zero compartments those specific promotion candidates simply won't appear — a graceful no-op, not a
crash. **Dreamer keeps working with compaction off.**

**ctx_search sources** (`features/magic-context/search.ts`; `SearchSource` type `:78`, default set
`:1501`):

| Source | Depends on compaction/historian? | Compaction-off behavior |
|---|---|---|
| `memory` | No (reads `memories`) | **Keeps working.** |
| `message` | **No** — FTS built from RAW messages (event-driven, §1.2) | **Keeps working.** |
| `compartment` (internal) | **Yes** (`compartment_chunk_embeddings`) | **Goes empty** (0 compartments). |
| `git_commit` | No (opt-in `memory.git_commit_indexing`) | Keeps working if enabled. |
| `primer` | No | Keeps working. |
| `note` | No | Keeps working. |

**Conclusion:** only the `compartment` chunk source goes empty; memories, raw-message FTS, git
commits, primers, and notes all survive. **Search stays useful with compaction off.**

---

## 4. MODE TRANSITIONS

**Flip OFF mid-project (existing compartments + markers in DB):**
- Compartments remain in `context.db` but go dormant (historian gated off). Dreamer/search that read
  memories/FTS keep working (§3).
- **OpenCode compaction markers are the hazard.** A previously-written marker makes `filterCompacted`
  hide pre-boundary messages; with MC no longer rendering `<session-history>` to replace them, that
  history is silently lost from the model's view. **Required:** on entering compaction-off mode,
  remove MC's markers for active sessions (`removeCompactionMarker`, `compaction-marker.ts:671`) and
  stop writing new ones. After removal, native compaction sees the full raw tail.
- Emergency latch: if `needs_emergency_recovery` was armed before flip-off, it must be cleared
   (`clearEmergencyRecovery`, `storage-meta-persisted.ts:1736-1752`) so a stale latch doesn't later
  force MC pruning.

**Flip back ON (huge unsummarized gap):**
- The historian trigger measures the live tail and fires when it crosses the threshold
  (`transform.ts:1459-1496`). A large gap means a large tail; the **incremental historian runner
  processes bounded chunks** (`compartment-runner-incremental.ts`) and `/ctx-wrapup` provides a manual
  sequential token-capped chunk loop (`wrapup-orchestrator.ts`). So the cold path exists and chunks
  the backlog rather than overflowing on it.
- **Latch cold path — verified:** `isEmergencyRecoveryArmed` is an in-memory `Set`
  (`storage-meta-persisted.ts:10-12`), but the durable state is `session_meta.needs_emergency_recovery`
  (`:1624-1673`), which survives restart. It is cleared on model change, successful recovery,
  historian publish, wrapup, or recomp (`hook-handlers.ts:347`, `transform.ts:849,2048`,
  `compartment-runner-incremental.ts:306,365,706`, `wrapup-orchestrator.ts:513`,
  `recomp-orchestrator.ts:328`). **No boot-time sweep** clears a stale persisted latch — so a flip
  back on should explicitly re-evaluate/clear it, or rely on the first successful historian publish.
  Recommend an explicit clear on the off→on transition to be safe.
- **Recommendation:** on flip-back, suggest `/ctx-wrapup` (or auto-run a chunked wrapup) to digest the
  gap predictably instead of waiting for threshold-triggered historian runs.

**Existing markers under native compaction:** tolerated by `filterCompacted` (they just mark a
boundary), but as above they hide history without replacement once MC stops injecting — so remove them
on flip-off rather than relying on tolerance.

---

## 5. CONFIG SHAPE + TIER

**Proposed Zod addition** (in `MagicContextConfigSchema`, alongside `memory`, `system_prompt_injection`):

```ts
compaction: z
    .object({
        enabled: z
            .boolean()
            .default(true)
            .describe(
                "When false, Magic Context stops managing the context window (no tool-output " +
                "pruning, no historian/compartments, no nudges, no auto-compact) and keeps only " +
                "the knowledge layer: memory injection, ctx_search, dreamer. Native harness " +
                "compaction (or nothing) owns the window. Default true.",
            ),
    })
    .default({ enabled: true }),
```

Add the matching field to the `MagicContextConfig` interface
(`config/schema/magic-context.ts:384-528`). Regenerate `assets/magic-context.schema.json` via
`packages/plugin/scripts/build-schema.ts`.

**Which tier may set it — recommend USER-tier only (strip from project).**
- Project-tier safety question: can a cloned repo turning compaction OFF hurt the user? It is not an
  RCE/cost/exfil vector (those are what `stripUnsafeProjectConfigFields` targets,
  `config/project-security.ts:214-316`). It is closest in spirit to the compaction thresholds, which
  a repo may only **raise** (delay compaction) — `constrainProjectThresholdOverrides`,
  `project-security.ts:318-474`. Turning compaction OFF is "delay compaction forever," which is
  directionally consistent with that allow-list.
- **However**, compaction-off silently changes how the user's window is managed AND interacts with
  OpenCode's own `compaction.auto` state and with conflict detection — a cloned repo flipping that
  without consent is surprising and can push sessions toward native-compaction/overflow behavior the
  user didn't choose. Precedent for user-only behavior flags exists (`auto_update`,
  `fail_closed_blocking`, `language`, `sqlite` are all stripped, `project-security.ts:217-244`).
- **Recommendation:** add `compaction` to `stripUnsafeProjectConfigFields` (user-tier only) for the
  initial implementation. If product later wants per-repo opt-in, allow project-tier `enabled:false`
  only as the "delay" direction with an explicit warning — but that is a follow-up, not v1.

**Interaction with existing flags:**
- **`enabled` (master switch):** compaction-off is orthogonal; `enabled:false` still wins (whole
  plugin off). Compaction-off only makes sense when `enabled:true`.
- **`memory.enabled`:** still controls whether memories inject. Compaction-off + `memory.enabled:false`
  ⇒ transform is nearly a pure passthrough (docs/key-files may still ride m[0]/m[1] if left on). The
  `m0M1.projectPath` gate already honors `memory.enabled` (`transform.ts:1361-1363`,
  `:1986-1993` comment).
- **`fail_closed_blocking`:** with compaction off, fail-closed blocking **stops making sense** — its
  purpose is to prevent silent fallthrough to native compaction (`messages-transform.ts:50-78`,
  `config/schema/magic-context.ts:447-453`), but fallthrough-to-native is now the DESIRED behavior.
  **Recommend:** force `fail_closed_blocking` ineffective in compaction-off mode (storage failure
  degrades to "knowledge layer off, native compaction runs" instead of blocking the turn). Memory
  writes (`ctx_memory`) still need storage, but a storage failure should fail-open, not block.
- **`historian.disable`:** compaction-off should imply it (no historian). Already wired to gate
  wrapup/recomp/upgrade (`hook.ts:1117-1127`).
- **`smart_drops`, `caveman_text_compression`:** both become inert (they are pruning). No change
  needed beyond the gates in §1.3.

---

## 6. RUST MODE

`transform_mode:"rust"` hands the ENTIRE pipeline to the subc module
(`transform.ts:668-675`; Rust side `crates/mc-module/src/transform.rs`, `historian.rs`,
`injection.rs`, `boundary.rs`). The Rust module implements tagging, drops, m[0]/m[1] injection, and
historian scheduling as one unit — there is no TS-side gate to flip.

**Cleanest rule: rust mode implies compaction ON.** When `compaction.enabled=false`, resolve the
transform mode to `"ts"` with a config warning (mirroring the existing downgrade pattern in
`resolveTransformMode`, `config/transform-mode.ts:11-23`, which already downgrades rust→ts when the
user-tier `subc` prerequisite is missing). The reduced TS transform is the compaction-off
implementation; duplicating the gating inside the Rust module is a second, large, parallel effort not
worth doing for v1.

- **Recommended:** `resolveTransformMode` returns `{ mode:"ts", warnings:["compaction disabled — rust
  transform requires compaction; running ts"] }` when compaction is off.
- **Alternative (also acceptable):** reject the combination outright with a clear config error. The
  downgrade-with-warning is friendlier and still honors the user's compaction-off intent.

---

## 7. Cleanest-path recommendation — what the agent SEES

- **Guidance section changes:** yes — use the existing no-`ctx_reduce` variant
  (`magic-context-prompt.ts:178-179`, `BASE_INTRO_NO_REDUCE`) so the prompt describes memory/search/
  notes but not §N§ tags or reduce mechanics. Keep `system_prompt_injection` honoring its existing
  escape hatches (`system-prompt-hash.ts:266-278`).
- **Tools list changes:** yes — **unregister `ctx_reduce` and `ctx_expand`** (both are compaction
  tools; `ctx_expand` expands dropped content, pointless with no drops). **Keep `ctx_search`,
  `ctx_memory`, `ctx_note`.** Tool registry is `plugin/tool-registry.ts` (see `dreamerEnabled` wiring
  at `tool-registry.ts:90` for the conditional-registration precedent). The issue explicitly asks to
  "not install the tools for compaction."
- **§N§ tag prefixes:** not injected (tagging is gated off with drops). `ctx_search`'s `message`
  source references message ordinals, not tags, so search is unaffected.
- **Nudges:** gone (Channel 1/2 are gated on `ctxReduceCallable`, `transform.ts:2254-2372`, which
  resolves false once `ctx_reduce` is unregistered).
- **Net agent experience:** memory + docs injected additively at session start; `ctx_search` /
  `ctx_memory` / `ctx_note` available; no pruning, no tag prefixes, no reduce nudges; OpenCode (or
  nothing) compacts the window. Exactly the issue's ask.

---

## 8. Implementation sequencing (dependency order)

1. **Config:** schema field + interface + JSON-schema regen + resolver helper
   (`isCompactionEnabled(config)`), user-tier strip in `project-security.ts`.
2. **Conflict detector:** skip `compactionAuto`/`compactionPrune` conflicts when compaction off
   (`conflict-detector.ts`), so MC stays loaded with native compaction available.
3. **Setup/doctor:** don't write `compaction.auto=false` when compaction off (`setup-opencode.ts`);
   offer to restore native compaction.
4. **Transform gating:** introduce `compactionOffMode`; gate off historian trigger, scheduler drops,
   heuristics, force/emergency materialization, 95% block, nudges, strips, synthetic-todo, caveman;
   KEEP m[0]/m[1] memory injection (re-gate onto `memory.enabled && compactionOff`) + FTS feed.
5. **Tools + guidance:** unregister `ctx_reduce`/`ctx_expand`; force no-reduce guidance variant.
6. **Machinery disarm:** overflow/emergency latch no-arm + clear; `/ctx-*` compaction commands refuse;
   remove MC compaction markers on flip-off; TUI sidebar "compaction off" state.
7. **Pi parity:** stop cancelling `session_before_compact` (`index.ts:1886`, `fail-closed-pi.ts:66`);
   mirror transform gating in `context-handler.ts`.
8. **Rust mode:** downgrade to ts with warning when compaction off (`transform-mode.ts`).
9. **Tests:** co-located `*.test.ts` for each gate + an e2e scenario in `packages/e2e-tests/`.

---

## 9. Risk list (ranked by breakage likelihood)

1. **HIGH — Conflict detector disables MC when native compaction is on.** If step 2 is missed or
   ordered after step 4, compaction-off users get ZERO MC features (plugin `enabled=false`), the exact
   opposite of intent. (`conflict-detector.ts:60-63`, `index.ts:147-155`.)
2. **HIGH — Orphaned compaction markers hide history.** Forgetting marker cleanup on flip-off silently
   drops pre-boundary messages with no `<session-history>` replacement. (`compaction-marker.ts:1-28,
   671`.)
3. **HIGH — Memory injection accidentally gated off.** The m[0]/m[1] path is currently gated on
   `fullFeatureMode`; re-gating it incorrectly (or letting `compactionOffMode` imply
   `fullFeatureMode=false` too broadly) kills memory delivery — the core feature. Needs a dedicated
   test that memory injects with compaction off. (`transform-postprocess-phase.ts:1184`,
   `transform.ts:1985`.)
4. **MEDIUM — Pi `session_before_compact` still cancels.** If missed, Pi users get NO compaction at
   all (MC won't, Pi is cancelled) → unbounded context → overflow. (`pi-plugin/src/index.ts:1886`.)
5. **MEDIUM — Stale emergency latch forces pruning after flip.** A persisted
   `needs_emergency_recovery` surviving into compaction-off could trigger MC pruning or block turns.
   (`storage-meta-persisted.ts:1624-1712`.)
6. **MEDIUM — Setup/doctor keeps forcing `compaction.auto=false`.** Leaves native compaction disabled
   so nothing compacts. (`setup-opencode.ts:84-88`.)
7. **LOW — Guidance/tools mismatch.** Leaving `ctx_reduce` registered or reduce guidance on confuses
   the agent into calling a no-op tool. (`tool-registry.ts`, `magic-context-prompt.ts`.)
8. **LOW — Rust-mode users surprised.** Without the downgrade, rust mode ignores compaction-off.
   (`transform-mode.ts`.)
9. **LOW — Flip-back backlog.** Large unsummarized gap may need multiple historian/wrapup passes; not a
   crash, but UX. Mitigate with a `/ctx-wrapup` suggestion (§4).

---

## 10. Build-effort estimate (mason-slices)

**~5–7 slices**, each independently verifiable:

1. **Config + tier + resolver** (schema, interface, JSON-schema regen, `isCompactionEnabled`,
   project-tier strip, unit tests). — 1 slice.
2. **Conflict detector + setup/doctor** (don't-conflict + don't-force-`compaction.auto`, tests). — 1 slice.
3. **OpenCode transform gating** (`compactionOffMode`, gate off §1.3 stages, keep memory+FTS, tests
   incl. "memory still injects"). — 1–2 slices (largest).
4. **Tools + guidance** (unregister `ctx_reduce`/`ctx_expand`, no-reduce guidance). — 1 slice.
5. **Machinery disarm** (overflow latch, `/ctx-*` refuse, marker cleanup, TUI state). — 1 slice.
6. **Pi parity** (`session_before_compact`, context-handler gates). — 1 slice.
7. **Rust downgrade + e2e** (resolveTransformMode warning, e2e scenario). — 1 slice.

The memory path already degrades correctly (zero-compartment injection), and subsystem-toggle
precedent exists (`agent-disable.ts`), which is why this is a BUILD despite the coupling count.

---

## Appendix — key file map

| Area | File(s) |
|---|---|
| Plugin entry / hook registration | `packages/plugin/src/index.ts` |
| Transform outer wrapper | `packages/plugin/src/plugin/messages-transform.ts` |
| Session-hook construction | `packages/plugin/src/plugin/hooks/create-session-hooks.ts` |
| Core transform | `packages/plugin/src/hooks/magic-context/transform.ts` |
| Post-transform phase (drops, m0/m1) | `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts` |
| Compartment/historian phase (95% block) | `packages/plugin/src/hooks/magic-context/transform-compartment-phase.ts` |
| Scheduler decision | `packages/plugin/src/hooks/magic-context/transform-context-state.ts` |
| Memory/compartment injection | `packages/plugin/src/hooks/magic-context/inject-compartments.ts` |
| System-prompt injection | `packages/plugin/src/hooks/magic-context/system-prompt-hash.ts` |
| Guidance text | `packages/plugin/src/agents/magic-context-prompt.ts` |
| Message-index FTS | `packages/plugin/src/features/magic-context/message-index.ts`, `message-index-async.ts` |
| Conflict detector | `packages/plugin/src/shared/conflict-detector.ts` |
| Compaction markers | `packages/plugin/src/features/magic-context/compaction-marker.ts` |
| Overflow/emergency latch | `packages/plugin/src/features/magic-context/storage-meta-persisted.ts`, `event-handler.ts` |
| Config schema | `packages/plugin/src/config/schema/magic-context.ts` |
| Project-tier security | `packages/plugin/src/config/project-security.ts` |
| Subsystem toggles | `packages/plugin/src/config/agent-disable.ts` |
| Transform-mode resolver | `packages/plugin/src/config/transform-mode.ts` |
| Setup wizard (opencode.jsonc) | `packages/cli/src/commands/setup-opencode.ts` |
| Built-in commands | `packages/plugin/src/features/builtin-commands/commands.ts`, `hooks/magic-context/command-handler.ts` |
| Pi entry / hooks | `packages/pi-plugin/src/index.ts` |
| Pi context transform | `packages/pi-plugin/src/context-handler.ts` |
| Pi fail-closed | `packages/pi-plugin/src/fail-closed-pi.ts` |
| Dreamer tasks | `packages/plugin/src/features/magic-context/dreamer/` |
| Search | `packages/plugin/src/features/magic-context/search.ts` |
| Rust transform | `crates/mc-module/src/transform.rs` (+ `historian.rs`, `injection.rs`, `boundary.rs`) |
