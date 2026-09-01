# Parity Hunt #22 — Pi hot reload, user-hint query bytes, and raised latch bands

Date: 2026-09-01

Standing protocol: #15150

Starting counter: 0/3 after Hunt 21's finding reset the sequence.

## Verdict and fence

**FINDINGS — NOT CLEAN.** This hunt convicted and fixed one independently observable behavior defect, **H22-01 RUST-AUTO-SEARCH-QUERY-WHITESPACE-COLLAPSE**. The consecutive-CLEAN counter remains **0/3**. The series now contains 43 defects across 22 hunts.

TypeScript remains canonical. No serializer, schema, migration, provider, authority, deployment, or project fence moved. Live SQLite access was read-only through the standing audit. This report contains counts, fixed vocabulary, byte counts, and short prefixes only; it contains no session content, full session identifiers, project paths, RPC credentials, or database rows.

## Seed 1 — Pi same-path overlay hot reload

`packages/e2e-tests/tests/window-overlay-reload.test.ts` now drives Pi's actual in-process extension/resource reload instead of replacing the process. The e2e harness loads a tiny test extension from `packages/e2e-tests/src/pi-runner/reload-extension.mjs`; its command awaits Pi's `ctx.reload()`. `PiTestHarness.reloadExtensions()` invokes that command through the persistent Pi RPC process.

The live fixture still proves all three phases against the same absolute overlay path:

1. A 100,000-token enforced window yields 21.784593935169045% persisted pressure.
2. Rewriting the same file to 160,000 tokens leaves the live Pi snapshot at 21.784593935169045%.
3. Hot-reloading extensions in the same RPC process preserves the Pi session id and refreshes pressure to 13.174536256323776%.

OpenCode retains its existing restart leg. The unchanged Pi process and unchanged session id distinguish this from Hunt 21's cold restart. The fixture proves that the Pi entrypoint's `reloadWindowOverlay` call is reached by the host's real reload lifecycle.

Non-vacuity was executed before restoration. The Pi entrypoint temporarily used ordinary `setWindowOverlayPath` under the exact `NON-VACUITY BREAK` token, suppressing the same-path refresh. The focused live fixture failed at `window-overlay-reload.test.ts:134`: expected 13.174536256323776%, received 21.784593935169045%. The break and token were removed, the Pi distribution rebuilt, and the fixture passed.

## Seed 2 — overlay e2e load classification

The dual-harness real-process drill is now explicitly `excluded` in `packages/e2e-tests/mode-manifest.json`. It remains a focused lifecycle gate after overlay or Pi reload changes, but the manifest-derived ordinary OpenCode/Pi host lanes no longer absorb its two daemons, six model turns, and restart/reload phases. The manifest validator pins the single excluded file and still requires every live e2e test to appear exactly once. This is test-load tooling, not a behavior finding.

## H22-01 RUST-AUTO-SEARCH-QUERY-WHITESPACE-COLLAPSE — fixed

### Conviction

The adversarial same-input fixture combines multiple user text blocks, a Magic Context tag prefix, nested system reminders, a multiline HTML comment, generic XML markup, repeated spaces, a tab, and excess newlines. TypeScript's production `runAutoSearchHint` seam passed the exact canonical query bytes:

```text
alpha   beta\tgamma

delta

omega  end
```

The new Rust fixture went red before the fix at `crates/mc-module/src/transform.rs:25637`:

```text
left:  "alpha beta gamma delta omega end"
right: "alpha   beta\tgamma\n\ndelta\n\nomega  end"
```

Rust's `sanitize_user_hint_query` stripped the same transport markup as TypeScript, but then called `split_whitespace().join(" ")`. TypeScript canonically preserves interior spaces, tabs, and one/two newline runs; it only removes horizontal whitespace immediately before a newline, folds three-or-more newlines to two, and trims the ends. The Rust lane therefore searched different FTS/embedding query bytes for the same prompt, which could change result ranking and whether a transform-time memory hint appeared.

### Fix

Rust now applies the TypeScript normalization sequence after reminder/comment/markup/tag removal: remove spaces or tabs immediately before `\n`, fold `\n{3,}` to `\n\n`, then trim. Existing Rust assertions that encoded the collapsed-whitespace behavior were updated to the canonical bytes. The focused TypeScript fixture and all Rust user-hint query fixtures passed; the complete Rust workspace passed afterward.

## Free-roam 2 — emergency episode latch across the raised T+2 ladder

