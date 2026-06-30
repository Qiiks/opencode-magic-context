//! Compartment coverage resolution: the contiguity-validated fold cut + the
//! folded-vs-new partition for the m0/m1 split.
//!
//! Pure over a chronological compartment list (the order
//! [`mc_store::McStore::load_compartments`] returns). Two settled, handshake-independent
//! pieces of the slice-4d-m0 spine:
//!  - [`resolve_coverage`] validates the compartments TILE the covered ordinal range
//!    with no gap, then reports the coverage end (last compartment's end_message /
//!    end_message_id) — the m0+m1 coverage anchor. A gap is FAIL-LOUD: silently taking
//!    `last.end` as coverage would drop the gap's raw messages from the tail (they are
//!    neither summarized nor carried), an unrecoverable loss.
//!  - [`partition_by_folded_seq`] splits the compartments into the set already inside m0
//!    (`sequence <= folded_seq`) and the new ones riding m1 at P1 (`sequence > folded_seq`).

use mc_store::StoredCompartment;

/// Hashes for the parts of the m0 baseline whose change means the frozen m0 bytes have
/// changed AND that have no cheaper correction, so the pass must re-render m0 (a HARD
/// fold). Folded into the render_config string the classifier compares each pass: when
/// any of these differ, the composed render_config differs, and the classifier's
/// existing "render_config changed → HARD" rule fires. Kept as NAMED fields (not one
/// combined hash) so a diff of the composed string shows WHICH part changed (useful for
/// telemetry); equality cost is the same, since any field change still flips the string.
///
/// Only MONOLITHIC wholesale content with no cheap correction belongs here. TWO classes
/// are deliberately EXCLUDED:
///  - DISCRETE itemized content (id'd memories, sequenced compartments, additive profile
///    entries) — forward-corrected on the m1 delta (a new memory appends, an in-session
///    memory edit rides a `<memory-updates>` correction); a SOFT that leaves m0 frozen.
///  - PROJECT-DOCS — even though docs are monolithic m0 content, a docs-only edit must
///    NOT evict the cached prefix (it's a low-value, frequent edit). Docs fold into m0
///    on the NEXT natural HARD (from another cause), which re-reads them from disk; the
///    docs hash is a SNAPSHOT MARKER persisted with the rendered m0 bytes for
///    observability, NOT a trigger. So `docs_hash` is intentionally NOT a field here.
///
/// Including any of these would force a full m0 re-render on a routine event — the exact
/// over-bust the m1 delta path and the docs-defer-fold exist to avoid.
///
/// THE RULE that decides what belongs here (so docs_hash isn't re-added "to be more
/// correct"): content-vs-composition. A stale CONTENT block inside an unchanged m0
/// composition is tolerable for a few passes — it folds in on the next natural HARD, no
/// HARD-on-its-own needed (project-docs is exactly this). But a stale COMPOSITION or
/// STRUCTURE marker means m0 is built WRONG: a stale workspace_fingerprint → m0 composed
/// over the wrong project set; a stale upgrade_state → m0 in an incompatible format; a
/// stale external memory epoch → m0 missing an out-of-process edit it can't see any other
/// way. Composition/structure staleness can't be tolerated, so it HARDs. Content
/// staleness defer-folds. Only composition/structure markers belong in this struct.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct M0ContentEpoch {
    /// The workspace membership/policy fingerprint (member identities + their epochs +
    /// the shared-category policy): a wholesale change to which foreign memories are
    /// visible.
    pub workspace_fingerprint: String,
    /// The session-upgrade migration state. A session upgrade re-evaluates the whole
    /// memory pool into the current taxonomy; the resulting wholesale rewrite changes it.
    pub upgrade_state: String,
    /// The EXTERNAL project-memory epoch — bumped ONLY by an out-of-process editor
    /// (the dashboard) or a session-upgrade migration. An external edit is the one
    /// memory change the module can't see as a discrete mutation-log row (the editor
    /// didn't queue one), so it signals via this wholesale counter and forces a HARD.
    /// In-session memory mutations do NOT touch this — they ride the m1 correction delta
    /// (the mutation log + cursor) as a SOFT. Must NOT be derived from the mutation log,
    /// or in-session edits would HARD and the m1 correction path would be dead.
    pub memory_content_epoch: String,
}

