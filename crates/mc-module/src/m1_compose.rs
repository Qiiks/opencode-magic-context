//! The store → m1 delta producer for bust arms, in TWO tiers (the
//! compose-on-opportunity discipline):
//!
//!  - [`m1_revision_signal`] is the CHEAP per-pass read: split in-session and external
//!    fingerprints run EVERY pass to tell the classifier whether work is pending and whether
//!    a render-config HARD is due, WITHOUT composing the body. In-session changes are allowed
//!    to remain pending so ordinary turns replay frozen bytes; external workspace changes stay
//!    eager-HARD.
//!  - [`compose_m1_from_store`] is the EXPENSIVE bust-only read: it composes the actual m1
//!    delta body (memory-updates + new-compartments + new-memories) from the store and
//!    reports whether a newly-published compartment EXTENDS coverage (so the bust must
//!    advance the boundary anchor). Runs ONLY on a bust opportunity — never on a defer (a
//!    defer replays the frozen m1 verbatim; re-composing from the now-possibly-mutated store
//!    on a defer would change bytes on a defer, violating the deferred-work invariant).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use mc_store::{McStore, McStoreError, ModuleMeta, NoteDelivery, StoredNote};

use crate::compartment_coverage::{partition_by_folded_seq, resolve_coverage, CoverageGap};
use crate::decay_render::DecayRenderCompartment;
use crate::m0_compose::{trim_memories_to_budget, trim_user_profile_to_budget};
use crate::memory_render::{
    assemble_m1, render_memory_block, render_memory_updates, render_new_compartments,
    render_user_profile_block, workspace_source_names, M1_PLACEHOLDER,
};

/// Why composing the SOFT m1 from the store failed.
#[derive(Debug)]
pub enum M1ComposeError {
    Store(McStoreError),
    /// Stored compartment ranges overlap or otherwise fail strict ordering.
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

/// The cheap per-pass revision signal, split into two lanes:
///
/// * `revision` is the IN-SESSION lane. Memory inserts/updates, the mutation log, note
///   surfacing, profile-version lines, and ordinary compartment publication all become
///   pending work. A mismatch is intentionally deferred until an independent render.
/// * `external_revision` is the EXTERNAL lane. Workspace membership/visibility changes
///   remain eager-HARD because they change the m0 memory universe; project memory epochs
///   are carried by state-sync and arm the same HARD path in durable metadata.
///
/// This table is the ordering contract for the module's bust opportunity gate:
///
/// | input | lane | no independent render | independent render |
/// | memory/profile/note/compartment signal | in-session | defer, preserve frozen bytes | fold in HARD/SOFT |
/// | workspace fingerprint | external | HARD | HARD |
/// | project memory epoch | external | HARD on next pass | HARD |
/// | flush/refresh, Force/Emergency, first reduction | opportunity | fold pending delta | fold pending delta |
///
/// The signal only identifies pending work; it never authorizes a bust by itself. This is the
/// deferred-work invariant: a provider cache must not be invalidated merely because a store
/// watermark moved between ordinary turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct M1RevisionSignal {
    /// Applied-revision comparison for the in-session lane.
    pub revision: u64,
    /// External workspace lane; changes route to HARD, never SOFT.
    pub external_revision: u64,
    /// Highest compartment sequence read while computing `revision`.
    pub max_compartment_seq: i64,
    pub max_memory_id: i64,
    pub max_memory_mutation_id: i64,
    pub note_status_version: i64,
    pub user_profile_version: u64,
}

pub fn m1_revision_signal(
    store: &McStore,
    project_path: &str,
    session_id: &str,
) -> Result<u64, McStoreError> {
    Ok(m1_revision_signal_parts(store, project_path, session_id)?.revision)
}

pub fn m1_revision_signal_parts(
    store: &McStore,
    project_path: &str,
    session_id: &str,
) -> Result<M1RevisionSignal, McStoreError> {
    m1_revision_signal_parts_for_pass(store, project_path, project_path, session_id, 0, 0)
}

