//! The transform op: the CK-in / CK-out cache-stability transform.
//!
//! Emits the rewritten array: `pass_output.ck_messages = [m0, m1] ++ tail`.
//! The covered contiguous prefix is REPLACED by two frozen synthesized-region blocks
//! (m0 cumulative baseline, frozen between HARD folds; m1 volatile delta, re-rendered
//! on SOFT); the live tail (after the coverage watermark) is carried verbatim.
//!
//! mc-module OWNS the render/splice; mc-core stays the pure classifier and
//! cortexkit-cache-core stays "dumb" (freezes whatever rendered units it is handed).
//!
//! Cache discipline: render byte-complete units ONLY on bust passes; replay verbatim
//! on defer; a pure defer (boundary present, no delta) writes nothing. Two paired
//! poison-resistance invariants: synthetic items are stripped before any boundary /
//! coverage / tail computation (PRIMARY), and the `mc_*` id namespace is reserved
//! (BACKSTOP) so a synthetic block can never masquerade as the real boundary.

use mc_core::{classify, CkItem, ClassifierInput, CoreState, FrozenUnit, PassInput, PassPlan};
use mc_store::{McStore, McStoreError, ModuleMeta};
use serde::{Deserialize, Serialize};

/// Max CAS retries before surfacing the conflict (the module is the single writer in
/// the daemon case, so this rarely loops; the shared-store case re-loads and re-steps).
const MAX_CAS_RETRIES: u32 = 8;

/// Reserved synthetic-block ids (never carried by a real conversation item).
const M0_ID: &str = "mc_m0";
const M1_ID: &str = "mc_m1";
/// The reserved id prefix: a non-synthetic item bearing it is a contract violation.
const RESERVED_ID_PREFIX: &str = "mc_";
/// The non-empty m1 placeholder (never fully empty — cache-breakpoint structure).
const M1_PLACEHOLDER: &str = "(no new content since last materialization)";
const SYNTH_REGION_KIND: &str = "synthesized-region";

/// A CK item on the wire: opaque id + monotonic ordinal + byte-complete rendering +
/// the synthetic-block flag (wire-envelope metadata, NOT part of the frozen bytes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CkItemWire {
    pub id: String,
    pub ordinal: u64,
    pub bytes: String,
    #[serde(default)]
    pub synthetic: bool,
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
    fn synthetic(&self) -> bool {
        self.synthetic
    }
}

/// The m1 delta content + its byte-affecting digest. `revision` is a digest over ALL
/// byte-affecting m1 render inputs such that `render` is a pure function of what the
/// digest covers: if the rendered bytes would differ, `revision` differs. NEVER a
/// max-id counter (a same-id update changes bytes without raising a max id).
/// `revision == 0` is the placeholder (no delta).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct M1Content {
    pub revision: u64,
    pub body: String,
}

/// Decision inputs for a pass: the contiguous cut point, the current m1 delta content,
/// and whether a hard-fold trigger fired. These come from the module's own decision
/// logic in production; the wire handler can also build them from an optional test-only
/// `_decider` request field (absent → the all-default spine behavior).
/// `Deserialize` with field defaults so a partial `_decider` body fills the rest.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeciderInputs {
    pub hard_fold_requested: bool,
    /// The contiguous cut for a fold (None = cover-all / bootstrap).
    pub fold_through_ordinal: Option<u64>,
    /// The FULL current m1 delta content, re-derived each pass (None = no delta).
    pub m1_content: Option<M1Content>,
}

/// A transform pass request. `boundary_present` is deliberately NOT a field: it is a
/// cache-correctness decision (replay-frozen vs reconcile) that the module computes
/// from its own durable state, never caller-supplied (a caller-supplied value would be
/// a poison surface — a crafted array could force a wrong replay or reconcile).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformRequest {
    pub session_id: String,
    pub render_config: String,
    pub items: Vec<CkItemWire>,
}

