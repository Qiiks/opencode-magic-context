# TS ⇄ Rust ⇄ Pi parity hunt #26

Date: 2026-09-02  
Base: `72b69881ec77c646b212d748a72ac6191a1464f1`

## Verdict

Two fresh parity defects were reproduced and fixed:

1. **Trailing-blank race (P1):** whitespace-only assistant framing was tagged before the TypeScript frozen-shape finalizer, while Rust-mode native serving had no host replay for the same OpenCode projection. A late harness `""` changed the previously served assistant in both cases.
2. **Fable 5.1 binding recovery (P1):** the provider error classifier armed durable recovery, but Rust-mode serving never consumed it. A Rust-mode session could therefore retry the same bound thinking block indefinitely.

The dedicated paired replay is green after the fixes. The other fresh seams are clean in the checked-out tree, with one baseline discrepancy documented below for the described low-usage HARD-flush latch.

## Instruments

### Privacy-preserving live dump audit

Command:

```text
python3 scripts/audit-transform-wire-parity.py --live --date 2026-09-02 --min-provider-bodies 10 --per-session 50 --skip-live-rpc --skip-live-rust-oracle --indent 2
```

Output summary:

- 673 provider bodies admitted.
- 175 had a resolved lane: 51 TypeScript/Anthropic, 61 Rust/Anthropic, and 63 TypeScript/OpenAI Responses.
- 498 bodies remained lane-unverified because their session/project config could not be resolved without the skipped live RPC/oracle legs.
- The audit's cross-lane findings were all explicitly `unlike_session_corpus`; they did not compare the same session sequence. The paired replay below supplies that missing controlled comparison.

### Meter-derived cache-bust analyzer

Command:

```text
bun packages/plugin/scripts/analyze-cache-busts.ts ses_47bb6989 --all-busts --show-diff --limit 20
```

Evidence:

- 12 dumps were analyzed from
  `/var/folders/18/257zzylx4h1gbkcvs4cnpqqc0000gn/T/opencode-anthropic-auth-dumps/2026-09-02T12-33-48-451Z-ses_47bb6989...-req_*.json`.
- Dump 11 was the sole meter-derived bust: previous input 42,385 tokens, cache read 41,502 (98%), short by 883 tokens.
- The first differing message was the final user-tail message; the earlier assistant prefix did not move. This is normal tail growth, not evidence that a Fable variant flip drained queued work.

### Exact trailing-blank paired replay

Command:

```text
bun -e 'import { runPairedTrailingBlankSequenceReplay } from "./src/paired-session-replay.ts"; console.log(JSON.stringify(await runPairedTrailingBlankSequenceReplay(), null, 2));'
```

The arm persists one signed-reasoning/tool-ending assistant, captures the streaming request, drives two next-turn passes, appends the late empty text part, and compares that same assistant's provider-wire bytes.

Post-fix output:

| Stage | TypeScript bytes / SHA-256 | Rust bytes / SHA-256 | frozen decision |
|---|---|---|---|
| streaming | 297 / `96ff6b3d6339...` | 270 / `2a829e18800e...` | strip / strip |
| next-turn pass 1 | 297 / `96ff6b3d6339...` | 270 / `2a829e18800e...` | strip / strip |
| next-turn pass 2 | 297 / `96ff6b3d6339...` | 270 / `2a829e18800e...` | strip / strip |
| late `""` | 297 / `96ff6b3d6339...` | 270 / `2a829e18800e...` | strip / strip |

Result: `ts_stable=true`, `rust_stable=true`, `parity=true`.

The constant 27-byte cross-lane difference is the already-adjudicated reasoning envelope: TypeScript carries one leading one-byte text sentinel before signed thinking, while the native Rust envelope begins with signed thinking. It is present in every stage and is not a fresh trailing-blank divergence.

Implementation/evidence:

- `packages/e2e-tests/src/rust-harness.ts:529` appends the late persisted part without rewriting the accepted prefix.
- `packages/e2e-tests/src/paired-session-replay.ts:819` drives and reports the four-stage paired sequence.
- `packages/e2e-tests/tests/rust-paired-session-replay.test.ts:36` locks both lanes' byte stability.
- Red replay: before the fix Rust pass 1 was `2a829e...` but the late-blank pass became `a37af2...`; `rust_stable=false`.
- `packages/plugin/src/hooks/magic-context/tag-messages.ts:618` now leaves empty assistant framing untagged so the TypeScript frozen-shape finalizer can still recognize it.
- `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:383` now freezes and replays the same decision on the Rust host boundary before native output is installed.

## Fresh seam results

### 1. Trailing-blank race — FIXED (P1)

The controlled replay above reproduced the late suffix mutation and now proves byte stability in both lanes. The replay also preserves `step-finish` during both next-turn passes and appends (rather than replacing) the final `text:""` row.

### 2. Low-usage HARD flush / Fable variant flip — CLEAN, with baseline discrepancy