/// Read both signal lanes for a transform pass. The extra context is supplied by the
/// already-loaded transform route so note/profile changes are covered without rendering.
pub fn m1_revision_signal_parts_for_pass(
    store: &McStore,
    project_path: &str,
    note_project_path: &str,
    session_id: &str,
    user_profile_version: u64,
    now_ms: i64,
) -> Result<M1RevisionSignal, McStoreError> {
    let paths = union_paths(store, project_path)?;
    let max_memory_id = store.max_memory_id(&paths)?;
    let max_memory_mutation_id = store.max_memory_mutation_id(&paths)?;
    let max_compartment_seq = store.max_compartment_seq(session_id)?;
    let note_status_version = store.max_note_status_version(note_project_path)?;

    let mut in_session = DefaultHasher::new();
    // Preserve the old digest format when both new inputs are zero, so sessions created
    // before these inputs existed do not appear changed solely because the signal gained fields.
    if note_status_version == 0 && user_profile_version == 0 {
        "mc-m1-rev-v1".hash(&mut in_session);
        max_memory_id.hash(&mut in_session);
        max_memory_mutation_id.hash(&mut in_session);
        max_compartment_seq.hash(&mut in_session);
    } else {
        "mc-m1-in-session-v2".hash(&mut in_session);
        max_memory_id.hash(&mut in_session);
        max_memory_mutation_id.hash(&mut in_session);
        max_compartment_seq.hash(&mut in_session);
        note_status_version.hash(&mut in_session);
        user_profile_version.hash(&mut in_session);
    }

    let workspace_fingerprint = store.workspace_fingerprint(project_path, now_ms)?;
    let mut external = DefaultHasher::new();
    "mc-m1-external-v1".hash(&mut external);
    workspace_fingerprint.hash(&mut external);

    Ok(M1RevisionSignal {
        revision: in_session.finish() | 1,
        external_revision: external.finish() | 1,
        max_compartment_seq,
        max_memory_id,
        max_memory_mutation_id,
        note_status_version,
        user_profile_version,
    })
}

/// The composed m1 delta: its body, and, when a newly-published compartment extends the
/// m0+m1 coverage, the new coverage anchor the SOFT must advance to (boundary id +
/// ordinal). `new_coverage` is None when only memory deltas ride (the boundary stays put,
/// the `new_boundary_id=None` SOFT path). The REVISION is NOT here — it is the cheap
/// [`m1_revision_signal`] the caller reads every pass; the body is the placeholder when
/// empty. (Keeping the revision out avoids two sources of "did m1 change".)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M1Composition {
    pub body: String,
    /// Number of memory corrections represented in the m1 body, for the pressure backstop.
    pub memory_update_count: usize,
    pub new_coverage: Option<(String, u64)>,
    pub note_deliveries: Vec<NoteDelivery>,
    /// True only when the pending profile version produced a non-empty, budgeted block.
    pub profile_rendered: bool,
}

pub fn claim_and_render_notes(
    store: &McStore,
    project_path: &str,
    session_id: &str,
    delivered_pass_fingerprint: &str,
    transform_pass_id: &str,
    now_ms: i64,
) -> Result<(String, Vec<NoteDelivery>), McStoreError> {
    let deliveries = store.claim_note_delivery(
        project_path,
        session_id,
        delivered_pass_fingerprint,
        transform_pass_id,
        now_ms,
    )?;
    let notes = deliveries
        .iter()
        .map(|(note, _)| note.clone())
        .collect::<Vec<_>>();
    Ok((
        render_note_delta(&notes),
        deliveries
            .into_iter()
            .map(|(_, delivery)| delivery)
            .collect(),
    ))
}

fn render_note_delta(notes: &[StoredNote]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut lines = vec!["<new-notes>".to_string()];
    for note in notes {
        let condition = note
            .ready_reason
            .as_deref()
            .or(note.surface_condition.as_deref())
            .unwrap_or("Condition satisfied");
        lines.push(format!(
            "- #{}: {}\n  Condition: {}",
            note.id, note.content, condition
        ));
    }
    lines.push("</new-notes>".to_string());
    lines.join("\n")
}

