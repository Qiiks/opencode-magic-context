# Task: mc-store/mc-module — ctx_memory store ports + guards + lexical search (identity-independent unit)

Repo: this worktree (branch off subc-migration). Rust workspace at repo root; crates involved: `crates/mc-store`, `crates/mc-module`.

You are building the STORE-SIDE half of the MCP facade slice. The plan is at `.alfonso/plans/mc-mcp-facade-slice.md` (v4) — read it in full first. The facade ROUTING/IDENTITY wiring (BindIdentity token, session.resolve consumer client, manifest/dispatch changes) is EXPLICITLY OUT OF SCOPE — parked on an external spike. Do not touch `manifest()`, `dispatch_value`, or any binding code.

## Deliverables

### 1. mc-store: full-row memory ports (crates/mc-store/src/lib.rs)

Current `StoredMemory` is render-only. Add named full-row lookup and mutation methods on McStore:

- `get_memory_full(id)` — full row incl. status, category, content, project_path, superseded_by_memory_id, merged_from, metadata_json.
- `insert_memory(...)` — additive write. Content-hash duplicate check per TS semantics (see reference below): exact-duplicate content in same project+category returns the existing id, no new row, NO mutation-log row.
- `update_memory_content(id, content, now_ms)` — row mutation + `mc_memory_mutation_log` append IN THE SAME fenced transaction.
- `archive_memory(id, reason, now_ms)` — status flip + mutation-log append, same transaction.
- `merge_memories(target_id, source_ids, merged_content, now_ms)` — target update + sources superseded (status + superseded_by_memory_id + merged_from on target) + ONE mutation-log append per affected row, all in ONE fenced transaction.

FENCED TRANSACTION = the store's existing CAS/transaction discipline (mirror how `promote_facts` and the pass-commit writes are structured — find them in lib.rs and copy the fencing shape, do not invent a new one).

THE LOAD-BEARING CACHE PIN (from the plan, non-negotiable): non-additive mutations (update/archive/merge) MUST append `mc_memory_mutation_log` in the same transaction as the row mutation. The transform's m1 change-detection (`crates/mc-module/src/m1_compose.rs` — `m1_revision_signal` reads `max_memory_mutation_id`) only surfaces changes through that log; a row mutation without a log row leaves cached m0/m1 silently stale. Additive inserts must NOT write the log (they ride `max_memory_id` → `<new-memories>`).

### 2. mc-module: guard layer (new module, e.g. crates/mc-module/src/memory_tool.rs)

Pure functions taking the store + args, no routing/identity code. Port the TS security guards with exact parity — reference: `packages/plugin/src/tools/ctx-memory/tools.ts`:

- Visibility-before-mutation (tools.ts:323-354 `memoryVisibleToTool` semantics): own-project memories fully mutable; FOREIGN memories (workspace-union visible) mutable ONLY when their category is workspace-shared. Use the SAME workspace-union primitives the transform already uses (`workspace_union` reads in mc-store — one code path, no parallel reader).
- Applied to update (tools.ts:434-437), merge (510-533), archive (686-712).
- Cross-category merge structurally REJECTED (tools.ts:537-544): all source memories + target must share one category; reject with a typed error, never merge.
- Status checks: only `active`/`permanent`, non-superseded rows are mutable; archive of already-archived is a no-op success (idempotent), mutation of a superseded row is a typed error.

### 3. mc-module: lexical search (same new module)

`search_memories_and_compartments(store, project, query, limit)`:
- SQL LIKE (case-insensitive) over `mc_memories` content (active+permanent, workspace-union visible per category-sharing) and `mc_compartments` title + tier text for the resolved session's project.
- Simple honest ranking: exact substring in memory content > compartment title hit > compartment body hit; recency tiebreak. No FTS, no embeddings, no fuzz — this is keyword search and will be described as such.
- Return a compact result struct (source kind, id, snippet with the match, category/seq metadata) ready for MCP text rendering later. Snippets: trim to ~200 chars around the first match.

### 4. Tests (co-located, non-vacuous)

- Security: foreign-workspace mutation DENIED on unshared category for each of update/merge/archive; ALLOWED on shared category; cross-category merge rejected; superseded-row mutation rejected; archive idempotence.
- Cache (THROUGH THE PUBLIC PORT FUNCTIONS, not raw SQL): update/archive/merge each advance `max_memory_mutation_id` (assert the log row exists with the right memory_id in the same transaction — e.g. crash-simulation via a failing closure must roll back BOTH row and log); additive insert does NOT write the log but advances `max_memory_id`.
- m1 integration: after a non-additive mutation via the public port, `m1_revision_signal` (m1_compose.rs) reports a change and `compose_m1_from_store` renders a `<memory-updates>` delta; after an additive insert, `<new-memories>` renders. Assert NO HARD trigger fires from either (use the existing transform test fixtures in crates/mc-module/src/transform.rs tests as the pattern for driving a pass).
- Duplicate-insert: same content+project+category returns existing id, row count unchanged.
- Search: match in memory content found; compartment tier-text match found; ranking order asserted; limit respected; foreign unshared-category memories EXCLUDED from results.

## Gates (all must pass)

- `cargo test -p mc-store -p mc-module`
- `cargo clippy -p mc-store -p mc-module -- -D warnings`
- `cargo fmt --check`
- `check_comments` — comments explain invariants (especially the mutation-log cache pin), never reference this task, the plan file, or review rounds.

## Style pins

- SQLite binds: spread positional args, never array-form.
- No 0-as-sentinel for ids/sequences — Option<T> everywhere absence is possible.
- Commit with trailer: `Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>`
