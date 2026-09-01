# Parity Hunt #23 — the two skipped roams, then roam

Date: 2026-09-01  
Standing protocol: #15150, counter 0/3  
Series entering this hunt: 43 defects / 22 hunts  
Canonical behavior: TypeScript

## Verdict

This hunt examined both mandatory seeds, one deep free roam, the post-#22 corpus window, and all three current replay arms. The memory-delta seed cleared byte-for-byte. The compiled-note seed convicted two independent persistence defects, and the free roam convicted stale compiled metadata on `ctx_note(update)`.

Series after this hunt: **46 defects / 23 hunts**.

## Findings

### H23-01 — authority recovery discarded migration-52 note compilation metadata

**Atomic behavior defect.** `McStore::seed_authority_rows` accepted complete note snapshots but `NOTE_INSERT_COLUMNS` and the seed upsert omitted `compiled_provider`, `compiled_config`, `compiled_at`, and `compile_status`. After module snapshot loss, recovering a post-migration smart note silently converted those four values to null. A pre-migration sparse snapshot happened to look correct because null is canonical for fields that did not yet exist.

**Executed red-first evidence.** A fresh module store was seeded with one pre-migration sparse row and one post-migration compiled row, read through the `ctx_note` facade, closed, reopened, and read again. Neutralizing only the recovered provider with `NON-VACUITY BREAK` failed `cargo test -p mc-module note_facade_recovers_pre_and_post_migration_compilation_metadata -- --nocapture` at `crates/mc-module/src/lib.rs:22955`: received null instead of `retina-local-fs`. The mutation was restored.

**Fix.** The seed insert/upsert now persists all four migration-52 fields. The shared insert column list and lineage descent copy were updated together so the richer row shape remains aligned. The Rust facade test proves pre-migration nulls, post-migration values, facade visibility, and reopen durability. The TypeScript mirror test proves the corresponding facade data shape uses camel-case fields with the same null/value semantics.

### H23-02 — post-migration direct module note writes accepted, then discarded compiled metadata

**Atomic behavior defect.** The public `McStore::insert_project_note` accepted the four compilation fields in `NoteWriteInput`, but its SQL omitted them. The facade transaction variant already stored them, making behavior depend on which legitimate store entrypoint created the note.

**Executed red-first evidence.** A real v51 store was constructed with a legacy note, opened through the normal migration path, and a post-migration compiled note was inserted and reopened. Neutralizing the provider parameter with `NON-VACUITY BREAK` failed `cargo test -p mc-store migration_52_preserves_legacy_notes_and_durably_stores_compilation_metadata -- --nocapture` at `crates/mc-store/src/lib.rs:20724`: received null instead of `retina-local-fs`. A separate expected-value mutation reached the durable reopen assertion and failed at line 20736. Both mutations were restored.

**Fix.** The public insert now writes all four migration-52 columns. The regression proves an actual v51 row migrates with null metadata, a post-migration row retains all supplied metadata, and the values survive close/reopen.

### H23-03 — `ctx_note(update)` retained the previous condition's compiled metadata

**Atomic behavior defect.** The TypeScript facade recompiles a supplied replacement `surface_condition` and sends fresh compiled metadata. The module handler did not forward those fields to `update_note_cas`; the store reset evaluator products but left the previous condition's migration-52 metadata in place. The note could therefore advertise the old compiled configuration after its condition changed.

**Executed red-first evidence.** A facade-created compiled note was evaluated once to populate old check products, then updated with a different condition and compilation. Neutralizing only the forwarded provider with `NON-VACUITY BREAK` failed `cargo test -p mc-module smart_note_writes_require_the_host_evaluator_capability -- --nocapture` at `crates/mc-module/src/lib.rs:22176`: received null instead of `retina-local-fs`. Replacing the expected compiled config with the token independently failed the same focused test at the same assertion region. Both mutations were restored.

**Fix.** The handler now forwards the four fields. `update_note_cas` stores them only when the condition actually changes and resets the old evaluator products to the TypeScript contract: readiness/check artifacts, schedules, counters, quarantine/liveness state, and check version/status. Content-only or same-condition updates preserve compiled metadata.

## Mandatory seed 1 — `<memory-updates>` differential

### Same-input drive

Both TypeScript and Rust used IDs 1–4 as the HARD baseline and the same `CONSTRAINTS` memory set. The drive was:

1. HARD materialize with rendered manifest `[1,2,3,4]` and `maxMemoryId=4`.
2. Update #1 and archive #2; render the first defer delta.
3. Between defer passes, insert #5 above the watermark.
4. Merge sources #3 (below/equal watermark side) and #5 (above watermark) into target #4, with #4 receiving merged content.
5. Render a second defer against the unchanged HARD markers.
6. Perform the next HARD fold and inspect the reconciled `<project-memory>` plus empty m1 correction state.

A single JSON fixture is consumed by the TypeScript `renderM1`/`materializeM0` test and Rust `m1_compose`/`m0_compose` test. The first and second `<memory-updates>` blocks and next-HARD `<project-memory>` block matched byte-for-byte. The above-watermark #5 source correctly emitted no stale-baseline correction; #3 emitted `<superseded ... by="4"/>`; the next HARD contained only updated #1 and merged #4 and emitted no `<memory-updates>`.

