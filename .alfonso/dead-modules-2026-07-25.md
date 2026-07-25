# Dead Modules Audit — 2026-07-25

**Scope:** `packages/plugin/src`, `packages/pi-plugin/src`, `packages/cli/src`, `crates/mc-module/src`, `crates/mc-core/src`, `crates/mc-store/src`

**Method:** Module-level transitive-closure reachability from all production entry surfaces + function-level `aft_inspect` dead-code/unused-exports seed, hand-verified.

**Report-only audit.** No source changes. No fixes. No deletions.

---

## Methodology

### Entry surfaces seeded (generously)

**TypeScript (unified graph across all three packages):**
- `packages/plugin/src/index.ts` — plugin package `exports["."]` entry (server plugin)
- `packages/plugin/src/tui/index.tsx` — plugin package `exports["./tui"]` entry (TUI source)
- `packages/plugin/src/tui-compiled/index.tsx` — precompiled TUI fallback loaded by `src/tui/entry.mjs`
- `packages/pi-plugin/src/index.ts` — Pi extension entry (`pi.extensions` in package.json)
- `packages/pi-plugin/src/subagent-entry.ts` — Pi subagent extension entry (loaded via `--extension` flag)
- `packages/cli/src/index.ts` — CLI binary entry (`bin.magic-context`)
- All `import()` dynamic import sites resolved relative to their host file (CLI lazy-loads commands; plugin lazy-loads notification/conflict hooks)

**Path aliases resolved:**
- `@magic-context/core/*` → `packages/plugin/src/*` (used by pi-plugin and cli)
- `@magic-context/pi-core/*` → `packages/pi-plugin/src/*` (used by cli)

**Rust:**
- `crates/mc-module/src/lib.rs` (crate root, `pub mod` declarations)
- `crates/mc-module/src/main.rs` (binary entry, `ck-mc`)
- `crates/mc-core/src/lib.rs` (crate root)
- `crates/mc-store/src/lib.rs` (crate root)

### Re-exports count as wiring

Barrel files (`export * from "./submodule"`) are traced through: a barrel is only dead if NO production code imports the barrel itself (consumers may import submodules directly, bypassing the barrel). A barrel whose submodules are all reached via direct imports is dead even if the submodules are live.

### Transitive closure

BFS from all entry surfaces across the unified import graph. A file is unreachable if no entry-point-reachable code reaches it through any import chain. A subtree that is internally interconnected but has no inbound edge from reachable code is collectively dead.

### Validation (detector tested on known-live modules)

Before trusting any finding, the detector was validated against modules confirmed live today:

| Known-live module | Status | Evidence |
|---|---|---|
| Mural renderer (`mural/render-mural.ts`, `mural/storage-mural.ts`, etc.) | ✅ REACHABLE | Rendered a wall yesterday |
| `hooks/magic-context/module-transport.ts` (shadow-sender, rust mode) | ✅ REACHABLE | Live in rust mode |
| `hooks/magic-context/decay-render.ts` | ✅ REACHABLE | Consumed by transform pipeline |
| `hooks/magic-context/compartment-render-epoch.ts` | ✅ REACHABLE | Compartment rendering |
| Stop-reason surface (`pi-historian-runner.ts`, `subagent-runner.ts`, `transcript-pi.ts`, etc.) | ✅ REACHABLE | Added yesterday, consumed by `scripts/backfill-embeddings.ts` |
| Drive-fault feature module (Rust, `#[cfg(feature = "drive-fault")]`) | ✅ NOT FLAGGED | Feature-gated, live under `--features drive-fault` |

All known-live modules passed validation. No false negatives on the validation set.

### Excluded classes (known-fine, not reported as dead)

