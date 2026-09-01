# Parity Hunt #21 — driven overlay lifecycle, Channel-2 lease matrix, and restart seams

Date: 2026-09-01

Standing protocol: #15150

Starting counter: 1/3 after Hunt 20's CLEAN #1.

## Verdict and fence

**FINDINGS — NOT CLEAN.** This hunt convicted and fixed one independently observable behavior defect, **H21-01 OVERLAY-SAME-PATH-AUTO-REFRESH**. The consecutive-CLEAN counter resets to **0/3**. The series now contains 42 defects across 21 hunts.

TypeScript remains canonical. No serializer, schema, migration, provider, authority, deployment, or project fence moved. Live SQLite access was read-only through the standing audit. This report contains counts, fixed vocabulary, byte counts, and short hash prefixes only; it contains no session content, full session identifiers, project paths, RPC credentials, or database rows.

## Seed 1 — same-path Fusiform overlay, driven through both live harnesses

The new executable fixture is `packages/e2e-tests/tests/window-overlay-reload.test.ts` — **keeps OpenCode and Pi on their snapshots until each live harness restarts**. It gives both real harness processes the same absolute overlay path and drives three phases with a 200,000-token catalog window:

1. An enforced 100,000-token overlay yields a 91,808-token usable window and 21.784593935169045% persisted pressure for 20,000 input tokens.
2. The exact same file is rewritten in place to enforce 160,000 tokens while each process remains live. A process snapshot must remain at 21.784593935169045%.
3. Each harness performs a cold config/extension reload by restarting against the same isolated config and data roots. Both must then refresh to a 151,808-token usable window and 13.174536256323776% pressure.

The fixture uses real `opencode serve` and Pi RPC processes, real plugin distributions, real config loaders, and the persisted `session_meta.last_context_percentage` result. `TestHarness.restart`, `PiRpcClient.restart`, and `PiTestHarness.restart` preserve the isolated roots while replacing the live process. The narrow loader fixtures in both config suites additionally prove that routine same-path config reads do not invalidate the snapshot, while an explicit plugin/extension reload refreshes it.

### H21-01 OVERLAY-SAME-PATH-AUTO-REFRESH — fixed

#### Conviction

The first live run went red before the fix. After the in-place rewrite, OpenCode had already changed to 13.174536256323776% while Pi correctly remained at 21.784593935169045%. The aggregated assertion failed at `window-overlay-reload.test.ts:135`:

```text
Expected: [21.784593935169045, 21.784593935169045]
Received: [13.174536256323776, 21.784593935169045]
```

OpenCode re-resolves runtime config during ordinary event processing. `setWindowOverlayPath` unconditionally cleared the shared overlay cache even when the configured path had not changed, so that routine config read silently turned an external file rewrite into a live geometry change. Pi did not perform the same routine reload and retained its process snapshot. Scheduler percentages, compaction thresholds, nudge copy, and the emergency wall could therefore diverge between the two harnesses without either process crossing a documented reload boundary.

#### Fix

`setWindowOverlayPath` now preserves the process snapshot when the configured path is unchanged. A genuine path change still invalidates immediately. `reloadWindowOverlay` provides the explicit same-path refresh boundary and is called by both the OpenCode plugin and Pi extension entrypoints. The restored live fixture passed all three phases for both harnesses, and both loader fixtures passed routine-read plus explicit-reload assertions.

## Seed 2 — named Channel-2 remaining matrix

The Hunt 20 assertion is now an explicit executable matrix rather than an unnamed citation:

| Contract | Executable evidence |
|---|---|
| Claim stores a non-empty generated token and timestamp | `channel2-delivery.test.ts` — **delivers via the in-process client and consumes the current cycle** reads the live claim inside `promptAsync`; the Pi counterpart is `ctx-reduce-nudge-pi.test.ts` — **regression: queues the model-visible ceiling nudge for the next real turn**. |
| Confirm consumes only the matching claimed lease | `channel2-delivery.test.ts` — **does not confirm when only the in-flight claim token changes** leaves the foreign token and claimed state intact. Existing sibling-state pins remain **preserves a sibling's delivered claim when token confirmation is no longer ours** and Pi **preserves a sibling's delivered lease when token confirmation is lost**. |
| Revert restores only the matching claim | `channel2-delivery.test.ts` — **reverts claimed→pending on send failure (cap not burned)** now asserts state, zero timestamp, and empty token. Pi directly supplies the opposite arm in **refuses a foreign revert attempt against a live claim**. |
| Boot heal repairs stale leases and preserves fresh metadata | `channel2-delivery.test.ts` — **boot-reaps only ten-minute claimed leases, not fresh live claims** now asserts preservation of the fresh token and clearing of all stale metadata; **cache-hit openDatabase heals stale claimed leases for long-lived processes** covers the already-open process. |

The new token-only confirm fixture closes the actual gap found while naming the matrix. The focused OpenCode suite passed 22 tests; the full Pi suite retains its matching and foreign-ownership arms. No additional Channel-2 behavior defect was found.

## Free-roam

### Rust `ctx_expand` over migration-50 deflate archives

