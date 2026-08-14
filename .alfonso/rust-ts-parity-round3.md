# Rust ↔ TypeScript transform parity — round 3

> **Disposition ledger (2026-08-14):** Batch A merged 18f2e2f1 (R3-02 threshold wire consumption, R3-03 budget key/default, R3-04 project-tier escalation, R3-17 config transport). Batch B merged 93346233 (R3-08 emergency floor, R3-09 honest reclaim via arm b, R3-12 unconditional newest-assistant exemption). Open: R3-01 (geometry transport — joint with Thalamus, ties to the I-10 sequencing ladder), R3-05..07 (auto-search batch 3), R3-10/11/14/15/24 (reduction edges batch 4 residue), R3-13/22/23 (CC profile batch 7, Thalamus-coordinated), R3-16/20/21 (composer batch 5), R3-17 output half + R3-18/19 (host-mode batch 6), R3-25 (#313 estimator — with the memory-accounting work merged 78b28941, evaluate remaining scope).

Audit base: `b81357458326574b91875002aaeaa42135dd1311` (2026-08-14).

## Scope and severity

This audit followed the OpenCode TypeScript path from `transform.ts` through postprocess, compartment injection, frozen strip/replay, scheduler and limit resolution, then followed the native path through `rust-mode-transform.ts`, `transform.rs`, `selection.rs`, `boundary.rs`, `injection.rs`, `m0_compose.rs`, and `historian.rs`. It also checked the Claude Code profile and config path used by Thalamus. Corpus/data-plane parity (I-03), Pi↔OpenCode drift, and memory-holder ownership are excluded as requested.

**CRITICAL** means overflow/400/security risk; **HIGH** means deterministic provider-wire or major feature drift; **MEDIUM** means scheduling/reclaim drift; **LOW** means an edge-case byte difference. `STALE-PORT` means a port exists but later work changed one side.

## Findings

| ID | Class | Severity | TypeScript site | Rust/native site | Consequence |
|---|---|---:|---|---|---|
| R3-01 | MISSING | CRITICAL | `shared/window-geometry.ts:458-595`; `event-resolvers.ts:18-85` | `transform.rs:4666-4678,2833-2850` | CC consumes Thalamus `context_limit` raw: no S1 output reserve, overlay, provider geometry, usableSoft/usableHard split, or hard wall. |
| R3-02 | STALE-PORT | CRITICAL | `event-resolvers.ts:166-245`; `rust-mode-transform.ts:1543-1549,1705` | `transform.rs:541-655`; `lib.rs:7007-7025`; `config.rs:87-120` | OC computes/sends the model/tokens-aware threshold, but Rust drops it and uses the bind-time scalar; all bands and protected-tail math use the wrong ceiling. |
| R3-03 | STALE-PORT | HIGH | `config/schema/magic-context.ts:573-603,978-984,1060`; `hook.ts:1358-1362` | `config.rs:23-31,105-120,298-304`; `lib.rs:7018-7023` | TS defaults `memory.injection_budget_tokens` to 4k; Rust reads nonexistent `memory.budget_tokens` and defaults to 8k, changing ordinary m0/m1 bytes. |
| R3-04 | MISSING | CRITICAL | `config/schema/magic-context.ts:70-104,573-613`; `config/project-security.ts:187-227` | `config.rs:298-333` | A project can set undocumented Rust-only memory/profile/historian budget keys that TS strips as unknown, enlarging provider-visible profile data and historian spend. |
| R3-05 | STALE-PORT | HIGH | `transform-postprocess-phase.ts:1841-1863`; `hook.ts:1350-1362` | `rust-mode-transform.ts:1689-1722`; `transform.rs:576-587` | OC Rust never forwards auto-search enabled/score/minimum controls, so native defaults override the user's resolved config. |
| R3-06 | DIVERGENT | HIGH | `auto-search-runner.ts:250-305` | `transform.rs:3420-3432,7165-7200` | TS decides on a new live user tail immediately; Rust decides only while another action already busts, delaying/suppressing low-pressure hints. |
| R3-07 | STALE-PORT | MEDIUM | `auto-search-runner.ts:296-309`; `auto-search-hint.ts:55-130` | `transform.rs:6863-6932,7167-7210,7468-7520` | ASCII hint bytes match, but Rust lacks stacked-augmentation suppression, checks minimum length after sanitize/cap, and counts Unicode scalars instead of UTF-16 units. |
| R3-08 | DIVERGENT | CRITICAL | `emergency-drop.ts:106-200` | `selection.rs:981-1010,1166-1180` | TS derives the emergency fixed floor from every active live tag; Rust subtracts tool arcs only and can remain above the safe ceiling. |
| R3-09 | STALE-PORT | CRITICAL | `tool-drop-target.ts:383-408`; `emergency-drop.ts:76-100` | `selection.rs:239-274,733-771`; `transform.rs:9556-9649` | I-22 prevents adjacency collapse, but Rust retains reasoning TS clears while counting those bytes as reclaimed; wire differs and emergency selection can stop early. |
| R3-10 | STALE-PORT | MEDIUM | `transform-postprocess-phase.ts:906-943,972-1038`; `heuristic-cleanup.ts:202-258` | `transform.rs:3095-3108,3213-3242`; `selection.rs:1160-1165` | I-08's duplicate selector runs only through the Rust ride/two-pass gate; TS runs it on every ordinary scheduler execute. |
| R3-11 | DIVERGENT | MEDIUM | `transform-postprocess-phase.ts:1247-1305`; `apply-operations.ts:80-141` | `selection.rs:89-92,440-531,1125-1158` | Rust adds an owner-message K=20 supersession floor; TS protects the newest active tag window, so Rust retains arcs TS supersedes. |
| R3-12 | STALE-PORT | CRITICAL | `transform-postprocess-phase.ts:751-773,1933-1943` | `transform.rs:2358-2360,9840-9863` | TS always exempts the latest assistant; Rust does so only for an OC mid-turn request, exposing completed signed reasoning to rewrite/400 risk. |
| R3-13 | MISSING | MEDIUM | `transform-postprocess-phase.ts:1880-1943` | `transform.rs:9884-9914` | Historical reasoning age-clearing remains OC-only in Rust; CC retains bytes TS would clear (I-09 remains open). |
| R3-14 | DIVERGENT | MEDIUM | `system-injection-stripper.ts:1-48`; `heuristic-cleanup.ts:162-199` | `transform.rs:8065-8117,8250-8265,8331-8337` | TS surgically removes injected blocks from mixed messages and recognizes two more markers; Rust only neutralizes whole injected messages. |
| R3-15 | DIVERGENT | LOW | `strip-content.ts:620-680` | `transform.rs:8206-8209,8289-8315` | With no tag age, TS strips an untagged processed image on a bust; Rust requires a positive tag/cutoff and preserves it. |
| R3-16 | MISSING | HIGH | `inject-compartments.ts:2188-2234`; `features/magic-context/mural/` | `m0_compose.rs:97-133,265-322` | TS injects the mural in HARD m0; Rust has no mural input or rendered block. |
| R3-17 | MISSING | HIGH | `transform.ts:749-815` | `rust-mode-transform.ts:1398-1408`; `transform.rs:541-655` | Compaction-off splits three ways: TS still injects memory/docs, OC Rust returns raw, and CC Rust has no mode field and continues full compaction. |
| R3-18 | MISSING | HIGH | `transform-postprocess-phase.ts:391-474` | `module-wire.ts:153-168`; `transform-postprocess-phase.ts:327-380` | OC Rust ingress removes the compaction summary, but native postprocess never rebuilds the canonical persisted summary after the synthetic head. |
| R3-19 | DIVERGENT | MEDIUM | `transform-postprocess-phase.ts:201-250`; `ctx-reduce-availability.ts:405-413` | `transform.rs:615,3140-3150`; `injection.rs:199-269`; `lib.rs:145-160` | TS freezes tool-map plus live permission denial; OC Rust sends map presence only and CC no verdict, so native todo synthesis fails open. |
| R3-20 | MISSING | HIGH | `prompt-surface-runtime.ts:136-178,216-221`; `system-prompt-hash.ts:346-365` | `prompt_surface.rs:13-28`; no override field in `config.rs`/`lib.rs` | CC guidance cannot honor the trusted user `guidance_override_path`. |
| R3-21 | DIVERGENT | MEDIUM | `prompt-surface-runtime.ts:181-213` | `lib.rs:6402-6460` | TS warns and ignores an unknown tool-description key; Rust rejects the entire prompt-surface selection/transform. |
| R3-22 | MISSING | MEDIUM | `transform.ts:2565-2604`; `channel2-delivery.ts:1-118` | `transform.rs:4428-4444,7720-7805`; CC profile suppressed | Channel-2 ceiling delivery exists for TS/OC and Rust/OC but is absent on CC. |
| R3-23 | DIVERGENT | MEDIUM | `transform.ts:1754-1815` | `transform.rs:2492-2519` | TS may tag the first eligible pass; Rust deliberately denies bootstrap tags to CC and activates them later. |
| R3-24 | DIVERGENT | LOW | `tool-drop-target.ts:209-239` | `selection.rs:562-615` | Edit-marker region caps use UTF-16 units in TS but Unicode scalars in Rust, producing different astral-text payload bytes. |
| R3-25 | DIVERGENT | HIGH | bust/materialization gates at `transform-postprocess-phase.ts:906-943`; `inject-compartments.ts:2147-2240` | `transform.rs:2838-2848,5116-5146,5180-5222` | #313 is valid: legacy rows tokenize on every pass, and estimator changes can flip boundary-recut/HARD and full-prefix cache bust on a nominal defer. |

## Detail

### R3-01 — S1 geometry is host-only and CC is uncarved

`deriveWindowGeometry` resolves provider/auth and overlay facts, filters placeholder output values, applies configured output reserve, distinguishes considered-unknown geometry, and returns `usableSoft` and `usableHard`. The OC adapter is correct: it passes `usableSoft` as `usage.context_limit_tokens` and uses `usableHard` only for raw-fallback/provider-wall checks (`rust-mode-transform.ts:1453-1479,1527-1549`). Rust recomputes usage percentage from that same soft denominator.

CC has no equivalent. `effective_context_limit_tokens` returns supplied `ModuleUsage.context_limit_tokens` (only a generic sane-range check), and scheduler/boundary math consumes it. Rust contains no S1 constants/table/overlay. A 200k combined shared-output window can therefore schedule as 200k where TS schedules near 168k and enforces a different hard wall.

### R3-02 — resolved threshold is sent but not consumed

The adapter resolves `execute_threshold_tokens`/per-model percentage against S1 soft geometry and serializes `effective_execute_threshold`. `TransformRequestWire` has no such field, so serde ignores it. `ProducerContext.execute_threshold_percentage` always comes from bind-time scalar config. Force bands, Channel 2, protected-tail sizing, emergency ceiling, and CC all inherit the stale scalar.

### R3-03/R3-04 — budget schema and trust mismatch

TS's standard key is `memory.injection_budget_tokens`, default 4,000, and the hook forwards it. Rust reads `/memory/budget_tokens` and otherwise keeps 8,000. With no user setting, enough memories yield different selected sets and m0 hashes.

Rust also accepts project `/memory/budget_tokens`, `/memory/user_profile_budget_tokens`, and `/historian/context_limit_tokens`. None exists in the TS schema. The profile budget is unbounded and can enlarge provider-visible global user-profile data; the historian value can raise a derived chunk to 50k. Existing protection for historian model/fallbacks and scalar-threshold lowering is sound; these undocumented leaves are the escalation path.

### R3-05 through R3-07 — auto-search

I-02/I-04 closed CC control transport and common ASCII rendering, but OC `buildTransformBody` sends none of the three controls. Scheduling also differs: TS safely decides on an unserved last user message on any pass; Rust's `maybe_decide_user_hint_on_bust` waits for a natural bust.

For an ASCII memory fixture, both sides render the wrapper, pluralized header, 80-character fragments, and 800-character total cap identically. Residual counterexamples remain: TS suppresses stacked augmentation while Rust can add another hint; TS checks raw prompt length before sanitizing while Rust checks the sanitized/capped query; astral Unicode crosses JS and Rust caps differently.

### R3-08/R3-09 — emergency population and actual reclaim

TS explicitly passes every active tag type as `floorTags`, subtracting live text/file/reasoning/tool bytes from observed input to recover the irreducible prefix. Rust derives `all_active_reclaim_tokens` from tool arcs only, folds other live tail into its fixed floor, and under-computes reclaim.

I-22's adjacency detector closes the Anthropic merged-assistant 400 shape, but it does not close byte/accounting parity. TS full tool drop calls `clearThinkingParts`; Rust emits call/result targets only. Yet `ToolArc::reclaim_bytes` includes `reasoning_bytes`, so Rust can declare a target met while those bytes remain.

### R3-10/R3-11 — duplicate and supersession opportunities

I-08's owner fingerprint, safe list, keep-newest rule, and full-drop mechanics match. The opportunity does not: TS runs dedup on every ordinary execute; Rust needs a ride or two-pass batch. The newer K=20 owner-message supersession floor is also Rust-only. The level-vs-edge fix itself is present: an armed but idle emergency latch does not trickle supersession.

### R3-12/R3-13 — reasoning exemptions

TS identifies the latest assistant and always passes it as `reasoningMutationExemptMessage`. Rust returns an exemption only for `OpencodeAiSdk && mid_turn`, exposing a completed latest assistant to residual stripping. Separately, historical reasoning cutoff is gated to OC, so CC lacks I-09. Signature safety may justify representation differences, but it does not make the bytes equivalent.

### R3-14/R3-15 — frozen strips

TS can remove an embedded reminder/directive while preserving authored text and recognizes the idle-background/subagent-claim warnings. Rust mints `system_injected` only when the whole non-user message matches. Processed-image replay otherwise aligns; the edge is that TS defaults missing tag age to zero while Rust requires a positive tag/cutoff.

### R3-16 — mural is confirmed absent

TS resolves and appends the persisted mural during HARD m0. `M0Inputs` has memory, profile, docs, and compartments but no mural field, renderer, or content epoch. This is the known gap, not a differently named implementation.

### R3-17 — compaction-off is not one contract

TS off mode skips tags, strips, heuristics, historian folds, and markers but still runs additive memory/docs injection. OC rust-mode returns before the module and serves raw, losing additive injection. CC receives no off field and `McModuleConfig` does not parse `compaction.enabled`, so it continues the whole pipeline.

### R3-18/R3-19 — summary and todo host decisions

`module-wire.ts` filters the raw OpenCode summary and records normalization. TS canonicalization removes stale summaries, tags the persisted one, and inserts it after the synthetic head. `runRustModePostprocess` only replays note/search anchors, so native output lacks the summary and shifts array/cache bytes.

TS todo synthesis combines a frozen tool-map verdict with live permission denial. OC native transports only map presence; CC transports nothing, and `None` deliberately fails open. A denied/absent tool can leave a synthetic call/result on wire.

### R3-20/R3-21 — prompt-surface residuals

Built-in full/light selection and known tool overrides are live. TS reads a trusted user guidance file and falls back with a warning; CC has no field/reader. TS also warns and ignores unknown description keys, while Rust rejects the complete request, turning a typo into native failure/LKG rather than warning.

### R3-22/R3-23 — CC profile deltas

TS arms and asynchronously delivers Channel 2; Rust computes an equivalent host directive only for OC and tests assert CC has none. CC also cannot activate tags on bootstrap because `bootstrap_tagging_active` is hard-gated to `OpencodeAiSdk`.

### R3-24 — edit-marker UTF-16 versus scalar units

TypeScript's edit-marker `safeSlice` caps JavaScript UTF-16 units and backs off a split surrogate pair. Rust caps Unicode scalar values. BMP text matches; astral-heavy diff text near the region boundary produces different payload bytes. Caveman does **not** share this defect: both sides compare UTF-8 byte size (`tag.byteSize` / `row.source_bytes.len()`).

### R3-25 — #313 confirmed

Before pass classification, any initialized legacy row without `publication_floor_ordinal` calls `protected_tail_floor_allowance`, tokenizing every non-system live block. Its ordinal span controls `coverage_gap <= live_tail_allowance`. An estimator/version change can therefore flip whether a boundary-divergence recut forces HARD and busts the full m0 prefix. Scope is limited to legacy rows missing that floor, but the cache consequence is real.

## Fresh-batch verdicts and checked non-findings

| Area | Verdict |
|---|---|
| I-02 | **CC closed; OC still open** as R3-05. |
| I-04 | **ASCII fixture closed; residual gate/Unicode drift** R3-07. |
| I-06 `ctx_expand` | **Closed**: verbose range previews precede full single-message expansion. |
| I-07 `ctx_reduce` | **Closed**: unknown tags named; zero-target refused without commit; acknowledgements distinguish states. |
| I-08 | **Mechanics closed; opportunity incomplete** R3-10. |
| I-12 | **Closed**: ×1.15, max three retries, strict `>1.05 × budget`. |
| I-14 | **Closed**: CC transport, UTF-8 minimum-size basis, and frozen pass-start max-tag age basis align. |
| I-22 | **Adjacency 400 fix closed; actual-reclaim/bytes incomplete** R3-09. |
| Threshold/escalation bands | Formulas/constants match; input threshold and emergency population do not (R3-02/R3-08). |
| `cache_ttl` three-state | **Closed for CC**: absent/default emits no marker override, explicit empty means provider default/no paid TTL, explicit finite maps to `5m|1h`. |
| LKG | **No new delta**: native output plus host note/search overlays is captured and replayed with live tail. It preserves other divergences rather than introducing one. |
| smart-drops gate | **Live on both sides**; K=20 representation differs (R3-11). |
| prompt presets | Built-in full/light and known overrides are live; residual policy is R3-20/R3-21. |
| #315 item 1 | **Rust has `forced_ids`** at `m1_compose.rs:359-416`, capped, removed from numeric additions, and appended outside quarter-budget trim. Pi parity is out of scope. |

## Proposed fix batches

1. **Geometry/threshold transport (R3-01, R3-02):** first-class soft/hard geometry and effective threshold contract across TS adapter, Thalamus, `lib.rs`, `transform.rs`, scheduler/boundary.
2. **Config schema/trust (R3-03, R3-04, R3-17 config half):** align `injection_budget_tokens` and defaults, reject undocumented project leaves, transport `compaction.enabled`.
3. **Auto-search control plane (R3-05–R3-07):** forward OC controls, separate live-tail decision from bust replay, stacked suppression, shared UTF-16 cap fixtures.
4. **Reduction correctness (R3-08–R3-12, R3-14/R3-15/R3-24):** one sequential worktree because all overlap `selection.rs`/`transform.rs`; fix actual-reclaim accounting before opportunity/byte edges while retaining I-22 safety.
5. **Composer/prompt bytes (R3-16, R3-20, R3-21):** mural input/content epoch, trusted guidance transport, normalized invalid-description policy.
6. **Host representation/mode (R3-17 output half, R3-18, R3-19):** reuse TS additive off-mode, marker, and permission decisions around native serving.
7. **CC transport/profile (R3-13, R3-22, R3-23):** Thalamus-coordinated reasoning safety, Channel-2 delivery, and bootstrap-tag behavior.
8. **Estimator fence (#313/R3-25):** isolated Rust patch to persist/backfill the floor or delay legacy estimation until already-HARD, with a non-vacuity test proving SOFT does no estimator work and cannot flip recut state.