/// EXPENSIVE bust-only: compose the m1 delta body from the store against the watermarks
/// the last HARD froze in `meta`. `now_ms` is the frozen expiry cutoff (same as the m0
/// compose). Reads compartments + memories; never call on a defer.
#[allow(clippy::too_many_arguments)]
pub fn compose_m1_from_store(
    store: &McStore,
    project_path: &str,
    note_project_path: &str,
    session_id: &str,
    meta: &ModuleMeta,
    now_ms: i64,
    memory_enabled: bool,
    memory_budget_tokens: f64,
    user_profile_budget_tokens: f64,
    temporal_awareness: bool,
    estimate_tokens: impl Fn(&str) -> usize + Copy,
) -> Result<M1Composition, M1ComposeError> {
    // Resolve the workspace membership ONCE from the calling project (mirrors the m0
    // compose). Re-resolving from a union path is WRONG: the union list is sorted, so its
    // first element is the lexicographically-first member, NOT necessarily this project —
    // using it as `own_identity` would treat this project's own (non-shared-category)
    // memories as foreign and filter them out.
    let membership = store.resolve_workspace_membership(project_path)?;
    let paths: Vec<String> = match &membership {
        Some(m) => m.union_identities.clone(),
        None => vec![project_path.to_string()],
    };

    // --- new compartments (seq past the folded watermark) at P1 + coverage extension ---
    // Store-only ordering deliberately allows sparse ordinal gaps; transform has
    // the live array and rejects any coverage advance that would trim present,
    // uncovered input.
    let compartments = store.load_compartments(session_id)?;
    let coverage = resolve_coverage(&compartments).map_err(M1ComposeError::CoverageGap)?;
    let (_folded, new_comps) = partition_by_folded_seq(&compartments, meta.folded_compartment_seq);
    let new_comp_decay: Vec<DecayRenderCompartment> = new_comps
        .iter()
        .map(|c| {
            let mut rendered = DecayRenderCompartment::from(*c);
            if !temporal_awareness {
                rendered.start_date = None;
                rendered.end_date = None;
            }
            rendered
        })
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
    let new_memories = load_new_memories(
        store,
        membership.as_ref(),
        &paths,
        meta.max_memory_id,
        now_ms,
    )?;
    let source_name_by_id = membership
        .as_ref()
        .map(|workspace| workspace_source_names(&new_memories, workspace))
        .unwrap_or_default();
    let new_memories = trim_memories_to_budget(
        new_memories,
        None,
        &source_name_by_id,
        (memory_budget_tokens.max(1.0) * 0.25).floor().max(1.0),
        estimate_tokens,
    );
    let new_memories_block = render_memory_block(&new_memories, "new-memories", &source_name_by_id);

    // Profile rows and their version arrive together through state sync. Render the block only
    // after a version change, and leave the applied version behind when trimming leaves no body
    // to send; that makes the next real render consume the pending delta instead of losing it.
    let (new_user_profile_block, profile_rendered) = if memory_enabled
        && meta.user_profile_version != meta.m1_user_profile_version
    {
        let profile_rows = if project_path.starts_with("shadow:") {
            store.load_shadow_user_profile(project_path)?
        } else {
            store.load_active_user_memories()?
        };
        let profile =
            trim_user_profile_to_budget(profile_rows, user_profile_budget_tokens, estimate_tokens);
        let block = render_user_profile_block(&profile, "new-user-profile");
        let rendered = !block.is_empty();
        (block, rendered)
    } else {
        (String::new(), false)
    };

    // Notes intentionally do not participate in m1_revision_signal: a condition can
    // become true during a defer, but it must ride the next natural bust rather than
    // creating a cache bust of its own. Unacknowledged ledger rows are included again,
    // which is the honest at-least-once contract.
    let (notes_block, note_deliveries) = claim_and_render_notes(
        store,
        note_project_path,
        session_id,
        &format!("m1:{}:{}", meta.m1_revision, now_ms),
        &format!("m1:{}:{}", meta.m1_revision, now_ms),
        now_ms,
    )?;
    let profile_and_notes = [new_user_profile_block.as_str(), notes_block.as_str()]
        .into_iter()
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let body = assemble_m1(
        &memory_updates_block,
        &new_compartments_block,
        &new_memories_block,
        &profile_and_notes, // profile and project-owned notes share the existing m1 delta slot
        M1_PLACEHOLDER,
    );

    Ok(M1Composition {
        body,
        memory_update_count: mutations.len(),
        new_coverage,
        note_deliveries,
        profile_rendered,
    })
}

