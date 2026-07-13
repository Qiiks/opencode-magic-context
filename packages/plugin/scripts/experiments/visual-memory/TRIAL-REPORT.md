# Palace Authoring Trial

Run: 2026-07-13T19:17:57.706Z

This is a non-agentic, single-call-per-category authoring trial. JSON parsing is fail-closed: no fence stripping, substring extraction, or partial manifest application. Each generated entry is overlaid onto the frontier manifest and checked with the exported `author-palace.ts` validator; the full generated category is also checked in one combined manifest. The acceptance gate requires complete JSON, 100% coverage, no validator-class failures, exact importance passthrough, and at least 85% exact-anchor fidelity.

# DS4F results

## ds4f: PROJECT_RULES

- Model: `deepseek/deepseek-v4-flash`
- Raw completion: `/tmp/visual-memory/trial-ds4f-PROJECT_RULES.json`
- Calls: 2; parse retry: attempted but did not recover
- First parse rejection: JSON parse failed: JSON Parse error: Unrecognized token '\`'
- Fail-closed parse rejection: JSON parse failed: JSON Parse error: Unrecognized token '\`'
- Coverage: not measured because the complete JSON root was rejected.
- Hard validator failures: not measured because validation never receives a partial manifest.
- Anchor fidelity: not measured because validation never receives a partial manifest.
- Room quality and side-by-side cues: not measured because validation never receives a partial manifest.

## ds4f: ARCHITECTURE

- Model: `deepseek/deepseek-v4-flash`
- Raw completion: `/tmp/visual-memory/trial-ds4f-ARCHITECTURE.json`
- Calls: 2; parse retry: attempted but did not recover
- First parse rejection: JSON parse failed: JSON Parse error: Unrecognized token '\`'
- Fail-closed parse rejection: JSON parse failed: JSON Parse error: Unrecognized token '\`'
- Coverage: not measured because the complete JSON root was rejected.
- Hard validator failures: not measured because validation never receives a partial manifest.
- Anchor fidelity: not measured because validation never receives a partial manifest.
- Room quality and side-by-side cues: not measured because validation never receives a partial manifest.

# GPT-5.6-sol sanity baseline

## gpt-5.6-sol: PROJECT_RULES

- Model: `openai/gpt-5.6-sol`
- Raw completion: `/tmp/visual-memory/trial-gpt-5.6-sol-PROJECT_RULES.json`
- Calls: 1; parse retry: not needed
- Coverage: **83/83**; uncovered: none
- Anchor fidelity: **70.8%** (206/291 source exact tokens retained in the effective cue)
- Importance passthrough: **83/83**; mismatches: none
- Full-manifest validator: passed

### Hard validator failures

- **missing polarity mechanism:** 0
- **hub noun repetition:** 0
- **memory ID leakage:** 0
- **broken exact anchors:** 0
- **unbalanced parentheses:** 0
- **other validator failures:** 0

### Room quality

- gpt-5.6-sol rooms (55): Bash surface, Bash tests, Benchmarks, Bug fixes, Cargo, Code Health, Commit trailers, Compression, Design decisions, Dev builds, Diagnostics, Documentation, Fixtures, Formatter tests, Git log, GitHub CLI, License, Load rigs, Lockfile, Masons, Memory cues, Merge waves, Mutations, Oracle loop, Parity fixtures, Plugin builds, Plugin config, Plugin gates, Plugin tests, Pre-push gate, Project roots, Prompt injections, Pull requests, Release, Release notes, Rollback, Run watchers, Semantic search, SQLite, State storage, Statuses, Storm tests, Subc protocol, Subc tests, Task dispatch, Task lifecycle, Test gates, Tool manifest, Tool schemas, Tool surface, TUI package, Watcher tests, Windows tests, Workspace packages, Worktrees
- Frontier rooms (7): E2E, Engineering policy, Inspect & tools, Plugin, Release & CI, Tests, Worktrees

### Six evenly spaced cue comparisons

#### #7863

Frontier cue:
```json
["shortcircuit: empty→empty; ⊘fabrication (filter summarizes only actual bytes)","guard compress_filters_test.rs + builtin_filters_never_fabricate_output_for_empty_input"]
```

gpt-5.6-sol cue:
```json
"shortcircuit replacement → summarize contained output only; EMPTY → \"\"; ⊘fabricated text (misreports successful empty probes); `compress_filters_test.rs::builtin_filters_never_fabricate_output_for_empty_input` ∀ builtin filters"
```

#### #5550

Frontier cue:
```json
"Unix APIs e.g. PermissionsExt→#[cfg(unix)] (Windows compile)"
```

