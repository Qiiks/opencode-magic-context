# TS↔Rust transform parity audit — 2026-07-21

Audited at base `41593e7af39ccb3868d8e8c7e59ab48b655a2190`.

## Scope and method

This was a source-to-source walk of the TypeScript pass lifecycle (`transform.ts` → tag/replay → `inject-compartments.ts` → `transform-postprocess-phase.ts`) against the native authority path in `crates/mc-module`, `crates/mc-store`, and `crates/mc-core`. The protected contracts in `ARCHITECTURE.md:34-90` were used as checklist rows, then the walk was extended through historian publication, state sync, feeds/mirrors, nudges, emergency handling, and session-mode gates.

Severity follows the requested scale:

- **P0** — normal-use wire/cache cost, unsafe overflow handling, or durable data loss.
- **P1** — common-condition behavior divergence.
- **P2** — rare, migration-only, configuration-edge, or malformed-state divergence.
- **INTENTIONAL** — explicitly accepted divergence.

The already-known eager m1 revision-signal defect (memory/note/profile revisions forcing SOFT before a natural bust) is **excluded**. Findings below are independent of that calibration defect.

## Executive summary

The audit found **11 P0**, **28 P1**, and **12 P2** divergences. The highest-risk clusters are:

1. Rust omits several replay surfaces and provider-proven overflow fail-close behavior.
2. Rust does not enforce the TS memory/profile injection budgets.
3. Rust ignores the primary/subagent feature matrix.
4. HARD trigger identity is both incomplete (model/system/epochs) and over-broad (workspace expiry/revocation).
5. Historian publication drops durable events, primers, and observations and ignores memory promotion gates.
6. TS→module bootstrap omits queued mutation state, frozen replay state, and several cache-stability latches.

---

## P0 findings

### P0-01 — Frozen placeholder/image/stale-reduce replay is absent

- **TS anchor:** `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1141-1214`; `packages/plugin/src/hooks/magic-context/drop-stale-reduce-calls.ts:81-143`; `packages/plugin/src/hooks/magic-context/strip-content.ts:600-660`.
- **Rust anchor:** `crates/mc-module/src/transform.rs:4211-4331`; `crates/mc-module/src/selection.rs:707-718`. The TS→module payload also has no corresponding frozen-ID fields at `packages/plugin/src/hooks/magic-context/module-state-sync.ts:61-82`.
- **Divergent sequence:** On a bust, TS freezes and persists IDs for placeholder stripping, stale `ctx_reduce` calls, and processed images; every later defer reconstructs the same sentinels. Rust only replays block reductions. A processed image, stale reduce call, or frozen placeholder therefore remains/reappears in the Rust wire.
- **Cost:** Full image token cost or stale control-output cost recurs on every request; a TS→Rust transition can also restore bytes behind an otherwise stable prefix.

### P0-02 — System-injected message neutralization is missing

- **TS anchor:** `packages/plugin/src/hooks/magic-context/strip-content.ts:46-108`; `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1183-1214`.
- **Rust anchor:** `crates/mc-module/src/transform.rs:4249-4331` passes uncovered, unreduced inbound messages through.
- **Divergent sequence:** An old internal notification/reminder is identified as system-injected. TS replaces it with a provider-aware sentinel and replays that choice. Rust keeps the original content unless an unrelated reduction covers it.
- **Cost:** Internal text remains on every wire and can alter assistant-message merging; cost is the complete notification/reminder payload per pass.

### P0-03 — m0/m1 memory and user-profile budgets are not enforced

- **TS anchor:** `packages/plugin/src/hooks/magic-context/inject-compartments.ts:1644-1671` trims project memory to the configured/default budget and user profile during `renderM0`; `packages/plugin/src/hooks/magic-context/inject-compartments.ts:1988-2004` caps new-memory m1 content to 25% of the memory budget.
- **Rust anchor:** `crates/mc-module/src/m0_compose.rs:130-180` loads and passes the full active memory/profile sets; `crates/mc-store/src/lib.rs:8277-8374` returns every active/permanent memory; `crates/mc-module/src/m1_compose.rs:254-266,299-320` renders every new memory. `crates/mc-module/src/memory_render.rs:204-241` says callers own sub-block trimming, but this caller does not trim.
- **Divergent sequence:** A project accumulates 100 one-thousand-token memories. TS selects roughly the configured/default 8k project-memory budget (and 4k user-profile budget); Rust injects the whole pool. A burst of new memories is similarly capped near 2k in TS m1 but unbounded in Rust.
- **Cost:** Unbounded normal-use wire growth; tens or hundreds of thousands of extra tokens are possible on the first Rust fold and every replay.

### P0-04 — Ordinary subagents are run through primary-session features

- **TS anchor:** `packages/plugin/src/hooks/magic-context/transform.ts:553-631` sets reduced mode for subagents and disables historian/m0m1; the complete matrix is `ARCHITECTURE.md:149-165`.
- **Rust anchor:** The coordinator sends `is_subagent` at `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:786-798`, but `crates/mc-module/src/transform.rs:195-274` has no such request field. The native handler runs historian preparation unconditionally at `crates/mc-module/src/lib.rs:5777-5905`.
- **Divergent sequence:** A normal child task/explore/general session enters Rust mode. TS would retain tag/drop plumbing only, run no historian, inject no m0/m1, and disable synthetic todo/auto-search/85%-95% primary controls. Rust treats it as a primary session, composes m0/m1, can fire the historian, and emits the primary overlays and emergency behavior.
- **Cost:** Every subagent can receive project docs/profile/memories/history that TS omits, plus unnecessary producer calls. This is normal use and can add many thousands of tokens per child.

### P0-05 — Model/provider/system cache-eviction signals never enter Rust HARD identity

- **TS anchor:** `packages/plugin/src/hooks/magic-context/transform.ts:1711-1737` supplies model/system/TTL signals; `packages/plugin/src/hooks/magic-context/inject-compartments.ts:1032-1057` folds on model/system/idle changes because the provider cache is already dead.
- **Rust anchor:** `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:554-577` sends `render_config: ""`; `crates/mc-module/src/lib.rs:8788-8801` binds `model_key: None`; `crates/mc-module/src/transform.rs:907-918,1143-1163` derives HARD identity from that empty base and does not compare request `provider_id`/`model_key` or a system hash.
- **Divergent sequence:** The user switches model/provider or the Magic Context system block changes. TS folds accumulated m1 into m0 during the already-dead provider cache event. Rust misses the free fold and carries the old m0/m1 split until a later cause.
- **Cost:** The next later refold loses the whole `system + m0` cache when it could have been free; until then, the entire accumulated m1 remains in the volatile recache region.

