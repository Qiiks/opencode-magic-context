# Parity Hunt #15 — OpenAI Responses replay and no-fire audit vocabulary

Date: 2026-09-01  
Base: `9f0157630a5d07fc6302f83460d189b521811b62`  
Standing protocol: `#15150` (incoming retirement counter `0/3`)

## CLEAN-OR-FINDINGS verdict

**CLEAN-OR-FINDINGS verdict: CLEAN — zero real defects found; the retirement counter advances to 1/3.**

The work added missing instrumentation, audit reading, and maintainer documentation. Those are not product findings. The post-deploy live window contains no new provider-wire byte class beyond the already paired/adjudicated Hunt 13/14 Anthropic residuals. The one unmet live assertion is a coverage gap, not a defect: no post-deploy durable historian no-fire row existed, so the deployed `canonical_cause` field could not be observed in live state during this hunt.

## OpenAI Responses paired replay

The schema-2 fixture in `packages/e2e-tests/fixtures/parity-hunt-14-session-shape.json` now contains independent Anthropic Messages and OpenAI Responses arms. The local provider supports real `/responses` streaming at `packages/e2e-tests/src/mock-provider/server.ts:162-260,490-671`; the spawner selects `@ai-sdk/openai` and registers the companion setup provider at `packages/e2e-tests/src/opencode-runner/spawn.ts:255-303`.

The Responses arm creates a real tool call/result arc in the isolated OpenCode session, seeds the sanitized post-sentinel `[dropped]` result shape, then measures the actual Responses `input` array. The setup and capture seam is `packages/e2e-tests/src/paired-session-replay.ts:537-613`. Classification covers logical empty content, dropped placeholders, signed reasoning items, and call/result pairing; all four axes feed the adjudication-aware exit count at `paired-session-replay.ts:334-426,652-674`.

Direct command:

```text
cd packages/e2e-tests
bun run replay:transform-wire-parity -- --provider-arm openai-responses
```

Observed result:

| Pass | TS bytes | Rust bytes | Empty content | Dropped placeholder | Tool pairing | Reasoning item |
|---|---:|---:|---|---|---|---|
| `observe-empty-tool-output-with-reasoning` | 34,756 | 34,756 | matched, none | matched, shared isolated tool `[dropped]` | matched, paired | matched, none yet |
| `observe-responses-history` | 35,135 | 35,129 | matched, none | matched, shared isolated tool `[dropped]` | matched, paired | matched, shared signed nonzero item |

`divergence_count=0` and `unadjudicated_divergence_count=0`. The six-byte raw difference on the second pass is provider metadata outside every logical axis; it does not alter role/item structure or the measured value space. `tests/rust-paired-session-replay.test.ts:54-82` permanently requires the sentinel, tool pair, and signed reasoning item to be present and shared rather than accepting mutual absence. The fast differ guard at `tests/paired-session-replay.test.ts` independently pins Responses classification.

The existing Anthropic and non-canonical-provider arms remain green with their exact signed-thinking adjudications. The CLI still exits nonzero only when a divergent axis lacks an exact pass/axis/`ts_only`/`rust_only` adjudication.

## Historian no-fire canonical causes in the live audit

The live helper now reads `mc_cache_state.meta.historian.last_no_fire` and reports:

- canonical cause counts;
- concrete raw-cause counts;
- durable detail-kind counts; and
- the number of legacy generic rows.

`canonicalHistorianNoFire` prefers the structured `canonical_cause` emitted by the taxonomy delivery. Known legacy detail kinds are mapped without inventing a concrete trigger decision. In particular, generic old `trigger_false` is reported as `legacy_trigger_false`, not falsely relabeled `no_new_raw_history`; this preserves the taxonomy constraint that the old label can represent several refusal sites. The parser and distribution are at `scripts/audit-transform-wire-parity-live.ts:1590-1664`, and `decision_window.historian_no_fire` is emitted at `:1772`. The differ fixture at `scripts/audit-transform-wire-parity.test.py:703-718` contains one structured `protected_tail` row and one generic legacy row.

The latest-24-hour snapshot found two durable no-fire states, both legacy generic rows. Restricting activity to after the Hunt 14 module commit (`2026-08-31T23:28:29Z`) found zero no-fire rows. Therefore:

- the audit now correctly reads and canonicalizes both generations;
- no deployed row contradicted the new taxonomy; but
- live presence of a structured `canonical_cause` remains **unobserved coverage**, because no post-deploy no-fire event was durable at the cutoff.

This is not green-washed as positive live evidence. A future hunt should repeat the same read after ordinary Rust traffic records a no-fire outcome; no production state was mutated to manufacture one.