/// A transform pass result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransformResponse {
    pub action: String,
    pub boundary_id: String,
    pub reconcile_pending: bool,
    pub version: u64,
    pub row_version: u64,
    pub committed: bool,
    /// THE REAL OUTPUT: `[m0, m1] ++ tail`.
    pub ck_messages: Vec<CkItemWire>,
}

/// Transform errors. Each leaves the durable frozen-set UNCHANGED (the CAS simply does
/// not advance), so the next pass replays the last good state or busts cleanly; the
/// handler maps these to a clean Error frame rather than a partial/raw array.
#[derive(Debug)]
pub enum TransformError {
    Store(McStoreError),
    /// Live-source ordinals must be unique + strictly increasing.
    OrdinalViolation,
    /// A non-synthetic item used a reserved `mc_*` id.
    ReservedId,
    /// An unknown / corrupt frozen-set shape (never destructively cleared).
    UnknownShape(&'static str),
    /// A HARD would fold but the re-derived m1 content is missing while the prior m1
    /// was non-placeholder — folding would silently drop live m1 content.
    HardWouldDropM1,
}

impl std::fmt::Display for TransformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransformError::Store(e) => write!(f, "store: {e}"),
            TransformError::OrdinalViolation => {
                write!(f, "live-source ordinals not strictly increasing")
            }
            TransformError::ReservedId => write!(f, "non-synthetic item used a reserved mc_* id"),
            TransformError::UnknownShape(m) => write!(f, "unknown frozen-set shape: {m}"),
            TransformError::HardWouldDropM1 => {
                write!(f, "hard fold would drop non-placeholder m1 content")
            }
        }
    }
}
impl std::error::Error for TransformError {}
impl From<McStoreError> for TransformError {
    fn from(e: McStoreError) -> Self {
        TransformError::Store(e)
    }
}

/// Apply one transform pass, retrying the whole load→classify→step→commit cycle on a
/// CAS conflict (re-classification depends on the freshly-loaded state).
pub fn transform(
    store: &McStore,
    req: &TransformRequest,
    deciders: &DeciderInputs,
) -> Result<TransformResponse, TransformError> {
    let mut attempt = 0;
    loop {
        match apply_once(store, req, deciders) {
            Err(TransformError::Store(McStoreError::CasConflict { .. }))
                if attempt < MAX_CAS_RETRIES =>
            {
                attempt += 1;
                continue;
            }
            other => return other,
        }
    }
}

