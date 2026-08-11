# CC-leg parity baseline: OpenCode and Pi reference

Measured 2026-08-11 from the live telemetry stores, opened read-only. The reusable analyzer is `packages/plugin/scripts/cache-parity-baseline.ts`.

## Executive result

**Bust frequency is divergent.** Across 25 primary OpenCode sessions with at least 200 measured consecutive pass pairs, OpenCode produced **3.12 dashboard-degraded passes per 200** (`cache% < 95%`) and **2.81 hard busts per 200** (`cache% < 80%`). The supplied Claude Code observation is approximately **25 red/orange passes per 200**, a gap of **21.88 per 200** and roughly **8.0×** the OpenCode rate. The repository's own `Magic CTX` session was lower still at **1.70 per 200**.

The sole joinable long Pi run produced 22.35 degraded passes per 200, numerically comparable to CC, but 189 of its 209 below-90% m0-stable passes coincided with coarse drop telemetry. It is one workload and is not evidence that Pi's general baseline is 22/200.

Hard folds are also divergent: **1.01/100 on OpenCode** and **0.56/100 on Pi**, versus **9.8/100 on CC**. OpenCode's m0-stable share was **98.99%**, versus **89.7% on CC**.

The corrected CC structural classification is used here: of 56 below-90%-on-m0-stable passes, 44 first diverged at m1, 7 at tool-argument supersession, 2 at agent drop, 2 at an overlay tag on an already-sent message, and 1 in the tail. No claim is made that a CC m0 anchor was reachable or paying; the withdrawn byte-to-token anchor arithmetic is not used.

## Scope and sources

- `context.db`: migration-38 `transform_decisions`, `session_meta`, and `session_projects`.
- OpenCode usage: assistant `message.data.tokens` in `~/.local/share/opencode/opencode.db`.
- Pi usage: assistant entries under `~/.pi/agent/sessions/**/*.jsonl`.
- Module state: `~/.local/share/cortexkit/magic-context/store.db`, specifically `mc_cache_state` and `mc_pass_trace`.
- OpenCode sample: 25 primary managed sessions, 45,168 consecutive pairs, 45,181 decision rows, 45,169 exact message-id usage joins, and 12 missing usage joins. Subagents were excluded using `session_meta.is_subagent`.
- Pi sample: one primary session, 1,978 consecutive pairs, 2,000 decisions, 1,978 exact joins, and 22 missing joins. A second 2,000-row Pi decision run had 2,177 assistant usage entries (2,140 with positive cache reads) but zero exact message-id matches, so it was excluded rather than joined by time.
- `transform_decisions` retains at most the newest 2,000 rows per session/harness. A 2,000-pair result is therefore a retained-window measurement, not the lifetime pass count.

All live databases were opened with Bun SQLite's `readonly: true`; no migration or write statement was run.

## Cache percentage formula

The primary formula is the dashboard's consecutive-step retention calculation from `packages/dashboard/src-tauri/src/db.rs:1505-1523,1581-1592,1704-1763`:

```text
previous_output = max(previous.total - previous.input - previous.cache_read - previous.cache_write, 0)
growth = previous.cache_write > 0 ? previous.cache_write : previous.input + previous_output
expected = previous.cache_read + growth
cache% = current.cache_read / expected * 100
```

When the preceding event has no positive cache-read baseline, the dashboard leaves its single-row value in place: `current.cache_read / (current.input + current.cache_read + current.cache_write) * 100`.

The analyzer also reports that single-row prompt-share formula as an alternative for every pass, citing `packages/dashboard/src/lib/cache-format.ts:105-120`. The OpenCode aggregate changes as follows:

| Metric | Dashboard retention | Prompt-share alternative | Verdict change? |
|---|---:|---:|---|
| degraded passes/200 (`<95%`) | 3.12 | 3.82 | No; both remain far below CC's ~25 |
| hard busts/200 (`<80%`) | 2.81 | 2.83 | No |
| median cache% on m0-stable | 100.00% | 99.84% | No; both comparable to CC's 99.0% |
| median cache% on fold | 1.58% | 1.92% | No; both diverge from CC's 54.3% |
| below-90% on m0-stable | 0.485% | 0.566% | No; both diverge from CC's 16.5% |

