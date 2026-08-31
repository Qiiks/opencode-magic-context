# TS <> Rust <> Pi parity hunt 13

Date: 2026-08-31  
Base: `eba3fc52ae067473662a20bd6f93ab33a8533582`  
Priority window: merges `a5af5179`, `cc5ffbbc` / `ea3996dc`, `84bc48c3`, and the #402 packaging series.  
Verdict: **findings; clean counter remains 0/3**.

## Method and live evidence

The source contract and privacy-preserving live probe were run with:

```text
python3 scripts/audit-transform-wire-parity.py \
  $DARWIN_USER_TEMP_DIR/opencode-anthropic-auth-dumps \
  --live --date 2026-08-31 \
  --after 2026-08-29T20-47-32-024Z \
  --engine-after 2026-08-29T20-47-32-024Z \
  --per-session 1000
```

The same command was run for August 29 and 30. Those dates contained no captures; all captures in the latest-dump 48-hour interval were on August 31. The August 31 report inspected 4,822 request/response bodies: 2,057 Anthropic and 2,765 OpenAI Responses. Lane coordinates resolved 640 captures (Rust Anthropic 229; TypeScript Anthropic 159 and OpenAI Responses 252); 4,182 remained privacy-safe unverified inventory. The audit reads SQLite read-only and records only hashes, counts, ordinals, session prefixes, and byte lengths.

The updated source contract has zero historian or Hunt 12 source invariants. The live scheduler probe read two post-cutoff history rows (`defer: 1`, `execute: 1`) and canonicalized the legacy Rust pass-band spellings. The Rust engine probe found two configured Rust sessions with newly mirrored memory embeddings after the cutoff: 4/4 and 1/1 mirror rows had vectors.

## Findings

