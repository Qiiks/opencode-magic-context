# W5-J — CLI hardening batch (v0.32.1), remaining findings

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration, packages/cli/src/. The 2 Highs (H1 parse-error-overwrite, H3 schema-fence) already merged — build on top; reuse the new lib/database-access.ts (openExistingDatabase / openExistingContextDatabase) and lib/jsonc-config.ts (tri-state parse) where relevant. Verify each finding at source before fixing. All in packages/cli — disjoint from the plugin.

M1 — harness routing ignores invalid overrides + blocks OpenCode no-host path (lib/harness-select.ts:14-19,40-50; commands/setup.ts:33-45; setup-opencode.ts:249-260): `--harness opencdoe` (typo) silently becomes "no flag"; `setup --harness opencode` on a no-OpenCode host is skipped so the per-harness "continue anyway?" path is unreachable; next-steps print even on nonzero dispatch. FIX: distinguish absent/valid/invalid harness flags, reject invalid; let explicit OpenCode setup reach its own no-host flow; print next steps only after success.

M2 — both-harness selection runs two wizards over one shared config (setup.ts:23-46; harness-select.ts:72-83; setup-opencode.ts:389-405; setup-pi.ts:357-370): choosing both runs OpenCode then Pi wizards, Pi overwrites shared historian/dreamer/sidekick values, invalidating the first summary + possibly selecting a model OpenCode can't use. FIX: run ONE shared Magic Context config phase then per-harness registration only; at minimum do not default to multiple harnesses while both full wizards write the same file.

M3 — fresh OpenCode setup creates opencode.json not .jsonc (lib/paths.ts:65-75): detectConfigPaths() picks opencode.json when neither exists, violating the fresh-install .jsonc invariant (#176). FIX: select jsoncPath when neither file exists; keep precedence for existing .jsonc/.json.

M4 — doctor reports malformed/unwritable TUI config as healthy (doctor-opencode.ts:1002-1058; callee shared/tui-config.ts:82-139): ensureTuiPluginEntry() catches + returns false, the parse-catch records "PASS TUI sidebar plugin configured". FIX: return a discriminated added|present|error result (or re-read+verify); parse/write errors must be FAIL not PASS.

M5 — stock binary probes test existence not runnability; Pi bypasses env-first HOME (lib/opencode-detect.ts:95-103; lib/pi-helpers.ts:14-26; doctor-opencode.ts:593-613; doctor-pi.ts:425-443): a non-executable ~/.opencode/bin/opencode or ~/.pi/bin/pi shadows a valid PATH binary → false "installed"; Pi fallback uses homedir() not env-first HOME. FIX: reuse executable validation for every fs candidate, keep searching after an unusable one, a failed CLI invocation is a doctor FAIL, resolve Pi fallback home env-first (process.env.HOME || homedir()).

M6 — OpenCode dev-path matcher accepts unrelated "magic-context" plugins (adapters/opencode.ts:252-260): `file:///tmp/magic-context-theme` is treated as our dev entry → setup suppresses the real entry, doctor reports registered. FIX: resolve the path + verify package.json name exactly (or strict recognized checkout/package basenames); unverifiable path warns, not counts-as-installed.

M7 — Pi package normalization disagrees across adapter/setup/doctor (adapters/pi.ts:191-215; lib/pi-package-entry.ts:20-35; setup-pi.ts:118-127; doctor-pi.ts:717-725): a source-only object entry (`{source:"npm:@cortexkit/pi-magic-context@0.31.5"}`) is accepted by the adapter but not recognized by setup (appends a duplicate) or doctor (reports no conflict). FIX: one canonical matcher everywhere, recursively recognizing source+name, no substring matches.

M8 — missing Pi registration only a warning, exits 0 (doctor-pi.ts:467-484,1039-1040): packages:[] → "extension missing" but "Doctor complete" exit 0. FIX: classify missing package registration as FAIL; under --force repair + rerun + return 0 only after registration verifies.

M9 — Pi cache/version checks ignore installed plugin + --force (doctor-pi.ts:251-269,349-405,737-747): cache matching CLI version untouched by --force (hardcoded force=false); older pinned CLI vs newer cache falsely "stale" (compares CLI self not npm/latest); managed roots (agent/npm/node_modules, project .pi/npm/node_modules) absent from scanning. FIX: inspect the actual installed package / pi list, compare vs configured spec + npm state, include managed roots, pass the real force flag.

M10 — Pi table parsing drops valid model IDs + accepts prose (lib/pi-helpers.ts:67-114): a row `openrouter anthropic/claude-sonnet-4 ...` is dropped (model column forbids `/`); a heading `Available models:` becomes fake model `Available/models:`. FIX: parse only rows beneath a recognized header, validate expected metadata columns, allow provider-qualified model IDs in column two.

M11 — ONNX runtime probe false-failure + false-PASS (lib/embedding-runtime.ts:37-74,252-270; doctor-opencode.ts:363-370; doctor-pi.ts:685-710): invoking with an absolute Node while node is absent from PATH → false broken-runtime; removing all inspectable plugin trees → probe returns unknown but doctors report local embedding PASS. FIX: spawn process.execPath, preserve tri-state; only state:"ok" → PASS; unknown → unverified INFO/WARN.

M12 — migration path derivation not XDG/Windows correct (migrate.ts:171-180,191-193; migrate-session.ts:401-405): XDG_DATA_HOME ignored (uses ~/.local/share); Windows projectPathToPiDirSlug keeps backslashes + `:`. FIX: share one canonical OpenCode-DB resolver with diagnostics/runtime; share Pi's real platform-specific session-slug algorithm.

M13 — live OpenCode migration reads no consistent snapshot (migrate.ts:569-598): separate autocommit reads (session/count/message/part) can observe a message before its parts. FIX: wrap all source reads in ONE deferred read transaction + busy_timeout; optionally detect source-session changed → ask retry.

M14 — migrate-session safety advisory + DB-open failures escape handling (migrate-session.ts:506-519,591-607; index.ts:127): constructors can throw before the command catch (top-level promise has no rejection handler → unhandled stack); --yes bypasses the only live-OpenCode stop; only context.db gets busy_timeout; raw main-file backup can omit WAL. FIX: wrap nullable DB acquisition in the command error boundary (already partly done via database-access.ts — verify), require existing source DBs, close partial handles; before mutation checkpoint/lock both DBs + use SQLite backup API not main-file copy.

L1 — exact @latest tuple "upgraded" every doctor run (doctor-opencode.ts:922-965): a `[["@cortexkit/opencode-magic-context@latest",{...}]]` tuple fails direct array-to-string compare → doctor rewrites the same tuple + requests restart every run. FIX: compare entryAsString(entry) with the desired specifier before upgrade logic.

## Gates
packages/cli: bun test (full), typecheck, lint, build, check_comments. Comments explain invariants, no audit refs. This is a large batch — if any single finding balloons, land the rest and report the deferred one with reason rather than blocking the batch. Report per-finding status + test evidence.