### P0-06 — External-memory and upgrade HARD epoch fields are hard-coded empty

- **TS anchor:** `packages/plugin/src/hooks/magic-context/inject-compartments.ts:927-955,1080-1116` compares project/workspace memory epoch and session upgrade state.
- **Rust anchor:** `crates/mc-module/src/compartment_coverage.rs:49-64` defines both as mandatory HARD identity fields, but `crates/mc-module/src/transform.rs:907-917` passes `upgrade_state: String::new()` and `memory_content_epoch: String::new()` every pass.
- **Divergent sequence:** A dashboard/external editor changes a memory epoch, or session upgrade rewrites the memory taxonomy. TS hard-folds the changed baseline. Rust identity is unchanged, so stale m0 survives until another unrelated HARD (and can survive indefinitely in steady state).
- **Cost:** Agent-visible stale data under normal dashboard/upgrade use; when a later HARD arrives, it pays the full fold late instead of at the authoritative change.

### P0-07 — Workspace identity contains eager time/revocation triggers that TS excludes

- **TS anchor:** `packages/plugin/src/features/magic-context/workspaces.ts:311-333` hashes sorted identities, project epochs, and share policy only; memory expiry is frozen to the materialization timestamp at `packages/plugin/src/hooks/magic-context/inject-compartments.ts:1964-1980`.
- **Rust anchor:** `crates/mc-store/src/lib.rs:8105-8166` additionally hashes the earliest foreign-memory expiry. Migration trigger `crates/mc-store/src/lib.rs:1072-1110` bumps a visibility epoch on status/expiry/scope/shareability/category revocation.
- **Divergent sequence:** (a) A shared foreign memory reaches `expires_at`; TS keeps the frozen baseline until a natural HARD, while Rust workspace fingerprint changes on the next pass and HARDs immediately. (b) An in-session archive/revocation can ride an m1 `<removed>` correction in TS, while Rust's visibility epoch immediately changes m0 identity.
- **Cost:** A full `system + m0` recache on routine workspace expiry/revocation—the same eager-HARD cost class as the calibration incident.

### P0-08 — Pressure-backstop refold is absent

- **TS anchor:** `packages/plugin/src/hooks/magic-context/inject-compartments.ts:2533-2591` refolds after m1 recomposition when mutations exceed 40, m1 exceeds 15% of a ≥500-token m0, or m1 exceeds 20% of the history budget.
- **Rust anchor:** `crates/mc-core/src/lib.rs:107-145` classifies only bootstrap/shape/render-config/HARD/reconcile/m1/reduction cases; `crates/mc-module/src/transform.rs:1291-1302,1579-1661` performs no post-m1 size/mutation escalation.
- **Divergent sequence:** On an eligible bust, m1 grows beyond the ratio/absolute/mutation threshold. TS changes the already-busting pass to HARD and resets m1. Rust remains SOFT and continues carrying the oversized volatile delta.
- **Cost:** With the default 60k history budget, the absolute trigger is about 12k m1 tokens. Rust can grow beyond it without bound and repeatedly recache that delta.

### P0-09 — Provider-proven 95% fail-close/abort is missing

- **TS anchor:** `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:430-450`; `packages/plugin/src/hooks/magic-context/transform.ts:1861-1894` blocks only for armed provider-proven overflow when no fold landed and confirms the abort.
- **Rust anchor:** `crates/mc-module/src/scheduler.rs:620-634` detects/returns a limit, while `crates/mc-module/src/lib.rs:5936-5940` only attaches output; there is no abort/fail-closed consumer.
- **Divergent sequence:** The provider proves a lower context limit, recovery is armed, and no fold can reclaim enough. TS confirms abort and blocks the next oversized request. Rust returns a transform response and permits the harness to retry the oversized prompt.
- **Cost:** Repeated rejected provider requests, no guaranteed stop condition, and possible session failure loops.

### P0-10 — Historian publication drops events, primers, and user observations

- **TS anchor:** `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:659-673,804-890` persists events, gated observations, and primers.
- **Rust anchor:** `crates/mc-module/src/historian.rs:373-390` and `crates/mc-store/src/lib.rs:2136-2145,7776-7788` publish compartments and facts only, although validation accepts the other side channels.
- **Divergent sequence:** A valid historian output contains `<events>`, observation candidates, and primers. TS commits them with the compartment. Rust commits the compartment/facts and silently drops the rest.
- **Cost:** Durable feature data is lost on every affected normal historian publish; downstream search, dreamer, and primer workflows never see it.

### P0-11 — Rust promotes historian facts despite memory/auto-promote opt-outs

- **TS anchor:** `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:581-593,643-656` requires memory enabled and `auto_promote`.
- **Rust anchor:** `crates/mc-store/src/lib.rs:7787-7788` always calls `promote_facts_tx`; `crates/mc-module/src/config.rs:23-34` has no `auto_promote` field.
- **Divergent sequence:** The user disables memory or automatic fact promotion, then a historian output contains facts. TS stores no project memories; Rust creates them.
- **Cost:** Unauthorized durable writes under a normal supported configuration; those rows can later enter prompts and search.

---

## P1 findings

### P1-01 — Full tool drops retain Rust shells

- **TS anchor:** `packages/plugin/src/hooks/magic-context/tool-drop-target.ts:256-267,311-322`; `packages/plugin/src/hooks/magic-context/apply-operations.ts:140-162` removes invocation/result parts and then empty messages.
- **Rust anchor:** `crates/mc-module/src/ck_wire.rs:194-240`; `crates/mc-module/src/transform.rs:4285-4305` retain reduced ToolCall/ToolResult carriers.
- **Divergent sequence:** The same completed tool arc is selected as a full drop. TS removes the parts/messages; Rust emits reduced shells.
- **Cost:** Lower reclaim and different role/tool adjacency on every such drop; exact provider framing cost depends on the serializer.

### P1-02 — Structural-noise stripping is missing

- **TS anchor:** `packages/plugin/src/hooks/magic-context/strip-structural-noise.ts:5-53`; invocation at `packages/plugin/src/hooks/magic-context/transform.ts:1555-1560`.
- **Rust anchor:** `crates/mc-module/src/transform.rs:4381-4390,4717-4736` handles merged reasoning/ignored blocks but not TS `meta`, `step-start`, and `step-finish` neutralization.
- **Divergent sequence:** Canonical Anthropic input contains structural SDK parts. TS sentinelizes them; Rust preserves opaque structural blocks.
- **Cost:** Extra wire noise and possible provider reasoning-position failures.

