# Claude Code parity gaps against OpenCode / Pi

## Scope and counting

OpenCode's shipped behavior is treated as the specification. Pi is included where it has the same shipped surface or where its harness encoding necessarily differs. This audit followed the serializer profile through `healing.rs`, `transform.rs`, the harness codecs, selection, injection, the OpenCode/Pi host legs, prompt-surface registration, and every feature named in the `ARCHITECTURE.md` session-mode table.

**Profile-dependent branches examined: 18 effective production decision sites.** This count excludes parser/string-to-enum mapping, tests, and debug-only assertions. The ledger is at the end of this report. Harness codec modules were also reviewed, but they are separate entry points rather than runtime `SerializerProfile` branches and are not included in 18.

A source statement saying that another component owns behavior is not treated as proof that the behavior exists on Claude Code. Such cases are called out under **INFERRED / EXTERNALLY OWNED**.

# INCIDENTAL DIFFERENCES

These can be implemented on the Claude Code leg without changing the harness's fundamental control surface.

## I-01 — Claude Code delays first-pass tag injection on a new session

- **OpenCode/Pi:** OpenCode is explicitly allowed to activate tagging during the bootstrap pass, so the first served response can contain `§N§` prefixes. Pi's host tagger likewise tags the first eligible context pass.
- **Claude Code:** `bootstrap_tagging_active` is restricted to `OpencodeAiSdk`; Claude Code first commits the surface latch and only becomes eligible to render tags on a later pass.
- **Source:** `crates/mc-module/src/transform.rs:2423-2430`; profile eligibility is defined at `crates/mc-module/src/lib.rs:373-382`.
- **Classification:** incidental — this is one profile gate.

## I-02 — The Claude Code auto-search switch is coupled to tagging and cannot honor `memory.auto_search`

- **OpenCode/Pi:** auto-search is independently configurable (`enabled`, `score_threshold`, and `min_prompt_chars`) and runs only when enabled: `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1841-1863`; Pi has the same gate at `packages/pi-plugin/src/context-handler.ts:2977-3001`.
- **Claude Code:** a user hint is computed whenever the tagging overlay is enabled; there is no `auto_search` field in `McModuleConfig` and no request field carrying the three controls. Disabling OpenCode/Pi auto-search therefore has no Claude Code equivalent, while losing `ctx_reduce` availability also disables Claude Code auto-search.
- **Source:** `crates/mc-module/src/transform.rs:7042-7079`; `crates/mc-module/src/config.rs:44-67`; `crates/mc-module/src/transform.rs:582-611`.
- **Classification:** incidental — missing config and request plumbing.

## I-03 — Claude Code auto-search uses a different corpus and ranker

- **OpenCode/Pi:** unified search can use semantic embeddings and searches hidden memories, raw/compacted messages, and git commits; it filters already-visible memory IDs and applies the configured top-score threshold: `packages/plugin/src/hooks/magic-context/auto-search-runner.ts:311-381` and `packages/pi-plugin/src/context-handler.ts:2977-3001`.
- **Claude Code:** performs a synchronous lexical overlap search over visible memory candidates and compartment bodies only. It requires two matched lexical tokens plus a rare-token and normalized-score test. It has no raw-message source, git-commit source, embedding path, visible-memory exclusion, or configured score threshold.
- **Source:** `crates/mc-module/src/transform.rs:7117-7253`, especially candidate construction at `7137-7185` and scoring at `7191-7237`.
- **Classification:** incidental — an unported search implementation.

## I-04 — Claude Code auto-search sanitization and hint bytes differ

- **OpenCode/Pi:** removes nested reminders, HTML comments, generic XML/HTML tags, and `§N§` prefixes before search; renders up to three caveman-ultra fragments at 80 characters each, source-aware commit metadata, singular/plural headers, and an 800-character total cap: `packages/plugin/src/hooks/magic-context/auto-search-runner.ts:177-220`; `packages/plugin/src/hooks/magic-context/auto-search-hint.ts:25-27,55-85,96-130`.
- **Claude Code:** removes only `<system-reminder>` wrappers and `§N§` notation, leaving other markup/comments in the query. It renders source-agnostic one-line snippets at 100 characters each, a fixed header/footer, and a 600-character debug-only cap.
- **Source:** `crates/mc-module/src/transform.rs:7255-7335,7337-7360`; limits at `crates/mc-module/src/transform.rs:101-108`.
- **Classification:** incidental — byte and preprocessing differences in the native implementation.

## I-05 — `ctx_search` is literal and exposes a smaller API on Claude Code

