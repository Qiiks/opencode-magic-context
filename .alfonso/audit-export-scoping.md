# Derived-artifact scoping audit

**Scope:** `packages/plugin/scripts/`, `packages/plugin/src/`, and `packages/cli/src/` (production code only; test fixtures and ordinary settings/config rewrites excluded).

**Coverage:** 63 candidate derived-artifact write-sites examined: 31 script file/report/corpus writes, 25 plugin runtime projection/blob/cache/log writes, and 7 CLI export/snapshot/report writes. A key stored on the enclosing exported record counts as surviving; the findings below are cases where a durable derived record cannot retain or recover the source row's scope once its source is removed.

## Findings — artifacts that can outlive their source

### 1. Stable user-memory provenance loses the source session when candidates are consumed

- **Write:** `packages/plugin/src/features/magic-context/user-memory/storage-user-memory.ts:120-122` writes `user_memories.source_candidate_ids` as a JSON array of candidate IDs.
- **Source-row scope:** every `user_memory_candidates` row carries `session_id` (and source compartment bounds) at `storage-user-memory.ts:43-54`.
- **Dropped key:** the durable user-memory record retains only numeric candidate IDs, not the candidate `session_id` values.
- **Unanswerable after source deletion:** “Which sessions supplied the observations that justified this stable user-memory fact?” The candidate table can answer this before cleanup; the surviving JSON array cannot.
- **Why it outlives the source:** the review flow creates stable memories and then deletes reviewed candidates in the same lease-guarded write (`dreamer/review-user-memories.ts:287-301`). Candidate TTL pruning is also explicit (`storage-user-memory.ts:98-107`). This is permanent provenance loss, not merely a convenience projection.

### 2. Primer support provenance loses session, harness, and source-message scope after candidate retention expires

- **Write:** `packages/plugin/src/features/magic-context/storage-primers.ts:351-352` and `:384-385` write `primers.source_candidate_ids` as a JSON array.
- **Source-row scope:** `primer_candidates` carries `project_path`, `harness`, `session_id`, and source start/end message IDs (`storage-primers.ts:6-20`, `:55-69`).
- **Dropped keys:** `session_id`, `harness`, and source message-span IDs are absent from the persisted primer provenance blob. The enclosing primer keeps `project_path`, so project scope survives; the per-observation session scope does not.
- **Unanswerable after source deletion:** “Which harness sessions and message spans provided the support for this primer?”
- **Why it outlives the source:** active primers retain indefinitely while the candidate-prune path deletes referenced candidates once they reach the 180-day maximum age (`dreamer/promote-primers.ts:86-112`), even though ordinary TTL cleanup temporarily protects referenced IDs. The remaining IDs are then dangling references.

### 3. LongMemEval result and summary files omit the project execution scope

- **Writes:** `packages/plugin/scripts/longmemeval/runner.ts:734` appends each `results.jsonl` record; `:901-905` writes `summary.json`.
- **Source-row scope:** the benchmark run is scoped by `projectDirectory` (and its OpenCode instance) in `RunnerConfig` / persisted runner state (`longmemeval/types.ts:74-99`, `runner.ts:224-268`). The generated OpenCode sessions also expose their directory in the live harness store.
- **Dropped key:** `QuestionResultRecord` records dataset question IDs and generated session IDs but no `projectDirectory`; `RunSummary` records the dataset and selection signature but no project directory either (`longmemeval/types.ts:178-235`).
- **Unanswerable after source deletion:** “Against which project directory was this result/summary run?” This matters when identical dataset selections are run against several worktrees.
- **Why it outlives the source:** the runner can delete every generated OpenCode session after recording results when `--cleanup` is enabled (`runner.ts:739-761`). The files are the intended durable benchmark record, while state files are resumability state rather than a guaranteed companion artifact.

### 4. Project-identity seed export aggregates away session binding provenance

- **Write:** `packages/plugin/scripts/export-project-identities.ts:114-128` writes one JSONL record per identity.
- **Source-row scope:** `session_projects` rows are scoped by `session_id` (and, in the durable schema, harness); the script joins that binding to an OpenCode session directory (`export-project-identities.ts:54-75`).
- **Dropped key:** the JSONL row retains `identity`, roots, and only a count of `session_bindings`; it omits the contributing `session_id` and harness.
- **Unanswerable after source deletion:** “Which session(s) established this identity/root binding, and on which harness?” The live joined rows can answer this; the seed corpus cannot distinguish several bindings with the same count.
- **Why it can outlive the source:** this is explicitly an export/seed-import corpus (`export-project-identities.ts:2-17`) whose target is user-selected and may be consumed later. OpenCode sessions can be removed independently of the exported corpus, so the aggregation can become the only surviving topology record.