### P1-03 — Reasoning-clearing watermark uses a different coordinate system

- **TS anchor:** `packages/plugin/src/hooks/magic-context/strip-content.ts:308-347`; `packages/plugin/src/hooks/magic-context/transform.ts:1570-1592` uses tag-number age.
- **Rust anchor:** `crates/mc-module/src/transform.rs:1334-1345,4539-4588` uses absolute message ordinal age.
- **Divergent sequence:** A message owns multiple taggable parts/files, or message ordinals are sparse. `maxTag - age` and `maxOrdinal - age` cross different assistant boundaries, so TS and Rust clear different reasoning blocks.
- **Cost:** Different reasoning bytes and prompt-cache keys; one side can retain substantially more historical reasoning.

### P1-04 — Inline `<thinking>/<think>` text stripping is missing

- **TS anchor:** `packages/plugin/src/hooks/magic-context/strip-content.ts:283-306,395-421` strips aged inline markup and replays it.
- **Rust anchor:** `crates/mc-module/src/transform.rs:4590-4679` clears typed native reasoning only.
- **Divergent sequence:** An assistant text part contains literal `<thinking>secret</thinking>`. TS removes the markup/content at the age gate; Rust leaves it in text.
- **Cost:** Hidden reasoning remains provider-visible and token-bearing.

### P1-05 — Merged-assistant reasoning strip is provider-over-broad

- **TS anchor:** `packages/plugin/src/hooks/magic-context/strip-content.ts:485-495`; finalization at `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:452-460` gates on exact provider `anthropic`.
- **Rust anchor:** `crates/mc-module/src/healing.rs:136-149`; `crates/mc-module/src/transform.rs:4386-4449` applies the residual for the general `opencode-aisdk` profile and additionally exempts a mid-turn assistant at `:4407-4411`.
- **Divergent sequence:** An OpenAI-compatible/Kimi-style provider uses the OpenCode profile and relies on `reasoning_content`. TS keeps it; Rust strips it. Conversely, the latest mid-turn assistant can be kept by Rust when TS finalization strips it.
- **Cost:** Provider-visible behavior/data difference; required reasoning content can disappear.

### P1-06 — Per-model execute thresholds, absolute overrides, and TTLs are discarded

- **TS anchor:** `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:771-798` resolves model-specific percentage/token thresholds and resolved session TTL and sends them.
- **Rust anchor:** Production uses route-frozen scalar config at `crates/mc-module/src/lib.rs:5739-5755`; bind-time `model_key` is `None` at `crates/mc-module/src/lib.rs:8788-8801`; `crates/mc-module/src/config.rs:159-177` parses only scalar percentage/string TTL.
- **Divergent sequence:** Configuration sets an absolute 100k threshold or a model-specific TTL. TS resolves the effective percentage/TTL correctly, but Rust ignores the per-pass values and schedules using its scalar/default route config.
- **Cost:** Execute/fold cadence can move by many turns; a wrong short TTL causes repeated full folds, while a wrong high threshold delays reclamation.

### P1-07 — Idle TTL is process-local transform time, not persisted response time

- **TS anchor:** `packages/plugin/src/hooks/magic-context/transform.ts:1722-1737`; `packages/plugin/src/hooks/magic-context/inject-compartments.ts:1044-1057` compares last completed response against materialization and self-consumes.
- **Rust anchor:** `crates/mc-module/src/lib.rs:2710-2743` deliberately ignores the durable anchor until an in-process observation; `crates/mc-module/src/lib.rs:5942` records the transform completion as the observation.
- **Divergent sequence:** After process restart and a long idle, TS folds on the first return; Rust suppresses TTL because no in-process response was observed. Conversely, a long model/tool step longer than TTL makes Rust measure from the previous transform and fold on the next step even though TS just observed a response.
- **Cost:** Missed free folds after restart or false full folds during slow turns.

### P1-08 — Normal execute ignores the historian-running veto

- **TS anchor:** `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:492-547,561-607` blocks pending ops/heuristics while historian runs, except 85%/HARD bypass.
- **Rust anchor:** `crates/mc-module/src/transform.rs:1185-1269,1936-1938` gates selection only on scheduler pass/HARD advisory; `crates/mc-module/src/lib.rs:5887-5903` permits a concurrent live historian.
- **Divergent sequence:** A historian is active and a normal execute arrives below emergency with queued drops. TS holds them. Rust selects/freezes them immediately.
- **Cost:** Different tail bytes and drop timing during a common long historian run; HARD drain-everything itself is correct, but the normal veto is missing.

### P1-09 — Mid-turn deferral misses the real-user release valve and differs on arc shapes

- **TS anchor:** `packages/plugin/src/hooks/magic-context/read-session-db.ts:100-131` clears mid-turn if a newer real user exists and treats unfinished client tool parts as mid-turn; `packages/plugin/src/hooks/magic-context/boundary-execution.ts:30-43` defers.
- **Rust anchor:** `crates/mc-module/src/transform.rs:1983-2009` derives state only from newest-assistant arc/result matching; `crates/mc-module/src/scheduler.rs:401-421` consumes that state. Request `mid_turn` is not used by this scheduler path.
- **Divergent sequence:** A stalled assistant has an open tool call and a newer real user arrives. TS releases execute; Rust still sees the open arc and defers. Tool-call-without-arc and provider-completed representations can also invert the decision.
- **Cost:** One or more delayed bust/reclaim opportunities and possible overflow under a stalled arc.

### P1-10 — Current-message `ctx_reduce` availability is replaced with a DB-only race

- **TS anchor:** `packages/plugin/src/hooks/magic-context/ctx-reduce-availability.ts:37-61`; `packages/plugin/src/hooks/magic-context/transform.ts:566-568` freezes directly from the first in-memory user message.
- **Rust anchor:** `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:748-751` calls the DB resolver and suppresses provider-visible bytes while provisional; the module HARDs on surface transition at `crates/mc-module/src/transform.rs:873-919`.
- **Divergent sequence:** The first user message is in the transform array but not yet visible in `opencode.db`. TS freezes the tools verdict and includes tags in the first fold. Rust first folds without the tagger surface, then sees the persisted verdict and performs a second HARD to activate it.
- **Cost:** An avoidable full-prefix recache on the ordinary first-message persistence race.

### P1-11 — User-profile deltas never render in m1