- **OpenCode/Pi:** the public schema accepts optional `query`, `limit`, and `sources`, with `memory`, `message`, `git_commit`, `primer`, and `note`; unified search may be semantic and supports source targeting: `packages/plugin/src/tools/ctx-search/tools.ts:163-177`.
- **Claude Code:** requires a non-empty query, defaults to 8 results, offers no `sources` argument, and searches only memories, notes, and compartment title/body rows. It cannot retrieve git commits, primers, or raw message hits.
- **Source:** schema at `crates/mc-module/src/lib.rs:12357-12376`; execution at `crates/mc-module/src/lib.rs:9076-9133`; the leg-specific light description explicitly says “literal, not semantic” at `crates/mc-module/src/prompt_surface.rs:32-34`.
- **Classification:** incidental — facade/schema and search backend gaps.

## I-06 — `ctx_expand` lacks OpenCode/Pi's verbose range mode

- **OpenCode/Pi:** accepts `verbose=true` for per-message/per-part previews before selecting a full single-message expansion: `packages/plugin/src/tools/ctx-expand/tools.ts:20-40,47-59`.
- **Claude Code:** advertises only `start`, `end`, and `message`; the facade ignores `verbose` and always uses its one range renderer.
- **Source:** `crates/mc-module/src/lib.rs:12378-12388`; `crates/mc-module/src/lib.rs:9136-9217`.
- **Classification:** incidental — an omitted argument and renderer.

## I-07 — `ctx_reduce` acknowledgement and validation are observably weaker on Claude Code

