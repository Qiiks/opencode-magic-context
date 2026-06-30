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
use rusqlite::params;
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
];

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
}
