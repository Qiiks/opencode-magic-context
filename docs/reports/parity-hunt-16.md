# Parity Hunt #16 — aged overlays, temporal carriers, and Channel-2 leases

Date: 2026-09-01

Standing protocol: #15150

State note: #2406

**CLEAN-OR-FINDINGS verdict: CLEAN. The standing counter advances from 1/3 to 2/3; protocol #15150 remains active for one more consecutive CLEAN.**

No behavior finding was convicted. The one repository change outside this report is a coverage closure: the hermetic Rust maintenance test now reads a real durable historian no-fire value and pins its canonical cause. Because no behavior was fixed, no #10588 mutation was applicable.

## Newest-corpus differ and live audit

The rolling cutoff was `2026-08-31T06-45-44-000Z`. The privacy-preserving live audit ran both UTC dates with RPC and the Rust caveman oracle enabled:

```text
python3 scripts/audit-transform-wire-parity.py \
  $DARWIN_USER_TEMP_DIR/opencode-anthropic-auth-dumps \
  --live --date 2026-08-31 \
  --after 2026-08-31T06-45-44-000Z \
  --engine-after 2026-08-31T06-45-44-000Z \
  --per-session 1000
python3 scripts/audit-transform-wire-parity.py \
  $DARWIN_USER_TEMP_DIR/opencode-anthropic-auth-dumps \
  --live --date 2026-09-01 \
  --after 2026-08-31T06-45-44-000Z \
  --engine-after 2026-08-31T06-45-44-000Z \
  --per-session 1000
```

Reports are `/tmp/parity-hunt-16-live-{31,01}.json`. They inspected 5,092 bodies across both capture roots. Collision-free lane coordinates resolved 573: 155 Rust Anthropic, 166 TypeScript Anthropic, and 252 TypeScript OpenAI Responses. The other 4,519 bodies remained privacy-safe unverified inventory. RPC independently returned one Rust and one TypeScript operator snapshot with stable direct/sidebar/status values and authority-correct tag totals.

The same four source-contract axes remained empty: historian producer, mural compose, wrapup, and Hunt 12. The decision-window audit also had zero unexplained invariants. The only unlike-session provider value spaces were the already-adjudicated Anthropic empty-text/reasoning positions and, in the older part of the window, the assistant dropped-placeholder class. The OpenAI empty tool-content observations remained lane-unverified raw-provider inventory.

A second slice used the Hunt 14 deployment instant:

```text
python3 scripts/audit-transform-wire-parity.py ... --live --date 2026-08-31 \
  --after 2026-08-31T23-28-29-000Z --engine-after 2026-08-31T23-28-29-000Z \
  --per-session 1000 --skip-live-rpc --skip-live-rust-oracle
python3 scripts/audit-transform-wire-parity.py ... --live --date 2026-09-01 \
  --after 2026-08-31T23-28-29-000Z --engine-after 2026-08-31T23-28-29-000Z \
  --per-session 1000 --skip-live-rpc --skip-live-rust-oracle
```

Those reports are `/tmp/parity-hunt-16-post-93409-{31,01}.json`. They inspected 776 bodies and resolved 58 Anthropic bodies. The TypeScript-only assistant dropped-placeholder class was absent, confirming live traffic through module `93409fea` or newer no longer exhibited the exemption-theft shape. The remaining empty-text and signed-reasoning positions are unlike-session residuals, not a new conviction.

The non-live differ was also run for both dates and both raw roots with the context and module stores attached; reports are `/tmp/parity-hunt-16-differ-{anthropic,openai}-{31,01}.json`. All source contracts were clean, but current served system bytes omit project roots, so every raw body was excluded from its lane denominator. That pass is recorded as a coverage limitation, not a green result. The live coordinate join above is the non-vacuous provider verdict.

All three same-input replay arms were then driven through the hermetic stack:

```text
bun packages/e2e-tests/scripts/replay-transform-wire-parity.ts --provider-id anthropic
bun packages/e2e-tests/scripts/replay-transform-wire-parity.ts --provider-id mock-anthropic
bun packages/e2e-tests/scripts/replay-transform-wire-parity.ts --provider-arm openai-responses
```

| Arm | Passes | Divergences | Unadjudicated |
|---|---:|---:|---:|
| canonical Anthropic | 4 | 3 signed-thinking differences | 0 |
| non-canonical provider | 4 | 0 | 0 |
| OpenAI Responses | 2 | 0 | 0 |

