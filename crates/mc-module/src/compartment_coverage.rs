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
