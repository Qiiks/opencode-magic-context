# Hunt 13 H13-02: Rust historian no-fire taxonomy

Date: 2026-09-01

Base: `ad6944160a5d561d660f17ecb1f42787fcc04579`

Disposition: **fixed without trigger-behavior, durable-schema, or fence movement**.

## Adjudicated constraint

The generic Rust `trigger_false` label was not renamed to `no_new_raw_history`. A false trigger can mean no new history, projected-drop redundancy, a protected-tail refusal, low pressure, or too little eligible substance. Relabeling every false decision as the TypeScript “no new raw history” message would make several incident classes actively misleading.

The fix retains the coarse raw discriminant (`trigger_false`, `no_models`, `backoff`, and the existing assembly strings), records the concrete Rust decision-site cause, and adds a TypeScript-aligned `canonical_cause`.

## TypeScript source taxonomy

The source-of-truth review covered `compartment-trigger.ts` and the preflight gates that run after a positive trigger but before the historian request starts.

| TypeScript decision | Operator vocabulary | Canonical cause |
|---|---|---|
| Historian already running / active run or lease | `historian already in progress`, active-run/lease skips | `in_flight` |
| Cheap low-pressure upper-bound gate | `cheap-skip ... no size trigger possible` | `cheap_skip` |
| Nothing beyond the published compartment | `no new raw history` | `no_new_raw_history` |
| Raw-tail provider/inspection failure | `raw tail inspection failed` | `raw_history_unavailable` |
| Projected drops reach the target | `historian redundancy skip` | `redundancy_skip` |
| Primary and emergency-scaled windows remain protected | `protected head genuinely empty` | `protected_tail` |
| Pressure remains under the proactive trigger floor | `below proactive floor` | `below_proactive_floor` |
| Protected eligible content is too small / assembled substance is below the minimum | `unsummarized tail ... is too small`, minimum chunk floor | `below_min_chunk` |
| Internal protected-tail drain limiter is spent | `internal drain budget spent` | `drain_budget` |
| Trigger-to-run boundary snapshot is absent or stale | `missing protected-tail boundary snapshot`, `stale protected-tail snapshot` | `missing_boundary_snapshot`, `stale_boundary_snapshot` |
| Filtering leaves no chunk or coverage validation rejects it | `chunk empty after filtering`, chunk-coverage failure | `below_min_chunk`, `invalid_chunk_coverage` |

Commit-cluster, tail-size, force-band, and projected-headroom decisions are positive fire causes, not no-fire causes. Disabled modes, absent clients, internal child sessions, and compaction-off are adapter/configuration exclusions rather than trigger evaluation causes.

## Rust implementation

`HistorianNoFireCause` in `crates/mc-module/src/historian.rs` owns the raw-to-canonical mapping. `boundary.rs` now places a concrete cause on every false `TriggerDecision`:

- `HistorianAlreadyInProgress` → `in_flight`
- `NoLiveMessageAtOrAfterOffset` → `no_new_raw_history`
- `ProjectedPostDropSatisfied` → `redundancy_skip`
- `ProtectedTailWindowEmpty` → `protected_tail`
- `BelowProactiveFloor` → `below_proactive_floor`
- `BelowMinimumEligibleContent` → `below_min_chunk`

The existing assembly reason enum is extended through `HistorianNoFireReason::cause`: empty chunks map to `no_new_raw_history`, empty eligible ranges to `protected_tail`, the substance floor to `below_min_chunk`, and missing identities remain an explicit Rust integrity class. Rust-only operational gates keep truthful canonical classes (`no_models`, `pending_rewrite`, `state_load_failed`, and `assembly_failed`). Failure backoff retains raw cause `FailureBackoff` and uses the canonical rate-gate class `rate_limit`.

`HistorianDiagnostics` preserves `no_fire` and adds optional `no_fire_detail` plus `canonical_cause`. Durable examples now have the form:

```text
trigger_false{raw_cause=ProtectedTailWindowEmpty,canonical_cause=protected_tail,eligible~0k,bar~15k,protected_n~10k,ctx_limit=200000}
no_models{raw_cause=NoModels,canonical_cause=no_models}
```

The quantized measurements remain in the trigger detail. `last_no_fire` still uses the existing `ModuleMeta` JSON field and the same string-equality change gate before CAS commit. Repeated identical detail therefore does not advance `row_version`; a fire still clears the field. No SQLite column, migration, state fence, or publication fence changed.

## Adapter and audit path

`rust-mode-transform.ts` now reads `response.historian.no_fire` and `response.historian.canonical_cause` and appends both to the normal pass line:

```text
rust pass: ... historian_no_fire=trigger_false canonical_cause=no_new_raw_history ...
```

This makes the TypeScript vocabulary grep-able from the Rust-mode host log while retaining the raw module discriminant.

`scripts/audit-transform-wire-parity-live.ts` and the legacy Python audit were inspected. They read scheduler history and transform-decision tables, but do not read transform-response historian diagnostics or `ModuleMeta.last_no_fire`. No differ/live-audit canonicalization change was applicable; adding one would require a new audit input surface rather than normalizing an existing field.

## Tests and non-vacuity

- `boundary::tests::no_fire_cause_taxonomy_discriminates_trigger_decision_sites` drives in-flight, no-new-history, projected-drop redundancy, protected-tail, below-floor, and below-minimum branches independently and pins the additional TypeScript preflight vocabulary.
- `historian_chunk::tests::assembly_no_fire_reasons_preserve_distinct_canonical_causes` pins every assembly-reason mapping; `below_budget_when_tail_reclaim_not_fold_only` drives the substance-floor decision site.
- Module integration tests pin the raw/canonical trigger detail, durable no-model detail, unchanged-reason row-version gate, rate-gate diagnostics, and clear-on-fire behavior.
- `rust-mode-transform.test.ts` proves `canonical_cause` reaches the host `rust pass:` line.

Every deliberate mutation used the exact `NON-VACUITY BREAK` token and was restored immediately:

1. Replacing every boundary refusal with the constant `HistorianAlreadyInProgress` failed `crates/mc-module/src/boundary.rs:2561`: expected `NoLiveMessageAtOrAfterOffset`, received `HistorianAlreadyInProgress`.
2. Replacing every assembly refusal with constant `EmptyChunk` failed `crates/mc-module/src/historian_chunk.rs:1763`: expected `no_models`, received `no_new_raw_history`.
3. Replacing the adapter’s response-derived canonical cause with constant `in_flight` failed `packages/plugin/src/hooks/magic-context/rust-mode-transform.test.ts:634`: expected `canonical_cause=no_new_raw_history`, received `canonical_cause=in_flight`.

No mutation marker remains in source or tests.

## Verification

- `cargo test -p mc-module`: passed (1,009 unit tests with 4 ignored, plus all package integration tests).
- `cargo test -p mc-store`: passed (132 tests).
- Plugin `bun run typecheck`: passed.
- Focused Rust adapter log test: passed.
- Plugin full `bun test`: 4,256/4,259 passed. Three unrelated 5-second suite-contention timeouts occurred in message-index reconciliation, explicit storage permission setup, and wrapup controls. Running those three files together in isolation passed all 66 tests (the timed-out cases completed in 25 ms, 33 ms, and 163 ms).
- `cargo fmt --all -- --check`: all task Rust is formatted; the gate reports only the two pre-existing base-branch diffs in `crates/mc-module/src/lib.rs` around manifest provenance formatting.
- AFT completed its language phases but its metrics phase did not produce a fresh terminal result; compiler and typecheck gates above are authoritative.
- Dependencies were installed with `bun install --frozen-lockfile`; manifests and lockfiles did not change.