/// Combine the base render_config (the provider-eviction triggers: system/model/
/// serializer) with the [`M0ContentEpoch`] fields into the effective render_config the
/// classifier compares. Any difference in the base OR any epoch field produces a
/// different string (→ the classifier's "render_config changed → HARD" fires). The base
/// is kept as a prefix so provider changes still trigger. Deterministic (fixed field
/// order); each field is length-prefixed so no value can forge a field boundary (e.g.
/// ("a","bc") and ("ab","c") must not collapse to the same string).
pub fn fold_m0_content_epoch(base_render_config: &str, epoch: &M0ContentEpoch) -> String {
    // length-prefix each field so no value can forge a boundary (a value containing the
    // delimiter can't masquerade as the next field).
    fn part(label: &str, value: &str) -> String {
        format!("{label}:{}:{value}", value.len())
    }
    format!(
        "{base_render_config}|m0epoch[{};{};{}]",
        part("ws", &epoch.workspace_fingerprint),
        part("upg", &epoch.upgrade_state),
        part("mem", &epoch.memory_content_epoch),
    )
}

/// The coverage summary of a contiguous compartment set: the latest sequence, the
/// terminal covered ordinal, and the boundary message id (the cache anchor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompartmentCoverage {
    /// The highest compartment `sequence` in the set.
    pub max_sequence: i64,
    /// The last compartment's `end_message` — the m0+m1 coverage end ordinal (the
    /// tail-trim point: items with a greater ordinal are the live tail).
    pub coverage_end_ordinal: u64,
    /// The last compartment's `end_message_id` — the cache/revert anchor.
    pub boundary_id: String,
}

/// A non-contiguous compartment set: a gap between two consecutive compartments would
/// drop raw messages from the tail if coverage advanced past it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageGap {
    /// The earlier compartment's end ordinal.
    pub prev_end: i64,
    /// The later compartment's start ordinal (expected `prev_end + 1`).
    pub next_start: i64,
}

impl std::fmt::Display for CoverageGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "compartment coverage gap: a compartment ends at ordinal {} but the next starts at {} (expected {}); messages {}..{} are covered by no compartment",
            self.prev_end,
            self.next_start,
            self.prev_end + 1,
            self.prev_end + 1,
            self.next_start - 1
        )
    }
}

/// Validate the compartments tile contiguously and report the coverage summary. None
/// when the set is empty (nothing covered). Errors on the FIRST gap found.
///
/// Contiguity rule: for consecutive compartments (chronological), the later one must
/// start exactly one ordinal past the earlier one's end (`next.start == prev.end + 1`).
/// An overlap (`next.start <= prev.end`) or a gap (`next.start > prev.end + 1`) both fail
/// — the historian emits contiguous non-overlapping ranges, so either is corruption.
pub fn resolve_coverage(
    compartments: &[StoredCompartment],
) -> Result<Option<CompartmentCoverage>, CoverageGap> {
    let Some(first) = compartments.first() else {
        return Ok(None);
    };
    let mut prev = first;
    for next in &compartments[1..] {
        if next.start_message != prev.end_message + 1 {
            return Err(CoverageGap {
                prev_end: prev.end_message,
                next_start: next.start_message,
            });
        }
        prev = next;
    }
    let last = compartments.last().expect("non-empty checked above");
    Ok(Some(CompartmentCoverage {
        max_sequence: compartments.iter().map(|c| c.sequence).max().unwrap_or(0),
        coverage_end_ordinal: last.end_message.max(0) as u64,
        boundary_id: last.end_message_id.clone(),
    }))
}

