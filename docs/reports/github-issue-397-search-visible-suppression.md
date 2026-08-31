# GitHub issue #397 — search was working; visibility suppression was silent

## Resolution

Search was working for the reported memory queries. Every SQL hit in the report was already present in that session's project-memory block, so `ctx_search` intentionally removed it from hidden recall. The defect was ours: the tool collapsed “matches exist but are already visible” into the same `No results found` sentence used for a genuinely empty corpus.

The report's direct SQL and session metadata make this conclusive for the primary symptom:

- `session_meta.memory_block_ids` was `[3,4,54,5,7,49,53,6,50,51,52,55]`;
- direct FTS returned `[6,3,49,55,51,50,7]`;
- all seven SQL hits are a strict subset of the rendered-memory ids.

The second project's SQL ids reportedly exactly matched its project-memory block, which is the same mechanism. The report's expiry, embedding identity, migration, workspace, sanitizer, and project-resolution probes correctly ruled out the lower layers and materially shortened this diagnosis.

## Source confirmation and limits

### Memory text, semantic, and id-shaped queries

`unifiedSearch` passes `visibleMemoryIds` into `searchMemories`. `mergeMemoryResults` removes a ranked candidate when its id is already rendered. This is intentional: `ctx_search` returns hidden recall, not content already present in the prompt.

The id-shaped fast path has the same rule. `resolveMemoriesByIdsForSearch` resolves a visible memory and then excludes it when the id is in the rendered set. Thus the report's `#49` probe also disappeared because id 49 was already visible; bypassing embeddings does not bypass prompt-visibility filtering.

`getVisibleMemoryIds` reads `session_meta.memory_block_ids` from SQLite on every tool call. That explains cross-process persistence: a fresh `pi -p` process observes the same durable rendered set rather than relying on process-local search state.

The Rust module facade also needed the fix. It loads durable `rendered_memory_ids`, passes them as `excluded_memory_ids`, and previously discarded matching memories without retaining a cause. The module now returns suppression diagnostics from both lexical and id-shaped memory searches and renders the same visible-memory explanation. Its owned search corpora are memories, durable compartments, and notes; it does not own the TypeScript raw-message, Primer, or git-commit indexes.

### Message history

Both OpenCode and Pi set the raw-message cutoff to the last compartment end. With no compartment, the cutoff is zero; message ordinals are positive, so a short/no-compartment session intentionally excludes every raw-message hit as live context. The search now obtains eligible rows and the exact newer-match count from one materialized FTS match set, rather than issuing a second search query, and reports the suppressed count.

### Git commits on directory projects

A `dir:` identity is strong evidence for a non-indexable commit corpus, but it is not by itself proof of “no repository”: project identity also uses `dir:` for empty repositories and cold transient git-resolution failures. The formatter therefore does not infer from the identity string. Pi and OpenCode now make a cheap `.git`-metadata probe on the actual session directory and say `no git repository — commit search unavailable for this project` only when that fact is known.

The report's `commits=false` registration line independently says commit indexing was disabled for that registration. Commit indexing classifies `not_a_repo` and `no_head` as non-indexable and writes no commit corpus.

### Notes and Primers

The report proves memory rows and memory FTS hits, but does not include corresponding note, Primer, raw-message, or commit-corpus rows for the tested query. Their empty results therefore do not establish a common all-source failure. They fit independent source behavior: no matching notes/Primers, no indexed commits, and live-tail raw messages can all be empty while memory SQL still matches. This is the residue the report cannot adjudicate from the captured data; the new output makes those intentional causes distinguishable where the filter has evidence.

### Why there were zero `[search]` lines

The normal successful search path does not emit a `[search]` log line. The only `[search]` log in `unifiedSearch` is the query-embedding failure path. A successful query whose candidates are all intentionally suppressed therefore produced neither an error nor a search log. The zero-line observation is expected and was not evidence that `unifiedSearch` did not execute.

### Mid-day change