- **OpenCode/Pi:** the tool call reports invalid range syntax, unknown tags, pre-compaction conflicts, already-queued tags, and immediate versus protected/deferred drops; see `packages/plugin/src/tools/ctx-reduce/tools.ts:75-210` (and Pi's matching implementation).
- **Claude Code:** the MCP-facing facade always returns `Queued for context compaction.` without validating or storing anything; a later response observer owns delivery. The module delivery method silently filters unknown tag numbers and can commit a zero-target command rather than report the unknown IDs.
- **Source:** acknowledgement-only facade at `crates/mc-module/src/lib.rs:8812-8818`; delayed validation/filtering at `crates/mc-module/src/lib.rs:4460-4537`.
- **Classification:** incidental — the observer can return the same validation result before acknowledging.

## I-08 — Claude Code does not run OpenCode/Pi's duplicate-tool heuristic

- **OpenCode/Pi:** on heuristic passes, older same-owner duplicate calls for a safe tool/argument fingerprint are dropped while the newest is kept: `packages/plugin/src/hooks/magic-context/heuristic-cleanup.ts:202-258`; Pi mirrors this in `packages/pi-plugin/src/heuristic-cleanup-pi.ts`.
- **Claude Code:** native selection implements two-pass age reclaim, emergency tiering, agent drops, and optional supersession, but has no safe-tool fingerprint/dedup candidate lane.
- **Source:** the complete native lane composition is `crates/mc-module/src/selection.rs:939-1063`; no duplicate fingerprint lane exists there. OpenCode's heuristic phases are `packages/plugin/src/hooks/magic-context/heuristic-cleanup.ts:90-200,202-258`.
- **Classification:** incidental — one heuristic lane is unported.

## I-09 — Historical reasoning clearing is OpenCode-only in the native transform

- **OpenCode/Pi:** OpenCode clears reasoning older than `clear_reasoning_age` on bust passes and replays the persisted watermark; Pi performs and replays the same operation: `packages/pi-plugin/src/context-handler.ts:4817-4866,5039-5109`.
- **Claude Code:** the native cutoff is explicitly gated to `OpencodeAiSdk`, so Claude Code preserves the newest historical assistant's signed reasoning and never advances this clearing watermark. `clear_reasoning_age` is also absent from `McModuleConfig`, so Claude Code cannot configure it through its own config path.
- **Source:** `crates/mc-module/src/transform.rs:9664-9694,9725-9740`; `crates/mc-module/src/config.rs:44-67`.
- **Classification:** incidental — the full-array Claude Code gateway can implement a safe historical-reasoning strip and replay contract.

## I-10 — Covered system-message placement and fold timing differ

- **OpenCode/Pi:** only system messages inside the current compartment coverage interval are copied into m0; an older uncovered leading system message remains a separate leading message.
- **Claude Code:** every covered system message before the coverage end is copied into m0 regardless of coverage start, any coverage advance over a system message forces a HARD, and covered leading system messages are never separately re-emitted.
- **Source:** force-HARD gate at `crates/mc-module/src/transform.rs:2987-3005`; m0 collection at `crates/mc-module/src/transform.rs:5153-5178`; separate-message gate at `crates/mc-module/src/transform.rs:9321-9339`.
- **Classification:** incidental — the gateway can project CK system items into Anthropic's top-level `system` field while preserving the OpenCode/Pi interval and timing.

## I-11 — Claude Code's cache-lifetime policy differs from OpenCode/Pi

- **OpenCode/Pi:** use their host-resolved cache TTL as the scheduler assumption; host behavior may also keep the provider cache warm.
- **Claude Code:** default-provenance TTL is forced to `1h`; explicit finite values above one hour are clamped to `1h`; subagents are forced to `5m`. Provider marker output is emitted only for an explicit Claude Code model match, with arbitrary configured values collapsed to Anthropic's `5m|1h` vocabulary.
- **Source:** `crates/mc-module/src/transform.rs:1692-1767`; provenance resolution at `crates/mc-module/src/config.rs:101-151` and binding use at `crates/mc-module/src/lib.rs:6658-6694`.
- **Classification:** incidental as a scheduling policy difference. The wire location used to express the marker is structural and is listed below.

## I-12 — Claude Code does not perform OpenCode/Pi's decay-pressure retry — **FIXED (2026-08-13, magic-context a9a1f121)**

> Ported the exact TS contract into `render_m0_with_decay_pressure_retry` (strict >1.05 gate on the history slice, ×1.15, max 3, keep-final), with a TS-generated differential fixture asserting retry count, tier demotions, and full m0 sha256 byte-identity.

- **OpenCode/Pi:** after an m0 render, if rendered session history exceeds 105% of budget, retries up to three times, multiplying decay pressure by 1.15 each time: `packages/plugin/src/hooks/magic-context/inject-compartments.ts:2181-2212` (Pi uses the same shared renderer contract).
- **Claude Code:** always calls the shared Rust renderer once with `decay_pressure_multiplier: 1.0`.
- **Source:** `crates/mc-module/src/m0_compose.rs:352-377`.
- **Classification:** incidental — missing bounded retry loop.

## I-13 — Mural is not live on Claude Code

- **OpenCode/Pi:** a HARD can resolve a deterministic memory mural and attach its image to synthetic m0 when the selected model supports vision: `packages/plugin/src/hooks/magic-context/inject-compartments.ts:1950-1996,2174-2180,2870-2909`; Pi wires the same feature in `packages/pi-plugin/src/inject-compartments-pi.ts:35-36,379-383`.
- **Claude Code:** m0 inputs contain docs, profile, compartments, memories, and budgets only; neither `McModuleConfig` nor `M0ComposeInputs` has a mural enablement/image field, and no CK media block is added to synthetic m0. Mural-cue columns may be state-synced, but they are never rendered.
- **Source:** `crates/mc-module/src/m0_compose.rs:280-391`; config surface `crates/mc-module/src/config.rs:44-67`.
- **Classification:** incidental — Anthropic supports image content and CK already has media carriers.

## I-14 — Caveman exists in Rust but is not live through the Claude Code config/binding path

- **OpenCode/Pi:** resolve `caveman_text_compression`, send the enablement and minimum size into the transform, persist tier depth, and replay compression. OpenCode's native request wiring is `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:1638-1663`; Pi replay is `packages/pi-plugin/src/context-handler.ts:4869-4907`.
- **Claude Code:** Rust has caveman selection/replay when `TransformRequest.caveman_enabled` is true, but the Claude Code-owned config has no caveman field and the binding does not derive one. The serde default is false, so a normal CC request that omits the unowned field never enables it.
- **Source:** request fields at `crates/mc-module/src/transform.rs:582-587`; native selection use at `crates/mc-module/src/transform.rs:3366-3385,4870-4900`; absent config surface at `crates/mc-module/src/config.rs:44-67`.
- **Classification:** incidental — missing CC config/request transport.

## I-15 — Runtime guidance is less feature-aware on Claude Code

- **OpenCode/Pi:** guidance composition dynamically removes memory guidance when memory is disabled and appends temporal-awareness guidance, the caveman anti-mimic warning, smart-note guidance when Dreamer is runnable, subagent-only guidance, and a language directive: `packages/plugin/src/agents/magic-context-prompt.ts:177-232`.
- **Claude Code:** selects one static full/no-reduce asset from tool presence and appends only language and date. Its full assets always discuss memory/search; they contain neither the temporal clause nor the caveman warning, and there is no subagent-specific composer branch.
- **Source:** `crates/mc-module/src/lib.rs:6213-6275`; static asset selection at `crates/mc-module/src/prompt_surface.rs:106-125`; assets at `crates/mc-module/assets/guidance_primary.txt` and `guidance_no_reduce.txt`.
- **Classification:** incidental — runtime booleans are not passed to the CC composer.

## I-16 — `prompt_surface.guidance_override_path` is absent on Claude Code

- **OpenCode/Pi:** resolve and validate a user guidance file, warning and falling back for unreadable, empty, or malformed-marker files: `packages/plugin/src/shared/prompt-surface-runtime.ts:136-178,216-221`.
- **Claude Code:** prompt-surface selection accepts preset/model/config identity and tool-description overrides only; guidance always comes from compiled assets.
- **Source:** `crates/mc-module/src/lib.rs:6072-6143,6263-6275`.
- **Classification:** incidental — omitted config and file-loading path.

## I-17 — Tool-description override error behavior differs

- **OpenCode/Pi:** unknown tool IDs and empty descriptions are ignored with a warning; valid IDs are applied: `packages/plugin/src/shared/prompt-surface-runtime.ts:188-213`.
- **Claude Code:** the manifest/guidance request fails with `invalid_params` on an unknown ID, non-string description, or empty string.
- **Source:** `crates/mc-module/src/lib.rs:6108-6135`.
- **Classification:** incidental — validation policy.

## I-18 — Memory-disabled tool registration differs

- **OpenCode/Pi:** omit `ctx_memory` entirely when memory is disabled while leaving `ctx_search` available for conversation/git recall: `packages/plugin/src/plugin/tool-registry.ts:122-166`.
- **Claude Code:** the module manifest always advertises `ctx_memory`; execution later returns `Error: memory is disabled for this project.`
- **Source:** unconditional tool list at `crates/mc-module/src/prompt_surface.rs:136-188`; runtime rejection at `crates/mc-module/src/lib.rs:8843-8845`.
- **Classification:** incidental — manifest filtering is unported.

## I-19 — Smart-note evaluation and deferred-note nudges are not live on Claude Code

- **OpenCode/Pi:** accept `surface_condition` only when the Dreamer evaluator is runnable, evaluate pending smart notes, and append persisted deferred-note instructions to a later user message: OpenCode postprocess at `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:327-380`; canonical Dreamer tasks include `evaluate-smart-notes` at `packages/plugin/src/features/magic-context/dreamer/task-registry.ts:11-26`.
- **Claude Code:** the light description explicitly says `surface_condition` is recorded but not evaluated. The facade rejects a smart-note write unless an external host asserts evaluator capability, but no CC-owned evaluator/scheduler is present. State sync can import note-nudge anchors, yet the module transform does not render them.
- **Source:** `crates/mc-module/src/prompt_surface.rs:40-42`; capability gate at `crates/mc-module/src/lib.rs:9420-9428`; state-sync-only anchor ingestion at `crates/mc-module/src/lib.rs:7511-7519`.
- **Classification:** incidental — evaluator and overlay rendering are unported.

## I-20 — Compaction-off mode is absent on Claude Code

- **OpenCode/Pi:** `compaction.enabled=false` creates an additive-knowledge-only mode: no new tags, reduction tool, historian, synthetic todo, heuristic drops, emergency management, or caveman; auto-search remains live. The intended matrix is `ARCHITECTURE.md:149-168`.
- **Claude Code:** `McModuleConfig` has no `compaction.enabled` field, and the transform has no additive-only session mode; it always follows normal primary/subagent context-management semantics.
- **Source:** `crates/mc-module/src/config.rs:44-67`; primary/subagent-only request switch at `crates/mc-module/src/transform.rs:555-557`.
- **Classification:** incidental — missing mode/config plumbing.

## I-21 — Dreamer task coverage is almost entirely absent

- **OpenCode/Pi:** canonical tasks are `map-memories`, `verify`, `verify-broad`, `curate`, `compress-cues`, `classify-memories`, `retrospective`, `maintain-docs`, `evaluate-smart-notes`, `review-user-memories`, `promote-primers`, and `refresh-primers`, with per-task schedules: `packages/plugin/src/features/magic-context/dreamer/task-registry.ts:11-26`.
- **Claude Code:** accepts only the single module task constant `CLASSIFY_TASK`; any other task is rejected, and status reports `scheduled_tasks: []`.
- **Source:** `crates/mc-module/src/lib.rs:8005-8008,8182`; empty schedule surface at `crates/mc-module/src/lib.rs:12425`.
- **Classification:** incidental — missing task runners and scheduler.

## I-22 — Serializer merge residuals differ by profile — **FIXED (2026-08-13, magic-context 6fb57cb3) + drill-closed**

> **Fix shipped:** fresh full-drop selections whose removal would collapse reasoning-bearing assistants into adjacency demote to skeleton mode (tool_result separator survives); frozen decisions replay byte-identically; Reasoning and RedactedReasoning uniform; single-side conservative predicate. Firing path proven by merged fail-first fixtures at the selection+serialization layer (control-fails on pre-fix HEAD).
> **Joint drill accounting (Thalamus container, 2 rounds + crafted array):** ESTABLISHED — absence of regression on both builds (384 passes, 0 adjacency, 0 fence fires); separator on every observed skeleton; selection live (206 full drops); boundary agreement BOTH directions through the real gateway (their fence refused the exact shape with pair+indices named when the separator was absent, accepted with it present; 6 vendored production-shape fixture tests green incl. live-API-verified 400/200 pair). NOT established on the wire — the firing path (driving cannot construct it: reclaim steady-state caps arcs at 22 with the historian gone; a crafted array arrives as a pressureless first pass). Three vacuity catches recorded: the fake provider had never emitted a thinking block; skeletons inside the newest-20 window are evidence about neither build; a crafted fixture invalid in the same way as the scenario makes a broken fixture and a working fence indistinguishable — validate fixtures against the probed property before submitting.

Original reclassification entry follows.

### Superseded heading: RECLASSIFIED CORRECTNESS (Thalamus wire evidence, 2026-08-12)

> **This is a wire-correctness bug, not a cosmetic gap.** The source below is accurate but its "incidental" classification was wrong. Anthropic COMBINES consecutive same-role assistant messages into one turn at the far end, then returns HTTP 400 (`thinking blocks in the latest assistant message cannot be modified`) on a merged turn carrying reasoning blocks it did not itself produce. Two adjacent assistants each carrying a reasoning block are individually well-formed and invalid only AFTER the provider's merge — which is exactly why a source-side `merges_consecutive_assistants: false` flag reads as safe and is not. Three of four production CC 400s were this shape. Thalamus fixtures `merged_reasoning_refused.json` (400) / `merged_reasoning_accepted.json` (200) verified against the live API; their `reject_merged_reasoning_turns` fence refuses exactly the 4 the provider refused across 364 replays, zero false positives — but a fence turns a 400 into a refused turn, it does not make the array valid. **The fix is upstream and is a design fork, NOT a port of OpenCode's strip** — see the caveat below.

**FIX-SHAPE CAVEAT:** OpenCode's residual STRIPS reasoning from all-but-first merged assistant. That is invalid on CC: the provider 400 explicitly forbids MODIFYING a signed thinking block in the latest assistant, so stripping one is itself the failure. The CC-safe remedy is to never PRODUCE two adjacent reasoning-bearing assistants (prevent, or self-merge into one message preserving the blocks in order), which is the reasoning-adjacency doctrine, not the strip. Interacts with I-09: clearing old reasoning shrinks the population that can strand.

**COVERAGE PROVEN AT SOURCE (2026-08-12), correcting an optimistic peer read:** the existing `reasoning_ineligible_arc_ids` rule (selection.rs:389-393) covers ONLY the reasoning-only shape `[reasoning, tool_call]`. A compiled probe against HEAD showed `[reasoning, text, tool_call]` is NOT protected (a text block is a durable non-tool sibling, so the arc stays droppable). Two of the three production fixtures are the text-bearing shape, so HEAD prevents 1 of 3, not 3 of 3. The mechanism: dropping the arc removes the intervening tool_result (user-role), collapsing two reasoning-bearing assistants into adjacency, which Anthropic merges and 400s. Thalamus's outbound fence masks this as a refused turn rather than a user-visible 400, so it did not resurface as new. **The self-merge/skeletonize fix DOES have a wire population.** Minimal correct fix: SKELETONIZE (not fully remove) an arc whose removal would collapse two reasoning-bearing assistants into adjacency — the `[dropped §N§]` skeleton keeps the tool_result block, preserving the separator, while still reclaiming bytes. Cache-stability path: needs a plan + adversarial review before code (rule #4975). Open wire question to Thalamus: does the merge 400 require BOTH assistants to bear reasoning, or does a single reasoning block across the collapse suffice?

### Original finding (source accurate, classification superseded)

- **OpenCode:** its AI-SDK serializer can merge adjacent assistant messages, so the module strips all but the permitted reasoning block across a merged run.
- **Pi:** does not merge adjacent assistants and reports its own empty-content/autofill healing behavior.
- **Claude Code:** is marked as neither merging assistants nor needing the OpenCode residual, so consecutive assistant reasoning is left intact.
- **Source:** profile coverage at `crates/mc-module/src/healing.rs:98-117`; residual table at `crates/mc-module/src/healing.rs:144-157`; application at `crates/mc-module/src/transform.rs:9040-9065`.
- **Classification:** incidental as model-visible behavior — a CC gateway can deliberately apply the same merge/residual policy even though its native serializer does not require it.

## I-23 — Known cache-anchor count differs

- **OpenCode:** places provider cache anchors on both synthetic head messages, m0 and m1.
- **Claude Code:** the gateway places one anchor.
- **Source seam:** the module constructs both independent synthetic heads at `crates/mc-module/src/transform.rs:9212-9254`; cache-marker ownership is outside this repository.
- **Classification:** incidental — nothing prevents marking the second head.
- **Evidence status:** **inferred from the task's stated known example, not independently readable in this worktree.**

# STRUCTURAL DIFFERENCES

These cannot use the same implementation mechanism because the host/harness surface is different. Matching behavior requires a different integration.

## S-01 — Claude Code has no in-repo encode/decode codec

- **OpenCode:** decodes and re-encodes MessageV2 `info`/`parts`, preserves sidecar/native fields, can serve native messages, and collapses a synthetic todo call/result pair into one synthetic tool part: `crates/mc-module/src/codec/opencode.rs:23-25,294-308,916-948,968-1018`.
- **Pi:** maps Pi `AgentMessage` / `toolResult` entries through its own codec: `crates/mc-module/src/codec/pi.rs:582-607`.
- **Claude Code:** no Anthropic/Claude Code codec exists under `crates/mc-module/src/codec/`; the module returns CK and an external gateway owns Anthropic request encoding.
- **Source:** exported codecs are only OpenCode and Pi at `crates/mc-module/src/codec/mod.rs:1-10`.
- **Reason structural:** the host schemas are different and the CC gateway, not this module, owns the provider request.

## S-02 — Provider cache markers and system prompts occupy different wire locations

- **OpenCode/Pi:** their host integrations own model-specific cache controls and system-prompt construction.
- **Claude Code:** must express cache markers in Anthropic's `cache_control` vocabulary, with `1h` requiring the beta header, and Anthropic carries system content outside the ordinary message array.
- **Source:** the marker vocabulary/header constraint is documented in `crates/mc-module/src/transform.rs:1692-1702`; the CC prefix invariant is checked at `crates/mc-module/src/transform.rs:9590-9600`.
- **Reason structural:** exact wire placement cannot be identical across Anthropic Messages, OpenCode MessageV2, and Pi AgentMessage. Semantic parity must be implemented by the respective encoder.

## S-03 — Channel 2 has no Claude Code async synthetic-user delivery hook

- **OpenCode:** the module emits a host directive only for `OpencodeAiSdk`; the plugin claims it and calls the host's async prompt API to create a synthetic user turn.
- **Pi:** has an equivalent Pi host delivery path.
- **Claude Code:** is explicitly excluded from the host directive; no Claude Code `promptAsync`-equivalent hook exists in this repository.
- **Source:** profile gate and comment at `crates/mc-module/src/transform.rs:7541-7595`; the OpenCode delivery interface is `packages/plugin/src/hooks/magic-context/channel2-delivery.ts:57-76`.
- **Reason structural:** an out-of-band user turn requires a host lifecycle callback. CC can only match through a different gateway/session integration, not the OpenCode plugin hook.

## S-04 — Slash commands and TUI/status UI use host registration APIs absent from the CC leg

- **OpenCode:** registers `ctx-status`, `ctx-recomp`, `ctx-wrapup`, `ctx-session-upgrade`, `ctx-flush`, `ctx-aug`, `ctx-dream`, and `ctx-embed`: `packages/plugin/src/features/builtin-commands/commands.ts:5-51`.
- **Pi:** registers matching command handlers under `packages/pi-plugin/src/commands/`.
- **Claude Code:** the module has management methods for status/flush/recomp/wrapup but no host command-registration or TUI callback layer; no `.claude/commands` integration exists in this repository. `ctx-aug` and `ctx-embed` also have no CC-facing command implementation.
- **Source:** backend methods begin at `crates/mc-module/src/lib.rs:4627-4646`; OpenCode registrations are above.
- **Reason structural:** Claude Code commands must be installed/invoked through a different host mechanism; OpenCode/Pi's plugin registration API cannot be reused.

## S-05 — OpenCode's fail-closed abort/overflow control is host-owned

- **OpenCode:** can call `client.session.abort`, distinguish a provider-proven overflow origin, and abort at the 95% band when no fold materialized: `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:651-719`.
- **Claude Code:** the gateway refuses any turn the transform cannot produce an array for with HTTP 503 and a structured body — never a passthrough of the harness's own array (`thalamus/crates/thalamus-core/src/proxy.rs`, `transform_unavailable`; covers Unavailable, RawOnlyFence, and provider 400/422 on a rewritten body). Claude Code retries a 503 with backoff (~12 attempts / ~180s) and renders the body beside the retry countdown, so a transform outage inside that window costs no turn. Pinned by Thalamus's container test `e2e/assert_refusal_is_retried.py` (real harness binary against a fake provider). 503 was chosen from measured harness behavior: it is the code CC treats as retryable; a 500 surfaces immediately.
- **Source:** scheduler emergency decision at `crates/mc-module/src/scheduler.rs:689-769`; gateway refusal in the thalamus repository as above.
- **Reason structural (narrowed):** both legs are fail-closed and neither serves an un-shrunk conversation; the difference is WHOSE LIFECYCLE OWNS THE RETRY — the OpenCode plugin owns the session and aborts it, the CC gateway owns the request and refuses it. The mechanism cannot be shared; the property is equivalent.
- **Evidence status:** cross-repository — verified by Thalamus at source and by container test (2026-08-13, pm_ad490214); not re-verified from this repository.
- **Residual gap (real, not wording):** the OpenCode abort distinguishes a PROVIDER-PROVEN OVERFLOW origin; the CC refusal has no equivalent signal — "context overflowed" and "transform unavailable" render the same sentence to the user. Closing it would need an origin discriminator in the refusal body.

# VERIFIED SAME

The following surfaces were checked and have the same effective module behavior, except where a numbered gap above narrows the statement.

- **Tail reclaim capability:** all shipping serializer profiles, including Claude Code, OpenCode, and Pi, are full-array consumers and return `true`: `crates/mc-module/src/healing.rs:120-141`. Consequently `fold_is_only_reclaim` is false for each: `crates/mc-module/src/lib.rs:3638-3642`.
- **Tag eligibility after activation:** both Claude Code and OpenCode require actual `ctx_reduce` tool presence: `crates/mc-module/src/lib.rs:373-382`. The bootstrap timing exception is I-01.
- **`ctx_reduce` queue consumption:** pending agent drops participate in the same native selection lane and are block-protected/replayed durably: `crates/mc-module/src/selection.rs:1061-1108`.
- **Channel 1 nudge:** live for activated Claude Code and OpenCode tag surfaces; it uses durable tag accounting, pressure/severity bands, refire cadence, sticky tool-result append, and replay: `crates/mc-module/src/transform.rs:7378-7438,7655-7835`.
- **Synthetic todowrite:** live for primary Claude Code/OpenCode/Pi transforms. The module captures state on bust, builds a deterministic call/result pair, and anchors/replays it: `crates/mc-module/src/injection.rs:101-116,125-190,197-220`; `crates/mc-module/src/transform.rs:4074-4083,9283-9311`.
- **Historian / compartments:** live on Claude Code primary sessions through the native historian state machine and m0/m1 compose paths; subagents skip m0/m1 as in the session-mode contract: `crates/mc-module/src/transform.rs:3408-3420,3422-3993`.
- **m0/m1 construction and memory/profile/docs budgets:** use the shared Rust composition path for every serializer profile: `crates/mc-module/src/m0_compose.rs:280-391`. The decay retry difference is I-12 and mural absence is I-13.
- **Decay curve itself:** the Rust renderer has differential/golden coverage against the TypeScript reference: `crates/mc-module/src/decay_render.rs:628-841`.
- **System-injection, stale-reduce, placeholder, and processed-image replay:** live in the native frozen-strip machinery: `crates/mc-module/src/transform.rs:8053-8159,8162-8230`. Duplicate-tool cleanup remains missing (I-08).
- **Emergency selector tiers:** scheduler force/emergency bands and tiered native selection are live: `crates/mc-module/src/scheduler.rs:726-735`; `crates/mc-module/src/selection.rs:955-1019`. Host-level 95% abort behavior is S-05.
- **Smart-drop supersession:** live when `smart_drops` is enabled, with the same drop/edit-marker precedence and ride gating: `crates/mc-module/src/selection.rs:950-1019`; config at `crates/mc-module/src/config.rs:62,288-289,355-356`.
- **Wrapup backend:** live through the native bounded wrapup runner and management method: `crates/mc-module/src/lib.rs:5364-5455` and the `session.wrapup` dispatch near `crates/mc-module/src/lib.rs:4627-4660`. Only the host command surface is structural (S-04).
- **Memory mutations:** primary `ctx_memory` supports write/update/archive/merge/get with project-vocabulary and command-id fencing: `crates/mc-module/src/lib.rs:8821-9074`.
- **Ordinary session notes:** write/read/update/dismiss are live: `crates/mc-module/src/lib.rs:9359-9680`. Smart evaluation/nudge delivery is I-19.
- **Prompt preset and per-tool description replacement:** full/light selection and valid description overrides are live on CC: `crates/mc-module/src/lib.rs:6072-6143`; `crates/mc-module/src/prompt_surface.rs:136-188`. Error policy and guidance override differ in I-16/I-17.
- **Primary language directive and sticky date:** live in CC guidance: `crates/mc-module/src/lib.rs:6249-6279,10783-10825`.

## Requested feature checklist

| Feature | Claude Code status |
|---|---|
| Tagging | **Live**, with first-pass delay I-01 |
| `ctx_reduce` | **Live**, with acknowledgement/validation gap I-07 |
| Nudge channel 1 | **Live / verified same** |
| Nudge channel 2 | **Absent structurally**, S-03 |
| Synthetic todowrite | **Live / verified same** |
| Auto-search | **Live but behaviorally different**, I-02 through I-04 |
| Historian / compartments | **Live / verified same primary-session core** |
| Decay | **Live**, retry-policy gap I-12 |
| Mural | **Absent**, I-13 |
| Caveman | **Implemented but not live from CC config**, I-14 |
| Heuristic drops | **Partially live**; emergency/two-pass/supersession/strips live, duplicate-tool lane absent (I-08), reasoning clearing absent (I-09) |
| Emergency tiers | **Native tiers live**; host abort behavior is S-05 |
| Wrapup | **Backend live**; command surface is S-04 |
| Commands | **No CC host command surface**, S-04 |
| Dreamer tasks | **Only classify task present; schedule empty**, I-21 |
| Compaction-off session mode | **Absent**, I-20 |

# INFERRED / EXTERNALLY OWNED

1. **Second cache anchor (I-23):** the one-versus-two statement comes from the task's supplied known example. This repository shows the two synthetic CK heads but not the Claude Code gateway marker writer.
2. **CC encode/decode details (S-01/S-02):** no Claude Code codec is present here. The report does not infer exact gateway field loss or ordering beyond the module's documented Anthropic constraints.
3. **CC 95% abort/retry outcome (S-05):** scheduler behavior is readable; the external gateway's active-request control is not. It must be verified in that gateway repository before claiming runtime parity.
4. **Caveman runtime absence (I-14):** this is inferred from the only readable CC-owned config/binding path plus the request field's false default. An external gateway could inject the undocumented field, but no evidence of that exists here.

# PROFILE-BRANCH LEDGER (18)

The counted effective production decisions are:

1. serializer healing coverage — `crates/mc-module/src/healing.rs:98-117`
2. tail-reclaim capability — `crates/mc-module/src/healing.rs:132-141`
3. residual serializer quirks — `crates/mc-module/src/healing.rs:144-157`
4. profile render epoch — `crates/mc-module/src/lib.rs:353-364`
5. Claude Code U1/tool surface — `crates/mc-module/src/lib.rs:367-371`
6. generic tagging profile eligibility — `crates/mc-module/src/lib.rs:373-382`
7. historian fold-only-reclaim derivation — `crates/mc-module/src/lib.rs:3638-3642`
8. profile-specific internal TTL assumption — `crates/mc-module/src/transform.rs:1716-1750`
9. profile-specific response marker TTL — `crates/mc-module/src/transform.rs:1752-1767`
10. OpenCode-only bootstrap tagging activation — `crates/mc-module/src/transform.rs:2423-2430`
11. Claude Code system-coverage forced HARD — `crates/mc-module/src/transform.rs:2987-3005`
12. profile tail-reclaim producer gate — `crates/mc-module/src/transform.rs:3036-3052`
13. covered-system inclusion policy — `crates/mc-module/src/transform.rs:5153-5178`
14. OpenCode-only channel-2 directive — `crates/mc-module/src/transform.rs:7541-7595`
15. residual application for merged assistants — `crates/mc-module/src/transform.rs:9040-9065`
16. Claude Code leading-system suppression — `crates/mc-module/src/transform.rs:9321-9339`
17. latest-assistant mutation exemption — `crates/mc-module/src/transform.rs:9664-9700`
18. OpenCode-only reasoning-clear cutoff — `crates/mc-module/src/transform.rs:9725-9740`
