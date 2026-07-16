//! The store → m0 byte producer for the HARD branch: read a session's durable state
//! (compartments, memories or the workspace union, user-profile, project-docs) and
//! compose the frozen m0 baseline bytes plus the watermarks the HARD persists.
//!
//! This is the BYTE producer only — it does not classify or decide HARD-vs-SOFT (that's
//! `apply_once`, which feeds these bytes into the cache core). It is pure given the store
//! contents + `now_ms` + `budget`: same inputs → same bytes, the property the frozen-m0
//! cache depends on. The expiry cutoff (`now_ms`) is passed in (frozen at the HARD by the
//! caller, never read here from a live clock) so a later defer replays identical bytes.

use std::collections::HashMap;

use mc_store::{McStore, McStoreError, MemoryRevision, SHADOW_SESSION_PREFIX};

use crate::compartment_coverage::{resolve_coverage, CoverageGap};
use crate::decay_render::DecayRenderCompartment;
use crate::memory_render::{render_m0, workspace_source_names, M0Inputs};
use crate::project_docs::read_project_docs_canonical;

/// Why composing the HARD m0 from the store failed.
#[derive(Debug)]
pub enum M0ComposeError {
    /// A store read failed.
    Store(McStoreError),
    /// The stored compartment ranges overlap or otherwise fail strict ordering.
    CoverageGap(CoverageGap),
}

impl std::fmt::Display for M0ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            M0ComposeError::Store(e) => write!(f, "store: {e}"),
            M0ComposeError::CoverageGap(g) => write!(f, "{g}"),
        }
    }
}
impl std::error::Error for M0ComposeError {}
impl From<McStoreError> for M0ComposeError {
    fn from(e: McStoreError) -> Self {
        M0ComposeError::Store(e)
    }
}

/// The composed m0 baseline: its frozen bytes plus the watermarks the HARD persists into
/// [`mc_store::ModuleMeta`] atomically with those bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M0Composition {
    /// The frozen m0 baseline bytes (docs + profile + decayed compartments + memories).
    pub m0_bytes: String,
    /// The last raw message id covered by m0 — the cache/revert anchor. Empty when the
    /// session has no compartments (nothing summarized → no covered prefix → the whole
    /// live array is the tail).
    pub boundary_id: String,
    /// The last covered ordinal (the m0 coverage end / tail-trim point). None when there
    /// are no compartments.
    pub coverage_ordinal: Option<u64>,
    /// The FIRST covered ordinal (the leading edge of m0 coverage = the first compartment's
    /// start). None when there are no compartments. The caller fails loud if any live item
    /// sits BELOW this — it would be covered by no compartment yet trimmed as covered (a
    /// silent leading-gap drop).
    pub first_covered_ordinal: Option<u64>,
    /// The highest compartment sequence folded into m0 (advances only on a HARD).
    pub folded_compartment_seq: i64,
    /// The memory ids actually rendered into m0 (the supersede manifest). Until the token
    /// estimator lands this is every active memory (no budget trim — stability-safe: a
    /// HARD rebuilds the prefix regardless, only m0 SIZE grows).
    pub rendered_memory_ids: Vec<i64>,
    /// The mutation-log cursor as of this HARD (corrections at/below it are folded in).
    pub memory_mutation_cursor: i64,
    /// The highest memory id folded into m0.
    pub max_memory_id: i64,
    /// Source revision captured in the same SQLite snapshot as the rendered rows.
    pub memory_revision: MemoryRevision,
    /// The canonical project-docs hash, a SNAPSHOT MARKER persisted with the bytes (NOT a
    /// HARD trigger — see `M0ContentEpoch`). Records which docs version is in m0 so the
    /// next natural HARD re-reads current docs.
    pub docs_hash: String,
}