The checked-out TypeScript tree does not contain the described separate HARD-flush latch. Its existing postprocess drains `pendingMaterializationSessions` after successful heuristic/materialization work at `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:1568`, including a low-usage HARD fold. There is consequently no additional TypeScript latch contract for Rust to mirror at this base.

The provider-specific no-fire path is present and clean:

- `packages/plugin/src/hooks/magic-context/hook-handlers.ts:271` gates variant refresh through `variantChangeBustsProviderCache`.
- `packages/plugin/src/hooks/magic-context/hook-handlers.test.ts:678` proves a Fable 5.1 variant flip signals none of the refresh/materialization sets.
- Focused result: 1 pass, 0 failures, 4 assertions.

This means a Fable effort flip itself cannot manufacture the second drain cycle in the checked-out implementation. The task's stated “armed until the next measured pass” behavior should be reconciled with the base revision before treating it as a Rust parity requirement.

### 3. Fable 5.1 bound-thinking retry — FIXED (P1)

The event classifier already armed `newest_reasoning_bearing_assistant` (`packages/plugin/src/hooks/magic-context/event-handler.test.ts:184`), and TypeScript postprocess consumed it. Rust-mode native serving did neither.

Fix:

- Rust host postprocess persists the selected assistant ID before stripping, replays that ID on rebuilt arrays, and returns the applied flag (`packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts:383`).
- The Rust transform enables the behavior only for canonical Anthropic Fable 5.1 and clears the durable flag only after successful output installation (`packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:2770`, `:2904`).
- Regression: `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.test.ts:3346`.

Red-first evidence: the new regression initially failed at line 3384 with `TypeError: undefined is not an object (evaluating 'recovery.thinkingBindingRecovery')`. After the fix: 1 pass, 0 failures, 6 assertions. The provider error-classifier test also passes (1 test, 2 assertions).

### 4. Active-session auto-embed latch — CLEAN

Both hosts implement the same outcome contract:

- OpenCode: `packages/plugin/src/hooks/magic-context/hook.ts:721-785`.
- Pi: `packages/pi-plugin/src/commands/ctx-embed.ts:271-322`.

Both acquire the process latch before async work, mark every drain return (including `busy`, `disabled`, `nothing`, and `stalled`) terminal, retain the latch after terminal outcomes, and release it on pre-drain exits or throws. Focused gates passed:

- OpenCode first-transform/zero-work rearm: 1 pass, 0 failures, 3 assertions (`hook.test.ts:444`).
- Pi zero-work rearm followed by successful terminal drain: 1 pass, 0 failures, 5 assertions (`ctx-embed.test.ts:195`).

### 5. Curate-memory safety under module authority — CLEAN

`ctx_memory` runs the shared `runCuratePreflight` before consulting or dispatching to Rust module authority. The new module-authority regression at `packages/plugin/src/tools/ctx-memory/tools.test.ts:1262` covers all three destructive refusal classes:

- successor-less archive,
- user-profile rationale applied to project-scoped memory,
- directive-shaped `PROJECT_RULES` archive.

All three returned their refusal reason, remained active in the mirror, and made zero module backend calls (1 test, 5 assertions). A non-vacuity mutation bypassing the preflight made the test fail with routed results `module archive`, `module merge`, `module merge`; restoring the guard returned it to green.

### 6. Pi Node-WASM twin — CLEAN

- Pi's build script invokes the shared Node-WASM builder and emits `dist/transformers-node-wasm.js` (`packages/pi-plugin/package.json:31`).
- The runtime chooses that sibling whenever the host has Node filesystem access (`embedding-local.ts:366-373`), while browser-like hosts retain `transformers-web.js`.
- `bun run build` in `packages/pi-plugin` emitted the Node target successfully.
- `bun scripts/verify-transformers-node-wasm.ts ../pi-plugin/dist/transformers-node-wasm.js` printed `transformers-node-wasm filesystem cache probe passed`.
- Runtime-host tests passed for vulnerable Bun, Node filesystem fallback, and browser isolation: 3 tests, 0 failures, 11 assertions.

## Verification summary

- Plugin production build: passed, including declaration emission.
- Exact TS/Rust paired replay gate: 1 pass, 0 failures, 7 assertions.
- Fable classifier and Rust binding recovery focused gates: passed.
- Curate module-authority refusal gate plus deliberate non-vacuity break: passed/red/passed as expected.
- OpenCode/Pi auto-embed focused gates: passed.
- Pi build, Node-WASM filesystem cache probe, and runtime host-selection tests: passed.
- Full impacted plugin suites: 252 tests passed, 0 failed, 2,947 assertions.
- Full paired-replay file: three tests passed; one OpenCode boot first failed readiness after repeated provider HTTP 499 responses, then its isolated rerun passed (19 assertions). The new trailing-blank arm passed again after the final harness edit.
- `bunx tsc --noEmit` in `packages/e2e-tests` reaches only the pre-existing `tests/pi-compaction-off.test.ts:60` `bun:sqlite.Database` versus `BetterSqlite3.Database` incompatibility; no changed-file type error was reported. The plugin production build and declaration emission passed.
