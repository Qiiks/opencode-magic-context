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

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    statements: "
        CREATE TABLE IF NOT EXISTS mc_cache_state (
            session_id   TEXT PRIMARY KEY,
            row_version  INTEGER NOT NULL,
            core_state   TEXT NOT NULL,
            meta         TEXT NOT NULL
        );
    ",
}];

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
}