**Examined and cleared.** No delta rendering defect was found.

**Non-vacuity.** Replacing the shared second delta with `NON-VACUITY BREAK` made both legs fail before restoration:

- Bun: `inject-compartments.test.ts:3048`, expected the token and received the four-entry rendered delta.
- Cargo: `m1_compose.rs:1092`, expected the token and received the same four-entry rendered delta.

## Mandatory seed 2 — migration-52 `ctx_note` recovery

The drive covered:

- a real note created before migration 52 and migrated forward;
- a post-migration note created through the public module-store API;
- sparse and complete authority snapshots seeded into an empty module store after simulated snapshot loss;
- close/reopen durability;
- module `ctx_note(read)` visibility for both rows;
- TypeScript mirror/facade field shape for null and populated metadata.

**Examined and convicted:** H23-01 and H23-02. Pre-migration null semantics and rendered facade text were otherwise aligned.

The TypeScript shape fixture was also mutation-proved: replacing the expected post-migration provider with `NON-VACUITY BREAK` failed `context-authority.test.ts:327`, then passed after restoration.

## Free roam — replacement-condition compilation lifecycle

One deep roam followed `ctx_note(update)` from TypeScript compilation request fields through the Rust handler, command transaction, persisted note, prior evaluator products, and subsequent facade read shape.

**Examined and convicted:** H23-03.

A second free roam was deliberately not started: the mandatory note seed already produced two independent defects and this roam produced a third; another broad lane would have traded depth and verification for shallow coverage.

## Newest-window corpus

Read-only live audits were run for the two post-#22 date partitions with cutoff `2026-08-31T11-44-59-000Z`:

- 2026-08-31: zero capture files / zero provider bodies.
- 2026-09-01: zero capture files / zero provider bodies.

The SQLite leg remained explicitly read-only. It observed 4,320 then 4,321 TypeScript transform rows and 48 Rust transform rows, with no unexplained decision invariants. The caveman oracle selected two lane sessions and all six depth samples were byte-exact between TypeScript and Rust. Provider-family denominators remained zero, so this window adds no provider-payload evidence and is not used to manufacture a clean claim.

## Replay arms

The current repository replay surfaces passed:

1. **TypeScript:** `lkg-transform-replay.test.ts` plus `migrations-armed-replay.test.ts` — 16 passed.
2. **Rust:** `cargo test -p mc-module m1_compose::tests` — 19 passed.
3. **Pi:** `reasoning-replay-pi.test.ts`, `tail-hygiene-parity.test.ts`, and `protected-tail-parity-pi.test.ts` — 17 passed.

The historical #22 command names `replay-determinism.test.ts`, `test/parity.test.ts`, and `test/replay.test.ts` were absent from this checkout, so they were skipped as nonexistent paths and replaced by the current replay/parity files above; no existing replay file was silently omitted.

## Examined / convicted / skipped ledger

### Examined and cleared

- Interleaved update → archive → cross-watermark merge delta bytes across TypeScript and Rust.
- Mutation arriving between two defer passes.
- Next-HARD m0 reconciliation and m1 correction reset.
- Pre-migration note null semantics after real migration 52.
- Module facade rendering of recovered pre/post rows.
- TypeScript mirror field-name/null/value shape.
- Current TypeScript, Rust, and Pi replay arms.
- Newest live SQLite decision/caveman invariants, within the zero-capture limitation.

### Examined and convicted

- H23-01: authority recovery omitted all migration-52 compiled-note fields.
- H23-02: direct post-migration module note insert omitted all accepted compiled-note fields.
- H23-03: replacement-condition facade updates retained stale compiled metadata and incompletely reset evaluator state.

### Skipped with rationale

- A second free roam: skipped to preserve depth after three convictions.
- Provider-payload comparison in the newest window: skipped because both date partitions contained zero capture bodies; the zero denominator is reported rather than treated as clean evidence.
- Historical replay filenames absent from this tree: replaced with current repository replay/parity tests, as listed above.
- Live SQLite mutation, migration movement, fence movement, and master push: prohibited by the hunt protocol; all live inspection was read-only and all mutation work was hermetic.
- Raw live payloads, paths, and full identifiers: omitted for privacy; only aggregate counts are recorded.

## Verification

Focused regressions and non-vacuity controls passed after every deliberate break was restored. No `NON-VACUITY BREAK` remains in changed source, tests, or fixtures.

- `cargo test --workspace` — passed: mc-module 1,015 passed / 4 ignored; mc-store 133 passed; all other workspace targets passed.
- `cd packages/plugin && bun test --parallel --timeout 30000` — 4,278 passed.
- `cd packages/plugin && bun run typecheck` — passed.
- `cd packages/pi-plugin && bun test` — 905 passed.
- `cd packages/pi-plugin && bun run typecheck` — passed.
- `python3 scripts/audit-transform-wire-parity.test.py` — 9 passed.
- `bun test ./scripts/audit-transform-wire-parity-live.test.ts` — 3 passed.
- Focused TypeScript parity fixtures — 2 passed.
- Focused Rust migration/recovery/update/delta regressions — 4 passed.

The older differ-test spelling `scripts/test-audit-transform-wire-parity.py` was absent; the current Python and Bun differ suites above were run instead.

CLEAN-OR-FINDINGS verdict: FINDINGS