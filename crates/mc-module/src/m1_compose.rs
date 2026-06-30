//! The store → m1 delta producer for the SOFT branch, in TWO tiers (the
//! compose-on-bust-only discipline):
//!
//!  - [`m1_revision_signal`] is the CHEAP per-pass read: a stable digest over the
//!    watermark triple (max memory id, max mutation-log id, max compartment seq). It runs
//!    EVERY pass to feed the classifier's "did m1 change?" gate WITHOUT composing the
//!    body. Monotonic: every byte-affecting m1 change advances one leg of the triple (the
//!    mutation log is append-only, so even a same-id memory edit creates a new row → the
//!    triple moves), so the signal never MISSES a real change. It can rarely over-fire (a
//!    mutation targeting a memory not in the rendered manifest advances the triple without
//!    changing the body) → a SOFT that re-renders byte-identical m1 → no provider bust,
//!    just a redundant commit. Safe direction: never a missed bust, only a benign extra.
//!  - [`compose_m1_from_store`] is the EXPENSIVE bust-only read: it composes the actual m1
//!    delta body (memory-updates + new-compartments + new-memories) from the store and
//!    reports whether a newly-published compartment EXTENDS coverage (so the SOFT must
//!    advance the boundary anchor). Runs ONLY on a bust arm — never on a defer (a defer
//!    replays the frozen m1 verbatim; re-composing from the now-possibly-mutated store on
//!    a defer would change bytes on a defer, the cache-break the whole design forbids).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use mc_store::{McStore, McStoreError, ModuleMeta};

use crate::compartment_coverage::{partition_by_folded_seq, resolve_coverage, CoverageGap};
use crate::decay_render::DecayRenderCompartment;
use crate::memory_render::{
    assemble_m1, render_memory_block, render_memory_updates, render_new_compartments,
    M1_PLACEHOLDER,
};

/// Why composing the SOFT m1 from the store failed.
#[derive(Debug)]
pub enum M1ComposeError {
    Store(McStoreError),
    /// A compartment-coverage gap (a raw message covered by no compartment).
    CoverageGap(CoverageGap),
}

impl std::fmt::Display for M1ComposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            M1ComposeError::Store(e) => write!(f, "store: {e}"),
            M1ComposeError::CoverageGap(g) => write!(f, "{g}"),
        }
    }
}
impl std::error::Error for M1ComposeError {}
impl From<McStoreError> for M1ComposeError {
    fn from(e: McStoreError) -> Self {
        M1ComposeError::Store(e)
    }
}

/// The union project identities for a session's project: the workspace union when the
/// project is in a workspace, else just the project itself. Used by both tiers so the
/// memory watermarks + corrections span the same set the m0 baseline did.
fn union_paths(store: &McStore, project_path: &str) -> Result<Vec<String>, McStoreError> {
    Ok(match store.resolve_workspace_membership(project_path)? {
        Some(m) => m.union_identities,
        None => vec![project_path.to_string()],
    })
}

/// The CHEAP per-pass m1 revision signal: a stable digest over the watermark triple. 0 is
/// reserved for "no delta" (the placeholder), so a real signal is never 0 — we fold a
/// constant in and force the low bit set, keeping 0 exclusively the empty marker.
pub fn m1_revision_signal(
    store: &McStore,
    project_path: &str,
    session_id: &str,
) -> Result<u64, McStoreError> {
    let paths = union_paths(store, project_path)?;
    let max_memory_id = store.max_memory_id(&paths)?;
    let max_mutation_id = store.max_memory_mutation_id(&paths)?;
    let max_compartment_seq = store.max_compartment_seq(session_id)?;

    let mut h = DefaultHasher::new();
    "mc-m1-rev-v1".hash(&mut h);
    max_memory_id.hash(&mut h);
    max_mutation_id.hash(&mut h);
    max_compartment_seq.hash(&mut h);
    // reserve 0 for the empty placeholder: never return 0 for a computed signal.
    Ok(h.finish() | 1)
}

