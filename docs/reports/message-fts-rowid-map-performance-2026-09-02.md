# Message FTS rowid-map incident fix and performance evidence

## SCHEMA MIGRATION v83 — COORDINATED OPENCODE/PI ROLLOUT REQUIRED

This change advances `context.db` to **schema v83**. Per the schema-fence rollout rule, merge, build, and OpenCode/Pi restart must be one coordinated fence-mover operation; an older live harness must not remain attached after v83 is applied. This task does not push master or restart fleet processes.

## Structural fix

Migration v83 adds:

```sql
CREATE TABLE message_fts_rowid_map (
  session_id TEXT NOT NULL,
  message_ordinal INTEGER NOT NULL,
  fts_rowid INTEGER NOT NULL,
  PRIMARY KEY(session_id, message_ordinal)
);
```

A sidecar was chosen instead of changing `message_history_source` because pre-v68 FTS rows can exist without source rows. The sidecar can therefore map every legacy FTS document without rebuilding FTS or pretending source metadata exists. Message indexing records each inserted FTS `lastInsertRowid` in the same writer transaction. Chunk reads constrain the ordinary table by `(session_id, message_ordinal)`, then access FTS5 by `rowid`.

A singleton `message_fts_rowid_map_backfill_state` stores `watermark_rowid`, completion, and update time. Legacy rows are read in rowid-ascending windows of 500. Every window and watermark update commit atomically, and the async driver yields through a zero-delay timer between windows. Incomplete legacy spans are deferred by chunk candidate selection; there is no fallback to the old UNINDEXED predicate.

Session deletion and message-index reconciliation use the sidecar for FTS rowid deletes/lookups. `message_fts_rowid_map` is in `SESSION_SCOPED_TABLES`, so both explicit session clearing and orphan sweeping remove it in dependency-safe order.

The automatic project chunk drain now embeds at most eight compartments per sweep. Candidate classification and embedding preparation yield between compartments. The shared registry is used by both OpenCode and Pi.

## Query plans

The plan assertion printed these plans under Bun SQLite:

```text
new: SEARCH map USING INDEX sqlite_autoindex_message_fts_rowid_map_1 (session_id=? AND message_ordinal>? AND message_ordinal<?)
     SCAN fts VIRTUAL TABLE INDEX 0:=
```

FTS5 labels a rowid equality access as `SCAN ... INDEX 0:=`; the `:=` is the rowid point constraint. The removed query printed:

```text
old: SCAN message_history_fts VIRTUAL TABLE INDEX 0:
     USE TEMP B-TREE FOR ORDER BY
```

The test requires both the map primary-key search and FTS virtual-table index `0:=`, and rejects unconstrained index `0:`. Its non-vacuity mutation replaced the rowid join with the former UNINDEXED identity join; the assertion failed because `INDEX 0:=` disappeared.

## Synthetic benchmark

Command:

```text
bun packages/plugin/scripts/benchmark-message-fts-chunk-loader.ts
```

Corpus: 200,000 FTS rows, 1,105 content characters per row, one 100-message span near the end of the corpus, five warm samples.

```text
old_ms=22.858 samples=22.858,22.747,22.667,23.220,23.148
new_ms=0.092 samples=0.103,0.093,0.092,0.090,0.090
speedup=247.6x
```

The synthetic in-memory result understates disk-cache pain on the 5.2 GB incident database, but it isolates the algorithmic change: full FTS content scan plus sort versus B-tree range resolution and 100 rowid point reads.