fn apply_once(
    store: &McStore,
    req: &TransformRequest,
    deciders: &DeciderInputs,
) -> Result<TransformResponse, TransformError> {
    // --- ingress: strip synthetic, reserve mc_*, validate ordinals (over live source) ---
    let live: Vec<&CkItemWire> = req.items.iter().filter(|i| !i.synthetic()).collect();
    for item in &live {
        if item.id().starts_with(RESERVED_ID_PREFIX) {
            return Err(TransformError::ReservedId);
        }
    }
    let mut prev: Option<u64> = None;
    for item in &live {
        if let Some(p) = prev {
            if item.ordinal() <= p {
                return Err(TransformError::OrdinalViolation);
            }
        }
        prev = Some(item.ordinal());
    }

    let loaded = store.load(&req.session_id)?;

    // --- boundary presence: computed over live source vs durable boundary_id ---
    let boundary_present = !loaded.core.boundary_id.is_empty()
        && live.iter().any(|i| i.id() == loaded.core.boundary_id);
    let boundary_token = if boundary_present {
        loaded.core.boundary_id.clone()
    } else {
        "-".to_string()
    };

    // --- classify ---
    let incoming_m1_rev = deciders
        .m1_content
        .as_ref()
        .map(|m| m.revision)
        .unwrap_or(0);
    let plan = classify(&ClassifierInput {
        initialized: loaded.meta.initialized,
        is_legacy_baseline: is_legacy_baseline(&loaded.core),
        valid_m0m1_shape: valid_m0m1_shape(&loaded.core),
        render_config_changed: loaded.meta.initialized
            && req.render_config != loaded.meta.last_render_config,
        hard_fold_requested: deciders.hard_fold_requested,
        boundary_present,
        reconcile_pending: loaded.core.reconcile_pending,
        m1_revision_changed: incoming_m1_rev != loaded.meta.m1_revision,
    });

    let mut core = loaded.core.clone();
    let mut meta = loaded.meta.clone();

    match plan {
        PassPlan::Reject(m) => return Err(TransformError::UnknownShape(m)),
        PassPlan::Hard | PassPlan::MigrateHard => {
            // Hard-render m1 guard (release): a fold must not silently drop a
            // non-placeholder m1 when its re-derived content is absent.
            if loaded.meta.m1_revision != 0 && deciders.m1_content.is_none() {
                return Err(TransformError::HardWouldDropM1);
            }
            if plan == PassPlan::MigrateHard {
                // The ONLY destructive clear, behind the exact legacy predicate. The
                // core's apply_units cannot remove a key, so clear before step_hard to
                // avoid `[baseline, m0, m1]` pollution. In-memory only, pre-CAS.
                core.frozen_units.clear();
                core.pending_changes.clear();
            }
            let covered = covered_items(&live, deciders.fold_through_ordinal);
            let rendered = render_hard(&covered, deciders.m1_content.as_ref());
            let new_boundary = covered.last().map(|i| i.id().to_string());
            let new_coverage = covered.last().map(|i| i.ordinal());

            core.step(PassInput {
                proposed: Some(mc_core::Action::Hard),
                boundary_present: boundary_token,
                rendered_units: rendered,
                new_boundary_id: new_boundary,
                queued: Vec::new(),
                run_started: false,
            });
            meta.initialized = true;
            meta.last_render_config = req.render_config.clone();
            meta.coverage_ordinal = new_coverage;
            meta.m1_revision = 0; // m1 folded into m0 + reset to placeholder
        }
        PassPlan::Soft => {
            core.step(PassInput {
                proposed: Some(mc_core::Action::Soft),
                boundary_present: boundary_token,
                rendered_units: vec![render_m1(deciders.m1_content.as_ref())],
                new_boundary_id: None,
                queued: Vec::new(),
                run_started: false,
            });
            meta.m1_revision = incoming_m1_rev;
        }
        PassPlan::Defer => {
            core.step(PassInput {
                proposed: Some(mc_core::Action::SoftPlus),
                boundary_present: boundary_token,
                ..Default::default()
            });
            // meta unchanged
        }
    }

    let result_action = action_str(&plan, &core);

    // Conditional commit: write only when durable state changed (a pure defer with
    // the boundary present mutates nothing → no write).
    let changed = core != loaded.core || meta != loaded.meta;
    let row_version = if changed {
        store.commit(&req.session_id, loaded.row_version, &core, &meta)?
    } else {
        loaded.row_version.unwrap_or(0)
    };

    let ck_messages = build_output(&core, &meta, &live);

    Ok(TransformResponse {
        action: result_action,
        boundary_id: core.boundary_id.clone(),
        reconcile_pending: core.reconcile_pending,
        version: core.version,
        row_version,
        committed: changed,
        ck_messages,
    })
}

// --- shape predicates (mc-module reads the concrete frozen set; mc-core stays blind) ---

fn is_legacy_baseline(core: &CoreState) -> bool {
    core.frozen_units.len() == 1
        && core.frozen_units[0].key == "baseline"
        && core.pending_changes.is_empty()
}

fn valid_m0m1_shape(core: &CoreState) -> bool {
    core.frozen_units
        .iter()
        .all(|u| u.key == "m0" || u.key == "m1")
}

// --- render helpers (the ONLY producers of frozen bytes) ---