/// The composed m1 delta: its content (revision + body) and, when a newly-published
/// compartment extends the m0+m1 coverage, the new coverage anchor the SOFT must advance
/// to (boundary id + ordinal). `new_coverage` is None when only memory deltas ride (the
/// boundary stays put, the `new_boundary_id=None` SOFT path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M1Composition {
    pub revision: u64,
    pub body: String,
    pub new_coverage: Option<(String, u64)>,
}

/// EXPENSIVE bust-only: compose the m1 delta body from the store against the watermarks
/// the last HARD froze in `meta`. `now_ms` is the frozen expiry cutoff (same as the m0
/// compose). Reads compartments + memories; never call on a defer.
pub fn compose_m1_from_store(
    store: &McStore,
    project_path: &str,
    session_id: &str,
    meta: &ModuleMeta,
    now_ms: i64,
) -> Result<M1Composition, M1ComposeError> {
    let paths = union_paths(store, project_path)?;

    // --- new compartments (seq past the folded watermark) at P1 + coverage extension ---
    let compartments = store.load_compartments(session_id)?;
    let coverage = resolve_coverage(&compartments).map_err(M1ComposeError::CoverageGap)?;
    let (_folded, new_comps) = partition_by_folded_seq(&compartments, meta.folded_compartment_seq);
    let new_comp_decay: Vec<DecayRenderCompartment> = new_comps
        .iter()
        .map(|c| DecayRenderCompartment::from(*c))
        .collect();
    let new_comp_refs: Vec<&DecayRenderCompartment> = new_comp_decay.iter().collect();
    let new_compartments_block = render_new_compartments(&new_comp_refs);

    // a new compartment EXTENDS coverage when the full set's coverage end is past what
    // m0+m1 currently cover (meta.coverage_ordinal). Then the SOFT advances the anchor.
    let new_coverage = match &coverage {
        Some(c) if Some(c.coverage_end_ordinal) > meta.coverage_ordinal => {
            Some((c.boundary_id.clone(), c.coverage_end_ordinal))
        }
        _ => None,
    };

    // --- memory-updates (corrections to in-m0 memories, past the cursor) ---
    let mutations = store.memory_mutations_for_render(
        &paths,
        meta.memory_mutation_cursor,
        &meta.rendered_memory_ids,
    )?;
    let rendered_ids: std::collections::HashSet<i64> =
        meta.rendered_memory_ids.iter().copied().collect();
    let memory_updates_block = render_memory_updates(&mutations, &rendered_ids);

    // --- new-memories (id past the folded max) ---
    let new_memories = load_new_memories(store, &paths, meta.max_memory_id, now_ms)?;
    let new_memories_block = render_memory_block(
        &new_memories,
        "new-memories",
        &std::collections::HashMap::new(),
    );

    // NOTE: <new-user-profile> is deferred in this slice — it gates on a profile-version
    // marker that has no mc_* source yet (the same no-source-inert bucket as
    // project_memory_epoch). It lands with the writer relocation.

    let body = assemble_m1(
        &memory_updates_block,
        &new_compartments_block,
        &new_memories_block,
        "", // new-user-profile (deferred, no source)
        M1_PLACEHOLDER,
    );
    let revision = if body == M1_PLACEHOLDER {
        0
    } else {
        m1_revision_signal(store, project_path, session_id)?
    };

    Ok(M1Composition {
        revision,
        body,
        new_coverage,
    })
}