/// Split the compartments into (folded-into-m0, riding-m1): `sequence <= folded_seq`
/// are inside the frozen m0 baseline; `sequence > folded_seq` are the new compartments
/// the m1 delta renders at P1 until the next HARD folds them. Preserves chronological
/// order in each half.
pub fn partition_by_folded_seq(
    compartments: &[StoredCompartment],
    folded_seq: i64,
) -> (Vec<&StoredCompartment>, Vec<&StoredCompartment>) {
    compartments.iter().partition(|c| c.sequence <= folded_seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp(seq: i64, start: i64, end: i64, end_id: &str) -> StoredCompartment {
        StoredCompartment {
            sequence: seq,
            start_message: start,
            end_message: end,
            end_message_id: end_id.to_string(),
            title: format!("c{seq}"),
            content: "x".into(),
            p1: Some("x".into()),
            importance: 50,
            ..Default::default()
        }
    }

    #[test]
    fn empty_set_has_no_coverage() {
        assert_eq!(resolve_coverage(&[]), Ok(None));
    }

    #[test]
    fn contiguous_set_reports_last_as_coverage() {
        let comps = vec![
            comp(1, 1, 10, "m10"),
            comp(2, 11, 20, "m20"),
            comp(3, 21, 30, "m30"),
        ];
        let cov = resolve_coverage(&comps).unwrap().unwrap();
        assert_eq!(cov.max_sequence, 3);
        assert_eq!(cov.coverage_end_ordinal, 30);
        assert_eq!(cov.boundary_id, "m30");
    }

    #[test]
    fn single_compartment_is_its_own_coverage() {
        let cov = resolve_coverage(&[comp(1, 1, 9, "m9")]).unwrap().unwrap();
        assert_eq!(cov.max_sequence, 1);
        assert_eq!(cov.coverage_end_ordinal, 9);
        assert_eq!(cov.boundary_id, "m9");
    }

    #[test]
    fn a_gap_fails_loud() {
        // covers 1-10 then 20-30 → messages 11-19 covered by nothing → must error
        let comps = vec![comp(1, 1, 10, "m10"), comp(2, 20, 30, "m30")];
        let err = resolve_coverage(&comps).unwrap_err();
        assert_eq!(err.prev_end, 10);
        assert_eq!(err.next_start, 20);
        assert!(err.to_string().contains("messages 11..19"), "{err}");
    }

    #[test]
    fn an_overlap_fails_loud() {
        // next starts at 8 but prev ended at 10 → overlap → not contiguous tiling
        let comps = vec![comp(1, 1, 10, "m10"), comp(2, 8, 15, "m15")];
        assert!(resolve_coverage(&comps).is_err());
    }

    #[test]
    fn m0_content_epoch_folds_legibly_and_deterministically() {
        let base = "sys0|tools0|model0|prof0";
        let epoch = M0ContentEpoch {
            workspace_fingerprint: "wf1".into(),
            upgrade_state: "u1".into(),
            memory_content_epoch: "mc1".into(),
        };
        let folded = fold_m0_content_epoch(base, &epoch);
        // the base is kept as a prefix (a provider change still alters the string) and
        // each epoch field appears by name so a diff shows the cause
        assert!(folded.starts_with(base));
        assert!(folded.contains("ws:3:wf1"));
        assert!(folded.contains("mem:3:mc1"));
        // docs hash is deliberately excluded from the fold (a docs-only edit must not
        // force a full m0 re-render)
        assert!(!folded.contains("docs"));
        // deterministic: same inputs → same string
        assert_eq!(folded, fold_m0_content_epoch(base, &epoch));

        // any epoch field change produces a different folded string
        let mut e2 = epoch.clone();
        e2.memory_content_epoch = "mc2".into();
        assert_ne!(folded, fold_m0_content_epoch(base, &e2));
        let mut e3 = epoch.clone();
        e3.upgrade_state = "u2".into();
        assert_ne!(folded, fold_m0_content_epoch(base, &e3));

        // length-prefix prevents a delimiter-forging collision: ("a","bc") vs ("ab","c")
        // for adjacent fields must NOT collapse to the same token.
        let forge_a = M0ContentEpoch {
            workspace_fingerprint: "a".into(),
            upgrade_state: "bc".into(),
            ..Default::default()
        };
        let forge_b = M0ContentEpoch {
            workspace_fingerprint: "ab".into(),
            upgrade_state: "c".into(),
            ..Default::default()
        };
        assert_ne!(
            fold_m0_content_epoch(base, &forge_a),
            fold_m0_content_epoch(base, &forge_b)
        );
    }

    #[test]
    fn partition_splits_at_folded_seq() {
        let comps = vec![
            comp(1, 1, 10, "m10"),
            comp(2, 11, 20, "m20"),
            comp(3, 21, 30, "m30"),
        ];
        let (folded, new) = partition_by_folded_seq(&comps, 1);
        assert_eq!(
            folded.iter().map(|c| c.sequence).collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            new.iter().map(|c| c.sequence).collect::<Vec<_>>(),
            vec![2, 3]
        );

        // folded_seq 0 (bootstrap) → everything is new (rides m1 until first fold)
        let (folded0, new0) = partition_by_folded_seq(&comps, 0);
        assert!(folded0.is_empty());
        assert_eq!(new0.len(), 3);

        // folded_seq at/past the max → nothing new (all folded)
        let (folded_all, new_all) = partition_by_folded_seq(&comps, 3);
        assert_eq!(folded_all.len(), 3);
        assert!(new_all.is_empty());
    }
}
