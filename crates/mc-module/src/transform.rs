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

use crate::compartment_coverage::{fold_m0_content_epoch, M0ContentEpoch};
use crate::m0_compose::compose_m0_from_store;
use crate::m1_compose::{compose_m1_from_store, m1_revision_signal};
use crate::memory_render::M1_PLACEHOLDER;
use mc_core::{classify, CkItem, ClassifierInput, CoreState, FrozenUnit, PassInput, PassPlan};
use mc_store::{McStore, McStoreError, ModuleMeta};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Max CAS retries before surfacing the conflict (the module is the single writer in
/// the daemon case, so this rarely loops; the shared-store case re-loads and re-steps).
const MAX_CAS_RETRIES: u32 = 8;

/// Reserved synthetic-block ids (never carried by a real conversation item).
const M0_ID: &str = "mc_m0";
const M1_ID: &str = "mc_m1";
/// The reserved id prefix: a non-synthetic item bearing it is a contract violation.
const RESERVED_ID_PREFIX: &str = "mc_";
const SYNTH_REGION_KIND: &str = "synthesized-region";
/// Frozen-unit key prefix for a tail reduction (a reduced tool output / superseded edit).
/// `red:<target_id>` — the target is the real tail item whose bytes are replaced.
const RED_KEY_PREFIX: &str = "red:";

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

/// One tail-reduction decision: the target tail item and the byte-complete reduced
/// payload that replaces its bytes (`[dropped N]`, or a `filePath + region-hint +
/// [dropped N]` skeleton). The payload is captured at FREEZE and is authoritative
/// thereafter — never re-read for an already-frozen target (a moving recent-window
/// re-derive must not flip the bytes).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReductionDecision {
    pub target_id: String,
    #[serde(default)]
    pub kind: String,
    pub payload: String,
}

/// The tail-reduction decisions for a pass. The producer that SELECTS reductions (the
/// supersession / ctx_reduce / emergency / smart-drops heuristics) is not yet implemented
/// in its final location, so these still arrive via the test-only `_decider` request field
/// — the one remaining such seam, which goes away when that producer lands. The m0/m1
/// content is NO LONGER here: it is composed from the store (see [`ProducerContext`]).
/// `Deserialize` with field defaults so a partial `_decider` body fills the rest.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeciderInputs {
    /// The FULL current tail-reduction set, re-derived each pass. A target NOT yet
    /// frozen + present in the live tail is a NEW reduction to freeze (a SOFT trigger).
    /// An already-frozen target's payload here is IGNORED (the frozen payload wins);
    /// supplying a DIFFERENT payload for a frozen target is a monotonicity-contract
    /// violation that fails loud (see `validate_reduction_monotonicity`).
    pub reductions: Vec<ReductionDecision>,
}