/// The covered contiguous prefix: items with `ordinal <= fold_through` (None =
/// cover-all). The slice is sorted by ordinal so m0 bytes are deterministic
/// regardless of input iteration order (a nondeterministic order = phantom HARD).
fn covered_items<'a>(live: &[&'a CkItemWire], fold_through: Option<u64>) -> Vec<&'a CkItemWire> {
    let mut covered: Vec<&CkItemWire> = match fold_through {
        Some(cut) => live
            .iter()
            .copied()
            .filter(|i| i.ordinal() <= cut)
            .collect(),
        None => live.to_vec(),
    };
    covered.sort_by_key(|i| i.ordinal());
    covered
}

/// Render m0 (covered concat in ordinal order ++ folded m1 content) + an m1
/// placeholder. The fold carries the current m1 content INTO m0 and resets m1.
fn render_hard(covered: &[&CkItemWire], fold_in: Option<&M1Content>) -> Vec<FrozenUnit> {
    let mut m0: String = covered.iter().map(|i| i.bytes()).collect();
    if let Some(c) = fold_in {
        if c.revision != 0 {
            m0.push_str(&c.body);
        }
    }
    vec![synth_region("m0", m0), render_m1(None)]
}

/// Render the m1 delta unit (replaces the m1 key on a Soft). None / revision 0 → the
/// non-empty placeholder.
fn render_m1(content: Option<&M1Content>) -> FrozenUnit {
    let payload = match content {
        Some(c) if c.revision != 0 => c.body.clone(),
        _ => M1_PLACEHOLDER.to_string(),
    };
    synth_region("m1", payload)
}

fn synth_region(key: &str, payload: String) -> FrozenUnit {
    FrozenUnit {
        key: key.to_string(),
        kind: SYNTH_REGION_KIND.to_string(),
        frozen_payload: payload,
        durability_class: mc_core::DurabilityClass::Lineage,
        reset_rule: String::new(),
    }
}

// --- output splice: [m0, m1] ++ tail(by coverage_ordinal) ---

fn build_output(core: &CoreState, meta: &ModuleMeta, live: &[&CkItemWire]) -> Vec<CkItemWire> {
    let mut out = Vec::with_capacity(2 + live.len());
    if let Some(u) = core.frozen_units.iter().find(|u| u.key == "m0") {
        out.push(synth_wire(M0_ID, &u.frozen_payload));
    }
    if let Some(u) = core.frozen_units.iter().find(|u| u.key == "m1") {
        out.push(synth_wire(M1_ID, &u.frozen_payload));
    }
    // Tail: live items strictly after the coverage watermark (disjoint from covered).
    let cutoff = meta.coverage_ordinal;
    for item in live {
        if cutoff.is_none_or(|c| item.ordinal() > c) {
            out.push((*item).clone());
        }
    }
    out
}

fn synth_wire(id: &str, bytes: &str) -> CkItemWire {
    CkItemWire {
        id: id.to_string(),
        ordinal: 0,
        bytes: bytes.to_string(),
        synthetic: true,
    }
}