- **TS anchor:** `packages/plugin/src/hooks/magic-context/inject-compartments.ts:2006-2022` emits `<new-user-profile>` when the version differs, without making the version a HARD trigger.
- **Rust anchor:** `crates/mc-module/src/m1_compose.rs:268-270` explicitly leaves the profile delta unimplemented; `crates/mc-module/src/m1_compose.rs:116-136` has no profile version in its revision inputs.
- **Divergent sequence:** A user-profile promotion lands after m0. At the next natural bust TS emits the new profile in m1. Rust m1 composition omits it even when another cause opens a SOFT pass; it appears only after unrelated HARD recomposition.
- **Cost:** Common stale personalization and a potentially long visibility delay.

### P1-12 — Auto-search configuration, corpus, scoring, and bytes diverge

- **TS anchor:** `packages/plugin/src/hooks/magic-context/auto-search-runner.ts:241-409` honors enable/min-length/score gates, searches memory/message/git, filters already-visible memories, and freezes hint/no-hint decisions; rendering is `packages/plugin/src/hooks/magic-context/auto-search-hint.ts:89-131`.
- **Rust anchor:** `crates/mc-module/src/transform.rs:3462-3502,3531-3667,3751-3775` always runs a lexical memory+compartment scorer for a new authored user when tagging is active; `crates/mc-module/src/config.rs:23-35` has no auto-search settings.
- **Divergent sequence:** Disable auto-search, use a short prompt, rely on semantic/message/git hits, or have a memory already visible in m0. TS suppresses or selects according to config/unified search; Rust can emit a different hint (and different text) or miss TS's hit.
- **Cost:** Up to roughly 600–800 hint characters plus search/query work, and deterministic tail-byte mismatch.

### P1-13 — Channel 1 uses different severity/protection math

- **TS anchor:** `packages/plugin/src/hooks/magic-context/ctx-reduce-nudge.ts:243-329`; configured protected tags enter at `packages/plugin/src/hooks/magic-context/transform.ts:2088-2094`.
- **Rust anchor:** `crates/mc-module/src/transform.rs:3792-3848,4023-4105` uses reclaimable/working-window and an additional usable-space ratio; its active-tag path does not consume the TS configured protected-tag count.
- **Divergent sequence:** The same tag pool sits near a severity boundary or contains protected tags. TS and Rust select different gentle/firm/urgent levels or disagree whether to fire.
- **Cost:** User-visible nudge cadence and severity differ; protected content can be advertised as reclaimable.

### P1-14 — Channel 2 is delivered at a different lifecycle point and uses fixed protection

- **TS anchor:** `packages/plugin/src/hooks/magic-context/transform.ts:2129-2172` records pending; `packages/plugin/src/hooks/magic-context/channel2-delivery.ts:80-145` delivers at terminal `message.updated`.
- **Rust anchor:** `crates/mc-module/src/transform.rs:3928-3978`; `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:984-1007` attempts `promptAsync` immediately after transform and uses a fixed newest-20 accounting basis.
- **Divergent sequence:** Pressure qualifies while a turn is still accumulating, or `protectedTags != 20`. TS waits for the step boundary and uses configured protection; Rust attempts delivery immediately and computes a different reclaimable set.
- **Cost:** A stale synthetic-user ceiling nudge can arrive mid-turn, and trigger decisions differ under supported configuration.

### P1-15 — Note-nudge trigger/deferral has no Rust equivalent

- **TS anchor:** `packages/plugin/src/hooks/magic-context/note-nudger.ts:75-105`; `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1337-1364` defers a commit/read-triggered nudge while the trigger-time user is current, then appends sticky `<instruction>` text.
- **Rust anchor:** The only native note injection is saved-note claiming into m1 at `crates/mc-module/src/m1_compose.rs:272-283`, acknowledged at `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:935-975`; it has no TS note-nudge trigger-message/deferral state.
- **Divergent sequence:** A commit/read trigger asks the agent to consider recording a note while the triggering user is current. TS defers and later replays the sticky nudge. Rust emits no equivalent nudge; unrelated saved notes may surface through `<new-notes>`, which is a different feature.
- **Cost:** The user-visible note reminder and its deterministic anchor are absent in Rust.

### P1-16 — Emergency newest-window semantics contradict TS implementation

- **TS anchor:** `packages/plugin/src/hooks/magic-context/heuristic-cleanup.ts:121-143` calls full `drop()` and persists `drop_mode="full"` for emergency. `packages/plugin/src/hooks/magic-context/apply-operations.ts:30-39` explicitly says emergency remains full-drop.
- **Rust anchor:** `crates/mc-module/src/selection.rs:735-809` converts every `FullDrop` inside the newest-20 arcs into `Skeleton`.
- **Divergent sequence:** Emergency selects a recent completed tool arc. TS removes it fully; Rust retains a skeleton.
- **Cost:** Rust can undershoot emergency target headroom. Note: protected `ARCHITECTURE.md:135` currently describes newest-20 skeletons, so the protected document agrees with Rust and conflicts with the TS implementation; fix direction needs explicit owner adjudication, but parity is not in doubt.

### P1-17 — Emergency skeleton payload bytes differ

- **TS anchor:** `packages/plugin/src/hooks/magic-context/tool-drop-target.ts:62-109,182-199` clamps only input JSON above 500 bytes and summarizes arrays/objects.
- **Rust anchor:** `crates/mc-module/src/selection.rs:416-441` always clamps long strings and preserves arrays/objects.
- **Divergent sequence:** A skeletonized call has long strings, arrays, or objects. The two implementations freeze different invocation bytes.
- **Cost:** Cache-key and token mismatch on each skeleton; arrays/objects can remain much larger in Rust.

### P1-18 — Emergency protection and same-sample idempotence inputs differ

- **TS anchor:** `packages/plugin/src/hooks/magic-context/emergency-drop.ts:176-237`; `packages/plugin/src/hooks/magic-context/heuristic-cleanup.ts:137-143` applies configured tag protection and latches every acting pass, including zero-removal acting passes.
- **Rust anchor:** `crates/mc-module/src/transform.rs:1199-1217,1694-1697`; `crates/mc-module/src/selection.rs:617-669` uses block protection/global cutoff zero and records the sample only when reductions remain after filtering.
- **Divergent sequence:** Protection filters all candidates or the same stale provider sample repeats. TS latches and no-ops next time; Rust can leave the sample unset and reselect, or protect a different set.
- **Cost:** Repeated bust attempts and over/under-drop around the emergency floor.

### P1-19 — Emergency drain latch lacks TS quota/reservation semantics

