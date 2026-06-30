//! Magic Context durable cache-state store.
//!
//! Persists the per-session `cortexkit-cache-core` [`CoreState`] plus a small
//! `module_meta` blob (`initialized`, `last_render_config`, `coverage_ordinal`).
//!
//! Concurrency: writes go through `cortexkit-store`'s epoch-fenced transaction
//! (rejects a superseded lease handover) AND an app-level `row_version` CAS inside
//! that same transaction. The epoch fence only rejects a STRICTLY-NEWER writer
//! (lease handover) — an equal-epoch writer is NOT fenced — so the row_version CAS
//! is what catches a same-epoch second writer. It is conditional: a pass writes
//! ONLY when durable state actually changed (a pure SoftPlus replay mutates
//! nothing and writes nothing), so the no-write-on-defer guarantee holds.

#![forbid(unsafe_code)]

use cortexkit_cache_core::CoreState;
use cortexkit_store::{open_sqlite, Migration, SqliteStore, StoreError};
use cortexkit_store_types::StorageDescriptor;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Migration namespace for the cache-state domain (one DB can host several
/// independent namespaces; this is ours).
const NS: &str = "mc_cache";

/// Sentinel row_version meaning "no row present" (COALESCE default inside the txn).
const NO_ROW: i64 = -1;

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        statements: "
        CREATE TABLE IF NOT EXISTS mc_cache_state (
            session_id   TEXT PRIMARY KEY,
            row_version  INTEGER NOT NULL,
            core_state   TEXT NOT NULL,
            meta         TEXT NOT NULL
        );
    ",
    },
    Migration {
        version: 2,
        // The compartment history (the m0/m1 render source). Keyed by
        // (session_id, sequence); sequence is the chronological order (1 = oldest).
        // `content` is the primary text (the P1 tier, or a legacy flat body); p1..p4
        // are the four paraphrase tiers a compartment can render at (NULL for legacy
        // rows); `importance` is the decay rate (1..100); `legacy=1` marks a pre-tier
        // flat row with no paraphrases.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_compartments (
            session_id        TEXT NOT NULL,
            sequence          INTEGER NOT NULL,
            start_message     INTEGER NOT NULL,
            end_message       INTEGER NOT NULL,
            start_message_id  TEXT NOT NULL DEFAULT '',
            end_message_id    TEXT NOT NULL DEFAULT '',
            title             TEXT NOT NULL,
            content           TEXT NOT NULL,
            p1                TEXT,
            p2                TEXT,
            p3                TEXT,
            p4                TEXT,
            importance        INTEGER NOT NULL DEFAULT 50,
            episode_type      TEXT,
            legacy            INTEGER NOT NULL DEFAULT 0,
            created_at        INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (session_id, sequence)
        );
    ",
    },
    Migration {
        version: 3,
        // Project memories — the durable knowledge rendered into the prompt baseline.
        // Uses the full original `memories` schema (every field) so the background
        // maintenance worker that owns memory upkeep can read/write all of it; the
        // prompt render only projects the subset it needs. Keyed by id;
        // UNIQUE(project_path, category, normalized_hash) dedups. `importance` orders the
        // budget trim (highest survives); `status` selects active/permanent for the
        // render (archived is ignored); `superseded_by_memory_id` records that a later
        // memory replaced this one (used when rendering memory corrections).
        statements: "
        CREATE TABLE IF NOT EXISTS mc_memories (
            id                       INTEGER PRIMARY KEY AUTOINCREMENT,
            project_path             TEXT NOT NULL,
            category                 TEXT NOT NULL,
            content                  TEXT NOT NULL,
            normalized_hash          TEXT NOT NULL,
            importance               INTEGER,
            scope                    TEXT NOT NULL DEFAULT 'project',
            shareable                INTEGER NOT NULL DEFAULT 0,
            source_session_id        TEXT,
            source_type              TEXT DEFAULT 'historian',
            seen_count               INTEGER DEFAULT 1,
            retrieval_count          INTEGER DEFAULT 0,
            first_seen_at            INTEGER NOT NULL DEFAULT 0,
            created_at               INTEGER NOT NULL DEFAULT 0,
            updated_at               INTEGER NOT NULL DEFAULT 0,
            last_seen_at             INTEGER NOT NULL DEFAULT 0,
            last_retrieved_at        INTEGER,
            status                   TEXT DEFAULT 'active',
            expires_at               INTEGER,
            verification_status      TEXT DEFAULT 'unverified',
            verified_at              INTEGER,
            classified_at            INTEGER,
            superseded_by_memory_id  INTEGER,
            merged_from              TEXT,
            metadata_json            TEXT,
            UNIQUE(project_path, category, normalized_hash)
        );
        CREATE INDEX IF NOT EXISTS idx_mc_memories_project_status
            ON mc_memories(project_path, status);
    ",
    },
    Migration {
        version: 4,
        // Records non-additive memory changes (update / archive / delete / superseded)
        // as append-only rows instead of editing the rendered memory baseline. The
        // prompt baseline is cached byte-for-byte once rendered; rather than re-rendering
        // it for every memory edit (which would invalidate the cache), these rows are
        // coalesced to one correction per target memory and sent as a small "corrections"
        // delta on top of the cached baseline. On the next full baseline re-render the
        // corrections fold in and a cursor advances past the processed rows.
        statements: "
        CREATE TABLE IF NOT EXISTS mc_memory_mutation_log (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            project_path       TEXT NOT NULL,
            mutation_type      TEXT NOT NULL
                CHECK (mutation_type IN ('archive', 'delete', 'update', 'superseded')),
            target_memory_id   INTEGER NOT NULL,
            superseded_by_id   INTEGER,
            category           TEXT,
            new_content        TEXT,
            queued_at          INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_mc_memory_mutation_log_project
            ON mc_memory_mutation_log(project_path, id);
    ",
    },
    Migration {
        version: 5,
        // User memories — the <user-profile> baseline source. GLOBAL (cross-project, no
        // project_path): durable observations about the user that apply everywhere. The
        // render reads active ones ordered promoted_at ASC, id ASC (the id tiebreaker is
        // load-bearing: promoted_at can tie at ms granularity, and a non-deterministic
        // order would drift the rendered bytes between passes).
        statements: "
        CREATE TABLE IF NOT EXISTS mc_user_memories (
            id                   INTEGER PRIMARY KEY AUTOINCREMENT,
            content              TEXT NOT NULL,
            status               TEXT NOT NULL DEFAULT 'active',
            promoted_at          INTEGER NOT NULL DEFAULT 0,
            source_candidate_ids TEXT DEFAULT '[]',
            created_at           INTEGER NOT NULL DEFAULT 0,
            updated_at           INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_mc_user_memories_status
            ON mc_user_memories(status);
    ",
    },
    Migration {
        version: 6,
        // Cross-project workspaces. A project belongs to at most one workspace (the
        // UNIQUE index on project_path). A member session reads the UNION of members'
        // memories, but a FOREIGN member's memories are visible only in the workspace's
        // shared categories (share_categories); the owning project always sees all its
        // own. share_categories is a JSON array (default ["CONSTRAINTS"]).
        statements: "
        CREATE TABLE IF NOT EXISTS mc_workspaces (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            name             TEXT NOT NULL UNIQUE,
            created_at       INTEGER NOT NULL DEFAULT 0,
            updated_at       INTEGER NOT NULL DEFAULT 0,
            share_categories TEXT NOT NULL DEFAULT '[\"CONSTRAINTS\"]'
        );
        CREATE TABLE IF NOT EXISTS mc_workspace_members (
            workspace_id  INTEGER NOT NULL REFERENCES mc_workspaces(id) ON DELETE CASCADE,
            project_path  TEXT NOT NULL,
            display_name  TEXT NOT NULL,
            display_path  TEXT NOT NULL,
            added_at      INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (workspace_id, project_path)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_mc_workspace_member_unique
            ON mc_workspace_members(project_path);
    ",
    },
];

/// A project's workspace membership: the union of member identities it reads, which of
/// them are its OWN (full visibility) vs FOREIGN (visible only in `share_categories`),
/// the shared-category allow-list, and per-foreign-member display attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMembership {
    /// All member project_paths (own + foreign), the union read set.
    pub union_identities: Vec<String>,
    /// The calling project's own identity (full-visibility).
    pub own_identity: String,
    /// Categories in which FOREIGN members' memories are shared into this project.
    pub share_categories: Vec<String>,
    /// project_path → display_name, for repo-attributing a foreign memory on render.
    pub display_name_by_path: std::collections::HashMap<String, String>,
}

/// A memory mutation-log entry. `update` is non-terminal (the memory is still present
/// with new content → renders `<updated>`); `archive`/`delete`/`superseded` are TERMINAL
/// (the memory left the active set → renders `<removed>`/`<superseded>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredMemoryMutation {
    pub id: i64,
    pub mutation_type: String,
    pub target_memory_id: i64,
    pub superseded_by_id: Option<i64>,
    pub category: Option<String>,
    pub new_content: Option<String>,
    pub queued_at: i64,
}

