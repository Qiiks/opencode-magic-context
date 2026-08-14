# Rust ↔ TypeScript transform parity — round 4

Audit base: `ae0e09943d207aba1f30407f796a8e8686c11851` (2026-08-14).

## Executive result

DISPOSITION (2026-08-14 end of wave): 13 of 14 CLOSED on master (R4-01/02 batch A 348fe104; R4-03/14 batch C c7b48627 + sibling-site fix 23e37040; R4-05..08 batch B 0d30508e; R4-04 reduced post-Thalamus-measurement to the geometry wire struct, closed both sides with 3-arm container drive; R4-10 d81089a8; R4-11/13 da968bb2; R4-12 module half 7583070a — gateway arm built at thalamus 713f6b6, two-arm joint drive pending). OPEN: R4-09 as a scoped follow-up (module-persisted project-level mural artifact; mason proved no daemon-reachable source exists today — dependency report, not a fix gap). Round 4 found **14 current transform-parity defects**: **3 CRITICAL, 6 HIGH, and 5 MEDIUM**. The most urgent fresh defects are in the same-hour B-arm implementations: the OpenCode CK adapter transports only one of the three TS reasoning part types and drops the `cache_control` discriminator, while Rust also excludes subagents from the applied-set transition. The S1 geometry port also remains incomplete in two independent ways: Claude Code still receives an uncarved limit, while OpenCode Rust uses the soft denominator for the absolute emergency wall that TypeScript now evaluates against `usableHard`.

The R3 reduction, threshold, off-mode, summary, todo, and OpenCode mural/guidance batches are otherwise present at this base. The R3 auto-search batch closed config transport and common hint rendering, but it did **not** close decision/application timing or all admission controls. The three open CC-profile findings from R3 remain open. Issue #313 is confirmed at the current source; the existing “HARD-only estimator” test does not observe the direct estimator call that violates its claim.

Corpus/source synchronization (I-03), Pi ↔ OpenCode drift, and memory-holder ownership remain out of scope. Auto-search findings below concern control-plane admission and application timing, not which corpora exist or how they synchronize.

## Findings