- **TS anchor:** `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:636-726` reserves per-window drain tokens, enforces per-run caps/backoff, and supports over-quota bypass; no-head paths clear it at `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:297-318`.
- **Rust anchor:** `crates/mc-module/src/scheduler.rs:443-478,614-633`; `crates/mc-module/src/transform.rs:1961-1980`; historian preparation at `crates/mc-module/src/lib.rs:3214-3230` does not consume an equivalent quota.
- **Divergent sequence:** Pressure arms catch-up, a run fails or has no eligible head, or a window quota is exhausted. TS reserves/rolls back/clears according to the durable window; Rust retains a time/usage latch with no equivalent controlled quota.
- **Cost:** Different producer-call count and chunk size under emergency; potentially one unnecessary chunk per pass.

### P1-20 — Batch-and-ride holds/applies different command sets

- **TS anchor:** `packages/plugin/src/hooks/magic-context/apply-operations.ts:88-175` applies all pending operations on an eligible pass.
- **Rust anchor:** `crates/mc-module/src/selection.rs:554-586`; `crates/mc-module/src/transform.rs:1230-1264` can hold a first-applied command unless a distinct unapplied command provides a ride opportunity.
- **Divergent sequence:** One already-first-applied command is the only queued command when a natural bust arrives. TS drains it with the batch; Rust can retain it for another pass. With multiple command IDs, the chosen coalescing set also differs.
- **Cost:** An avoidable later bust or one-pass drop delay.

### P1-21 — Synthetic-todo anchor selection is not TS-equivalent

- **TS anchor:** `packages/plugin/src/hooks/magic-context/transform-message-helpers.ts:92-108`; `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:120-155` chooses the latest eligible non-summary assistant, with deterministic head fallback.
- **Rust anchor:** `crates/mc-module/src/transform.rs:2441-2446,2721-2735` anchors to the last non-synthetic tail message regardless of role.
- **Divergent sequence:** Tail ends in a user or tool carrier after the last good assistant. TS anchors to the assistant; Rust anchors to the tail end.
- **Cost:** Different synthetic pair position, tool adjacency, and cache prefix bytes.

### P1-22 — Historian chunk input budget is fixed instead of model-relative

- **TS anchor:** `packages/plugin/src/hooks/magic-context/derive-budgets.ts:60-73`; `packages/plugin/src/hooks/magic-context/hook.ts:250-255` derives `clamp(historian_context × 0.25, 8k, 50k)`.
- **Rust anchor:** `crates/mc-module/src/lib.rs:220-221,3223-3230` uses a fixed 32k producer chunk budget.
- **Divergent sequence:** A 16k historian model gets an 8k TS chunk but up to 32k Rust; a 200k model gets 50k TS but only 32k Rust.
- **Cost:** Up to 24k extra input tokens/call on a small model, or approximately `50/32 = 1.56×` as many calls for the same large-model history.

### P1-23 — Pending-drop projection is not supplied to the Rust historian trigger

- **TS anchor:** `packages/plugin/src/hooks/magic-context/compartment-trigger.ts:591-611,722-731` suppresses fire when projected drops/reasoning reach the 0.75-relative target.
- **Rust anchor:** Rust supports the field, but production passes `projected_post_drop_percentage: None` at `crates/mc-module/src/lib.rs:3093-3112`.
- **Divergent sequence:** Pending reductions alone would bring pressure below target. TS skips historian; Rust fires.
- **Cost:** One unnecessary producer call, up to the fixed 32k Rust input budget.

### P1-24 — Discard-last fact retention differs

- **TS anchor:** `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:577-580,650-656,804-844` suppresses all fact/observation/primer promotion when any final compartment is discarded.
- **Rust anchor:** `crates/mc-module/src/historian_validate.rs:517-545` retains side channels anchored to retained compartments, and retained facts reach the publisher at `crates/mc-store/src/lib.rs:7776-7788`.
- **Divergent sequence:** Validation discards the lookahead-free final compartment while earlier compartments and their facts exist. TS suppresses all promotion for the run; Rust still publishes facts anchored before the discarded tail. Rust's broader loss of events/primers/observations is separately P0-10.
- **Cost:** Durable project-memory facts differ after an otherwise identical discard-last run.

### P1-25 — Final wrapup can discard its last compartment in Rust

- **TS anchor:** `packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts:236-248`; `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:352-354,505-516` sets `forceKeepLastCompartment` for the actual final chunk.
- **Rust anchor:** `crates/mc-module/src/lib.rs:3366-3384` supplies no equivalent wrapup flag.
- **Divergent sequence:** Manual wrapup reaches its final weak-lookahead chunk. TS persists the last compartment; Rust discard-last can remove it.
- **Cost:** An extra wrapup round and more raw tail left uncompacted.

### P1-26 — TS pending operations are omitted from bootstrap

- **TS anchor:** `packages/plugin/src/features/magic-context/storage-ops.ts:14-16,86-99`; apply gate at `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:548-565`.
- **Rust anchor:** `packages/plugin/src/hooks/magic-context/module-state-sync.ts:61-82,470-471` seeds dropped tags but no pending-op queue; Rust loads only its own queue at `crates/mc-module/src/transform.rs:1072-1073`.
- **Divergent sequence:** A TS drop is queued but has not become `status='dropped'` when Rust authority starts. The seed excludes it and Rust serves the original content.
- **Cost:** User-requested reduction is lost/delayed across a TS→Rust transition; the full target remains on wire.

### P1-27 — Sticky note/auto-search decisions and synthetic-todo anchor are omitted from bootstrap

- **TS anchor:** Sticky decisions replay at `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1227-1239`; stores are `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:1204-1218`. Synthetic todo persists `(callId,messageId,stateJson)` at `:1460-1502`.
- **Rust anchor:** `packages/plugin/src/hooks/magic-context/module-state-sync.ts:607-612` sends only todo state JSON; `crates/mc-module/src/transform.rs:2721-2734` chooses a fresh anchor. There are no note/auto-search seed fields in `module-state-sync.ts:61-82`.
- **Divergent sequence:** A prior TS pass persisted a note reminder, no-hint/hint decision, or todo pair anchored at assistant A. Rust receives raw messages plus todo state only; note/hint augmentation disappears and todo can move to another anchor.
- **Cost:** Tail bytes change across mode transition; synthetic pair relocation can move the cache breakpoint.

### P1-28 — Emergency and reclaim frontiers are omitted from bootstrap