The canonical arm's three rows matched the exact standing signed-thinking adjudications. Every empty-content, dropped-placeholder, and tool-pairing axis matched in all arms.

## Cheap closure: durable historian canonical cause

`packages/e2e-tests/tests/rust-maintenance-contract.test.ts:45-74` now creates a real hermetic Rust session, drives a low-pressure transform, opens that harness's module store read-only, and selects `$.historian.last_no_fire` from the real `mc_cache_state.meta` row. The observed durable value was:

```text
trigger_false{raw_cause=BelowProactiveFloor,canonical_cause=below_proactive_floor,...}
```

The permanent test requires both the concrete Rust cause and the canonical vocabulary. It passed before the longer maintenance drill in the same harness. This closes Hunt 15's executable-coverage gap without mutating production state. The production read still had two legacy generic rows in the broad window and zero post-deploy rows; that absence is not presented as live production confirmation.

## Fresh roam 1: caveman age-tier bytes on aged real sessions

This was not a synthetic-only golden check. The live audit selected one aged TypeScript session and one aged Rust session with all three persisted caveman depths, recovered their durable source bytes, and sent every sample through both implementations (`scripts/audit-transform-wire-parity-live.ts:612-873`).

| Lane/session prefix | Persisted depth counts | Ordering inversions | TS boundary oracle | Exact TS/Rust compression |
|---|---|---:|---|---|
| TypeScript `ses_00fc` | ultra 75 / full 74 / lite 75 | 0 | matched | 3 source samples × 3 levels |
| Rust `ses_0ad8` | ultra 17 / full 17 / lite 17 | 0 | matched | 3 source samples × 3 levels |

All 18 byte-length/SHA-256 comparisons matched. The TypeScript tier boundary is `computeTargetDepth` in `packages/plugin/src/hooks/magic-context/caveman-cleanup.ts:68-79`; the compressors are `packages/plugin/src/hooks/magic-context/caveman.ts:327-365` and `crates/mc-module/src/caveman.rs:586-610`. The Rust transform's depth/frozen-unit selection is at `crates/mc-module/src/transform.rs:6260-6367`. The shared differential golden also passed. No aged-session boundary or byte divergence was found.

## Fresh roam 2: temporal marker rendering across lanes

A read-only scanner over every newest-window served body joined full session IDs to `session_projects`/`authority_managed`, then counted only marker classes and tag/marker order. It did not emit captured prose.

| Lane/family | Files | Files with markers | Markers | Marker-before-tag inversions |
|---|---:|---:|---:|---:|
| Rust Anthropic | 62 | 62 | 248 | 0 |
| TypeScript Anthropic | 1,919 | 1,747 | 8,464 | 0 |
| TypeScript OpenAI Responses | 3,518 | 26 | 317 | 0 |

The marker-only scan ran after the main differ, so the append-only corpus had grown beyond the 5,092-body differ snapshot while retaining the same lower bound. Every tagged Rust marker had the tag before the marker (248/248). TypeScript also had no marker-before-tag order. Its additional untagged carriers are allowed messages outside the registered authored-text tag surface, not reversed overlays. Both lanes served overlapping minute classes (`+7m`, `+9m`), while the TypeScript Responses traffic additionally exercised hour/day/week formatting.

The durable Rust store contained 182 non-empty markers across 89 values plus 2,240 frozen no-marker decisions; all 182 matched `<!-- +N[u] [N[u]] -->\n`. Formatting is defined by `packages/plugin/src/hooks/magic-context/temporal-awareness.ts:32-106` and `crates/mc-module/src/transform.rs:8185-8220`. The TypeScript-generated dump golden is consumed at `crates/mc-module/src/transform.rs:25143-25243`, including one-transition backfill, exact text bytes, persisted rows, and replay stability. Eight focused Rust temporal tests passed. No value-space, ordering, or replay divergence was found.

## Fresh roam 3: Channel-2 lease/CAS parity

The active host store had 18,880 empty, 1,095 delivered, nine pending, and zero claimed Channel-2 rows. Therefore no live claim was wedged at the ten-minute reap boundary. The Rust module store had 850 empty and four delivered host-state mirrors, with no pending direct directive, active pressure latch, or nonzero arming watermark. For both configured Rust sessions, the host and module rows agreed on `delivered`; claimed-at and claim-token were cleared and no direct directive remained.