impl StoredMemoryMutation {
    fn is_terminal(&self) -> bool {
        matches!(
            self.mutation_type.as_str(),
            "archive" | "delete" | "superseded"
        )
    }
}

/// The non-CoreState durable blob: bootstrap + epoch-detection + coverage watermark.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleMeta {
    /// A baseline has been materialized at least once. Gates the bootstrap-Hard rule.
    pub initialized: bool,
    /// The render-config fingerprint as of the last Hard fold; an incoming pass whose
    /// fingerprint differs is an epoch change → Hard.
    pub last_render_config: String,
    /// The terminal covered ordinal as of the last baseline. Monotonic-absolute,
    /// never positional; can DECREASE on a revert-Hard.
    pub coverage_ordinal: Option<u64>,
    /// The content-digest revision the frozen m1 block was last rendered from. The
    /// classifier compares the incoming m1 content's revision against this to decide
    /// whether an m1 delta rides (Soft) WITHOUT rendering. 0 = placeholder (no delta).
    /// `serde(default)` so meta JSON persisted before this field loads cleanly.
    #[serde(default)]
    pub m1_revision: u64,

    // --- slice 4d-m0: the two-watermark coverage model + memory manifest ---
    // (all serde(default) so pre-4d meta JSON loads cleanly)
    /// The highest compartment `sequence` folded INTO m0. The "in-m0 vs riding-m1"
    /// divider — advances ONLY on a HARD fold. The m1 renderer treats compartments with
    /// `sequence > folded_compartment_seq` as new (renders them at P1); the HARD folds
    /// them and advances this. Distinct from `coverage_ordinal` (the m0+m1 coverage end /
    /// tail-trim point, which advances on a coverage-extending SOFT too).
    #[serde(default)]
    pub folded_compartment_seq: i64,
    /// The manifest of memory ids actually rendered into the frozen m0 (post-budget-trim).
    /// The supersede router uses membership here (NOT id<=max_memory_id, since a trim
    /// drops low-importance memories) to decide whether a memory UPDATE rides m1 as a
    /// `<memory-updates>` correction. Persisted atomically with the m0 bytes.
    #[serde(default)]
    pub rendered_memory_ids: Vec<i64>,
    /// The memory-mutation-log id folded as of the last HARD. m1 renders corrections with
    /// `id > memory_mutation_cursor`; a HARD reconciles them into m0 and advances this.
    #[serde(default)]
    pub memory_mutation_cursor: i64,
    /// The highest memory id folded into m0. m1 renders memories with `id > max_memory_id`
    /// as `<new-memories>`; a HARD folds them and advances this.
    #[serde(default)]
    pub max_memory_id: i64,
}

/// A stored compartment row (the m0/m1 history source). `sequence` is the
/// chronological order (1 = oldest).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredCompartment {
    pub sequence: i64,
    pub start_message: i64,
    pub end_message: i64,
    pub start_message_id: String,
    pub end_message_id: String,
    pub title: String,
    /// v2 P1 text, or the flat legacy body. Always present.
    pub content: String,
    /// v2 paraphrase tiers; None for legacy rows.
    pub p1: Option<String>,
    pub p2: Option<String>,
    pub p3: Option<String>,
    pub p4: Option<String>,
    /// Decay rate (1..100), defaults to 50.
    pub importance: i32,
    pub episode_type: Option<String>,
    /// 1 = pre-v2 flat compartment, 0 = v2 tiered.
    pub legacy: i32,
    pub created_at: i64,
}

/// A project memory row projected for rendering into the prompt. The store keeps the
/// full original schema; this struct carries only the columns the render, the budget
/// trim (drop lowest-importance when over budget), and the supersede/correction logic
/// actually read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredMemory {
    pub id: i64,
    pub category: String,
    pub content: String,
    /// Decay-rate / budget-trim ordering signal (1..100); None when unclassified.
    pub importance: Option<i32>,
    /// "active" | "permanent" | "archived" — the render set is active+permanent.
    pub status: String,
    pub expires_at: Option<i64>,
    /// Set when a later memory has replaced this one; consulted when rendering the list
    /// of memory corrections (a superseded memory renders as "X → Y").
    pub superseded_by_memory_id: Option<i64>,
    pub updated_at: i64,
}

