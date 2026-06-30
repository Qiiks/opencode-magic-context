//! Magic Context cache-stability core.
//!
//! Origin-agnostic: consumes already-decoded CK items, classifies each transform
//! pass into a cache [`Action`], renders the full baseline on a Hard fold, and
//! drives `cortexkit-cache-core`'s [`CoreState`]. Slice-1 is the spine only
//! (full-baseline freeze + Hard fold + SoftPlus replay); reduction (the only
//! `Soft` producer) lands in a later slice, so this classifier never returns
//! [`Action::Soft`].

#![forbid(unsafe_code)]

pub use cortexkit_cache_core::{
    Action, CoreState, DurabilityClass, FrozenUnit, PassInput, StepResult,
};

/// A decoded CK conversation item the renderer can freeze. Origin-agnostic: the
/// harness/codec produces these; the core never parses provider/harness wire bytes.
pub trait CkItem {
    /// Stable identity. The coverage boundary is expressed as one of these ids.
    fn id(&self) -> &str;
    /// Monotonic absolute ordinal — strictly increasing across the lineage, NEVER
    /// positional (the window start moves; the ordinal does not). The coverage
    /// watermark is compared against this.
    fn ordinal(&self) -> u64;
    /// Opaque byte-complete rendering of this item: the source of the frozen
    /// payload. The core concatenates these and never interprets them.
    fn bytes(&self) -> &str;
}

/// Signals the harness extracts from a pass, fed to [`classify`]. The core then
/// executes the chosen [`Action`].
#[derive(Debug, Clone, Default)]
pub struct ClassifierInput {
    /// Has a baseline ever been materialized? (`module_meta.initialized`). False on
    /// a fresh session — forces a bootstrap Hard so a baseline always exists before
    /// any defer can replay it.
    pub initialized: bool,
    /// Does the current render-config differ from the persisted one? An epoch-class
    /// change (model key, system-prompt hash, project-memory epoch, …) whose
    /// baseline bytes differ → Hard.
    pub render_config_changed: bool,
    /// Is the live coverage boundary token present in the incoming CK array?
    pub boundary_present: bool,
    /// The prior-pass reconcile flag (`CoreState.reconcile_pending`): an earlier
    /// defer lost the boundary because a revert removed it.
    pub reconcile_pending: bool,
}

/// Ordered first-match pass classifier.
///
/// Hard triggers are evaluated BEFORE the boundary-absent path so a pass that is
/// both boundary-absent AND an epoch change folds Hard (rematerializes) instead of
/// mis-deferring. The slice-1 spine never returns [`Action::Soft`] — reduction is
/// the only `Soft` producer and lands later.
pub fn classify(input: &ClassifierInput) -> Action {
    // 1. Bootstrap: no baseline yet → Hard (materialize the first baseline).
    if !input.initialized {
        return Action::Hard;
    }
    // 2. Render-config epoch change → Hard (baseline bytes differ).
    if input.render_config_changed {
        return Action::Hard;
    }
    // 3. Reconcile forced-Hard: a prior revert removed the boundary and it is STILL
    //    absent → rematerialize against the live (shorter) array. If the boundary
    //    returned (the user undid the revert), this is skipped and the next defer
    //    clears reconcile_pending naturally — folds once, never re-fires.
    if input.reconcile_pending && !input.boundary_present {
        return Action::Hard;
    }
    // 4. Default: no baseline change → defer; replay frozen bytes verbatim.
    Action::SoftPlus
}

/// Render the full baseline as a single `synthesized-region` frozen unit covering
/// all `items`. Slice-1 freezes the whole compacted prefix as one lineage unit; the
/// reduction slice will emit finer drop / strip / skeleton units.
pub fn render_baseline<I: CkItem>(items: &[I]) -> Vec<FrozenUnit> {
    if items.is_empty() {
        return Vec::new();
    }
    let payload: String = items.iter().map(CkItem::bytes).collect();
    vec![FrozenUnit {
        key: "baseline".to_string(),
        kind: "synthesized-region".to_string(),
        frozen_payload: payload,
        durability_class: DurabilityClass::Lineage,
        reset_rule: String::new(),
    }]
}

/// The boundary id minted for a rendered baseline: the id of the terminal covered
/// item. A Hard fold sets `PassInput.new_boundary_id` to this; later passes test
/// boundary presence by finding this id in the live array.
pub fn boundary_id<I: CkItem>(items: &[I]) -> Option<String> {
    items.last().map(|i| i.id().to_string())
}

/// The coverage watermark for a rendered baseline: the terminal (highest) ordinal
/// covered. A monotonic absolute ordinal, never positional. On a revert-Hard this
/// can DECREASE (the live array is shorter) — it is always the CURRENT terminal,
/// never `max(old, new)`.
pub fn coverage_ordinal<I: CkItem>(items: &[I]) -> Option<u64> {
    items.last().map(CkItem::ordinal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_when_uninitialized_is_hard() {
        let input = ClassifierInput {
            initialized: false,
            ..Default::default()
        };
        assert_eq!(classify(&input), Action::Hard);
    }

    #[test]
    fn render_config_change_is_hard() {
        let input = ClassifierInput {
            initialized: true,
            render_config_changed: true,
            boundary_present: true,
            reconcile_pending: false,
        };
        assert_eq!(classify(&input), Action::Hard);
    }

    #[test]
    fn reconcile_with_boundary_still_absent_is_hard() {
        let input = ClassifierInput {
            initialized: true,
            render_config_changed: false,
            boundary_present: false,
            reconcile_pending: true,
        };
        assert_eq!(classify(&input), Action::Hard);
    }

    #[test]
    fn reconcile_but_boundary_returned_defers() {
        // The user undid the revert: boundary is back, so do NOT re-fold — a defer
        // clears reconcile_pending naturally (folds once, never re-fires).
        let input = ClassifierInput {
            initialized: true,
            render_config_changed: false,
            boundary_present: true,
            reconcile_pending: true,
        };
        assert_eq!(classify(&input), Action::SoftPlus);
    }

    #[test]
    fn steady_state_defers() {
        let input = ClassifierInput {
            initialized: true,
            render_config_changed: false,
            boundary_present: true,
            reconcile_pending: false,
        };
        assert_eq!(classify(&input), Action::SoftPlus);
    }

    struct Item {
        id: String,
        ordinal: u64,
        bytes: String,
    }
    impl CkItem for Item {
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

    fn items() -> Vec<Item> {
        vec![
            Item {
                id: "a".into(),
                ordinal: 10,
                bytes: "AA".into(),
            },
            Item {
                id: "b".into(),
                ordinal: 20,
                bytes: "BB".into(),
            },
        ]
    }

    #[test]
    fn baseline_concatenates_bytes_as_one_lineage_unit() {
        let units = render_baseline(&items());
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].frozen_payload, "AABB");
        assert_eq!(units[0].kind, "synthesized-region");
        assert_eq!(units[0].durability_class, DurabilityClass::Lineage);
    }

    #[test]
    fn boundary_and_coverage_are_terminal_not_max() {
        assert_eq!(boundary_id(&items()).as_deref(), Some("b"));
        assert_eq!(coverage_ordinal(&items()), Some(20));
        // A shorter (reverted) array yields the SMALLER terminal — never the max.
        let reverted = &items()[..1];
        assert_eq!(coverage_ordinal(reverted), Some(10));
    }

    #[test]
    fn empty_renders_nothing() {
        let empty: Vec<Item> = Vec::new();
        assert!(render_baseline(&empty).is_empty());
        assert_eq!(boundary_id(&empty), None);
        assert_eq!(coverage_ordinal(&empty), None);
    }
}