OpenCode and Pi intentionally share the same SQLite token CAS (`packages/plugin/src/features/magic-context/storage-meta-persisted.ts:1384-1421`). Both re-check token ownership immediately before send, token-bind confirm/revert, hold unknown baselines pending, cancel known-false predicates to the re-armable empty state, and never re-arm after a post-send confirmation failure. Their delivery paths are `packages/plugin/src/hooks/magic-context/channel2-delivery.ts:131-307` and `packages/pi-plugin/src/ctx-reduce-nudge-pi.ts:227-357`.

Rust OpenCode returns host directives only while the mirrored host lease is empty (`crates/mc-module/src/transform.rs:9650-9693`), so delivery still goes through that same host CAS. Direct Claude transport has no shared host database; it instead freezes a deterministic `(session, arming watermark)` directive ID, replays that ID until a matching delivery echo, and reaps it only after the lease TTL (`crates/mc-module/src/transform.rs:9752-9832`). That transport-specific mechanism is an intentional implementation difference with the same one-delivery/retry contract.

Focused evidence passed: 21 OpenCode Channel-2 tests, 25 Pi nudge tests, and six Rust Channel-2 tests, including foreign-claim protection, failed-send restore, lost post-send confirmation, deterministic IDs, stale lease replacement, and matching-echo consumption. No CAS ownership or cycle-latch divergence was found.

## Adjudications and gaps

| Observation | Decision |
|---|---|
| Unlike-session Anthropic empty text and reasoning positions | Existing residual only. All paired replay axes match except exact signed-thinking rows already adjudicated by pass/axis/value space. |
| Older-window TypeScript-only assistant dropped placeholder | Existing pre-fix evidence. It is absent from the post-`93409fea` resolved slice and matches in paired replay. |
| Lane-unverified Responses empty tool content | Raw provider-origin inventory from Hunt 14. The OpenAI Responses same-input arm has zero divergence. |
| Offline raw differ has no configured lane denominator | Coverage limitation caused by served bytes omitting project roots. The live collision-free coordinate join supplies the non-vacuous 573-body denominator. |
| Claude direct directive ID versus OpenCode/Pi database claim token | Intentional transport difference. Direct transport uses durable echo/TTL because it cannot share the host SQLite CAS; OpenCode Rust uses the host CAS. |
| Zero live Rust wrapup/recomp/dreamer apply rows | Standing observational gaps, not parity findings. Wrapup/recomp and the new no-fire row are executable hermetic contracts. |
| Zero post-deploy production `canonical_cause` row | Production coverage remains absent. The new hermetic durable-row test closes the executable contract without claiming production presence. |

## Mutation status

No behavior finding was convicted and no production behavior was changed. An executed #10588 mutation would therefore test a nonexistent fix and was not run. The new maintenance assertion is a coverage closure; removing or changing either expected cause makes the ordinary test fail directly, so it is not a silent guard requiring a deliberate non-vacuity break.

## Verification

- `bun install --frozen-lockfile` — passed; manifests and lockfile unchanged.
- Full newest-window live audits, non-live differs, post-`93409fea` slice, and all three paired replay arms — passed as detailed above.
- `bun test --timeout 700000 --max-concurrency=1 tests/rust-maintenance-contract.test.ts` — passed: 2 tests.
- `bun test src/hooks/magic-context/channel2-delivery.test.ts` — passed: 21 tests.
- `bun test src/ctx-reduce-nudge-pi.test.ts` — passed: 25 tests.
- `cargo test -p mc-module channel2_` — passed: 6 tests.
- `cargo test -p mc-module temporal_gap_` — passed: 8 tests.
- `cargo test -p mc-module differential_golden_matches_typescript_oracle` — passed: caveman differential in both library/bin targets.
- `bun run test:rust-e2e` — passed: 25 tests, two documented skips, zero failures.
- `bunx tsc --noEmit -p packages/e2e-tests/tsconfig.json` — the package gate remains red only on the pre-existing `tests/pi-compaction-off.test.ts:60` Bun SQLite versus BetterSqlite3 mismatch.
- Changed-file TypeScript check with the package's compiler options — passed.
- `git diff --check` — passed.
- AFT completed 12 phases but timed out waiting for LSP quiescence; the compiler and executable suites above are authoritative.
