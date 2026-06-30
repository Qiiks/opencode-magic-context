//! The transform op: the CK-in / CK-out cache-stability spine.
//!
//! One pass: load the session's persisted state, classify (via `mc-core`), drive
//! `cortexkit-cache-core`'s `step`, and conditionally commit. The classifier is
//! state-driven (bootstrap / epoch / reconcile / else-defer) — the spine needs no
//! signal-kind interpretation, so a slice-1 pass never produces `Soft` (reduction,
//! the only `Soft` producer, lands later).
//!
//! Conditional commit: a pass writes ONLY when durable state actually changed. A
//! pure SoftPlus replay mutates nothing and skips the write entirely (the
//! no-write-on-defer guarantee). A CAS conflict means a concurrent writer won; the
//! whole pass re-loads and re-steps (classification depends on the loaded state).

use mc_core::{
    boundary_id, classify, coverage_ordinal, Action, CkItem, ClassifierInput, PassInput,
};
use mc_store::{McStore, McStoreError};
use serde::{Deserialize, Serialize};

/// Max CAS retries before surfacing the conflict (a contended session re-loads and
/// re-steps; in practice the module is the single writer so this rarely loops).
const MAX_CAS_RETRIES: u32 = 8;

/// A CK item on the wire: opaque id + monotonic ordinal + byte-complete rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CkItemWire {
    pub id: String,
    pub ordinal: u64,
    pub bytes: String,
}

impl CkItem for CkItemWire {
    fn id(&self) -> &str {
        &self.id
    }
    fn ordinal(&self) -> u64 {
        self.ordinal
    }
    fn bytes(&self) -> &str {
        &self.bytes
    }
}

/// A transform pass request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformRequest {
    pub session_id: String,
    /// The live boundary token: the id the harness found at the coverage boundary in
    /// the incoming array, or a sentinel (e.g. "-") when the boundary was removed.
    pub boundary_present: String,
    /// The render-config fingerprint (system hash + tool set + model key + serializer
    /// profile, folded by the harness). A change vs the persisted one is an epoch Hard.
    pub render_config: String,
    /// The live CK array. Used to render the baseline + mint the boundary on a Hard;
    /// ignored on a defer.
    #[serde(default)]
    pub items: Vec<CkItemWire>,
}

/// A transform pass result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransformResponse {
    /// "SOFT+" | "SOFT" | "HARD" (cache-core's serde rename).
    pub action: Action,
    pub boundary_id: String,
    /// The in-order concatenation of every frozen unit's payload — the cached prefix.
    /// Two consecutive SoftPlus passes MUST return an identical value.
    pub cached_prefix_bytes: String,
    pub reconcile_pending: bool,
    pub version: u64,
    pub row_version: u64,
    /// Whether this pass wrote durable state (false on a pure defer replay).
    pub committed: bool,
}

/// Apply one transform pass, retrying the whole load→classify→step→commit cycle on a
/// CAS conflict (re-classification depends on the freshly-loaded state).
pub fn transform(
    store: &McStore,
    req: &TransformRequest,
) -> Result<TransformResponse, McStoreError> {
    let mut attempt = 0;
    loop {
        match apply_once(store, req) {
            Err(McStoreError::CasConflict { .. }) if attempt < MAX_CAS_RETRIES => {
                attempt += 1;
                continue;
            }
            other => return other,
        }
    }
}