The observed “worked earlier, then became empty” is compatible with prompt materialization. Both harnesses persist the ids rendered into m[0] alongside the materialized project-memory block; subsequent searches read that durable set. Fresh writes/mutations advance the memory markers and a later materialization can expand the rendered set to include the newest matching rows (the report's ids 54 and 55 were already in the block when captured). Before that refresh, the same rows can be hidden recall; after it, they are intentionally suppressed.

The report does not timestamp m[0] materialization relative to the last successful search, so that sequence is a source-supported explanation, not a proven reconstruction of the exact mid-day transition.

## Fix

The TypeScript search layer now exposes a cheap diagnostic sink populated from the candidate sets already used by filtering:

- memory diagnostics retain the count and sorted ids of matching memories already visible in the project-memory block;
- raw-message diagnostics retain the exact count newer than the compartment boundary from the same materialized FTS query;
- tool wrappers identify an actually non-git session directory without a `git log` subprocess;
- the shared `search.ts` formatter is used by both OpenCode and Pi, removing the duplicated formatter and keeping output parity;
- genuine empty searches retain the existing sentence byte-for-byte.

Examples:

```text
No hidden results found for "Odoo".

Memories: 7 matches found, all already visible in your project-memory block (ids 3, 6, 7, 49, 50, 51, 55).
```

Mixed results retain hidden hits and add:

```text
Memories: 2 additional matches suppressed because they are already visible in your project-memory block (ids 3, 6).
```

Other distinguishable causes render as:

```text
Message history: 2 raw-message matches are newer than the last compartment boundary (already in your context).
Git commits: no git repository — commit search unavailable for this project.
```

## Secondary finding: Pi dreamer maintenance registration

The `embeddings=false` line was a separate real wiring bug, not the search cause. Pi's project-config resolver correctly produced `current.config.memory.enabled`, and `syncDreamerProjectRegistration` correctly passed it to `registerPiDreamerProject`. That function then omitted both `memoryEnabled` and `embeddingConfig` when constructing the shared timer registration. Consequently `embeddingSweepEnabled = args.memoryEnabled === true` observed `undefined` and logged false.

The Pi registration now threads both resolved values into `startDreamScheduleTimer`. This flag controls scheduled maintenance/backfill; it does not gate `ctx_search`, whose embedding configuration is registered through the separate project embedding registry.

## Regression and mutation evidence

Regression coverage includes:

- the report's exact shape: visible ids `[3,4,54,5,7,49,53,6,50,51,52,55]`, seven matching ids `[6,3,49,55,51,50,7]`, and no hidden memory result;
- mixed visible/invisible memories returning the invisible hit plus the suppressed count and ids;
- Pi id-shaped lookup explaining an already-visible memory;
- a no-compartment session reporting two matching raw messages in the live tail;
- a non-repository directory reporting commit search unavailable;
- a genuinely empty search retaining the previous sentence;
- Rust module-facade mixed and id-shaped visibility output;
- Pi dreamer registration receiving resolved `memoryEnabled=true` and the embedding provider.

Executed mutation proof used the exact `NON-VACUITY BREAK` marker and restored each change immediately:

1. Removing visible-id diagnostic collection failed the all-visible fixture at `packages/plugin/src/tools/ctx-search/tools.test.ts:97` and the mixed fixture at line 128.
2. Zeroing the live-tail suppression count failed the boundary fixture at `packages/plugin/src/tools/ctx-search/tools.test.ts:167`.
3. Forcing the repository-availability probe true failed the directory-project fixture at `packages/plugin/src/tools/ctx-search/tools.test.ts:187`.
4. Omitting the resolved dreamer maintenance fields failed `packages/pi-plugin/src/dreamer/index.test.ts:263` (`true` expected, `undefined` received).

No deliberate mutation remains in changed source or tests.

## Verification

- `bun install --frozen-lockfile` — passed; the lockfile was unchanged.
- focused search/dreamer regression gate — passed: 53 tests, 193 assertions across the shared search, OpenCode tool, Pi tool, Rust-mode visibility adapter, and Pi dreamer files.
- `bun run --cwd packages/plugin test` — 4,243 passed; the one changed visibility assertion and three unrelated timing-sensitive failures were rerun narrowly and passed (async reconciliation, tail-hygiene p95, and Rust adapter steady-state timing).
- `bun run --cwd packages/pi-plugin test` — 880 passed; the unrelated pre-existing 15 ms Pi tail-hygiene p95 performance gate failed under host contention and also failed in isolation (24 ms in-suite, 63 ms isolated). No changed search/dreamer test failed.
- `bun run typecheck` — passed for plugin, Pi, CLI, and retina-local-fs.
- `bun run build` — passed for plugin, Pi, and CLI.
- plugin and Pi Biome gates — passed; Pi retained two pre-existing non-null-assertion warnings in unrelated files.
- `cargo fmt --all -- --check` — passed.
- `cargo test -p mc-module ctx_search_matches_typescript_shape_for_available_module_corpora` — blocked before the changed search code by the base revision's unrelated `E0639` at `crates/mc-module/src/lib.rs:15060`: the newly pinned `subc-protocol` marks `ModuleManifest` non-exhaustive while the base constructs it with a struct literal.

## Reply draft for #397

Resolved: search was working; every memory match in the report was already in the session's project-memory block. Your direct FTS ids `[6,3,49,55,51,50,7]` are a strict subset of `session_meta.memory_block_ids`, and the second project's exact equality confirms the same visibility filter. `ctx_search` intentionally returns only recall the agent cannot currently see, so it removed those rows. The defect was ours: we rendered that intentional suppression as a bare `No results found`, making healthy search indistinguishable from failure.

Thank you for the instrument-grade report, especially the ruled-out table and the direct production-SQL comparison. It let us clear expiry, embeddings, migration, workspace, sanitizer, project resolution, and process-local state and focus on the post-query filter.

The tool now says exactly what happened, including count and ids, for example: `Memories: 7 matches found, all already visible in your project-memory block (ids 3, 6, 7, 49, 50, 51, 55).` Mixed searches still return hidden hits and note how many visible matches were suppressed. It also distinguishes raw-message hits still newer than the compartment boundary and commit search in a directory with no git repository. Genuine empty searches keep the prior wording. OpenCode, Pi, and the Rust module facade now agree on visible-memory suppression output, and id-shaped lookups such as `#49` get the same explanation.

The zero `[search]` lines were expected under the old code: only embedding failures logged with that prefix; successful searches ending in intentional suppression did not log. The earlier-in-the-day behavior is compatible with a later project-memory materialization refreshing the durable rendered-id set, although the report does not timestamp that refresh closely enough to prove the exact transition sequence.

Separately, your `embeddings=false` observation found a real Pi registration bug. Resolved `memory.enabled` reached the Pi dreamer wrapper but was not forwarded into the shared maintenance timer, so `args.memoryEnabled === true` saw `undefined`. We now thread the resolved memory and embedding config. That affected scheduled embedding maintenance, not retrieval, but your observation was correct.