Pi's alternative is also non-flipping: 28.31 versus 22.35 degraded passes/200, 98.99% versus 99.34% stable median, 0% fold median under both formulas, and 11.34% versus 10.63% below-90%-on-stable.

## Aggregate reference table and CC verdicts

“Comparable” below means the supplied values are close in practical scale; “divergent” means the gap is large enough to change the cache-behavior conclusion. Sample size is not itself a behavior verdict.

| Metric | Claude Code supplied | OpenCode reference | Pi reference | OC verdict / gap | Pi verdict / gap |
|---|---:|---:|---:|---|---|
| Consecutive pass pairs | 379 | 45,168 | 1,978 | Sample size only | Sample size only |
| **Degraded/bust passes per 200 (`cache% <95%`)** | ~25 | **3.12** | **22.35** | **DIVERGENT: -21.88; CC is ~8.0× OC** | COMPARABLE numerically: -2.65; one drop-heavy run |
| Hard busts per 200 (`cache% <80%`) | Not separately supplied | 2.81 | 22.24 | NOT COMPARABLE | NOT COMPARABLE |
| m0-stable rate | 89.7% | 98.995% | 99.444% | **DIVERGENT: +9.30 points** | **DIVERGENT: +9.74 points** |
| Median cache% on m0-stable | 99.0% | 100.0% | 99.34% | COMPARABLE: +1.0 point | COMPARABLE: +0.34 point |
| Median cache% on fold | 54.3% | 1.58% | 0% | **DIVERGENT: -52.72 points** | **DIVERGENT: -54.3 points** |
| Below-90% rate on m0-stable | 16.5% (56/339) | 0.485% (217/44,714) | 10.63% (209/1,967) | **DIVERGENT: -16.01 points; CC is ~34× OC** | **DIVERGENT: -5.88 points** |
| Folds per 100 passes | 9.8 | 1.005 | 0.556 | **DIVERGENT: -8.79; CC is ~9.7× OC** | **DIVERGENT: -9.24; CC is ~17.6× Pi** |
| Fold trigger breakdown | Not recoverable from module store | See below | See below | NOT MEASURABLE for CC | NOT MEASURABLE for CC |
| Below-90 attribution | 44 m1; 7 tool-arg; 2 agent drop; 2 overlay; 1 tail | See below | See below | **Structurally divergent** | Coarse Pi telemetry only |

Stable-run lengths reinforce the fold result. CC had 38 runs, median 7 passes, maximum 40. OpenCode had 393 retained-window runs, median 70, maximum 530. Pi had 12, median 186.5, maximum 519. OpenCode's median stable run is 10× CC's.

Historical fold fill is **NOT MEASURABLE** for OpenCode/Pi: `transform_decisions.input_tokens` has no per-pass context-limit column. Applying today's `session_meta` limit to historical passes would fabricate a result.

## Fold/materialize reasons

### OpenCode: 454 folds in 45,168 pairs

| Reason | Count | Share of folds |
|---|---:|---:|
| `system_hash` | 172 | 37.9% |
| `pressure_refold` | 113 | 24.9% |
| `ttl_idle` | 98 | 21.6% |
| `model_change` | 22 | 4.8% |
| `renderer_transition` | 14 | 3.1% |
| `epoch_change` | 13 | 2.9% |
| `ttl_expiry` | 8 | 1.8% |
| `hard_trigger` | 7 | 1.5% |
| `compartment_render_epoch` | 4 | 0.9% |
| `boundary_divergence_recut` | 2 | 0.4% |
| `explicit_flush` | 1 | 0.2% |

`ttl_idle` and `ttl_expiry` are mode/version spellings of idle-expiry materialization and together account for 106/454 folds (23.3%).

### Pi: 11 folds in 1,978 pairs

`pressure_refold=5`, `model_change=3`, `first_render=1`, `system_hash=1`, `unknown=1`.

### CC module-store limitation

The requested historical reason distribution for CC session keys is **NOT MEASURABLE from the durable module store**. `mc_cache_state.meta` stores current state but no last or cumulative `materialize_reason`; `mc_pass_trace` stores receive/complete/reject counters and only the latest divergence JSON. Source inspection confirms `materialize_reason` is returned by `mc-module` but is not persisted by `mc-store`. Reporting a reason histogram from current `expiry_cutoff_ms` values would be an estimate, not telemetry.