- **TS anchor:** Emergency drain/sample state lives at `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:676-693,804-848`; the tool reclaim watermark is consumed at `packages/plugin/src/hooks/magic-context/tool-reclaim.ts:10-39` and advanced at `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:870-943`.
- **Rust anchor:** Rust selection consumes `last_emergency_input_sample`, `has_prior_emergency_drop`, and `last_execute_ordinal` at `crates/mc-module/src/transform.rs:1253-1260`; none are present in `packages/plugin/src/hooks/magic-context/module-state-sync.ts:61-82`.
- **Divergent sequence:** After TS emergency/reclaim activity, Rust starts on the same stale usage and tail. It can repeat emergency selection, while a zero `last_execute_ordinal` can miss tool arcs TS already aged into the next execute.
- **Cost:** Over-drop/repeated bust or under-reclaim on the first Rust passes after transition.

---

## P2 findings

### P2-01 — `dreamer.inject_docs: false` is ignored

- **TS anchor:** `packages/plugin/src/hooks/magic-context/inject-compartments.ts:715-718` returns an empty docs block when injection is disabled.
- **Rust anchor:** `crates/mc-module/src/m0_compose.rs:152-180` always reads and renders project docs; `crates/mc-module/src/config.rs:23-35` has no inject-docs field.
- **Sequence/cost:** With docs disabled, TS emits none while Rust injects all canonical docs on every m0 replay. Cost is the full docs block; configuration-edge severity keeps this P2.

### P2-02 — Temporal-awareness option/timestamps are dropped

- **TS anchor:** `packages/plugin/src/hooks/magic-context/transform.ts:1404-1430`; `packages/plugin/src/hooks/magic-context/temporal-awareness.ts:138-189` replays deterministic gaps from message timestamps when enabled.
- **Rust anchor:** `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:554-577` sends neither `prev_response_completed_at_ms` nor `request_observed_at_ms`; Rust requires them for the newest-user gap at `crates/mc-module/src/transform.rs:3395-3413`.
- **Sequence/cost:** Enable temporal awareness and return after >5m. TS prepends a gap marker; Rust freezes an empty decision. Small byte cost, but deterministic behavior differs.

### P2-03 — Initialized state with missing m1 rejects instead of recovering

- **TS anchor:** `packages/plugin/src/hooks/magic-context/inject-compartments.ts:1017-1018` treats `cached_m1_missing` as HARD.
- **Rust anchor:** `crates/mc-module/src/transform.rs:2020-2032`; `crates/mc-core/src/lib.rs:107-119` rejects any initialized shape missing exactly one m0/m1.
- **Sequence/cost:** A migration/partial legacy state has valid m0 but no m1. TS reconstructs both; Rust fails transform and falls to LKG/raw. Rare malformed-state path.

### P2-04 — Rust-only 512-token historian substance floor

- **TS anchor:** `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:351-374` proceeds for any nonempty chunk after trigger gating.
- **Rust anchor:** `crates/mc-module/src/historian_chunk.rs:544-560` rejects `<512` tokens except emergency/fold-only.
- **Sequence/cost:** A forced non-emergency head has 1–511 tokens. TS spends one call; Rust leaves it raw. Edge-sized chunk.

### P2-05 — Discard-last emergency guard keys different state

- **TS anchor:** `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:503-510` disables discard-last on durable overflow-recovery state.
- **Rust anchor:** `crates/mc-module/src/lib.rs:3223-3230`; `crates/mc-module/src/historian_validate.rs:488-503` disables it from current usage ≥95%.
- **Sequence/cost:** Recovery stays armed after measured usage falls below 95, or usage hits 95 without the TS recovery flag. One side discards the last compartment and the other retains it.

### P2-06 — Sparse ordinal gaps change discard-last healing

- **TS anchor:** `packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:488-510` uses numeric ordinal distance.
- **Rust anchor:** `crates/mc-module/src/historian_validate.rs:493-501` counts present ordinals.
- **Sequence/cost:** Retired ordinal gaps make TS believe enough lookahead existed while Rust sees fewer present messages and discards. Dense sessions match.

### P2-07 — Deferred compaction marker and deferred execute are not seeded

- **TS anchor:** Marker state is persisted at `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:2052-2114`; deferred execute at `:2253-2300`.
- **Rust anchor:** State sync applies only the todo member of this metadata class at `crates/mc-store/src/lib.rs:6452-6454`; Rust's independent deferred state lives at `crates/mc-store/src/lib.rs:2494-2499` but is not initialized by TS.
- **Sequence/cost:** Switch to Rust while a marker move or mid-turn execute is pending. Rust can omit marker reconciliation or choose defer after the tool closes. Transition-only timing edge.

### P2-08 — Historical v23 note feed rows can clobber rich TS columns

- **TS anchor:** `packages/plugin/src/features/magic-context/context-authority.ts:1473-1511` writes absent smart-note fields as null/zero.
- **Rust anchor:** Legacy trigger `crates/mc-store/src/lib.rs:1019-1027` emitted only a partial note row; current v26 rows are complete at `:1218-1240`.
- **Sequence/cost:** A pre-v26 feed row remains behind the mirror cursor, then is consumed after upgrade over an existing rich TS note. Missing manifest/check/liveness/status fields overwrite good columns. This is real data loss but requires a narrow legacy backlog.

### P2-09 — Channel 2 module directives can repeat after terminal TS state

- **TS anchor:** Terminal claim/delivery state is persisted at `packages/plugin/src/features/magic-context/storage-meta-persisted.ts:970-983`.
- **Rust anchor:** `crates/mc-module/src/transform.rs:3928-3962` emits from pressure alone; host CAS at `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:993-1007` rejects nonempty→pending.
- **Sequence/cost:** TS state is `claimed` or `delivered`; Rust repeatedly returns a directive, but host CAS suppresses the user message. No double fire, only repeated response/decision work.

### P2-10 — Same-owner duplicate call IDs have different composite identity

- **TS anchor:** `packages/plugin/src/features/magic-context/tagger.ts:30-34`; `packages/plugin/src/hooks/magic-context/tag-messages.ts:484-491` keys `(ownerMessageId, callId)`.
- **Rust anchor:** `crates/mc-module/src/ck_wire.rs:318-335`; `crates/mc-module/src/selection.rs:247-277` keys distinct block-index arcs.
- **Sequence/cost:** One assistant message contains two invocations with the same call ID. TS aggregates one drop target; Rust can reduce them independently. Malformed/rare provider shape.

### P2-11 — Missing synthetic-todo anchor fails rather than skips

- **TS anchor:** `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1463-1471` silently skips defer replay when a persisted real anchor is absent.
- **Rust anchor:** `crates/mc-module/src/transform.rs:4345-4355` returns `SyntheticTodoAnchorMissing`.
- **Sequence/cost:** The anchored assistant vanishes without a coverage-advance relocation. TS omits the pair; Rust fails and uses LKG/raw. Rare reshape path.