fn apply_once(store: &McStore, req: &TransformRequest) -> Result<TransformResponse, McStoreError> {
    let loaded = store.load(&req.session_id)?;

    let render_config_changed =
        loaded.meta.initialized && req.render_config != loaded.meta.last_render_config;
    let boundary_present = req.boundary_present == loaded.core.boundary_id;

    let action = classify(&ClassifierInput {
        initialized: loaded.meta.initialized,
        render_config_changed,
        boundary_present,
        reconcile_pending: loaded.core.reconcile_pending,
    });

    let mut core = loaded.core.clone();
    let mut meta = loaded.meta.clone();

    let pass = match action {
        Action::Hard => PassInput {
            proposed: Some(Action::Hard),
            boundary_present: req.boundary_present.clone(),
            rendered_units: mc_core::render_baseline(&req.items),
            new_boundary_id: boundary_id(&req.items),
            queued: Vec::new(),
            run_started: false,
        },
        // The slice-1 classifier only ever returns Hard or SoftPlus; Soft has no
        // producer yet. Treat any non-Hard as the defer path.
        _ => PassInput {
            proposed: Some(Action::SoftPlus),
            boundary_present: req.boundary_present.clone(),
            ..Default::default()
        },
    };

    let result = core.step(pass);

    if matches!(action, Action::Hard) {
        meta.initialized = true;
        meta.last_render_config = req.render_config.clone();
        meta.coverage_ordinal = coverage_ordinal(&req.items);
    }

    // Conditional commit: write only when durable state actually changed. A pure
    // SoftPlus replay leaves core+meta byte-identical → no write.
    let changed = core != loaded.core || meta != loaded.meta;
    let row_version = if changed {
        store.commit(&req.session_id, loaded.row_version, &core, &meta)?
    } else {
        loaded.row_version.unwrap_or(0)
    };

    Ok(TransformResponse {
        action: result.action,
        boundary_id: core.boundary_id.clone(),
        cached_prefix_bytes: core.cached_prefix_bytes(),
        reconcile_pending: result.reconcile_pending,
        version: core.version,
        row_version,
        committed: changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};

    fn store(dir: &std::path::Path) -> McStore {
        McStore::open(&StorageDescriptor {
            module_id: "magic-context-test".to_string(),
            storage_namespace: "mc_cache".to_string(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.join("store.db").to_string_lossy().to_string(),
            },
        })
        .unwrap()
    }

    fn item(id: &str, ordinal: u64, bytes: &str) -> CkItemWire {
        CkItemWire {
            id: id.to_string(),
            ordinal,
            bytes: bytes.to_string(),
        }
    }

    fn req(session: &str, boundary: &str, cfg: &str, items: Vec<CkItemWire>) -> TransformRequest {
        TransformRequest {
            session_id: session.to_string(),
            boundary_present: boundary.to_string(),
            render_config: cfg.to_string(),
            items,
        }
    }

    /// Bootstrap-Hard: a fresh session's first pass folds Hard, renders the baseline,
    /// mints the boundary, and commits.
    #[test]
    fn bootstrap_first_pass_is_hard_and_creates_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let r = transform(
            &s,
            &req("ses", "ignored", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
        )
        .unwrap();
        assert_eq!(r.action, Action::Hard);
        assert_eq!(r.boundary_id, "a");
        assert_eq!(r.cached_prefix_bytes, "<h>BASE</h>");
        assert!(r.committed);
        assert_eq!(r.row_version, 1);
    }

    /// V1: after bootstrap, growing-tail passes with the boundary present defer and
    /// stay byte-identical, and a pure defer writes nothing.
    #[test]
    fn v1_growing_tail_defers_byte_stable_no_write() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        transform(
            &s,
            &req("ses", "x", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
        )
        .unwrap();

        let mut last: Option<String> = None;
        for _ in 0..4 {
            let r = transform(&s, &req("ses", "a", "cfg0", vec![])).unwrap();
            assert_eq!(r.action, Action::SoftPlus);
            assert!(!r.committed, "pure defer must not write");
            assert_eq!(r.row_version, 1, "row_version must not advance on defer");
            if let Some(prev) = &last {
                assert_eq!(&r.cached_prefix_bytes, prev, "defer changed bytes");
            }
            last = Some(r.cached_prefix_bytes);
        }
    }

    /// Epoch-Hard: a render-config change after a baseline exists folds Hard.
    #[test]
    fn epoch_change_is_hard() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        transform(
            &s,
            &req("ses", "x", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
        )
        .unwrap();
        // Same boundary present, but a NEW render config → Hard rematerialize.
        let r = transform(
            &s,
            &req("ses", "a", "cfg1", vec![item("a", 1, "<h>BASE2</h>")]),
        )
        .unwrap();
        assert_eq!(r.action, Action::Hard);
        assert_eq!(r.cached_prefix_bytes, "<h>BASE2</h>");
        assert!(r.committed);
    }

    /// V7: a provider-nonce-only pass (no boundary/config change) defers byte-stable.
    #[test]
    fn v7_provider_nonce_only_defers_stable() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        transform(&s, &req("ses", "x", "cfg0", vec![item("a", 1, "BASE")])).unwrap();
        let a = transform(&s, &req("ses", "a", "cfg0", vec![])).unwrap();
        let b = transform(&s, &req("ses", "a", "cfg0", vec![])).unwrap(); // nonce-only
        assert_eq!(a.action, Action::SoftPlus);
        assert_eq!(b.action, Action::SoftPlus);
        assert_eq!(a.cached_prefix_bytes, b.cached_prefix_bytes);
    }

    /// V8: a revert that removes the boundary defers (no bust) and flags reconcile;
    /// the next pass with the boundary still absent folds Hard and rematerializes.
    #[test]
    fn v8_revert_then_reconcile_rematerializes() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        transform(
            &s,
            &req("ses", "x", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
        )
        .unwrap();
        let before = transform(&s, &req("ses", "a", "cfg0", vec![])).unwrap();
        assert_eq!(before.action, Action::SoftPlus);

        // Revert removed the boundary message: boundary_present "-" != "a".
        let revert = transform(&s, &req("ses", "-", "cfg0", vec![])).unwrap();
        assert_eq!(revert.action, Action::SoftPlus, "revert must not bust");
        assert!(revert.reconcile_pending, "boundary loss flags reconcile");
        assert_eq!(
            revert.cached_prefix_bytes, before.cached_prefix_bytes,
            "revert keeps frozen bytes"
        );
        assert!(revert.committed, "reconcile flag flip must persist");

        // Next pass, boundary still absent → Hard rematerialize against the live array.
        let remat = transform(
            &s,
            &req("ses", "-", "cfg0", vec![item("a2", 2, "<h>REVERTED</h>")]),
        )
        .unwrap();
        assert_eq!(remat.action, Action::Hard);
        assert_eq!(remat.boundary_id, "a2");
        assert_eq!(remat.cached_prefix_bytes, "<h>REVERTED</h>");
        assert!(!remat.reconcile_pending, "Hard clears reconcile");

        // Re-stabilizes on the new boundary.
        let after = transform(&s, &req("ses", "a2", "cfg0", vec![])).unwrap();
        assert_eq!(after.action, Action::SoftPlus);
        assert!(!after.committed);
        assert_eq!(after.cached_prefix_bytes, "<h>REVERTED</h>");
    }

    /// V9 (in-process half): persisted lineage state reproduces byte-identical when
    /// re-opened (the store round-trips the frozen units) — the restart-recovery
    /// property the wire harness then proves across a real process restart.
    #[test]
    fn v9_state_reproduces_byte_identical_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let bytes_before;
        {
            let s = store(dir.path());
            transform(
                &s,
                &req("ses", "x", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
            )
            .unwrap();
            bytes_before = transform(&s, &req("ses", "a", "cfg0", vec![]))
                .unwrap()
                .cached_prefix_bytes;
        } // drop releases the lease (simulates process exit)

        let s2 = store(dir.path()); // re-open (simulates restart)
        let after = transform(&s2, &req("ses", "a", "cfg0", vec![])).unwrap();
        assert_eq!(after.action, Action::SoftPlus);
        assert!(!after.committed, "restart replay writes nothing");
        assert_eq!(
            after.cached_prefix_bytes, bytes_before,
            "lineage state must reproduce byte-identical across reopen"
        );
    }
}