/// The project context the module composes m0/m1 FROM. Resolved once per request from the
/// authenticated route binding (never a request body field) and threaded into the
/// transform. Production ALWAYS supplies it; it carries the frozen render inputs (budget,
/// expiry cutoff) so a HARD freezes them and later passes replay identical bytes.
pub struct ProducerContext<'a> {
    /// The project identity the store reads key off (memories, mutation log, workspace).
    pub project_path: &'a str,
    /// The project directory on disk, for reading ARCHITECTURE.md / STRUCTURE.md.
    pub project_directory: &'a str,
    /// The history budget in tokens, FROZEN at route bind (byte-affecting: a different
    /// budget → a different m0 trim → different bytes, so it can't change mid-session).
    pub history_budget_tokens: f64,
    /// The wall-clock now (ms) for THIS pass. Used only to SET `meta.expiry_cutoff_ms` on
    /// a HARD (the first materialization freezes it); every later pass reads the frozen
    /// meta value, never this, so expiry never drifts the bytes between passes.
    pub now_ms: i64,
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
    /// The decider supplied a reduction for an already-frozen target with DIFFERENT
    /// bytes — a monotonicity-contract violation (a frozen reduction is immutable
    /// within an epoch). Fail loud instead of silently serving the stale frozen bytes.
    ReductionConflict,
    /// The stored compartments don't tile contiguously — a raw message is covered by no
    /// compartment, so composing m0/m1 would silently drop it from the tail. Fail loud.
    CoverageGap(String),
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
            TransformError::ReductionConflict => write!(
                f,
                "decider re-supplied an already-frozen reduction target with different bytes"
            ),
            TransformError::CoverageGap(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for TransformError {}
impl From<McStoreError> for TransformError {
    fn from(e: McStoreError) -> Self {
        TransformError::Store(e)
    }
}
impl From<crate::m0_compose::M0ComposeError> for TransformError {
    fn from(e: crate::m0_compose::M0ComposeError) -> Self {
        use crate::m0_compose::M0ComposeError;
        match e {
            M0ComposeError::Store(s) => TransformError::Store(s),
            M0ComposeError::CoverageGap(g) => TransformError::CoverageGap(g.to_string()),
        }
    }
}
impl From<crate::m1_compose::M1ComposeError> for TransformError {
    fn from(e: crate::m1_compose::M1ComposeError) -> Self {
        use crate::m1_compose::M1ComposeError;
        match e {
            M1ComposeError::Store(s) => TransformError::Store(s),
            M1ComposeError::CoverageGap(g) => TransformError::CoverageGap(g.to_string()),
        }
    }
}

/// The token estimator threaded into the decay renderer. A no-op (returns 0) until the
/// real BPE estimator lands; under a loose budget the m0 render is estimator-independent,
/// so this keeps the compose pure + deterministic in the interim.
fn no_estimate(_: &str) -> usize {
    0
}

/// Apply one transform pass, retrying the whole load→classify→step→commit cycle on a
/// CAS conflict (re-classification depends on the freshly-loaded state). `ctx` is the
/// resolved project producer context (m0/m1 are composed from its store reads);
/// `deciders` now carries ONLY the tail reductions (the m0/m1 content is store-produced).
pub fn transform(
    store: &McStore,
    req: &TransformRequest,
    ctx: &ProducerContext<'_>,
    deciders: &DeciderInputs,
) -> Result<TransformResponse, TransformError> {
    let mut attempt = 0;
    loop {
        match apply_once(store, req, ctx, deciders) {
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
    ctx: &ProducerContext<'_>,
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

    // Fail-loud monotonicity guard, BEFORE classify and on EVERY pass: a frozen
    // reduction target re-supplied with different bytes breaks the immutable contract,
    // and the set-membership trigger would silently skip it (already frozen) and serve
    // the stale bytes — including on a defer. Error here instead.
    validate_reduction_monotonicity(&loaded.core, deciders)?;

    // --- boundary presence: computed over live source vs durable boundary_id ---
    let boundary_present = !loaded.core.boundary_id.is_empty()
        && live.iter().any(|i| i.id() == loaded.core.boundary_id);
    let boundary_token = if boundary_present {
        loaded.core.boundary_id.clone()
    } else {
        "-".to_string()
    };

    // --- CHEAP per-pass classify signals (read EVERY pass; never the m0/m1 BODY) ---
    // The effective render_config folds the live m0-content-epoch (workspace_fingerprint)
    // into the provider base, so a membership/policy change drives the render_config HARD.
    // upgrade_state + the external memory epoch have no mc_* source yet → empty (inert).
    let effective_render_config = fold_m0_content_epoch(
        &req.render_config,
        &M0ContentEpoch {
            workspace_fingerprint: store.workspace_fingerprint(ctx.project_path)?,
            upgrade_state: String::new(),
            memory_content_epoch: String::new(),
        },
    );
    // The cheap m1-change digest (watermark triple). Computed every pass to gate SOFT vs
    // defer WITHOUT composing the body; the body composes only on the bust arm below.
    let current_m1_digest = m1_revision_signal(store, ctx.project_path, &req.session_id)?;
    let reductions_pending_now =
        reductions_pending(&loaded.core, deciders, &live, loaded.meta.coverage_ordinal);
    let plan = classify(&ClassifierInput {
        initialized: loaded.meta.initialized,
        is_legacy_baseline: is_legacy_baseline(&loaded.core),
        valid_m0m1_shape: valid_m0m1_shape(&loaded.core),
        render_config_changed: loaded.meta.initialized
            && effective_render_config != loaded.meta.last_render_config,
        // No store-sourced HARD trigger yet: new compartments RIDE m1 as a SOFT delta and
        // fold into m0 on the next natural HARD. A dedicated fold-on-publish trigger lands
        // when the compartment write path moves in. For now all HARDs come from bootstrap,
        // a render_config change, or a reconcile.
        hard_fold_requested: false,
        boundary_present,
        reconcile_pending: loaded.core.reconcile_pending,
        m1_revision_changed: current_m1_digest != loaded.meta.m1_revision,
        reductions_pending: reductions_pending_now,
    });

    let mut core = loaded.core.clone();
    let mut meta = loaded.meta.clone();

    match plan {
        PassPlan::Reject(m) => return Err(TransformError::UnknownShape(m)),
        PassPlan::Hard | PassPlan::MigrateHard => {
            // EXPENSIVE bust-only: compose the m0 baseline from the store. now_ms freezes
            // the expiry cutoff into meta so every later in-epoch SOFT/defer reads the
            // SAME memory set (a memory expiring mid-epoch stays rendered until the next
            // HARD re-freezes the cutoff — the byte-stability tradeoff).
            let comp = compose_m0_from_store(
                store,
                &crate::m0_compose::M0ComposeInputs {
                    session_id: &req.session_id,
                    project_path: ctx.project_path,
                    project_directory: ctx.project_directory,
                    now_ms: ctx.now_ms,
                    history_budget_tokens: ctx.history_budget_tokens,
                },
                no_estimate,
            )?;

            // Leading-gap guard (symmetric with the interior contiguity gap that
            // resolve_coverage fails loud on): a live item BELOW the first covered ordinal
            // is covered by no compartment, yet build_output trims everything at/under the
            // coverage end (and the first covered ordinal is itself <= the coverage end), so
            // it would be silently dropped from the tail. resolve_coverage can't see the
            // live array (it's store-pure), so the check lives here where the ordinals are.
            if let Some(first) = comp.first_covered_ordinal {
                if let Some(stray) = live.iter().find(|i| i.ordinal() < first) {
                    return Err(TransformError::CoverageGap(format!(
                        "leading coverage gap: live item {} (ordinal {}) sits before the first \
                         compartment start (ordinal {}); composing m0 would silently drop it \
                         from the tail",
                        stray.id(),
                        stray.ordinal(),
                        first
                    )));
                }
            }

            // The reductions that SURVIVE the fold: m0 is now a compartment SUMMARY (not
            // covered raw bytes), so a reduction on a now-covered item simply drops with
            // it (no "fold reduced bytes into m0"); a target still in the new tail is kept;
            // a reverted-away target is an orphan. apply_units can't delete → rebuild.
            let effective = effective_reductions(&core, deciders);
            let survivors = surviving_red_units(&effective, &live, comp.coverage_ordinal);
            core.frozen_units.clear();
            core.pending_changes.clear();
            let mut rendered = vec![synth_region("m0", comp.m0_bytes), render_m1_placeholder()];
            rendered.extend(survivors);

            // A HARD re-composes m0 fully from the store, so the boundary ALWAYS reflects
            // the current coverage — set it unconditionally (empty when no compartments,
            // keeping boundary_id + coverage_ordinal consistent). The core only SETS on
            // Some, so mapping empty→None would leave a stale prior anchor alongside a
            // None coverage_ordinal — an inconsistent state.
            core.step(PassInput {
                proposed: Some(mc_core::Action::Hard),
                boundary_present: boundary_token,
                rendered_units: rendered,
                new_boundary_id: Some(comp.boundary_id.clone()),
                queued: Vec::new(),
                run_started: false,
            });
            meta.initialized = true;
            meta.last_render_config = effective_render_config;
            meta.coverage_ordinal = comp.coverage_ordinal;
            meta.folded_compartment_seq = comp.folded_compartment_seq;
            meta.rendered_memory_ids = comp.rendered_memory_ids;
            meta.memory_mutation_cursor = comp.memory_mutation_cursor;
            meta.max_memory_id = comp.max_memory_id;
            meta.expiry_cutoff_ms = ctx.now_ms; // FROZEN here, atomic with the m0 bytes
                                                // The post-fold m1 baseline digest — NOT 0. After folding up to the current
                                                // watermarks, "no delta" == "watermarks unchanged since this digest"; setting
                                                // 0 would make the next pass's non-zero digest read as a phantom SOFT.
            meta.m1_revision = current_m1_digest;
        }
        PassPlan::Soft => {
            // EXPENSIVE bust-only: compose the m1 delta body from the store against the
            // watermarks the last HARD froze (incl. the FROZEN expiry cutoff). A
            // reduction-only SOFT recomposes byte-identical m1 (watermarks unchanged), so
            // the m1 unit stays stable; a new compartment extends coverage → advance the
            // boundary anchor in this same commit.
            let m1 = compose_m1_from_store(
                store,
                ctx.project_path,
                &req.session_id,
                &meta,
                meta.expiry_cutoff_ms,
            )?;
            let mut rendered = vec![render_m1_body(&m1.body)];
            rendered.extend(new_reduction_units(
                &core,
                deciders,
                &live,
                loaded.meta.coverage_ordinal,
            ));
            // A coverage-extending SOFT advances the boundary anchor (the bound core
            // primitive); a memory-only SOFT leaves it put (None).
            let new_boundary_id = m1.new_coverage.as_ref().map(|(id, _)| id.clone());
            core.step(PassInput {
                proposed: Some(mc_core::Action::Soft),
                boundary_present: boundary_token,
                rendered_units: rendered,
                new_boundary_id,
                queued: Vec::new(),
                run_started: false,
            });
            // coverage_ordinal advances ATOMICALLY with the anchor (two views of one
            // coverage end — they must not desync).
            if let Some((_, ord)) = m1.new_coverage {
                meta.coverage_ordinal = Some(ord);
            }
            meta.m1_revision = current_m1_digest;
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

/// A valid current shape: EXACTLY one `m0`, EXACTLY one `m1`, and zero-or-more `red:*`
/// tail-reduction units. An initialized state missing `m0`/`m1`, or carrying any other
/// key, is an unknown shape (rejected, never cleared). Tighter than "keys ⊆ {m0,m1,red}"
/// so a corrupt initialized state missing a region can't validate.
fn valid_m0m1_shape(core: &CoreState) -> bool {
    let m0 = core.frozen_units.iter().filter(|u| u.key == "m0").count();
    let m1 = core.frozen_units.iter().filter(|u| u.key == "m1").count();
    let rest_ok = core
        .frozen_units
        .iter()
        .all(|u| u.key == "m0" || u.key == "m1" || u.key.starts_with(RED_KEY_PREFIX));
    m0 == 1 && m1 == 1 && rest_ok
}

// --- reduction helpers (the tail-reducer mechanics) ---

/// Is `ordinal` in the live TAIL (strictly after the coverage watermark)? None coverage
/// = nothing folded yet = all live items are tail.
fn is_tail(ordinal: u64, coverage: Option<u64>) -> bool {
    coverage.is_none_or(|c| ordinal > c)
}

/// The frozen payload for a target's reduction, if one is frozen.
fn frozen_red_payload<'a>(core: &'a CoreState, target: &str) -> Option<&'a str> {
    let key = format!("{RED_KEY_PREFIX}{target}");
    core.frozen_units
        .iter()
        .find(|u| u.key == key)
        .map(|u| u.frozen_payload.as_str())
}

/// Target ids that already carry a frozen `red:*` unit.
fn frozen_red_targets(core: &CoreState) -> std::collections::HashSet<String> {
    core.frozen_units
        .iter()
        .filter_map(|u| u.key.strip_prefix(RED_KEY_PREFIX).map(str::to_string))
        .collect()
}

/// Build a `red:<target>` frozen unit (Lineage — it persists + replays byte-identical).
fn red_unit(target: &str, kind: &str, payload: &str) -> FrozenUnit {
    FrozenUnit {
        key: format!("{RED_KEY_PREFIX}{target}"),
        kind: kind.to_string(),
        frozen_payload: payload.to_string(),
        durability_class: mc_core::DurabilityClass::Lineage,
        reset_rule: String::new(),
    }
}

/// Fail-loud monotonicity guard (runs EVERY pass, before classify). If the decider
/// supplies a reduction whose target is ALREADY frozen with DIFFERENT bytes, that
/// breaks the immutable-once-frozen contract — and the set-membership trigger would
/// SILENTLY skip it (already in keys) and serve the stale frozen payload. Error instead.
fn validate_reduction_monotonicity(
    core: &CoreState,
    deciders: &DeciderInputs,
) -> Result<(), TransformError> {
    for r in &deciders.reductions {
        if let Some(frozen) = frozen_red_payload(core, &r.target_id) {
            if frozen != r.payload {
                return Err(TransformError::ReductionConflict);
            }
        }
    }
    Ok(())
}

/// Is there a NEW reduction to freeze: a decider reduction whose target is in the live
/// tail AND not yet frozen. Pure id set-membership — the SOFT trigger.
fn reductions_pending(
    core: &CoreState,
    deciders: &DeciderInputs,
    live: &[&CkItemWire],
    coverage: Option<u64>,
) -> bool {
    let frozen = frozen_red_targets(core);
    let tail: std::collections::HashSet<&str> = live
        .iter()
        .filter(|i| is_tail(i.ordinal(), coverage))
        .map(|i| i.id())
        .collect();
    deciders
        .reductions
        .iter()
        .any(|r| tail.contains(r.target_id.as_str()) && !frozen.contains(&r.target_id))
}

/// The `red:*` units to freeze on a SOFT: each NEW decider reduction (target in the live
/// tail, not yet frozen), deduped by target, deterministic order.
fn new_reduction_units(
    core: &CoreState,
    deciders: &DeciderInputs,
    live: &[&CkItemWire],
    coverage: Option<u64>,
) -> Vec<FrozenUnit> {
    let frozen = frozen_red_targets(core);
    let tail: std::collections::HashSet<&str> = live
        .iter()
        .filter(|i| is_tail(i.ordinal(), coverage))
        .map(|i| i.id())
        .collect();
    let mut by_target: BTreeMap<String, FrozenUnit> = BTreeMap::new();
    for r in &deciders.reductions {
        if tail.contains(r.target_id.as_str()) && !frozen.contains(&r.target_id) {
            by_target
                .entry(r.target_id.clone())
                .or_insert_with(|| red_unit(&r.target_id, &r.kind, &r.payload));
        }
    }
    by_target.into_values().collect()
}

/// The reductions in EFFECT this pass, snapshotted BEFORE any frozen-set mutation (the
/// HARD-fold snapshot): every frozen `red:*` (authoritative payload) ∪ every NEW decider
/// reduction (target not yet frozen). Keyed by target_id → (kind, payload), deterministic.
fn effective_reductions(
    core: &CoreState,
    deciders: &DeciderInputs,
) -> BTreeMap<String, (String, String)> {
    let mut eff: BTreeMap<String, (String, String)> = BTreeMap::new();
    for u in &core.frozen_units {
        if let Some(target) = u.key.strip_prefix(RED_KEY_PREFIX) {
            eff.insert(
                target.to_string(),
                (u.kind.clone(), u.frozen_payload.clone()),
            );
        }
    }
    for r in &deciders.reductions {
        eff.entry(r.target_id.clone())
            .or_insert_with(|| (r.kind.clone(), r.payload.clone()));
    }
    eff
}

/// The `red:*` units that SURVIVE a HARD rebuild: a target that is COVERED (folded into
/// m0) is dropped; a target in the new TAIL is kept; a target ABSENT from the live array
/// (reverted away) is dropped as an orphan. So a unit survives iff its target is in the
/// live array AND still in the tail after the fold.
fn surviving_red_units(
    effective: &BTreeMap<String, (String, String)>,
    live: &[&CkItemWire],
    new_coverage: Option<u64>,
) -> Vec<FrozenUnit> {
    let live_ord: BTreeMap<&str, u64> = live.iter().map(|i| (i.id(), i.ordinal())).collect();
    effective
        .iter()
        .filter_map(
            |(target, (kind, payload))| match live_ord.get(target.as_str()) {
                Some(&ord) if is_tail(ord, new_coverage) => Some(red_unit(target, kind, payload)),
                _ => None,
            },
        )
        .collect()
}

// --- render helpers (the ONLY producers of frozen bytes) ---

/// The m1 placeholder unit (a HARD resets m1 to it; m1 is never fully empty).
fn render_m1_placeholder() -> FrozenUnit {
    synth_region("m1", M1_PLACEHOLDER.to_string())
}

/// The m1 delta unit from a composed body (an empty delta composes to the placeholder
/// body upstream, so this is a verbatim wrap).
fn render_m1_body(body: &str) -> FrozenUnit {
    synth_region("m1", body.to_string())
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
    // A tail item with a frozen `red:<id>` emits the FROZEN reduced payload as its bytes
    // (same id, same ordinal, still a real item — just reduced); the frozen payload is
    // authoritative, never re-derived from the live bytes. Reductions interleave with
    // non-reduced live items; per-item byte-stability holds the contiguous-prefix cache.
    let cutoff = meta.coverage_ordinal;
    for item in live {
        if is_tail(item.ordinal(), cutoff) {
            match frozen_red_payload(core, item.id()) {
                Some(reduced) => out.push(synth_reduced_wire(item, reduced)),
                None => out.push((*item).clone()),
            }
        }
    }
    out
}

/// A tail item rendered with its frozen reduced bytes — same id + ordinal, NOT synthetic
/// (still a real conversation item), just byte-reduced.
fn synth_reduced_wire(item: &CkItemWire, reduced: &str) -> CkItemWire {
    CkItemWire {
        id: item.id.clone(),
        ordinal: item.ordinal,
        bytes: reduced.to_string(),
        synthetic: false,
    }
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

    use mc_store::StoredCompartment;

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

    /// A store compartment covering raw ordinals `start..=end`, ending at message id
    /// `end_id`, rendered at P1 with body `p1`. The m0 baseline is composed from these.
    fn comp(seq: i64, start: i64, end: i64, end_id: &str, p1: &str) -> StoredCompartment {
        StoredCompartment {
            sequence: seq,
            start_message: start,
            end_message: end,
            end_message_id: end_id.to_string(),
            title: format!("C{seq}"),
            content: p1.to_string(),
            p1: Some(p1.to_string()),
            importance: 50,
            ..Default::default()
        }
    }

    /// A producer context over a throwaway project dir (no docs on disk → empty docs
    /// block). `now_ms` is FIXED per test (never wall-clock) so the frozen expiry cutoff
    /// is deterministic.
    fn pctx<'a>(project: &'a str, dir: &'a str, now_ms: i64) -> ProducerContext<'a> {
        ProducerContext {
            project_path: project,
            project_directory: dir,
            history_budget_tokens: 60_000.0,
            now_ms,
        }
    }

    /// Run a transform with a default producer context (project "git:proj", a nonexistent
    /// docs dir, now_ms=0). Most tests don't vary the context.
    fn run(s: &McStore, req: &TransformRequest, d: &DeciderInputs) -> TransformResponse {
        transform(s, req, &pctx("git:proj", "/nonexistent-docs", 0), d).unwrap()
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

    // ===== Module-integration tests: STORE STATE in → compose+core bytes out.
    // The cache MECHANICS (defer-replay, the SOFT/HARD taxonomy, reduction freeze/replay)
    // are owned by cortexkit-cache-core's golden vectors + the live-daemon harness; these
    // tests prove the MC module's job: resolve → compose-from-store → wire-to-core. m0 is
    // a compartment SUMMARY composed from the store (NOT live bytes), so "cover ordinal N"
    // means a store compartment covering N, and the raw boundary message stays in the live
    // input (only absent from the OUTPUT tail). =====

    #[test]
    fn bootstrap_with_no_compartments_is_empty_baseline_whole_array_is_tail() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // no compartments → nothing summarized → empty boundary, the live array is all tail
        let r = run(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "<h>BASE</h>")]),
            &spine(),
        );
        assert_eq!(r.action, "HARD", "first pass materializes a baseline");
        assert_eq!(r.boundary_id, "", "no compartment → no coverage anchor");
        // m0 is the empty-history placeholder baseline (no docs/memories seeded)
        assert!(
            m0_bytes(&r).contains("<session-history></session-history>"),
            "{}",
            m0_bytes(&r)
        );
        assert_eq!(m1_bytes(&r), M1_PLACEHOLDER);
        assert_eq!(tail_ids(&r), vec!["a"], "uncovered live item is the tail");
        assert!(r.committed);
    }

    #[test]
    fn bootstrap_with_a_compartment_summarizes_it_and_trims_the_covered_tail() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // a compartment covers raw ordinals 1..=10, ending at message id "m10"
        s.replace_compartments("ses", &[comp(1, 1, 10, "m10", "SUMMARY-OF-1-10")])
            .unwrap();
        // the live array still carries the raw covered message m10 (ordinal 10) + a tail item
        let items = vec![item("m10", 10, "raw covered"), item("t11", 11, "tail")];
        let r = run(&s, &req("ses", "cfg0", items), &spine());
        assert_eq!(r.action, "HARD");
        assert_eq!(
            r.boundary_id, "m10",
            "anchor = the compartment's end message id"
        );
        // m0 is the decay-rendered SUMMARY, not the raw covered bytes
        assert!(
            m0_bytes(&r).contains("SUMMARY-OF-1-10"),
            "m0 is the summary: {}",
            m0_bytes(&r)
        );
        assert!(
            !m0_bytes(&r).contains("raw covered"),
            "m0 is NOT the raw bytes"
        );
        // the covered raw message (ordinal 10 <= coverage 10) is trimmed; only the tail remains
        assert_eq!(
            tail_ids(&r),
            vec!["t11"],
            "covered raw msg trimmed, tail kept"
        );
    }

    #[test]
    fn leading_coverage_gap_fails_loud_not_silent_drop() {
        // Regression: the first compartment starts at ordinal 10, but the live array still
        // carries raw messages at ordinals 1..9 (before the first compartment). Those are
        // covered by no compartment, yet they sit below coverage_ordinal so build_output
        // would trim them — a SILENT drop. The leading-gap guard must fail loud instead
        // (symmetric with the interior contiguity gap resolve_coverage already catches).
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 10, 20, "m20", "S")])
            .unwrap();
        let items = vec![
            item("early", 1, "live before the first compartment"),
            item("m20", 20, "covered"),
            item("t21", 21, "tail"),
        ];
        let err = transform(
            &s,
            &req("ses", "cfg0", items),
            &pctx("git:proj", "/nonexistent-docs", 0),
            &spine(),
        )
        .unwrap_err();
        assert!(
            matches!(err, TransformError::CoverageGap(_)),
            "a leading gap must fail loud, not silently drop the early live item: {err:?}"
        );
    }

    #[test]
    fn growing_tail_defers_byte_stable_and_no_write() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // a compartment covers ordinal 1 (end id "m1msg"); the boundary stays present
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1msg", "SUMMARY")])
            .unwrap();
        run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );

        let mut prev_m0: Option<String> = None;
        let mut prev_m1: Option<String> = None;
        for n in 2..=5u64 {
            let mut items = vec![item("m1msg", 1, "raw")];
            for k in 2..=n {
                items.push(item(&format!("t{k}"), k, &format!("tail{k}")));
            }
            let r = run(&s, &req("ses", "cfg0", items), &spine());
            assert_eq!(r.action, "SOFT+", "no delta → pure defer");
            assert!(!r.committed, "pure defer must not write");
            if let Some(p) = &prev_m0 {
                assert_eq!(m0_bytes(&r), p, "m0 changed on defer");
            }
            if let Some(p) = &prev_m1 {
                assert_eq!(m1_bytes(&r), p, "m1 changed on defer");
            }
            // tail = the verbatim live items past coverage_ordinal=1 (the covered m1msg trimmed)
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
    fn new_memory_rides_m1_soft_and_m0_stays_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1msg", "SUMMARY")])
            .unwrap();
        let before = run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(before.action, "HARD");

        // a NEW memory lands (id past the folded max) → the digest moves → a SOFT
        s.seed_memory(5, "git:proj", "ARCHITECTURE", "a durable rule", 70)
            .unwrap();
        let soft = run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(soft.action, "SOFT", "a new store memory rides a SOFT");
        assert_eq!(
            m0_bytes(&soft),
            m0_bytes(&before),
            "m0 frozen across the SOFT"
        );
        assert!(
            m1_bytes(&soft).contains("<new-memories>"),
            "{}",
            m1_bytes(&soft)
        );
        assert!(m1_bytes(&soft).contains("a durable rule"));
        assert!(soft.committed);

        // defer after: the store is unchanged → digest stable → pure SOFT+ replay, no write
        let after = run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(after.action, "SOFT+");
        assert!(!after.committed);
        assert_eq!(
            m1_bytes(&after),
            m1_bytes(&soft),
            "m1 replays byte-identical"
        );
        assert_eq!(m0_bytes(&after), m0_bytes(&before));
    }

    #[test]
    fn in_m0_memory_update_rides_m1_as_a_supersede_delta() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // a memory is in the m0 baseline (seeded before bootstrap → in the manifest)
        s.seed_memory(5, "git:proj", "ARCHITECTURE", "original", 70)
            .unwrap();
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1msg", "SUMMARY")])
            .unwrap();
        let before = run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert!(
            m0_bytes(&before).contains("original"),
            "memory in m0 baseline"
        );

        // an in-session UPDATE to that in-m0 memory → a mutation-log row → digest moves → SOFT
        s.seed_mutation("git:proj", "update", 5, "corrected")
            .unwrap();
        let r = run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(r.action, "SOFT", "an in-m0 memory mutation rides a SOFT");
        assert_eq!(
            m0_bytes(&r),
            m0_bytes(&before),
            "m0 frozen (the supersede rides m1)"
        );
        assert!(
            m1_bytes(&r).contains("corrected"),
            "memory-updates delta: {}",
            m1_bytes(&r)
        );
    }

    #[test]
    fn render_config_change_hard_folds_the_m1_delta_into_m0() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1msg", "SUMMARY")])
            .unwrap();
        run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        // ride a new memory on m1
        s.seed_memory(5, "git:proj", "ARCHITECTURE", "folded rule", 70)
            .unwrap();
        let soft = run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(soft.action, "SOFT");
        assert!(!m0_bytes(&soft).contains("folded rule"), "not in m0 yet");

        // a render_config (model/system) change → HARD: re-compose m0 from the store, which
        // now INCLUDES the memory, and reset m1 to the placeholder.
        let r = run(
            &s,
            &req("ses", "cfg1", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(r.action, "HARD");
        assert!(
            m0_bytes(&r).contains("folded rule"),
            "m1 delta folded into m0: {}",
            m0_bytes(&r)
        );
        assert_eq!(m1_bytes(&r), M1_PLACEHOLDER, "m1 reset to placeholder");
    }

    #[test]
    fn new_compartment_extends_coverage_on_soft_advancing_the_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // m0 folds C1 (covers 1..=10, end "m10")
        s.replace_compartments("ses", &[comp(1, 1, 10, "m10", "S1")])
            .unwrap();
        let boot = run(
            &s,
            &req(
                "ses",
                "cfg0",
                vec![item("m10", 10, "raw"), item("t11", 11, "tail")],
            ),
            &spine(),
        );
        assert_eq!(boot.boundary_id, "m10");
        assert_eq!(tail_ids(&boot), vec!["t11"]);

        // C2 (covers 11..=20, end "m20") publishes → it rides m1 at P1 AND extends coverage
        s.replace_compartments(
            "ses",
            &[comp(1, 1, 10, "m10", "S1"), comp(2, 11, 20, "m20", "S2")],
        )
        .unwrap();
        let items = vec![
            item("m10", 10, "raw"),
            item("m20", 20, "raw2"),
            item("t21", 21, "tail"),
        ];
        let soft = run(&s, &req("ses", "cfg0", items.clone()), &spine());
        assert_eq!(soft.action, "SOFT", "a new compartment rides a SOFT");
        assert_eq!(
            soft.boundary_id, "m20",
            "the anchor ADVANCED on the SOFT (b0→b1)"
        );
        assert!(
            m1_bytes(&soft).contains("<new-compartments>"),
            "{}",
            m1_bytes(&soft)
        );
        assert!(m1_bytes(&soft).contains("S2") && !m1_bytes(&soft).contains("title=\"C1\""));
        // coverage advanced to 20 → raw m20 trimmed, only t21 remains
        assert_eq!(tail_ids(&soft), vec!["t21"]);

        // a defer at the new anchor replays byte-identical
        let defer = run(&s, &req("ses", "cfg0", items), &spine());
        assert_eq!(defer.action, "SOFT+");
        assert!(!defer.committed);
        assert_eq!(
            m1_bytes(&defer),
            m1_bytes(&soft),
            "m1 replays identical at b1"
        );
        assert_eq!(m0_bytes(&defer), m0_bytes(&soft));

        // revert BELOW the new boundary (m20 gone) → boundary absent → reconcile
        let revert = run(
            &s,
            &req("ses", "cfg0", vec![item("z", 30, "other")]),
            &spine(),
        );
        assert_eq!(revert.action, "SOFT+");
        assert!(
            revert.reconcile_pending,
            "revert below b1 → reconcile pending"
        );
    }

    #[test]
    fn frozen_expiry_cutoff_survives_a_wall_clock_advance_on_recompose() {
        // Resume-determinism guard for the frozen expiry cutoff: a memory live under the
        // FROZEN cutoff must keep rendering even when a later SOFT recomposes at a
        // wall-clock past its expiry (e.g. after a restart). A live-clock bug (using now_ms
        // instead of the frozen meta cutoff) drops it here and ONLY here — the non-vacuous
        // proof.
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1msg", "SUMMARY")])
            .unwrap();
        // bootstrap at now_ms=500 → expiry cutoff FROZEN at 500. No memories folded yet.
        transform(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &pctx("git:proj", "/nonexistent-docs", 500),
            &spine(),
        )
        .unwrap();

        // a NEW memory expiring at 1000: LIVE under cutoff 500, EXPIRED under wall-clock 2000.
        s.seed_expiring_memory(5, "git:proj", "ARCHITECTURE", "still valid", 70, 1000)
            .unwrap();

        // a SOFT recompose at wall-clock 2000 — the cutoff stays FROZEN at 500, so the
        // memory is live and renders. A bug using now_ms=2000 would expire + drop it.
        let soft = transform(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &pctx("git:proj", "/nonexistent-docs", 2000),
            &spine(),
        )
        .unwrap();
        assert_eq!(soft.action, "SOFT");
        assert!(
            m1_bytes(&soft).contains("still valid"),
            "frozen cutoff (500) keeps the memory live at wall-clock 2000: {}",
            m1_bytes(&soft)
        );
    }

    #[test]
    fn workspace_membership_change_is_a_render_config_hard() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1msg", "SUMMARY")])
            .unwrap();
        run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        // a steady defer (no change)
        let defer = run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(defer.action, "SOFT+");

        // joining a workspace changes the deterministic workspace_fingerprint → the folded
        // render_config changes → a HARD (m0 is now composed over a different project set).
        s.seed_workspace_member("ws1", "git:proj", "[\"CONSTRAINTS\"]")
            .unwrap();
        let r = run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(r.action, "HARD", "a membership change re-materializes m0");
    }

    #[test]
    fn revert_defers_then_reconcile_rematerializes() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 10, "m10", "S1")])
            .unwrap();
        let before = run(
            &s,
            &req("ses", "cfg0", vec![item("m10", 10, "raw")]),
            &spine(),
        );
        assert_eq!(before.action, "HARD");
        assert_eq!(before.boundary_id, "m10");

        // revert removed the boundary message m10 — array no longer contains it
        let revert = run(
            &s,
            &req("ses", "cfg0", vec![item("z", 50, "other")]),
            &spine(),
        );
        assert_eq!(revert.action, "SOFT+", "revert must not bust");
        assert!(revert.reconcile_pending);
        assert_eq!(m0_bytes(&revert), m0_bytes(&before), "frozen m0 replays");
        assert!(revert.committed, "reconcile flag flip persists");

        // the compartment is also gone after the revert (the historian would re-cut), so
        // the next pass with the boundary still absent → Hard rematerialize. With no
        // compartments now, m0 is the empty-history placeholder over the live tail.
        s.replace_compartments("ses", &[]).unwrap();
        let remat = run(
            &s,
            &req("ses", "cfg0", vec![item("a2", 60, "reverted")]),
            &spine(),
        );
        assert_eq!(remat.action, "HARD");
        assert_eq!(remat.boundary_id, "", "no compartment → empty anchor");
        assert!(!remat.reconcile_pending);
        assert_eq!(tail_ids(&remat), vec!["a2"], "live item is the tail");
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
            ..Default::default()
        };
        s.commit("ses", None, &legacy_core, &legacy_meta).unwrap();
        // a compartment so the migrated m0 has real summary content
        s.replace_compartments("ses", &[comp(1, 1, 1, "a", "FRESH-SUMMARY")])
            .unwrap();

        let r = run(&s, &req("ses", "cfg0", vec![item("a", 1, "NEW")]), &spine());
        assert_eq!(
            r.action, "HARD",
            "legacy shape migrates via clear-then-Hard"
        );
        // no "baseline" residue: exactly [m0, m1] synthetic blocks, m0 re-composed from store
        let synth_ids: Vec<&str> = r
            .ck_messages
            .iter()
            .filter(|i| i.synthetic)
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(synth_ids, vec![M0_ID, M1_ID]);
        assert!(
            m0_bytes(&r).contains("FRESH-SUMMARY"),
            "m0 re-composed: {}",
            m0_bytes(&r)
        );
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
            ..Default::default()
        };
        s.commit("ses", None, &weird, &meta).unwrap();
        let err = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "X")]),
            &pctx("git:proj", "/nonexistent-docs", 0),
            &spine(),
        )
        .unwrap_err();
        assert!(matches!(err, TransformError::UnknownShape(_)));
        // durable state unchanged (the "junk" unit survives — not destructively cleared)
        let reloaded = s.load("ses").unwrap();
        assert_eq!(reloaded.core.frozen_units[0].key, "junk");
    }

    #[test]
    fn reserved_id_and_ordinal_violations_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let dc = pctx("git:proj", "/nonexistent-docs", 0);

        // a non-synthetic item with a reserved mc_* id (a pre-load ingress guard)
        let reserved = transform(
            &s,
            &req("ses", "cfg0", vec![item("mc_m0", 2, "x")]),
            &dc,
            &spine(),
        );
        assert!(matches!(reserved, Err(TransformError::ReservedId)));

        // non-monotonic ordinals
        let bad = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 5, "x"), item("b", 5, "y")]),
            &dc,
            &spine(),
        );
        assert!(matches!(bad, Err(TransformError::OrdinalViolation)));
    }

    #[test]
    fn synthetic_ingress_is_stripped_before_boundary_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1msg", "S")])
            .unwrap();
        run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        // feed our own synthetic m0 back in alongside the real array
        let items = vec![
            CkItemWire {
                id: M0_ID.into(),
                ordinal: 0,
                bytes: "STALE".into(),
                synthetic: true,
            },
            item("m1msg", 1, "raw"),
            item("t2", 2, "tail2"),
        ];
        let r = run(&s, &req("ses", "cfg0", items), &spine());
        // boundary m1msg still found (synthetic stripped), tail filter uncorrupted
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
            s.replace_compartments("ses", &[comp(1, 1, 1, "m1msg", "SUMMARY")])
                .unwrap();
            run(
                &s,
                &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
                &spine(),
            );
            let r = run(
                &s,
                &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
                &spine(),
            );
            bytes_m0 = m0_bytes(&r).to_string();
            bytes_m1 = m1_bytes(&r).to_string();
        } // lease released
        let s2 = store(dir.path());
        let after = run(
            &s2,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(after.action, "SOFT+");
        assert!(!after.committed);
        assert_eq!(m0_bytes(&after), bytes_m0);
        assert_eq!(m1_bytes(&after), bytes_m1);
    }

    #[test]
    fn old_meta_json_without_new_fields_loads() {
        // serde(default) lets older meta JSON (written before m1_revision and the
        // two-watermark fields existed) deserialize cleanly — they all default.
        let json = r#"{"initialized":true,"last_render_config":"cfg0","coverage_ordinal":1}"#;
        let meta: ModuleMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.m1_revision, 0);
        assert_eq!(meta.folded_compartment_seq, 0);
        assert_eq!(meta.expiry_cutoff_ms, 0);
        assert!(meta.initialized);
    }

    // ===== slice 3: tail reducers =====

    fn reduce(target: &str, kind: &str, payload: &str) -> ReductionDecision {
        ReductionDecision {
            target_id: target.to_string(),
            kind: kind.to_string(),
            payload: payload.to_string(),
        }
    }
    fn with_reductions(rs: Vec<ReductionDecision>) -> DeciderInputs {
        DeciderInputs { reductions: rs }
    }
    /// The bytes of a tail item (non-synthetic) by id.
    fn tail_bytes<'a>(r: &'a TransformResponse, id: &str) -> &'a str {
        &r.ck_messages
            .iter()
            .find(|i| i.id == id && !i.synthetic)
            .unwrap_or_else(|| panic!("no tail item {id}"))
            .bytes
    }

    /// Bootstrap a session whose m0 covers ordinal 1 (compartment ends at id "a"), so the
    /// boundary "a" is present and tail items (ordinal ≥ 2) are reducible.
    fn bootstrap_covering_a(s: &McStore) {
        s.replace_compartments("ses", &[comp(1, 1, 1, "a", "SUMMARY")])
            .unwrap();
        run(s, &req("ses", "cfg0", vec![item("a", 1, "raw")]), &spine());
    }

    #[test]
    fn reduction_freezes_on_bust_replays_on_defer() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);

        // a new reduction on tail item t2 → SOFT, frozen → [dropped 1]
        let items = vec![item("a", 1, "raw"), item("t2", 2, "BIGOUTPUT")];
        let d = with_reductions(vec![reduce("t2", "drop", "[dropped 1]")]);
        let soft = run(&s, &req("ses", "cfg0", items.clone()), &d);
        assert_eq!(soft.action, "SOFT", "a new reduction rides a SOFT");
        assert_eq!(
            tail_bytes(&soft, "t2"),
            "[dropped 1]",
            "t2 reduced in place"
        );
        assert_eq!(
            tail_ids(&soft),
            vec!["t2"],
            "covered 'a' trimmed, only t2 in tail"
        );

        for _ in 0..3 {
            let after = run(&s, &req("ses", "cfg0", items.clone()), &d);
            assert_eq!(after.action, "SOFT+", "no new reduction → pure defer");
            assert!(!after.committed, "pure defer must not write");
            assert_eq!(tail_bytes(&after, "t2"), "[dropped 1]");
        }
    }

    #[test]
    fn frozen_reduction_never_first_applied_on_defer() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);

        let d = with_reductions(vec![reduce("t2", "strip", "[dropped 99999]")]);
        run(
            &s,
            &req(
                "ses",
                "cfg0",
                vec![item("a", 1, "raw"), item("t2", 2, "OUT")],
            ),
            &d,
        );

        // the tail grows; the SAME reduction set re-supplied each pass → pure defer, the
        // frozen [dropped 99999] replays verbatim, never first-applied on a defer.
        for n in 3..=6u64 {
            let mut items = vec![item("a", 1, "raw"), item("t2", 2, "OUT")];
            for k in 3..=n {
                items.push(item(&format!("t{k}"), k, &format!("new{k}")));
            }
            let r = run(&s, &req("ses", "cfg0", items), &d);
            assert_eq!(
                r.action, "SOFT+",
                "an aged-but-unchanged reduction set defers"
            );
            assert!(!r.committed);
            assert_eq!(
                tail_bytes(&r, "t2"),
                "[dropped 99999]",
                "frozen strip replays"
            );
        }
    }

    #[test]
    fn skeleton_byte_complete_across_moving_window() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);

        let skel = "edit packages/app/x.ts | @@ -10,6 +10,8 @@ [dropped 1]";
        let d1 = with_reductions(vec![reduce("edit1", "edit_marker", skel)]);
        let items1 = vec![item("a", 1, "raw"), item("edit1", 2, "FULL-DIFF-BYTES")];
        let frozen = run(&s, &req("ses", "cfg0", items1), &d1);
        assert_eq!(tail_bytes(&frozen, "edit1"), skel);

        // a newer edit lands; edit1 must replay its FROZEN payload verbatim (a re-derive
        // of the region-hint from current content would flip its bytes).
        let skel2 = "edit packages/app/y.ts | @@ -1,2 +1,3 @@ [dropped 2]";
        let d2 = with_reductions(vec![
            reduce("edit1", "edit_marker", skel),
            reduce("edit2", "edit_marker", skel2),
        ]);
        let items2 = vec![
            item("a", 1, "raw"),
            item("edit1", 2, "FULL-DIFF-BYTES"),
            item("edit2", 3, "ANOTHER-FULL-DIFF"),
        ];
        let moved = run(&s, &req("ses", "cfg0", items2), &d2);
        assert_eq!(moved.action, "SOFT", "the new edit2 reduction rides a SOFT");
        assert_eq!(tail_bytes(&moved, "edit1"), skel, "older skeleton verbatim");
        assert_eq!(tail_bytes(&moved, "edit2"), skel2, "new skeleton frozen");
    }

    #[test]
    fn fold_gcs_a_reduction_whose_item_becomes_covered() {
        // The new-model equivalent of "fold carries reduced bytes into m0": m0 is now a
        // SUMMARY (never reduced raw bytes), so when a HARD's coverage crosses a reduced
        // tail item, that item is represented by the compartment summary and its red:* unit
        // is GC'd — no stale [dropped] leak, no double-count.
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);

        // freeze a drop on tail item t2 (ordinal 2)
        let d = with_reductions(vec![reduce("t2", "drop", "[dropped 1]")]);
        run(
            &s,
            &req(
                "ses",
                "cfg0",
                vec![item("a", 1, "raw"), item("t2", 2, "HUGE")],
            ),
            &d,
        );

        // a compartment now covers ordinal 2 (t2 is summarized); a HARD (render_config
        // change) re-composes m0 over both compartments — coverage advances to 2, so
        // surviving_red_units GCs red:t2 (its ordinal is now covered).
        s.replace_compartments(
            "ses",
            &[comp(1, 1, 1, "a", "S1"), comp(2, 2, 2, "t2", "S2")],
        )
        .unwrap();
        let r = run(
            &s,
            &req(
                "ses",
                "cfg1",
                vec![item("a", 1, "raw"), item("t2", 2, "HUGE")],
            ),
            &d,
        );
        assert_eq!(r.action, "HARD");
        assert_eq!(r.boundary_id, "t2", "anchor = last compartment end id");
        assert!(
            m0_bytes(&r).contains("S2"),
            "m0 is the summary, not [dropped 1]: {}",
            m0_bytes(&r)
        );
        assert!(
            !m0_bytes(&r).contains("[dropped 1]"),
            "m0 never carries reduced bytes"
        );
        let reloaded = s.load("ses").unwrap();
        assert!(
            !reloaded.core.frozen_units.iter().any(|u| u.key == "red:t2"),
            "covered reduction GC'd"
        );
        assert!(tail_ids(&r).is_empty(), "both items covered, tail empty");
    }

    #[test]
    fn coalesced_memory_delta_and_reduction_one_soft() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);

        // both a store m1 delta (a new memory) AND a new reduction on one pass → ONE SOFT
        s.seed_memory(5, "git:proj", "ARCHITECTURE", "a rule", 70)
            .unwrap();
        let d = with_reductions(vec![reduce("t2", "drop", "[dropped 1]")]);
        let items = vec![item("a", 1, "raw"), item("t2", 2, "OUT")];
        let r = run(&s, &req("ses", "cfg0", items.clone()), &d);
        assert_eq!(r.action, "SOFT");
        assert!(
            m1_bytes(&r).contains("a rule"),
            "m1 delta rendered: {}",
            m1_bytes(&r)
        );
        assert_eq!(
            tail_bytes(&r, "t2"),
            "[dropped 1]",
            "reduction frozen, same SOFT"
        );

        // defer after: both replay byte-identical, no second bust
        let after = run(&s, &req("ses", "cfg0", items), &d);
        assert_eq!(after.action, "SOFT+");
        assert!(!after.committed);
        assert_eq!(m1_bytes(&after), m1_bytes(&r));
        assert_eq!(tail_bytes(&after, "t2"), "[dropped 1]");
    }

    #[test]
    fn reverted_orphan_reduction_gcd_on_reconcile_hard() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);

        // freeze a drop on t2
        let d = with_reductions(vec![reduce("t2", "drop", "[dropped 1]")]);
        run(
            &s,
            &req(
                "ses",
                "cfg0",
                vec![item("a", 1, "raw"), item("t2", 2, "OUT")],
            ),
            &d,
        );

        // a revert removes BOTH the boundary "a" AND the reduced item t2 from the array
        let revert = run(
            &s,
            &req("ses", "cfg0", vec![item("z", 9, "other")]),
            &spine(),
        );
        assert_eq!(revert.action, "SOFT+");
        assert!(revert.reconcile_pending);

        // reconcile-HARD rematerializes; the compartment is gone too (the historian re-cut),
        // so m0 is the empty-history baseline and the orphaned red:t2 is GC'd.
        s.replace_compartments("ses", &[]).unwrap();
        let remat = run(
            &s,
            &req("ses", "cfg0", vec![item("a2", 10, "reverted")]),
            &spine(),
        );
        assert_eq!(remat.action, "HARD");
        assert!(
            !m0_bytes(&remat).contains("[dropped 1]"),
            "no orphaned reduction in m0"
        );
        let reloaded = s.load("ses").unwrap();
        assert!(
            !reloaded.core.frozen_units.iter().any(|u| u.key == "red:t2"),
            "orphan red:t2 GC'd"
        );
    }

    #[test]
    fn monotonicity_violation_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);

        // freeze t2 → [dropped 1]
        let d = with_reductions(vec![reduce("t2", "drop", "[dropped 1]")]);
        let items = vec![item("a", 1, "raw"), item("t2", 2, "OUT")];
        run(&s, &req("ses", "cfg0", items.clone()), &d);

        // re-supply t2 with DIFFERENT bytes (a contract violation) → fail loud, not a
        // silent skip-and-serve-stale. Tested on a defer (the silent-miss surface).
        let bad = with_reductions(vec![reduce("t2", "drop", "[dropped DIFFERENT]")]);
        let err = transform(
            &s,
            &req("ses", "cfg0", items),
            &pctx("git:proj", "/nonexistent-docs", 0),
            &bad,
        )
        .unwrap_err();
        assert!(matches!(err, TransformError::ReductionConflict));
    }

    #[test]
    fn interleaved_reduction_keeps_surrounding_tail_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);

        // a reduction sits BETWEEN live tail items; the surrounding items stay verbatim
        // and stable across a defer (the contiguous-prefix cache holds per-item).
        let d = with_reductions(vec![reduce("t3", "drop", "[dropped 1]")]);
        let items = vec![
            item("a", 1, "raw"),
            item("t2", 2, "before"),
            item("t3", 3, "REDUCED-AWAY"),
            item("t4", 4, "after"),
        ];
        let soft = run(&s, &req("ses", "cfg0", items.clone()), &d);
        assert_eq!(soft.action, "SOFT");
        assert_eq!(
            tail_ids(&soft),
            vec!["t2", "t3", "t4"],
            "order + ids preserved"
        );
        assert_eq!(tail_bytes(&soft, "t2"), "before");
        assert_eq!(tail_bytes(&soft, "t3"), "[dropped 1]");
        assert_eq!(tail_bytes(&soft, "t4"), "after");

        let after = run(&s, &req("ses", "cfg0", items), &d);
        assert_eq!(after.action, "SOFT+");
        assert_eq!(tail_bytes(&after, "t2"), "before");
        assert_eq!(tail_bytes(&after, "t3"), "[dropped 1]");
        assert_eq!(tail_bytes(&after, "t4"), "after");
    }

    #[test]
    fn shape_tighten_rejects_missing_m1_but_allows_red() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let dc = pctx("git:proj", "/nonexistent-docs", 0);
        // an initialized state with m0 + a red:* but NO m1 → unknown shape, reject
        let bad = CoreState {
            version: 1,
            boundary_id: "a".into(),
            frozen_units: vec![
                synth_region("m0", "BASE".into()),
                red_unit("t2", "drop", "[dropped 1]"),
            ],
            pending_changes: vec![],
            reconcile_pending: false,
        };
        let meta = ModuleMeta {
            initialized: true,
            last_render_config: "cfg0".into(),
            coverage_ordinal: Some(1),
            ..Default::default()
        };
        s.commit("ses", None, &bad, &meta).unwrap();
        let err = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "BASE")]),
            &dc,
            &spine(),
        )
        .unwrap_err();
        assert!(
            matches!(err, TransformError::UnknownShape(_)),
            "missing m1 rejects"
        );

        // a valid m0 + m1 + red:* state classifies normally (does NOT reject). Use the
        // effective render_config (with the empty-workspace fingerprint folded) and the
        // matching post-HARD m1 digest so the steady-state pass is a clean SOFT+ (no
        // phantom delta from a mismatched digest).
        let good = CoreState {
            version: 1,
            boundary_id: "a".into(),
            frozen_units: vec![
                synth_region("m0", "BASE".into()),
                synth_region("m1", M1_PLACEHOLDER.into()),
                red_unit("t2", "drop", "[dropped 1]"),
            ],
            pending_changes: vec![],
            reconcile_pending: false,
        };
        let good_cfg = fold_m0_content_epoch(
            "cfg0",
            &M0ContentEpoch {
                workspace_fingerprint: s.workspace_fingerprint("git:proj").unwrap(),
                upgrade_state: String::new(),
                memory_content_epoch: String::new(),
            },
        );
        let good_meta = ModuleMeta {
            initialized: true,
            last_render_config: good_cfg,
            coverage_ordinal: Some(1),
            m1_revision: m1_revision_signal(&s, "git:proj", "ses2").unwrap(),
            ..Default::default()
        };
        s.commit("ses2", None, &good, &good_meta).unwrap();
        let ok = transform(
            &s,
            &req("ses2", "cfg0", vec![item("a", 1, "BASE")]),
            &dc,
            &spine(),
        )
        .unwrap();
        assert_eq!(ok.action, "SOFT+", "m0+m1+red is a valid shape");
    }
}
