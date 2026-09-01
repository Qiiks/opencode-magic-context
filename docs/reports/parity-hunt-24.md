# Parity Hunt #24 — descent, denominator, and two deep roams

Date: 2026-09-01  
Standing protocol: #15150, counter entering 0/3  
Series entering this hunt: 46 defects / 23 hunts  
Canonical behavior: TypeScript

## Verdict

This hunt drove the migration-52 descent seam rather than inferring it from SQL column alignment, installed a non-vacuous live-corpus denominator, and examined two additional field/lifecycle families across the TypeScript, Pi, and Rust implementations. No new atomic behavior defect was found.

Series after this hunt: **46 defects / 24 hunts**. Clean counter: **1/3**.

## Mandatory seed 1 — D5 lineage descent with compiled note metadata

The existing D5 handler drive now carries a real session note through the complete successor path:

1. Build the ten-message source session through the module transform handler.
2. Seed a migration-52-complete session-note snapshot under the source key.
3. Persist source compartments and issue the D5 lineage switch through the target transform channel.
4. Read the copied target row by its new id and assert `compiled_provider`, `compiled_config`, `compiled_at`, and `compile_status` independently.
5. Read `ctx_note` through the successor's facade channel and observe the inherited note content.

The target row received a new note id, retained all four compilation fields, and was visible only through the successor's session-scoped facade. The source metadata was asserted before descent, so a sparse precondition could not make the copy check pass.

**Examined and cleared.** H23-01's shared-column alignment is behaviorally sound on the D5 descent path.

**Non-vacuity.** Replacing only the inherited provider expectation with `NON-VACUITY BREAK` failed `cargo test -p mc-module handler_delta_d5_lineage_descent_forces_full_projection -- --nocapture` at `crates/mc-module/src/lib.rs:20838`: the copied row contained `retina-local-fs`. The mutation was restored and the focused test passed.

## Mandatory seed 2 — minimum live provider denominator

Live mode now accepts `--min-provider-bodies` (default 1). It first selects the requested newest window. If that denominator is below the stated minimum, it walks earlier privacy-safe capture timestamp bounds until the per-session-capped selection reaches the minimum. The report prints:

- requested lower bound;
- effective lower bound;
- whether widening occurred;
- stated minimum;
- admitted body count.

If all bodies at or before the upper bound cannot meet the minimum, the audit refuses instead of emitting a vacuous provider result. Bounds contain timestamps only; no path or identifier is added to report output. The effective bound also drives the read-only decision window unless the caller explicitly supplies a separate engine bound.

The hermetic live integration started at an empty 2026-08-28 fixture window, required three bodies, widened to `2026-08-27T12-00-00-000Z`, and printed an admitted count of three.

**Non-vacuity.** Replacing the expected effective bound with `NON-VACUITY BREAK` failed `python3 scripts/audit-transform-wire-parity.test.py` at line 797. The mutation was restored; all nine Python differ tests passed.

### Live widened corpus

The privacy-preserving live run requested `2026-09-01T23-59-59-999Z`, where the provider denominator was zero, and stated a minimum of 100 bodies. The audit widened to `2026-09-01T15-03-20-053Z` and admitted exactly 100 bodies across two capture roots:

- 5 Anthropic bodies;
- 95 OpenAI Responses bodies.

All 100 newest bodies lacked a decisive live lane coordinate (eight collision-free session hashes requested, zero resolved), so none entered a TypeScript/Rust comparison denominator. Four Responses bodies carried the standing `non_anthropic_empty_content` shape, all in the unverified lane; this is inventory, not same-input evidence and not a finding. No provider value-space divergence was reported for admitted lanes because there were no admitted lanes.

Every SQLite handle reported `readonly: true` and verified `query_only`. The effective decision window contained 29 TypeScript decisions, zero Rust decisions, and zero scheduler rows. Engine, decision, source-contract, maintenance, operator-read, and caveman unexplained-invariant lists were empty. The Rust caveman oracle completed for both lanes. Operator reads covered both lanes. The standing gaps remained: no post-bound Rust publish/decision/scheduler evidence, no live Rust maintenance commands, and one previously adjudicated Pi native-compaction/pending-marker observation. None is promoted into a clean claim or a new defect.

## Free roam 1 — migration-51 `mapping_origin` write-family enumeration

The migration lesson was applied to the entire field family rather than one SQL statement.

### Rust/module engine

- **Migration/default:** migration 51 adds `mapping_origin NOT NULL DEFAULT 'mapper'`; a real v50 row migrated to `mapper`.
- **Authority seed insert:** `seed_memory_snapshots` writes the supplied origin with its mapping.
- **Authority seed update/adoption:** reseeding the same context row updates both mapping bytes and origin.
- **Direct mapping update:** `memory.set_mapping` validates the two-value vocabulary and `set_memory_mapping_tx` upserts the origin in the same transaction as the mapping/feed row.
- **Content update/archive:** verification-driven content replacement and archive deliberately delete the now-stale mapping row, so there is no origin to preserve.
- **Descent copy:** not applicable. Module mappings are project-owned side-table rows with no conversation key; lineage descent copies session-owned history only.

The new v50 migration/seed fixture proves default, seed insert, and seed update behavior. Existing module facade coverage proves direct `host_rejected_fallback` update plus changefeed projection.

### TypeScript engine