For the six composite CC keys with at least 200 receives, `mc_pass_trace.receive_count` was 1,220, 839, 741, 721, 361, and 280. Their **latest divergence only** classified as three `m1:content_changed`, two `message:content_changed`, and one `message:reordered`. These six latest values are not a frequency distribution.

## Below-90%-on-m0-stable attribution

### OpenCode

The 217 low-cache stable passes break down as:

| Recorded class | Count | Share of low-stable | Share of all 44,714 stable passes |
|---|---:|---:|---:|
| No recorded byte mutation | 119 | 54.8% | 0.266% |
| Strict reclaim-only `selection` | 43 | 19.8% | 0.096% |
| Soft execute, unattributed | 27 | 12.4% | 0.060% |
| m1 re-render: `coverage_fold` | 11 | 5.1% | 0.025% |
| Coarse drop landing | 11 | 5.1% | 0.025% |
| m1 re-render: `m1_delta` | 5 | 2.3% | 0.011% |
| Coarse emergency drop landing | 1 | 0.5% | 0.002% |

The sharp parity comparison is:

- CC m1 re-render: **44/339 stable passes = 12.98%**.
- OC m1 re-render: **16/44,714 = 0.0358%**. All 16 came from ASTRO. CC's rate is about **363×** OC's, a 12.94-point gap.
- CC tool-argument supersession + agent drop: **9/339 = 2.655%**.
- OC strict reclaim-only (`materialize_reason=selection`): **43/44,714 = 0.0962%**. All 43 came from ASTRO. The gap is **2.56 points**, and CC is about **27.6×** OC.
- Including OC's 12 legacy coarse candidates gives a conservative upper bound of **55/44,714 = 0.123%**; CC is still about **21.6×** that upper bound.
- Including CC overlay and tail mutations makes its full non-m1 population **12/339 = 3.54%**.

`selection` is the strict OC reclaim-only class because Rust's materialize-reason precedence records `coverage_fold` or `m1_delta` before `selection`; therefore a `selection` pass was m0-stable and did not ride an m1 re-render. Legacy TypeScript rows with `dropped_count>0` and no reason are kept separate because migration 38 cannot prove that a drop was their only mutation.

Across all OC pairs, 377 coarse drop records rode a hard fold, zero observable coarse drops rode an m1 re-render, 43 were strict reclaim-only, and 12 were coarse reclaim-only candidates. This split is not exhaustive for Rust passes because the Rust transform-decision adapter currently records zero in `dropped_count`; the reason `selection` is the durable Rust signal.

The migration-38 table cannot subdivide legacy coarse rows into tool-argument supersession, agent drop, age reclaim, overlay, or tail. Those subclass counts are **NOT MEASURABLE** and were not inferred from token arithmetic.

### Pi

Pi had 209 below-90%-on-stable passes: 165 coarse drop landings, 24 coarse emergency-drop landings, and 20 with no recorded mutation. Across all stable passes it had 247 coarse reclaim-only candidates, 189 of which were below 90%. Pi's legacy rows carry no reason that can prove “only mutation,” so there is no strict Pi reclaim-only count comparable to OC's `selection` class.

## Per-session variance

The ordinary long OpenCode sessions are tightly clustered; ASTRO is the clear exception.

| Session | Pairs | Degraded/200 | m0 stable | Stable median | Fold median | Low-stable | Folds/100 |
|---|---:|---:|---:|---:|---:|---:|---:|
| Magic CTX (this repo) | 2,000 | 1.70 | 99.40% | 100.00% | 20.49% | 0.252% | 0.600 |
| SUBCONSCIOUS | 2,000 | 1.80 | 99.40% | 100.00% | 3.98% | 0.252% | 0.600 |
| THALAMUS | 2,000 | 2.00 | 99.05% | 100.00% | 30.00% | 0.050% | 0.950 |
| AFT | 2,000 | 1.80 | 99.20% | 100.00% | 10.12% | 0.101% | 0.800 |
| ENGRAM | 2,000 | 1.40 | 99.45% | 100.00% | 31.03% | 0.201% | 0.550 |
| ASTRO | 1,990 | 16.78 | 97.74% | 99.60% | 9.44% | 5.141% | 2.261 |