New paired fixtures exercise every valid raised execute threshold from 84% through the configured 90% cap. At each threshold, both implementations now prove the four-state episode:

1. `T+1` does not arm.
2. `T+2` arms at the exact derived force boundary.
3. Returning to `T+1` holds the original episode timestamp.
4. Falling below `T-10` clears the latch.

The TypeScript fixture drives the durable SQLite reserve path in `emergency-drain-latch.test.ts`; the Rust fixture drives `advance_drain_latch` in `scheduler.rs`. All seven rungs matched. No force-band entry, episode hold, timestamp, or exit differential was found.

Both fixtures were mutation-proved with the exact `NON-VACUITY BREAK` token and restored. TypeScript failed at `emergency-drain-latch.test.ts:161`, expected 0 and received 5600001. Rust failed at `scheduler.rs:1426` on threshold 84, expected `None` and received `Some(2000001)`.

## Newest-window corpus and same-input replay

The rolling cutoff remained `2026-08-31T11-44-59-000Z` for direct comparison with Hunts 20 and 21. The privacy-preserving audit opened SQLite read-only and ran both UTC dates with RPC and the Rust caveman oracle enabled. Reports are `/tmp/parity-hunt-22-live-{31,01}.json`.

Across both capture roots, the audit inspected 8,688 bodies. Collision-free lane coordinates admitted 811: 330 Rust Anthropic, 199 TypeScript Anthropic, and 282 TypeScript OpenAI Responses. The remaining 7,877 bodies stayed in privacy-safe unverified inventory. There were zero ambiguous capture hashes and zero aggregate source-contract, decision-window, maintenance, historian-producer, mural, wrapup, or caveman unexplained invariants.

The unlike-session Anthropic empty-text/reasoning value spaces remain the standing provider-native signed-thinking residuals. OpenAI Responses empty-content observations remained lane-only or unverified inventory with valid adjacency, not same-input evidence. The live caveman oracle again matched TypeScript and Rust source bytes at all three depths with zero ordering inversions.

All three same-input replay arms passed:

| Arm | Passes | Divergences | Unadjudicated |
|---|---:|---:|---:|
| canonical Anthropic | 4 | 3 signed-thinking differences | 0 |
| non-canonical Anthropic | 4 | 0 | 0 |
| OpenAI Responses | 2 | 0 | 0 |

Every empty-content, dropped-placeholder, and tool-pairing axis matched. The canonical arm's three differences remain the existing provider-native signed-thinking adjudications.

## Mutation and finding status

Three coverage mutations used the exact `NON-VACUITY BREAK` token and were restored: the Pi reload boundary and both emergency-latch ladder fixtures. H22-01 was convicted red-first by a same-input canonical byte assertion before production code changed. No mutation token remains in source or tests.

## Verification

- `bun install --frozen-lockfile` — passed; workspace manifests and lockfile unchanged.
- Two newest-window live audits — passed read-only: 8,688 bodies, 811 lane-resolved, zero aggregate unexplained contract invariants.
- Three same-input replay arms — passed with zero unadjudicated divergences.
- Focused real-process overlay reload fixture — failed under the recorded non-vacuity break, then passed after restoration.
- Mode-manifest validator — passed: 5 tests; the heavy overlay drill is excluded from ordinary host lanes.
- Focused TypeScript/Rust auto-search sanitization fixtures — TypeScript passed; Rust failed red-first with collapsed bytes, then all four Rust user-hint query tests passed after the fix.
- Focused TypeScript/Rust raised-threshold latch fixtures — passed after both recorded non-vacuity breaks were restored.
- Root TypeScript gate (plugin, Pi, CLI, and retina-local-fs) — passed.
- Broad e2e `tsc` retained the unrelated baseline `pi-compaction-off.test.ts:60` Bun-SQLite versus BetterSqlite3 mismatch; a narrow `tsc --noEmit` over every changed e2e TypeScript file passed.
- Full OpenCode plugin suite — 4,276 passed, 0 failed.
- Full Pi plugin suite — 905 passed, 0 failed.
- Complete Rust workspace — 1,175 passed, 4 ignored, 0 failed; `mc-module` was 1,014 passed / 4 ignored.
- Python differ suite — 9 passed.
- Live/replay TypeScript differ suite — 9 passed.
- Rust formatting and `git diff --check` — passed.

CLEAN-OR-FINDINGS verdict: FINDINGS