| ID | Class | Severity | TypeScript site | Rust/native site | One-line consequence |
|---|---|---:|---|---|---|
| R4-01 | STALE-PORT | CRITICAL | `packages/plugin/src/hooks/magic-context/strip-content.ts:454-507`; `packages/plugin/src/hooks/magic-context/module-wire.ts:631-646,681-691` | `crates/mc-module/src/transform.rs:10010-10093,10688-10693` | CK maps raw `reasoning` but not TS-recognized `thinking`/`redacted-thinking`, and drops reasoning `cache_control`; Rust therefore misses some invalid merged shapes and strips an exception TS preserves. |
| R4-02 | MISSING | CRITICAL | `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:827-851,1906-1928` | `crates/mc-module/src/transform.rs:3788-3792,10010-10034` | TypeScript can first-apply the B-arm on a subagent bust; Rust makes every subagent pass non-busting for this transition, leaving the merged-reasoning 400 shape untouched. |
| R4-03 | STALE-PORT | HIGH | `packages/plugin/src/hooks/magic-context/transform.ts:1237-1247`; `packages/plugin/src/shared/window-geometry.ts:573-595` | `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:1515-1541`; `crates/mc-module/src/transform.rs:3205-3212`; `crates/mc-module/src/scheduler.rs:502-511` | OpenCode Rust derives both `Emergency95` and host fail-closed from `usableSoft`; TypeScript’s absolute provider wall uses `usableHard`, so split-geometry models can abort or enter emergency materially early. |
| R4-04 | MISSING | HIGH (downgraded 2026-08-14 post-report: Thalamus measured 167,000 on all 494 production requests — the gateway already applies the 0.835 soft haircut before the wire, so the band-lateness half of this finding rested on reading the module's fallback arm as the production value and collapses to ~1k vs OC usable_soft; the MISSING HARD WALL half stands, with production evidence ex658 survived 176k / ex569 died 191k — session-death class. Fix reduced to the geometry wire struct, agreed with Thalamus.) | `packages/plugin/src/shared/window-geometry.ts:458-608` | `crates/mc-module/src/transform.rs:3205-3212,5113-5119`; `crates/mc-module/src/lib.rs:7326-7355` | The CC leg still treats Thalamus `context_limit` as the whole usable window: no overlay, placeholder filter, output reserve, provider geometry, soft/hard split, or hard-wall basis. |
| R4-05 | STALE-PORT | HIGH | `packages/plugin/src/hooks/magic-context/auto-search-runner.ts:241-409` | `crates/mc-module/src/transform.rs:3795-3820,4603-4623,7790-7863` | TS exposes a hint on the first call for the new user tail; Rust persists it as pending and withholds it until a later independent bust, so a one-step turn never sees it and a later pass can alter an older prefix. |
| R4-06 | DIVERGENT | HIGH | `packages/plugin/src/hooks/magic-context/auto-search-runner.ts:269-280` | `crates/mc-module/src/transform.rs:7588-7599` | TS requires the target user message to be the physical last array element; Rust skips synthetic/system/tool/ignored carriers and can attach a hint to a buried user message, creating a historical-prefix mutation. |
| R4-07 | DIVERGENT | MEDIUM | `packages/plugin/src/hooks/magic-context/auto-search-runner.ts:306-384` | `crates/mc-module/src/transform.rs:7865-8002,8037-8054` | Rust alone caps the sanitized query at 500 scalars and applies the configured score threshold to every result; TS searches the full prompt and gates only on the top result, changing searches and hint bullet counts. |
| R4-08 | MISSING | MEDIUM | `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1868-1882`; `packages/plugin/src/hooks/magic-context/auto-search-runner.ts:338-345` | `crates/mc-module/src/transform.rs:7865-7905` | TS removes memories already visible in `<session-history>` before building a hint; Rust has no rendered-memory exclusion input and can recommend memory the model already sees. |
| R4-09 | STALE-PORT | HIGH | `packages/plugin/src/hooks/magic-context/inject-compartments.ts:2066-2128,2188-2234` | `crates/mc-module/src/transform.rs:5168-5205`; `crates/mc-module/src/config.rs:82-115` | Batch 5 added mural bytes for the OpenCode host path only; CC config/request has no mural field, so identical stores produce different HARD m0 bytes. |
| R4-10 | STALE-PORT | HIGH | `packages/plugin/src/shared/prompt-surface-runtime.ts:136-179` | `crates/mc-module/src/config.rs:377-383`; `crates/mc-module/src/lib.rs:146-164,3659-3668,10497-10513` | CC does not resolve the standard trusted `guidance_override_path`; it only accepts an undocumented pre-resolved `_text` key, so normal user config falls back to built-in guidance. |
| R4-11 | MISSING | MEDIUM | `packages/plugin/src/hooks/magic-context/strip-content.ts:319-351`; `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1171-1194` | `crates/mc-module/src/transform.rs:10753-10765,10854-10867` | Historical reasoning age-clearing is still hard-gated to OpenCode in Rust; CC retains old reasoning bytes that the canonical TS policy clears. |
| R4-12 | MISSING | MEDIUM | `packages/plugin/src/hooks/magic-context/channel2-delivery.ts:134-324` | `crates/mc-module/src/transform.rs:8379-8419` | Rust computes/returns Channel-2 only for OpenCode; CC receives no one-shot ceiling reminder under the same reclaimable-tail pressure. |
| R4-13 | DIVERGENT | MEDIUM | `packages/plugin/src/hooks/magic-context/transform.ts:1789-1815` | `crates/mc-module/src/transform.rs:2855-2859` | TS may tag on the first eligible transform, while Rust explicitly denies CC bootstrap tagging and starts only after module initialization, shifting first-pass bytes and age bases. |
| R4-14 | DIVERGENT | HIGH | HARD/materialization gates at `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:906-943` and `packages/plugin/src/hooks/magic-context/inject-compartments.ts:2147-2240` | `crates/mc-module/src/transform.rs:3213-3234,5611-5641,24041-24118` | #313 remains valid: a nominal defer can run the real tokenizer to decide divergence tolerance, and an estimator change can flip the pass to a full-prefix HARD bust. |

## Detail and repros

### R4-01 — the B-arm ingress predicate is only partially transported

The TS planner classifies raw OpenCode parts with `REASONING_PART_TYPES = {reasoning, thinking, redacted-thinking}` and also checks the adjacent reasoning part’s `cache_control` (`strip-content.ts:454-507`). The OpenCode CK encoder maps only raw `type === "reasoning"` to `CkKind::Reasoning` and copies only its text (`module-wire.ts:631-646`). Raw `thinking` and `redacted-thinking` fall through as opaque blocks (`:681-691`), and the reasoning part’s `cache_control` is not represented.

Rust’s detector recognizes only `CkKind::Reasoning` and `CkKind::RedactedReasoning` (`transform.rs:10688-10693`). The common raw `reasoning` case therefore reaches Rust, but the other two TS-recognized cases do not, and Rust cannot honor TS’s cache-control exception. Existing Rust fixtures constructed directly from CK reasoning blocks prove the common module predicate but not these adapter edges.

There is a second predicate mismatch in the same function: TS requires `providerID === "anthropic"`; Rust rejects a known non-Anthropic provider but accepts `provider_id=None` (`transform.rs:10022-10026`). That is a smaller cold/recovery edge than the projection defect, but should be fixed in the same contract.

**Repro A:** encode two adjacent OpenCode assistant messages where the second has a `redacted-thinking` part plus answer text and is not the latest assistant. TS returns the first assistant id. CK carries the part as opaque, so Rust sees no reasoning block and creates no frozen strip unit.

**Repro B:** use raw `type: "reasoning"` with `cache_control` on the adjacent reasoning part. TS deliberately skips the candidate. CK carries a reasoning kind but loses `cache_control`, so Rust strips it on a bust.

**Required fix:** make the three reasoning kinds and cache-control exception explicit in the OpenCode CK contract (or transport host-computed candidate ids), then generate adapter-to-module goldens for all three kinds and the exception.

### R4-02 — independent B-arm implementations disagree on subagents

TS’s `isCacheBustingPass` includes ordinary subagent scheduler executes and emergency passes (`transform-postprocess-phase.ts:827-851`), and the merged-reasoning application block has no `fullFeatureMode` exclusion (`:1906-1928`). Rust defines `is_bust_pass` with a leading `!req.is_subagent` (`transform.rs:3788-3792`) and only mints applied-set units when that value is true (`:10010-10034`).

**Repro:** run a subagent request containing an eligible older merged assistant on a scheduler execute. TS records the message id and strips on that pass. Rust records no frozen unit and replays the reasoning. The newest-assistant exemption is not the issue; choose an older assistant.

**Required fix:** separate “provider prefix can be mutated on this pass” from the primary-only historian/materialization gate. Add a subagent adapter-level fixture with two consecutive assistant messages and prove first-apply plus replay.

### R4-03 — S1’s hard-wall denominator is discarded in OpenCode Rust mode

S1 intentionally returns two values. `usableSoft` drives normal scheduler/force/drain math; `usableHard` drives the absolute provider wall (`transform.ts:1237-1247`, `window-geometry.ts:573-595`). The Rust adapter resolves both, but sends only `usableSoft` as `usage.context_limit_tokens`; its preflight `emergencyFailClosed` reads the cached soft-based percentage (`rust-mode-transform.ts:1515-1541`). The module recomputes one percentage from input/soft (`transform.rs:3205-3212`) and maps `>=95` to `Emergency95` (`scheduler.rs:502-511`).

This is not a disagreement for cells where soft and hard happen to coincide. It is material for split cells, including an input-limited soft window with a larger provider hard wall. If soft is 128k and hard is 168k, the Rust 95% arm begins at 121.6k input while TS’s absolute wall begins at 159.6k.

**Repro:** provide geometry with `usableSoft < usableHard`, then choose input between `0.95*soft` and `0.95*hard`. TS remains below the absolute wall. The Rust adapter sets `emergencyFailClosed=true`, and the module reports `Emergency95`.

**Required fix:** transport both denominators (or a host-resolved hard-wall percentage/boolean). Keep execute, force, and drain on soft; use hard only for the absolute wall and provider-overflow fallback.

### R4-04 — CC still has no S1 geometry contract

`deriveWindowGeometry` consumes provider/model tables, Fusiform overlay facts, three-state geometry, placeholder-filtered output, configured output reserve, OpenCode’s 32k cap, Pi’s output floor, and prompt-wall margin before returning soft/hard (`window-geometry.ts:458-608`). The CC module receives only `ModuleUsage.context_limit_tokens`; `effective_context_limit_tokens` accepts that value as-is above the plausibility floor, and all module pressure/boundary math uses it (`transform.rs:3205-3212,5113-5119`). `ProducerContext` has no provider geometry, output reserve, overlay, or hard limit (`lib.rs:7326-7355`).

Thalamus sending `context_limit` therefore does not close R3-01. A combined 200k shared-output window remains 200k to the module where TS may schedule against roughly 168k, and the module has no independent provider-wall denominator.

**Repro:** send CC usage `input=120k, context_limit=200k` for a shared-upfront 200k/32k cell. The module computes 60%; the TS soft basis computes about 71.4%. The execute threshold, force bands, protected-tail target, emergency ceiling, Channel-1 pressure, and drain latch all move.

**Required fix:** a Thalamus-coordinated first-class wire structure containing at least `usable_soft`, `usable_hard`, and derivation identity/provenance. Do not duplicate the provider table in Rust unless one generated source owns both tables.

### R4-05 — auto-search decision persistence is not same-pass visibility

TS performs the search and CAS-appends the hint to the currently live user message before the first provider call returns (`auto-search-runner.ts:241-409`). Rust now computes and commits a durable hint decision on a defer, which fixes repeated decision work, but it adds the block id to `pending_user_hint_block_ids`; rendering excludes pending rows (`transform.rs:3795-3820,4603-4623`). That set is cleared only on a later `is_bust_pass`.

The result is worse than a small timing shift: a simple one-step turn never receives the hint. If a later tool step or turn creates a natural bust, Rust then mutates the now-older user message, whereas TS added the bytes while that message was the uncached tail.

**Repro:** bootstrap below execute pressure with one eligible new user message and a qualifying hit. TS’s outgoing request contains `<ctx-search-hint>`. Rust commits the row but serves the original user bytes. If no later model step occurs, the model never sees the hint; if a later bust occurs, the old user block changes then.

**Required fix:** distinguish an append to a genuinely unserved physical tail from a prefix mutation. Apply the former immediately in the same response and freeze the decision; retain bust-gating only for replay into an already-served prefix.

### R4-06 — Rust’s “tail” can be behind transport messages

TS first finds the latest meaningful user message, then separately requires that exact id to equal `messages[messages.length-1]` (`auto-search-runner.ts:269-280`). Rust reverse-scans while skipping synthetic, system, tool, and non-authored user carriers and accepts the prior authored user if no assistant intervenes (`transform.rs:7588-7599`).

That makes an ignored notification, synthetic carrier, system message, or tool-role message after the user invisible to Rust’s race guard. It can decide for a message that is not the physical tail and later attach bytes inside the already-built prefix.

**Repro:** `[authored user U, ignored synthetic user N]`. TS identifies U as meaningful but rejects because N is last. Rust skips N and returns U from `eligible_authored_user_tail`.

**Required fix:** preserve the semantic filtering used to identify an authored prompt, but require the selected message to be the physical final ingress item before same-pass mutation.

### R4-07 — query cap and threshold scope are not the TS controls

TS passes the entire sanitized prompt to `unifiedSearch`; it checks only `results[0].score < scoreThreshold` and, once admitted, renders up to three results regardless of lower-result scores (`auto-search-runner.ts:306-384`). Rust caps the query at 500 Unicode scalar values and truncates at a space (`transform.rs:8037-8054`). It also filters each candidate against the threshold before sorting/rendering (`:7957-8002`).

This finding is independent of the excluded corpus/search-engine delta. Even with identical ordered `(score, result)` inputs, TS renders scores `[0.8,0.6,0.5]` at threshold `0.7`; Rust renders only `0.8`. A prompt whose discriminating terms occur after scalar 500 is searched by TS and discarded by Rust.

**Required fix:** choose one generated control contract for query length units/cap and threshold scope. If the 500-character cap is intentional, add it to TS and configuration/docs rather than retaining a Rust-only constant.

### R4-08 — rendered-memory suppression was not ported

Before TS searches, postprocess obtains the ids currently rendered in `<session-history>` and passes them to auto-search (`transform-postprocess-phase.ts:1868-1882`). The search path removes matching memory hits (`auto-search-runner.ts:338-345`). Rust’s search function receives project/session/query/config only and loads every visible memory candidate (`transform.rs:7865-7905`); it has no `rendered_memory_ids` or equivalent exclusion.

**Repro:** render memory id 7 in m0 and make id 7 the strongest auto-search hit. TS filters it and either shows another result or persists no hint. Rust can emit id 7’s content again.

**Required fix:** pass `meta.rendered_memory_ids` into the hint search admission filter. This does not require solving I-03 corpus synchronization.

### R4-09 — mural transport closes OpenCode but not CC

The TS HARD composer resolves and inserts mural bytes into m0 (`inject-compartments.ts:2066-2128,2188-2234`). Batch 5 added `m0_mural` to the Rust `ProducerContext` and folds it into native m0 when the OpenCode host supplies it (`transform.rs:5168-5205`). `McModuleConfig` and the CC transform request still expose no mural bytes, enable flag, budget, or content epoch (`config.rs:82-115`).

**Repro:** same project/store with a persisted mural and a natural HARD. TS/OpenCode and OpenCode Rust include the mural. CC Rust cannot load or render it, so m0 content and hash differ.

**Required fix:** add daemon-owned CC mural resolution and a content identity to the route/request, using the same rendered byte producer as TS/OpenCode. Avoid reading mutable mural state on a defer.

### R4-10 — the CC guidance fix transports a key normal config never produces

TS accepts user-tier `prompt_surface.guidance_override_path`, resolves relative paths from the user config directory, validates file/nonempty/single marker, and falls back with a warning (`prompt-surface-runtime.ts:136-179`). Rust config instead reads `/prompt_surface/guidance_override_text` and explicitly ignores project injection (`config.rs:377-383`). Production route bind calls `effective_for_project` on the raw config files (`lib.rs:3659-3668,10497-10513`) and then copies that optional text into CC requests (`:146-164`). No production code resolves `guidance_override_path` into `_text`; the passing test injects a pre-resolved `McModuleConfig` programmatically.

**Repro:** set the documented user key `guidance_override_path` to a valid file and bind a CC route. TS uses the file. Rust ignores the path because `McModuleConfig` never reads it and serves built-in guidance.

**Required fix:** resolve/validate the path at the trusted route boundary and store the resolved immutable text in `McModuleConfig`, or add a generated host-resolved field. Keep project-tier text/path rejected.

### R4-11 — CC historical reasoning cleanup remains absent

TS’s `clearOldReasoning` is provider-shape policy, protected by the newest-assistant exemption and watermark (`strip-content.ts:319-351`, `transform-postprocess-phase.ts:1171-1194`). Rust returns no cutoff unless the serializer is `OpencodeAiSdk` (`transform.rs:10753-10765`) and likewise only runs served-reasoning clearing for that profile (`:10854-10867`). The unconditional newest exemption from batch B is present; it does not make the CC age lane exist.

**Repro:** a CC request with an old non-newest reasoning block beyond the configured cutoff. TS policy emits a sentinel/removal on a bust. Rust computes `None` and retains it.

**Required fix:** define a CC-safe representation (respecting Anthropic signatures) and port the same watermark/age contract, or document a deliberate non-parity exception with a compensating token/safety policy.

### R4-12 — CC Channel-2 remains absent

TS arms and delivers a one-shot synthetic ceiling reminder after revalidating pressure (`channel2-delivery.ts:134-324`). Rust’s directive computation explicitly returns `None` unless the serializer profile is `OpencodeAiSdk` (`transform.rs:8379-8390`). There is no CC host-delivery field or callback.

**Repro:** same active-tag/reclaimable state above the one-third usable threshold. TS/OpenCode and Rust/OpenCode produce/deliver the reminder; Rust/CC produces no directive.

**Required fix:** add a Thalamus-delivered CC directive with the same pending/claimed/delivered lease and revalidation contract.

### R4-13 — CC bootstrap tagging still starts one pass late

TS can mint tags on the first eligible transform (`transform.ts:1789-1815`). Rust’s bootstrap activation is explicitly `OpencodeAiSdk` only; CC waits for `loaded.meta.initialized` plus persisted surface state (`transform.rs:2855-2859`). This changes first-pass provider bytes and shifts tag-derived age/floor calculations.

**Repro:** first CC transform with eligible assistant/tool content and no module state. TS canonical path produces tag overlays. Rust’s `tagging_active` is false and emits none; the next initialized pass can differ.

**Required fix:** permit CC bootstrap tagging under the same requested-surface predicate and add a first-call golden, or freeze a documented CC exception rather than silently starting on pass two.

### R4-14 — #313 is confirmed; the current test is a false proxy

Before classification, every initialized row without `publication_floor_ordinal` calls `protected_tail_floor_allowance` (`transform.rs:3213-3234`). That helper directly calls `mc_tokenizer::estimate_tokens` for every non-system live block (`:5611-5641`). Its resulting ordinal span controls whether boundary divergence is tolerated or forces recut/HARD.

The test named `token_estimator_is_hard_only_never_called_on_soft_or_defer` passes a counting closure only to `apply_once_with_estimator` and observes m0 composition (`:24041-24118`). The violating helper does not call that closure; it calls the global estimator directly. The test can therefore report zero calls on SOFT/defer while real tokenization occurred.

The decision surface is the protected floor, clamped to **2k–12k tokens** (`crates/mc-module/src/boundary.rs:24-28`). An estimator/version change near that floor can change the tolerated ordinal span; if the inequality flips, the consequence is not a 2k–12k local edit but a full m0/prefix HARD rebuild and cache bust. Scope remains legacy/partially seeded rows lacking `publication_floor_ordinal`.

**Required fix:** persist/backfill the floor under an already-HARD pass or compute it with a frozen estimate captured at publication. Route every estimator call through the injected counting interface and add a non-vacuity test that deliberately makes a defer-side call fail.

## R3/post-fix re-verification ledger

Closed items below were checked in current source and are **not** re-reported as findings:

| Area | Round-4 verdict |
|---|---|
| Effective execute threshold wire | **Closed.** OC serializes `effective_execute_threshold`; serde accepts it; route construction consumes the request value before the bind-time scalar. Escalation formulas remain `max(85, threshold+2)` and absolute 95. |
| Memory budget key/default and project-tier escalation | **Closed.** Rust uses `memory.injection_budget_tokens`, 4k default, and project tiers cannot set memory/profile/historian spend leaves. No current project-tier escalation path was found. |
| Emergency floor / honest reclaim | **Closed.** Active tag classes contribute to floor/reclaim accounting and signed reasoning is not counted as reclaimed unless actually removed. |
| Newest-assistant exemption (#161) | **Closed for all profiles.** `latest_assistant_reasoning_mutation_exempt_mid` is unconditional by serializer/mid-turn; R4-11 is the separate CC historical-age lane. |
| Duplicate/supersession opportunity | **Closed.** Dedup participates in ordinary execute; supersession uses the newest active tag floor, and the level-vs-edge gate remains correct. |
| Surgical injected-block strips, image no-age default, edit-marker UTF-16 | **Closed** by batch 4; current golden coverage is present. |
| OC mural | **Closed on natural HARDs.** Mural bytes and identity are passed to Rust m0; R4-09 is CC-only residue. |
| Unknown prompt tool-description keys | **Closed.** Unknown keys warn/ignore rather than reject the transform. |
| OpenCode guidance override | **Closed.** Host resolves trusted text and sends it; R4-10 is CC config resolution. |
| Compaction-off | **Closed.** Module runs additive-only, suppresses mutating overlays/strips/historian, and both OC/CC config carry the mode. |
| Canonical compaction summary | **Closed.** Rust-mode postprocess rebuilds the canonical persisted summary after the synthetic head. |
| Todowrite verdict | **Closed/fail-closed.** OC combines tool-map and permission availability; CC `None` normalizes unavailable and cannot synthesize an unreachable todo call. |
| `cache_ttl` three-state contract | **Closed for CC.** Absent/default, explicit empty, and finite paid TTL remain distinct. |
| Caveman | **Closed for configured enable/minimum and frozen pass-start max-tag age basis.** |
| Smart-drops config gate | **Closed.** User/project enablement reaches native selection without bypassing the supersession edge gate. |
| LKG interactions | **No new delta.** Postprocessed native output (including canonical summary and host note/search overlays) is what the OC LKG captures/replays. Deep-holder accounting changes retention telemetry/limits, not transform bytes. |
| #315 item 1 | **Verified.** Rust has `forced_ids` in `m1_compose.rs:359-416`, removes them from ordinary numeric additions, caps them, and appends them outside quarter-budget trim. Pi drift is out of scope. |
| I-04 hint bytes | **Common fixture byte-identical.** A three-memory `alpha/beta/gamma` fixture including the native outer `\n\n` append matched exactly at 234 UTF-8 bytes. R4-05/R4-07/R4-08 can still change whether/how many fragments reach that renderer. |
| I-06 / I-07 / I-12 / I-14 / I-22 | **No fresh delta found** beyond the separately listed B-arm and auto-search findings. Expand ordering, reduce acknowledgements, historian retry ladder, caveman basis, and adjacency safety remain present. |

## Fresh-risk verdicts

- **Window-geometry floor-clamp log (`d85a6e89`)**: diagnostics-only. The new once-log does not change `flooredReserve`, `usableSoft`, or `usableHard`; no parity delta beyond R4-03/R4-04.
- **Deep memory accounting (`78b28941`)**: holder charging and telemetry changed; no transform-byte branch was found. Memory-holder ownership remains a separate audit. The change does not close #313 because `protected_tail_floor_allowance` still calls the global tokenizer.
- **Conflict detector explicit booleans (`9cb94a2d`)**: boot conflict detection only. Missing/nonboolean resolved compaction now falls back to file detection and does not alter an already-selected transform mode.
- **Transport orphan rejection (`be4bf30c`)**: promise-observation/error-surface only; accepted module responses, LKG selection, and transform bytes are unchanged.
- **Config trust boundaries**: project tiers cannot set the Rust-only resolved guidance text or the user-tier spend/model controls. No escalation path was found. R4-10 is a missing documented user feature, not a project-security bypass.

## Verification performed

- `cargo test -p mc-module`: **passed** — 893 passed, 4 ignored in the library target; integration targets also passed (1 + 1).
- Dependency-independent TS suites (`auto-search-hint.test.ts`, `module-wire.test.ts`, `conflict-detector.test.ts`): **passed** — 61 tests.
- Direct TS-vs-Rust hint fixture: **passed**, byte-identical 234-byte rendered append.
- A broader eight-file Bun selection loaded 61 tests but five suites could not load because this worktree lacks the internal package `@cortexkit/subc-client`. The dependency-independent subset above was rerun and passed. This is an environment/package-resolution limitation, not a test assertion failure.
- Static adapter-to-module review was used for R4-01 and R4-14 specifically because the existing Rust fixtures do not cover R4-01’s raw-part mapping edges and R4-14’s injected estimator cannot observe the direct global call.

## Proposed fix batches

1. **B-arm ingress and transition contract — R4-01/R4-02.** One cross-language worktree: extend CK or send host candidate ids, preserve cache-control/provider predicates, split subagent bust eligibility, and add adapter-level non-vacuity fixtures. These findings overlap `module-wire.ts`, postprocess, and `transform.rs` and should land atomically.
2. **Geometry contract — R4-03/R4-04.** Joint plugin/module/Thalamus work: transport soft and hard values plus provenance; route only the absolute wall to hard while retaining soft for execute/force/drain/boundary. Do not patch CC with another raw scalar.
3. **Auto-search control/application — R4-05 through R4-08.** Shared TS/Rust fixtures for physical-tail eligibility, same-pass unserved-tail append, query cap units, threshold scope, and visible-memory exclusion. This is separate from I-03 corpus synchronization.
4. **CC provider-visible composition — R4-09/R4-10.** Route-resolve mural and trusted guidance into immutable CC bind state/content identity. Both affect m0/guidance bytes but do not overlap reduction selection.
5. **CC profile behavior — R4-11 through R4-13.** Thalamus-coordinated historical reasoning safety, Channel-2 lease/delivery, and first-pass tag bootstrap. Keep this separate from composer bytes because it overlaps transform state and host callbacks.
6. **Estimator fence — R4-14.** Isolated Rust patch: eliminate defer-side global tokenization, persist/backfill under HARD, and replace the proxy test with an injected estimator that every call path must traverse.
