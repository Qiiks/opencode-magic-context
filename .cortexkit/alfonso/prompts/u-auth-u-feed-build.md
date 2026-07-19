# Build U-AUTH + U-FEED: authority state machine + changefeed/mirror protocol

You are building the foundation units of the rust-mode ownership plan. The COMPLETE and
BINDING spec is `.alfonso/plans/rust-mc-ownership-u9-u11.md` (v5) in the repo — read it
fully first, especially the "U-AUTH", "U-FEED", "v3 amendments", "v4 amendments", and
"v5 pins" sections. The v5 pins (P1-P7) are non-negotiable review outcomes from four
adversarial Oracle rounds; do not reinterpret them. Context: the rust-mode transform
spine (U1-U8) is shipped — see `.alfonso/plans/rust-mc-mode-v1.md`,
`packages/plugin/src/hooks/magic-context/rust-mode-transform.ts`,
`module-state-sync.ts`, and `crates/mc-module/` + `crates/mc-store/`.

## Scope of THIS task (U-AUTH + U-FEED only — no U10 route flip, no U9/U11)

### Module side (crates/mc-store + crates/mc-module)

1. Authority table: rows keyed (context_store_uuid, project, domain) with
   state TS|PREPARING|MODULE|DRAINING, generation u64, DRAINING journal columns
   (captured upper bounds per domain, drain generation, per-domain cursors, per-step
   completion bits, coordinator lease, checksum results) per v3 H1. New migration.
2. Changefeed: per-domain append-only feed table (feed_seq, domain, op
   insert|update|tombstone, module_row_id, full_row_snapshot JSON, content_hash from
   the stored normalized_hash column). Feed appends implemented as SQLite TRIGGERS on
   mc_memories and mc_notes (INSERT/UPDATE/DELETE; NEW snapshot for insert/update, OLD
   for delete; no-op UPDATE filtered with a WHEN clause using `IS NOT` comparisons —
   P7, plain != forbidden, add the trigger-SQL lint test). Inventory every existing
   mutation site (facade ops, promote_facts_tx, any direct SQL) and add the
   trigger-coverage test: mutate via every site, assert feed rows appear.
3. Module ops: authority.status (read), authority.prepare (runs the PREPARING protocol
   — but note the barrier itself is TS-side per P1; the module op coordinates state +
   generation and receives the seed/verification results), authority.drain_* ops for
   the DRAINING steps, mirror.pull(domain, cursor, limit) returning feed pages.
4. Source identity: module memories/notes gain (context_store_uuid, context_row_id)
   columns with a unique index; seed upserts by source key (crash-idempotent, v3 B3).

### TS side (packages/plugin, packages/dashboard/src-tauri, packages/cli)

5. context.db: store-uuid meta row (minted at first writable open by this version);
   authority_managed marker row per project; guard TRIGGERS on memories + notes tables
   that RAISE(ABORT) when the marker is present and `mc_privileged_writer()` is false.
   P2 exactly: privilege is a connection-local scalar UDF registered default-false at
   EVERY writable opener — the plugin's shared sqlite.ts chokepoint, dashboard db.rs
   (rusqlite create_scalar_function), CLI database-access.ts. A connection that never
   registered the UDF must fail writes closed (missing-function error) — add the
   cross-backend test (bun:sqlite, node:sqlite, rusqlite) proving all three register
   and that an unregistered raw connection is rejected by the trigger.
   Notes guard scoping per P6: owned set is type='smart' AND project_path IS NOT NULL;
   INSERT judges NEW, DELETE judges OLD, UPDATE judges OLD OR NEW.
6. PREPARING barrier per P1: the TS-side prepare routine holds ONE BEGIN IMMEDIATE
   transaction on context.db through seed capture, content verification, and the
   authority-flip acknowledgment, then commits. Structure it so the module round-trips
   happen while the barrier is held (the connection holding the lock does the reads and
   sends pages; document the WAL single-writer rationale in a comment).
7. UUID reconciliation at writable open per P3: plugin, dashboard, and CLI writable
   opens read uuid + marker and (module reachable) reconcile against authority rows;
   REGRESSED (marker absent + authority row present for this uuid) = reinstall marker
   under a write barrier, writes stay closed until repair completes. For dashboard and
   CLI, module-unreachable + REGRESSED-unknown is fine: they cannot detect REGRESSED
   without the module, so they proceed as legacy TS ONLY when no marker row exists —
   document this residual honestly in the plan file under a new "build notes" heading.
8. Mirror service per U-FEED: mirror.pull consumer applying pages in one context.db
   transaction each (privileged-writer scope), durable per-domain cursor advanced only
   after commit, mirror_identity table (domain, module_project, module_row_id ->
   context_row_id) seeded with ORIGINAL context ids at seed time, fresh context ids
   only for module-born rows, reference-column translation (superseded_by_memory_id)
   both directions, stale-vector deletion in the same apply transaction when
   content_hash changed.

### Tests (fail-first where the spec names a race)

- Straddling-writer test: writer transaction begins before PREPARING barrier, commits
  after — assert the seed bound includes it (barrier waited) OR the write was rejected.
- Guard-trigger coverage: every TS write path (storage-memory, storage-notes,
  smart-notes storage, dashboard db.rs mutation commands, clearSession) against a
  marked project: privileged paths succeed, unprivileged fail closed.
- Marker + migrations: schema migrations run with privilege; clearSession keeps
  deleting session notes (TS-owned, unguarded per P6).
- Seed idempotency: crash mid-seed (kill between pages), re-run converges by source
  key, no duplicates.
- REGRESSED repair: simulate restore (delete marker, keep authority row), assert
  writes closed until repair reinstalls marker.
- Feed/trigger: mutation via every module write site produces correct snapshots;
  no-op update produces no feed row; NULL transition produces one (P7).
- mirror_identity round-trip including reference columns.

## Constraints

- Cache safety: NOTHING in this unit may change rendered m0/m1 bytes — authority and
  feed are storage-plane only. Assert: existing transform tests all pass unchanged.
- Migration discipline per STRUCTURE.md: new context.db migration (next version after
  current highest), bump LATEST_SUPPORTED_VERSION, fresh-DB schema updated,
  ensureColumn calls, clearSession additions for new session-scoped tables (the
  authority/marker/meta rows are project/store-scoped — NOT in clearSession),
  migrations-v<N>.test.ts. Module store: next migration number in mc-store.
- SQLite binds: spread positional args, never array form.
- Comments explain invariants for a future reader; never reference plan versions,
  review rounds, or pin numbers.
- Gates: full plugin suite, Pi suite, CLI suite, cargo test + clippy, dashboard
  cargo test. All green before you commit. Commit to the current branch with a
  descriptive message + Co-Authored-By: Alfonso <alfonso@cortexkit.io>.

Work in your worktree; if a question is genuinely blocking, ask it rather than
guessing — the spec is the authority and ambiguity beyond it is a stop condition.
