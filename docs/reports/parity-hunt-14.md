# Parity Hunt #14 — paired-session replay and maintenance coverage

Date: 2026-09-01

## Result

Hunt #14 added the missing same-shape replay instrument, converted the H13 value-space observations into a permanent hermetic drill, fixed one Rust signed-reasoning exemption bug, adjudicated the remaining signed-thinking position difference, cleared the empty/dropped-placeholder producer suspicions, and added durable wrapup/recomp coverage against a Rust session with real compartment state.

No capture payload is stored. The fixture retains roles, block order, byte lengths, placeholder class, and signature presence only.

## Keeper replay instrument

Run from `packages/e2e-tests`:

```bash
bun run replay:transform-wire-parity
bun run replay:transform-wire-parity -- --provider-id mock-anthropic
```

The driver in `src/paired-session-replay.ts`:

1. boots the hermetic ck-mc stack once;
2. starts OpenCode in TS mode and drives a session from `fixtures/parity-hunt-14-session-shape.json`;
3. restarts OpenCode against the same isolated data directory in Rust mode and drives a second session from the identical fixture;
4. captures the provider-visible messages after every pass;
5. emits byte counts, first differing byte, sanitized role/block/length structure, and the audit differ's `matched_value_space` / `divergent_value_space` vocabulary; and
6. exits nonzero only for an unadjudicated value-space divergence.

`tests/rust-paired-session-replay.test.ts` is the CI-viable hermetic guard. `tests/paired-session-replay.test.ts` pins the differ vocabulary without Rust prerequisites.

### Canonical Anthropic replay

| Pass | TS bytes | Rust bytes | Empty-content space | Dropped-placeholder space | Signed-reasoning space |
|---|---:|---:|---|---|---|
| signed-thinking-index-zero | 334 | 334 | matched | matched | matched |
| isolated-assistant-dropped-placeholder | 890 | 830 | matched | matched | divergent, adjudicated |
| raw-empty-assistant-text | 1069 | 1036 | matched | matched; shared isolated assistant placeholder | divergent, adjudicated |
| observe-empty-and-placeholder | 1282 | 1249 | matched | matched; shared isolated assistant placeholder | divergent, adjudicated |

All three value-space divergences are the same signed-thinking position exception described below. `unadjudicated_divergence_count` is zero.

### Non-canonical provider replay

The same fixture was replayed with provider id `mock-anthropic`, which deliberately takes the non-Anthropic sentinel gate while retaining the hermetic local transport.

| Pass | TS bytes | Rust bytes | Empty-content space | Dropped-placeholder space | Signed-reasoning space |
|---|---:|---:|---|---|---|
| signed-thinking-index-zero | 334 | 334 | matched, empty | matched | matched |
| isolated-assistant-dropped-placeholder | 836 | 830 | matched, empty | matched | matched, shared index-0 signed |
| raw-empty-assistant-text | 1042 | 1036 | matched, empty | matched; shared isolated assistant placeholder | matched |
| observe-empty-and-placeholder | 1255 | 1249 | matched, empty | matched; shared isolated assistant placeholder | matched |

There are zero value-space divergences and zero empty-content shapes in either lane.

## H13-03 convictions and clearances

| H13 class | Same-input result | Producer branch / source | Verdict |
|---|---|---|---|
| Rust-only empty assistant/user text blocks | Neither canonical nor non-canonical paired replay produced a lane-only empty block. The raw-empty response fixture remained symmetric. | TS gate: `packages/plugin/src/hooks/magic-context/sentinel.ts:20-37`; Rust mirror: `crates/mc-module/src/transform.rs:13215-13221`. | **Cleared.** The unpaired live samples had different raw inputs/provider projections; MC producer branches agree. |
| TS-only isolated assistant `[dropped]` placeholders | Both lanes expose the same isolated assistant placeholder on the same passes. | TS whole-message placeholder: `sentinel.ts:18`; Rust placeholder recognition and sentinel rendering: `transform.rs:10594-10600,10628-10633`. | **Cleared.** This is shared raw/session shape, not a TS-only producer branch. |
| Rust-only absent signed-thinking | The initial replay convicted the dropped assistant shell as stealing the signed-reasoning exemption; later passes lost the earlier signed block only in Rust. | Fixed at `crates/mc-module/src/transform.rs:13456-13460`: empty and dropped-only assistant shells are ignored when selecting the latest provider-visible reasoning exemption. Regression test: `transform.rs` test `dropped_assistant_shell_does_not_steal_signed_reasoning_exemption`. | **Convicted and fixed.** After the fix, the signed block remains present on all paired passes. |
| Rust-only index-0 signed-thinking | Rust serves the original provider-native `[thinking,text]` vector. Canonical TS projection adds blank/tag text around that vector, shifting thinking from index 0. Non-canonical replay places thinking at index 0 in both lanes. | `crates/mc-module/src/codec/opencode.rs:459-477,759-776`. | **Intentional difference.** Signed-reasoning bytes and content-array position are a provider safety boundary; Rust does not mutate the native vector merely to reproduce TS adapter residue. The fixture records this adjudication explicitly and exact-set matching prevents it from hiding a different divergence. |