| ID | Finding and conviction | Disposition | Permanent repro |
|---|---|---|---|
| H13-01 | Rust had an equivalent refusal surface, contrary to the earlier “no twin” adjudication, but `mc_pass_trace.scheduler_history` exposed pass bands (`Defer`, `Force85`, etc.) while TypeScript `transform_decisions` and refusal logs use `defer` / `execute` plus `scheduler_defer` / `mid_turn_boundary`. The write was at `crates/mc-module/src/transform.rs:2019`; the durable shape is `crates/mc-store/src/lib.rs:2822-2894`; TypeScript vocabulary is emitted at `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1171-1196`. | **Fixed.** Rust now retains the raw pass band for compatibility and adds `canonical_decision` plus `defer_reason`. Transform responses carry the same fields, and `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:449-493` appends them to `rust pass:` lines. The live audit maps old rows to canonical classes at `scripts/audit-transform-wire-parity-live.ts:1590-1701`. | `scheduler::tests::canonical_defer_classes_preserve_the_typescript_reason_vocabulary`; `transform::tests::scheduler_trace_keeps_mid_turn_downgrades_in_the_defer_pass_class`; scheduler-history store tests; live-probe fixture in `scripts/audit-transform-wire-parity.test.py`. |
| H13-02 | The compartment refusal does have a Rust twin, but its vocabulary is coarser. TypeScript logs `compartment trigger: skipped — no new raw history` at `packages/plugin/src/hooks/magic-context/compartment-trigger.ts:595`. Rust returns `HistorianDiagnostics.no_fire` (`crates/mc-module/src/transform.rs:1622-1638`) but collapses this and other non-fire causes into `trigger_false{...}` at `crates/mc-module/src/lib.rs:4828-4848`. An operator cannot grep the TypeScript reason in Rust incidents. | **Finding, adjudication required.** Do not relabel generic `trigger_false` as “no new raw history”: that would misreport below-bar and protected-tail cases. The Rust boundary result needs a no-fire taxonomy before a canonical vocabulary can be added. No behavior was silently changed. | Existing Rust diagnostic regression at `crates/mc-module/src/lib.rs:29502-29504` permanently proves the generic surface; the TypeScript line is covered by compartment-trigger tests. |
| H13-03 | Resolved Anthropic live captures had three lane-only value spaces: Rust-only empty assistant/user text blocks; TypeScript-only isolated assistant `[dropped]` placeholders; and Rust-only absent or index-0 signed-thinking shapes. The differ convicted these as `divergent_value_space`, not as tool-count differences between unrelated sessions (`scripts/audit-transform-wire-parity.py:1100-1212`). | **Finding, ambiguous.** The captures are different sessions, so they prove unequal rendered value spaces but do not prove which producer branch created them. No canonical TypeScript behavior was changed. A paired-session replay is required before moving Rust. | New hermetic differential `test_provider_matrix_keeps_anthropic_value_space_divergences_as_findings` pins the empty-content and placeholder classes; the existing provider fixture pins reasoning-signature classification. |
| H13-04 | One resolved TypeScript OpenAI Responses capture contained an empty tool-content shape. The privacy record was `sha256=4506f17b…`, 26,464 bytes, session prefix `ses_fa82`; the live invariant class was `non_anthropic_empty_content`. There was no resolved Rust OpenAI Responses capture, so this is also a cross-leg coverage gap. The canonical empty-sentinel gate explicitly permits only Anthropic (`packages/plugin/src/hooks/magic-context/sentinel.ts:20-37`). | **Finding, ambiguous origin.** It may be raw tool output rather than a synthetic Magic Context marker. Preserve TypeScript semantics pending a paired fixture and provenance of the empty block. | `test_provider_matrix_reports_non_anthropic_empty_and_adjacency_breaks` in `scripts/audit-transform-wire-parity.test.py`. |
| H13-05 | The standing live leg verdicts still report `zero_post_cutoff_rust_compartment_publish_rows` and `pi_native_compaction_with_pending_marker`; wrapup/recomp also have zero live Rust command coverage. | **Inherited live findings / gaps.** Memory-mirror liveness is positive (see #402 below), but these observations do not supply a same-input transform fixture and were not converted into speculative behavior changes. | Standing audit leg verdicts and the generated 48-hour reports in `/tmp/parity-hunt-13-live-{29,30,31}.json`. |

## Priority adjudications

### 1. Historian temperature opt-in

Resolved request shapes agree:

| Configuration | TypeScript/OpenCode override | Pi child and calibration extension | Rust Broca generation |
|---|---|---|---|
| absent | temperature key omitted | env omitted; generation field removed | `Option::None`, omitted by serde |
| `0.1` | `temperature: 0.1` | env `"0.1"`; numeric generation field | `Some(0.1)` |
| `0` | `temperature: 0` survives object spread | `!== undefined` preserves env `"0"`; extension preserves numeric zero | `Some(0.0)` survives to JSON |

TypeScript preserves non-model fields, including temperature, while resolving only `historian.opencode` at `packages/plugin/src/shared/model-resolution.ts:204-225`. Pi resolves `historian.pi` and passes the shared temperature at `packages/pi-plugin/src/index.ts:706`; the child env uses the undefined check at `packages/pi-plugin/src/subagent-runner.ts:1271-1277`. Rust carries `Option<f64>` through `crates/mc-module/src/historian.rs:930-931` and serializes it in `crates/mc-module/src/historian_producer.rs:41-45,593-656`.

The audit's stale fixed-`0.1` calibration expectation was itself corrected: absent temperature is now canonical, and the source contract emits an absent/0.1/0 request-shape matrix. Per-harness tests use deliberately different OpenCode and Pi model chains so a cross-feed fails.

### 2. Observability trio

| TypeScript surface | Rust equivalent | Adjudication |
|---|---|---|
| `heuristics WILL NOT RUN — reason=<deferReason>` | scheduler trace and `rust pass:` line | H13-01: equivalent existed with incompatible vocabulary; fixed with canonical class/reason. |
| `pending ops WILL NOT APPLY — reason=<deferReason>` | same scheduler trace and pass line | H13-01: fixed. |
| `compartment trigger: skipped — no new raw history` | `HistorianDiagnostics.no_fire=trigger_false{...}` | H13-02: equivalent but under-specified; taxonomy decision remains. |

### 3. Turn detection keepers

The TypeScript boundary transition (`packages/plugin/src/hooks/magic-context/boundary-execution.ts:96-108`) distinguishes a base scheduler defer (`scheduler_defer`) from an execute downgraded at a mid-turn boundary (`mid_turn_boundary`) while both remain pass class `defer`. Rust now records exactly that split. `crates/mc-module/src/scheduler.rs:532-552,708-808` assigns the reason after applying the tail-state boundary transition; both serialize as canonical `defer` while retaining raw `Defer` as the Rust pressure-band label.

This closes the telemetry mapping without changing the known host `mid_turn` versus paired-tail structural adjudication in `docs/reports/mid-turn-protection-design-evidence.md`.

### 4. #402 packaging and mirrored-memory embeddings

The Rust transform path still records the session/project binding at `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:2010`. The shared idle hook drains unembedded project memories at `packages/plugin/src/hooks/magic-context/hook.ts:992`, and local provider initialization remains lazy in `packages/plugin/src/features/magic-context/memory/embedding-local.ts:195-222`.

The source regressions and full plugin build passed with the bundled transformer chunks. More importantly, the read-only engine probe saw 5 post-cutoff Rust mirror memory rows across the two configured Rust sessions, all 5 with vectors. The #402 lazy-loading change therefore did not suppress Hunt 6's Rust-session trigger path.

### 5. Free-roam wire classes

No part-ordering, synthetic-tag overlay, temporal marker, or tool-result adjacency divergence was reported for the resolved 48-hour captures. The four reported byte-class differences are H13-03 and H13-04 above. Coverage is not a clean pass: 4,182 captures had no collision-free lane coordinate and Rust had no resolved OpenAI Responses leg.

## Executed mutation evidence

Every deliberate mutation carried the exact `NON-VACUITY BREAK` token and was restored immediately:

1. Replacing Pi's `temperature !== undefined` guard with truthiness failed `subagent-runner.test.ts:1281`: expected `"0"`, received `undefined`.
2. Filtering `0.0` from Rust's generation request failed `historian_producer.rs:1637`: expected JSON number `0.0`, received `Null`.
3. Mapping Rust `MidTurnBoundary` to `scheduler_defer` failed `transform.rs:15341`: expected `mid_turn_boundary`, received `scheduler_defer`.
4. Removing legacy pass-band canonicalization failed `audit-transform-wire-parity.test.py:693`: expected `defer`, received `Defer`.

No mutation marker remains in changed source or tests.

## Verification

- `cargo test -p mc-module && cargo test -p mc-store`: passed (1,011 module tests with 4 ignored; 132 store tests).
- Plugin `bun run typecheck`: passed.
- Plugin focused parity and timeout-retry files: passed (163 parity tests and 66 isolated timeout cases).
- Plugin full `bun test`: 4,256/4,259 passed; three unrelated first-test timeouts reproduced on two full runs under suite contention. All three pass in 25–152 ms when run together in isolation. A 30-second full run reached 4,258/4,259, with only the message-index test timing out despite its isolated 25 ms pass.
- Plugin `bun run build`: passed and emitted the lazy `transformers-web.js`, `transformers.node-*`, and split runtime chunks.
- Pi `bun run typecheck && bun test && bun run build`: passed (903 tests).
- `python3 scripts/audit-transform-wire-parity.test.py`: passed (8 tests).
- `cargo fmt --all -- --check`: task Rust files are formatted; the gate reports only two pre-existing formatting diffs in `crates/mc-module/src/lib.rs:15104` and `:15928`, introduced on the base branch and intentionally left outside this change.
- AFT diagnostics collection completed its language phases but its metrics phase did not produce a fresh terminal result; compiler/typecheck gates above are authoritative.