- **Migration/default:** the TypeScript schema migration adds the same `mapper` default to `memory_verifications`.
- **Mapping insert/replace:** `recordMemoryMapping` deletes the old set and writes one origin across every replacement row.
- **Verification replacement:** `recordMemoryVerifications` intentionally establishes fresh `mapper` rows.
- **Authority mirror insert/update/delete:** mirror application rebuilds rows from `mapping` plus `mapping_origin`, and removes the relation when mapping is null.
- **Identity merge/collision copy:** the collision UPSERT copies origin and selects the origin belonging to the newer `mapped_at` row.
- **Content update/archive:** stale verification/mapping rows are cleared rather than retained.

The identity-collision fixture now drives a newer `host_rejected_fallback` source into an older `mapper` survivor and proves both resulting rows retain the newer origin.

**Examined and cleared.** No missing insert, update, seed, migration-default, mirror, collision-copy, delete, or applicable descent entrypoint was found.

**Non-vacuity.** Two restored mutations failed independently:

- Rust seed/update expectation: `mc-store/src/lib.rs:20745`, received `mapper` instead of `NON-VACUITY BREAK`.
- TypeScript identity merge: `storage-identity-merge.test.ts:240`, received `host_rejected_fallback` instead of `NON-VACUITY BREAK`.

## Free roam 2 — historian discard-last during emergency recovery

The ordinary greedy historian path may discard a provisional final compartment when at least one earlier compartment remains and the retained boundary does not split a completed tool arc. Emergency recovery deliberately bypasses that quality heuristic to obtain immediate space relief.

The audit followed all three engines:

- TypeScript reads `needsEmergencyRecovery` and gates the shared discard predicate with `!inEmergency`.
- Pi uses the same overflow state and the same shared structural predicate.
- Rust carries `ValidateOptions.in_emergency` into `validate_historian_output` and bypasses its corresponding pop.

New Pi and Rust fixtures each emit two compartments ending at the chunk edge, a shape that would ordinarily discard the second. With emergency recovery armed, both persist endpoints `[2,4]`; the Pi drive goes through the full runner/store path and the Rust drive goes through parser/validator mapping. The TypeScript call site uses the same predicate and gate as Pi, and its existing structural guard fixture continues to prove ordinary k=2 discard and completed-tool-arc retention.

**Examined and cleared.** Emergency recovery keeps the provisional final compartment in all three implementations; completed-tool-arc safety remains independent of the bypass.

**Non-vacuity.** Replacing the second title expectation with `NON-VACUITY BREAK` failed the Rust fixture at `historian_validate.rs:1668` and the Pi fixture at `pi-historian-runner.test.ts:837`. Both mutations were restored.

## Replay arms

The current TypeScript, Rust, and Pi replay surfaces were run separately from the broad suites:

- TypeScript: `lkg-transform-replay.test.ts` and `migrations-armed-replay.test.ts`.
- Rust: `cargo test -p mc-module m1_compose::tests`.
- Pi: `reasoning-replay-pi.test.ts`, `tail-hygiene-parity.test.ts`, and `protected-tail-parity-pi.test.ts`.

## Examined / convicted / skipped ledger

### Examined and cleared

- D5 successor descent of all four migration-52 note compilation fields.
- Successor `ctx_note(read)` visibility after descent.
- Live-corpus lower-bound widening and minimum-denominator refusal contract.
- Migration-51 `mapping_origin` migration, seed, insert/update, mirror, collision-copy, and stale-delete paths across both engines.
- Historian discard-last emergency bypass across TypeScript, Pi, and Rust.
- Read-only widened live corpus, within its fully unverified provider-lane limitation.
- Current TypeScript, Rust, and Pi replay arms.

### Examined and convicted

- None.

### Skipped with rationale

- Provider payload parity in the widened newest 100-body corpus: skipped because all 100 bodies lacked decisive lane coordinates; the denominator and four unverified shape observations are reported instead of manufacturing a comparison.
- `mapping_origin` lineage descent: structurally inapplicable because mappings are project-owned and have no session key.
- Additional `/ctx-recomp` and workspace-fingerprint roams: skipped after completing two deep free roams with write-entrypoint and three-engine lifecycle coverage.
- Live SQLite mutation, migration movement, fence movement, master push, and raw payload/path/full-identifier disclosure: prohibited by protocol.

## Verification

Focused regressions and every non-vacuity control passed after restoration. No `NON-VACUITY BREAK` remains in changed source, tests, or fixtures.

- `bun install --frozen-lockfile` — passed; manifests and lockfile unchanged.
- `python3 scripts/audit-transform-wire-parity.test.py` — 9 passed.
- Widened live audit — passed read-only; minimum 100 admitted at the printed effective bound.
- Focused D5 descent, migration-51 seed/update, TypeScript identity merge, Pi emergency runner, and Rust emergency validator fixtures — passed after restoration.
- Full OpenCode plugin suite — 4,278 passed, 0 failed.
- Plugin typecheck (`tsc` for retina-local-fs, plugin, and plugin scripts) — passed.
- Complete Rust workspace — 1,179 passed, 4 ignored, 0 failed. The unfiltered `cargo test --workspace -- --nocapture` result lines were:
  - `test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s`
  - `test result: ok. 1016 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 64.92s`
  - `test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s`
  - `test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s`
  - `test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s`
  - `test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 11.68s`
  - `test result: ok. 134 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 13.91s`
  - `test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s`
  - `test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.11s`
  - `test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s`
  - `test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s`
  - `test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s`
  - `test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s`
- Full Pi plugin suite — 906 passed, 0 failed.
- Pi typecheck — passed.
- Python differ suite — 9 passed; live TypeScript differ suite — 3 passed.
- Replay arms — TypeScript 16 passed, Rust 19 passed, Pi 17 passed; zero failures.
- Rust formatting and final `git diff --check` — passed.

CLEAN-OR-FINDINGS verdict: CLEAN