/// The fixed expiry/budget inputs for an m0 compose, threaded from the caller so the
/// HARD freezes them (a defer replays the same bytes, never re-reading a live clock or
/// config).
pub struct M0ComposeInputs<'a> {
    pub session_id: &'a str,
    /// The project the store reads key off (resolved from the route binding, never the
    /// request body).
    pub project_path: &'a str,
    /// The project directory on disk, for reading ARCHITECTURE.md / STRUCTURE.md.
    pub project_directory: &'a str,
    /// The expiry cutoff, FROZEN at the HARD (a memory expiring after this still renders;
    /// a later defer uses the same cutoff → identical bytes).
    pub now_ms: i64,
    /// The history budget in tokens (frozen at route bind). The decay renderer fits the
    /// compartments to it; under a loose budget the render is estimator-independent.
    pub history_budget_tokens: f64,
    /// System-role content that is no longer in the live tail because the current fold
    /// covers its ordinal. Passing it explicitly keeps m0 composition deterministic and
    /// replayable.
    pub covered_system_messages: &'a [String],
    /// Disabled memory removes both project memories and the user-profile memory block.
    pub memory_enabled: bool,
}

/// Read the store and compose the HARD m0 bytes + watermarks. `estimate_tokens` is the
/// (currently no-op) token estimator the decay renderer uses for its budget fit.
pub fn compose_m0_from_store(
    store: &McStore,
    inputs: &M0ComposeInputs<'_>,
    estimate_tokens: impl Fn(&str) -> usize + Copy,
) -> Result<M0Composition, M0ComposeError> {
    // --- compartments: the session history, coverage anchor, and folded watermark ---
    let compartments = store.load_compartments(inputs.session_id)?;
    // Store-pure coverage checks enforce strict ordering without assuming integer
    // contiguity: consumer producers may retire ordinal numbers permanently. The
    // transform layer has the live array and fails loud if a present message below
    // the coverage end is not covered by any compartment.
    let coverage = resolve_coverage(&compartments).map_err(M0ComposeError::CoverageGap)?;
    let (boundary_id, coverage_ordinal, first_covered_ordinal, folded_compartment_seq) =
        match &coverage {
            Some(c) => (
                c.boundary_id.clone(),
                Some(c.coverage_end_ordinal),
                Some(c.first_covered_ordinal),
                c.max_sequence,
            ),
            // no compartments → nothing summarized → no covered prefix
            None => (String::new(), None, None, 0),
        };

    // --- memories: rows and watermarks share one SQLite snapshot ---
    let membership = store.resolve_workspace_membership(inputs.project_path)?;
    let snapshot = if inputs.memory_enabled {
        store.load_memory_render_snapshot(
            inputs.project_path,
            membership.as_ref(),
            inputs.now_ms,
        )?
    } else {
        mc_store::MemoryRenderSnapshot {
            memories: Vec::new(),
            revision: MemoryRevision::default(),
        }
    };
    let source_name_by_id = membership
        .as_ref()
        .map(|value| workspace_source_names(&snapshot.memories, value))
        .unwrap_or_else(HashMap::new);
    let rendered_memory_ids: Vec<i64> = snapshot.memories.iter().map(|memory| memory.id).collect();
    let max_memory_id = snapshot.revision.max_memory_id;
    let memory_mutation_cursor = snapshot.revision.mutation_cursor;

    // --- user-profile + project-docs ---
    let user_profile = if inputs.memory_enabled {
        if inputs.project_path.starts_with(SHADOW_SESSION_PREFIX) {
            store.load_shadow_user_profile(inputs.project_path)?
        } else {
            store.load_active_user_memories()?
        }
    } else {
        Vec::new()
    };
    let docs = read_project_docs_canonical(inputs.project_directory);

    // --- compose the m0 bytes via the shared render_m0 (no trim until the estimator
    // lands → loose budget, decay-pressure 1.0; the render is pure + estimator-independent) ---
    let decay_compartments: Vec<DecayRenderCompartment> = compartments
        .iter()
        .map(DecayRenderCompartment::from)
        .collect();
    let m0_bytes = render_m0(
        &M0Inputs {
            project_docs: &docs.rendered_block,
            user_profile: &user_profile,
            covered_system_messages: inputs.covered_system_messages,
            compartments: &decay_compartments,
            memories: &snapshot.memories,
            source_name_by_id: &source_name_by_id,
            history_budget_tokens: inputs.history_budget_tokens,
            decay_pressure_multiplier: 1.0,
        },
        estimate_tokens,
    );

    Ok(M0Composition {
        m0_bytes,
        boundary_id,
        coverage_ordinal,
        first_covered_ordinal,
        folded_compartment_seq,
        rendered_memory_ids,
        memory_mutation_cursor,
        max_memory_id,
        memory_revision: snapshot.revision,
        docs_hash: docs.canonical_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_store_types::StorageDescriptor;
    use mc_store::{InsertMemoryInput, ModuleMeta, ShadowStateSyncRequest, StoredCompartment};

    fn no_estimate(_: &str) -> usize {
        0
    }

    fn descriptor(dir: &std::path::Path) -> StorageDescriptor {
        use cortexkit_store_types::{Isolation, StorageBackend};
        StorageDescriptor {
            module_id: "magic-context".into(),
            storage_namespace: "mc_cache".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.join("store.db").to_string_lossy().into_owned(),
            },
        }
    }

    fn comp(seq: i64, start: i64, end: i64, end_id: &str) -> StoredCompartment {
        StoredCompartment {
            sequence: seq,
            start_message: start,
            end_message: end,
            end_message_id: end_id.to_string(),
            title: format!("C{seq}"),
            content: format!("body{seq}"),
            p1: Some(format!("P1 of {seq}")),
            importance: 50,
            ..Default::default()
        }
    }

    #[test]
    fn composes_m0_from_compartments_with_coverage_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let project = "git:proj";
        let project_dir = dir.path().join("repo");
        std::fs::create_dir_all(&project_dir).unwrap();

        store
            .replace_compartments("ses_a", &[comp(1, 1, 10, "m10"), comp(2, 11, 20, "m20")])
            .unwrap();

        let inputs = M0ComposeInputs {
            session_id: "ses_a",
            project_path: project,
            project_directory: project_dir.to_str().unwrap(),
            now_ms: 0,
            history_budget_tokens: 60_000.0,
            covered_system_messages: &[],
            memory_enabled: true,
        };
        let m0 = compose_m0_from_store(&store, &inputs, no_estimate).unwrap();

        // coverage anchors at the LAST compartment (the m0+m1 coverage end)
        assert_eq!(m0.boundary_id, "m20");
        assert_eq!(m0.coverage_ordinal, Some(20));
        assert_eq!(m0.folded_compartment_seq, 2);
        // Both compartments render as headings inside the stable session-history block.
        assert!(m0.m0_bytes.contains("<session-history>"), "{}", m0.m0_bytes);
        assert!(m0.m0_bytes.contains("## 1-10 · C1\nP1 of 1"));
        assert!(m0.m0_bytes.contains("## 11-20 · C2\nP1 of 2"));
        assert!(!m0.m0_bytes.contains("<compartment"));
    }

    #[test]
    fn shadow_profile_seed_matches_typescript_profile_block_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let session_id = "shadow:profile";
        let profile = vec!["prefers root cause".to_string(), "x < y & z".to_string()];
        store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id,
                shadow_project_path: session_id,
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                compartments: &[],
                memories: &[],
                memory_mutations: &[],
                user_profile: &profile,
                workspace: None,
                last_todo_state: None,
                acked_watermarks: serde_json::Value::Null,
            })
            .unwrap();

        let project_dir = dir.path().join("repo");
        std::fs::create_dir_all(&project_dir).unwrap();
        let inputs = M0ComposeInputs {
            session_id,
            project_path: session_id,
            project_directory: project_dir.to_str().unwrap(),
            now_ms: 0,
            history_budget_tokens: 60_000.0,
            covered_system_messages: &[],
            memory_enabled: true,
        };
        let composed = compose_m0_from_store(&store, &inputs, no_estimate).unwrap();
        assert_eq!(
            composed.m0_bytes,
            "<user-profile>\n- prefers root cause\n- x &lt; y &amp; z\n</user-profile>\n\n<session-history></session-history>"
        );
    }

    #[test]
    fn no_compartments_yields_empty_boundary_and_placeholder_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let project_dir = dir.path().join("repo");
        std::fs::create_dir_all(&project_dir).unwrap();

        let inputs = M0ComposeInputs {
            session_id: "ses_empty",
            project_path: "git:proj",
            project_directory: project_dir.to_str().unwrap(),
            now_ms: 0,
            history_budget_tokens: 60_000.0,
            covered_system_messages: &[],
            memory_enabled: true,
        };
        let m0 = compose_m0_from_store(&store, &inputs, no_estimate).unwrap();

        // nothing summarized → no covered prefix → empty anchor, the whole array is tail
        assert_eq!(m0.boundary_id, "");
        assert_eq!(m0.coverage_ordinal, None);
        assert_eq!(m0.folded_compartment_seq, 0);
        assert!(m0.rendered_memory_ids.is_empty());
    }

    #[test]
    fn memory_disabled_omits_memory_blocks_and_watermarks() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        store
            .insert_memory(InsertMemoryInput {
                project_path: "git:proj",
                category: "CONSTRAINTS",
                content: "must stay hidden",
                source_session_id: None,
                source_type: Some("agent"),
                importance: Some(50),
                expires_at: None,
                metadata_json: None,
                now_ms: 1,
            })
            .unwrap();
        let inputs = M0ComposeInputs {
            session_id: "ses",
            project_path: "git:proj",
            project_directory: dir.path().to_str().unwrap(),
            now_ms: 2,
            history_budget_tokens: 60_000.0,
            covered_system_messages: &[],
            memory_enabled: false,
        };

        let composed = compose_m0_from_store(&store, &inputs, no_estimate).unwrap();
        assert!(!composed.m0_bytes.contains("must stay hidden"));
        assert!(!composed.m0_bytes.contains("<project-memory>"));
        assert!(composed.rendered_memory_ids.is_empty());
        assert_eq!(composed.max_memory_id, 0);
        assert_eq!(composed.memory_mutation_cursor, 0);
    }

    #[test]
    fn sparse_coordinate_gap_composes_store_pure() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let project_dir = dir.path().join("repo");
        std::fs::create_dir_all(&project_dir).unwrap();

        // Store-only composition cannot tell whether 11-19 are retired ordinals
        // or present uncovered messages, so sparse coordinate gaps compose here.
        store
            .replace_compartments("ses_gap", &[comp(1, 1, 10, "m10"), comp(2, 20, 30, "m30")])
            .unwrap();
        let inputs = M0ComposeInputs {
            session_id: "ses_gap",
            project_path: "git:proj",
            project_directory: project_dir.to_str().unwrap(),
            now_ms: 0,
            history_budget_tokens: 60_000.0,
            covered_system_messages: &[],
            memory_enabled: true,
        };
        let composed = compose_m0_from_store(&store, &inputs, no_estimate).unwrap();
        assert_eq!(composed.coverage_ordinal, Some(30));
        assert_eq!(composed.boundary_id, "m30");
    }

    #[test]
    fn determinism_same_inputs_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let project_dir = dir.path().join("repo");
        std::fs::create_dir_all(&project_dir).unwrap();
        store
            .replace_compartments("ses_d", &[comp(1, 1, 10, "m10")])
            .unwrap();
        let _ = ModuleMeta::default(); // (meta unused by the byte producer)
        let inputs = M0ComposeInputs {
            session_id: "ses_d",
            project_path: "git:proj",
            project_directory: project_dir.to_str().unwrap(),
            now_ms: 1000,
            history_budget_tokens: 60_000.0,
            covered_system_messages: &[],
            memory_enabled: true,
        };
        let a = compose_m0_from_store(&store, &inputs, no_estimate).unwrap();
        let b = compose_m0_from_store(&store, &inputs, no_estimate).unwrap();
        assert_eq!(
            a.m0_bytes, b.m0_bytes,
            "same store + inputs → identical m0 bytes"
        );
    }
}