`crates/mc-module/src/lib.rs` — **ctx_expand_uses_durable_raw_messages_for_exact_ranges_and_snapshot_loss** directly publishes original CK messages through `raw_chunk_messages`, which stores the migration-50 `raw_messages_deflate` payload. With no transform snapshot installed, the fixture recovered only ordinals 50–55 from a 1–100 archive, returned the complete 20,000-line tool result for `message=52`, and returned the correct tool-output size in `verbose=true` mode. The focused test passed. The recovery comes from the deflate archive, not a live raw-row proxy or the condensed historian transcript.

### Wrapup dispositions across module restart

The new Rust fixture `crates/mc-module/src/lib.rs` — **all_terminal_wrapup_dispositions_replay_after_module_restart** records `completed`, `nothing_to_compact`, and structured `failed` terminal commands, drops both the handler and store, reopens SQLite, binds a fresh handler, and retries all three command ids. Every response preserves its disposition, rounds, summary, and failure fields with `replayed: true`; the historian producer start count remains zero. This closes the restart seam left implicit by the existing same-handler replay fixtures. No disposition loss or second drive was found.

### Synthetic-todo relocation ride rule, TypeScript versus Rust

The new TypeScript production-seam fixture `transform-todo-state.test.ts` — **rides an unchanged synthetic pair to the new tail anchor and replays it on defer** folds away the persisted assistant anchor, keeps todo state unchanged, and verifies that the exact synthetic bytes and call id move to the new tail assistant and replay there on defer.

The Rust counterparts `transform.rs` — **synthetic_todo_keep_reanchors_when_coverage_advance_folds_anchor** and **synthetic_todo_defer_after_keep_reanchor_replays_at_new_position** passed. They additionally pin native OpenCode message ordering after relocation. The ride rule, frozen pair bytes, persisted anchor, and following defer position matched across the two implementations.

## Newest-window corpus and replay arms

The rolling cutoff remained Hunt 20's `2026-08-31T11-44-59-000Z` so the new window is directly comparable. The privacy-preserving live audit opened SQLite read-only and ran both UTC dates with RPC and the Rust caveman oracle enabled:

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

Reports are `/tmp/parity-hunt-21-live-{31,01}.json`. They inspected 7,691 bodies across both capture roots. Collision-free coordinates admitted 806: 326 Rust Anthropic, 198 TypeScript Anthropic, and 282 TypeScript OpenAI Responses; 6,885 remained privacy-safe unverified inventory. There were zero ambiguous capture hashes and zero aggregate source-contract, decision-window, maintenance, historian-producer, mural, wrapup, or caveman unexplained invariants.

The unlike-session Anthropic empty-text/reasoning value spaces remain the standing signed-thinking residuals. Twenty-one Responses empty-content observations consisted of the previously adjudicated TypeScript `4506f17b…` capture plus lane-unverified inventory with no adjacency violation; they are not same-input evidence.

All three same-input replay arms passed:

| Arm | Passes | Divergences | Unadjudicated |
|---|---:|---:|---:|
| canonical Anthropic | 4 | 3 signed-thinking differences | 0 |
| non-canonical Anthropic | 4 | 0 | 0 |
| OpenAI Responses | 2 | 0 | 0 |

Every empty-content, dropped-placeholder, and tool-pairing axis matched. The canonical arm's three differences remain the exact provider-native signed-thinking adjudications.

## Non-vacuity mutations

Every temporary mutation contained the exact token `NON-VACUITY BREAK` and was restored before final verification:

1. Disabling the same-path overlay guard made the focused loader fixture fail at `config/index.test.ts:142`, expected `before-rewrite`, received `after-rewrite`.
2. Replacing the Channel-2 foreign-token assertion made the focused fixture fail at `channel2-delivery.test.ts:371`, expected `NON-VACUITY BREAK`, received `foreign-token`.
3. Replacing the restarted wrapup summary made Cargo fail at `lib.rs:27857`, where the replay returned `finished` instead of `NON-VACUITY BREAK`.
4. Replacing the relocated todo anchor made Bun fail at `transform-todo-state.test.ts:350`, expected `NON-VACUITY BREAK`, received `msg-asst-3`.

No mutation token remains in source or tests.

## Verification

- Frozen workspace install — passed; manifests and lockfile unchanged.
- Two newest-window live audits — passed read-only: 7,691 bodies, 806 lane-resolved, zero aggregate unexplained invariants.
- Three same-input replay arms — passed with zero unadjudicated divergences.
- Focused dual-harness overlay fixture — red before the fix as recorded above, then passed after the fix.
- Focused Channel-2, wrapup-restart, migration-50 `ctx_expand`, and TypeScript/Rust todo-relocation fixtures — passed after restoration.
- Plugin/Pi/CLI build — passed.
- Root TypeScript gate (plugin, Pi, CLI, and retina-local-fs) — passed.
- Changed E2E harness files — passed a narrow `tsc --noEmit`; the broad E2E tsconfig retains the unrelated baseline `pi-compaction-off.test.ts:60` Bun-SQLite versus BetterSqlite3 type mismatch.
- Full OpenCode plugin suite — 4,274 passed, 0 failed.
- Full Pi plugin suite — 905 passed, 0 failed.
- Complete Rust workspace — 1,173 passed, 4 ignored, 0 failed; `mc-module` was 1,012 passed / 4 ignored.
- Python differ suite — 9 passed.
- Live/replay TypeScript differ suite — 9 passed.
- Changed Rust file formatting and `git diff --check` — passed.

CLEAN-OR-FINDINGS verdict: FINDINGS
