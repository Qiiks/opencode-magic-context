# Parity Hunt #20 — event-bus deletion, reversible Pi lifecycle, and residual roams

Date: 2026-09-01

Standing protocol: #15150

Starting counter: 0/3 after three consecutive FINDINGS hunts (#17–#19).

No behavior defect was convicted. This is CLEAN #1 of the new retirement sequence; the counter advances to 1/3.

## Newest-window corpus and same-input replay

The rolling cutoff was `2026-08-31T11-44-59-000Z`. The privacy-preserving live audit opened SQLite read-only and ran both UTC dates with RPC and the Rust caveman oracle enabled:

```text
python3 scripts/audit-transform-wire-parity.py \
  $DARWIN_USER_TEMP_DIR/opencode-anthropic-auth-dumps \
  --live --date 2026-08-31 \
  --after 2026-08-31T11-44-59-000Z \
  --engine-after 2026-08-31T11-44-59-000Z \
  --per-session 1000
python3 scripts/audit-transform-wire-parity.py \
  $DARWIN_USER_TEMP_DIR/opencode-anthropic-auth-dumps \
  --live --date 2026-09-01 \
  --after 2026-08-31T11-44-59-000Z \
  --engine-after 2026-08-31T11-44-59-000Z \
  --per-session 1000
```

Reports are `/tmp/parity-hunt-20-live-{31,01}.json`. They inspected 7,556 bodies across both capture roots. Collision-free lane coordinates resolved 805: 326 Rust Anthropic, 197 TypeScript Anthropic, and 282 TypeScript OpenAI Responses. The remaining 6,751 bodies stayed in privacy-safe unverified inventory. Both reports had zero source-contract, decision-window, maintenance, or caveman unexplained invariants.

The unlike-session Anthropic empty-text/reasoning value spaces remained the standing signed-thinking residuals. The 21 OpenAI Responses empty-content observations were lane-unverified provider inventory, not paired divergences. The live caveman oracle again matched both implementations exactly: three source samples at all three depths for TypeScript `ses_00fc` and Rust `ses_0ad8`, with persisted tier counts matching the TypeScript boundary oracle and zero ordering inversions.

All three same-input replay arms passed:

```text
bun packages/e2e-tests/scripts/replay-transform-wire-parity.ts --provider-id anthropic
bun packages/e2e-tests/scripts/replay-transform-wire-parity.ts --provider-id mock-anthropic
bun packages/e2e-tests/scripts/replay-transform-wire-parity.ts --provider-arm openai-responses
```

| Arm | Passes | Divergences | Unadjudicated |
|---|---:|---:|---:|
| canonical Anthropic | 4 | 3 signed-thinking differences | 0 |
| non-canonical Anthropic | 4 | 0 | 0 |
| OpenAI Responses | 2 | 0 | 0 |

Every empty-content, dropped-placeholder, and tool-pairing axis matched. The canonical arm's three differences were the exact existing provider-native signed-thinking adjudications.

## Seed 1 — same-tick duplicate deletion through the actual event path

Hunt 19 drove concurrent promises directly at the Magic Context handler seam. The new fixture at `packages/plugin/src/hooks/magic-context/event-handler.test.ts:1157` composes that real handler with `packages/plugin/src/plugin/event.ts`, the plugin-level OpenCode event router, then starts two `session.deleted` deliveries for the same session without awaiting either.

The interleave made both module deletion calls observable before either settled. The first module deletion rejected and the second succeeded. The durable result was still exactly the required contract: host rows were retained while neither module result was authoritative, then removed after the successful delivery, and `pending_session_cleanup` ended empty. The full event-handler suite passed this bus-down fixture alongside the Hunt 19 seam matrix. No duplicate-event contract failure was found.

## Seed 2 — Pi reversible lifecycle adjudication

The executable pin is `packages/pi-plugin/src/index-in-process-latch.test.ts:334`. It initializes the real Pi extension registration, persists both a tag and non-default `session_meta` usage values, and drives the registered `session_before_switch` and `session_shutdown` callbacks through the counting Pi event runtime. Both callbacks clear process-local maps but preserve the durable tag and metadata.

The test name states the adjudication directly: **reversible switch and shutdown preserve durable session state**. This distinguishes Pi's in-process cleanup events from OpenCode's irreversible `session.deleted`; a future durable clear in either Pi callback now fails visibly rather than being protected only by comments/source-shape tests.

Non-vacuity was executed before restoration. With the exact token `NON-VACUITY BREAK`, the fixture deleted the durable row after `session_before_switch`; Bun failed at the tag assertion (`index-in-process-latch.test.ts:355`, expected length 1, received 0). The break and token were removed, and the focused and full Pi suites passed.

## Roam 1 — caveman age-tier boundaries under marker advance

The prior live oracle covered aged real rows and all three compression depths, but it did not isolate the exact 20/40/60-percent crossings when `caveman_age_basis_tag` advances. The new Rust transform fixture at `crates/mc-module/src/transform.rs:30161` does:

1. Five eligible tail rows are tagged while caveman is armed.
2. An independent soft bust freezes the five-row population at depths `[3, 2, 1]`.
3. A sixth row arrives on a defer; its tag advances while both frozen units and `caveman_age_basis_tag` remain unchanged.
4. The next independent bust advances the basis and retiers the six-row population to `[3, 3, 2, 1]`.

This pins exact boundary behavior, no first-apply mutation on defer, and marker-gated deepening from original source bytes. A `NON-VACUITY BREAK` held the expected unit count at three after the six-row advance; Cargo failed at the recorded assertion (`transform.rs:30213`, left 4, right 3). The break and token were removed. The focused test and all 1,015 `mc-module` tests passed. No tier-boundary, regrowth, or marker-timing defect was found.

## Roam 2 — Channel-2 lease state across module restart

`packages/plugin/src/hooks/magic-context/module-state-sync.test.ts:240` now persists the host lease as `delivered`, performs a forced cold-start state sync, and verifies that the restarted module receives `channel2_nudge_state: "delivered"`. This closes the restart edge between the shared SQLite CAS and Rust's non-empty host-state suppression: a consumed cycle is not silently re-presented to a fresh module as empty/re-armable.

The host-side claim token, timestamp, confirm/revert, and boot-heal matrix remains covered by the existing Channel-2 suites; this fixture adds the missing forced-sync carrier. No restart re-arm or duplicate-delivery defect was found.

## Roam 3 — Fusiform overlay consumption

The geometry path was traced from both harness configuration loaders through the shared resolver. OpenCode calls `setWindowOverlayPath` at `packages/plugin/src/config/index.ts:627`; Pi does the same at `packages/pi-plugin/src/config/index.ts:448,629`. Both then consume `getWindowOverlay` from `packages/plugin/src/shared/window-geometry.ts:340-348`. A same-path overlay file is a process/config-load snapshot; calling the config setter clears that snapshot. Therefore an external file rewrite alone is consumed by neither live harness, while a config/extension reload refreshes both. Rust does not independently read Fusiform data: OpenCode resolves the geometry in the host and transports `usable_soft`/`usable_hard` on each transform.

The existing split contract also remained intact in the full suites: scheduler compatibility uses the observed usage denominator, nudge copy uses `usable_soft`, and only the absolute emergency wall uses `usable_hard`. No TS/Rust/Pi consumption divergence was found when following the same process-lifecycle boundary. Dynamic file watching would be a separate product behavior decision, not a parity conviction from this hunt.

## Mutation and finding status

Two coverage mutations were executed red-first and restored as detailed above. No production behavior changed, so there was no behavior-fix mutation to run and no finding ID to mint. The repository changes are pinning fixtures plus this report.

## Verification

- `bun install --frozen-lockfile` — passed; 728 installs checked across 936 packages, manifests and lockfile unchanged.
- Newest-window live audits for 2026-08-31 and 2026-09-01 — passed; 7,556 privacy-safe bodies, 805 lane-resolved, zero unexplained contract invariants.
- Three replay arms — passed; zero unadjudicated divergences.
- `bun test src/hooks/magic-context/event-handler.test.ts src/hooks/magic-context/module-state-sync.test.ts` — passed: 68 tests.
- `bun test src/index-in-process-latch.test.ts --test-name-pattern "Pi lifecycle adjudication"` — passed: 1 test.
- `cargo test -p mc-module caveman_age_tiers_cross_exact_boundaries_only_when_the_marker_advances -- --nocapture` — passed.
- `bun run test` in `packages/plugin` — passed: 4,271 tests.
- `bun run test` in `packages/pi-plugin` — passed: 904 tests.
- `bun run typecheck` in `packages/plugin` — passed.
- `bun run typecheck` in `packages/pi-plugin` — passed.
- `python3 scripts/audit-transform-wire-parity.test.py` — passed: 9 tests.
- `bun test scripts/audit-transform-wire-parity-live.test.ts packages/e2e-tests/tests/paired-session-replay.test.ts packages/e2e-tests/tests/rust-paired-session-replay.test.ts` — passed: 9 tests.
- `cargo test --workspace` — passed; `mc-module` 1,011 passed / 4 ignored, all other workspace suites passed.

CLEAN-OR-FINDINGS verdict: CLEAN