/// A loaded per-session row: the core state, the meta blob, and the CAS token.
#[derive(Debug, Clone)]
pub struct LoadedState {
    pub core: CoreState,
    pub meta: ModuleMeta,
    /// The row_version read from disk; pass it back to [`McStore::commit`] as the CAS
    /// expectation. `None` when no row existed yet (first bootstrap → INSERT path).
    pub row_version: Option<u64>,
}

/// CAS / serialization errors layered over `cortexkit-store`.
#[derive(Debug)]
pub enum McStoreError {
    Store(StoreError),
    /// The on-disk row_version moved under us (a concurrent writer committed first).
    /// The caller re-loads and re-steps.
    CasConflict {
        expected: Option<u64>,
        found: u64,
    },
    Serde(String),
}

impl std::fmt::Display for McStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McStoreError::Store(e) => write!(f, "store: {e}"),
            McStoreError::CasConflict { expected, found } => {
                write!(f, "cas conflict: expected {expected:?}, found {found}")
            }
            McStoreError::Serde(e) => write!(f, "serde: {e}"),
        }
    }
}
impl std::error::Error for McStoreError {}
impl From<StoreError> for McStoreError {
    fn from(e: StoreError) -> Self {
        McStoreError::Store(e)
    }
}

/// Outcome of the fenced commit txn: either the new row_version, or a CAS conflict
/// carrying the version observed on disk. Modeled as a return value (not an error)
/// so a conflicting pass commits an empty txn and the caller re-loads cleanly.
enum CommitOutcome {
    Committed(u64),
    CasConflict(u64),
}

/// The Magic Context cache-state store: one single-writer SQLite handle for the
/// module's lifetime.
pub struct McStore {
    inner: SqliteStore,
}

impl McStore {
    /// Open from a resolved descriptor (acquires the single-writer lease) and apply
    /// the cache-state migration chain. Open exactly ONCE per module lifetime.
    pub fn open(descriptor: &StorageDescriptor) -> Result<Self, McStoreError> {
        let inner = open_sqlite(descriptor)?;
        inner.migrate(NS, MIGRATIONS)?;
        Ok(McStore { inner })
    }