## Findings — artifacts that may outlive their source

### 5. Visual-memory trial metrics and coverage reports omit the frozen corpus project identity

- **Writes:** `packages/plugin/scripts/experiments/visual-memory/run-palace-trial.ts:971` writes each trial `metrics.json` and `:1126` writes `trials/REPORT.md`; `author-palace.ts:1071-1072` writes the detached palace text and `coverage.json`.
- **Source-row scope:** the trial corpus is explicitly scoped by `projectIdentity` (`run-palace-trial.ts:63-68`, `:228-233`); coverage maps memory IDs sourced from that corpus.
- **Dropped key:** metrics contain model, prompt, directory, coverage counts, and usage, but no project identity or corpus revision. `coverage.json` likewise maps numeric memory IDs without a project identity.
- **Unanswerable after source replacement:** “Which project identity and frozen memory corpus produced this model/prompt result (or coverage placement)?” Memory IDs alone are not a portable project-scoped reference.
- **Why it may outlive the source:** the frozen corpus is overwritten on `--rebuild-corpus` (`run-palace-trial.ts:239-242`, `:1131-1134`), while trial metrics/reports are retained under their output directories. These are development artifacts, so this is lower severity than the database findings, but a retained report can become detached from the corpus it describes.

## CLEAN — checked surfaces with no dropped scoping key

- **Context dump export:** `packages/plugin/scripts/context-dump/write-dump-json.ts:34-76` writes a top-level `session_id`; exported messages also retain `info.sessionID` when source data lacks it (`read-opencode-session.ts:75-89`). Its pending-operation and per-message arrays remain inside that session record.
- **Embedding baseline snapshots:** `packages/plugin/scripts/embedding-baseline.ts:362-392` retain `projectIdentity`, model ID, dimension, timestamp, and per-query result data in the snapshot record.
- **LongMemEval runner state:** `runner-state.json` retains `projectDirectory`, dataset path, selection signature, and per-question session mappings; only the separately exportable result/summary artifacts above are affected.
- **Visual-memory frozen corpus:** `run-palace-trial.ts:228-242` retains `projectIdentity` and source database metadata alongside its memory rows.
- **Historian state/response XML files:** `historian-state-file.ts:48-75` and `compartment-runner-historian.ts:671-715` are transient, session-named artifacts. They are consumed during the active historian run and explicitly deleted; they are not a retained record. Response-dump cleanup is called after each consuming path (`compartment-runner-historian.ts:159, :204, :289, :609`).
- **Persisted model-limit cache:** `packages/plugin/src/shared/models-dev-cache.ts:161-191` is harness-scoped in its filename and model-keyed in its serialized map; it caches model metadata rather than projecting scoped source rows.
- **Runtime SQLite blobs whose containing row retains scope:** checked `session_meta` state blobs, smart-note compiled config/manifests, compaction marker JSON, clone ID blobs, dream-run task/memory-change JSON, historian-run fact counts, compartment-event fields, mural memory IDs, and embedding/synapse measurement blobs. Their containing rows carry the relevant `session_id` and/or `project_path` (and harness where required); mirror full-row snapshots also retain `project_path` in both the row and snapshot.
- **CLI OpenCode-to-Pi migration JSONL:** `packages/cli/src/commands/migrate.ts:486-580, :1012-1025` includes a session header and an explicit “migrated from OpenCode session …” boundary before derived entries, preserving source-session lineage.
- **CLI SQLite backups:** `packages/cli/src/lib/database-access.ts:149-165` serializes complete SQLite snapshots, not a reduced projection. `migrate-session.ts:621-640` writes those snapshots while the databases are locked.
- **CLI doctor diagnostics, redaction, and issue reports:** `diagnostics-opencode.ts:854-946`, `diagnostics-pi.ts:515-580`, `logs-opencode.ts:108-193`, and `logs-pi.ts:49-111` retain reported session IDs in recent-session metadata. Redaction replaces only home/user paths and credential material (`packages/plugin/src/shared/redaction.ts:110-215`); it does not redact session, project-identity, harness, or store-UUID keys. The optional interactive session filter only limits log lines and is not itself a source-row scope key.
- **Other plugin script writers:** checked schema/config/reference/prompt/font/TUI generators, calibration output, Git-dedup goldens, prompt-surface artifacts, and temporary smoke fixtures. Their inputs are versioned source or same-function test fixtures rather than scoped live rows; none produced a durable projection that drops a row key.
- **CLI setup/adapters and plugin config/preference writes:** these rewrite authoritative configuration/preferences rather than serializing a derived row projection, so they are out of scope for this audit.

No source code, database, migration, or test command was run or modified during this audit.