### P2-12 — Provider empty-sentinel fallback is broader than exact TS provider identity

- **TS anchor:** `packages/plugin/src/hooks/magic-context/sentinel.ts:20-37,78-95` accepts empty content only for exact provider `anthropic`.
- **Rust anchor:** `crates/mc-module/src/transform.rs:4512-4536,4608-4612` can infer acceptance from `anthropic/...` model key/native metadata.
- **Sequence/cost:** Only manifests if `provider_id`, `model_key`, and native metadata disagree; Rust can send empty blocks where TS would use `[dropped]`. Kept P2 because the authority adapter normally supplies consistent metadata.

---

## Intentional divergence

### INTENTIONAL-01 — Caveman compression is TS-only

- **TS anchor:** `packages/plugin/src/hooks/magic-context/heuristic-cleanup.ts:55-58,257-275`; user configuration is documented in `CONFIGURATION.md:566-570`.
- **Rust anchor:** Selector scope is tool/control reduction only at `crates/mc-module/src/selection.rs:9-11`; `crates/mc-module/src/config.rs:23-35` has no caveman setting.
- **Disposition:** The audit brief explicitly identifies this as known-absent/intentional. Repository documentation does not currently label the Rust omission, so adding a parity note would prevent future re-audits.

---

## Undetermined — needs a beat

These were not promoted to findings because source alone did not prove the final behavior:

1. **Native compaction-marker ownership.** TS defers an OpenCode marker (`compartment-runner-incremental.ts:687-725`), while Rust owns m0/m1 coverage directly (`crates/mc-module/src/transform.rs:4211-4265`) and historian publish leaves render state untouched (`crates/mc-store/src/lib.rs:7708-7713`). A native-host reproduction is needed to decide whether the missing marker is a defect outside TS→Rust transition seeding.
2. **Compartment mirror `legacy`.** Rust status omits `legacy` (`crates/mc-module/src/lib.rs:4346-4365`); TS infers it from missing p1 (`packages/plugin/src/hooks/magic-context/module-state-sync.ts:193-200`). The invariant `legacy=1 ⇔ p1=null` was not established.
3. **Generic non-authority memory sync.** `module-state-sync.ts:849-874` omits `classified_at`, but production Rust mode first establishes authority and uses full `SELECT *` seeds (`rust-mode-transform.ts:343-383`). No live transform sequence using the generic path was proven.
4. **Direct-caller context-limit fallback.** Direct Rust calls fall back to 200k (`crates/mc-module/src/transform.rs:1919-1925`), while the TS authority adapter supplies 128k when unknown (`rust-mode-transform.ts:765-770`). Production authority impact was not established.
5. **Pending m0 structural-mutation identity.** TS has a `max_mutation_id` HARD trigger, while Rust native recomp/revert owns a separate reconcile protocol. State import carries complete compartments, but source comparison did not prove whether every delete/merge/recomp transition necessarily toggles reconcile before transform.
6. **Late growth after a completed OpenCode tool result.** Neither path bumps token accounting; TS explicitly treats this as unreachable (`packages/plugin/src/features/magic-context/tagger.ts:571-585,685-696`). Pi has a separate growth path, so no OpenCode finding was recorded.

---

## Surfaces verified clean

The following rows were walked and matched (apart from findings above):