The five non-ASTRO named sessions span only 1.4-2.0 degraded passes/200 and 0.55-0.95 folds/100. ASTRO contains every strict `selection` landing (43), every measured m1 re-render (16), and a transition-heavy fold mix (`epoch_change=12`, `renderer_transition=11`, `ttl_expiry=8`, `hard_trigger=7`). The aggregate is therefore partly session-specific, but even the ASTRO outlier remains below CC on folds/100 and m1-re-render rate.

## CC idle-TTL hypothesis

The TTL mismatch is real in the inspected profile and is a strong hypothesis for CC's human-pause cadence:

1. `TransformRequest.cache_ttl` is optional. Its source comment at `crates/mc-module/src/transform.rs:568-572` explicitly says it is `None` when the consumer does not resolve TTLs (the CC leg).
2. At `crates/mc-module/src/lib.rs:6678-6682`, an omitted wire TTL falls back to `binding.config.resolve_cache_ttl(binding.model_key.as_deref())`.
3. `McModuleConfig::resolve_cache_ttl` at `crates/mc-module/src/config.rs:90-126` returns the configured default when the model key is absent or malformed. The config default is `5m` (`config.rs:69-85`).
4. Every one of the 59 composite CC `mc_cache_state` rows had empty `last_model_key` and empty `last_provider_id`. The active user config's `cache_ttl.default` is `5m`, while its Anthropic model-specific entries are `300m`; an empty key cannot select those overrides.
5. The scheduler parses `5m` to 300,000 ms and fires on strict `elapsed > ttl` (`crates/mc-module/src/scheduler.rs:370-416,772-774`). The resulting hard reason is `ttl_expiry` (`transform.rs:3087-3101` and `9797-9849`).
6. The inspected CC session `a00456c3-669e-48a4-8a67-26b67e29afcf` reports **20,187,921 `ephemeral_1h_input_tokens` and 0 `ephemeral_5m_input_tokens`** in Claude's own JSONL usage. The provider cache purchased on that leg was therefore one hour, not five minutes.

Thus the module assumes a 5-minute expiry while the CC request purchases a 1-hour cache. Any human pause longer than five minutes but shorter than one hour can trigger an unnecessary HARD fold while the provider cache is still valid, matching the observed every-5-8-step cadence when those steps contain ordinary pauses.

This is a hypothesis about the frequency mechanism, not a proven attribution of each observed CC fold: the module store does not retain historical materialize reasons, so the number of CC folds specifically caused by `ttl_expiry` cannot be recovered after the fact. No code change is proposed in this measurement task.

## Join spot checks

The analyzer emits three spot checks in JSON. For the captured OpenCode run, exact message-id joins traced:

1. Fold: `msg_ff0eee0f1001fjMQDL35OZB7XG -> msg_ff0ef615500161Thv7eqbL6Xjb`, `pressure_refold`, cache read 565,840 -> 46,370, dashboard retention 8.18%.
2. Stable/low: `msg_ff0ef615500161Thv7eqbL6Xjb -> msg_ff0ef89450017ejAmnhlwlohyl`, no recorded mutation, cache read 46,370 -> 58,184, retention 64.80%.
3. Stable/healthy: `msg_ff0ef89450017ejAmnhlwlohyl -> msg_ff0efd596001YOB7z3cZ74NWom`, no recorded mutation, cache read 58,184 -> 93,711, retention 100.00%.

For each check, the later message id is present both in `transform_decisions.message_id` and the harness's assistant usage record, and the preceding id is the immediately prior assistant usage event in that same session. Aggregate invariants also held: all rates were in [0,1], folds plus stable pairs equaled pass pairs, and join coverage was 45,169/45,181 decisions for OC and 1,978/2,000 for the included Pi run.

## Reproduction

```bash
bun packages/plugin/scripts/cache-parity-baseline.ts --harness opencode --min-passes 200
bun packages/plugin/scripts/cache-parity-baseline.ts --harness opencode --min-passes 200 --json
bun packages/plugin/scripts/cache-parity-baseline.ts --harness pi --min-passes 200 --json
bun packages/plugin/scripts/cache-parity-baseline.ts --harness opencode --session ses_331acff95fferWZOYF1pG0cjOn --min-passes 1
```