- **Generated files:** `*.generated.ts` (historian-prompt, reference-seeds, mural-font) — generated, live by consumption
- **tui-compiled/** — generated build artifact (`scripts/build-tui.ts` output), shipped in npm package, loaded by `entry.mjs` fallback. Not dead.
- **`.d.ts` ambient type declarations** — `tui/types/opencode-plugin-tui.d.ts` and `tui-compiled/types/opencode-plugin-tui.d.ts` provide `declare module "@opencode-ai/plugin/tui"` consumed by `tui/index.tsx` and `tui-compiled/index.tsx` type imports. Live type infrastructure.
- **Test seams:** `__setXTestHooks`, `__test`, `_reset*ForTests` patterns — test-only by design
- **cfg-gated code:** `drive-fault` feature module — intentionally feature-gated, not dead
- **Pi/OpenCode twin exports:** where one harness is the only consumer (e.g. `subagent-runner.ts` consumed by pi-plugin via `@magic-context/core/` alias)
- **Migration `up()` bodies:** invoked dynamically
- **Entry-point default exports:** `subagent-entry.ts::default` — consumed by Pi's extension loader via `pi.extensions` string config (tool-registry string lookup pattern)

---

## Findings

### MODULE-LEVEL DEAD CODE

Ranked by line count. All verified: zero production importers outside their own subtree, confirmed by full-text search for the module name across all `packages/` and `scripts/` (not just import-statement search).

#### 1. `packages/plugin/src/shared/transcript-opencode.ts` — 302 lines

| Field | Value |
|---|---|
| **What it was** | OpenCode adapter for the harness-agnostic `Transcript` interface. A thin proxy over OpenCode's `MessageLike[]` that let the transform pipeline work for both OpenCode and Pi without harness branching. |
| **Last commit** | `e109c126` 2026-06-26 |
| **Confidence** | HIGH — zero importers anywhere (no production code, no test code, no scripts) |
| **Evidence** | Full-text search for `transcript-opencode` across `packages/` and `scripts/` returns zero matches outside the file itself. The module header says "By the end of 4b the only OpenCode-aware code in the plugin is this file plus `messages-transform.ts`" — but the refactor that would have consumed it never landed, or it was superseded by a different approach. The `Transcript` interface is now consumed via `shared/transcript.ts` (which IS reachable, used by pi-plugin's `transcript-pi.ts`). |
| **Disposition** | **Delete.** The harness-agnostic transcript abstraction it was meant to enable is served by `shared/transcript.ts` + `pi-plugin/transcript-pi.ts` instead. |

#### 2. `packages/plugin/src/features/magic-context/plugin-messages.ts` — 237 lines

| Field | Value |
|---|---|
| **What it was** | SQLite-backed message bus for TUI ↔ server plugin communication (toasts, dialogs, state updates). Written by one side, consumed by the other via polling. |
| **Last commit** | `3b9f6ec7` 2026-07-14 |
| **Confidence** | HIGH — zero importers anywhere (no production, no tests, no scripts). All 5 exported functions (`sendToServer`, `peekMessages`, `sendTuiToast`, `sendTuiConfirmDialog`, `checkDialogResult`) are flagged as dead code by `aft_inspect`. |
| **Evidence** | Full-text search for `plugin-messages` returns zero matches outside the file. TUI↔server communication now uses the RPC layer (`shared/rpc-server.ts`, `shared/rpc-client.ts`) and `send-session-notification.ts` instead. |
| **Disposition** | **Delete.** Superseded by the RPC-based notification layer. |

#### 3. `packages/plugin/src/hooks/magic-context/compartment-runner-state-xml.ts` — 83 lines

| Field | Value |
|---|---|
| **What it was** | XML state builder for the compartment runner historian prompt — `buildExistingStateXml()` and `mergePriorCompartments()`. Constructed the `<compartment>` / `<facts>` / memory-block XML fed to the historian. |
| **Last commit** | `2c6c73c8` 2026-03-30 |
| **Confidence** | HIGH — zero importers anywhere (no production, no tests). Both exported functions flagged as unused exports by `aft_inspect`. |
| **Evidence** | Full-text search for `compartment-runner-state-xml` returns zero matches outside the file. The compartment runner now builds state XML via a different path (likely inline in `compartment-runner.ts` or `compartment-prompt.ts`). |
| **Disposition** | **Delete.** The XML state construction moved elsewhere; this module is an orphaned extraction. |

#### 4. `packages/plugin/src/shared/opencode-compaction-detector.ts` — 81 lines

| Field | Value |
|---|---|
| **What it was** | OpenCode auto-compaction config detector — read `opencode.jsonc` to check if `compaction.auto` / `compaction.prune` were enabled, for conflict warning. |
| **Last commit** | `2c6c73c8` 2026-03-30 |
| **Confidence** | HIGH — zero production importers. Only referenced by its own test file (`opencode-compaction-detector.test.ts`) and a comment in `conflict-detector.ts` ("Compaction detection (extracted from opencode-compaction-detector.ts)"). |
| **Evidence** | The comment in `conflict-detector.ts:97` explicitly says the logic was **extracted from** this file — meaning `conflict-detector.ts` now contains the live copy and this file is the abandoned original. |
| **Disposition** | **Delete.** Logic was moved to `conflict-detector.ts`; this is the leftover pre-extraction copy. |

#### 5. `packages/plugin/src/hooks/magic-context/issue-135-wire-fixtures.ts` — 23 lines

| Field | Value |
|---|---|
| **What it was** | Captured wire fixtures from issue #135 (openai-compat pinning harness). A single exported constant `ISSUE_135_ORPHAN_WIRE` — a known-fail test case. |
| **Last commit** | `eb0a3938` 2026-06-12 |
| **Confidence** | HIGH — zero production importers. Only imported by `openai-compat-adjacency.test.ts` (test-only consumer). |
| **Evidence** | Full-text search shows only `openai-compat-adjacency.test.ts:2` imports it. Test-only = dead from production. |
| **Disposition** | **Delete** (or move to a test-fixtures directory if the test still needs it). The fixture is test data, not production code. |

#### 6. `packages/cli/src/lib/opencode-install.ts` — 10 lines

| Field | Value |
|---|---|
| **What it was** | Stock OpenCode install path resolver (`resolveStockOpenCodeBinary()` → `~/.opencode/bin/opencode`). |
| **Last commit** | `13f17675` 2026-06-28 |
| **Confidence** | HIGH — zero importers anywhere (no production, no tests). `resolveStockOpenCodeBinary` flagged as unused export by `aft_inspect`. |
| **Evidence** | Full-text search for `opencode-install` returns zero matches outside the file. The CLI's OpenCode detection uses `lib/opencode-detect.ts` and `lib/opencode-helpers.ts` instead. |
| **Disposition** | **Delete.** Never wired in; the install-path resolution it was meant to provide is handled elsewhere. |

#### 7. `packages/plugin/src/features/magic-context/index.ts` — 12 lines (barrel)

| Field | Value |
|---|---|
| **What it was** | Barrel re-exporting 12 submodules of `features/magic-context/` (compaction, compartment-storage, dreamer, memory, range-parser, scheduler, search, sidekick, smart-notes, storage, tagger, types). |
| **Last commit** | `34f5ef74` 2026-06-22 |
| **Confidence** | HIGH — zero importers. No production code imports `features/magic-context` (without a trailing submodule path). All 12 re-exported submodules are reached via direct imports by their consumers. |
| **Evidence** | Precise search for `from .../features/magic-context` (no trailing slash/subdir) returns zero matches. The barrel was likely created for a planned public API surface that was never adopted; consumers import submodules directly. |
| **Disposition** | **Delete.** Dead barrel — all re-exported submodules are live and reached directly. |

#### 8. `packages/plugin/src/features/magic-context/dreamer/index.ts` — 5 lines (barrel)

| Field | Value |
|---|---|
| **What it was** | Barrel re-exporting 5 dreamer submodules (lease, protected-regions, storage-dream-runs, storage-dream-state, task-prompts). |
| **Last commit** | `485c0d33` 2026-06-19 |
| **Confidence** | HIGH — zero importers. No production code imports `features/magic-context/dreamer` (without a trailing submodule path). All re-exported submodules are reached via direct imports. (Note: `pi-plugin/src/dreamer/index.ts` is a DIFFERENT file — the pi-plugin's own dreamer barrel, which IS live.) |
| **Evidence** | Precise search for `from .../features/magic-context/dreamer` (no trailing slash) returns zero matches. |
| **Disposition** | **Delete.** Dead barrel. |

#### 9. `packages/plugin/src/features/magic-context/smart-notes/index.ts` — 9 lines (barrel)

| Field | Value |
|---|---|
| **What it was** | Barrel re-exporting 9 smart-notes submodules (capabilities, compiler, compiler-prompt, runner, sandbox-runner, schedule, ssrf-guard, storage, types). |
| **Last commit** | `34f5ef74` 2026-06-22 |
| **Confidence** | HIGH — zero importers. Only re-exported by the dead `features/magic-context/index.ts` barrel (which is itself unreachable). All smart-notes submodules are reached via direct imports. |
| **Evidence** | Precise search for `from .../smart-notes` (no trailing slash) returns zero matches outside the dead `features/magic-context/index.ts`. |
| **Disposition** | **Delete.** Dead barrel. |

#### 10. `packages/plugin/src/features/magic-context/sidekick/index.ts` — 2 lines (barrel)

| Field | Value |
|---|---|
| **What it was** | Barrel re-exporting `SidekickConfig` type and `runSidekick` / `SIDEKICK_SYSTEM_PROMPT` from `./agent`. |
| **Last commit** | `2c6c73c8` 2026-03-30 |
| **Confidence** | HIGH — zero importers. Only re-exported by the dead `features/magic-context/index.ts` barrel. `sidekick/agent.ts` is reached directly by `plugin/src/index.ts` (`SIDEKICK_SYSTEM_PROMPT`). |
| **Evidence** | Precise search for `from .../sidekick` (no trailing slash) returns zero matches outside the dead barrels. |
| **Disposition** | **Delete.** Dead barrel. |

#### 11. `packages/plugin/src/tools/index.ts` — 5 lines (barrel)

| Field | Value |
|---|---|
| **What it was** | Barrel re-exporting 5 tool modules (ctx-expand, ctx-memory, ctx-note, ctx-reduce, ctx-search). |
| **Last commit** | `2c6c73c8` 2026-03-30 |
| **Confidence** | HIGH — zero importers. `plugin/tool-registry.ts` imports each tool module directly (`./ctx-expand`, `./ctx-memory`, etc.), not through this barrel. |
| **Evidence** | Precise search for `from .../tools` (no trailing slash, in plugin/src) returns zero matches outside the individual tool `index.ts` files (which re-export `./tools`, a different path). |
| **Disposition** | **Delete.** Dead barrel — tool-registry imports tools directly. |

#### 12. `packages/plugin/src/config/schema.ts` — 2 lines (barrel)

| Field | Value |
|---|---|
| **What it was** | Barrel re-exporting `schema/agent-overrides` and `schema/magic-context`. |
| **Last commit** | `2c6c73c8` 2026-03-30 |
| **Confidence** | HIGH — zero importers. All consumers import `config/schema/magic-context` directly (the actual schema file), not this barrel. |
| **Evidence** | Precise search for `from .../config/schema` (no trailing slash) returns zero matches. |
| **Disposition** | **Delete.** Dead barrel. |

#### 13. `packages/plugin/src/features/magic-context/mock-database.ts` — 5 lines

| Field | Value |
|---|---|
| **What it was** | Test helper — `toDatabase<T>(db: T): Database` cast function. |
| **Last commit** | `3b1179e2` 2026-04-25 |
| **Confidence** | HIGH — zero importers anywhere (no production, no tests). |
| **Evidence** | Full-text search for `mock-database` returns zero matches outside the file. |
| **Disposition** | **Delete.** Unused test helper. |

#### 14. `packages/pi-plugin/src/storage.ts` — 1 line (barrel)

| Field | Value |
|---|---|
| **What it was** | Single-line re-export: `export * from "@magic-context/core/features/magic-context/storage"`. |
| **Last commit** | `0f883435` 2026-05-28 |
| **Confidence** | HIGH — zero importers. No pi-plugin file imports `./storage`. |
| **Evidence** | Full-text search for `from './storage'` in pi-plugin/src returns zero matches. |
| **Disposition** | **Delete.** Dead re-export. |

**Module-level total: 14 files, 977 lines of dead code.**

---

### FUNCTION-LEVEL DEAD CODE (in live modules)

These are individual exported functions/constants in reachable modules that have zero production callers. Each was hand-verified: full-text search confirmed no call sites outside the defining file (excluding test files). These are NOT module-level dead — the module is live, but these specific exports are dead.

#### High-confidence dead functions

| # | File | Symbol | Lines (approx) | Last commit | Evidence | Disposition |
|---|---|---|---|---|---|---|
| 1 | `pi-plugin/src/inject-compartments-pi.ts` | `injectSessionHistoryIntoPi` | ~85 (2673–2758) | `44dc65ab` 2026-07-24 | Only defined (line 2673), never called. Referenced only in comments in `context-handler.ts`. | **Delete** — dead exported function in a 2758-line live module |
| 2 | `pi-plugin/src/commands/ctx-status.ts` | `formatCtxStatusSummary` | ~15 | `4ea539b3` 2026-07-07 | Zero importers anywhere (no production, no tests). | **Delete** |
| 3 | `plugin/src/features/magic-context/git-commits/storage-git-commits.ts` | `upsertCommit` (singular) | ~12 | `ab5994e8` 2026-06-04 | Only re-exported by `git-commits/index.ts` barrel; the indexer uses `upsertCommits` (plural). The singular variant has no caller. | **Delete** (or merge into `upsertCommits` if it was meant as a single-row convenience) |
| 4 | `plugin/src/features/magic-context/git-commits/storage-git-commits.ts` | `evictOldestCommits` | ~10 | `ab5994e8` 2026-06-04 | Only re-exported by barrel; no caller. | **Delete** |
| 5 | `plugin/src/features/magic-context/git-commits/storage-git-commits.ts` | `getCommitBySha` | ~8 | `ab5994e8` 2026-06-04 | Only re-exported by barrel; no caller. | **Delete** |
| 6 | `plugin/src/features/magic-context/git-commits/storage-git-commit-embeddings.ts` | `clearProjectCommitEmbeddings` | ~8 | `7b96265c` 2026-06-24 | Zero callers. | **Delete** |
| 7 | `plugin/src/features/magic-context/git-commits/storage-git-commit-embeddings.ts` | `getDistinctCommitEmbeddingModelIds` | ~6 | `7b96265c` 2026-06-24 | Zero callers. | **Delete** |
| 8 | `plugin/src/features/magic-context/git-commits/indexer.ts` | `_resetIndexerGuards` | ~5 | `07e13e88` 2026-07-18 | Test-only (`_reset` prefix = test seam pattern). | **Keep** (test seam, excluded) |
| 9 | `plugin/src/features/magic-context/compartment-chunk-embedding.ts` | `getDistinctChunkEmbeddingModelIds` | ~6 | `64341b29` 2026-07-24 | Zero callers. | **Delete** |
| 10 | `plugin/src/features/magic-context/compartment-chunk-embedding.ts` | `clearChunkEmbeddingsForProject` | ~8 | `64341b29` 2026-07-24 | Zero callers. | **Delete** |
| 11 | `plugin/src/features/magic-context/project-embedding-registry.ts` | `attachShadowQueueDatabase` | ~15 | `7a1a0f23` 2026-07-25 | Zero callers. | **Delete** |
| 12 | `plugin/src/features/magic-context/project-embedding-registry.ts` | `detachShadowQueueDatabase` | ~10 | `7a1a0f23` 2026-07-25 | Zero callers. | **Delete** |
| 13 | `plugin/src/features/magic-context/project-embedding-registry.ts` | `unregisterProjectEmbedding` | ~8 | `7a1a0f23` 2026-07-25 | Imported by `memory/embedding.ts` but never called (only the import binding exists). | **Suspicious but unproven** — verify the call site in `embedding.ts` is not behind a conditional/dynamic dispatch before deleting |
| 14 | `plugin/src/features/magic-context/smart-notes/capabilities.ts` | `capabilitySecurityError` | ~5 | `3b9f6ec7` 2026-07-14 | Zero callers. | **Delete** |
| 15 | `plugin/src/features/magic-context/smart-notes/compiler.ts` | `logSmartNoteCompilerFailure` | ~8 | `3b9f6ec7` 2026-07-14 | Zero callers. | **Delete** |
| 16 | `plugin/src/features/magic-context/smart-notes/types.ts` | `SmartNoteCheckRow` | ~3 (type) | `664d147d` 2026-07-07 | Type-only export, zero type-position consumers. | **Delete** |
| 17 | `plugin/src/features/magic-context/storage-meta-persisted.ts` | `setPersistedDeliveredNoteNudge` | ~8 | `ba53c7ee` 2026-07-24 | Zero callers. | **Delete** |
| 18 | `plugin/src/features/magic-context/user-memory/storage-user-memory.ts` | `getAllUserMemories` | ~8 | `73a1655f` 2026-06-22 | Zero callers. | **Delete** |
| 19 | `plugin/src/features/magic-context/user-memory/storage-user-memory.ts` | `deleteUserMemory` | ~6 | `73a1655f` 2026-06-22 | Zero callers. | **Delete** |
| 20 | `plugin/src/hooks/auto-update-checker/checker.ts` | `updatePinnedVersion` | ~10 | `c0f032e2` 2026-07-22 | Zero callers. | **Delete** |
| 21 | `plugin/src/hooks/auto-update-checker/index.ts` | `getAutoUpdateInstallDir` | ~5 | `b711e45b` 2026-07-22 | Zero callers. | **Delete** |
| 22 | `plugin/src/hooks/auto-update-checker/types.ts` | `NpmPackageEnvelope`, `OpencodeConfig`, `PackageJson` | ~15 (types) | `664395fa` 2026-05-06 | Type-only exports, zero type-position consumers. | **Delete** |
| 23 | `plugin/src/hooks/magic-context/cache-busting-signals.ts` | `createMaterializationPassSignals` | ~10 | `7916d0b6` 2026-05-28 | Zero callers. | **Delete** |
| 24 | `plugin/src/hooks/magic-context/compartment-runner-incremental.ts` | `clearHistorianAlertState` | ~5 | `dd52281c` 2026-07-24 | Zero production callers (test-only via `__test` seam in a different module). | **Suspicious but unproven** — may be called via the `__test` reset pattern; verify before deleting |
| 25 | `plugin/src/hooks/magic-context/compartment-runner-mapping.ts` | `mapParsedCompartmentsToSession` | ~15 | `c99ae878` 2026-05-31 | Zero callers. | **Delete** |
| 26 | `plugin/src/features/magic-context/memory/embedding.ts` | `disposeEmbeddingModel` | ~8 | `3487e223` 2026-07-24 | Zero callers. | **Delete** |
| 27 | `plugin/src/features/magic-context/memory/memory-migration.ts` | `loadMemoriesForMigration` | ~10 | `cf08fc5c` 2026-06-25 | Zero callers. | **Delete** |
| 28 | `plugin/src/features/magic-context/memory/storage-memory-embeddings.ts` | `getDistinctStoredModelIds` | ~6 | `f9f19b5d` 2026-07-07 | Zero callers. | **Delete** |
| 29 | `plugin/src/features/magic-context/memory/storage-memory.ts` | `WorkspaceMemorySharingFilter` | ~5 (type) | `1c381b08` 2026-07-24 | Type-only export, zero type-position consumers. | **Delete** |
| 30 | `plugin/src/features/magic-context/memory/verification-paths.ts` | `gitCommitExists` | ~8 | `ea981bd0` 2026-07-07 | Zero callers. | **Delete** |
| 31 | `plugin/src/features/magic-context/mural/render-mural.ts` | `MURAL_FONT`, `MURAL_LINE_CAPACITY`, `muralImageTokenEstimate` | ~10 | `be8e624c` 2026-07-23 | `MURAL_FONT` and `MURAL_LINE_CAPACITY` are exported but only used internally within `render-mural.ts` (not imported by any other file). `muralImageTokenEstimate` is used by `scripts/test-mural-render.ts` (script-only). | **Suspicious but unproven** — `MURAL_FONT`/`MURAL_LINE_CAPACITY` may be intended as public API for the mural subsystem; verify intent before removing the export (the constants themselves are live internally) |
| 32 | `plugin/src/features/magic-context/mural/storage-mural.ts` | `muralDataUrl` | ~5 | `87662172` 2026-07-21 | Zero callers. | **Delete** |
| 33 | `plugin/src/features/magic-context/dreamer/task-registry.ts` | `AgenticDreamTask` | ~3 (type) | `ec044346` 2026-07-22 | Type-only export, zero type-position consumers. | **Delete** |
| 34 | `plugin/src/config/schema/magic-context.ts` | `DREAMER_TASKS`, `DreamingTaskSchema` | ~8 | `8dba1575` 2026-07-23 | Zero callers. | **Delete** |

**Function-level total: ~34 symbols, ~250 lines of dead exported code across live modules.**

---

### SUSPICIOUS BUT UNPROVEN

Items that look dead but where I cannot prove unreachability from a production entrypoint with full confidence. Per the conservative mandate, these go here rather than in the confirmed-dead list.

| File | Symbol | Why suspicious | Why unproven |
|---|---|---|---|
| `project-embedding-registry.ts` | `unregisterProjectEmbedding` | Imported by `memory/embedding.ts` but the import binding may never be called | The import exists; need to trace whether the call is behind a conditional or dynamic dispatch that my static analysis missed |
| `compartment-runner-incremental.ts` | `clearHistorianAlertState` | Zero production callers; test-only usage via `__test` seam | May be invoked through the `__test.reset()` pattern which my analysis treats as test-only but could be wired to a production cleanup path |
| `mural/render-mural.ts` | `MURAL_FONT`, `MURAL_LINE_CAPACITY` | Exported constants with no external importers; only used internally | May be intended as public API for the mural subsystem (the mural was rendered yesterday — live); removing the export is safe but removing the constant is not |
| `pi-plugin/src/context-perf-hooks.ts` | `setPiTransformTimingObserver` | Only used by `scripts/experiments/perf/run.ts` (experiment script, not production) | Scripts are not production entrypoints, but the function may be intended for future production wiring |

---

### RUST CRATES — NO DEAD MODULES

**mc-module** (30 `.rs` files): All `pub mod` declarations in `lib.rs` are reached from production code. Modules with no `use` statement in `lib.rs` (caveman, codec, compartment_coverage, decay_render, divergence, historian_prompt, historian_validate, injection, m0_compose, m1_compose, memory_tool, project_docs) are all consumed via fully-qualified paths (`crate::mod::...`) in other non-test source files (transform.rs, m0_compose.rs, m1_compose.rs, historian_chunk.rs, codec/, etc.). No dead modules.

**mc-core** (2 `.rs` files): `lib.rs` + `decay.rs`. `pub mod decay` is used via `crate::decay::...` in lib.rs. No dead modules.

**mc-store** (1 `.rs` file): Single `lib.rs` monolith. No dead modules.

**Drive-fault feature module:** All fault-injection code lives under `#[cfg(feature = "drive-fault")]` in `lib.rs` (lines 9161–9230+). Default OFF — not dead, intentionally feature-gated. Excluded per instructions.

---

## Summary

| Category | Count | Lines |
|---|---|---|
| Module-level dead (confirmed) | 14 files | 977 |
| Function-level dead (confirmed, in live modules) | ~31 symbols | ~250 |
| Suspicious but unproven | 4 items | ~30 |
| Rust dead modules | 0 | 0 |
| **Total confirmed dead** | **14 files + ~31 functions** | **~1,227 lines** |

### Top recommendations (by impact)

1. **Delete `transcript-opencode.ts`** (302 lines) — largest single dead file, superseded by `shared/transcript.ts`
2. **Delete `plugin-messages.ts`** (237 lines) — entire dead message-bus subsystem, superseded by RPC layer
3. **Delete 7 dead barrels** (36 lines total) — `config/schema.ts`, `features/magic-context/index.ts`, `dreamer/index.ts`, `sidekick/index.ts`, `smart-notes/index.ts`, `tools/index.ts`, `pi-plugin/storage.ts` — all re-export hubs with zero consumers
4. **Delete `opencode-compaction-detector.ts`** (81 lines) — logic extracted to `conflict-detector.ts`, this is the abandoned original
5. **Delete `compartment-runner-state-xml.ts`** (83 lines) — orphaned extraction, XML state construction moved elsewhere
6. **Wire-review `project-embedding-registry.ts`** — 3 dead exported functions (`attachShadowQueueDatabase`, `detachShadowQueueDatabase`, `unregisterProjectEmbedding`) suggest a shadow-queue feature that was partially implemented then abandoned

### Detector validation

The detector was validated against 6 known-live modules (mural renderer, module-transport/shadow-sender, decay-render, compartment-render-epoch, stop-reason surface, drive-fault). All passed — no false negatives on the validation set. The detector correctly handles: re-export chains, path aliases (`@magic-context/core/*`, `@magic-context/pi-core/*`), dynamic `import()` sites, and cfg-gated Rust code.