fn action_str(plan: &PassPlan, _core: &CoreState) -> String {
    match plan {
        PassPlan::Hard | PassPlan::MigrateHard => "HARD",
        PassPlan::Soft => "SOFT",
        PassPlan::Defer => "SOFT+",
        PassPlan::Reject(_) => "ERROR",
    }
    .to_string()
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
            synthetic: false,
        }
    }

    fn req(session: &str, cfg: &str, items: Vec<CkItemWire>) -> TransformRequest {
        TransformRequest {
            session_id: session.to_string(),
            render_config: cfg.to_string(),
            items,
        }
    }

    fn spine() -> DeciderInputs {
        DeciderInputs::default()
    }

    fn m0_bytes(r: &TransformResponse) -> &str {
        &r.ck_messages.iter().find(|i| i.id == M0_ID).unwrap().bytes
    }
    fn m1_bytes(r: &TransformResponse) -> &str {
        &r.ck_messages.iter().find(|i| i.id == M1_ID).unwrap().bytes
    }
    fn tail_ids(r: &TransformResponse) -> Vec<&str> {
        r.ck_messages
            .iter()
            .filter(|i| !i.synthetic)
            .map(|i| i.id.as_str())
            .collect()
    }

    #[test]
    fn bootstrap_emits_m0_m1_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let r = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
            &spine(),
        )
        .unwrap();
        assert_eq!(r.action, "HARD");
        assert_eq!(r.boundary_id, "a");
        assert_eq!(m0_bytes(&r), "<h>BASE</h>");
        assert_eq!(m1_bytes(&r), M1_PLACEHOLDER);
        assert!(tail_ids(&r).is_empty(), "all covered, tail empty");
        assert!(r.committed);
    }

    #[test]
    fn v1_growing_tail_prefix_byte_stable_tail_verbatim_no_write() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // bootstrap covers "a"
        transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "BASE")]),
            &spine(),
        )
        .unwrap();

        let mut prev_m0: Option<String> = None;
        let mut prev_m1: Option<String> = None;
        for n in 2..=5u64 {
            // tail grows each pass; boundary "a" stays present
            let mut items = vec![item("a", 1, "BASE")];
            for k in 2..=n {
                items.push(item(&format!("t{k}"), k, &format!("tail{k}")));
            }
            let r = transform(&s, &req("ses", "cfg0", items), &spine()).unwrap();
            assert_eq!(r.action, "SOFT+");
            assert!(!r.committed, "pure defer must not write");
            // prefix blocks byte-identical across the growing tail
            if let Some(p) = &prev_m0 {
                assert_eq!(m0_bytes(&r), p, "m0 changed on defer");
            }
            if let Some(p) = &prev_m1 {
                assert_eq!(m1_bytes(&r), p, "m1 changed on defer");
            }
            // tail is the verbatim live items after coverage_ordinal=1
            let expected: Vec<String> = (2..=n).map(|k| format!("t{k}")).collect();
            assert_eq!(
                tail_ids(&r),
                expected.iter().map(|s| s.as_str()).collect::<Vec<_>>()
            );
            prev_m0 = Some(m0_bytes(&r).to_string());
            prev_m1 = Some(m1_bytes(&r).to_string());
        }
    }

    #[test]
    fn v5_m1_delta_soft_m0_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "BASE")]),
            &spine(),
        )
        .unwrap();
        let before = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "BASE")]),
            &spine(),
        )
        .unwrap();

        // an m1 delta rides (boundary present)
        let deciders = DeciderInputs {
            m1_content: Some(M1Content {
                revision: 7,
                body: "<mem>rule</mem>".to_string(),
            }),
            ..Default::default()
        };
        let soft = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "BASE")]),
            &deciders,
        )
        .unwrap();
        assert_eq!(soft.action, "SOFT");
        assert_eq!(
            m0_bytes(&soft),
            m0_bytes(&before),
            "m0 must stay frozen across a SOFT"
        );
        assert_eq!(m1_bytes(&soft), "<mem>rule</mem>");
        assert!(soft.committed);

        // Defer after the delta: the decider RE-DERIVES the SAME live content each pass
        // (same revision), so the steady state is a pure SOFT+ replay, no write. (Passing
        // an EMPTY decider would mean the delta vanished — a real m1 change back to the
        // placeholder, which is a legitimate SOFT, not a defer.)
        let after = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "BASE")]),
            &deciders,
        )
        .unwrap();
        assert_eq!(after.action, "SOFT+");
        assert!(!after.committed);
        assert_eq!(m1_bytes(&after), "<mem>rule</mem>");
        assert_eq!(m0_bytes(&after), m0_bytes(&before));
    }

    #[test]
    fn v5_same_id_update_fires_on_content_digest_not_max_id() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "BASE")]),
            &spine(),
        )
        .unwrap();
        // first delta, revision 7
        let d1 = DeciderInputs {
            m1_content: Some(M1Content {
                revision: 7,
                body: "v1".into(),
            }),
            ..Default::default()
        };
        transform(&s, &req("ses", "cfg0", vec![item("a", 1, "BASE")]), &d1).unwrap();
        // a same-id UPDATE: max-id unchanged, but content (digest) changed → must Soft again
        let d2 = DeciderInputs {
            m1_content: Some(M1Content {
                revision: 8,
                body: "v2".into(),
            }),
            ..Default::default()
        };
        let r = transform(&s, &req("ses", "cfg0", vec![item("a", 1, "BASE")]), &d2).unwrap();
        assert_eq!(
            r.action, "SOFT",
            "a content change must re-bust m1 even at unchanged max-id"
        );
        assert_eq!(m1_bytes(&r), "v2");
    }

    #[test]
    fn v6_fold_only_folds_m1_into_m0_and_resets() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
            &spine(),
        )
        .unwrap();
        // m1 delta
        let d = DeciderInputs {
            m1_content: Some(M1Content {
                revision: 3,
                body: "<mem>X</mem>".into(),
            }),
            ..Default::default()
        };
        transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
            &d,
        )
        .unwrap();
        // a HARD fold: m1 folds into m0, m1 resets, new boundary minted
        let fold = DeciderInputs {
            hard_fold_requested: true,
            fold_through_ordinal: Some(2),
            m1_content: Some(M1Content {
                revision: 3,
                body: "<mem>X</mem>".into(),
            }),
        };
        let items = vec![item("a", 1, "<h>BASE</h>"), item("b", 2, "<h>MORE</h>")];
        let r = transform(&s, &req("ses", "cfg0", items), &fold).unwrap();
        assert_eq!(r.action, "HARD");
        assert_eq!(r.boundary_id, "b", "fold mints the new terminal boundary");
        assert_eq!(
            m0_bytes(&r),
            "<h>BASE</h><h>MORE</h><mem>X</mem>",
            "m1 folded into m0"
        );
        assert_eq!(m1_bytes(&r), M1_PLACEHOLDER, "m1 reset to placeholder");
    }

    #[test]
    fn v8_revert_defers_then_reconcile_rematerializes() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
            &spine(),
        )
        .unwrap();
        let before = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
            &spine(),
        )
        .unwrap();

        // revert removed the boundary item "a" — array no longer contains it
        let revert = transform(
            &s,
            &req("ses", "cfg0", vec![item("z", 5, "<h>OTHER</h>")]),
            &spine(),
        )
        .unwrap();
        assert_eq!(revert.action, "SOFT+", "revert must not bust");
        assert!(revert.reconcile_pending);
        assert_eq!(m0_bytes(&revert), m0_bytes(&before), "frozen m0 replays");
        assert!(revert.committed, "reconcile flag flip persists");

        // next pass, boundary still absent → Hard rematerialize against live array
        let remat = transform(
            &s,
            &req("ses", "cfg0", vec![item("a2", 6, "<h>REVERTED</h>")]),
            &spine(),
        )
        .unwrap();
        assert_eq!(remat.action, "HARD");
        assert_eq!(remat.boundary_id, "a2");
        assert_eq!(m0_bytes(&remat), "<h>REVERTED</h>");
        assert!(!remat.reconcile_pending);
    }

    #[test]
    fn legacy_baseline_migrates_to_clean_m0_m1() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // seed a legacy single-"baseline"-unit state directly, initialized
        let legacy_core = CoreState {
            version: 1,
            boundary_id: "a".into(),
            frozen_units: vec![synth_region("baseline", "OLD".into())],
            pending_changes: vec![],
            reconcile_pending: false,
        };
        let legacy_meta = ModuleMeta {
            initialized: true,
            last_render_config: "cfg0".into(),
            coverage_ordinal: Some(1),
            m1_revision: 0,
        };
        s.commit("ses", None, &legacy_core, &legacy_meta).unwrap();

        let r = transform(&s, &req("ses", "cfg0", vec![item("a", 1, "NEW")]), &spine()).unwrap();
        assert_eq!(
            r.action, "HARD",
            "legacy shape migrates via clear-then-Hard"
        );
        // no "baseline" residue: exactly [m0, m1] synthetic blocks
        let synth_ids: Vec<&str> = r
            .ck_messages
            .iter()
            .filter(|i| i.synthetic)
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(synth_ids, vec![M0_ID, M1_ID]);
        assert_eq!(m0_bytes(&r), "NEW");
        let reloaded = s.load("ses").unwrap();
        assert!(reloaded
            .core
            .frozen_units
            .iter()
            .all(|u| u.key == "m0" || u.key == "m1"));
    }

    #[test]
    fn unknown_shape_rejects_without_clearing() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let weird = CoreState {
            version: 1,
            boundary_id: "a".into(),
            frozen_units: vec![synth_region("junk", "??".into())],
            pending_changes: vec![],
            reconcile_pending: false,
        };
        let meta = ModuleMeta {
            initialized: true,
            last_render_config: "cfg0".into(),
            coverage_ordinal: Some(1),
            m1_revision: 0,
        };
        s.commit("ses", None, &weird, &meta).unwrap();
        let err =
            transform(&s, &req("ses", "cfg0", vec![item("a", 1, "X")]), &spine()).unwrap_err();
        assert!(matches!(err, TransformError::UnknownShape(_)));
        // durable state unchanged (the "junk" unit survives — not destructively cleared)
        let reloaded = s.load("ses").unwrap();
        assert_eq!(reloaded.core.frozen_units[0].key, "junk");
    }

    #[test]
    fn reserved_id_and_ordinal_violations_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "BASE")]),
            &spine(),
        )
        .unwrap();

        // a non-synthetic item with a reserved mc_* id
        let reserved = transform(
            &s,
            &req("ses", "cfg0", vec![item("mc_m0", 2, "x")]),
            &spine(),
        );
        assert!(matches!(reserved, Err(TransformError::ReservedId)));

        // non-monotonic ordinals
        let bad = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 5, "x"), item("b", 5, "y")]),
            &spine(),
        );
        assert!(matches!(bad, Err(TransformError::OrdinalViolation)));
    }

    #[test]
    fn synthetic_ingress_is_stripped_before_boundary_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "BASE")]),
            &spine(),
        )
        .unwrap();
        // feed our own synthetic m0 back in alongside the real array
        let mut items = vec![CkItemWire {
            id: M0_ID.into(),
            ordinal: 0,
            bytes: "STALE".into(),
            synthetic: true,
        }];
        items.push(item("a", 1, "BASE"));
        items.push(item("t2", 2, "tail2"));
        let r = transform(&s, &req("ses", "cfg0", items), &spine()).unwrap();
        // boundary "a" still found (synthetic stripped), tail filter uncorrupted
        assert_eq!(r.action, "SOFT+");
        assert_eq!(tail_ids(&r), vec!["t2"]);
    }

    #[test]
    fn restart_replays_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let bytes_m0;
        let bytes_m1;
        {
            let s = store(dir.path());
            transform(
                &s,
                &req("ses", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
                &spine(),
            )
            .unwrap();
            let r = transform(
                &s,
                &req("ses", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
                &spine(),
            )
            .unwrap();
            bytes_m0 = m0_bytes(&r).to_string();
            bytes_m1 = m1_bytes(&r).to_string();
        } // lease released
        let s2 = store(dir.path());
        let after = transform(
            &s2,
            &req("ses", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
            &spine(),
        )
        .unwrap();
        assert_eq!(after.action, "SOFT+");
        assert!(!after.committed);
        assert_eq!(m0_bytes(&after), bytes_m0);
        assert_eq!(m1_bytes(&after), bytes_m1);
    }

    #[test]
    fn old_meta_json_without_m1_revision_loads() {
        // serde(default) lets pre-m1_revision meta JSON deserialize cleanly.
        let json = r#"{"initialized":true,"last_render_config":"cfg0","coverage_ordinal":1}"#;
        let meta: ModuleMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.m1_revision, 0);
        assert!(meta.initialized);
    }
}