    /// Load a session's persisted state. Returns defaults (uninitialized, no row)
    /// when the session has never been seen — the classifier then bootstraps.
    pub fn load(&self, session_id: &str) -> Result<LoadedState, McStoreError> {
        let row = self.inner.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT row_version, core_state, meta FROM mc_cache_state WHERE session_id = ?1",
                    params![session_id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)? as u64,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .ok())
        })?;

        match row {
            None => Ok(LoadedState {
                core: CoreState::default(),
                meta: ModuleMeta::default(),
                row_version: None,
            }),
            Some((rv, core_json, meta_json)) => Ok(LoadedState {
                core: serde_json::from_str(&core_json)
                    .map_err(|e| McStoreError::Serde(e.to_string()))?,
                meta: serde_json::from_str(&meta_json)
                    .map_err(|e| McStoreError::Serde(e.to_string()))?,
                row_version: Some(rv),
            }),
        }
    }

    /// Commit new state under the row_version CAS, inside the epoch-fenced txn.
    ///
    /// `expected` is the row_version from [`load`] (`None` = expect no row → INSERT).
    /// On success the row_version is bumped by one. A `CasConflict` means a
    /// concurrent writer won; the caller re-loads and re-steps. Call ONLY when
    /// durable state changed — a pure SoftPlus replay must skip the commit entirely
    /// so a defer pass performs no write.
    pub fn commit(
        &self,
        session_id: &str,
        expected: Option<u64>,
        core: &CoreState,
        meta: &ModuleMeta,
    ) -> Result<u64, McStoreError> {
        let core_json =
            serde_json::to_string(core).map_err(|e| McStoreError::Serde(e.to_string()))?;
        let meta_json =
            serde_json::to_string(meta).map_err(|e| McStoreError::Serde(e.to_string()))?;
        let next = expected.unwrap_or(0) + 1;

        let outcome = self.inner.with_conn_fenced(|tx| {
            // Read the current row_version inside the fenced txn; NO_ROW when absent.
            let current: i64 = tx.query_row(
                "SELECT COALESCE((SELECT row_version FROM mc_cache_state WHERE session_id = ?1), ?2)",
                params![session_id, NO_ROW],
                |r| r.get(0),
            )?;

            let cas_ok = match expected {
                None => current == NO_ROW,
                Some(v) => current == v as i64,
            };
            if !cas_ok {
                // Empty txn (commits nothing); the caller re-loads and re-steps.
                return Ok(CommitOutcome::CasConflict(current.max(0) as u64));
            }

            // INSERT-or-UPDATE in the same fenced txn (bootstrap has no row to UPDATE).
            tx.execute(
                "INSERT INTO mc_cache_state (session_id, row_version, core_state, meta)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id) DO UPDATE SET
                     row_version = excluded.row_version,
                     core_state  = excluded.core_state,
                     meta        = excluded.meta",
                params![session_id, next as i64, core_json, meta_json],
            )?;
            Ok(CommitOutcome::Committed(next))
        })?;

        match outcome {
            CommitOutcome::Committed(v) => Ok(v),
            CommitOutcome::CasConflict(found) => Err(McStoreError::CasConflict { expected, found }),
        }
    }

    /// Read a session's compartments in chronological order (oldest first), the order
    /// the decay renderer expects (it indexes from newest internally).
    pub fn load_compartments(
        &self,
        session_id: &str,
    ) -> Result<Vec<StoredCompartment>, McStoreError> {
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT sequence, start_message, end_message, start_message_id, end_message_id,
                        title, content, p1, p2, p3, p4, importance, episode_type, legacy, created_at
                 FROM mc_compartments WHERE session_id = ?1 ORDER BY sequence ASC",
            )?;
            let mapped = stmt
                .query_map(params![session_id], |r| {
                    Ok(StoredCompartment {
                        sequence: r.get(0)?,
                        start_message: r.get(1)?,
                        end_message: r.get(2)?,
                        start_message_id: r.get(3)?,
                        end_message_id: r.get(4)?,
                        title: r.get(5)?,
                        content: r.get(6)?,
                        p1: r.get(7)?,
                        p2: r.get(8)?,
                        p3: r.get(9)?,
                        p4: r.get(10)?,
                        importance: r.get::<_, Option<i64>>(11)?.unwrap_or(50) as i32,
                        episode_type: r.get(12)?,
                        legacy: r.get::<_, Option<i64>>(13)?.unwrap_or(0) as i32,
                        created_at: r.get(14)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// Replace a session's entire compartment set in one fenced transaction. The
    /// history producer republishes the full chronological set each time, so a
    /// wholesale delete-then-insert (rather than an incremental upsert) keeps the
    /// stored `sequence` contiguous. Writes are serialized by the store's single-writer
    /// lease (the same one guarding the cache-state commit).
    pub fn replace_compartments(
        &self,
        session_id: &str,
        compartments: &[StoredCompartment],
    ) -> Result<(), McStoreError> {
        self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "DELETE FROM mc_compartments WHERE session_id = ?1",
                params![session_id],
            )?;
            for c in compartments {
                tx.execute(
                    "INSERT INTO mc_compartments
                       (session_id, sequence, start_message, end_message, start_message_id,
                        end_message_id, title, content, p1, p2, p3, p4, importance,
                        episode_type, legacy, created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                    params![
                        session_id,
                        c.sequence,
                        c.start_message,
                        c.end_message,
                        c.start_message_id,
                        c.end_message_id,
                        c.title,
                        c.content,
                        c.p1,
                        c.p2,
                        c.p3,
                        c.p4,
                        c.importance as i64,
                        c.episode_type,
                        c.legacy as i64,
                        c.created_at,
                    ],
                )?;
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Load a project's render-eligible memories: `active` + `permanent`, excluding
    /// expired ones (an `expires_at` at/before `now_ms`, NULL = never expires), ordered
    /// by importance descending then id ascending (the budget-trim order — highest
    /// importance survives a trim; id breaks ties deterministically). The expiry cutoff
    /// is supplied by the caller, NOT read from the live clock, so the full render and
    /// every later byte-identical replay of it observe the SAME memory set — a live
    /// clock would expire a memory mid-replay and silently change the rendered bytes.
    pub fn load_active_memories(
        &self,
        project_path: &str,
        now_ms: i64,
    ) -> Result<Vec<StoredMemory>, McStoreError> {
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, category, content, importance, status, expires_at,
                        superseded_by_memory_id, updated_at
                 FROM mc_memories
                 WHERE project_path = ?1
                   AND status IN ('active', 'permanent')
                   AND (expires_at IS NULL OR expires_at > ?2)
                 ORDER BY COALESCE(importance, 50) DESC, id ASC",
            )?;
            let mapped = stmt
                .query_map(params![project_path, now_ms], |r| {
                    Ok(StoredMemory {
                        id: r.get(0)?,
                        category: r.get(1)?,
                        content: r.get(2)?,
                        importance: r.get(3)?,
                        status: r.get(4)?,
                        expires_at: r.get(5)?,
                        superseded_by_memory_id: r.get(6)?,
                        updated_at: r.get(7)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// The coalesced memory corrections to render as the delta, across one OR MORE
    /// project identities (the workspace union — a single-project session passes a
    /// 1-element slice). Mutation-log rows with `id > after_id`, from any of
    /// `project_paths`, whose `target_memory_id` is in `rendered_memory_ids` (the exact
    /// set of memories included in the last rendered baseline). The manifest membership
    /// IS the share filter: a foreign non-shared memory never entered the baseline, so
    /// it's not in `rendered_memory_ids`, so its mutation can't supersede here — no extra
    /// category check needed. The manifest test (NOT an `id <= last_id` test) is required
    /// because budget-trim can drop a low-importance in-range memory.
    ///
    /// Coalesced to ONE correction per target, deterministic latest-wins with terminal
    /// precedence: a terminal mutation (archive/delete/superseded) always outranks a
    /// later `update`, so an update queued after an archive can't resurrect a memory that
    /// already left the set. Sorted by id for a stable render order. Coalescing by
    /// target_id is union-safe (a memory id is unique across the store).
    pub fn memory_mutations_for_render(
        &self,
        project_paths: &[String],
        after_id: i64,
        rendered_memory_ids: &[i64],
    ) -> Result<Vec<StoredMemoryMutation>, McStoreError> {
        if rendered_memory_ids.is_empty() || project_paths.is_empty() {
            return Ok(Vec::new());
        }
        // dedup + sort the id set for a stable IN-clause.
        let mut ids: Vec<i64> = rendered_memory_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let id_ph = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        // dedup the project identities for a stable IN-clause.
        let mut projects: Vec<String> = project_paths.to_vec();
        projects.sort_unstable();
        projects.dedup();
        let proj_ph = std::iter::repeat_n("?", projects.len())
            .collect::<Vec<_>>()
            .join(", ");

        let rows = self.inner.with_conn(|conn| {
            let sql = format!(
                "SELECT id, mutation_type, target_memory_id, superseded_by_id, category,
                        new_content, queued_at
                 FROM mc_memory_mutation_log
                 WHERE project_path IN ({proj_ph}) AND id > ? AND target_memory_id IN ({id_ph})
                 ORDER BY id ASC"
            );
            let mut stmt = conn.prepare(&sql)?;
            // bind: the project set, then after_id, then the id set (matching SQL order).
            let mut binds: Vec<rusqlite::types::Value> = projects
                .iter()
                .map(|p| rusqlite::types::Value::from(p.clone()))
                .collect();
            binds.push(rusqlite::types::Value::from(after_id));
            binds.extend(ids.iter().map(|&i| rusqlite::types::Value::from(i)));
            let mapped = stmt
                .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                    Ok(StoredMemoryMutation {
                        id: r.get(0)?,
                        mutation_type: r.get(1)?,
                        target_memory_id: r.get(2)?,
                        superseded_by_id: r.get(3)?,
                        category: r.get(4)?,
                        new_content: r.get(5)?,
                        queued_at: r.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;

        Ok(coalesce_mutations(rows))
    }

    /// Resolve a project's workspace membership: the union of member identities it
    /// reads, the share-category allow-list, and per-foreign-member display attribution.
    /// Returns None when the project is in no workspace (the single-project fast path —
    /// the caller reads only its own memories). A project is in at most one workspace
    /// (the UNIQUE index on project_path).
    pub fn resolve_workspace_membership(
        &self,
        project_path: &str,
    ) -> Result<Option<WorkspaceMembership>, McStoreError> {
        let membership = self.inner.with_conn(|conn| {
            // which workspace (if any) does this project belong to?
            let ws: Option<(i64, String)> = conn
                .query_row(
                    "SELECT w.id, w.share_categories
                       FROM mc_workspace_members m
                       JOIN mc_workspaces w ON w.id = m.workspace_id
                      WHERE m.project_path = ?1",
                    params![project_path],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((workspace_id, share_categories_json)) = ws else {
                return Ok(None);
            };

            let mut stmt = conn.prepare(
                "SELECT project_path, display_name FROM mc_workspace_members
                  WHERE workspace_id = ?1 ORDER BY project_path ASC",
            )?;
            let members: Vec<(String, String)> = stmt
                .query_map(params![workspace_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;

            let union_identities: Vec<String> = members.iter().map(|(p, _)| p.clone()).collect();
            let display_name_by_path = members.into_iter().collect();
            let share_categories: Vec<String> =
                serde_json::from_str(&share_categories_json).unwrap_or_default();

            Ok(Some(WorkspaceMembership {
                union_identities,
                own_identity: project_path.to_string(),
                share_categories,
                display_name_by_path,
            }))
        })?;
        Ok(membership)
    }

    /// A DETERMINISTIC fingerprint of the project's workspace membership + share policy.
    /// The cache layer treats a change in this value as a baseline re-render (a HARD fold):
    /// a membership/policy change re-composes m0 over a DIFFERENT project set, so a stale
    /// fingerprint can't be tolerated the way stale content can. MUST be canonical: members
    /// sorted by `project_path`, the share-category list sorted, each field length-prefixed
    /// so no value forges a boundary. A nondeterministic fingerprint over a STABLE
    /// workspace would false-HARD every pass (the over-bust the m0/m1 split exists to
    /// avoid). Empty string when the project is in no workspace (the single-project state —
    /// a stable "no workspace" marker). NOTE: in this slice the fingerprint covers
    /// membership + share-policy only; production also folds each member's project-memory
    /// epoch, which has no `mc_*` source yet (it lands with the deferred
    /// `project_memory_epoch` marker when the write paths relocate).
    pub fn workspace_fingerprint(&self, project_path: &str) -> Result<String, McStoreError> {
        let Some(m) = self.resolve_workspace_membership(project_path)? else {
            return Ok(String::new());
        };
        // resolve_workspace_membership returns members sorted by project_path; sort the
        // share categories too so the policy axis is order-independent.
        let mut shared = m.share_categories.clone();
        shared.sort_unstable();
        let mut out = String::from("ws[");
        for id in &m.union_identities {
            out.push_str(&format!("m:{}:{};", id.len(), id));
        }
        out.push_str("|share:");
        for cat in &shared {
            out.push_str(&format!("{}:{};", cat.len(), cat));
        }
        out.push(']');
        Ok(out)
    }

    /// Load render-eligible memories across a workspace UNION: every member's `active` +
    /// `permanent` non-expired memories, but a FOREIGN member's only in the shared
    /// categories (`share_categories`); the OWN project sees all its own. Ordered by
    /// importance desc then id asc (the same order budget-trimming uses — highest
    /// importance survives a trim, id breaks ties). The own-vs-foreign-by-category
    /// filter is the security boundary — a foreign memory outside the shared categories
    /// must never render here. `now_ms` is the frozen expiry cutoff (see
    /// [`Self::load_active_memories`]).
    pub fn load_workspace_union_memories(
        &self,
        membership: &WorkspaceMembership,
        now_ms: i64,
    ) -> Result<Vec<StoredMemory>, McStoreError> {
        let WorkspaceMembership {
            union_identities,
            own_identity,
            share_categories,
            ..
        } = membership;
        if union_identities.is_empty() {
            return Ok(Vec::new());
        }
        // own predicate (full visibility) OR foreign predicate (shared categories only).
        let foreign: Vec<&String> = union_identities
            .iter()
            .filter(|p| *p != own_identity)
            .collect();

        let mut binds: Vec<rusqlite::types::Value> = Vec::new();
        let mut sharing = String::from("project_path IN (?)");
        binds.push(rusqlite::types::Value::from(own_identity.clone()));
        if !foreign.is_empty() && !share_categories.is_empty() {
            let fph = std::iter::repeat_n("?", foreign.len())
                .collect::<Vec<_>>()
                .join(", ");
            let cph = std::iter::repeat_n("?", share_categories.len())
                .collect::<Vec<_>>()
                .join(", ");
            sharing.push_str(&format!(
                " OR (project_path IN ({fph}) AND category IN ({cph}))"
            ));
            for p in &foreign {
                binds.push(rusqlite::types::Value::from((*p).clone()));
            }
            for c in share_categories {
                binds.push(rusqlite::types::Value::from(c.clone()));
            }
        }

        let rows = self.inner.with_conn(|conn| {
            let sql = format!(
                "SELECT id, category, content, importance, status, expires_at,
                        superseded_by_memory_id, updated_at
                 FROM mc_memories
                 WHERE ({sharing})
                   AND status IN ('active', 'permanent')
                   AND (expires_at IS NULL OR expires_at > ?)
                 ORDER BY COALESCE(importance, 50) DESC, id ASC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut all_binds = binds.clone();
            all_binds.push(rusqlite::types::Value::from(now_ms));
            let mapped = stmt
                .query_map(rusqlite::params_from_iter(all_binds.iter()), |r| {
                    Ok(StoredMemory {
                        id: r.get(0)?,
                        category: r.get(1)?,
                        content: r.get(2)?,
                        importance: r.get(3)?,
                        status: r.get(4)?,
                        expires_at: r.get(5)?,
                        superseded_by_memory_id: r.get(6)?,
                        updated_at: r.get(7)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// Load active user-memory contents (the `<user-profile>` baseline source), ordered
    /// `promoted_at ASC, id ASC`. The id tiebreaker is load-bearing: `promoted_at` can
    /// tie at ms granularity and a non-deterministic order would drift the rendered
    /// bytes between passes. Returns just the contents (the render is `- <content>`).
    pub fn load_active_user_memories(&self) -> Result<Vec<String>, McStoreError> {
        let rows = self.inner.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT content FROM mc_user_memories WHERE status = 'active'
                 ORDER BY promoted_at ASC, id ASC",
            )?;
            let mapped = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })?;
        Ok(rows)
    }

    /// The highest memory-mutation-log id across the given project identities (the union,
    /// or a single-element slice). The cursor a baseline re-render (HARD) folds the
    /// corrections up to, and the watermark a delta pass (SOFT) reads new corrections
    /// past. 0 when the log is empty. Union-scoped to match
    /// [`Self::memory_mutations_for_render`].
    pub fn max_memory_mutation_id(&self, project_paths: &[String]) -> Result<i64, McStoreError> {
        if project_paths.is_empty() {
            return Ok(0);
        }
        let mut projects: Vec<String> = project_paths.to_vec();
        projects.sort_unstable();
        projects.dedup();
        let ph = std::iter::repeat_n("?", projects.len())
            .collect::<Vec<_>>()
            .join(", ");
        let max = self.inner.with_conn(|conn| {
            let sql = format!(
                "SELECT COALESCE(MAX(id), 0) FROM mc_memory_mutation_log
                 WHERE project_path IN ({ph})"
            );
            let v: i64 =
                conn.query_row(&sql, rusqlite::params_from_iter(projects.iter()), |r| {
                    r.get(0)
                })?;
            Ok(v)
        })?;
        Ok(max)
    }

    /// The highest memory id across the given project identities (the union, or a
    /// single-element slice). A baseline re-render (HARD) folds memories up to this; a
    /// delta pass (SOFT) renders memories with `id > max_memory_id` as `<new-memories>`.
    /// 0 when there are no memories.
    pub fn max_memory_id(&self, project_paths: &[String]) -> Result<i64, McStoreError> {
        if project_paths.is_empty() {
            return Ok(0);
        }
        let mut projects: Vec<String> = project_paths.to_vec();
        projects.sort_unstable();
        projects.dedup();
        let ph = std::iter::repeat_n("?", projects.len())
            .collect::<Vec<_>>()
            .join(", ");
        let max = self.inner.with_conn(|conn| {
            let sql = format!(
                "SELECT COALESCE(MAX(id), 0) FROM mc_memories WHERE project_path IN ({ph})"
            );
            let v: i64 =
                conn.query_row(&sql, rusqlite::params_from_iter(projects.iter()), |r| {
                    r.get(0)
                })?;
            Ok(v)
        })?;
        Ok(max)
    }
}

/// Coalesce mutation-log rows to one per target memory: deterministic latest-wins with
/// TERMINAL precedence (terminal always outranks a non-terminal `update`, regardless of
/// id order). Among rows of the same terminality, the later id wins. Sorted by id for a
/// stable render order.
fn coalesce_mutations(rows: Vec<StoredMemoryMutation>) -> Vec<StoredMemoryMutation> {
    use std::collections::HashMap;
    let mut chosen: HashMap<i64, StoredMemoryMutation> = HashMap::new();
    // rows arrive id-ASC; iterate in that order so "later id wins" = last-write-wins
    // for same-terminality, and the terminal-precedence guard handles the rest.
    for candidate in rows {
        match chosen.get(&candidate.target_memory_id) {
            None => {
                chosen.insert(candidate.target_memory_id, candidate);
            }
            Some(current) => {
                let current_terminal = current.is_terminal();
                let candidate_terminal = candidate.is_terminal();
                if current_terminal && !candidate_terminal {
                    // keep the terminal; a later update can't resurrect it.
                    continue;
                }
                // candidate-terminal-over-current-nonterminal, OR same-terminality
                // later-id → candidate wins.
                chosen.insert(candidate.target_memory_id, candidate);
            }
        }
    }
    let mut out: Vec<StoredMemoryMutation> = chosen.into_values().collect();
    out.sort_by_key(|m| m.id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend};

    fn descriptor(dir: &std::path::Path) -> StorageDescriptor {
        StorageDescriptor {
            module_id: "magic-context-test".to_string(),
            storage_namespace: "mc_cache".to_string(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.join("store.db").to_string_lossy().to_string(),
            },
        }
    }

    #[test]
    fn bootstrap_load_returns_uninitialized_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let loaded = store.load("ses_a").unwrap();
        assert!(!loaded.meta.initialized);
        assert_eq!(loaded.row_version, None);
        assert_eq!(loaded.core, CoreState::default());
    }

    #[test]
    fn commit_then_load_roundtrips_and_bumps_row_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();

        let core = CoreState {
            boundary_id: "b1".into(),
            ..Default::default()
        };
        let meta = ModuleMeta {
            initialized: true,
            last_render_config: "cfg1".into(),
            coverage_ordinal: Some(42),
            m1_revision: 0,
            ..Default::default()
        };

        let v1 = store.commit("ses_a", None, &core, &meta).unwrap();
        assert_eq!(v1, 1);

        let loaded = store.load("ses_a").unwrap();
        assert_eq!(loaded.row_version, Some(1));
        assert_eq!(loaded.core.boundary_id, "b1");
        assert_eq!(loaded.meta, meta);

        let v2 = store.commit("ses_a", Some(1), &core, &meta).unwrap();
        assert_eq!(v2, 2);
    }

    #[test]
    fn stale_cas_expectation_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let core = CoreState::default();
        let meta = ModuleMeta::default();

        store.commit("ses_a", None, &core, &meta).unwrap(); // row_version now 1
                                                            // A writer that still thinks the row is absent must conflict.
        let err = store.commit("ses_a", None, &core, &meta).unwrap_err();
        match err {
            McStoreError::CasConflict { expected, found } => {
                assert_eq!(expected, None);
                assert_eq!(found, 1);
            }
            other => panic!("expected CasConflict, got {other}"),
        }
    }

    #[test]
    fn double_open_same_path_is_rejected_by_lease() {
        let dir = tempfile::tempdir().unwrap();
        let d = descriptor(dir.path());
        let _first = McStore::open(&d).unwrap();
        // Second live handle on the same database must be rejected (single-writer).
        assert!(McStore::open(&d).is_err());
    }

    #[test]
    fn compartments_roundtrip_chronological_with_tiers_and_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        assert!(store.load_compartments("ses_a").unwrap().is_empty());

        let comps = vec![
            StoredCompartment {
                sequence: 1,
                start_message: 1,
                end_message: 9,
                title: "oldest legacy".into(),
                content: "U: flat body".into(),
                legacy: 1,
                importance: 50,
                created_at: 100,
                ..Default::default()
            },
            StoredCompartment {
                sequence: 2,
                start_message: 10,
                end_message: 19,
                title: "v2 row".into(),
                content: "P1 full".into(),
                p1: Some("P1 full".into()),
                p2: Some("P2 dense".into()),
                p3: Some("P3".into()),
                p4: None,
                importance: 80,
                episode_type: Some("design,feature".into()),
                legacy: 0,
                created_at: 200,
                ..Default::default()
            },
        ];
        store.replace_compartments("ses_a", &comps).unwrap();

        let read = store.load_compartments("ses_a").unwrap();
        assert_eq!(
            read, comps,
            "chronological round-trip incl NULL p4 + tiers + legacy"
        );
        assert_eq!(read[0].sequence, 1, "oldest first");

        // a wholesale replace fully supplants the prior set
        let replacement = vec![StoredCompartment {
            sequence: 1,
            title: "only".into(),
            content: "x".into(),
            importance: 50,
            ..Default::default()
        }];
        store.replace_compartments("ses_a", &replacement).unwrap();
        let read2 = store.load_compartments("ses_a").unwrap();
        assert_eq!(read2.len(), 1);
        assert_eq!(read2[0].title, "only");

        // distinct sessions are isolated
        assert!(store.load_compartments("ses_b").unwrap().is_empty());
    }

    fn insert_memory(
        store: &McStore,
        project: &str,
        id: i64,
        content: &str,
        importance: Option<i32>,
        status: &str,
        expires_at: Option<i64>,
    ) {
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_memories
                       (id, project_path, category, content, normalized_hash, importance,
                        status, expires_at, first_seen_at, created_at, updated_at, last_seen_at)
                     VALUES (?1,?2,'ARCHITECTURE',?3,?4,?5,?6,?7,0,0,0,0)",
                    params![
                        id,
                        project,
                        content,
                        format!("h{id}"),
                        importance,
                        status,
                        expires_at
                    ],
                )?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn active_memories_filter_order_and_frozen_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let proj = "git:proj";

        insert_memory(&store, proj, 1, "low active", Some(20), "active", None);
        insert_memory(&store, proj, 2, "high active", Some(90), "active", None);
        insert_memory(&store, proj, 3, "permanent", Some(50), "permanent", None);
        insert_memory(&store, proj, 4, "archived", Some(99), "archived", None); // excluded
        insert_memory(&store, proj, 5, "expired", Some(99), "active", Some(1000)); // expires at 1000
        insert_memory(&store, proj, 6, "other proj", Some(99), "active", None);
        // (re-key the last under a different project)
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "UPDATE mc_memories SET project_path = 'git:other' WHERE id = 6",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        // cutoff AFTER the expiry → memory 5 excluded; archived + other-project excluded;
        // ordered importance desc (90, 50, 20).
        let read = store.load_active_memories(proj, 2000).unwrap();
        let ids: Vec<i64> = read.iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            vec![2, 3, 1],
            "active+permanent, expired excluded, importance desc"
        );

        // a cutoff BEFORE the expiry keeps memory 5 (frozen-cutoff determinism: the
        // caller controls the cutoff, not a live clock).
        let read_early = store.load_active_memories(proj, 500).unwrap();
        assert!(
            read_early.iter().any(|m| m.id == 5),
            "not-yet-expired at the earlier cutoff"
        );
    }

    fn log_mutation(store: &McStore, project: &str, kind: &str, target: i64, content: &str) {
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_memory_mutation_log
                       (project_path, mutation_type, target_memory_id, new_content, queued_at)
                     VALUES (?1, ?2, ?3, ?4, 0)",
                    params![project, kind, target, content],
                )?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn mutation_render_coalesces_latest_wins_with_terminal_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let proj = "git:proj";

        // memory 10: two updates → latest-wins (single terminal correction).
        log_mutation(&store, proj, "update", 10, "v1"); // id 1
        log_mutation(&store, proj, "update", 10, "v2"); // id 2 (newer wins)
                                                        // memory 20: an archive then a later update → terminal (archive) outranks update.
        log_mutation(&store, proj, "archive", 20, ""); // id 3 terminal
        log_mutation(&store, proj, "update", 20, "resurrect?"); // id 4 must NOT win
                                                                // memory 30: in the log but NOT in the rendered manifest → excluded.
        log_mutation(&store, proj, "update", 30, "off-m0");

        let rendered = [10i64, 20];
        let projects = vec![proj.to_string()];
        let out = store
            .memory_mutations_for_render(&projects, 0, &rendered)
            .unwrap();

        assert_eq!(out.len(), 2, "one coalesced row per in-manifest target");
        let m10 = out.iter().find(|m| m.target_memory_id == 10).unwrap();
        assert_eq!(m10.new_content.as_deref(), Some("v2"), "latest update wins");
        let m20 = out.iter().find(|m| m.target_memory_id == 20).unwrap();
        assert_eq!(
            m20.mutation_type, "archive",
            "terminal outranks a later update"
        );
        assert!(
            !out.iter().any(|m| m.target_memory_id == 30),
            "off-manifest excluded"
        );

        // afterId cursor past id 2 → memory 10's updates fold out; 20's archive renders.
        let after = store
            .memory_mutations_for_render(&projects, 2, &rendered)
            .unwrap();
        assert!(
            !after.iter().any(|m| m.target_memory_id == 10),
            "folded updates excluded by cursor"
        );
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].target_memory_id, 20);

        // empty manifest → no corrections (nothing in m0 to correct).
        assert!(store
            .memory_mutations_for_render(&projects, 0, &[])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn workspace_fingerprint_is_deterministic_and_membership_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:own";
        let foreign = "git:foreign";

        // a project in NO workspace → stable empty marker
        assert_eq!(store.workspace_fingerprint(own).unwrap(), "");

        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_workspaces (id, name, share_categories) VALUES (1,'ws','[\"CONSTRAINTS\",\"ARCHITECTURE\"]')",
                    [],
                )?;
                // insert members in NON-sorted order to prove the fingerprint canonicalizes
                tx.execute(
                    "INSERT INTO mc_workspace_members (workspace_id, project_path, display_name, display_path, added_at)
                     VALUES (1, ?1, 'foreign', '/f', 0), (1, ?2, 'own', '/o', 0)",
                    params![foreign, own],
                )?;
                Ok(())
            })
            .unwrap();

        // same membership → byte-identical across repeated reads (stability for an
        // unchanged workspace, so it never forces a needless re-render)
        let fp1 = store.workspace_fingerprint(own).unwrap();
        let fp2 = store.workspace_fingerprint(own).unwrap();
        assert_eq!(fp1, fp2, "stable workspace → stable fingerprint");
        assert!(!fp1.is_empty());
        // both members appear; the foreign member changes the marker (membership-sensitive)
        assert!(fp1.contains(own) && fp1.contains(foreign), "{fp1}");

        // removing the foreign member changes the fingerprint (a real membership change HARDs)
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "DELETE FROM mc_workspace_members WHERE project_path = ?1",
                    params![foreign],
                )?;
                Ok(())
            })
            .unwrap();
        let fp3 = store.workspace_fingerprint(own).unwrap();
        assert_ne!(fp1, fp3, "a real membership change must change the marker");
    }

    #[test]
    fn max_mutation_and_memory_ids_union_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:own";
        let foreign = "git:foreign";

        insert_memory(&store, own, 10, "a", Some(50), "active", None);
        insert_memory(&store, foreign, 25, "b", Some(50), "active", None);
        log_mutation(&store, own, "update", 10, "v2"); // id 1
        log_mutation(&store, foreign, "archive", 25, ""); // id 2

        // single-project sees only its own max
        assert_eq!(store.max_memory_id(&[own.to_string()]).unwrap(), 10);
        assert_eq!(store.max_memory_mutation_id(&[own.to_string()]).unwrap(), 1);
        // union spans both
        let union = vec![own.to_string(), foreign.to_string()];
        assert_eq!(store.max_memory_id(&union).unwrap(), 25);
        assert_eq!(store.max_memory_mutation_id(&union).unwrap(), 2);
        // empty inputs → 0 (no panic, no all-rows scan)
        assert_eq!(store.max_memory_id(&[]).unwrap(), 0);
        assert_eq!(store.max_memory_mutation_id(&[]).unwrap(), 0);
    }

    #[test]
    fn mutation_render_spans_workspace_union() {
        // a foreign member's shared memory (in the manifest) updates → its correction
        // must render across the whole workspace union, not just the own project; a
        // single-project query would miss it (the foreign update would never supersede).
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:own";
        let foreign = "git:foreign";

        log_mutation(&store, own, "update", 100, "own-updated"); // id 1
        log_mutation(&store, foreign, "update", 200, "foreign-updated"); // id 2

        // manifest holds BOTH (the union baseline rendered own-100 + foreign-shared-200)
        let rendered = [100i64, 200];
        let single = store
            .memory_mutations_for_render(&[own.to_string()], 0, &rendered)
            .unwrap();
        assert_eq!(single.len(), 1, "single-project misses the foreign update");
        assert_eq!(single[0].target_memory_id, 100);

        let union = store
            .memory_mutations_for_render(&[own.to_string(), foreign.to_string()], 0, &rendered)
            .unwrap();
        let targets: Vec<i64> = union.iter().map(|m| m.target_memory_id).collect();
        assert!(
            targets.contains(&100) && targets.contains(&200),
            "union supersedes both: {targets:?}"
        );
    }

    #[test]
    fn active_user_memories_ordered_promoted_then_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let insert = |id: i64, content: &str, status: &str, promoted: i64| {
            store
                .inner
                .with_conn_fenced(|tx| {
                    tx.execute(
                        "INSERT INTO mc_user_memories (id, content, status, promoted_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![id, content, status, promoted],
                    )?;
                    Ok(())
                })
                .unwrap();
        };
        // two share promoted_at=100 → id breaks the tie deterministically (3 before 4).
        insert(1, "first", "active", 50);
        insert(4, "tie-later-id", "active", 100);
        insert(3, "tie-earlier-id", "active", 100);
        insert(2, "archived", "archived", 10); // status != active → excluded from the result

        let got = store.load_active_user_memories().unwrap();
        assert_eq!(got, vec!["first", "tie-earlier-id", "tie-later-id"]);
    }

    #[test]
    fn workspace_union_shares_foreign_only_in_shared_categories() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:own";
        let foreign = "git:foreign";

        // own: id1 CONSTRAINTS + id2 ARCHITECTURE (both visible to own). foreign: id3
        // CONSTRAINTS (shared) + id4 ARCHITECTURE (NOT shared). insert_memory defaults
        // category=ARCHITECTURE, so insert all four then UPDATE ids 1 and 3 to CONSTRAINTS.
        insert_memory(&store, own, 1, "own constraint", Some(70), "active", None);
        insert_memory(&store, own, 2, "own arch", Some(90), "active", None);
        insert_memory(
            &store,
            foreign,
            3,
            "foreign shared",
            Some(80),
            "active",
            None,
        );
        insert_memory(
            &store,
            foreign,
            4,
            "foreign secret",
            Some(99),
            "active",
            None,
        );
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "UPDATE mc_memories SET category='CONSTRAINTS' WHERE id IN (1,3)",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        // build a workspace with share_categories=["CONSTRAINTS"], members own+foreign.
        store
            .inner
            .with_conn_fenced(|tx| {
                tx.execute(
                    "INSERT INTO mc_workspaces (id, name, share_categories) VALUES (1,'ws','[\"CONSTRAINTS\"]')",
                    [],
                )?;
                tx.execute(
                    "INSERT INTO mc_workspace_members (workspace_id, project_path, display_name, display_path, added_at)
                     VALUES (1, ?1, 'own', '/own', 0), (1, ?2, 'svc-foreign', '/foreign', 0)",
                    params![own, foreign],
                )?;
                Ok(())
            })
            .unwrap();

        let membership = store.resolve_workspace_membership(own).unwrap().unwrap();
        assert_eq!(membership.own_identity, own);
        assert_eq!(membership.share_categories, vec!["CONSTRAINTS"]);
        assert_eq!(
            membership
                .display_name_by_path
                .get(foreign)
                .map(String::as_str),
            Some("svc-foreign")
        );

        let union = store.load_workspace_union_memories(&membership, 0).unwrap();
        let ids: Vec<i64> = union.iter().map(|m| m.id).collect();
        // own sees BOTH its own (1 CONSTRAINTS, 2 ARCHITECTURE); foreign only the shared
        // CONSTRAINTS (3) — NOT the foreign ARCHITECTURE (4, the security boundary).
        assert!(
            ids.contains(&1) && ids.contains(&2),
            "own sees all own: {ids:?}"
        );
        assert!(
            ids.contains(&3),
            "foreign shared CONSTRAINTS visible: {ids:?}"
        );
        assert!(
            !ids.contains(&4),
            "foreign non-shared ARCHITECTURE must NOT leak: {ids:?}"
        );

        // a project in NO workspace → None (single-project fast path)
        assert!(store
            .resolve_workspace_membership("git:loner")
            .unwrap()
            .is_none());
    }
}