gpt-5.6-sol cue:
```json
"Unix-only APIs such as `PermissionsExt` → `#[cfg(unix)]` ∵ Windows CI compilation"
```

#### #7293

Frontier cue:
```json
"format subsystem tests opt in format_on_edit:true"
```

gpt-5.6-sol cue:
```json
"formatting integration tests → explicit `format_on_edit: true`, e.g. `configure_format_on_edit`"
```

#### #8199

Frontier cue:
```json
"A/B vs mason tree >30min→dedicated checkout (reclaimer may collect)"
```

gpt-5.6-sol cue:
```json
"A/B against mason tree older than 30 minutes → dedicated checkout; ⊘ephemeral old tree (reclamation timer collection)"
```

#### #5297

Frontier cue:
```json
"path params template=`(absolute or relative to project root)`"
```

gpt-5.6-sol cue:
```json
"file/directory/path parameter template exactly `(absolute or relative to project root)`"
```

#### #8255

Frontier cue:
```json
{"mergeInto":8391}
```

gpt-5.6-sol cue:
```json
"TypeScript changes → `bun lint` ≺ push ∵ CI lint failures"
```

# Verdict

**SHIP-WITH-STRONGER-MODEL-RECOMMENDED.** DS4F missed the acceptance gate because PROJECT_RULES: fail-closed parse rejection; ARCHITECTURE: fail-closed parse rejection. The GPT-5.6-sol sanity baseline completed and is included above for comparison.

# Round 2

Run: 2026-07-13T20:09:21.493Z

This round is a non-agentic, single-call-per-category authoring trial across the requested OpenRouter model matrix. XML parsing is fail-closed: the reply must be one complete `<palace>` root beginning with `<palace` and ending with `</palace>`; the harness never strips fences, extracts a substring, or applies a partial manifest. A parse rejection receives one error-fed retry. Parsed XML entries are converted to the existing `SpecEntry` shape and checked by the unchanged `author-palace.ts` validator. The strict safety gate requires complete XML, 100% coverage, no validator-class failures, and exact importance passthrough; cue quality also targets at least 85% anchor fidelity and the 4–8-room, three-entries-per-room budget.

## Matrix

Parse is `pass`, `retry-recovered`, or `fail`; failure counts are ordered as the named validator classes.

| Model | Category | Parse | Coverage | Validator failure classes (counts) | Anchor fidelity | Rooms (frontier target) |
| --- | --- | --- | --- | --- | --- | --- |
| DeepSeek V4 Flash (deepseek/deepseek-v4-flash) | PROJECT_RULES | retry-recovered | 55/83 | missing-polarity-mechanism=0, hub-noun-repetition=0, memory-ID-leakage=0, broken-exact-anchors=0, unbalanced-parentheses=0, other-validator-failures=1 | 38.5% | 8/7 |
| DeepSeek V4 Flash (deepseek/deepseek-v4-flash) | ARCHITECTURE | pass | 122/123 | missing-polarity-mechanism=77, hub-noun-repetition=0, memory-ID-leakage=1, broken-exact-anchors=0, unbalanced-parentheses=0, other-validator-failures=0 | 69.2% | 5/16 |
| DeepSeek V4 Pro (deepseek/deepseek-v4-pro) | PROJECT_RULES | pass | 83/83 | missing-polarity-mechanism=21, hub-noun-repetition=0, memory-ID-leakage=0, broken-exact-anchors=0, unbalanced-parentheses=0, other-validator-failures=0 | 58.1% | 20/7 |
| DeepSeek V4 Pro (deepseek/deepseek-v4-pro) | ARCHITECTURE | pass | 122/123 | missing-polarity-mechanism=33, hub-noun-repetition=0, memory-ID-leakage=1, broken-exact-anchors=0, unbalanced-parentheses=0, other-validator-failures=2 | 69.2% | 51/16 |
| Kimi K2 (moonshotai/kimi-k2) | PROJECT_RULES | retry-recovered | 82/83 | missing-polarity-mechanism=43, hub-noun-repetition=0, memory-ID-leakage=0, broken-exact-anchors=0, unbalanced-parentheses=1, other-validator-failures=0 | 71.8% | 8/7 |
| Kimi K2 (moonshotai/kimi-k2) | ARCHITECTURE | pass | 72/123 | missing-polarity-mechanism=26, hub-noun-repetition=0, memory-ID-leakage=0, broken-exact-anchors=0, unbalanced-parentheses=0, other-validator-failures=1 | 23.7% | 9/16 |
| Gemini 3.5 Flash (google/gemini-3.5-flash) | PROJECT_RULES | pass | 83/83 | missing-polarity-mechanism=9, hub-noun-repetition=0, memory-ID-leakage=0, broken-exact-anchors=0, unbalanced-parentheses=0, other-validator-failures=0 | 62.5% | 8/7 |
| Gemini 3.5 Flash (google/gemini-3.5-flash) | ARCHITECTURE | fail | — | — | — | — |

## Cost per model run

Estimates use the OpenRouter model-catalog input/output price at run time and the API-reported tokens for all completed calls, including parse retries. Cache, tool, and provider-specific surcharges are not included.

| Model | Input / output price | Reported usage | Estimated run cost | Pricing note |
| --- | --- | --- | --- | --- |
| DeepSeek V4 Flash (deepseek/deepseek-v4-flash) | $0.090/M / $0.180/M | 3 response(s); 19853 input + 13731 output | $0.00426 | OpenRouter model-catalog input/output prices retrieved at run time. |
| DeepSeek V4 Pro (deepseek/deepseek-v4-pro) | $0.435/M / $0.870/M | 2 response(s); 13570 input + 10882 output | $0.015 | OpenRouter model-catalog input/output prices retrieved at run time. |
| Kimi K2 (moonshotai/kimi-k2) | $0.570/M / $2.30/M | 3 response(s); 19352 input + 10516 output | $0.035 | OpenRouter model-catalog input/output prices retrieved at run time. |
| Gemini 3.5 Flash (google/gemini-3.5-flash) | $1.50/M / $9.00/M | 3 response(s); 22934 input + 14671 output | $0.166 | OpenRouter model-catalog input/output prices retrieved at run time. |

## Per-model verdicts

- **DeepSeek V4 Flash: NOT-VIABLE.** PROJECT_RULES: uncovered memories; PROJECT_RULES: other validator failures; PROJECT_RULES: importance passthrough mismatches; ARCHITECTURE: uncovered memories; ARCHITECTURE: missing polarity mechanism; ARCHITECTURE: memory ID leakage; ARCHITECTURE: importance passthrough mismatches
- **DeepSeek V4 Pro: NOT-VIABLE.** PROJECT_RULES: missing polarity mechanism; ARCHITECTURE: uncovered memories; ARCHITECTURE: missing polarity mechanism; ARCHITECTURE: memory ID leakage; ARCHITECTURE: other validator failures; ARCHITECTURE: importance passthrough mismatches
- **Kimi K2: NOT-VIABLE.** PROJECT_RULES: uncovered memories; PROJECT_RULES: missing polarity mechanism; PROJECT_RULES: unbalanced parentheses; PROJECT_RULES: importance passthrough mismatches; ARCHITECTURE: uncovered memories; ARCHITECTURE: missing polarity mechanism; ARCHITECTURE: other validator failures; ARCHITECTURE: importance passthrough mismatches
- **Gemini 3.5 Flash: NOT-VIABLE.** PROJECT_RULES: missing polarity mechanism; ARCHITECTURE: fail-closed parse rejection

## Cell details

### DeepSeek V4 Flash: PROJECT_RULES

- Model: `deepseek/deepseek-v4-flash`
- Raw completion: `/tmp/visual-memory/trial-DeepSeek V4 Flash-PROJECT_RULES.xml`
- Calls: 2; parse retry: recovered
- First parse rejection: XML parse failed: entry 7702 cue contains an unescaped or unknown XML entity
- Coverage: **55/83**; uncovered: 7383, 7087, 6896, 6556, 6438, 6150, 6093, 5269, 5216, 5210, 4773, 4761, 8201, 7991, 7916, 6394, 6369, 6112, 5266, 6161, 8913, 8912, 6504, 5297, 8612, 8440, 4922, 4791
- Anchor fidelity: **38.5%** (112/291 source exact tokens retained in the effective cue)
- Importance passthrough: **55/83**; mismatches: 7383, 7087, 6896, 6556, 6438, 6150, 6093, 5269, 5216, 5210, 4773, 4761, 8201, 7991, 7916, 6394, 6369, 6112, 5266, 6161, 8913, 8912, 6504, 5297, 8612, 8440, 4922, 4791
- Full-manifest validator: failed — uncovered source ids: 7383, 7087, 6896, 6556, 6438, 6150, 6093, 5269, 5216, 5210, 4773, 4761, 8201, 7991, 7916, 6394, 6369, 6112, 5266, 6161, 8913, 8912, 6504, 5297, 8612, 8440, 4922, 4791

#### Hard validator failures

- **missing polarity mechanism:** 0
- **hub noun repetition:** 0
- **memory ID leakage:** 0
- **broken exact anchors:** 0
- **unbalanced parentheses:** 0
- **other validator failures:** 1
  - manifest: uncovered source ids: 7383, 7087, 6896, 6556, 6438, 6150, 6093, 5269, 5216, 5210, 4773, 4761, 8201, 7991, 7916, 6394, 6369, 6112, 5266, 6161, 8913, 8912, 6504, 5297, 8612, 8440, 4922, 4791

#### Room quality

- DeepSeek V4 Flash rooms (8): Benchmark harness (8), CI workflows (8), Compression filters (3), Git conventions (9), Integration tests (9), Mason protocol (5), Plugin builds (8), Subc transport (5)
- Frontier spec rooms (7; Round-2 comparison target 7): E2E, Engineering policy, Inspect & tools, Plugin, Release & CI, Tests, Worktrees

#### Four evenly spaced cue comparisons

#### #7863

Frontier cue:
```json
["shortcircuit: empty→empty; ⊘fabrication (filter summarizes only actual bytes)","guard compress_filters_test.rs + builtin_filters_never_fabricate_output_for_empty_input"]
```

DeepSeek V4 Flash cue:
```json
"⊘fabricate output (shortcircuit only SUMMARIZE actual content); empty→empty; kubectl.toml empty-match rendered \"kubectl: no resources found\" for empty exec probe; compress_filters_test.rs::builtin_filters_never_fabricate_output_for_empty_input"
```

#### #8201

Frontier cue:
```json
"client timeout→explicit bash_kill; ⊘orphan execution (executor starvation)"
```

DeepSeek V4 Flash cue:
```json
MISSING
```

#### #6366

Frontier cue:
```json
"prewarm indexes via `aft warmup` with identical prompts"
```

DeepSeek V4 Flash cue:
```json
"AFT benchmarks require pre-warmed search indexes via `aft warmup` with identical prompts"
```

#### #8255

Frontier cue:
```json
{"mergeInto":8391}
```

DeepSeek V4 Flash cue:
```json
"run `bun lint` before pushing TS changes ∵ CI lint failures"
```

### DeepSeek V4 Flash: ARCHITECTURE

- Model: `deepseek/deepseek-v4-flash`
- Raw completion: `/tmp/visual-memory/trial-DeepSeek V4 Flash-ARCHITECTURE.xml`
- Calls: 1; parse retry: not needed
- Coverage: **122/123**; uncovered: 8280
- Anchor fidelity: **69.2%** (222/321 source exact tokens retained in the effective cue)
- Importance passthrough: **122/123**; mismatches: 8280
- Full-manifest validator: failed — polarity mechanism must follow marker 5736: chunk reuse keyed by blake3(embed_text); verified exact-string comparison; ⊘redundant embeddings

#### Hard validator failures

- **missing polarity mechanism:** 77
  - #5736: polarity mechanism must follow marker 5736: chunk reuse keyed by blake3(embed_text); verified exact-string comparison; ⊘redundant embeddings
  - #7499: polarity mechanism missing from rendered cue 7499: ranking priors per-prior gating: P2 definition-gated exact-name boost on Identifier; P1/P4 NL-only; P3 qualified-name via V7; ⊘changing QueryKind routing
  - #7538: polarity mechanism missing from rendered cue 7538: Tier-2 cold-seed serialization gate cleared during retry-wait window when embedding backend down; ⊘code-health starvation
- **hub noun repetition:** 0
- **memory ID leakage:** 1
  - #8050: memory id leaked into cue 8050
- **broken exact anchors:** 0
- **unbalanced parentheses:** 0
- **other validator failures:** 0

#### Room quality

- DeepSeek V4 Flash rooms (5): AFT commands (17), Callgraph and dead code (38), Semantic index (13), Subc transport (33), Watcher and maintenance (21)
- Frontier spec rooms (10; Round-2 comparison target 16): Artifacts, Bash, Bridge, Embeddings, Inspect & LSP, Prompts, Subc, Synapse, TUI, Watcher & scheduler

#### Four evenly spaced cue comparisons

#### #7467

Frontier cue:
```json
["semantic extensions=every real tree-sitter symbol query; generic collect_file_chunks_from_source","⊘HTML/Markdown/JSON (query=None); sync guard semantic_extension_policy_stays_in_sync_with_parser_code_arms"]
```

DeepSeek V4 Flash cue:
```json
"is_semantic_indexed_extension covers EVERY language with real tree-sitter symbol query; chunker generic parse→extract_symbols_from_tree→symbols_to_chunks; dae42377 adds Java/Kotlin/Ruby/Swift/Scala/Lua/Perl/R(.r/.R); ⊘HTML/Markdown/JSON (query=None, document/data tree-walking); integration test semantic_extension_policy_stays_in_sync_with_parser_code_arms regression-guards"
```

#### #8298

Frontier cue:
```json
"health/root list try-lock only→busy/empty; ⊘blocking responder (try-lock)"
```

DeepSeek V4 Flash cue:
```json
"health.check+root-list assembly answered from try-lock-only state (reporting busy/empty lists on contended locks); ⊘blocking health responder"
```

#### #7528

Frontier cue:
```json
"active semantic build defers callgraph warm + Tier-2 scans (peak RSS)"
```

DeepSeek V4 Flash cue:
```json
"Tier-2 cold-seed serialization defers callgraph-warm builds+Tier-2 scans during active semantic builds; ⊘peak RSS"
```

#### #8601

Frontier cue:
```json
["artifact_cache_key root commit: aft 90ff783f3f4c5cf2; opencode aa69d52ef2dcad4d","alfonso be627d40119a995e; magic-context e274ab0872bb490b; subc 29f01b6c90df2c83; synapse 3e6b8ee50bb2bb06"]
```

DeepSeek V4 Flash cue:
```json
"artifact key ground truth (verified 2026-07-09): aft repo artifact_cache_key=90ff783f3f4c5cf2=sha256(\"3fba0e3dcc1cb26da9af7a0ccbd98b749e46219b\")[..16] (root commit); aa69d52ef2dcad4d=OpenCode OSS repo (~/Work/OSS/opencode), ⊘aft; other memoized keys: alfonso=be627d40119a995e, magic-context=e274ab0872bb490b, subconscious=29f01b6c90df2c83, synapse=3e6b8ee50bb2bb06, ai-proxy=866735efc3e32f4d; root→key memo at <storage>/cache-keys.json; path-identity key of aft repo path=6d75dd56448a4a9c (alfonso worktree-group dir hash)"
```

### DeepSeek V4 Pro: PROJECT_RULES

- Model: `deepseek/deepseek-v4-pro`
- Raw completion: `/tmp/visual-memory/trial-DeepSeek V4 Pro-PROJECT_RULES.xml`
- Calls: 1; parse retry: not needed
- Coverage: **83/83**; uncovered: none
- Anchor fidelity: **58.1%** (169/291 source exact tokens retained in the effective cue)
- Importance passthrough: **83/83**; mismatches: none
- Full-manifest validator: failed — polarity mechanism must follow marker 8354: release gates: packed-install visual/behavioral check (JSX reactivity, ⊘eager evaluation freezes)

#### Hard validator failures

- **missing polarity mechanism:** 21
  - #8354: polarity mechanism must follow marker 8354: release gates: packed-install visual/behavioral check (JSX reactivity, ⊘eager evaluation freezes)
  - #8198: polarity mechanism must follow marker 8198: Review worktrees default \`--detach\` (⊘block future mason branch rehydration)
  - #6979: polarity mechanism must follow marker 6979: commands serial (⊘build-lock/CPU contention on shared target dir)
- **hub noun repetition:** 0
- **memory ID leakage:** 0
- **broken exact anchors:** 0
- **unbalanced parentheses:** 0
- **other validator failures:** 0

#### Room quality

- DeepSeek V4 Pro rooms (20): Benchmarks (5), CI & release (7), Compression (3), Config & env (3), Design & process (3), Docs & release notes (5), Error handling (3), Git & commits (6), Integration tests (8), Latency & perf (2), Mason & workers (4), Misc (5), OpenCode plugin (2), Plugin build (7), Rust & Cargo (6), SQLite (2), Status & terminal (2), Subc (3), Testing (3), Tool schema (4)
- Frontier spec rooms (7; Round-2 comparison target 7): E2E, Engineering policy, Inspect & tools, Plugin, Release & CI, Tests, Worktrees

#### Four evenly spaced cue comparisons

#### #7863

Frontier cue:
```json
["shortcircuit: empty→empty; ⊘fabrication (filter summarizes only actual bytes)","guard compress_filters_test.rs + builtin_filters_never_fabricate_output_for_empty_input"]
```

DeepSeek V4 Pro cue:
```json
"⊘fabricate output (shortcircuit only SUMMARIZE real output; empty stays empty). Guard: compress_filters_test.rs::builtin_filters_never_fabricate_output_for_empty_input"
```

#### #8201

Frontier cue:
```json
"client timeout→explicit bash_kill; ⊘orphan execution (executor starvation)"
```

DeepSeek V4 Pro cue:
```json
"Client-side timeout → kill long-running task explicitly (`bash_kill`; ⊘executor starvation)"
```

#### #6366

Frontier cue:
```json
"prewarm indexes via `aft warmup` with identical prompts"
```

DeepSeek V4 Pro cue:
```json
"AFT benchmark runs: search indexes pre-warmed (`aft warmup` identical prompts)"
```

#### #8255

Frontier cue:
```json
{"mergeInto":8391}
```

DeepSeek V4 Pro cue:
```json
"Run `bun lint` before pushing TS changes"
```

### DeepSeek V4 Pro: ARCHITECTURE

- Model: `deepseek/deepseek-v4-pro`
- Raw completion: `/tmp/visual-memory/trial-DeepSeek V4 Pro-ARCHITECTURE.xml`
- Calls: 1; parse retry: not needed
- Coverage: **122/123**; uncovered: 8326
- Anchor fidelity: **69.2%** (222/321 source exact tokens retained in the effective cue)
- Importance passthrough: **122/123**; mismatches: 8326
- Full-manifest validator: failed — polarity mechanism missing from rendered cue 8615: actor rebind recovery: when bind fails/expires, next rebind recreates actor→⊘rejecting as already-pending or config-divergence

#### Hard validator failures

- **missing polarity mechanism:** 33
  - #8615: polarity mechanism missing from rendered cue 8615: actor rebind recovery: when bind fails/expires, next rebind recreates actor→⊘rejecting as already-pending or config-divergence
  - #7221: polarity mechanism missing from rendered cue 7221: subc_translate called from run_tool_call to translate arguments and select commands≺lane selection→⊘aliasing agent names into shared main::dispatch
  - #7220: polarity mechanism must follow marker 7220: storage segments sanitized (stripping \`/ \ :\`, collapsing to \`-\`, prefixing \`mcp--\`)→⊘directory traversal
- **hub noun repetition:** 0
- **memory ID leakage:** 1
  - #8050: memory id leaked into cue 8050
- **broken exact anchors:** 0
- **unbalanced parentheses:** 0
- **other validator failures:** 2
  - #8534: duplicate spec id 8534 (2 entries)
  - #7528: duplicate spec id 7528 (2 entries)

#### Room quality

- DeepSeek V4 Pro rooms (51): .h files (2), AFT bash (2), AFT test-support (2), AFT version (2), aft-tokenizer (2), AftRpcClient/AftRpcServer (2), apply_patch (2), ArtifactAccess (2), Background completions (2), Binds (2), Bridge (2), Cache-key identity (2), Callgraph (5), configure command (4), Cross-project Alfonso (2), Diagnostics (2), Fan-out (2), Federation (2), Framework entry-points (2), Health check (3), Image-resize (2), Inspect engine (2), is_subc_native_plumbing_tool (2), LSP clients (2), MCP client (2), Orchestration (2), Pi tool (2), Project-scoped artifacts (3), Prompt context (2), PTY kill (2), RawAftConfig (2), Reconfigure (3), Reliable writer (2), Route reopening (2), RouteBind (3), run_tool_call (4), Rust macro liveness (2), Scheduler (2), Search index (3), Semantic indexing (5), Subc executor (6), Subc mode (2), subc-core (5), Tier-2 cold-seed (2), Transactional rollback (2), TreeSitterProvider (2), tsconfig_membership (2), TUI TSX (2), Unix PATH (2), Unused exports (2), Watcher drains (2)
- Frontier spec rooms (10; Round-2 comparison target 16): Artifacts, Bash, Bridge, Embeddings, Inspect & LSP, Prompts, Subc, Synapse, TUI, Watcher & scheduler

#### Four evenly spaced cue comparisons

#### #7467

Frontier cue:
```json
["semantic extensions=every real tree-sitter symbol query; generic collect_file_chunks_from_source","⊘HTML/Markdown/JSON (query=None); sync guard semantic_extension_policy_stays_in_sync_with_parser_code_arms"]
```

DeepSeek V4 Pro cue:
```json
"is_semantic_indexed_extension covers every lang w/ real tree-sitter symbol query; chunker fully generic (parse → extract_symbols_from_tree → symbols_to_chunks, zero per-lang logic). dae42377 adds Java, Kotlin, Ruby, Swift, Scala, Lua, Perl, R (.r/.R). ⊘HTML/Markdown/JSON (query=None, document/data tree-walking). semantic_extension_policy_stays_in_sync_with_parser_code_arms regression-guards: adding lang w/ real extractor but forgetting allowlist fails CI"
```

#### #8298

Frontier cue:
```json
"health/root list try-lock only→busy/empty; ⊘blocking responder (try-lock)"
```

DeepSeek V4 Pro cue:
```json
"health.check and root-list assembly answered from try-lock-only state (reporting busy or returning empty lists on contended locks) → ⊘blocking health responder"
```

#### #7528

Frontier cue:
```json
"active semantic build defers callgraph warm + Tier-2 scans (peak RSS)"
```

DeepSeek V4 Pro cue:
```json
"Tier-2 cold-seed serialization defers callgraph-warm builds and Tier-2 scans during active semantic builds → reduce peak RSS"
```

#### #8601

Frontier cue:
```json
["artifact_cache_key root commit: aft 90ff783f3f4c5cf2; opencode aa69d52ef2dcad4d","alfonso be627d40119a995e; magic-context e274ab0872bb490b; subc 29f01b6c90df2c83; synapse 3e6b8ee50bb2bb06"]
```

DeepSeek V4 Pro cue:
```json
"artifact key ground truth (verified 2026-07-09): aft repo artifact_cache_key = 90ff783f3f4c5cf2 = sha256(\"3fba0e3dcc1cb26da9af7a0ccbd98b749e46219b\")[..16] (root commit); aa69d52ef2dcad4d is OpenCode OSS repo (~/Work/OSS/opencode), NOT aft. Other memoized keys: alfonso=be627d40119a995e, magic-context=e274ab0872bb490b, subconscious=29f01b6c90df2c83, synapse=3e6b8ee50bb2bb06, ai-proxy=866735efc3e32f4d. Root→key memo at <storage>/cache-keys.json. Path-identity key of aft repo path = 6d75dd56448a4a9c (used as alfonso worktree-group dir hash)"
```

### Kimi K2: PROJECT_RULES

- Model: `moonshotai/kimi-k2`
- Raw completion: `/tmp/visual-memory/trial-Kimi K2-PROJECT_RULES.xml`
- Calls: 2; parse retry: recovered
- First parse rejection: XML parse failed: entry 7702 cue contains an unescaped or unknown XML entity
- Coverage: **82/83**; uncovered: 6394
- Anchor fidelity: **71.8%** (209/291 source exact tokens retained in the effective cue)
- Importance passthrough: **82/83**; mismatches: 6394
- Full-manifest validator: failed — polarity mechanism missing from rendered cue 8201: kill long-running tasks via \`bash_kill\` on client timeout ⊘executor starvation

#### Hard validator failures

- **missing polarity mechanism:** 43
  - #8201: polarity mechanism missing from rendered cue 8201: kill long-running tasks via \`bash_kill\` on client timeout ⊘executor starvation
  - #8623: polarity mechanism must follow marker 8623: live OpenCode loads AFT plugin file:///Users/ufukaltinok/Work/Projects/CortexKit/aft/packages/opencode-plugin; packages/*/dist ARE production; TS merges must rebuild dists (aft-bridge, opencode-plugin, pi-plugin) as part of merge ⊘just release time; stale dist→merged features silently absent
  - #5422: polarity mechanism must follow marker 5422: ≻plugin \`index.ts\` imports, run \`bun run lint\` (biome organize-imports)≺release gate; per-package \`bun test\` ⊘biome
- **hub noun repetition:** 0
- **memory ID leakage:** 0
- **broken exact anchors:** 0
- **unbalanced parentheses:** 1
  - #7554: unbalanced mechanism in rendered cue 7554: watch CI/release: use durable scripts (\`scripts/watch-ci.sh\`, \`scripts/wait-release.sh\`) ⊘hand-roll \`nohup ... gh run view ... &\`; hand-roll footgun: \`gh run view --jq '\(.status)/\(.conclusion)'\` INVALID jq (bare \( outside string); ⊘\`({ background:true, command:"nohup  -c '...' &" })\` (self-detaches from AFT tracker, mcp_Bash_watch fires on launcher exit); use \`({ background:true, command:"<real cmd>" })\` directly
- **other validator failures:** 0

#### Room quality

- Kimi K2 rooms (8): Bash execution (5), Build artifacts (6), CI operations (9), Code health (22), Compression filters (3), Documentation (16), Plugin system (6), Test infrastructure (15)
- Frontier spec rooms (7; Round-2 comparison target 7): E2E, Engineering policy, Inspect & tools, Plugin, Release & CI, Tests, Worktrees

#### Four evenly spaced cue comparisons

#### #7863

Frontier cue:
```json
["shortcircuit: empty→empty; ⊘fabrication (filter summarizes only actual bytes)","guard compress_filters_test.rs + builtin_filters_never_fabricate_output_for_empty_input"]
```

Kimi K2 cue:
```json
"⊘fabricate output for empty (synthesize text); shortcircuit only SUMMARIZE actual content; empty stays empty; field: kubectl.toml empty-match → \"kubectl: no resources found\" on kubectl exec probe; guard: compress_filters_test.rs::builtin_filters_never_fabricate_output_for_empty_input asserts \"\" for \"\""
```

#### #8201

Frontier cue:
```json
"client timeout→explicit bash_kill; ⊘orphan execution (executor starvation)"
```

Kimi K2 cue:
```json
"kill long-running tasks via `bash_kill` on client timeout ⊘executor starvation"
```

#### #6366

Frontier cue:
```json
"prewarm indexes via `aft warmup` with identical prompts"
```

Kimi K2 cue:
```json
"AFT benchmark: pre-warm search indexes via `aft warmup` identical prompts"
```

#### #8255

Frontier cue:
```json
{"mergeInto":8391}
```

Kimi K2 cue:
```json
{"mergeInto":8391}
```

### Kimi K2: ARCHITECTURE

- Model: `moonshotai/kimi-k2`
- Raw completion: `/tmp/visual-memory/trial-Kimi K2-ARCHITECTURE.xml`
- Calls: 1; parse retry: not needed
- Coverage: **72/123**; uncovered: 8355, 7017, 6738, 6562, 6486, 6258, 6160, 5262, 4882, 7814, 7939, 6887, 8298, 8840, 8253, 8130, 8040, 6981, 6616, 6255, 6159, 6145, 5903, 5618, 5604, 5592, 5589, 5248, 8280, 8349, 8299, 8087, 7571, 6845, 8616, 8555, 8158, 7940, 7835, 7738, 7728, 7723, 7294, 7012, 8347, 8050, 7308, 8534, 7353, 6094, 6059
- Anchor fidelity: **23.7%** (76/321 source exact tokens retained in the effective cue)
- Importance passthrough: **72/123**; mismatches: 8355, 7017, 6738, 6562, 6486, 6258, 6160, 5262, 4882, 7814, 7939, 6887, 8298, 8840, 8253, 8130, 8040, 6981, 6616, 6255, 6159, 6145, 5903, 5618, 5604, 5592, 5589, 5248, 8280, 8349, 8299, 8087, 7571, 6845, 8616, 8555, 8158, 7940, 7835, 7738, 7728, 7723, 7294, 7012, 8347, 8050, 7308, 8534, 7353, 6094, 6059
- Full-manifest validator: failed — polarity mechanism missing from rendered cue 7801: executor: lock-free epoch guard for bg completion wakes; ⊘stale maintenance CLEARs erasing newer wakes

#### Hard validator failures

- **missing polarity mechanism:** 26
  - #7801: polarity mechanism missing from rendered cue 7801: executor: lock-free epoch guard for bg completion wakes; ⊘stale maintenance CLEARs erasing newer wakes
  - #7802: polarity mechanism missing from rendered cue 7802: SubcTransportPool: singleflight + tombstone states; ⊘concurrent routeOpen races
  - #7753: polarity mechanism missing from rendered cue 7753: bg_events subscription: dedicated route channel; ⊘tool dispatch saturation blocking bg completion wakes
- **hub noun repetition:** 0
- **memory ID leakage:** 0
- **broken exact anchors:** 0
- **unbalanced parentheses:** 0
- **other validator failures:** 1
  - #7537: duplicate spec id 7537 (2 entries)

#### Room quality

- Kimi K2 rooms (9): AFT routing (8), Bash permissions (4), Build lifecycle (12), LSP clients (7), Security boundaries (6), Semantic indexing (6), Storage keys (11), Subc transport (13), Watcher drains (6)
- Frontier spec rooms (10; Round-2 comparison target 16): Artifacts, Bash, Bridge, Embeddings, Inspect & LSP, Prompts, Subc, Synapse, TUI, Watcher & scheduler

#### Four evenly spaced cue comparisons

#### #7467

Frontier cue:
```json
["semantic extensions=every real tree-sitter symbol query; generic collect_file_chunks_from_source","⊘HTML/Markdown/JSON (query=None); sync guard semantic_extension_policy_stays_in_sync_with_parser_code_arms"]
```

Kimi K2 cue:
```json
"is_semantic_indexed_extension; generic chunker (parse → extract_symbols_from_tree → symbols_to_chunks); includes Java/Kotlin/Ruby/Swift/Scala/Lua/Perl/R (.r/.R); ⊘HTML/Markdown/JSON (query=None, document/data tree-walking); regression-guard: semantic_extension_policy_stays_in_sync_with_parser_code_arms"
```

#### #8298

Frontier cue:
```json
"health/root list try-lock only→busy/empty; ⊘blocking responder (try-lock)"
```

Kimi K2 cue:
```json
MISSING
```

#### #7528

Frontier cue:
```json
"active semantic build defers callgraph warm + Tier-2 scans (peak RSS)"
```

Kimi K2 cue:
```json
"Tier-2 cold-seed serialization: defer callgraph-warm + Tier-2 scans during active semantic builds; reduce peak RSS"
```

#### #8601

Frontier cue:
```json
["artifact_cache_key root commit: aft 90ff783f3f4c5cf2; opencode aa69d52ef2dcad4d","alfonso be627d40119a995e; magic-context e274ab0872bb490b; subc 29f01b6c90df2c83; synapse 3e6b8ee50bb2bb06"]
```

Kimi K2 cue:
```json
"artifact key ground truth: aft=90ff783f3f4c5cf2 (sha256(root commit)[..16]); aa69d52ef2dcad4d=OpenCode OSS; memo at <storage>/cache-keys.json"
```

### Gemini 3.5 Flash: PROJECT_RULES

- Model: `google/gemini-3.5-flash`
- Raw completion: `/tmp/visual-memory/trial-Gemini 3.5 Flash-PROJECT_RULES.xml`
- Calls: 1; parse retry: not needed
- Coverage: **83/83**; uncovered: none
- Anchor fidelity: **62.5%** (182/291 source exact tokens retained in the effective cue)
- Importance passthrough: **83/83**; mismatches: none
- Full-manifest validator: failed — polarity mechanism must follow marker 8713: latency assertions/measurements ⊘calibrate on main dev box (contested by parallel gates); run locally for correctness, run bench harnesses on quiet M1 Max box (\`ssh test@tests-MacBook-Pro.local\`); M1 protocol: \`bench.lock\` + ping SYNAPSE first (pause self-hosted runner); storm-test bounds use \`DEBUG_BOUND_MULTIPLIER\` headroom, ⊘use local wall times

#### Hard validator failures

- **missing polarity mechanism:** 9
  - #8713: polarity mechanism must follow marker 8713: latency assertions/measurements ⊘calibrate on main dev box (contested by parallel gates); run locally for correctness, run bench harnesses on quiet M1 Max box (\`ssh test@tests-MacBook-Pro.local\`); M1 protocol: \`bench.lock\` + ping SYNAPSE first (pause self-hosted runner); storm-test bounds use \`DEBUG_BOUND_MULTIPLIER\` headroom, ⊘use local wall times
  - #8354: polarity mechanism must follow marker 8354: plugin release gates must include packed-install visual/behavioral check (⊘rely solely on import probe) to verify JSX reactivity and prevent eager evaluation freezes
  - #7087: polarity mechanism must follow marker 7087: classify tool \`execution_mode\` by-hand based on externally-observable side effects (file writes/deletes); ⊘auto-derive from concurrency lane
- **hub noun repetition:** 0
- **memory ID leakage:** 0
- **broken exact anchors:** 0
- **unbalanced parentheses:** 0
- **other validator failures:** 0

#### Room quality

- Gemini 3.5 Flash rooms (8): Mason & Worktrees (5), Performance & Benchmarks (8), Plugins & Workspace (10), Process & Environment (19), Release & Docs (11), Subc & Storm Tests (5), Testing & CI (11), Tooling & State (14)
- Frontier spec rooms (7; Round-2 comparison target 7): E2E, Engineering policy, Inspect & tools, Plugin, Release & CI, Tests, Worktrees

#### Four evenly spaced cue comparisons

#### #7863

Frontier cue:
```json
["shortcircuit: empty→empty; ⊘fabrication (filter summarizes only actual bytes)","guard compress_filters_test.rs + builtin_filters_never_fabricate_output_for_empty_input"]
```

Gemini 3.5 Flash cue:
```json
"compress filters ⊘fabricate output (empty input stays empty); test `compress_filters_test.rs::builtin_filters_never_fabricate_output_for_empty_input` asserts \"\" for \"\""
```

#### #8201

Frontier cue:
```json
"client timeout→explicit bash_kill; ⊘orphan execution (executor starvation)"
```

Gemini 3.5 Flash cue:
```json
"kill long-running tasks explicitly (e.g. via `bash_kill`) upon client-side timeout to prevent executor starvation"
```

#### #6366

Frontier cue:
```json
"prewarm indexes via `aft warmup` with identical prompts"
```

Gemini 3.5 Flash cue:
```json
"AFT benchmark runs require search indexes to be pre-warmed via `aft warmup` with identical prompts"
```

#### #8255

Frontier cue:
```json
{"mergeInto":8391}
```

Gemini 3.5 Flash cue:
```json
{"mergeInto":8391}
```

### Gemini 3.5 Flash: ARCHITECTURE

- Model: `google/gemini-3.5-flash`
- Raw completion: `/tmp/visual-memory/trial-Gemini 3.5 Flash-ARCHITECTURE.xml`
- Calls: 2; parse retry: attempted but did not recover
- First parse rejection: XML parse failed: entry in room Subc transport has malformed attributes
- Fail-closed parse rejection: XML parse failed: entry in room Tool execution has malformed attributes
- Coverage: not measured because the complete XML root was rejected.
- Hard validator failures: not measured because validation never receives a partial manifest.
- Anchor fidelity: not measured because validation never receives a partial manifest.
- Room quality and side-by-side cues: not measured because validation never receives a partial manifest.