### Executed mutation

`bun scripts/run-paired-replay-mutation.ts` removed dropped-placeholder recognition from the reasoning exemption selector, ran the permanent hermetic replay, restored the source, and reran it.

Record: `packages/e2e-tests/mutations/parity-replay.json`.

- Mutated run: exit 1.
- Recorded red lines: `tests/rust-paired-session-replay.test.ts:12` and `:35`; each observed two unadjudicated divergences.
- Restored run: 2 passed, 0 failed.

This proves the replay is non-vacuous for the fixed producer branch.

## H13-04 non-Anthropic empty-content provenance

The sentinel gate is exact-provider by design:

- TS returns empty only for `providerID === "anthropic"` (`sentinel.ts:20-37`).
- Rust uses the same predicate (`transform.rs:13215-13221`).
- Every unknown/non-canonical provider receives the non-empty `[dropped]` whole-message sentinel.

The non-canonical paired replay exercised a raw empty assistant response and an isolated dropped assistant. Neither MC lane emitted an empty provider-wire block; the dropped placeholder remained non-empty in both lanes. Therefore the H13 non-Anthropic empty-content observation did **not** originate from an MC-synthetic sentinel. Its provenance is the raw tool-output part carried into the capture before MC's whole-message producer branch; the non-Anthropic adapter can forward that raw shape. No MC canonical-behavior violation exists on this path.

## H13-05 hermetic maintenance drill

`tests/rust-maintenance-contract.test.ts` now:

1. drives ten real-content turns through Rust mode until the deterministic Broca producer publishes a durable compartment;
2. requires nonzero `coverage_ordinal` and reads the real module store at the hermetic data path;
3. executes `session.wrapup` through the Subc management route and requires the deterministic `nothing_to_compact` machine disposition because the landed fold already covers the eligible head;
4. verifies the matching `mc_wrapup_commands` row and zero rounds;
5. executes `session.recomp`, requires `started`, and verifies the matching `mc_recomp_commands` row;
6. proves cache `row_version` is monotonic across wrapup and increases across recomp; and
7. proves recomp clears the durable compartment count and coverage ordinal.

This replaces `zero_live_rust_wrapup_commands` / `zero_live_rust_recomp_commands` as a test-coverage statement with pinned hermetic contract coverage. The observed dispositions are members of the machine-readable sets documented by the architecture and checked by the live audit.

## Verification

- `bun install --frozen-lockfile` — passed; lockfile unchanged.
- `cargo test -p mc-module` — passed: 1008 passed, 4 ignored, plus all package integration binaries passed.
- `bun run test:rust-e2e` — passed: 23 passed, 2 documented skips, 0 failed.
- Full manifest-derived TS OpenCode leg — passed: 48 tests across 20 files, 0 failed.
- `bun test tests/paired-session-replay.test.ts scripts/validate-mode-manifest.test.ts` — passed: 7 tests, 0 failed.
- Final `bun test --timeout 700000 --max-concurrency=1 tests/rust-maintenance-contract.test.ts` — passed.
- `bunx tsc --noEmit -p packages/e2e-tests/tsconfig.json` — changed files typecheck; the command remains red only on the pre-existing `tests/pi-compaction-off.test.ts:60` Bun SQLite vs BetterSqlite3 type mismatch.
- `cargo fmt --all -- --check` — changed Rust file is formatted; the workspace check remains red only on pre-existing formatting in `crates/mc-module/src/lib.rs:15104,15928`.