## Free-roam live differ

Privacy-preserving read-only commands covered both UTC dates in the rolling window and then repeated with the Hunt 14 deployment instant as the lower bound:

```text
python3 scripts/audit-transform-wire-parity.py --live --date 2026-08-31 \
  --after 2026-08-31T01-05-00-000Z --engine-after 2026-08-31T01-05-00-000Z \
  --per-session 1000 --skip-live-rpc --skip-live-rust-oracle
python3 scripts/audit-transform-wire-parity.py --live --date 2026-09-01 \
  --after 2026-08-31T01-05-00-000Z --engine-after 2026-08-31T01-05-00-000Z \
  --per-session 1000 --skip-live-rpc --skip-live-rust-oracle
```

The rolling snapshot inspected 5,640 request bodies from the two dump roots. Lane coordinates resolved 671: Rust Anthropic 239, TypeScript Anthropic 180, and TypeScript OpenAI Responses 252. The remaining 4,969 stayed privacy-safe unverified inventory. The post-Hunt-14 snapshot contained five resolved Rust and nine resolved TypeScript Anthropic captures; its OpenAI Responses traffic remained lane-unverified, so the hermetic arm is still the only same-input Rust Responses evidence.

Post-deploy observations:

- system message/block counts and tool pairing matched;
- the earlier TypeScript-only assistant dropped-placeholder class was absent;
- no provider-wire invariant was reported;
- no new empty, dropped, reasoning, ordering, adjacency, or tool-cardinality class appeared; and
- the remaining Rust-only empty assistant/user and signed-reasoning position classes are the unlike-session residuals already resolved by Hunt 14's paired replay. They are not re-convicted as new defects.

The broad window still contains H13-04's original TypeScript Responses empty tool-output capture (`sha256=4506f17b…`, 26,464 bytes). There is no newer resolved contradiction. The new paired arm turns that origin class into a permanent same-input sentinel/tool/reasoning drill without storing capture prose.

Standing maintenance gaps (`zero_live_rust_dreamer_apply_commands`, `zero_live_rust_recomp_commands`, and `zero_live_rust_wrapup_commands`) remained observational gaps with zero invariant failures. Hunt 14's hermetic maintenance test continues to own their executable contract coverage.

## Executed mutation evidence

Every deliberate break used the exact `NON-VACUITY BREAK` token and was restored before verification.

1. Removing the Responses arm's dropped-sentinel fixture step failed `tests/rust-paired-session-replay.test.ts:72`: expected shared `tool:isolated_dropped_placeholder`, received none. The restored arm passed.
2. Removing canonical-field parsing from the live historian reader failed `scripts/audit-transform-wire-parity.test.py:702`: `protected_tail` collapsed into a second `legacy_trigger_false`. The restored differ passed all eight tests.
3. `bun scripts/run-paired-replay-mutation.ts` re-executed the signed-reasoning producer mutation. The mutated run failed `tests/rust-paired-session-replay.test.ts:12` and `:36` with two unadjudicated divergences in each legacy arm; the restored run passed all three paired replay tests. The refreshed record is `packages/e2e-tests/mutations/parity-replay.json`.

No mutation marker remains in source, tests, or fixtures.

## Maintainer documentation

`packages/e2e-tests/README.md` now documents commands, schema-2 provider arms, sanitized fixture rules, setup/sequence semantics, every differ axis, exact adjudication matching, nonzero exit behavior, and the procedure for adding a new provider family. It is written as the instrument contract rather than relying on knowledge of prior hunts.

## Verification

- `bun install --frozen-lockfile` — passed; manifests and lockfile unchanged.
- `python3 scripts/audit-transform-wire-parity.test.py` — passed: 8 tests.
- `bun test tests/paired-session-replay.test.ts src/opencode-runner/spawn.test.ts` — passed: 8 tests.
- `bun test --timeout 700000 --max-concurrency=1 tests/rust-paired-session-replay.test.ts` — passed: 3 tests.
- `bun scripts/run-paired-replay-mutation.ts` — mutated run red at the recorded lines; restored run passed 3 tests.
- `bunx tsc --noEmit -p packages/e2e-tests/tsconfig.json` — changed files typecheck; the command remains red only on the pre-existing `tests/pi-compaction-off.test.ts:60` Bun SQLite versus BetterSqlite3 mismatch.
- Full manifest-derived TS OpenCode leg — passed: 49 tests across 20 files, 0 failed.
- `bun run test:rust-e2e` — passed: 24 tests, 2 documented skips, 0 failed.
- `git diff --check` — passed.
- AFT completed its language phases, but the metrics phase did not return a fresh terminal result; the typecheck and executable suites above are authoritative.