/// Active memories with `id > after_id` across the union, ordered by importance then id
/// (the <new-memories> render set). Filters the active pool in memory — the new set is a
/// small tail and this runs only on a bust.
fn load_new_memories(
    store: &McStore,
    paths: &[String],
    after_id: i64,
    now_ms: i64,
) -> Result<Vec<mc_store::StoredMemory>, McStoreError> {
    let all = if paths.len() == 1 {
        store.load_active_memories(&paths[0], now_ms)?
    } else {
        // union: read each member then merge (the membership filter already applied in the
        // single-project read isn't needed here — new-memories are additive, the share
        // filter applies to the BASELINE manifest; a new foreign memory in a non-shared
        // category won't be in rendered_memory_ids so its later updates won't supersede,
        // but as a NEW memory it follows the same own-vs-foreign visibility as the union
        // baseline). Reuse the membership union read for correctness.
        let membership = store
            .resolve_workspace_membership(&paths[0])?
            .expect("union paths imply a membership");
        store.load_workspace_union_memories(&membership, now_ms)?
    };
    Ok(all.into_iter().filter(|m| m.id > after_id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use mc_store::StoredCompartment;

    fn descriptor(dir: &std::path::Path) -> StorageDescriptor {
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
            content: format!("b{seq}"),
            p1: Some(format!("P1-{seq}")),
            importance: 50,
            ..Default::default()
        }
    }

    fn meta_after_hard(
        folded_seq: i64,
        coverage: Option<u64>,
        max_mem: i64,
        cursor: i64,
        manifest: Vec<i64>,
    ) -> ModuleMeta {
        ModuleMeta {
            initialized: true,
            folded_compartment_seq: folded_seq,
            coverage_ordinal: coverage,
            max_memory_id: max_mem,
            memory_mutation_cursor: cursor,
            rendered_memory_ids: manifest,
            ..Default::default()
        }
    }

    #[test]
    fn revision_signal_is_stable_and_moves_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let p = "git:proj";
        let s0 = m1_revision_signal(&store, p, "ses").unwrap();
        let s1 = m1_revision_signal(&store, p, "ses").unwrap();
        assert_eq!(s0, s1, "stable store → stable signal");
        assert_ne!(s0, 0, "a computed signal is never the empty marker 0");

        // publish a compartment → the signal moves
        store
            .replace_compartments("ses", &[comp(1, 1, 9, "m9")])
            .unwrap();
        let s2 = m1_revision_signal(&store, p, "ses").unwrap();
        assert_ne!(s1, s2, "new compartment → signal moves");
    }

    #[test]
    fn empty_delta_is_placeholder_revision_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        // a HARD folded everything (folded_seq covers all, no new memories/mutations)
        let meta = meta_after_hard(5, Some(50), 100, 9, vec![1, 2]);
        let m1 = compose_m1_from_store(&store, "git:proj", "ses", &meta, 0).unwrap();
        assert_eq!(m1.body, M1_PLACEHOLDER);
        assert_eq!(m1.revision, 0, "empty delta → revision 0");
        assert_eq!(m1.new_coverage, None);
    }

    #[test]
    fn new_compartment_rides_m1_and_extends_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        // m0 folded C1 (covers ord 1-10, folded_seq=1, coverage=10). C2 (11-20) publishes.
        store
            .replace_compartments("ses", &[comp(1, 1, 10, "m10"), comp(2, 11, 20, "m20")])
            .unwrap();
        let meta = meta_after_hard(1, Some(10), 0, 0, vec![]);
        let m1 = compose_m1_from_store(&store, "git:proj", "ses", &meta, 0).unwrap();

        // C2 rides m1 at P1, and coverage extends 10 → 20 (the SOFT advances the anchor)
        assert!(m1.body.contains("<new-compartments>"), "{}", m1.body);
        assert!(m1.body.contains("title=\"C2\"") && !m1.body.contains("title=\"C1\""));
        assert!(m1.body.contains("P1-2"), "rides at P1: {}", m1.body);
        assert_eq!(m1.new_coverage, Some(("m20".to_string(), 20)));
        assert_ne!(m1.revision, 0);
    }

    #[test]
    fn memory_only_delta_does_not_extend_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        // one folded compartment; a NEW memory (id 5) past the folded max (0).
        store
            .replace_compartments("ses", &[comp(1, 1, 10, "m10")])
            .unwrap();
        store
            .seed_memory(5, "git:proj", "ARCHITECTURE", "new mem", 70)
            .unwrap();
        // meta: folded_seq=1, coverage=10 (matches the only compartment), folded max_mem=0
        let meta = meta_after_hard(1, Some(10), 0, 0, vec![]);
        let m1 = compose_m1_from_store(&store, "git:proj", "ses", &meta, 0).unwrap();

        assert!(m1.body.contains("<new-memories>"), "{}", m1.body);
        assert!(m1.body.contains("new mem"));
        // no new compartment → coverage does NOT extend (the None-boundary SOFT path)
        assert_eq!(
            m1.new_coverage, None,
            "memory-only delta keeps the boundary put"
        );
    }
}
