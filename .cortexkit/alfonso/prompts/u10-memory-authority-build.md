# Build U10: memory authority flip (TS -> module) on the U-AUTH/U-FEED substrate

Binding spec: `.alfonso/plans/rust-mc-ownership-u9-u11.md` (v5) — read it fully,
especially "U10", "v3 amendments", "v4 amendments", "v5 pins". If the file is absent
from your worktree, STOP and ask; do not improvise the spec. The foundation you build
on merged as commit cb9c8874 (authority state machine + drain ops + changefeed +
guard triggers + `withPrivilegedWriter` + `context-authority.ts` orchestrator); study
`packages/plugin/src/features/magic-context/context-authority.ts`,
`packages/plugin/src/shared/sqlite.ts` (privilege bracket), and the mc-store
migration 23 surface before writing anything.

## Scope of THIS task

U10 flips the MEMORIES domain to module authority for rust-mode projects and builds
the mirror-back loop. No U9 (dreamer) and no U11 (notes) work.

### 1. Authority flip orchestration (TS, rust-mode projects only)
- Wire `context-authority.ts` prepare/flip into rust-mode session bootstrap
  (`rust-mode-transform.ts` init path): when `transform_mode: "rust"` resolves for a
  project and the module is reachable, run the PREPARING protocol for domain
  'memories' if authority is TS. The PREPARING barrier per P1: one BEGIN IMMEDIATE on
  context.db spans seed capture + module verification round-trip + flip ack. Seed all
  memory rows for the project (status active/permanent/archived — full fidelity,
  including embeddings-relevant columns and mutation-log watermarks per the spec's
  literal column map) via authority.seed pages under the barrier.
- Content verification before flip: count + per-row content hash comparison
  (checksum_expected/actual on authority.prepare complete). Mismatch = abort prepare,
  stay TS, loud log. Never flip unverified.
- UUID reconciliation per P3 is already partially in the foundation — extend writable
  opens (plugin path) to refuse memory writes for MODULE-authority projects except
  through the module facade; the guard triggers enforce, this item is the graceful
  error surface above them (typed error naming the module route, not a raw SQLITE_ABORT).

### 2. Module-side memory write authority
- The facade memory ops (ctx_memory write/update/archive/merge paths in
  crates/mc-module) become the canonical write path for MODULE-authority projects.
  Verify every mutation appends to the mutation log in the same fenced transaction
  (existing CC-leg discipline) AND now feeds mc_changefeed via the migration-23
  triggers (should be automatic — add the coverage test for each op: write, update,
  archive, merge, promote_facts_tx).
- Visibility predicate per P5: implement `foreign_visible` ONCE as a shared SQL
  fragment constant in mc-store, used by m0 memory render, m1 delta, and mirror
  filtering. Mirror the same predicate string in a TS constant; golden test asserts
  byte-equality of the two strings.
- Per-field cache visibility doctrine (v3): importance/scope/shareable column updates
  are cache-neutral (no m1 revision bump); content/status/category changes ride the
  m1 revision signal; shareable REVOCATION on a workspace-member project additionally
  bumps the new per-project memory_visibility_epoch (v3 amendment — add the column to
  the module store, fold it into the workspace fingerprint input so revocation takes
  exactly one coordinated HARD). Fail-first test: flip shareable 1->0 on a
  workspace-shared row, assert exactly one HARD on the next pass of a member session
  and byte-stable passes after.

### 3. Mirror-back loop (module -> context.db)
- Implement the mirror consumer service in the plugin: poll mirror.pull for domain
  'memories' on the existing background cadence (dream-timer tick or the rust-mode
  postprocess hook — choose the one that cannot run concurrently with itself; document
  why), apply pages inside withPrivilegedWriter transactions, advance the durable
  cursor only after commit (foundation's mirror_identity + cursor tables).
- Apply semantics: insert/update upsert by mirror_identity translation;
  tombstone = archive-or-delete per the spec's tombstone rule; superseded_by_memory_id
  translated through mirror_identity both directions; stale embedding vectors deleted
  in the same apply transaction when content_hash changed (the embedding sweep then
  regenerates naturally).
- Dashboard/search/embeddings read the mirror — verify ctx_search memory reads and
  the embedding sweep work unchanged against mirrored rows for a MODULE-authority
  project (integration test with a real module store + seeded context.db).

### 4. Drain (MODULE -> TS) for flip-back
- Implement the DRAINING consumer using the foundation's drain ops: capture bounds,
  drain memories through the journal steps with cursors, checksum verify, flip to TS,
  guard marker removed inside the final privileged transaction. This is the U8-style
  escape hatch; test: flip a project to MODULE, mutate via facade, drain back,
  assert context.db state equals module state and TS writes work again.

### Tests (fail-first where a race is named)
- PREPARING straddle: TS writer transaction open across prepare start — barrier waits
  or rejects, seed bound includes the straddler's commit or excludes its absence.
- Verification mismatch aborts (corrupt one row hash mid-seed via test hook).
- Facade mutation -> changefeed row -> mirror apply -> context.db row, end to end
  with hash equality, including merge (multi-source) and promote_facts.
- Tombstone + vector deletion same-transaction atomicity (kill between ops must be
  impossible by construction — single transaction).
- memory_visibility_epoch revocation HARD exactness (above).
- Drain round-trip equality.
- Existing suites: full plugin, Pi, CLI, cargo workspace + clippy, dashboard cargo.
  Transform byte tests unchanged — U10 storage work must not alter rendered bytes
  except the two designed triggers (m1 revision on content change, epoch on
  revocation).

## Constraints
- SQLite binds: spread positional args only. Migration discipline per STRUCTURE.md
  (new mc-store migration number; context.db only if a new column is genuinely
  needed — prefer none). clearSession untouched (memories are project-scoped).
- Comments explain invariants; never reference plan versions, pins, or review rounds.
- Commit to your branch with Co-Authored-By: Alfonso <alfonso@cortexkit.io>.
- If the spec and the foundation code disagree anywhere, STOP and report the exact
  disagreement instead of choosing silently.