- **First render and first-compartment bootstrap:** Rust HARDs uninitialized state and first publish before a boundary exists (`crates/mc-core/src/lib.rs:107-114`; `crates/mc-module/src/transform.rs:1123-1142`), matching TS first render and the first-publish exception.
- **Deliberate m1 non-HARD markers:** New compartment sequence, max memory ID, and memory mutation cursor live in Rust m1 composition (`crates/mc-module/src/m1_compose.rs:116-136,221-266`) rather than Rust m0 identity. User-profile version is also absent from both HARD identities, although Rust's missing m1 profile renderer is P1-11. The known eager revision-signal classifier defect is excluded, not marked clean.
- **Project-docs/tool-set hashes are not HARD triggers:** TS omits docs and tool-set hashes from `mustMaterialize` (`packages/plugin/src/hooks/magic-context/inject-compartments.ts:1108-1118`); Rust excludes docs from `M0ContentEpoch` (`crates/mc-module/src/compartment_coverage.rs:25-34`) and has no tool-set member in that identity.
- **Workspace membership/share-policy identity:** Apart from Rust's extra expiry/revocation axes, both canonicalize sorted member identity/epochs and share policy (`workspaces.ts:311-333`; `mc-store/src/lib.rs:8105-8152`).
- **Format epochs:** Rust folds memory, compartment, serializer-profile, and tagger epochs into one deterministic render identity (`transform.rs:884-918`; `compartment_coverage.rs:94-117`).
- **HARD drain-everything advisory:** Rust opens selection on HARD advisory (`transform.rs:1185-1269,1936-1938`), matching the TS fold-exec bypass. The normal historian veto is the separate P1-08 defect.
- **Deferred execute persistence/clear within Rust:** Rust commits a pending flag on defer and clears it atomically on a non-defer outcome (`transform.rs:1104-1114,1969-1974`), matching TS re-peek/CAS intent once the flag exists.
- **Usage basis:** TS persists input + cache-read + cache-write (`event-handler.ts:477-481`) and Rust mode sends that same `inputTokens` total (`rust-mode-transform.ts:306-313`). Rust does not substitute output tokens.
- **Context-limit resolution on the authority adapter:** TS resolves models.dev/user override, smaller detected limit, then model-matched sane usage limit (`event-resolvers.ts:75-124`) before sending the limit to Rust (`rust-mode-transform.ts:759-783`).
- **80% clamp:** TS resolves/clamps before send (`event-resolvers.ts:226-324`); scalar Rust config also clamps at 80 (`config.rs:195-197`). Per-model/absolute transport is the P1-06 defect.
- **85/95 bands and emergency mid-turn bypass:** `transform-postprocess-phase.ts:481-547` and `scheduler.rs:390-421` use matching basic bands/bypass.
- **Emergency selector constants:** T1/T2/T3 ordering, 20% T1/T2 reserve, 30% target, and 2,000-token minimum match (`emergency-drop.ts:22-33,209-251`; `selection.rs:32-73,617-691`). Shape/protection/idempotence differences are listed separately.
- **Synthetic-todo normalization and ID:** Terminal filtering, title count, pretty output, and 16-hex SHA-256 call ID match (`todo-view.ts:149-200`; `injection.rs:108-189`). None-anchor placement immediately after m0/m1 also matches (`transform-postprocess-phase.ts:120-143`; `transform.rs:4230-4247`).
- **Frozen reduction immutability and defer gating:** Both freeze payload/mode once and replay it without re-deciding (`packages/plugin/src/hooks/magic-context/apply-operations.ts:30-37,180-214`; `crates/mc-module/src/transform.rs:2281-2337`); Rust's selector is closed on plain defer (`crates/mc-module/src/transform.rs:1185-1197,1936-1938`).
- **OpenCode tag token stability:** Existing completed tool tags are not bump-on-growth on either path (`packages/plugin/src/features/magic-context/tagger.ts:571-585,685-696`; `crates/mc-module/src/transform.rs:2788-2808`). TS treats post-completion growth as unreachable; Pi's separate growth-bump contract is out of this OpenCode audit.
- **Current memory feed rows:** Rust current triggers emit complete memory snapshots (`mc-store/src/lib.rs:945-995`); TS preserves absent legacy values while consuming (`context-authority.ts:1169-1246`).
- **Current v26 note feed rows:** Rust emits complete smart-note snapshots (`mc-store/src/lib.rs:1218-1312`); TS consumes the complete shape (`context-authority.ts:1473-1511`). Only historical v23 rows are P2-08.
- **Authority memory/note seed:** Rust mode uses full TS rows (`rust-mode-transform.ts:343-383`), and Rust stores classification and other durable fields (`mc-store/src/lib.rs:10372-10447,10528-10604`).
- **Compartment and memory-mutation feed shape:** TS sends dates/legacy and complete mutation rows (`module-state-sync.ts:386-407,884-893`); Rust accepts them (`mc-module/src/lib.rs:1223-1255,1426-1446,1485-1499`).
- **Dropped-tag mode seed:** `full`, `truncated`, and `edit_marker` are preserved (`module-state-sync.ts:470-541`; `mc-store/src/lib.rs:3897-3984`). The healed drop-state calibration defect was intentionally not re-reported.
- **Channel 2 delivery authority:** Despite early timing and accounting differences, empty→pending CAS and terminal no-double-fire are host-owned (`storage-meta-persisted.ts:1074-1131`; `rust-mode-transform.ts:984-1007`).
- **Channel 1 once-per-level/refire structure:** Both sides have once-level/refire suppression; only severity/protection inputs diverge.
- **Note deltas do not independently trigger a bust:** TS defers note delivery (`note-nudger.ts:94-105`); Rust excludes notes from m1 revision (`m1_compose.rs:272-275`).
- **Historian trigger constants:** 5% relative budget, 5k/50k clamps, 2-point proactive offset, 0.75 target, 6k/12-message floor, 3 commit clusters, 3× tail multiplier, and 80/95 bands match (`compartment-trigger.ts:35-43`; `boundary.rs:23-47`).
- **Protected-tail 0.40/96k sizing:** `protected-tail-boundary.ts:176-201` matches `boundary.rs:360-401`.
- **Live-prompt and open-arc fences:** TS `protected-tail-boundary.ts:546-581` / `read-session-true-raw-tokens.ts:491-523` match Rust `boundary.rs:474-519,1162-1196` structurally.
- **Commit-cluster and TC chunking:** TS `read-session-chunk.ts:644-680` matches Rust `historian_chunk.rs:260-299`.
- **Historian discard-last base guard:** Both require progress/multiple compartments before discard; the emergency-state, final-wrapup, side-channel, and sparse-ordinal differences are P1-24/P1-25 and P2-05/P2-06.
- **Historian atomicity/staleness fence:** Rust publication is CAS/fingerprint-guarded (`crates/mc-module/src/historian.rs:346-439`; `crates/mc-store/src/lib.rs:7708-7811`).
- **Independent cache ownership:** TS cached m0/m1 bytes are not imported into the authority branch (`packages/plugin/src/hooks/magic-context/transform.ts:537-546`); Rust commits its own cache state atomically (`crates/mc-store/src/lib.rs:6329-6359,6465-6473`). This is a separate-cache design, not a partial-row overwrite.
- **Hidden MC-owned sessions:** TypeScript returns before the Rust adapter for internal historian/dreamer/sidekick sessions (`transform.ts:524-545`), so the ordinary-subagent defect does not recurse into MC's own hidden children.
- **Caveman absence:** explicitly intentional, above.

---

## Ranked fix order

1. **Restore safety and data integrity:** P0-09 fail-close; P0-10/P0-11 historian publication/promotion gates; P2-08 legacy feed preserve-on-absent.
2. **Restore wire bounds:** P0-03 memory/profile trims; P0-01/P0-02 frozen strip/neutralization; P1-02/P1-04 structural/inline reasoning cleanup.
3. **Restore session isolation:** P0-04 propagate and enforce subagent mode before m0/m1/historian/overlays/emergency.
4. **Repair HARD identity:** P0-05 model/system signals; P0-06 real upgrade/external-memory epochs; P0-07 remove time/revocation eager axes or route them through cache-neutral m1; P1-07 TTL clock.
5. **Implement pressure refold:** P0-08 using the TS mutation/ratio/absolute gates, after the known eager-m1 fix lands.
6. **Repair emergency parity:** adjudicate P1-16 against protected `ARCHITECTURE.md`, then fix P1-17/P1-18/P1-19.
7. **Seed transition state:** P1-26/P1-27/P1-28 and P2-07; keep full rows/preserve-on-absent at every feed consumer.
8. **Fix scheduler/boundary gates:** P1-06, P1-08, P1-09, P1-10.
9. **Fix overlays/nudges:** P1-11 through P1-15 and P1-21.
10. **Fix historian cost/healing:** P1-22 through P1-25 and P2-04 through P2-06.
11. **Harden rare shapes:** P2-01 through P2-03 and P2-09 through P2-12.

## Suggested parity gates

- Add a table-driven protected-invariant test that feeds one TS scenario and one Rust scenario for every HARD trigger and deliberate non-trigger.
- Add golden first-pass/cold-restart cases with ordinary primary, ordinary subagent, and MC-owned hidden child sessions.
- Add a maximum-wire assertion for m0 project memory, user profile, and m1 new memories.
- Add mode-transition fixtures containing every persisted state class: pending ops, strip IDs, marker/execute state, emergency/reclaim frontiers, note/auto decisions, todo anchor, and Channel 2 claim.
- Add feed fixtures for both current and historical row shapes; TS consumers must preserve every absent field.
- Add emergency goldens that assert target headroom, skeleton/full shape, protected set, and repeated same-sample idempotence.
- Add historian publication goldens including discarded-tail facts, events, observations, primers, promotion-disabled config, wrapup final chunk, and projected post-drop suppression.