/// Active memories with `id > after_id` for the calling project (or the workspace union
/// when `membership` is Some). This loader sorts by importance then id; the compact
/// `<new-memories>` renderer canonicalizes the final category-grouped wire order.
/// Filters the active pool in memory — the new set is a small tail and this runs
/// only on a bust. `membership` is the ALREADY-RESOLVED membership for the CALLING project
/// (so own-vs-foreign visibility keys off the right own_identity); `paths[0]` is the
/// single project when there is no membership.
fn load_new_memories(
    store: &McStore,
    membership: Option<&mc_store::WorkspaceMembership>,
    paths: &[String],
    after_id: i64,
    now_ms: i64,
) -> Result<Vec<mc_store::StoredMemory>, McStoreError> {
    let all = match membership {
        // union: own (full visibility) + foreign (shared categories only), keyed off the
        // membership's own_identity = the calling project. A new own memory in a non-shared
        // category is still visible (it's own); a new foreign one follows the share policy.
        Some(m) => store.load_workspace_union_memories(m, now_ms)?,
        None => store.load_active_memories(&paths[0], now_ms)?,
    };
    Ok(all.into_iter().filter(|m| m.id > after_id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use mc_store::{InsertMemoryInput, StoredCompartment};

    fn no_estimate(_: &str) -> usize {
        0
    }

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

    fn insert_input<'a>(
        project: &'a str,
        category: &'a str,
        content: &'a str,
        now: i64,
    ) -> InsertMemoryInput<'a> {
        InsertMemoryInput {
            project_path: project,
            route_project_root: None,
            category,
            content,
            source_session_id: None,
            source_type: Some("tool"),
            importance: Some(70),
            expires_at: None,
            metadata_json: None,
            now_ms: now,
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
    fn empty_delta_is_the_placeholder_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        // a HARD folded everything (folded_seq covers all, no new memories/mutations)
        let meta = meta_after_hard(5, Some(50), 100, 9, vec![1, 2]);
        let m1 = compose_m1_from_store(
            &store,
            "git:proj",
            "git:proj",
            "ses",
            &meta,
            0,
            true,
            8_000.0,
            4_000.0,
            true,
            no_estimate,
        )
        .unwrap();
        assert_eq!(m1.body, M1_PLACEHOLDER, "no delta → the placeholder body");
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
        let m1 = compose_m1_from_store(
            &store,
            "git:proj",
            "git:proj",
            "ses",
            &meta,
            0,
            true,
            8_000.0,
            4_000.0,
            true,
            no_estimate,
        )
        .unwrap();

        // C2 rides m1 at P1, and coverage extends 10 → 20 (the SOFT advances the anchor)
        assert!(m1.body.contains("<new-compartments>"), "{}", m1.body);
        assert!(m1.body.contains("## 11-20 · C2") && !m1.body.contains("## 1-10 · C1"));
        assert!(m1.body.contains("P1-2"), "rides at P1: {}", m1.body);
        assert_eq!(m1.new_coverage, Some(("m20".to_string(), 20)));
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
        let m1 = compose_m1_from_store(
            &store,
            "git:proj",
            "git:proj",
            "ses",
            &meta,
            0,
            true,
            8_000.0,
            4_000.0,
            true,
            no_estimate,
        )
        .unwrap();

        assert!(m1.body.contains("<new-memories>"), "{}", m1.body);
        assert!(m1.body.contains("new mem"));
        // no new compartment → coverage does NOT extend (the None-boundary SOFT path)
        assert_eq!(
            m1.new_coverage, None,
            "memory-only delta keeps the boundary put"
        );
    }

    #[test]
    fn public_memory_ports_drive_m1_revision_and_delta_blocks() {
        let project = "git:proj";

        for case in ["update", "archive", "merge"] {
            let dir = tempfile::tempdir().unwrap();
            let store = McStore::open(&descriptor(dir.path())).unwrap();
            store
                .replace_compartments("ses", &[comp(1, 1, 10, "m10")])
                .unwrap();
            let target = store
                .insert_memory(insert_input(project, "CONSTRAINTS", "original", 1))
                .unwrap();
            let merge_source = (case == "merge").then(|| {
                store
                    .insert_memory(insert_input(project, "CONSTRAINTS", "duplicate", 1))
                    .unwrap()
            });
            let mut manifest = vec![target];
            if let Some(source) = merge_source {
                manifest.push(source);
            }
            let max_mem = store.max_memory_id(&[project.to_string()]).unwrap();
            let cursor = store
                .max_memory_mutation_id(&[project.to_string()])
                .unwrap();
            let before_signal = m1_revision_signal(&store, project, "ses").unwrap();

            match case {
                "update" => {
                    store
                        .update_memory_content(project, target, "corrected", 2)
                        .unwrap();
                }
                "archive" => {
                    store
                        .archive_memory(project, target, Some("obsolete"), 2)
                        .unwrap();
                }
                "merge" => {
                    store
                        .merge_memories(project, target, &[merge_source.unwrap()], "merged", 2)
                        .unwrap();
                }
                _ => unreachable!(),
            }

            let after_signal = m1_revision_signal(&store, project, "ses").unwrap();
            assert_ne!(
                before_signal, after_signal,
                "{case} must move the m1 signal"
            );
            let meta = meta_after_hard(1, Some(10), max_mem, cursor, manifest);
            let m1 = compose_m1_from_store(
                &store,
                project,
                project,
                "ses",
                &meta,
                0,
                true,
                8_000.0,
                4_000.0,
                true,
                no_estimate,
            )
            .unwrap();
            assert!(m1.body.contains("<memory-updates>"), "{case}: {}", m1.body);
            assert_eq!(
                m1.new_coverage, None,
                "memory-only mutations do not extend coverage"
            );
        }
    }

    #[test]
    fn public_insert_renders_new_memories_without_mutation_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let project = "git:proj";
        store
            .replace_compartments("ses", &[comp(1, 1, 10, "m10")])
            .unwrap();
        let before_signal = m1_revision_signal(&store, project, "ses").unwrap();
        let cursor = store
            .max_memory_mutation_id(&[project.to_string()])
            .unwrap();

        store
            .insert_memory(insert_input(project, "CONSTRAINTS", "brand new", 1))
            .unwrap();
        let after_signal = m1_revision_signal(&store, project, "ses").unwrap();
        assert_ne!(before_signal, after_signal, "insert moves max_memory_id");
        assert_eq!(
            store
                .max_memory_mutation_id(&[project.to_string()])
                .unwrap(),
            cursor,
            "additive inserts do not write the mutation log"
        );
        let meta = meta_after_hard(1, Some(10), 0, cursor, vec![]);
        let m1 = compose_m1_from_store(
            &store,
            project,
            project,
            "ses",
            &meta,
            0,
            true,
            8_000.0,
            4_000.0,
            true,
            no_estimate,
        )
        .unwrap();
        assert!(m1.body.contains("<new-memories>"), "{}", m1.body);
        assert!(m1.body.contains("brand new"), "{}", m1.body);
    }

    #[test]
    fn new_own_memory_rides_m1_when_project_is_not_first_in_workspace() {
        // Regression: the calling project sorts SECOND in the workspace union (the union is
        // sorted ASC), and adds a NEW own memory in a NON-SHARED category. The new-memories
        // read must resolve own-visibility from the CALLING project, not the union's first
        // member — else this own memory is wrongly treated as foreign and filtered out
        // while the digest still advanced, leaving a silently stale m1.
        let dir = tempfile::tempdir().unwrap();
        let store = McStore::open(&descriptor(dir.path())).unwrap();
        let own = "git:zzz-own"; // sorts AFTER the foreign member
        let foreign = "git:aaa-foreign";
        // a workspace sharing ONLY CONSTRAINTS; the new memory is ARCHITECTURE (non-shared)
        store
            .seed_workspace_member("ws", own, "[\"CONSTRAINTS\"]")
            .unwrap();
        store
            .seed_workspace_member("ws", foreign, "[\"CONSTRAINTS\"]")
            .unwrap();
        store
            .replace_compartments("ses", &[comp(1, 1, 10, "m10")])
            .unwrap();
        // a new OWN memory id 5, ARCHITECTURE (NOT a shared category), past the folded max
        store
            .seed_memory(5, own, "ARCHITECTURE", "own arch rule", 70)
            .unwrap();

        let meta = meta_after_hard(1, Some(10), 0, 0, vec![]);
        let m1 = compose_m1_from_store(
            &store,
            own,
            own,
            "ses",
            &meta,
            0,
            true,
            8_000.0,
            4_000.0,
            true,
            no_estimate,
        )
        .unwrap();
        assert!(
            m1.body.contains("own arch rule"),
            "the calling project's own non-shared new memory must ride m1 even when it is \
             not the union's first member: {}",
            m1.body
        );
        // the digest detects it too (MAX(id) over the union, no visibility filter), so the
        // body now AGREES with what the digest moved on — no silent stale m1.
        let before = {
            let s = McStore::open(&descriptor(&dir.path().join("probe"))).unwrap();
            s.seed_workspace_member("ws", own, "[\"CONSTRAINTS\"]")
                .unwrap();
            s.seed_workspace_member("ws", foreign, "[\"CONSTRAINTS\"]")
                .unwrap();
            m1_revision_signal(&s, own, "ses").unwrap()
        };
        assert_ne!(
            before,
            m1_revision_signal(&store, own, "ses").unwrap(),
            "the new memory advances the digest"
        );
    }
}
