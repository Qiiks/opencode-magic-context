//! The transform op: the CK-in / CK-out cache-stability transform.
//!
//! Emits the rewritten array: `pass_output.ck_messages = [m0, m1] ++ tail`.
//! The covered sparse-but-ordered prefix is REPLACED by two frozen synthesized-region blocks
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

use crate::ck_wire;
use crate::compartment_coverage::{fold_m0_content_epoch, resolve_coverage, M0ContentEpoch};
use crate::healing::{self, quirk_residual, SerializerProfile};
use crate::injection::{advance_injection_from_meta, capture_todo_state_on_bust, InjectionOutcome};
use crate::m0_compose::compose_m0_from_store;
use crate::m1_compose::{compose_m1_from_store, m1_revision_signal, m1_revision_signal_parts};
use crate::memory_render::M1_PLACEHOLDER;
use crate::scheduler::{
    self, BoundaryBypass, ContextUsage, DeferredExecute, ExecuteThresholdConfig, LatchState,
    SchedulerConfig, SchedulerInputs, SessionMeta, TailState,
};
use crate::selection::{
    select_reductions, PassClass, SelItem, SelKind, SelectionConfig, SelectionContext,
};
use mc_core::{classify, CkItem, ClassifierInput, CoreState, FrozenUnit, PassInput, PassPlan};
use mc_store::{
    Channel1AppendRow, DeferredExecuteState, McStore, McStoreError, McTagRow, ModuleMeta,
    ModuleUsage, PendingAgentDrop, PendingRewriteState, StoredCompartment, TagMintInput,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use crate::ck_wire::{
    duplicate_ids, project_messages, reduced_block, split_block_id, CkIngressMessage, CkWireBlock,
    CkWireError, CkWireMessage, FlatBlock, FlatProjection,
};

/// Max CAS retries before surfacing the conflict (the module is the single writer in
/// the daemon case, so this rarely loops; the shared-store case re-loads and re-steps).
const MAX_CAS_RETRIES: u32 = 8;

/// Reserved synthetic-block ids (never carried by a real conversation item).
#[cfg(test)]
const M0_ID: &str = "mc_m0";
/// The reserved id prefix: a non-synthetic item bearing it is a contract violation.
const RESERVED_ID_PREFIX: &str = "mc_";
const SYNTH_REGION_KIND: &str = "synthesized-region";
/// Frozen-unit key prefix for a tail reduction (a reduced tool output / superseded edit).
/// `red:<target_id>` — the target is the real tail item whose bytes are replaced.
const RED_KEY_PREFIX: &str = "red:";
/// Repeated pending/raw ↔ present interleaving should be impossible with correctly
/// separated upstream session keys. Five edges corresponds to three arm/clear cycles
/// when the initial arm is not counted as evidence of multiplexing by itself.
const PENDING_REWRITE_AMBIGUOUS_EDGE_THRESHOLD: u32 = 5;
const CHANNEL1_FLOOR_TOKENS: i64 = 10_000;
const CHANNEL1_REFIRE_FLOOR_TOKENS: i64 = 10_000;
const CHANNEL1_PRESSURE_FLOOR: f64 = 0.8;
const CHANNEL1_USABLE_FRACTION: f64 = 1.0 / 3.0;

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

/// Legacy flat request item accepted only for backward compatibility with older
/// request fixtures. New callers send `messages` and receive bare CK messages.
#[derive(Debug, Clone, Deserialize)]
struct LegacyCkItemWire {
    pub id: String,
    pub ordinal: u64,
    pub bytes: String,
    #[serde(default)]
    pub synthetic: bool,
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
    /// Whether memory tools and m0 memory rendering are enabled for this binding.
    pub memory_enabled: bool,
    /// The wall-clock now (ms) for THIS pass. Used only to SET `meta.expiry_cutoff_ms` on
    /// a HARD (the first materialization freezes it); every later pass reads the frozen
    /// meta value, never this, so expiry never drifts the bytes between passes.
    pub now_ms: i64,
    /// Execute threshold frozen at route bind for scheduler and selection headroom.
    pub execute_threshold_percentage: f64,
    /// Smart-drop selector gate frozen at route bind.
    pub smart_drops: bool,
    /// Cache TTL string from SessionMeta config; defaults to `5m`.
    pub cache_ttl: String,
    /// Provider/model key for threshold lookup. Per-model overrides are deferred, so
    /// production currently supplies None.
    pub model_key: Option<String>,
    /// In-process response observation. None disables TTL-hard even if durable metadata
    /// has an older sparse commit anchor.
    pub observed_last_response_at_ms: Option<i64>,
    /// Current `Today's date: ...` guidance line. The transform copies it only when
    /// this pass already rewrites cached context; updating wall-clock text during an
    /// otherwise stable pass would make the date itself a reason to rewrite again.
    pub guidance_date: Option<String>,
    #[cfg(test)]
    pub injected_reductions: Vec<ReductionDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclaredTrim {
    pub flat_boundary_id: String,
    pub boundary_bare_message_id: String,
    pub boundary_absolute_ordinal: u64,
    pub next_absolute_ordinal: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TrimMismatch {
    pub predicate: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundaryState {
    LivePresent,
    DeclaredTrimValidated,
    Absent,
}

impl BoundaryState {
    fn is_present(&self) -> bool {
        matches!(self, Self::LivePresent | Self::DeclaredTrimValidated)
    }
}

/// A transform pass request. `boundary_present` is deliberately NOT a field: it is a
/// cache-correctness decision (replay-frozen vs reconcile) that the module computes
/// from its own durable state, never caller-supplied (a caller-supplied value would be
/// a poison surface — a crafted array could force a wrong replay or reconcile). The
/// wire carries full CK messages; the module flattens them at ingress and groups them
/// back to CK messages at egress.
#[derive(Debug, Clone, Serialize)]
pub struct TransformRequest {
    #[serde(default)]
    pub kind: String,
    #[serde(default = "default_wire_version")]
    pub v: u32,
    /// Required on the v2 wire. It is a plain string at the parse layer so a missing
    /// or unknown value can be reported with the typed contract error instead of serde's
    /// generic malformed-request path.
    pub serializer_profile: String,
    pub session_id: String,
    pub render_config: String,
    /// Whether this request advertises the canonical reduction tool. Missing input is
    /// deliberately false so older callers stay on the dormant byte path.
    #[serde(default)]
    pub tool_present: bool,
    /// Caller-owned identity for the full raw array. The module treats it as opaque and
    /// only echoes it on success-shaped responses so consumers can validate cached bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_array_fingerprint: Option<String>,
    pub messages: Vec<CkIngressMessage>,
    /// Future delta optimization. Parsed explicitly so a delta-shaped request is rejected
    /// with flow-control bytes rather than silently treated as an empty/full payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_delta: Option<Value>,
    #[serde(default)]
    pub usage: Option<ModuleUsage>,
    #[serde(default)]
    pub provider_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_trim: Option<DeclaredTrim>,
}

fn default_wire_version() -> u32 {
    2
}

#[derive(Deserialize)]
struct TransformRequestWire {
    #[serde(default)]
    kind: String,
    #[serde(default = "default_wire_version")]
    v: u32,
    #[serde(default)]
    serializer_profile: Option<String>,
    session_id: String,
    render_config: String,
    #[serde(default)]
    tool_present: bool,
    #[serde(default)]
    full_array_fingerprint: Option<String>,
    #[serde(default)]
    messages: Vec<CkIngressMessage>,
    #[serde(default)]
    items: Vec<LegacyCkItemWire>,
    #[serde(default)]
    tail_delta: Option<Value>,
    #[serde(default)]
    usage: Option<ModuleUsage>,
    #[serde(default)]
    provider_error: Option<String>,
    #[serde(default)]
    declared_trim: Option<DeclaredTrim>,
}

impl<'de> Deserialize<'de> for TransformRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = TransformRequestWire::deserialize(deserializer)?;
        let messages = if wire.messages.is_empty() && !wire.items.is_empty() {
            wire.items.into_iter().map(legacy_item_to_message).collect()
        } else {
            wire.messages
        };
        Ok(Self {
            kind: wire.kind,
            v: wire.v,
            serializer_profile: wire.serializer_profile.unwrap_or_default(),
            session_id: wire.session_id,
            render_config: wire.render_config,
            tool_present: wire.tool_present,
            full_array_fingerprint: wire.full_array_fingerprint,
            messages,
            tail_delta: wire.tail_delta,
            usage: wire.usage,
            provider_error: wire.provider_error,
            declared_trim: wire.declared_trim,
        })
    }
}

fn legacy_item_to_message(item: LegacyCkItemWire) -> CkIngressMessage {
    CkIngressMessage {
        mid: item.id.clone(),
        ordinal: item.ordinal,
        ck: CkWireMessage::from_parts(
            "user",
            vec![ck_wire::CkWireBlock::bare(ck_wire::CkKind::Text {
                text: item.bytes,
            })],
            None,
            ck_wire::ProviderExtras::new(),
            ck_wire::HarnessMeta {
                harness_id: Some(item.id),
                synthetic: item.synthetic,
                ..Default::default()
            },
        ),
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransformStatus {
    Ok,
    NeedFullSync,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServedFrom {
    Transform,
    DaemonLkg,
}

/// The coherent request-local reduction surface state reported to the forwarding layer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceState {
    Inactive,
    Transition,
    Active,
}

/// A transform pass result. Diagnostics remain alongside the CK array, but the response
/// messages themselves are bare CK messages: no request-only `mid` or `ordinal` sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransformResponse {
    pub status: TransformStatus,
    pub served_from: ServedFrom,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_array_fingerprint: Option<String>,
    pub action: String,
    pub boundary_id: String,
    pub reconcile_pending: bool,
    pub version: u64,
    pub row_version: u64,
    pub surface_state: SurfaceState,
    pub committed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_ordinal: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub historian: Option<HistorianDiagnostics>,
    /// The actual output messages for this pass: synthetic m0 and m1 messages followed
    /// by the tail messages, all expressed as bare CK messages. `None` (field ABSENT on
    /// the wire) on a `need_full_sync` response: the consumer discriminates structurally
    /// on array presence, and an empty array would be a third ambiguous state between
    /// "transformed to nothing" and "re-send required". Every `ok` response carries
    /// `Some`, even when legitimately empty.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ck_messages: Option<Vec<CkWireMessage>>,
}

impl TransformResponse {
    /// The output array of an `ok`/passthrough response; empty for `need_full_sync`
    /// (whose wire form omits the field entirely).
    pub fn messages(&self) -> &[CkWireMessage] {
        self.ck_messages.as_deref().unwrap_or(&[])
    }

    pub fn need_full_sync(full_array_fingerprint: Option<String>) -> Self {
        Self {
            status: TransformStatus::NeedFullSync,
            served_from: ServedFrom::Transform,
            full_array_fingerprint,
            action: "NEED_FULL_SYNC".to_string(),
            boundary_id: String::new(),
            reconcile_pending: false,
            version: 0,
            row_version: 0,
            surface_state: SurfaceState::Inactive,
            committed: false,
            coverage_ordinal: None,
            historian: None,
            ck_messages: None,
        }
    }

    pub fn passthrough(
        ck_messages: Vec<CkWireMessage>,
        full_array_fingerprint: Option<String>,
    ) -> Self {
        Self {
            status: TransformStatus::Ok,
            served_from: ServedFrom::Transform,
            full_array_fingerprint,
            action: "PASSTHROUGH".to_string(),
            boundary_id: String::new(),
            reconcile_pending: false,
            version: 0,
            row_version: 0,
            surface_state: SurfaceState::Inactive,
            committed: false,
            coverage_ordinal: None,
            historian: None,
            ck_messages: Some(ck_messages),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistorianDiagnostics {
    pub fired: bool,
    pub reason: Option<String>,
    pub no_fire: Option<String>,
    pub state: String,
    /// Tail-size progress numbers from the trigger's boundary resolution, absent when the
    /// pass never reached boundary resolution (busy, load failure, no messages). Purely
    /// observational: lets a rig drive see eligible content approach the fire bar per pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<HistorianTriggerProgress>,
    /// Detail of the most recent failed firing, from durable state. Present until a later
    /// firing establishes its producer run; supervised deployments have no stderr capture,
    /// so this is the only place the failure reason is visible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistorianTriggerProgress {
    pub eligible_chunk_tokens: f64,
    pub tail_size_bar: f64,
    pub protected_tail_n_tokens: f64,
    pub protected_start_ordinal: u64,
}

pub struct TransformWithProjection {
    pub response: TransformResponse,
    pub projection: FlatProjection,
    pub scheduler_pass: scheduler::PassDecision,
    pub boundary_state: BoundaryState,
    pub trim_mismatch: Option<TrimMismatch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaggableKind {
    Message,
    ToolResult,
}

impl TaggableKind {
    fn as_store_kind(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::ToolResult => "tool_result",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel1Level {
    Gentle,
    Firm,
    Urgent,
}

impl Channel1Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gentle => "gentle",
            Self::Firm => "firm",
            Self::Urgent => "urgent",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Gentle => 1,
            Self::Firm => 2,
            Self::Urgent => 3,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "gentle" => Some(Self::Gentle),
            "firm" => Some(Self::Firm),
            "urgent" => Some(Self::Urgent),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct TagOverlayState {
    tag_by_block_id: BTreeMap<String, i64>,
    channel1_by_block_id: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct ActiveTagForNudge {
    tag_number: i64,
    kind: String,
    token_count: i64,
}

#[derive(Debug, Clone)]
struct Channel1Decision {
    fire: bool,
    level: Channel1Level,
    reclaimable_tokens: i64,
    next_last_nudge: i64,
    next_last_level: String,
}

struct Channel1NudgeInputs<'a, 'ctx> {
    store: &'a McStore,
    req: &'a TransformRequest,
    ctx: &'a ProducerContext<'ctx>,
    core: &'a CoreState,
    projection: &'a FlatProjection,
    tag_rows: &'a [McTagRow],
    channel1_appends: &'a [Channel1AppendRow],
    context_limit_tokens: f64,
    input_tokens: f64,
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
    /// A stored coverage range overlaps, or the live array proves a present raw message
    /// would be trimmed without being covered by any compartment. Fail loud.
    CoverageGap(String),
    /// CK ingress rejected an unsupported or unpairable block before any partial projection.
    CkWire(CkWireError),
    /// Two flattened blocks produced the same `mid#block_index` id in one request.
    DuplicateBlockId(String),
    /// A live message's block-kind/fingerprint vector changed after first sight.
    IdentityDrift(String),
    /// A frozen synthetic todo pair could not be replayed at its stored tail anchor.
    SyntheticTodoAnchorMissing(String),
    /// A frozen reduction still names a live message, but that exact block disappeared.
    FrozenRedTargetVanish(String),
    /// A bust folded/advanced coverage from a compartment, but the anchor it minted (the
    /// last covered block's id) is empty or absent from the live input this pass. The
    /// anchor can then never be present, so reconcile can never clear and the pass loops
    /// as an unbounded phantom HARD. Fail loud: it signals an empty or wrong-vocabulary
    /// compartment end_message_id (the anchor must be a flat block id, `<mid>#<index>`).
    BoundaryNotPresent(String),
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
            TransformError::CkWire(e) => write!(f, "ck wire: {e}"),
            TransformError::DuplicateBlockId(id) => write!(f, "duplicate flattened block id: {id}"),
            TransformError::IdentityDrift(mid) => {
                write!(f, "CK message block identity drift for mid {mid}")
            }
            TransformError::SyntheticTodoAnchorMissing(mid) => write!(
                f,
                "synthetic todo anchor mid {mid} is missing from the live tail"
            ),
            TransformError::FrozenRedTargetVanish(id) => {
                write!(
                    f,
                    "frozen reduction target vanished while its message is live: {id}"
                )
            }
            TransformError::BoundaryNotPresent(m) => {
                write!(f, "minted boundary not present: {m}")
            }
        }
    }
}
impl std::error::Error for TransformError {}

impl From<CkWireError> for TransformError {
    fn from(e: CkWireError) -> Self {
        TransformError::CkWire(e)
    }
}

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

/// Apply one transform pass, retrying the whole load→classify→step→commit cycle on a
/// CAS conflict (re-classification depends on the freshly-loaded state). `ctx` is the
/// resolved project producer context (m0/m1 are composed from its store reads). Tail
/// reductions are produced inside the pass from the scheduler-gated selector.
///
/// The real Claude token estimator ([`mc_tokenizer::estimate_tokens`]) is injected into
/// the m0 compose (the decay renderer's budget guard). It is reached ONLY on the
/// Hard/MigrateHard arm — never SOFT, defer, m1 compose, or the tail splice — so it can
/// only change bytes during an intentional HARD rematerialization; determinism (the same
/// text always counts identically, via the vendored+pinned vocab) is what preserves
/// byte-identical replay between HARDs.
pub fn transform(
    store: &McStore,
    req: &TransformRequest,
    ctx: &ProducerContext<'_>,
) -> Result<TransformResponse, TransformError> {
    transform_with_projection(store, req, ctx).map(|result| result.response)
}

pub fn transform_with_projection(
    store: &McStore,
    req: &TransformRequest,
    ctx: &ProducerContext<'_>,
) -> Result<TransformWithProjection, TransformError> {
    apply_once_with_estimator(store, req, ctx, mc_tokenizer::estimate_tokens)
}

/// The retry wrapper around [`apply_once`], parameterized by the token estimator so tests
/// can inject a panicking/counting one to prove the estimator is HARD-only (never called
/// on SOFT/defer). Production always passes [`mc_tokenizer::estimate_tokens`].
fn apply_once_with_estimator(
    store: &McStore,
    req: &TransformRequest,
    ctx: &ProducerContext<'_>,
    estimate_tokens: impl Fn(&str) -> usize + Copy,
) -> Result<TransformWithProjection, TransformError> {
    let mut attempt = 0;
    loop {
        match apply_once(store, req, ctx, estimate_tokens) {
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
    estimate_tokens: impl Fn(&str) -> usize + Copy,
) -> Result<TransformWithProjection, TransformError> {
    // --- ingress: CK messages -> flat blocks, then strip synthetic before cache logic ---
    let projection = project_messages(&req.messages)?;
    if let Some(id) = duplicate_ids(&projection.blocks) {
        return Err(TransformError::DuplicateBlockId(id));
    }
    let live: Vec<&FlatBlock> = projection
        .blocks
        .iter()
        .filter(|i| !i.synthetic())
        .collect();
    for item in &live {
        if item.id().starts_with(RESERVED_ID_PREFIX) {
            return Err(TransformError::ReservedId);
        }
    }
    let mut prev: Option<u64> = None;
    for msg in req.messages.iter().filter(|m| !m.ck.meta.synthetic) {
        if let Some(p) = prev {
            if msg.ordinal <= p {
                return Err(TransformError::OrdinalViolation);
            }
        }
        prev = Some(msg.ordinal);
    }

    let serializer_profile = SerializerProfile::parse(&req.serializer_profile);
    let cc_u1_active = crate::cc_u1_active(serializer_profile, req.tool_present);
    let loaded = store.load(&req.session_id)?;
    let surface_transition = serializer_profile == Some(SerializerProfile::ClaudeCodeAnthropic)
        && loaded.meta.cc_u1_active != cc_u1_active;
    let surface_state = if surface_transition {
        SurfaceState::Transition
    } else if cc_u1_active {
        SurfaceState::Active
    } else {
        SurfaceState::Inactive
    };

    // Every module-owned byte-affecting epoch is folded before activation decisions. The
    // tagger is active only after its non-zero epoch is present in the session's committed
    // render identity, so an established dormant session cannot acquire tags before the
    // coordinating cache-breaking HARD fold has committed.
    let memory_render_epoch = if crate::MEMORY_RENDER_FORMAT_EPOCH != 0 {
        format!("mre{}", crate::MEMORY_RENDER_FORMAT_EPOCH)
    } else {
        String::new()
    };
    let compartment_render_epoch = if crate::COMPARTMENT_RENDER_FORMAT_EPOCH != 0 {
        format!("cre{}", crate::COMPARTMENT_RENDER_FORMAT_EPOCH)
    } else {
        String::new()
    };
    let profile_render_epoch = serializer_profile
        .map(crate::profile_render_epoch)
        .filter(|epoch| *epoch != 0)
        .map(|epoch| format!("mpe{epoch}"))
        .unwrap_or_default();
    let tagger_feature_epoch = match crate::tagger_feature_epoch(cc_u1_active) {
        0 => String::new(),
        epoch => format!("tfe{epoch}"),
    };
    let effective_render_config = fold_m0_content_epoch(
        &req.render_config,
        &M0ContentEpoch {
            workspace_fingerprint: store.workspace_fingerprint(ctx.project_path)?,
            upgrade_state: String::new(),
            memory_content_epoch: String::new(),
            memory_render_epoch,
            compartment_render_epoch,
            profile_render_epoch,
            tagger_feature_epoch: tagger_feature_epoch.clone(),
        },
    );
    let tagging_active = cc_u1_active && loaded.meta.cc_u1_active;
    let tag_rows = if cc_u1_active {
        // Prior rows must be visible on the activation transition so protected pending
        // drops are classified against the same durable tag history as steady passes.
        if tagging_active {
            mint_tags_for_projection(
                store,
                &req.session_id,
                &projection,
                &loaded.core,
                ctx.now_ms,
            )?;
        }
        store.load_tags_for_session(&req.session_id)?
    } else {
        Vec::new()
    };
    let mut channel1_appends = if tagging_active {
        store.load_channel1_appends(&req.session_id)?
    } else {
        Vec::new()
    };

    // Check whether the boundary is present in the live messages, or through a shadow
    // trim record that matches durable coverage and the first untrimmed message. A failed
    // trim record is treated as Absent so the existing reconciliation error paths still run.
    let (boundary_state, trim_mismatch) =
        resolve_boundary_state(store, req, &loaded.core, &loaded.meta, &live)?;
    let boundary_present = boundary_state.is_present();
    let boundary_token = if boundary_present {
        loaded.core.boundary_id.clone()
    } else {
        "-".to_string()
    };

    let mut has_compartments_cache: Option<bool> = None;
    let mut pending_rewrite_absent_shape = false;
    if !boundary_present {
        let needs_lineage_check = loaded.meta.pending_rewrite.is_some()
            || !loaded.core.boundary_id.is_empty()
            || loaded.meta.coverage_ordinal.is_some()
            || {
                let has_compartments = store.has_compartments(&req.session_id)?;
                has_compartments_cache = Some(has_compartments);
                has_compartments
            };
        if needs_lineage_check {
            let compartments = store.load_compartments(&req.session_id)?;
            let has_compartments = !compartments.is_empty();
            has_compartments_cache = Some(has_compartments);
            pending_rewrite_absent_shape = loaded.row_version.is_some()
                && has_durable_lineage(&loaded.core, &loaded.meta, has_compartments)
                && surviving_revert_prefix_seq(&compartments, &live) < 0;
        }
    }

    // A share-nothing boundary absence on a session with held lineage is not a valid
    // re-cut target. New conversations must arrive on a fresh upstream key; on an old
    // key this is either a missed lineage switch or foreign traffic. The destructive
    // truncate-all arm is eliminated, not gated: only a committed truncate may bump the
    // revert epoch, and this arm never owns a truncate. It records one durable alarm and
    // then serves raw bytes without touching identity, usage, scheduler, or core state.
    if pending_rewrite_absent_shape {
        let fingerprint = absent_shape_fingerprint(&live);
        if loaded.meta.pending_rewrite.is_some() {
            eprintln!(
                "mc-module: pending_rewrite raw pass-through for {} fingerprint {}",
                req.session_id, fingerprint
            );
            let passthrough_overlay =
                tagging_active.then(|| tag_overlay_state(&tag_rows, &channel1_appends));
            return Ok(pending_passthrough_result(
                projection,
                req,
                loaded.row_version.unwrap_or(0),
                false,
                trim_mismatch,
                passthrough_overlay.as_ref(),
                surface_state,
            ));
        }

        let core = loaded.core.clone();
        let mut meta = loaded.meta.clone();
        let mut trip_count = meta.pending_rewrite_trip_count;
        if trip_count > 0 {
            trip_count = trip_count.saturating_add(1);
        }
        let ambiguous = meta.pending_rewrite_ambiguous
            || trip_count >= PENDING_REWRITE_AMBIGUOUS_EDGE_THRESHOLD;
        meta.pending_rewrite = Some(PendingRewriteState {
            armed_at_ms: ctx.now_ms,
            absent_shape_fingerprint: fingerprint.clone(),
            absent_request_count: 1,
            last_present_at_ms: (meta.last_committed_pass_at_ms > 0)
                .then_some(meta.last_committed_pass_at_ms),
        });
        meta.pending_rewrite_trip_count = trip_count;
        meta.pending_rewrite_ambiguous = ambiguous;
        meta.pending_rewrite_last_failure = Some(pending_rewrite_detail(
            &req.session_id,
            &fingerprint,
            ambiguous,
        ));
        meta.last_committed_pass_at_ms = ctx.now_ms;
        let row_version = store.commit(&req.session_id, loaded.row_version, &core, &meta)?;
        eprintln!(
            "mc-module: armed pending_rewrite for {} fingerprint {} ambiguous={}",
            req.session_id, fingerprint, ambiguous
        );
        let passthrough_overlay =
            tagging_active.then(|| tag_overlay_state(&tag_rows, &channel1_appends));
        return Ok(pending_passthrough_result(
            projection,
            req,
            row_version,
            true,
            trim_mismatch,
            passthrough_overlay.as_ref(),
            surface_state,
        ));
    }

    let clear_pending_rewrite_on_present =
        loaded.meta.pending_rewrite.is_some() && boundary_present;

    enforce_block_identity(&loaded.meta, &projection, &loaded.core)?;
    let pending_agent_drops = store.load_pending_agent_drops(&req.session_id)?;

    // --- CHEAP per-pass classify signals (read EVERY pass; never the m0/m1 BODY) ---
    // The cheap m1-change digest (watermark triple). Computed every pass to gate SOFT vs
    // defer WITHOUT composing the body; the body composes only on the bust arm below.
    let m1_signal = m1_revision_signal_parts(store, ctx.project_path, &req.session_id)?;
    let mut current_m1_digest = m1_signal.revision;
    let effective_usage = effective_usage(req.usage.as_ref(), loaded.meta.last_usage.as_ref());
    let context_limit_tokens = effective_context_limit_tokens(&effective_usage);
    let usage_input_tokens = effective_usage.current_total_input_tokens as f64;
    let usage_percentage = if context_limit_tokens > 0.0 {
        usage_input_tokens / context_limit_tokens * 100.0
    } else {
        0.0
    };
    let scheduler_outcome = scheduler::decide(&SchedulerInputs {
        config: scheduler_config(ctx.execute_threshold_percentage),
        usage: ContextUsage {
            percentage: usage_percentage,
            input_tokens: usage_input_tokens,
        },
        session: SessionMeta {
            last_response_time_ms: ctx
                .observed_last_response_at_ms
                .map(|ts| ts.max(0) as u64)
                .unwrap_or(0),
            cache_ttl: ctx.cache_ttl.clone(),
        },
        now_ms: ctx.now_ms.max(0) as u64,
        model_key: ctx.model_key.clone(),
        context_limit: Some(context_limit_tokens),
        tail_state: tail_state_from_live(&live),
        deferred_execute: loaded
            .meta
            .deferred_execute_state
            .as_ref()
            .map(deferred_from_meta),
        boundary_bypass: BoundaryBypass {
            explicit_bust: false,
            subagent: false,
        },
        drain_latch: latch_from_meta(&loaded.meta),
        overflow_error_text: req.provider_error.clone(),
    });
    // First-fold HARD trigger: a never-minted boundary (empty boundary_id) means no
    // compartment has ever folded into m0 (the fold is what mints the boundary). Once the
    // historian publishes the session's FIRST compartment, it cannot ride m1 as a SOFT
    // delta — a SOFT delta requires the boundary to be present so the new compartment can
    // splice onto it, and there is no boundary yet — so without this trigger it strands on
    // defer forever. Force a HARD to fold it and mint the first boundary. Uses a presence
    // check, NOT max_compartment_seq (which COALESCEs a missing MAX to 0, indistinguishable
    // from a real first compartment at sequence 0). Self-limiting: the fold mints a
    // non-empty boundary_id, so this is false on every subsequent pass and later publishes
    // correctly ride m1 as a SOFT delta once the boundary is present. The store query runs
    // only in this rare never-minted window (short-circuited by is_empty), never in steady
    // state where the boundary is present.
    let first_fold_due = if loaded.core.boundary_id.is_empty() {
        match has_compartments_cache {
            Some(value) => value,
            None => store.has_compartments(&req.session_id)?,
        }
    } else {
        false
    };
    let render_config_changed =
        loaded.meta.initialized && effective_render_config != loaded.meta.last_render_config;
    let reconcile_hard_due = loaded.core.reconcile_pending && !boundary_present;
    // If Claude-code coverage advances over system messages, force a HARD render so the
    // messages move into the m0 prefix before the byte-splice profile suppresses their
    // separate re-emission. The full compartment rows load only when the cheap max-seq
    // scalar says coverage may have changed since meta last recorded it.
    let compartment_seq_changed_since_meta = loaded.meta.initialized
        && m1_signal.max_compartment_seq != meta_coverage_compartment_seq(&loaded.meta);
    let system_absorb_hard_due = if serializer_profile
        == Some(SerializerProfile::ClaudeCodeAnthropic)
        && compartment_seq_changed_since_meta
    {
        let compartments = store.load_compartments(&req.session_id)?;
        let new_coverage = coverage_ordinal_from_compartments(&compartments)?;
        coverage_advance_covers_new_system(req, loaded.meta.coverage_ordinal, new_coverage)
    } else {
        false
    };
    let hard_fold_requested =
        first_fold_due || scheduler_outcome.idle_ttl_fired || system_absorb_hard_due;
    // Profile defaults remain conservative, while the request-local tool signal enables
    // full-array tail reclaim for the Claude Code profile. A false request therefore
    // retains the exact pre-capability behavior without changing the global profile table.
    let tail_reclaim_enabled =
        serializer_profile.is_none_or(|profile| healing::tail_reclaim(profile) || cc_u1_active);
    let producer_gate = tail_reclaim_enabled
        && producer_gate(
            scheduler_outcome.pass,
            !loaded.meta.initialized
                || render_config_changed
                || reconcile_hard_due
                || hard_fold_requested,
        );
    let selection_class = if producer_gate {
        selection_pass_class(scheduler_outcome.pass)
    } else {
        PassClass::Defer
    };
    let tail_for_selection = tail_sel_items(&live, loaded.meta.coverage_ordinal);
    let protected_block_ids = if cc_u1_active {
        newest_active_tag_block_ids(&loaded.core, &loaded.meta, &projection, &tag_rows)
    } else {
        HashSet::new()
    };
    let selected_reductions = if producer_gate {
        let frozen = frozen_red_targets(&loaded.core);
        // No per-request gate here: producer_gate already requires
        // tail_reclaim_enabled, which is the profile default OR the request-local
        // surface. Gating again on the request-local surface alone would starve
        // the durable queue on profiles whose default is true (owned/Pi/OpenCode
        // legs drain unconditionally).
        let agent_drop_ids = pending_agent_drops
            .iter()
            .map(|drop| drop.target_id.clone())
            .collect::<Vec<_>>();
        select_reductions(
            &tail_for_selection,
            &frozen,
            &SelectionContext {
                pass_class: selection_class,
                current_total_input_tokens: usage_input_tokens,
                ceiling_tokens: context_limit_tokens
                    * ctx.execute_threshold_percentage.clamp(1.0, 100.0)
                    / 100.0,
                protected_cutoff_ordinal: 0,
                last_execute_ordinal: if loaded.core.reconcile_pending {
                    0
                } else {
                    loaded.meta.last_execute_ordinal
                },
                prior_input_sample: loaded.meta.last_emergency_input_sample,
                has_prior_drop: loaded.meta.has_prior_emergency_drop,
                agent_drop_ids,
                protected_block_ids: protected_block_ids.clone(),
            },
            &SelectionConfig {
                smart_drops: ctx.smart_drops,
            },
        )
    } else {
        Vec::new()
    };
    #[cfg(test)]
    let selected_reductions = if ctx.injected_reductions.is_empty() {
        selected_reductions
    } else {
        ctx.injected_reductions.clone()
    };
    // Fail-loud monotonicity guard, BEFORE classify and on EVERY pass: a frozen
    // reduction target re-supplied with different bytes breaks the immutable contract,
    // and the set-membership trigger would silently skip it (already frozen) and serve
    // the stale bytes — including on a defer. Error here instead.
    validate_reduction_monotonicity(&loaded.core, &selected_reductions)?;

    let reductions_pending_now = reductions_pending(
        &loaded.core,
        &selected_reductions,
        &live,
        loaded.meta.coverage_ordinal,
    );
    let plan = classify(&ClassifierInput {
        initialized: loaded.meta.initialized,
        is_legacy_baseline: is_legacy_baseline(&loaded.core),
        valid_m0m1_shape: valid_m0m1_shape(&loaded.core),
        render_config_changed,
        hard_fold_requested,
        boundary_present,
        reconcile_pending: loaded.core.reconcile_pending,
        m1_revision_changed: current_m1_digest != loaded.meta.m1_revision,
        reductions_pending: reductions_pending_now,
    });

    let mut core = loaded.core.clone();
    let mut meta = loaded.meta.clone();
    let mut commit_expected = loaded.row_version;
    if clear_pending_rewrite_on_present {
        meta.pending_rewrite = None;
        meta.pending_rewrite_trip_count = meta.pending_rewrite_trip_count.saturating_add(1);
        if meta.pending_rewrite_trip_count >= PENDING_REWRITE_AMBIGUOUS_EDGE_THRESHOLD {
            meta.pending_rewrite_ambiguous = true;
            meta.pending_rewrite_last_failure = Some(pending_rewrite_detail(
                &req.session_id,
                "boundary_present_recovery",
                true,
            ));
            eprintln!(
                "mc-module: pending_rewrite ambiguous after boundary-present recovery for {}",
                req.session_id
            );
        } else if !meta.pending_rewrite_ambiguous {
            meta.pending_rewrite_last_failure = None;
        }
    }
    apply_ingress_meta(&mut meta, req, &projection);
    meta.cc_u1_active = cc_u1_active;
    if cc_u1_active {
        meta.last_serializer_profile = req.serializer_profile.clone();
    }
    apply_scheduler_meta(&mut meta, &scheduler_outcome);

    let is_bust_pass = matches!(
        plan,
        PassPlan::Hard | PassPlan::MigrateHard | PassPlan::Soft
    );
    if is_bust_pass {
        if let Some(guidance_date) = ctx.guidance_date.as_ref() {
            meta.guidance_date = guidance_date.clone();
        }
    }
    let tail_for_capture = tail_for_selection.clone();
    if is_bust_pass && tail_reclaim_enabled {
        capture_todo_state_on_bust(&mut meta, &tail_for_capture, true);
    }

    let mut coverage_shrunk_on_bust = false;
    let mut commit_memory_revision = None;

    match plan {
        PassPlan::Reject(m) => return Err(TransformError::UnknownShape(m)),
        PassPlan::Hard | PassPlan::MigrateHard => {
            // EXPENSIVE bust-only: compose the m0 baseline from the store. now_ms freezes
            // the expiry cutoff into meta so every later in-epoch SOFT/defer reads the
            // SAME memory set (a memory expiring mid-epoch stays rendered until the next
            // HARD re-freezes the cutoff — the byte-stability tradeoff).
            let compartments_for_live_coverage = store.load_compartments(&req.session_id)?;
            let coverage_bounds =
                coverage_bounds_from_compartments(&compartments_for_live_coverage)?;
            let covered_system_messages = covered_system_messages_for_coverage(
                req,
                coverage_bounds.map(|(_, end)| end),
                coverage_bounds.map(|(start, _)| start),
                serializer_profile,
            );
            let mut comp = compose_m0_from_store(
                store,
                &crate::m0_compose::M0ComposeInputs {
                    session_id: &req.session_id,
                    project_path: ctx.project_path,
                    project_directory: ctx.project_directory,
                    now_ms: ctx.now_ms,
                    history_budget_tokens: ctx.history_budget_tokens,
                    covered_system_messages: &covered_system_messages,
                    memory_enabled: ctx.memory_enabled,
                },
                estimate_tokens,
            )?;

            // Live coverage guard: store-pure validation allows sparse coordinate
            // gaps because consumer producers can retire ordinal numbers permanently.
            // Once the live array is available, every present non-system block at or
            // below the coverage end must fall inside some compartment range; otherwise
            // build_output would trim unsummarized raw bytes from the tail.
            if let Some(stray) = first_uncovered_live_block(
                &compartments_for_live_coverage,
                &live,
                comp.coverage_ordinal,
            ) {
                return Err(TransformError::CoverageGap(format!(
                    "coverage gap: live item {} (ordinal {}) sits at or below coverage end {:?} \
                     but no compartment covers it; composing m0 would silently drop it from the tail",
                    stray.id(),
                    stray.ordinal(),
                    comp.coverage_ordinal
                )));
            }

            // Mint-absent guard: when this fold takes its anchor from a compartment
            // (coverage present), the minted boundary must be a block id that exists in
            // the live input THIS pass — the anchor is the last covered block, which the
            // producer always sends (trimming happens in our OUTPUT, and a producer-side
            // coverage trim keeps ordinals >= coverage_ordinal, so the boundary block
            // itself is never trimmed away). An empty or absent mint means either the
            // compartment's end_message_id is empty or in the wrong vocabulary (it must
            // be the flat block id `<mid>#<index>`, not a bare message id), or the store
            // still covers messages a revert removed and has not been re-cut. Committing
            // such an anchor makes presence impossible on every later pass, so reconcile
            // can never clear and every pass re-materializes — an unbounded phantom-HARD
            // loop serving summaries of content that may no longer exist. Fail loud
            // instead, on EVERY hard including a reconcile-rematerialize: a rematerialize
            // that cannot mint a presentable anchor has no path to clearing reconcile
            // either, and the loud error repeats until the store is re-cut. A revert that
            // clears the compartments entirely stays legitimate: coverage is then None
            // and the fold mints the reserved empty anchor without entering this guard.
            if comp.coverage_ordinal.is_some() {
                let minted = comp.boundary_id.as_str();
                if minted.is_empty()
                    || !boundary_available(
                        minted,
                        &live,
                        &boundary_state,
                        req.declared_trim.as_ref(),
                    )
                {
                    if loaded.core.reconcile_pending {
                        let compartments = store.load_compartments(&req.session_id)?;
                        let keep_through_seq = surviving_revert_prefix_seq(&compartments, &live);
                        let outcome = store.truncate_compartments_for_revert(
                            &req.session_id,
                            keep_through_seq,
                            commit_expected,
                        )?;
                        commit_expected = Some(outcome.row_version);
                        meta.revert_epoch = outcome.revert_epoch;
                        meta.last_recut = outcome.last_recut;
                        current_m1_digest =
                            m1_revision_signal(store, ctx.project_path, &req.session_id)?;
                        let recut_compartments = store.load_compartments(&req.session_id)?;
                        let recut_coverage_bounds =
                            coverage_bounds_from_compartments(&recut_compartments)?;
                        let recut_covered_system_messages = covered_system_messages_for_coverage(
                            req,
                            recut_coverage_bounds.map(|(_, end)| end),
                            recut_coverage_bounds.map(|(start, _)| start),
                            serializer_profile,
                        );
                        comp = compose_m0_from_store(
                            store,
                            &crate::m0_compose::M0ComposeInputs {
                                session_id: &req.session_id,
                                project_path: ctx.project_path,
                                project_directory: ctx.project_directory,
                                now_ms: ctx.now_ms,
                                history_budget_tokens: ctx.history_budget_tokens,
                                covered_system_messages: &recut_covered_system_messages,
                                memory_enabled: ctx.memory_enabled,
                            },
                            estimate_tokens,
                        )?;
                        meta.last_execute_ordinal = meta
                            .last_execute_ordinal
                            .min(comp.coverage_ordinal.unwrap_or(0));

                        if let Some(stray) = first_uncovered_live_block(
                            &recut_compartments,
                            &live,
                            comp.coverage_ordinal,
                        ) {
                            return Err(TransformError::CoverageGap(format!(
                                "coverage gap after re-cut: live item {} (ordinal {}) is below coverage end {:?} but uncovered",
                                stray.id(),
                                stray.ordinal(),
                                comp.coverage_ordinal
                            )));
                        }

                        if comp.coverage_ordinal.is_some() {
                            let reminted = comp.boundary_id.as_str();
                            if reminted.is_empty()
                                || !boundary_available(
                                    reminted,
                                    &live,
                                    &boundary_state,
                                    req.declared_trim.as_ref(),
                                )
                            {
                                return Err(TransformError::BoundaryNotPresent(format!(
                                    "re-cut kept compartments through sequence {keep_through_seq}, \
                                     but the fold still minted absent anchor {reminted:?}; \
                                     the publisher must write flat end_message_id block ids"
                                )));
                            }
                        }
                    } else {
                        return Err(TransformError::BoundaryNotPresent(format!(
                            "fold minted anchor {minted:?} from the folded compartment's \
                             end_message_id, but no live block carries that id; the anchor \
                             must be the flat block id (`<mid>#<index>`) of the last covered \
                             block; check the publisher's end_message_id"
                        )));
                    }
                }
            }

            // The reductions that SURVIVE the fold: m0 is now a compartment SUMMARY (not
            // covered raw bytes), so a reduction on a now-covered item simply drops with
            // it (no "fold reduced bytes into m0"); a target still in the new tail is kept;
            // a reverted-away target is an orphan. apply_units can't delete → rebuild.
            let effective = effective_reductions(&core, &selected_reductions);
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
            coverage_shrunk_on_bust =
                coverage_shrank(loaded.meta.coverage_ordinal, comp.coverage_ordinal);
            if coverage_shrunk_on_bust && tail_reclaim_enabled {
                let post_truncate_tail = tail_sel_items(&live, comp.coverage_ordinal);
                capture_todo_state_on_bust(&mut meta, &post_truncate_tail, true);
            }
            meta.coverage_ordinal = comp.coverage_ordinal;
            meta.coverage_start_ordinal = comp.first_covered_ordinal;
            meta.coverage_compartment_seq = Some(comp.folded_compartment_seq);
            meta.folded_compartment_seq = comp.folded_compartment_seq;
            commit_memory_revision = Some(comp.memory_revision.clone());
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
                &selected_reductions,
                &live,
                loaded.meta.coverage_ordinal,
            ));
            // A coverage-extending SOFT advances the boundary anchor (the bound core
            // primitive); a memory-only SOFT leaves it put (None).
            let new_boundary_id = m1.new_coverage.as_ref().map(|(id, _)| id.clone());
            if let Some((_, coverage_end)) = &m1.new_coverage {
                let compartments_for_live_coverage = store.load_compartments(&req.session_id)?;
                if let Some(stray) = first_uncovered_live_block(
                    &compartments_for_live_coverage,
                    &live,
                    Some(*coverage_end),
                ) {
                    return Err(TransformError::CoverageGap(format!(
                        "coverage gap: live item {} (ordinal {}) sits at or below coverage end {} \
                         but no compartment covers it; composing m1 would silently drop it from the tail",
                        stray.id(),
                        stray.ordinal(),
                        coverage_end
                    )));
                }
            }
            // Mint-absent guard, SOFT arm (same invariant as the fold's guard above): an
            // advanced anchor must exist in the live input this pass, or presence can
            // never hold afterward and the session decays into a reconcile-HARD loop. A
            // SOFT can only reach here with reconcile clear (the classifier routes a
            // pending reconcile to defer/HARD), so every advance is a fresh mint and the
            // check is unconditional.
            if let Some(id) = &new_boundary_id {
                if id.is_empty()
                    || !boundary_available(id, &live, &boundary_state, req.declared_trim.as_ref())
                {
                    return Err(TransformError::BoundaryNotPresent(format!(
                        "coverage-extending delta advanced the anchor to {id:?}, but no \
                         live block carries that id; the anchor must be the flat block id \
                         (`<mid>#<index>`) of the last covered block"
                    )));
                }
            }
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
                meta.coverage_compartment_seq = Some(m1_signal.max_compartment_seq);
                // A coverage advance folds items out of the tail, so frozen red:*
                // units targeting them must go WITH the coverage. Only the HARD arm
                // rebuilds the frozen set (surviving_red_units); without this prune a
                // covered reduction would survive a coverage-extending SOFT as silent
                // bloat — and a later re-decide of that target with different bytes
                // would false-fire the monotonicity conflict guard.
                prune_covered_red_units(&mut core, &live, meta.coverage_ordinal);
            } else if compartment_seq_changed_since_meta {
                meta.coverage_compartment_seq = Some(m1_signal.max_compartment_seq);
            }
            meta.m1_revision = current_m1_digest;
        }
        PassPlan::Defer => {
            core.step(PassInput {
                proposed: Some(mc_core::Action::SoftPlus),
                boundary_present: boundary_token,
                ..Default::default()
            });
            if compartment_seq_changed_since_meta && current_m1_digest == loaded.meta.m1_revision {
                meta.coverage_compartment_seq = Some(m1_signal.max_compartment_seq);
            }
        }
    }

    // The two-pass watermark advances on EVERY scheduler execute-class decision
    // (Execute/Force85/Emergency95), not only on passes that froze reductions: this
    // execute's tail is the NEXT execute's age-drop candidate set, so a zero-drop execute
    // must still stamp the max ordinal or completed arcs never age in. It does NOT advance
    // when the producer gate opened via a hard advisory on a scheduler-Defer pass
    // (first-fold, render-config change): those busts are not execute cadence, and
    // stamping there would age the current tail into the very next execute. The write may
    // be the only meta change on the pass — a metadata-only commit with byte-identical
    // output, not a cache bust. Held back under reconcile (the watermark may be stale-high
    // against a store about to be re-cut; the re-cut arm re-clamps it).
    let scheduler_execute_class =
        tail_reclaim_enabled && !matches!(scheduler_outcome.pass, scheduler::PassDecision::Defer);
    if scheduler_execute_class && !loaded.core.reconcile_pending {
        meta.last_execute_ordinal = tail_for_selection
            .iter()
            .map(|item| item.ordinal)
            .max()
            .unwrap_or(0)
            .max(meta.last_execute_ordinal);
    }
    if is_bust_pass && reductions_pending_now && selection_class == PassClass::EmergencyForce {
        meta.last_emergency_input_sample = usage_input_tokens;
        meta.has_prior_emergency_drop = true;
    }

    if tail_reclaim_enabled {
        advance_synthetic_todo(
            &mut meta,
            is_bust_pass,
            loaded.meta.coverage_ordinal,
            coverage_shrunk_on_bust,
            req,
        )?;
    }

    let result_action = action_str(&plan, &core);

    let mut tag_overlay = if tagging_active {
        tag_overlay_state(&tag_rows, &channel1_appends)
    } else {
        TagOverlayState::default()
    };
    if tagging_active {
        if let Some(row) = maybe_append_channel1_nudge(
            Channel1NudgeInputs {
                store,
                req,
                ctx,
                core: &core,
                projection: &projection,
                tag_rows: &tag_rows,
                channel1_appends: &channel1_appends,
                context_limit_tokens,
                input_tokens: usage_input_tokens,
            },
            &mut meta,
        )? {
            tag_overlay
                .channel1_by_block_id
                .insert(row.block_id.clone(), row.reminder_text.clone());
            channel1_appends.push(row);
        }
    }

    let ck_messages = build_output(
        &core,
        &meta,
        &projection,
        req,
        tagging_active.then_some(&tag_overlay),
        tail_reclaim_enabled,
    )?;

    // Build the output before committing so a missing synthetic-todo anchor cannot
    // persist an unusable frozen pair. Pending rows are classified from the final plan:
    // live unfrozen targets remain durable, while applied or retired targets are consumed.
    let consumed_drop_ids = consumed_pending_drop_ids(
        &pending_agent_drops,
        &loaded.core,
        &core,
        &projection,
        meta.coverage_ordinal,
    );
    let state_changed = core != loaded.core || meta != loaded.meta;
    if state_changed {
        meta.last_committed_pass_at_ms = ctx.now_ms;
    }
    let commit_required = state_changed || !consumed_drop_ids.is_empty();
    let row_version = if commit_required {
        if consumed_drop_ids.is_empty() && commit_memory_revision.is_none() {
            store.commit(&req.session_id, commit_expected, &core, &meta)?
        } else {
            store.commit_with_consumed_drops(
                &req.session_id,
                commit_expected,
                &core,
                &meta,
                &consumed_drop_ids,
                commit_memory_revision.as_ref(),
            )?
        }
    } else {
        loaded.row_version.unwrap_or(0)
    };

    Ok(TransformWithProjection {
        projection,
        scheduler_pass: scheduler_outcome.pass,
        boundary_state,
        trim_mismatch,
        response: TransformResponse {
            status: TransformStatus::Ok,
            served_from: ServedFrom::Transform,
            full_array_fingerprint: req.full_array_fingerprint.clone(),
            action: result_action,
            boundary_id: core.boundary_id.clone(),
            reconcile_pending: core.reconcile_pending,
            version: core.version,
            row_version,
            surface_state,
            committed: commit_required,
            coverage_ordinal: meta.coverage_ordinal,
            historian: None,
            ck_messages: Some(ck_messages),
        },
    })
}

fn enforce_block_identity(
    meta: &ModuleMeta,
    projection: &FlatProjection,
    core: &CoreState,
) -> Result<(), TransformError> {
    for (mid, vector) in &projection.identity_by_mid {
        if let Some(stored) = meta.block_identity_by_mid.get(mid) {
            if stored != vector {
                return Err(TransformError::IdentityDrift(mid.clone()));
            }
        }
    }

    let live_ids: BTreeSet<&str> = projection
        .blocks
        .iter()
        .filter(|block| !block.synthetic)
        .map(|block| block.id.as_str())
        .collect();
    let live_mids: BTreeSet<&str> = projection
        .identity_by_mid
        .keys()
        .map(String::as_str)
        .collect();
    for target in frozen_red_targets(core) {
        let Some((mid, _)) = split_block_id(&target) else {
            continue;
        };
        if live_mids.contains(mid) && !live_ids.contains(target.as_str()) {
            return Err(TransformError::FrozenRedTargetVanish(target));
        }
    }
    Ok(())
}

fn apply_ingress_meta(meta: &mut ModuleMeta, req: &TransformRequest, projection: &FlatProjection) {
    for (mid, vector) in &projection.identity_by_mid {
        meta.block_identity_by_mid
            .entry(mid.clone())
            .or_insert_with(|| vector.clone());
    }
    meta.newest_live_block_id = projection
        .blocks
        .iter()
        .filter(|block| !block.synthetic)
        .max_by_key(|block| block.ordinal)
        .map(|block| block.id.clone());
    if let Some(usage) = req.usage.as_ref().filter(|usage| usage.is_non_zero()) {
        meta.last_usage = Some(usage.clone());
    }
}

fn effective_usage(request: Option<&ModuleUsage>, persisted: Option<&ModuleUsage>) -> ModuleUsage {
    request
        .filter(|usage| usage.is_non_zero())
        .or(persisted)
        .cloned()
        .unwrap_or_default()
}

fn effective_context_limit_tokens(usage: &ModuleUsage) -> f64 {
    if usage.context_limit_tokens >= crate::scheduler::MIN_PLAUSIBLE_CONTEXT_LIMIT {
        usage.context_limit_tokens as f64
    } else {
        200_000.0
    }
}

fn scheduler_config(execute_threshold_percentage: f64) -> SchedulerConfig {
    SchedulerConfig {
        execute_threshold_percentage: ExecuteThresholdConfig::Percentage(
            execute_threshold_percentage,
        ),
        execute_threshold_tokens: None,
    }
}

fn producer_gate(pass: scheduler::PassDecision, hard_advisory: bool) -> bool {
    !matches!(pass, scheduler::PassDecision::Defer) || hard_advisory
}

fn selection_pass_class(pass: scheduler::PassDecision) -> PassClass {
    match pass {
        scheduler::PassDecision::Force85 | scheduler::PassDecision::Emergency95 => {
            PassClass::EmergencyForce
        }
        scheduler::PassDecision::Defer | scheduler::PassDecision::Execute => PassClass::Execute,
    }
}

fn deferred_from_meta(state: &DeferredExecuteState) -> DeferredExecute {
    DeferredExecute {
        reason: state.reason.clone(),
    }
}

fn deferred_to_meta(state: DeferredExecute) -> DeferredExecuteState {
    DeferredExecuteState {
        reason: state.reason,
    }
}

fn latch_from_meta(meta: &ModuleMeta) -> LatchState {
    LatchState {
        active_since_ms: meta
            .emergency_drain_active
            .then_some(meta.emergency_drain_entered_at_ms.max(0) as u64),
    }
}

fn apply_scheduler_meta(meta: &mut ModuleMeta, outcome: &scheduler::SchedulerOutcome) {
    meta.deferred_execute_state = if matches!(outcome.pass, scheduler::PassDecision::Defer) {
        outcome.deferred_execute.clone().map(deferred_to_meta)
    } else {
        None
    };
    meta.emergency_drain_active = outcome.drain_latch.active_since_ms.is_some();
    meta.emergency_drain_entered_at_ms = outcome
        .drain_latch
        .active_since_ms
        .map(|ts| ts as i64)
        .unwrap_or(0);
}

fn tail_state_from_live(live: &[&FlatBlock]) -> TailState {
    let Some(newest_assistant_ordinal) = live
        .iter()
        .filter(|block| block.role == "assistant")
        .map(|block| block.ordinal())
        .max()
    else {
        return TailState {
            mid_tool_use: false,
        };
    };
    let completed_arcs: HashSet<&str> = live
        .iter()
        .filter(|block| block.kind_tag == "tool_result" && !block.provider_executed)
        .filter_map(|block| block.arc_id.as_deref())
        .collect();
    let mid_tool_use = live.iter().any(|block| {
        block.role == "assistant"
            && block.ordinal() == newest_assistant_ordinal
            && block.kind_tag == "tool_call"
            && !block.provider_executed
            && block
                .arc_id
                .as_deref()
                .is_some_and(|arc| !completed_arcs.contains(arc))
    });
    TailState { mid_tool_use }
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

fn is_uncovered_leading_system(message: &CkIngressMessage, meta: &ModuleMeta) -> bool {
    if message.ck.role != "system" || meta.coverage_ordinal.is_none() {
        return false;
    }
    match meta.coverage_start_ordinal {
        Some(start) => message.ordinal < start,
        // Legacy metadata did not persist the first covered ordinal. Preserve the common
        // pinned system prompt at absolute ordinal 0 rather than risk dropping it.
        None => message.ordinal == 0,
    }
}

fn coverage_ordinal_from_compartments(
    compartments: &[StoredCompartment],
) -> Result<Option<u64>, TransformError> {
    coverage_bounds_from_compartments(compartments).map(|coverage| coverage.map(|(_, end)| end))
}

fn coverage_bounds_from_compartments(
    compartments: &[StoredCompartment],
) -> Result<Option<(u64, u64)>, TransformError> {
    resolve_coverage(compartments)
        .map(|coverage| coverage.map(|c| (c.first_covered_ordinal, c.coverage_end_ordinal)))
        .map_err(|gap| TransformError::CoverageGap(gap.to_string()))
}

fn meta_coverage_compartment_seq(meta: &ModuleMeta) -> i64 {
    meta.coverage_compartment_seq
        .unwrap_or(meta.folded_compartment_seq)
}

fn system_content_for_m0(message: &CkWireMessage) -> String {
    if message.content.len() == 1 {
        if let ck_wire::CkKind::Text { text } = &message.content[0].kind {
            return text.clone();
        }
    }
    serde_json::to_string(&message.content).unwrap_or_default()
}

fn covered_system_messages_for_coverage(
    req: &TransformRequest,
    coverage_ordinal: Option<u64>,
    coverage_start_ordinal: Option<u64>,
    profile: Option<SerializerProfile>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut covered = Vec::new();
    for message in req.messages.iter().filter(|message| {
        if message.ck.meta.synthetic
            || message.ck.role != "system"
            || is_tail(message.ordinal, coverage_ordinal)
        {
            return false;
        }
        if profile == Some(SerializerProfile::ClaudeCodeAnthropic) {
            return true;
        }
        coverage_start_ordinal.is_none_or(|start| message.ordinal >= start)
    }) {
        let content = system_content_for_m0(&message.ck);
        if seen.insert(content.clone()) {
            covered.push(content);
        }
    }
    covered
}

fn coverage_advance_covers_new_system(
    req: &TransformRequest,
    old_coverage: Option<u64>,
    new_coverage: Option<u64>,
) -> bool {
    if !coverage_advanced(old_coverage, new_coverage) {
        return false;
    }
    req.messages.iter().any(|message| {
        !message.ck.meta.synthetic
            && message.ck.role == "system"
            && is_tail(message.ordinal, old_coverage)
            && !is_tail(message.ordinal, new_coverage)
    })
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

/// Return only queue rows whose target was frozen by this plan or is no longer a live,
/// unfrozen tail block. Every other row remains durable for a later eligible pass.
fn consumed_pending_drop_ids(
    pending: &[PendingAgentDrop],
    loaded_core: &CoreState,
    final_core: &CoreState,
    projection: &FlatProjection,
    final_coverage: Option<u64>,
) -> Vec<i64> {
    let frozen_before = frozen_red_targets(loaded_core);
    let frozen_after = frozen_red_targets(final_core);
    // Retirement must be PROVEN, not inferred from absence: the request array can be
    // a transient subset of the session (interactive side-requests), so a target
    // missing from this pass's projection may reappear on the next one. Consuming
    // its row on absence would silently lose an acknowledged drop. A block that is
    // PRESENT but at-or-under the coverage watermark is permanently retired
    // (coverage only advances), and an already-frozen target is satisfied — those
    // are the only non-applied rows safe to consume.
    let covered = projection
        .blocks
        .iter()
        .filter(|block| !block.synthetic && !is_tail(block.ordinal, final_coverage))
        .map(|block| block.id.as_str())
        .collect::<HashSet<_>>();

    pending
        .iter()
        .filter(|drop| {
            let applied =
                !frozen_before.contains(&drop.target_id) && frozen_after.contains(&drop.target_id);
            let obsolete = frozen_before.contains(&drop.target_id)
                || covered.contains(drop.target_id.as_str());
            applied || obsolete
        })
        .map(|drop| drop.id)
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

/// Fail-loud monotonicity guard (runs EVERY pass, before classify). If the selector
/// supplies a reduction whose target is ALREADY frozen with DIFFERENT bytes, that
/// breaks the immutable-once-frozen contract — and the set-membership trigger would
/// SILENTLY skip it (already in keys) and serve the stale frozen payload. Error instead.
fn validate_reduction_monotonicity(
    core: &CoreState,
    reductions: &[ReductionDecision],
) -> Result<(), TransformError> {
    for r in reductions {
        if let Some(frozen) = frozen_red_payload(core, &r.target_id) {
            if frozen != r.payload {
                return Err(TransformError::ReductionConflict);
            }
        }
    }
    Ok(())
}

/// Is there a NEW reduction to freeze: a selected reduction whose target is in the live
/// tail AND not yet frozen. Pure id set-membership — the SOFT trigger.
fn reductions_pending(
    core: &CoreState,
    reductions: &[ReductionDecision],
    live: &[&FlatBlock],
    coverage: Option<u64>,
) -> bool {
    let frozen = frozen_red_targets(core);
    let tail: std::collections::HashSet<&str> = live
        .iter()
        .filter(|i| is_tail(i.ordinal(), coverage))
        .map(|i| i.id())
        .collect();
    reductions
        .iter()
        .any(|r| tail.contains(r.target_id.as_str()) && !frozen.contains(&r.target_id))
}

/// The `red:*` units to freeze on a SOFT: each NEW selected reduction (target in the live
/// tail, not yet frozen), deduped by target, deterministic order.
fn new_reduction_units(
    core: &CoreState,
    reductions: &[ReductionDecision],
    live: &[&FlatBlock],
    coverage: Option<u64>,
) -> Vec<FrozenUnit> {
    let frozen = frozen_red_targets(core);
    let tail: std::collections::HashSet<&str> = live
        .iter()
        .filter(|i| is_tail(i.ordinal(), coverage))
        .map(|i| i.id())
        .collect();
    let mut by_target: BTreeMap<String, FrozenUnit> = BTreeMap::new();
    for r in reductions {
        if tail.contains(r.target_id.as_str()) && !frozen.contains(&r.target_id) {
            by_target
                .entry(r.target_id.clone())
                .or_insert_with(|| red_unit(&r.target_id, &r.kind, &r.payload));
        }
    }
    by_target.into_values().collect()
}

/// The reductions in EFFECT this pass, snapshotted BEFORE any frozen-set mutation (the
/// HARD-fold snapshot): every frozen `red:*` (authoritative payload) ∪ every NEW selected
/// reduction (target not yet frozen). Keyed by target_id → (kind, payload), deterministic.
fn effective_reductions(
    core: &CoreState,
    reductions: &[ReductionDecision],
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
    for r in reductions {
        eff.entry(r.target_id.clone())
            .or_insert_with(|| (r.kind.clone(), r.payload.clone()));
    }
    eff
}

/// Drop frozen `red:*` units whose target is now COVERED (ordinal at/below the new
/// coverage). Runs on a coverage-extending SOFT, where the tail shrinks but the frozen
/// set is otherwise kept: a reduction whose target left the tail can never be applied
/// again (`build_output` trims covered items first), so keeping it is pure bloat and a
/// false-conflict trap if the same target id is ever re-decided after a revert.
fn prune_covered_red_units(
    core: &mut mc_core::CoreState,
    live: &[&FlatBlock],
    new_coverage: Option<u64>,
) {
    let live_ord: BTreeMap<&str, u64> = live.iter().map(|i| (i.id(), i.ordinal())).collect();
    core.frozen_units.retain(|u| {
        let Some(target) = u.key.strip_prefix("red:") else {
            return true; // non-reduction units are coverage-independent
        };
        match live_ord.get(target) {
            Some(&ord) => is_tail(ord, new_coverage),
            // Target absent from the live array: leave it to the HARD-fold orphan GC,
            // which sees the authoritative post-revert array.
            None => true,
        }
    });
}

/// The `red:*` units that SURVIVE a HARD rebuild: a target that is COVERED (folded into
/// m0) is dropped; a target in the new TAIL is kept; a target ABSENT from the live array
/// (reverted away) is dropped as an orphan. So a unit survives iff its target is in the
/// live array AND still in the tail after the fold.
fn surviving_red_units(
    effective: &BTreeMap<String, (String, String)>,
    live: &[&FlatBlock],
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

fn sel_item_from_flat(block: &FlatBlock) -> SelItem {
    let kind = match &block.wire.kind {
        ck_wire::CkKind::ToolCall { name, input, .. } => SelKind::ToolCall {
            name: name.clone(),
            input: input.clone(),
        },
        ck_wire::CkKind::ToolResult { tool_name, .. } => SelKind::ToolResult {
            tool_name: tool_name.clone(),
        },
        ck_wire::CkKind::Reasoning { .. } => SelKind::Reasoning,
        ck_wire::CkKind::Text { .. } => SelKind::Text,
        ck_wire::CkKind::RedactedReasoning { .. } => SelKind::RedactedReasoning,
        ck_wire::CkKind::Media(_) => SelKind::Media,
        ck_wire::CkKind::Opaque(_) => SelKind::Opaque,
    };
    SelItem {
        id: block.id.clone(),
        ordinal: block.ordinal,
        kind,
        provider_executed: block.provider_executed,
        byte_size: block.bytes.len(),
        arc_id: block.arc_id.clone(),
    }
}

fn tail_sel_items(live: &[&FlatBlock], coverage: Option<u64>) -> Vec<SelItem> {
    live.iter()
        .filter(|block| is_tail(block.ordinal(), coverage))
        .map(|block| sel_item_from_flat(block))
        .collect()
}

fn tail_end_mid(req: &TransformRequest, coverage: Option<u64>) -> Option<String> {
    req.messages
        .iter()
        .rfind(|msg| !msg.ck.meta.synthetic && is_tail(msg.ordinal, coverage))
        .map(|msg| msg.mid.clone())
}

fn tail_contains_mid(req: &TransformRequest, coverage: Option<u64>, mid: &str) -> bool {
    req.messages
        .iter()
        .any(|msg| !msg.ck.meta.synthetic && msg.mid == mid && is_tail(msg.ordinal, coverage))
}

fn coverage_advanced(old: Option<u64>, new: Option<u64>) -> bool {
    match (old, new) {
        (Some(old), Some(new)) => new > old,
        (None, Some(_)) => true,
        _ => false,
    }
}

fn coverage_shrank(old: Option<u64>, new: Option<u64>) -> bool {
    match (old, new) {
        (Some(old), Some(new)) => new < old,
        (Some(_), None) => true,
        _ => false,
    }
}

fn stored_compartment_covers_ordinal(compartment: &StoredCompartment, ordinal: u64) -> bool {
    let start = compartment.start_message.max(0) as u64;
    let end = compartment.end_message.max(0) as u64;
    start <= ordinal && ordinal <= end
}

fn first_uncovered_live_block<'a>(
    compartments: &[StoredCompartment],
    live: &[&'a FlatBlock],
    coverage: Option<u64>,
) -> Option<&'a FlatBlock> {
    let coverage = coverage?;
    live.iter()
        .copied()
        .filter(|block| block.role != "system" && block.ordinal() <= coverage)
        .filter(|block| {
            !compartments
                .iter()
                .any(|compartment| stored_compartment_covers_ordinal(compartment, block.ordinal()))
        })
        .min_by_key(|block| block.ordinal())
}

fn boundary_available(
    id: &str,
    live: &[&FlatBlock],
    boundary_state: &BoundaryState,
    declared: Option<&DeclaredTrim>,
) -> bool {
    live.iter().any(|block| block.id() == id)
        || matches!(boundary_state, BoundaryState::DeclaredTrimValidated)
            && declared.is_some_and(|declared| declared.flat_boundary_id == id)
}

fn resolve_boundary_state(
    store: &McStore,
    req: &TransformRequest,
    core: &CoreState,
    meta: &ModuleMeta,
    live: &[&FlatBlock],
) -> Result<(BoundaryState, Option<TrimMismatch>), TransformError> {
    if !core.boundary_id.is_empty() && live.iter().any(|block| block.id() == core.boundary_id) {
        return Ok((BoundaryState::LivePresent, None));
    }

    let Some(declared) = req.declared_trim.as_ref() else {
        return Ok((BoundaryState::Absent, None));
    };

    if declared.flat_boundary_id != core.boundary_id {
        return Ok((
            BoundaryState::Absent,
            Some(trim_mismatch(
                "boundary_identity",
                format!(
                    "declared boundary {:?} did not match durable boundary {:?}",
                    declared.flat_boundary_id, core.boundary_id
                ),
            )),
        ));
    }

    if meta.coverage_ordinal != Some(declared.boundary_absolute_ordinal) {
        return Ok((
            BoundaryState::Absent,
            Some(trim_mismatch(
                "coverage_ordinal",
                format!(
                    "declared boundary ordinal {} did not match durable coverage {:?}",
                    declared.boundary_absolute_ordinal, meta.coverage_ordinal
                ),
            )),
        ));
    }

    let compartments = store.load_compartments(&req.session_id)?;
    let tail = compartments
        .iter()
        .max_by_key(|compartment| compartment.sequence);
    match tail {
        Some(tail)
            if tail.end_message_id == declared.flat_boundary_id
                && tail.end_message == declared.boundary_absolute_ordinal as i64
                && split_block_id(&tail.end_message_id)
                    .map(|(mid, _)| mid == declared.boundary_bare_message_id)
                    .unwrap_or(false) => {}
        Some(tail) => {
            return Ok((
                BoundaryState::Absent,
                Some(trim_mismatch(
                    "tail_compartment",
                    format!(
                        "tail compartment ended at id {:?} ordinal {}, not declared id {:?} bare {:?} ordinal {}",
                        tail.end_message_id,
                        tail.end_message,
                        declared.flat_boundary_id,
                        declared.boundary_bare_message_id,
                        declared.boundary_absolute_ordinal
                    ),
                )),
            ));
        }
        None => {
            return Ok((
                BoundaryState::Absent,
                Some(trim_mismatch(
                    "tail_compartment",
                    "declared trim had no durable tail compartment".to_string(),
                )),
            ));
        }
    }

    let first_live_non_system = req
        .messages
        .iter()
        .filter(|message| !message.ck.meta.synthetic && message.ck.role != "system")
        .map(|message| message.ordinal)
        .min();
    if first_live_non_system != Some(declared.next_absolute_ordinal) {
        return Ok((
            BoundaryState::Absent,
            Some(trim_mismatch(
                "continuity",
                format!(
                    "first non-system live ordinal {:?} did not match declared next ordinal {}",
                    first_live_non_system, declared.next_absolute_ordinal
                ),
            )),
        ));
    }

    Ok((BoundaryState::DeclaredTrimValidated, None))
}

fn trim_mismatch(predicate: &'static str, detail: String) -> TrimMismatch {
    TrimMismatch { predicate, detail }
}

fn surviving_revert_prefix_seq(compartments: &[StoredCompartment], live: &[&FlatBlock]) -> i64 {
    let live_ids: BTreeSet<&str> = live.iter().map(|block| block.id()).collect();
    compartments
        .iter()
        .take_while(|compartment| live_ids.contains(compartment.end_message_id.as_str()))
        .map(|compartment| compartment.sequence)
        .last()
        .unwrap_or(-1)
}

fn has_durable_lineage(core: &CoreState, meta: &ModuleMeta, has_compartments: bool) -> bool {
    has_compartments || !core.boundary_id.is_empty() || meta.coverage_ordinal.is_some()
}

fn absent_shape_fingerprint(live: &[&FlatBlock]) -> String {
    let mut hasher = Sha256::new();
    for block in live {
        hasher.update(block.id.as_bytes());
        hasher.update([0]);
        hasher.update(block.ordinal.to_le_bytes());
        hasher.update([0]);
        hasher.update(block.role.as_bytes());
        hasher.update([0]);
        hasher.update(block.kind_tag.as_bytes());
        hasher.update([0]);
        hasher.update(block.bytes.as_bytes());
        hasher.update([0xff]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn pending_rewrite_detail(session_id: &str, fingerprint: &str, ambiguous: bool) -> String {
    let state = if ambiguous {
        "ambiguous_pending_rewrite"
    } else {
        "pending_rewrite"
    };
    format!(
        "{state}: boundary-absent share-nothing array on session {session_id}; \
         absent_shape_fingerprint={fingerprint}; expected causes are an upstream \
         lineage-switch detection miss or foreign traffic on this session key; serving raw \
         pass-through and preserving the held lineage"
    )
}

fn pending_passthrough_result(
    projection: FlatProjection,
    req: &TransformRequest,
    row_version: u64,
    committed: bool,
    trim_mismatch: Option<TrimMismatch>,
    tag_overlay: Option<&TagOverlayState>,
    surface_state: SurfaceState,
) -> TransformWithProjection {
    let blocks_by_mid = projection_blocks_by_mid(&projection);
    let mut response = TransformResponse::passthrough(
        req.messages
            .iter()
            .map(|message| {
                let mut rendered = message.ck.clone();
                if let Some(blocks) = blocks_by_mid.get(message.mid.as_str()) {
                    apply_tag_overlay_to_message(
                        &mut rendered,
                        message,
                        blocks,
                        tag_overlay,
                        |_| false,
                    );
                }
                rendered
            })
            .collect(),
        req.full_array_fingerprint.clone(),
    );
    response.row_version = row_version;
    response.surface_state = surface_state;
    response.committed = committed;
    TransformWithProjection {
        projection,
        scheduler_pass: scheduler::PassDecision::Defer,
        boundary_state: BoundaryState::Absent,
        trim_mismatch,
        response,
    }
}

fn anchor_folded_by_coverage(
    req: &TransformRequest,
    old_coverage: Option<u64>,
    new_coverage: Option<u64>,
    anchor_mid: &str,
) -> bool {
    coverage_advanced(old_coverage, new_coverage)
        && req.messages.iter().any(|msg| {
            !msg.ck.meta.synthetic
                && msg.mid == anchor_mid
                && is_tail(msg.ordinal, old_coverage)
                && !is_tail(msg.ordinal, new_coverage)
        })
}

fn advance_synthetic_todo(
    meta: &mut ModuleMeta,
    is_bust_pass: bool,
    old_coverage: Option<u64>,
    coverage_shrunk_on_bust: bool,
    req: &TransformRequest,
) -> Result<(), TransformError> {
    let existing = meta.synthetic_todo.clone();
    let outcome = advance_injection_from_meta(meta, existing.as_ref(), is_bust_pass);
    match outcome {
        InjectionOutcome::Replace(next) => {
            let anchor_mid = tail_end_mid(req, meta.coverage_ordinal);
            meta.synthetic_todo = Some((*next).freeze_at(anchor_mid));
        }
        InjectionOutcome::Clear => meta.synthetic_todo = None,
        InjectionOutcome::Keep => {
            if is_bust_pass {
                reanchor_kept_synthetic_todo_if_folded_or_shrunk(
                    meta,
                    old_coverage,
                    coverage_shrunk_on_bust,
                    req,
                )?;
            }
        }
        InjectionOutcome::None => {}
    }
    Ok(())
}

fn reanchor_kept_synthetic_todo_if_folded_or_shrunk(
    meta: &mut ModuleMeta,
    old_coverage: Option<u64>,
    coverage_shrunk_on_bust: bool,
    req: &TransformRequest,
) -> Result<(), TransformError> {
    let Some(pair) = meta.synthetic_todo.as_mut() else {
        return Ok(());
    };
    let Some(anchor_mid) = pair.anchor_mid.clone() else {
        return Ok(());
    };
    if tail_contains_mid(req, meta.coverage_ordinal, &anchor_mid) {
        return Ok(());
    }
    let folded_by_advance =
        anchor_folded_by_coverage(req, old_coverage, meta.coverage_ordinal, &anchor_mid);
    if !folded_by_advance && !coverage_shrunk_on_bust {
        return Err(TransformError::SyntheticTodoAnchorMissing(anchor_mid));
    }

    // A coverage-moving bust already changes the rendered bytes: advance folds the old
    // anchor into history, while shrink means the old anchor was in reverted-away tail. In
    // both cases an unchanged synthetic todo can move to the new tail end without turning
    // into an always-last floater on ordinary tail growth or defer passes.
    debug_assert!(folded_by_advance || coverage_shrunk_on_bust);
    pair.anchor_mid = tail_end_mid(req, meta.coverage_ordinal);
    Ok(())
}

fn push_synthetic_todo_pair(out: &mut Vec<CkWireMessage>, meta: &ModuleMeta) {
    if let Some(pair) = &meta.synthetic_todo {
        out.push(pair.assistant_msg.clone());
        out.push(pair.tool_msg.clone());
    }
}

fn mint_tags_for_projection(
    store: &McStore,
    session_id: &str,
    projection: &FlatProjection,
    core: &CoreState,
    now_ms: i64,
) -> Result<(), TransformError> {
    let frozen = frozen_red_targets(core);
    let inputs = projection
        .blocks
        .iter()
        .filter(|block| !frozen.contains(&block.id))
        .filter_map(|block| {
            let (kind, source) = taggable_source(block)?;
            Some(TagMintInput {
                block_id: block.id.clone(),
                kind: kind.as_store_kind().to_string(),
                token_count: mc_tokenizer::estimate_tokens(source) as i64,
                source_bytes: source.as_bytes().to_vec(),
            })
        })
        .collect::<Vec<_>>();
    if !inputs.is_empty() {
        store.mint_or_get_tags(session_id, &inputs, now_ms)?;
    }
    Ok(())
}

/// Return exactly the span the overlay can prefix. Mint scope and overlay scope share
/// this predicate so every visible tag number has a renderable §N§ carrier.
fn taggable_source(block: &FlatBlock) -> Option<(TaggableKind, &str)> {
    if block.synthetic || block.role == "system" {
        return None;
    }
    match &block.wire.kind {
        ck_wire::CkKind::Text { text } if block.role == "user" || block.role == "assistant" => {
            Some((TaggableKind::Message, text))
        }
        ck_wire::CkKind::ToolResult { output, .. } => match &output.kind {
            ck_wire::CkOutputKind::Text { text } | ck_wire::CkOutputKind::ErrorText { text } => {
                Some((TaggableKind::ToolResult, text))
            }
            ck_wire::CkOutputKind::Content { blocks } => blocks.iter().find_map(|block| {
                if let ck_wire::ResultBlockKind::Text { text } = &block.kind {
                    Some((TaggableKind::ToolResult, text.as_str()))
                } else {
                    None
                }
            }),
            ck_wire::CkOutputKind::Json { .. }
            | ck_wire::CkOutputKind::ErrorJson { .. }
            | ck_wire::CkOutputKind::ExecutionDenied { .. } => None,
        },
        _ => None,
    }
}

fn taggable_kind(block: &FlatBlock) -> Option<TaggableKind> {
    taggable_source(block).map(|(kind, _)| kind)
}

/// Compute the newest protected tags as exact block ids over the current canonical tail.
/// Stored provenance must still match the live carrier before a row can occupy a slot.
fn newest_active_tag_block_ids(
    core: &CoreState,
    meta: &ModuleMeta,
    projection: &FlatProjection,
    tag_rows: &[McTagRow],
) -> HashSet<String> {
    const PROTECTED_TAG_COUNT: usize = 20;

    let block_by_id = projection
        .blocks
        .iter()
        .map(|block| (block.id.as_str(), block))
        .collect::<HashMap<_, _>>();
    let mut active = tag_rows
        .iter()
        .filter(|row| {
            let Some(block) = block_by_id.get(row.block_id.as_str()) else {
                return false;
            };
            if !is_tail(block.ordinal, meta.coverage_ordinal)
                || frozen_red_payload(core, block.id()).is_some()
            {
                return false;
            }
            let Some((kind, source)) = taggable_source(block) else {
                return false;
            };
            row.kind == kind.as_store_kind() && row.source_bytes == source.as_bytes()
        })
        .collect::<Vec<_>>();
    active.sort_by(|left, right| {
        right
            .tag_number
            .cmp(&left.tag_number)
            .then_with(|| right.block_id.cmp(&left.block_id))
    });
    active
        .into_iter()
        .take(PROTECTED_TAG_COUNT)
        .map(|row| row.block_id.clone())
        .collect()
}

fn tag_overlay_state(tag_rows: &[McTagRow], appends: &[Channel1AppendRow]) -> TagOverlayState {
    TagOverlayState {
        tag_by_block_id: tag_rows
            .iter()
            .map(|row| (row.block_id.clone(), row.tag_number))
            .collect(),
        channel1_by_block_id: appends
            .iter()
            .map(|row| (row.block_id.clone(), row.reminder_text.clone()))
            .collect(),
    }
}

fn apply_tag_overlay_to_message(
    message: &mut CkWireMessage,
    ingress: &CkIngressMessage,
    blocks: &[&FlatBlock],
    overlay: Option<&TagOverlayState>,
    is_reduced: impl Fn(&FlatBlock) -> bool,
) {
    let Some(overlay) = overlay else {
        return;
    };
    if ingress.ck.role == "system" || ingress.ck.meta.synthetic {
        return;
    }
    let mut modified = false;
    for block in blocks {
        if block.block_index >= message.content.len() {
            continue;
        }
        if !is_reduced(block) {
            if let Some(kind) = taggable_kind(block) {
                if let Some(tag_number) = overlay.tag_by_block_id.get(&block.id) {
                    modified |= apply_tag_prefix_to_block(
                        ingress.ck.role.as_str(),
                        &mut message.content[block.block_index],
                        kind,
                        *tag_number,
                    );
                } else {
                    debug_assert!(
                        false,
                        "taggable block {} was not minted before render",
                        block.id
                    );
                }
            }
            if let Some(reminder) = overlay.channel1_by_block_id.get(&block.id) {
                modified |=
                    append_channel1_to_block(&mut message.content[block.block_index], reminder);
            }
        }
    }
    if modified {
        message.mark_modified();
    }
}

fn apply_tag_prefix_to_block(
    role: &str,
    block: &mut CkWireBlock,
    kind: TaggableKind,
    tag_number: i64,
) -> bool {
    match (&mut block.kind, kind) {
        (ck_wire::CkKind::Text { text }, TaggableKind::Message)
            if role == "user" || role == "assistant" =>
        {
            let next = prepend_tag(tag_number, text);
            if *text != next {
                *text = next;
                return true;
            }
        }
        (ck_wire::CkKind::ToolResult { output, .. }, TaggableKind::ToolResult) => {
            return prepend_tag_to_tool_output(output, tag_number);
        }
        _ => {}
    }
    false
}

fn prepend_tag_to_tool_output(output: &mut ck_wire::CkToolOutput, tag_number: i64) -> bool {
    match &mut output.kind {
        ck_wire::CkOutputKind::Text { text } | ck_wire::CkOutputKind::ErrorText { text } => {
            let next = prepend_tag(tag_number, text);
            if *text != next {
                *text = next;
                return true;
            }
        }
        ck_wire::CkOutputKind::Content { blocks } => {
            for block in blocks {
                if let ck_wire::ResultBlockKind::Text { text } = &mut block.kind {
                    let next = prepend_tag(tag_number, text);
                    if *text != next {
                        *text = next;
                        return true;
                    }
                    return false;
                }
            }
        }
        ck_wire::CkOutputKind::Json { .. }
        | ck_wire::CkOutputKind::ErrorJson { .. }
        | ck_wire::CkOutputKind::ExecutionDenied { .. } => {}
    }
    false
}

fn append_channel1_to_block(block: &mut CkWireBlock, reminder: &str) -> bool {
    match &mut block.kind {
        ck_wire::CkKind::ToolResult { output, .. } => append_channel1_to_output(output, reminder),
        _ => false,
    }
}

fn append_channel1_to_output(output: &mut ck_wire::CkToolOutput, reminder: &str) -> bool {
    match &mut output.kind {
        ck_wire::CkOutputKind::Text { text } | ck_wire::CkOutputKind::ErrorText { text } => {
            if !text.ends_with(reminder) {
                text.push_str(reminder);
                return true;
            }
        }
        ck_wire::CkOutputKind::Content { blocks } => {
            for block in blocks {
                if let ck_wire::ResultBlockKind::Text { text } = &mut block.kind {
                    if !text.ends_with(reminder) {
                        text.push_str(reminder);
                        return true;
                    }
                    return false;
                }
            }
        }
        ck_wire::CkOutputKind::Json { .. }
        | ck_wire::CkOutputKind::ErrorJson { .. }
        | ck_wire::CkOutputKind::ExecutionDenied { .. } => {}
    }
    false
}

fn tag_prefix(tag_number: i64) -> String {
    format!("§{tag_number}§ ")
}

fn prepend_tag(tag_number: i64, value: &str) -> String {
    let tagged = format!("{}{value}", tag_prefix(tag_number));
    debug_assert_eq!(strip_tag_prefix(&tagged, tag_number), value);
    tagged
}

/// Remove exactly the prefix added for this block's registered number. This is the
/// inverse of [`prepend_tag`]; it never trims source whitespace or interprets another
/// block's tag-like user content.
fn strip_tag_prefix(value: &str, tag_number: i64) -> &str {
    value.strip_prefix(&tag_prefix(tag_number)).unwrap_or(value)
}

fn maybe_append_channel1_nudge(
    input: Channel1NudgeInputs<'_, '_>,
    meta: &mut ModuleMeta,
) -> Result<Option<Channel1AppendRow>, TransformError> {
    let active_tags = active_tags_for_nudge(input.core, meta, input.projection, input.tag_rows);
    let live_tail_tokens = active_tags
        .iter()
        .map(|tag| tag.token_count.max(0))
        .sum::<i64>();
    let working_window_tokens = (input.context_limit_tokens
        * input.ctx.execute_threshold_percentage.clamp(1.0, 100.0)
        / 100.0)
        .round()
        .max(0.0) as i64;
    let reclaimable_tokens =
        reclaimable_older_than_working_window(&active_tags, working_window_tokens);
    let decision = decide_channel1(
        reclaimable_tokens,
        live_tail_tokens,
        working_window_tokens,
        input.context_limit_tokens,
        input.input_tokens,
        input.ctx.execute_threshold_percentage,
        meta,
    );
    meta.channel1_last_nudge_undropped = decision.next_last_nudge;
    meta.channel1_last_nudge_level = decision.next_last_level;
    let was_suppressed = meta.channel1_reduce_suppressed;
    meta.channel1_reduce_suppressed = false;
    if was_suppressed || !decision.fire {
        return Ok(None);
    }
    let existing_blocks = input
        .channel1_appends
        .iter()
        .map(|row| row.block_id.as_str())
        .collect::<HashSet<_>>();
    let Some(block_id) =
        newest_tool_result_for_channel1(input.core, meta, input.projection, &existing_blocks)
    else {
        return Ok(None);
    };
    let hint = oldest_reclaimable_hint(&active_tags, working_window_tokens);
    let reminder = build_channel1_reminder(decision.level, decision.reclaimable_tokens, &hint);
    let inserted = input.store.append_channel1_nudge(
        &input.req.session_id,
        &block_id,
        &reminder,
        input.ctx.now_ms,
    )?;
    if inserted {
        Ok(Some(Channel1AppendRow {
            block_id,
            reminder_text: reminder,
            fired_at_ms: input.ctx.now_ms,
        }))
    } else {
        Ok(None)
    }
}

fn active_tags_for_nudge(
    core: &CoreState,
    meta: &ModuleMeta,
    projection: &FlatProjection,
    tag_rows: &[McTagRow],
) -> Vec<ActiveTagForNudge> {
    let tag_by_block = tag_rows
        .iter()
        .map(|row| (row.block_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let mut out = Vec::new();
    for block in projection.blocks.iter().filter(|block| {
        taggable_kind(block).is_some()
            && is_tail(block.ordinal, meta.coverage_ordinal)
            && frozen_red_payload(core, block.id()).is_none()
    }) {
        if let Some(row) = tag_by_block.get(block.id.as_str()) {
            out.push(ActiveTagForNudge {
                tag_number: row.tag_number,
                kind: row.kind.clone(),
                token_count: row.token_count.max(0),
            });
        }
    }
    out.sort_by_key(|tag| tag.tag_number);
    out
}

fn reclaimable_older_than_working_window(
    active_tags: &[ActiveTagForNudge],
    working_window_tokens: i64,
) -> i64 {
    let mut protected = HashSet::new();
    let mut sum = 0i64;
    for tag in active_tags.iter().rev() {
        if sum >= working_window_tokens.max(0) {
            break;
        }
        protected.insert(tag.tag_number);
        sum += tag.token_count.max(0);
    }
    active_tags
        .iter()
        .filter(|tag| tag.kind != "tool_call" && !protected.contains(&tag.tag_number))
        .map(|tag| tag.token_count.max(0))
        .sum()
}

fn decide_channel1(
    reclaimable_tokens: i64,
    live_tail_tokens: i64,
    working_window_tokens: i64,
    context_limit_tokens: f64,
    input_tokens: f64,
    execute_threshold_percentage: f64,
    meta: &ModuleMeta,
) -> Channel1Decision {
    let reset_cycle = meta.channel1_reduce_suppressed
        || reclaimable_tokens < meta.channel1_last_nudge_undropped.max(0);
    let last_nudge = if reset_cycle {
        0
    } else {
        meta.channel1_last_nudge_undropped.max(0)
    };
    let last_level = if reset_cycle {
        None
    } else {
        Channel1Level::parse(&meta.channel1_last_nudge_level)
    };
    let last_level_string = |level: Option<Channel1Level>| {
        level
            .map(|level| level.as_str().to_string())
            .unwrap_or_default()
    };
    let quiet = |next_last_nudge: i64, next_last_level: String| Channel1Decision {
        fire: false,
        level: Channel1Level::Gentle,
        reclaimable_tokens,
        next_last_nudge,
        next_last_level,
    };
    if meta.channel1_reduce_suppressed {
        return quiet(0, String::new());
    }
    if reclaimable_tokens < CHANNEL1_FLOOR_TOKENS {
        return quiet(last_nudge, last_level_string(last_level));
    }
    let pressure = if context_limit_tokens > 0.0 && execute_threshold_percentage > 0.0 {
        ((input_tokens / context_limit_tokens) * 100.0 / execute_threshold_percentage)
            .clamp(0.0, 1.0)
    } else {
        0.0
    };
    if pressure < CHANNEL1_PRESSURE_FLOOR {
        return quiet(last_nudge, last_level_string(last_level));
    }
    let execute_threshold_tokens =
        context_limit_tokens * execute_threshold_percentage.clamp(1.0, 100.0) / 100.0;
    let usable_tokens =
        (execute_threshold_tokens - input_tokens + live_tail_tokens as f64).max(0.0);
    if usable_tokens > 0.0 && (reclaimable_tokens as f64) < usable_tokens * CHANNEL1_USABLE_FRACTION
    {
        return quiet(last_nudge, last_level_string(last_level));
    }
    let severity = if working_window_tokens > 0 {
        (reclaimable_tokens as f64 / working_window_tokens as f64).min(1.0)
    } else {
        1.0
    };
    let level = if severity >= 0.65 {
        Channel1Level::Urgent
    } else if severity >= 0.4 {
        Channel1Level::Firm
    } else {
        Channel1Level::Gentle
    };
    if let Some(last) = last_level {
        if level.rank() <= last.rank() {
            return quiet(last_nudge, last.as_str().to_string());
        }
    } else if reclaimable_tokens < last_nudge + channel1_refire_tokens(working_window_tokens) {
        return quiet(last_nudge, String::new());
    }
    Channel1Decision {
        fire: true,
        level,
        reclaimable_tokens,
        next_last_nudge: reclaimable_tokens,
        next_last_level: level.as_str().to_string(),
    }
}

fn channel1_refire_tokens(working_window_tokens: i64) -> i64 {
    let scaled = (0.05 * working_window_tokens.max(0) as f64).round() as i64;
    CHANNEL1_REFIRE_FLOOR_TOKENS.max(scaled)
}

fn newest_tool_result_for_channel1(
    core: &CoreState,
    meta: &ModuleMeta,
    projection: &FlatProjection,
    existing_blocks: &HashSet<&str>,
) -> Option<String> {
    projection
        .blocks
        .iter()
        .filter(|block| {
            block.kind_tag == "tool_result"
                && taggable_kind(block).is_some()
                && is_tail(block.ordinal, meta.coverage_ordinal)
                && frozen_red_payload(core, block.id()).is_none()
                && !existing_blocks.contains(block.id.as_str())
                && tool_result_can_carry_channel1(&block.wire)
        })
        .max_by_key(|block| (block.ordinal, block.block_index))
        .map(|block| block.id.clone())
}

fn tool_result_can_carry_channel1(block: &CkWireBlock) -> bool {
    match &block.kind {
        ck_wire::CkKind::ToolResult { output, .. } => match &output.kind {
            ck_wire::CkOutputKind::Text { .. } | ck_wire::CkOutputKind::ErrorText { .. } => true,
            ck_wire::CkOutputKind::Content { blocks } => blocks
                .iter()
                .any(|block| matches!(block.kind, ck_wire::ResultBlockKind::Text { .. })),
            ck_wire::CkOutputKind::Json { .. }
            | ck_wire::CkOutputKind::ErrorJson { .. }
            | ck_wire::CkOutputKind::ExecutionDenied { .. } => false,
        },
        _ => false,
    }
}

fn oldest_reclaimable_hint(
    active_tags: &[ActiveTagForNudge],
    working_window_tokens: i64,
) -> Vec<(i64, String)> {
    let mut protected = HashSet::new();
    let mut sum = 0i64;
    for tag in active_tags.iter().rev() {
        if sum >= working_window_tokens.max(0) {
            break;
        }
        protected.insert(tag.tag_number);
        sum += tag.token_count.max(0);
    }
    active_tags
        .iter()
        .filter(|tag| tag.kind == "tool_result" && !protected.contains(&tag.tag_number))
        .take(4)
        .map(|tag| (tag.tag_number, "tool".to_string()))
        .collect()
}

fn build_channel1_reminder(
    level: Channel1Level,
    reclaimable_tokens: i64,
    hint: &[(i64, String)],
) -> String {
    let amount = approx_thousands(reclaimable_tokens);
    let hint_text = format_reclaimable_hint(hint);
    let body = match level {
        Channel1Level::Gentle => format!(
            "You have ~{amount} tokens of tool output you have not reduced. When you are done with earlier outputs, dropping them with ctx_reduce keeps context lean."
        ),
        Channel1Level::Firm => format!(
            "~{amount} tokens of unreduced tool output has built up. At your next natural stopping point, consider dropping what you have already processed with ctx_reduce."
        ),
        Channel1Level::Urgent => format!(
            "~{amount} tokens of unreduced tool output remain, and a large span of this session will be comparted before long. Consider dropping spent outputs with ctx_reduce so the archived span is the part that matters."
        ),
    };
    format!("\n\n<system-reminder>\n{body}{hint_text}\n</system-reminder>")
}

fn approx_thousands(tokens: i64) -> String {
    format!("{}k", (tokens.max(0) as f64 / 1000.0).round() as i64)
}

fn format_reclaimable_hint(hint: &[(i64, String)]) -> String {
    if hint.is_empty() {
        return String::new();
    }
    let rendered = hint
        .iter()
        .map(|(tag, name)| format!("§{tag}§ {name}"))
        .collect::<Vec<_>>()
        .join(" · ");
    format!("\noldest reclaimable: {rendered}.")
}

// --- output splice: [m0, m1] ++ tail(by coverage_ordinal) ---

fn build_output(
    core: &CoreState,
    meta: &ModuleMeta,
    projection: &FlatProjection,
    req: &TransformRequest,
    tag_overlay: Option<&TagOverlayState>,
    synthetic_todo_enabled: bool,
) -> Result<Vec<CkWireMessage>, TransformError> {
    let mut out = Vec::with_capacity(4 + req.messages.len());
    if let Some(u) = core.frozen_units.iter().find(|u| u.key == "m0") {
        out.push(CkWireMessage::synthetic_user_text(u.frozen_payload.clone()));
    }
    if let Some(u) = core.frozen_units.iter().find(|u| u.key == "m1") {
        out.push(CkWireMessage::synthetic_user_text(u.frozen_payload.clone()));
    }

    let blocks_by_mid = projection_blocks_by_mid(projection);

    // A synthetic-todo pair with no message anchor (anchor_mid == None) was composed when
    // the tail was empty (every live message folded under coverage). It is frozen
    // immediately AFTER the m0/m1 head blocks pushed above and BEFORE the tail loop below
    // — emitting it HERE, not after that loop, is what keeps its position byte-stable:
    // later tail growth appends after it, so the [m0, m1, pair] prefix stays identical on
    // every subsequent defer pass. Emitting it after the loop would let the pair float to
    // the end of a growing tail, changing the bytes of a cached prefix on every turn — the
    // exact failure the position-freeze design prevents. (A None anchor also never
    // relocates on a bust: reanchor_kept_synthetic_todo_if_folded early-returns on None, so
    // the pair stays right after m0/m1 for its whole life until a Replace or Clear.)
    if synthetic_todo_enabled
        && meta
            .synthetic_todo
            .as_ref()
            .is_some_and(|pair| pair.anchor_mid.is_none())
    {
        push_synthetic_todo_pair(&mut out, meta);
    }

    // Covered system messages are stored in m0 during the HARD render for the current
    // coverage. The claude-code-anthropic profile epoch makes old sessions do that HARD
    // before this code stops emitting those messages separately, preventing old m0 bytes
    // that lack the block from losing the prompt content.

    let serializer_profile = SerializerProfile::parse(&req.serializer_profile);
    let mut inserted_synthetic_todo = false;
    // Tail messages are strictly after the coverage watermark. Full-array profiles also
    // keep pinned leading system prompts that sit before the first summarized ordinal.
    // The outer loop is the inbound message list, not the reduced-block map, so a live
    // tail message with zero content blocks still passes through instead of disappearing.
    for msg in req.messages.iter().filter(|m| !m.ck.meta.synthetic) {
        let keep_leading_system = serializer_profile
            != Some(SerializerProfile::ClaudeCodeAnthropic)
            && is_uncovered_leading_system(msg, meta);
        if !is_tail(msg.ordinal, meta.coverage_ordinal) && !keep_leading_system {
            continue;
        }
        let rendered = if let Some(blocks) = blocks_by_mid.get(msg.mid.as_str()) {
            let reduced: BTreeMap<usize, &str> = blocks
                .iter()
                .filter_map(|block| {
                    frozen_red_payload(core, block.id()).map(|p| (block.block_index, p))
                })
                .collect();
            let mut rebuilt = msg.ck.clone();
            if !reduced.is_empty() {
                rebuilt.mark_modified();
                for block in blocks {
                    if let Some(payload) = reduced.get(&block.block_index) {
                        // Canonical frozen bytes never carry a tag number. The egress clone
                        // may add the target's live number, while legacy numbered payloads
                        // remain byte-identical because only exact `[dropped]` is overlaid.
                        let display_payload = (*payload == "[dropped]")
                            .then(|| {
                                tag_overlay
                                    .and_then(|overlay| overlay.tag_by_block_id.get(&block.id))
                                    .map(|tag_number| format!("[dropped §{tag_number}§]"))
                            })
                            .flatten();
                        rebuilt.content[block.block_index] = reduced_block(
                            &block.wire,
                            display_payload.as_deref().unwrap_or(payload),
                            block.file_path.as_deref(),
                        );
                    }
                }
            }
            apply_tag_overlay_to_message(&mut rebuilt, msg, blocks, tag_overlay, |block| {
                reduced.contains_key(&block.block_index)
            });
            rebuilt
        } else {
            let mut rebuilt = msg.ck.clone();
            apply_tag_overlay_to_message(&mut rebuilt, msg, &[], tag_overlay, |_| false);
            rebuilt
        };
        out.push(rendered);

        if synthetic_todo_enabled
            && meta
                .synthetic_todo
                .as_ref()
                .and_then(|pair| pair.anchor_mid.as_deref())
                == Some(msg.mid.as_str())
        {
            push_synthetic_todo_pair(&mut out, meta);
            inserted_synthetic_todo = true;
        }
    }

    // A pair anchored to a real message must have been spliced inside the loop; if its
    // anchor is absent from the current tail we fail loud rather than silently relocate
    // (a bust folds the anchor via reanchor_kept_synthetic_todo_if_folded, so reaching
    // here means the anchor vanished on a defer = a revert/drift invariant violation).
    // The None-anchor case was already emitted before the loop, so it is not re-checked.
    if synthetic_todo_enabled {
        if let Some(pair) = &meta.synthetic_todo {
            if pair.anchor_mid.is_some() && !inserted_synthetic_todo {
                let mid = pair.anchor_mid.clone().unwrap_or_default();
                return Err(TransformError::SyntheticTodoAnchorMissing(mid));
            }
        }
    }
    if serializer_profile == Some(SerializerProfile::ClaudeCodeAnthropic) {
        let first_tail = out
            .iter()
            .position(|message| !message.meta.synthetic)
            .unwrap_or(out.len());
        debug_assert!(
            out[..first_tail]
                .iter()
                .all(|message| message.role != "system"),
            "claude-code-anthropic synthetic prefix must not contain system-role messages"
        );
    }
    if let Some(profile) = serializer_profile {
        apply_serializer_residuals(profile, &mut out);
    }
    Ok(out)
}

fn apply_serializer_residuals(profile: SerializerProfile, messages: &mut [CkWireMessage]) -> usize {
    if quirk_residual(profile).strips_reasoning_from_merged_assistants {
        strip_reasoning_from_merged_assistants(messages)
    } else {
        0
    }
}

fn strip_reasoning_from_merged_assistants(messages: &mut [CkWireMessage]) -> usize {
    let mut stripped = 0;
    let mut prev_assistant = false;
    let mut kept_reasoning_in_run = false;

    for message in messages {
        if message.role != "assistant" {
            prev_assistant = false;
            kept_reasoning_in_run = false;
            continue;
        }

        let first_in_run = !prev_assistant;
        if first_in_run {
            kept_reasoning_in_run = false;
        }

        let mut keep_index = None;
        if first_in_run && !kept_reasoning_in_run {
            for (idx, block) in message.content.iter().enumerate() {
                if is_empty_text_block(block) {
                    continue;
                }
                if is_reasoning_block(block) {
                    keep_index = Some(idx);
                }
                break;
            }
        }

        let mut modified = false;
        for (idx, block) in message.content.iter_mut().enumerate() {
            if !is_reasoning_block(block) {
                continue;
            }
            if Some(idx) == keep_index {
                kept_reasoning_in_run = true;
                continue;
            }
            *block = CkWireBlock::bare(ck_wire::CkKind::Text {
                text: String::new(),
            });
            stripped += 1;
            modified = true;
        }
        if modified {
            message.mark_modified();
        }
        prev_assistant = true;
    }

    stripped
}

fn is_reasoning_block(block: &CkWireBlock) -> bool {
    matches!(
        &block.kind,
        ck_wire::CkKind::Reasoning { .. } | ck_wire::CkKind::RedactedReasoning { .. }
    )
}

fn is_empty_text_block(block: &CkWireBlock) -> bool {
    matches!(&block.kind, ck_wire::CkKind::Text { text } if text.is_empty())
}

fn projection_blocks_by_mid(projection: &FlatProjection) -> BTreeMap<&str, Vec<&FlatBlock>> {
    let mut by_mid: BTreeMap<&str, Vec<&FlatBlock>> = BTreeMap::new();
    for block in &projection.blocks {
        by_mid.entry(block.mid.as_str()).or_default().push(block);
    }
    by_mid
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

    use mc_store::{InsertMemoryInput, ModuleUsage, ShadowStateSyncRequest, StoredCompartment};

    #[test]
    fn effective_context_limit_falls_back_below_plausible_floor() {
        assert_eq!(
            effective_context_limit_tokens(&ModuleUsage {
                current_total_input_tokens: 1,
                context_limit_tokens: 500,
            }),
            200_000.0
        );
        assert_eq!(
            effective_context_limit_tokens(&ModuleUsage {
                current_total_input_tokens: 1,
                context_limit_tokens: 0,
            }),
            200_000.0
        );
        assert_eq!(
            effective_context_limit_tokens(&ModuleUsage {
                current_total_input_tokens: 1,
                context_limit_tokens: crate::scheduler::MIN_PLAUSIBLE_CONTEXT_LIMIT,
            }),
            crate::scheduler::MIN_PLAUSIBLE_CONTEXT_LIMIT as f64
        );
    }
    use serde_json::{json, Value};

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

    fn run_active_surface_test<T>(f: impl FnOnce() -> T) -> T {
        f()
    }

    fn text_message(id: &str, text: &str) -> CkWireMessage {
        CkWireMessage::from_parts(
            "user",
            vec![ck_wire::CkWireBlock::bare(ck_wire::CkKind::Text {
                text: text.to_string(),
            })],
            None,
            ck_wire::ProviderExtras::new(),
            ck_wire::HarnessMeta {
                harness_id: Some(id.to_string()),
                ..Default::default()
            },
        )
    }

    fn item(id: &str, ordinal: u64, bytes: &str) -> CkIngressMessage {
        CkIngressMessage {
            mid: id.to_string(),
            ordinal,
            ck: text_message(id, bytes),
        }
    }

    /// Build the omitted-wire and explicit-false forms of one request through the
    /// SAME wire deserialization path, differing only in tool_present presence.
    fn wire_pair_absent_and_explicit_false(
        base: TransformRequest,
    ) -> (TransformRequest, TransformRequest) {
        let mut explicit_value = serde_json::to_value(&base).unwrap();
        explicit_value
            .as_object_mut()
            .unwrap()
            .insert("tool_present".to_string(), serde_json::Value::Bool(false));
        let explicit: TransformRequest = serde_json::from_value(explicit_value).unwrap();
        let mut absent_value = serde_json::to_value(&base).unwrap();
        absent_value.as_object_mut().unwrap().remove("tool_present");
        let absent: TransformRequest = serde_json::from_value(absent_value).unwrap();
        (absent, explicit)
    }

    fn two_block_item(id: &str, ordinal: u64, first: &str, second: &str) -> CkIngressMessage {
        CkIngressMessage {
            mid: id.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "user",
                vec![
                    ck_wire::CkWireBlock::bare(ck_wire::CkKind::Text {
                        text: first.to_string(),
                    }),
                    ck_wire::CkWireBlock::bare(ck_wire::CkKind::Text {
                        text: second.to_string(),
                    }),
                ],
                None,
                ck_wire::ProviderExtras::new(),
                ck_wire::HarnessMeta::default(),
            ),
        }
    }

    fn system_item(id: &str, ordinal: u64, bytes: &str) -> CkIngressMessage {
        CkIngressMessage {
            mid: id.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "system",
                vec![ck_wire::CkWireBlock::bare(ck_wire::CkKind::Text {
                    text: bytes.to_string(),
                })],
                None,
                ck_wire::ProviderExtras::new(),
                ck_wire::HarnessMeta::default(),
            ),
        }
    }

    fn item_with_block_provider_extras(
        id: &str,
        ordinal: u64,
        bytes: &str,
        nonce: &str,
    ) -> CkIngressMessage {
        let mut provider = std::collections::BTreeMap::new();
        provider.insert("cache_control".to_string(), json!({ "nonce": nonce }));
        let mut extras = ck_wire::ProviderExtras::new();
        extras.insert("synthetic".to_string(), provider);
        CkIngressMessage {
            mid: id.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "user",
                vec![ck_wire::CkWireBlock::with_provider_extras(
                    ck_wire::CkKind::Text {
                        text: bytes.to_string(),
                    },
                    extras,
                )],
                None,
                ck_wire::ProviderExtras::new(),
                ck_wire::HarnessMeta {
                    harness_id: Some(id.to_string()),
                    ..Default::default()
                },
            ),
        }
    }

    fn post_submit_strip_block_provider_extras(
        mut messages: Vec<CkIngressMessage>,
    ) -> Vec<CkIngressMessage> {
        for message in &mut messages {
            for block in &mut message.ck.content {
                block.provider_extras.clear();
            }
        }
        messages
    }

    fn flat_id(mid: &str) -> String {
        format!("{mid}#0")
    }

    fn target_id(id: &str) -> String {
        if id.contains('#') {
            id.to_string()
        } else {
            flat_id(id)
        }
    }

    fn req(session: &str, cfg: &str, messages: Vec<CkIngressMessage>) -> TransformRequest {
        TransformRequest {
            kind: "transform".to_string(),
            v: 2,
            serializer_profile: "owned-llmrunner".to_string(),
            session_id: session.to_string(),
            render_config: cfg.to_string(),
            tool_present: false,
            full_array_fingerprint: None,
            messages,
            tail_delta: None,
            usage: None,
            provider_error: None,
            declared_trim: None,
        }
    }

    fn spine() -> Vec<ReductionDecision> {
        Vec::new()
    }

    /// A store compartment covering raw ordinals `start..=end`, ending at message id
    /// `end_id`, rendered at P1 with body `p1`. The m0 baseline is composed from these.
    fn comp(seq: i64, start: i64, end: i64, end_id: &str, p1: &str) -> StoredCompartment {
        StoredCompartment {
            sequence: seq,
            start_message: start,
            end_message: end,
            end_message_id: target_id(end_id),
            title: format!("C{seq}"),
            content: p1.to_string(),
            p1: Some(p1.to_string()),
            importance: 50,
            ..Default::default()
        }
    }

    fn memory_input<'a>(
        project_path: &'a str,
        category: &'a str,
        content: &'a str,
        now_ms: i64,
    ) -> InsertMemoryInput<'a> {
        InsertMemoryInput {
            project_path,
            category,
            content,
            source_session_id: None,
            source_type: Some("tool"),
            importance: Some(70),
            expires_at: None,
            metadata_json: None,
            now_ms,
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
            memory_enabled: true,
            now_ms,
            execute_threshold_percentage: 65.0,
            smart_drops: false,
            cache_ttl: "5m".to_string(),
            model_key: None,
            observed_last_response_at_ms: None,
            guidance_date: Some("Today's date: Thu Jan 01 1970".to_string()),
            injected_reductions: Vec::new(),
        }
    }

    fn smart_pctx<'a>() -> ProducerContext<'a> {
        let mut ctx = pctx("git:proj", "/nonexistent-docs", 0);
        ctx.smart_drops = true;
        ctx
    }

    fn with_usage(
        mut request: TransformRequest,
        current_total_input_tokens: u64,
        context_limit_tokens: u64,
    ) -> TransformRequest {
        let min = crate::scheduler::MIN_PLAUSIBLE_CONTEXT_LIMIT;
        let (input, limit) = if context_limit_tokens >= min {
            (current_total_input_tokens, context_limit_tokens)
        } else if context_limit_tokens > 0 {
            // Shorthand fixtures (e.g. 70/100 for 70% usage): scale to a plausible limit
            // while preserving the implied percentage.
            let pct = current_total_input_tokens as f64 / context_limit_tokens as f64;
            let limit = 100_000u64;
            let input = (pct * limit as f64).round().max(1.0) as u64;
            (input, limit)
        } else {
            (current_total_input_tokens, min)
        };
        request.usage = Some(ModuleUsage {
            current_total_input_tokens: input,
            context_limit_tokens: limit,
        });
        request
    }

    fn todowrite_arc(mid: &str, call_ordinal: u64) -> Vec<CkIngressMessage> {
        vec![
            todowrite_call(mid, call_ordinal, json!([])),
            tool_result(
                &format!("{mid}_result"),
                call_ordinal + 1,
                &format!("call_{mid}"),
                "todo output",
            ),
        ]
    }

    /// Run a transform with a default producer context (project "git:proj", a nonexistent
    /// docs dir, now_ms=0). Most tests don't vary the context.
    fn run(s: &McStore, req: &TransformRequest, d: &[ReductionDecision]) -> TransformResponse {
        let mut ctx = pctx("git:proj", "/nonexistent-docs", 0);
        ctx.injected_reductions = d.to_vec();
        transform(s, req, &ctx).unwrap()
    }

    fn synthetic_text(r: &TransformResponse, index: usize) -> &str {
        ck_wire::text_from_message(
            r.messages()
                .iter()
                .filter(|m| m.meta.synthetic)
                .nth(index)
                .unwrap(),
        )
        .unwrap()
    }

    fn m0_bytes(r: &TransformResponse) -> &str {
        synthetic_text(r, 0)
    }
    fn m1_bytes(r: &TransformResponse) -> &str {
        synthetic_text(r, 1)
    }
    fn tail_ids(r: &TransformResponse) -> Vec<&str> {
        r.messages()
            .iter()
            .filter(|m| !m.meta.synthetic)
            .map(|m| m.meta.harness_id.as_deref().unwrap_or(""))
            .collect()
    }

    fn ingress_from_ck(messages: Vec<CkWireMessage>) -> Vec<CkIngressMessage> {
        messages
            .into_iter()
            .enumerate()
            .map(|(i, ck)| CkIngressMessage {
                mid: format!("m{i}"),
                ordinal: i as u64 + 1,
                ck,
            })
            .collect()
    }

    fn assistant_tool_call(mid: &str, ordinal: u64, call_id: &str) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "assistant",
                vec![ck_wire::CkWireBlock::bare(ck_wire::CkKind::ToolCall {
                    id: call_id.to_string(),
                    name: "read".to_string(),
                    input: json!({ "path": "a.txt" }),
                    provider_executed: false,
                })],
                None,
                ck_wire::ProviderExtras::new(),
                ck_wire::HarnessMeta {
                    harness_id: Some(mid.to_string()),
                    ..Default::default()
                },
            ),
        }
    }

    fn tool_result(mid: &str, ordinal: u64, call_id: &str, text: &str) -> CkIngressMessage {
        tool_result_with_output(
            mid,
            ordinal,
            call_id,
            ck_wire::CkOutputKind::Text {
                text: text.to_string(),
            },
        )
    }

    fn tool_result_with_output(
        mid: &str,
        ordinal: u64,
        call_id: &str,
        output: ck_wire::CkOutputKind,
    ) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "tool",
                vec![ck_wire::CkWireBlock::bare(ck_wire::CkKind::ToolResult {
                    id: call_id.to_string(),
                    tool_name: "read".to_string(),
                    output: ck_wire::CkToolOutput::bare(output),
                    provider_executed: false,
                })],
                None,
                ck_wire::ProviderExtras::new(),
                ck_wire::HarnessMeta {
                    harness_id: Some(mid.to_string()),
                    ..Default::default()
                },
            ),
        }
    }

    fn todowrite_call(mid: &str, ordinal: u64, todos: Value) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "assistant",
                vec![ck_wire::CkWireBlock::bare(ck_wire::CkKind::ToolCall {
                    id: format!("call_{mid}"),
                    name: "todowrite".to_string(),
                    input: json!({ "todos": todos }),
                    provider_executed: false,
                })],
                None,
                ck_wire::ProviderExtras::new(),
                ck_wire::HarnessMeta {
                    harness_id: Some(mid.to_string()),
                    ..Default::default()
                },
            ),
        }
    }

    fn empty_message(mid: &str, ordinal: u64) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "user",
                Vec::new(),
                None,
                ck_wire::ProviderExtras::new(),
                ck_wire::HarnessMeta {
                    harness_id: Some(mid.to_string()),
                    ..Default::default()
                },
            ),
        }
    }

    fn message_index(r: &TransformResponse, harness_id: &str) -> usize {
        r.messages()
            .iter()
            .position(|m| !m.meta.synthetic && m.meta.harness_id.as_deref() == Some(harness_id))
            .unwrap_or_else(|| panic!("message {harness_id} not found"))
    }

    fn synthetic_todo_index(r: &TransformResponse) -> usize {
        r.messages()
            .iter()
            .position(|m| {
                m.meta.synthetic
                    && matches!(
                        m.content.first().map(|block| &block.kind),
                        Some(ck_wire::CkKind::ToolCall { name, .. }) if name == "todowrite"
                    )
            })
            .expect("synthetic todowrite assistant message not found")
    }

    fn synthetic_todo_call_id(r: &TransformResponse) -> String {
        let msg = &r.messages()[synthetic_todo_index(r)];
        match &msg.content[0].kind {
            ck_wire::CkKind::ToolCall { id, .. } => id.clone(),
            other => panic!("expected synthetic todowrite ToolCall, got {other:?}"),
        }
    }

    fn prefix_through_synthetic_todo(r: &TransformResponse) -> Vec<Vec<u8>> {
        let end = synthetic_todo_index(r) + 1;
        r.messages()[..=end]
            .iter()
            .map(|m| serde_json::to_vec(m).unwrap())
            .collect()
    }

    fn synthetic_todo_pair_bytes(r: &TransformResponse) -> (Vec<u8>, Vec<u8>) {
        let i = synthetic_todo_index(r);
        (
            serde_json::to_vec(&r.messages()[i]).unwrap(),
            serde_json::to_vec(&r.messages()[i + 1]).unwrap(),
        )
    }

    /// Cross-repo drift pin for the shared CK wire fixture. Three parties ride
    /// this exact byte shape (llm-runner produces it, this module parses it,
    /// the thalamus gateway produces it), and each repo vendors its own copy — so a
    /// one-sided regeneration would leave every repo locally green while the
    /// wire silently drifts. Each repo pins the fixture's sha256; a regen
    /// fails the pin everywhere until each consumer deliberately re-vendors
    /// and updates its constant. llm-runner owns the canonical fixture and
    /// announces the new sha when it legitimately changes.
    #[test]
    fn ck_wire_golden_bytes_match_cross_repo_pin() {
        use sha2::{Digest, Sha256};
        const GOLDEN_SHA256: &str =
            "e6143e10762f3f1b33a2a2bc32860e8fcd51dece00d7af67e7c2245c309db192";
        let bytes = include_bytes!("../testdata/ck_wire_golden.json");
        let actual = format!("{:x}", Sha256::digest(bytes));
        assert_eq!(
            actual, GOLDEN_SHA256,
            "ck_wire_golden.json changed. If this is a deliberate re-vendor of the \
             canonical fixture, update GOLDEN_SHA256 to match; otherwise restore the \
             vendored bytes."
        );
    }

    #[test]
    fn ck_wire_golden_projects_to_flat_blocks() {
        let ck: Vec<CkWireMessage> =
            serde_json::from_str(include_str!("../testdata/ck_wire_golden.json")).unwrap();
        let projection = project_messages(&ingress_from_ck(ck)).unwrap();
        let actual = serde_json::to_value(&projection.blocks).unwrap();
        if std::env::var_os("MC_REGEN_PROJECTION_GOLDEN").is_some() {
            std::fs::write(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/testdata/ingress-projection-golden.json"
                ),
                serde_json::to_string_pretty(&actual).unwrap(),
            )
            .unwrap();
        }
        let expected: Value =
            serde_json::from_str(include_str!("../testdata/ingress-projection-golden.json"))
                .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn arc_identity_is_session_injective_for_reused_tool_ids() {
        let messages = vec![
            assistant_tool_call("turn1_call", 1, "call_0"),
            tool_result("turn1_result", 2, "call_0", "one"),
            assistant_tool_call("turn2_call", 3, "call_0"),
            tool_result("turn2_result", 4, "call_0", "two"),
        ];
        let projection = project_messages(&messages).unwrap();
        let result_arcs: Vec<_> = projection
            .blocks
            .iter()
            .filter(|b| b.kind_tag == "tool_result")
            .map(|b| b.arc_id.as_deref().unwrap())
            .collect();
        assert_eq!(result_arcs, vec!["turn1_call#0", "turn2_call#0"]);
        assert_ne!(result_arcs[0], result_arcs[1]);
    }

    #[test]
    fn opaque_and_media_project_verbatim_across_passes() {
        // Opaque is a first-class carrier: it must project with verbatim bytes and
        // an "opaque" kind tag rather than rejecting provider-native blocks.
        let opaque = CkIngressMessage {
            mid: "opaque".to_string(),
            ordinal: 1,
            ck: CkWireMessage::from_parts(
                "user",
                vec![ck_wire::CkWireBlock::bare(ck_wire::CkKind::Opaque(
                    ck_wire::OpaqueBlock {
                        source: json!({ "source": "wire", "wire": "test" }),
                        kind: "native".to_string(),
                        raw: json!({ "x": 1 }),
                        arc: None,
                    },
                ))],
                None,
                ck_wire::ProviderExtras::new(),
                ck_wire::HarnessMeta::default(),
            ),
        };
        let projection = project_messages(&[opaque]).unwrap();
        assert_eq!(projection.blocks.len(), 1);
        let block = &projection.blocks[0];
        assert_eq!(block.kind_tag, "opaque");
        // Verbatim source bytes: the serialized block must round-trip the
        // source-tagged struct shape ({"source":"wire","wire":...}) untouched.
        assert!(block
            .bytes
            .contains("\"source\":{\"source\":\"wire\",\"wire\":\"test\"}"));

        let media = CkIngressMessage {
            mid: "media".to_string(),
            ordinal: 1,
            ck: CkWireMessage::from_parts(
                "user",
                vec![ck_wire::CkWireBlock::bare(ck_wire::CkKind::Media(
                    ck_wire::MediaBlock {
                        kind: ck_wire::MediaKind::Image,
                        media_type: "image/png".to_string(),
                        filename: None,
                        source: json!({ "source": "url", "url": "file://x" }),
                    },
                ))],
                None,
                ck_wire::ProviderExtras::new(),
                ck_wire::HarnessMeta::default(),
            ),
        };
        let media_wire = serde_json::to_value(&media.ck).unwrap();
        let media_projection = project_messages(std::slice::from_ref(&media)).unwrap();
        assert_eq!(media_projection.blocks[0].kind_tag, "media");
        assert!(media_projection.blocks[0].bytes.contains("file://x"));

        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let first = run(
            &store,
            &req("media-pass", "cfg", vec![media.clone()]),
            &spine(),
        );
        let second = run(&store, &req("media-pass", "cfg", vec![media]), &spine());
        for response in [&first, &second] {
            let replayed = response
                .messages()
                .iter()
                .find(|message| !message.meta.synthetic)
                .expect("live media message must pass through");
            assert_eq!(serde_json::to_value(replayed).unwrap(), media_wire);
        }
    }

    #[test]
    fn enforcement_rejects_drift_duplicates_and_vanished_reduction_targets() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        run(&s, &req("ses", "cfg0", vec![item("a", 1, "one")]), &spine());
        let drift = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "two")]),
            &pctx("git:proj", "/nonexistent-docs", 0),
        )
        .unwrap_err();
        assert!(matches!(drift, TransformError::IdentityDrift(mid) if mid == "a"));

        let dup = transform(
            &s,
            &req(
                "dup",
                "cfg0",
                vec![item("same", 1, "x"), item("same", 2, "y")],
            ),
            &pctx("git:proj", "/nonexistent-docs", 0),
        )
        .unwrap_err();
        assert!(matches!(dup, TransformError::DuplicateBlockId(id) if id == "same#0"));

        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let one_block = vec![item("live", 1, "only block")];
        let projection = project_messages(&one_block).unwrap();
        let core = CoreState {
            frozen_units: vec![
                synth_region("m0", "BASE".into()),
                synth_region("m1", M1_PLACEHOLDER.into()),
                red_unit("live#1", "drop", "[dropped 1]"),
            ],
            ..Default::default()
        };
        let meta = ModuleMeta {
            initialized: true,
            block_identity_by_mid: projection.identity_by_mid,
            ..Default::default()
        };
        s.commit("vanish", None, &core, &meta).unwrap();
        let vanished = transform(
            &s,
            &req("vanish", "cfg0", one_block),
            &pctx("git:proj", "/nonexistent-docs", 0),
        )
        .unwrap_err();
        assert!(matches!(
            vanished,
            TransformError::FrozenRedTargetVanish(id) if id == "live#1"
        ));
    }

    #[test]
    fn usage_non_zero_wins_and_absent_or_zero_falls_back_to_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let mut first = req("usage", "cfg0", vec![item("a", 1, "x")]);
        first.usage = Some(ModuleUsage {
            current_total_input_tokens: 100,
            context_limit_tokens: 1000,
        });
        run(&s, &first, &spine());
        assert_eq!(
            s.load("usage").unwrap().meta.last_usage,
            Some(ModuleUsage {
                current_total_input_tokens: 100,
                context_limit_tokens: 1000,
            })
        );

        let mut lower = req("usage", "cfg0", vec![item("a", 1, "x")]);
        lower.usage = Some(ModuleUsage {
            current_total_input_tokens: 50,
            context_limit_tokens: 1000,
        });
        run(&s, &lower, &spine());
        assert_eq!(
            s.load("usage")
                .unwrap()
                .meta
                .last_usage
                .unwrap()
                .current_total_input_tokens,
            50,
            "a non-zero decrease is accepted instead of max-merged"
        );

        let absent = req("usage", "cfg0", vec![item("a", 1, "x")]);
        run(&s, &absent, &spine());
        assert_eq!(
            s.load("usage")
                .unwrap()
                .meta
                .last_usage
                .unwrap()
                .current_total_input_tokens,
            50,
            "absent usage keeps the persisted value for restart continuity"
        );

        let mut zero = req("usage", "cfg0", vec![item("a", 1, "x")]);
        zero.usage = Some(ModuleUsage {
            current_total_input_tokens: 0,
            context_limit_tokens: 0,
        });
        run(&s, &zero, &spine());
        assert_eq!(
            s.load("usage")
                .unwrap()
                .meta
                .last_usage
                .unwrap()
                .current_total_input_tokens,
            50,
            "all-zero usage also falls back to persisted"
        );
    }

    #[test]
    fn response_shape_is_bare_ck_messages_and_reduced_tool_result_stays_paired() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let messages = vec![
            assistant_tool_call("call", 1, "call_0"),
            tool_result("result", 2, "call_0", "large output"),
        ];
        let r = run(
            &s,
            &req("shape", "cfg0", messages),
            &with_reductions(vec![reduce("result#0", "drop", "[dropped 12]")]),
        );
        let value = serde_json::to_value(&r).unwrap();
        assert!(value.get("coverage_ordinal").is_none());
        let ck_messages = value["ck_messages"].as_array().unwrap();
        assert!(ck_messages.iter().all(|m| m.get("mid").is_none()));
        assert!(ck_messages.iter().all(|m| m.get("ordinal").is_none()));
        assert_eq!(ck_messages[0]["role"], "user");
        assert_eq!(ck_messages[0]["meta"]["synthetic"], true);
        assert_eq!(ck_messages[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(ck_messages[1]["meta"]["synthetic"], true);
        let reduced_tool = ck_messages.last().unwrap();
        assert_eq!(reduced_tool["content"][0]["kind"]["type"], "tool_result");
        assert_eq!(
            reduced_tool["content"][0]["kind"]["output"]["kind"]["text"],
            "[dropped 12]"
        );
    }

    #[test]
    fn unreduced_golden_messages_are_passed_through_by_identity() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let ck: Vec<CkWireMessage> =
            serde_json::from_str(include_str!("../testdata/ck_wire_golden.json")).unwrap();
        let inbound = ingress_from_ck(ck);
        let r = run(&s, &req("identity", "cfg0", inbound.clone()), &spine());
        let tail: Vec<_> = r.messages().iter().filter(|m| !m.meta.synthetic).collect();
        assert_eq!(tail.len(), inbound.len());
        for (input, output) in inbound.iter().zip(tail) {
            assert_eq!(
                serde_json::to_vec(&input.ck).unwrap(),
                serde_json::to_vec(output).unwrap(),
                "unreduced mid {} must be returned by identity",
                input.mid
            );
        }
    }

    #[test]
    fn pure_passthrough_defer_round_trips_tail_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let ck: Vec<CkWireMessage> =
            serde_json::from_str(include_str!("../testdata/ck_wire_golden.json")).unwrap();
        let inbound = ingress_from_ck(ck);
        s.replace_compartments("roundtrip", &[comp(1, 1, 1, "m0", "SUMMARY")])
            .unwrap();
        run(&s, &req("roundtrip", "cfg0", inbound.clone()), &spine());
        let r = run(&s, &req("roundtrip", "cfg0", inbound.clone()), &spine());
        assert_eq!(r.action, "SOFT+");
        assert!(!r.committed);
        // This fixture starts with a covered system message. It should move out of the
        // live message list and replay from frozen m0 while the remaining live messages
        // keep their original bytes.
        assert!(r.messages()[0].meta.synthetic);
        assert_eq!(
            covered_system_entries(m0_bytes(&r)),
            vec![system_content_for_m0(&inbound[0].ck)]
        );
        let tail: Vec<_> = r.messages().iter().filter(|m| !m.meta.synthetic).collect();
        assert_eq!(tail.len(), inbound.len() - 1);
        for (input, output) in inbound.iter().skip(1).zip(tail) {
            assert_eq!(
                serde_json::to_vec(&input.ck).unwrap(),
                serde_json::to_vec(output).unwrap()
            );
        }
    }

    #[test]
    fn agent_drop_ids_freeze_add_only_through_flat_ids() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let request = active_cc_req("agent-drop", "cfg0", vec![item("a", 1, "drop me")]);
        s.append_pending_agent_drops("agent-drop", &["a#0".to_string()], 1)
            .unwrap();
        let r = run(&s, &request, &spine());
        assert_eq!(tail_bytes(&r, "a"), "[dropped]");
        assert!(s
            .load("agent-drop")
            .unwrap()
            .core
            .frozen_units
            .iter()
            .any(|unit| unit.key == "red:a#0"));
        assert!(s.load_pending_agent_drops("agent-drop").unwrap().is_empty());
        let again = run(&s, &request, &spine());
        assert_eq!(again.action, "SOFT+");
        assert_eq!(tail_bytes(&again, "a"), "[dropped]");
        s.append_pending_agent_drops("agent-drop", &["a#0".to_string()], 2)
            .unwrap();
        let mut hard_request = request.clone();
        hard_request.render_config = "cfg1".to_string();
        let hard = run(&s, &hard_request, &spine());
        assert_eq!(hard.action, "HARD");
        assert_eq!(tail_bytes(&hard, "a"), "[dropped]");
        assert!(s.load_pending_agent_drops("agent-drop").unwrap().is_empty());
    }

    #[test]
    fn producer_gate_runs_on_execute_force_and_hard_advisory_never_plain_defer() {
        let ctx = smart_pctx();

        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);
        let mut messages = vec![item("a", 1, "raw")];
        messages.extend(todowrite_arc("old", 2));
        messages.extend(todowrite_arc("new", 4));
        let defer = transform(
            &s,
            &with_usage(req("ses", "cfg0", messages.clone()), 10, 100),
            &ctx,
        )
        .unwrap();
        assert_eq!(defer.action, "SOFT+");
        assert!(s
            .load("ses")
            .unwrap()
            .core
            .frozen_units
            .iter()
            .all(|unit| !unit.key.starts_with("red:old")));

        let execute = transform(
            &s,
            &with_usage(req("ses", "cfg0", messages.clone()), 70, 100),
            &ctx,
        )
        .unwrap();
        assert_eq!(execute.action, "SOFT");
        assert!(s
            .load("ses")
            .unwrap()
            .core
            .frozen_units
            .iter()
            .any(|unit| unit.key == "red:old#0"));

        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);
        let huge = "x".repeat(50_000);
        let force_messages = vec![
            item("a", 1, "raw"),
            assistant_tool_call("force_old", 2, "force_old_call"),
            tool_result("force_old_result", 3, "force_old_call", &huge),
            assistant_tool_call("force_new", 4, "force_new_call"),
            tool_result("force_new_result", 5, "force_new_call", &huge),
        ];
        let force = transform(
            &s,
            &with_usage(req("ses", "cfg0", force_messages), 90_000, 100_000),
            &ctx,
        )
        .unwrap();
        assert_eq!(force.action, "SOFT");
        assert!(s
            .load("ses")
            .unwrap()
            .core
            .frozen_units
            .iter()
            .any(|unit| unit.key == "red:force_old#0"));

        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);
        let mut hard_messages = vec![item("a", 1, "raw")];
        hard_messages.extend(todowrite_arc("hard_old", 2));
        hard_messages.extend(todowrite_arc("hard_new", 4));
        let hard = transform(
            &s,
            &with_usage(req("ses", "cfg1", hard_messages), 10, 100),
            &ctx,
        )
        .unwrap();
        assert_eq!(hard.action, "HARD");
        assert!(s
            .load("ses")
            .unwrap()
            .core
            .frozen_units
            .iter()
            .any(|unit| unit.key == "red:hard_old#0"));
    }

    #[test]
    fn coverage_filtered_pool_never_selects_covered_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let ctx = smart_pctx();
        let mut messages = todowrite_arc("a", 1);
        s.replace_compartments("ses", &[comp(1, 1, 2, "a_result", "SUMMARY")])
            .unwrap();
        let boot = transform(&s, &req("ses", "cfg0", messages.clone()), &ctx).unwrap();
        assert_eq!(boot.action, "HARD");
        messages.extend(todowrite_arc("tail_old", 3));
        messages.extend(todowrite_arc("tail_new", 5));
        let response =
            transform(&s, &with_usage(req("ses", "cfg0", messages), 70, 100), &ctx).unwrap();
        assert_eq!(response.action, "SOFT");
        let loaded = s.load("ses").unwrap();
        assert!(loaded
            .core
            .frozen_units
            .iter()
            .all(|unit| unit.key != "red:a#0"));
        assert!(loaded
            .core
            .frozen_units
            .iter()
            .any(|unit| unit.key == "red:tail_old#0"));
    }

    #[test]
    fn provider_executed_open_arc_does_not_defer_execute_selection() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);
        let mut ctx = smart_pctx();
        ctx.observed_last_response_at_ms = Some(1);
        let mut messages = vec![item("a", 1, "raw")];
        messages.extend(todowrite_arc("old", 2));
        messages.extend(todowrite_arc("new", 4));
        messages.push(CkIngressMessage {
            mid: "server_tool".to_string(),
            ordinal: 6,
            ck: CkWireMessage::from_parts(
                "assistant",
                vec![ck_wire::CkWireBlock::bare(ck_wire::CkKind::ToolCall {
                    id: "server_call".to_string(),
                    name: "web_search".to_string(),
                    input: json!({}),
                    provider_executed: true,
                })],
                None,
                ck_wire::ProviderExtras::new(),
                ck_wire::HarnessMeta {
                    harness_id: Some("server_tool".to_string()),
                    ..Default::default()
                },
            ),
        });
        let response =
            transform(&s, &with_usage(req("ses", "cfg0", messages), 70, 100), &ctx).unwrap();
        assert_eq!(response.action, "SOFT");
        assert!(s
            .load("ses")
            .unwrap()
            .core
            .frozen_units
            .iter()
            .any(|unit| unit.key == "red:old#0"));
    }

    #[test]
    fn ttl_hard_requires_in_process_observation_not_durable_anchor_alone() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);
        let mut loaded = s.load("ses").unwrap();
        loaded.meta.last_committed_pass_at_ms = 1;
        s.commit("ses", loaded.row_version, &loaded.core, &loaded.meta)
            .unwrap();

        let mut ctx = pctx("git:proj", "/nonexistent-docs", 10 * 60 * 1000);
        ctx.cache_ttl = "5m".to_string();
        ctx.observed_last_response_at_ms = None;
        let no_observation =
            transform(&s, &req("ses", "cfg0", vec![item("a", 1, "raw")]), &ctx).unwrap();
        assert_eq!(no_observation.action, "SOFT+");
        assert!(!no_observation.committed);

        ctx.observed_last_response_at_ms = Some(1);
        let observed = transform(&s, &req("ses", "cfg0", vec![item("a", 1, "raw")]), &ctx).unwrap();
        assert_eq!(observed.action, "HARD");
    }

    #[test]
    fn reconcile_pending_disables_two_pass_selector() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);
        let loaded = s.load("ses").unwrap();
        let mut core = loaded.core.clone();
        let mut meta = loaded.meta.clone();
        core.boundary_id = "missing#0".to_string();
        core.reconcile_pending = true;
        meta.last_execute_ordinal = 99;
        s.commit("ses", loaded.row_version, &core, &meta).unwrap();

        let messages = vec![
            item("a", 1, "raw"),
            assistant_tool_call("old", 2, "old_call"),
            tool_result("old_result", 3, "old_call", "old output"),
        ];
        let response = transform(
            &s,
            &with_usage(req("ses", "cfg0", messages), 70, 100),
            &pctx("git:proj", "/nonexistent-docs", 0),
        )
        .unwrap();
        assert_eq!(response.action, "HARD");
        assert!(s
            .load("ses")
            .unwrap()
            .core
            .frozen_units
            .iter()
            .all(|unit| unit.key != "red:old#0"));
    }

    #[test]
    fn execute_with_zero_delta_is_defer_shaped_and_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "a", "SUMMARY")])
            .unwrap();
        let boot_req = with_usage(req("ses", "cfg0", vec![item("a", 1, "raw")]), 90, 100);
        let boot = run(&s, &boot_req, &spine());
        assert_eq!(boot.action, "HARD");
        let before = serde_json::to_vec(&boot.ck_messages).unwrap();
        let mut ctx = pctx("git:proj", "/nonexistent-docs", 0);
        ctx.observed_last_response_at_ms = Some(0);
        let execute = transform(&s, &boot_req, &ctx).unwrap();
        // A zero-drop execute is byte-identical (no bust) but MAY commit metadata: the
        // two-pass watermark stamps this tail as the next execute's candidate set.
        assert_eq!(execute.action, "SOFT+");
        assert_eq!(serde_json::to_vec(&execute.ck_messages).unwrap(), before);
        let meta = s.load("ses").unwrap().meta;
        assert_eq!(
            meta.last_execute_ordinal, 1,
            "zero-drop execute still advances the two-pass watermark"
        );
        // And the pass after it, with an unchanged tail, is a true no-write defer.
        let again = transform(&s, &boot_req, &ctx).unwrap();
        assert!(!again.committed);
        assert_eq!(serde_json::to_vec(&again.ck_messages).unwrap(), before);
    }

    #[test]
    fn zero_drop_execute_watermark_ages_arcs_into_next_execute() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // Two-pass aging requires a FOLDED session: a new reduction can only freeze on
        // a SOFT pass, and the classifier admits a SOFT only when a boundary is present.
        // Seed one compartment covering ordinal 1 and fold it, then drive executes over
        // the post-coverage tail.
        s.replace_compartments("ses", &[comp(1, 1, 1, "a", "SUMMARY")])
            .unwrap();
        let msgs = vec![
            item("a", 1, "covered head"),
            assistant_tool_call("m1", 2, "call_age"),
            tool_result("m2", 3, "call_age", "big tool output payload"),
            item("m9", 9, "newest user text"),
        ];
        let boot = run(&s, &req("ses", "cfg0", msgs.clone()), &spine());
        assert_eq!(boot.action, "HARD");
        // Execute pass 1 over the completed tool arc: nothing to drop yet (the arc is
        // newer than the 0 watermark), but the watermark must stamp its ordinal.
        // 70% usage: above the execute threshold (65) but below Force85, so the
        // scheduler classes the pass Execute and the two-pass selector runs.
        let exec_req = with_usage(req("ses", "cfg0", msgs.clone()), 70, 100);
        let mut ctx = pctx("git:proj", "/nonexistent-docs", 0);
        ctx.observed_last_response_at_ms = Some(0);
        let _ = transform(&s, &exec_req, &ctx).unwrap();
        let after_first = s.load("ses").unwrap();
        assert!(
            after_first
                .core
                .frozen_units
                .iter()
                .all(|unit| !unit.key.starts_with("red:m1#") && !unit.key.starts_with("red:m2#")),
            "first execute has no candidates old enough to drop"
        );
        assert!(
            after_first.meta.last_execute_ordinal >= 9,
            "watermark stamped from the execute tail"
        );
        // Execute pass 2: the same arc is now at-or-below the watermark → age-drops.
        let _ = transform(&s, &exec_req, &ctx).unwrap();
        let after_second = s.load("ses").unwrap();
        assert!(
            after_second
                .core
                .frozen_units
                .iter()
                .any(|unit| unit.key.starts_with("red:m1#") || unit.key.starts_with("red:m2#")),
            "completed arc aged in by the prior execute's watermark must freeze a drop"
        );
    }

    #[test]
    fn pure_defer_with_scheduler_fields_present_keeps_row_version_stable() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        bootstrap_covering_a(&s);
        let mut loaded = s.load("ses").unwrap();
        loaded.meta.deferred_execute_state = None;
        loaded.meta.emergency_drain_active = false;
        loaded.meta.emergency_drain_entered_at_ms = 0;
        loaded.meta.last_execute_ordinal = 2;
        loaded.meta.has_prior_emergency_drop = true;
        loaded.meta.last_emergency_input_sample = 50.0;
        s.commit("ses", loaded.row_version, &loaded.core, &loaded.meta)
            .unwrap();
        let row_before = s.load("ses").unwrap().row_version.unwrap();
        let response = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 1, "raw")]),
            &pctx("git:proj", "/nonexistent-docs", 0),
        )
        .unwrap();
        assert_eq!(response.action, "SOFT+");
        assert!(!response.committed);
        assert_eq!(s.load("ses").unwrap().row_version.unwrap(), row_before);
    }

    #[test]
    fn transform_request_parses_full_flat_wire_envelope() {
        let value = json!({
            "kind": "transform",
            "v": 2,
            "serializer_profile": "owned-llmrunner",
            "session_id": "ses",
            "render_config": "cfg",
            "full_array_fingerprint": "fp-full-array",
            "messages": [{ "mid": "m", "ordinal": 7, "ck": text_message("m", "hello") }],
            "usage": { "current_total_input_tokens": 1, "context_limit_tokens": 2 },
            "provider_error": "prompt is too long"
        });
        let parsed: TransformRequest = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.kind, "transform");
        assert_eq!(parsed.v, 2);
        assert_eq!(parsed.serializer_profile, "owned-llmrunner");
        assert_eq!(
            parsed.full_array_fingerprint.as_deref(),
            Some("fp-full-array")
        );
        assert_eq!(parsed.messages[0].mid, "m");
        assert_eq!(parsed.usage.unwrap().context_limit_tokens, 2);
        assert_eq!(parsed.provider_error.as_deref(), Some("prompt is too long"));
    }

    #[test]
    fn transform_request_legacy_items_shim_parses_with_v2_profile() {
        let value = json!({
            "kind": "transform",
            "v": 2,
            "serializer_profile": "owned-llmrunner",
            "session_id": "ses",
            "render_config": "cfg",
            "items": [{ "id": "legacy", "ordinal": 3, "bytes": "hello" }]
        });
        let parsed: TransformRequest = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].mid, "legacy");
        assert_eq!(parsed.messages[0].ordinal, 3);
        assert_eq!(
            ck_wire::text_from_message(&parsed.messages[0].ck),
            Some("hello")
        );
        assert_eq!(parsed.serializer_profile, "owned-llmrunner");
    }

    #[test]
    fn v2_defer_replays_ck_messages_byte_identically_and_echoes_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let mut request = req("v2-defer", "cfg0", vec![item("a", 1, "raw")]);
        request.full_array_fingerprint = Some("fp-v2-defer".to_string());

        let first = run(&s, &request, &spine());
        let second = run(&s, &request, &spine());

        assert_eq!(first.status, TransformStatus::Ok);
        assert_eq!(first.served_from, ServedFrom::Transform);
        assert_eq!(first.full_array_fingerprint.as_deref(), Some("fp-v2-defer"));
        assert_eq!(second.action, "SOFT+");
        assert_eq!(
            second.full_array_fingerprint.as_deref(),
            Some("fp-v2-defer")
        );
        assert_eq!(
            serde_json::to_vec(&first.ck_messages).unwrap(),
            serde_json::to_vec(&second.ck_messages).unwrap(),
            "defer replay must keep the CK array byte-identical"
        );
    }

    #[test]
    fn reasoning_strip_residual_is_profile_gated_by_merge_coverage() {
        fn assistant(mid: &str, reasoning: &str, text: &str) -> CkWireMessage {
            CkWireMessage::from_parts(
                "assistant",
                vec![
                    ck_wire::CkWireBlock::bare(ck_wire::CkKind::Reasoning {
                        text: reasoning.to_string(),
                        signature: Some(format!("sig-{mid}")),
                    }),
                    ck_wire::CkWireBlock::bare(ck_wire::CkKind::Text {
                        text: text.to_string(),
                    }),
                ],
                None,
                ck_wire::ProviderExtras::new(),
                ck_wire::HarnessMeta {
                    harness_id: Some(mid.to_string()),
                    ..Default::default()
                },
            )
        }

        let base = vec![
            assistant("a1", "keep-first", "answer one"),
            assistant("a2", "strip-second", "answer two"),
        ];
        for profile in [
            SerializerProfile::OwnedLlmRunner,
            SerializerProfile::Pi,
            SerializerProfile::ClaudeCodeAnthropic,
        ] {
            let mut messages = base.clone();
            assert_eq!(apply_serializer_residuals(profile, &mut messages), 0);
            assert!(matches!(
                &messages[1].content[0].kind,
                ck_wire::CkKind::Reasoning { .. }
            ));
        }

        assert!(
            crate::healing::coverage(SerializerProfile::OpencodeAiSdk)
                .merges_consecutive_assistants
        );
        let mut messages = base;
        assert_eq!(
            apply_serializer_residuals(SerializerProfile::OpencodeAiSdk, &mut messages),
            1
        );
        assert!(matches!(
            &messages[0].content[0].kind,
            ck_wire::CkKind::Reasoning { .. }
        ));
        assert!(matches!(
            &messages[1].content[0].kind,
            ck_wire::CkKind::Text { text } if text.is_empty()
        ));
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
    fn empty_store_bootstrap_then_defers_stably_without_hard_oscillation() {
        // A session with no compartments keeps boundary_id = "" for its whole
        // pre-first-compartment life. That empty id is the "no boundary ever minted"
        // sentinel, NOT a "boundary reverted away" signal, so repeated identical passes
        // after the bootstrap HARD must stay pure defers — never oscillate back into a
        // HARD by treating the vacuous boundary as reconcile-pending. (The bytes stay
        // identical either way, so this guards telemetry honesty + write churn, not a
        // prefix-cache bust.)
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let items = vec![item("a", 1, "<h>HELLO</h>"), item("b", 2, "world")];
        let first = run(&s, &req("ses", "cfg0", items.clone()), &spine());
        assert_eq!(first.action, "HARD", "first pass is the bootstrap HARD");
        let m0 = m0_bytes(&first).to_string();
        let m1 = m1_bytes(&first).to_string();

        for _ in 0..4 {
            let r = run(&s, &req("ses", "cfg0", items.clone()), &spine());
            assert_eq!(
                r.action, "SOFT+",
                "an unseeded-store defer must not oscillate back into a HARD"
            );
            assert_eq!(
                m0_bytes(&r),
                m0,
                "m0 must stay byte-identical across defers"
            );
            assert_eq!(
                m1_bytes(&r),
                m1,
                "m1 must stay byte-identical across defers"
            );
            assert_eq!(tail_ids(&r), vec!["a", "b"]);
        }
    }

    #[test]
    fn first_compartment_published_after_empty_bootstrap_hard_folds_and_mints_boundary() {
        // The production historian arc: a fresh session bootstraps EMPTY (boundary_id "" —
        // never minted), runs turns, THEN the historian publishes the session's FIRST
        // compartment mid-session. That publish cannot ride m1 as a SOFT delta (a SOFT delta
        // needs the boundary present so the compartment can splice onto it, and none exists
        // yet), so without the first-fold HARD trigger it would strand on defer forever. It
        // must instead HARD-fold and MINT the first boundary.
        //
        // The first compartment is at SEQUENCE 0 on purpose: max_compartment_seq COALESCEs a
        // missing MAX to 0 and folded_compartment_seq defaults to 0, so a seq-comparison
        // trigger (max > folded) reads 0 > 0 = false and silently misses exactly this case.
        // The presence-based guard (empty boundary + a compartment exists) catches it.
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());

        // Pass 1: empty store, two live turns → bootstrap HARD, empty boundary, all tail.
        let live1 = vec![item("a", 1, "<h>first</h>"), item("t2", 2, "turn two")];
        let boot = run(&s, &req("ses", "cfg0", live1.clone()), &spine());
        assert_eq!(boot.action, "HARD", "bootstrap HARD");
        assert_eq!(boot.boundary_id, "", "boundary never minted yet");
        assert_eq!(tail_ids(&boot), vec!["a", "t2"]);

        // A defer before publish stays a pure defer (the empty-boundary no-oscillation path).
        let pre = run(&s, &req("ses", "cfg0", live1.clone()), &spine());
        assert_eq!(pre.action, "SOFT+", "no compartment yet → pure defer");

        // The historian publishes the FIRST compartment at SEQUENCE 0, covering ordinal 1
        // (raw message "a"). Same live array — "a" is still the raw covered message.
        s.replace_compartments("ses", &[comp(0, 1, 1, "a", "S0-FIRST")])
            .unwrap();
        let fold = run(&s, &req("ses", "cfg0", live1.clone()), &spine());
        assert_eq!(
            fold.action, "HARD",
            "first compartment after an empty bootstrap must HARD-fold, not strand on defer"
        );
        assert_eq!(
            fold.boundary_id, "a#0",
            "the fold MINTED the first boundary"
        );
        assert!(
            m0_bytes(&fold).contains("S0-FIRST"),
            "m0 now carries the folded summary: {}",
            m0_bytes(&fold)
        );
        assert_eq!(
            tail_ids(&fold),
            vec!["t2"],
            "covered ordinal 1 trimmed from tail"
        );

        // ONE-SHOT: with the boundary now minted, a defer stays a pure defer (NOT a repeated
        // HARD) — the guard is self-limiting.
        let defer = run(&s, &req("ses", "cfg0", live1), &spine());
        assert_eq!(
            defer.action, "SOFT+",
            "post-fold the boundary is present → defer, never a repeated first-fold HARD"
        );
        assert!(!defer.committed, "a settled defer does not write");

        // ONE-SHOT continued: a SECOND compartment publishes → it RIDES m1 as a SOFT delta
        // (valid now that the boundary exists to splice onto), NOT another first-fold HARD.
        s.replace_compartments(
            "ses",
            &[
                comp(0, 1, 1, "a", "S0-FIRST"),
                comp(1, 2, 2, "t2", "S1-SECOND"),
            ],
        )
        .unwrap();
        let second = run(
            &s,
            &req(
                "ses",
                "cfg0",
                vec![
                    item("a", 1, "<h>first</h>"),
                    item("t2", 2, "turn two"),
                    item("t3", 3, "turn three"),
                ],
            ),
            &spine(),
        );
        assert_eq!(
            second.action, "SOFT",
            "a subsequent publish rides m1 SOFT — the first-fold HARD fires exactly once"
        );
        assert_eq!(second.boundary_id, "t2#0", "the SOFT advanced the anchor");
        assert!(
            m1_bytes(&second).contains("S1-SECOND"),
            "{}",
            m1_bytes(&second)
        );
    }

    #[test]
    fn fold_minting_wrong_vocabulary_anchor_fails_loud_instead_of_looping() {
        // A compartment whose end_message_id is a BARE message id ("m1") instead of the
        // flat block id ("m1#0"). Presence checks live flat block ids, so a fold that
        // mints the bare id produces an anchor that can NEVER be present: the next pass
        // reads boundary-absent, sets reconcile, HARDs, re-mints the same bare id — an
        // unbounded phantom-HARD loop (each HARD byte-identical, so it is invisible to
        // the provider cache but burns a version bump + full recompose every pass). The
        // guard must fail the MINTING pass loudly instead.
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments(
            "ses",
            &[StoredCompartment {
                sequence: 0,
                start_message: 1,
                end_message: 1,
                end_message_id: "m1".to_string(), // bare mid — wrong vocabulary
                title: "C0".to_string(),
                content: "S".to_string(),
                p1: Some("S".to_string()),
                importance: 50,
                ..Default::default()
            }],
        )
        .unwrap();
        let live = vec![item("m1", 1, "raw"), item("t2", 2, "tail")];
        let ctx = pctx("git:proj", "/nonexistent-docs", 0);
        let err = transform(&s, &req("ses", "cfg0", live.clone()), &ctx);
        match err {
            Err(TransformError::BoundaryNotPresent(_)) => {}
            other => panic!("expected BoundaryNotPresent, got {other:?}"),
        }
        // Nothing committed → the error stays visible on retry, never a silent loop.
        let retry = transform(&s, &req("ses", "cfg0", live), &ctx);
        assert!(matches!(retry, Err(TransformError::BoundaryNotPresent(_))));
    }

    #[test]
    fn fold_minting_empty_anchor_with_coverage_fails_loud() {
        // An empty end_message_id with real coverage would mint boundary_id="" — the
        // reserved no-boundary sentinel — while compartments exist. The first-fold
        // trigger (empty boundary + compartments present) would then re-fire a HARD on
        // every pass forever. The guard catches the empty mint at the source.
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments(
            "ses",
            &[StoredCompartment {
                sequence: 0,
                start_message: 1,
                end_message: 1,
                end_message_id: String::new(), // empty — never presentable
                title: "C0".to_string(),
                content: "S".to_string(),
                p1: Some("S".to_string()),
                importance: 50,
                ..Default::default()
            }],
        )
        .unwrap();
        let live = vec![item("m1", 1, "raw")];
        let ctx = pctx("git:proj", "/nonexistent-docs", 0);
        let err = transform(&s, &req("ses", "cfg0", live), &ctx);
        assert!(matches!(err, Err(TransformError::BoundaryNotPresent(_))));
    }

    #[test]
    fn coverage_extending_soft_minting_absent_anchor_fails_loud() {
        // Same invariant on the OTHER mint site: a second compartment publishing with a
        // wrong-vocabulary end_message_id rides a coverage-extending SOFT — the advanced
        // anchor must exist in the live array or the session decays into the same
        // reconcile-HARD loop one pass later.
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // Healthy first fold (flat vocabulary).
        s.replace_compartments("ses", &[comp(0, 1, 1, "a", "S0")])
            .unwrap();
        let live = vec![item("a", 1, "raw"), item("t2", 2, "turn two")];
        let boot = run(&s, &req("ses", "cfg0", live.clone()), &spine());
        assert_eq!(boot.action, "HARD");
        assert_eq!(boot.boundary_id, "a#0");

        // Second compartment publishes with a BARE end_message_id → the SOFT advance
        // must fail loud, not mint an unpresentable anchor.
        s.replace_compartments(
            "ses",
            &[
                comp(0, 1, 1, "a", "S0"),
                StoredCompartment {
                    sequence: 1,
                    start_message: 2,
                    end_message: 2,
                    end_message_id: "t2".to_string(), // bare mid — wrong vocabulary
                    title: "C1".to_string(),
                    content: "S1".to_string(),
                    p1: Some("S1".to_string()),
                    importance: 50,
                    ..Default::default()
                },
            ],
        )
        .unwrap();
        let ctx = pctx("git:proj", "/nonexistent-docs", 0);
        let err = transform(&s, &req("ses", "cfg0", live), &ctx);
        assert!(matches!(err, Err(TransformError::BoundaryNotPresent(_))));
    }

    #[test]
    fn reconcile_rematerialize_after_revert_is_not_blocked_by_the_mint_guard() {
        // A reconcile-rematerialize composes from the RE-CUT store (the historian re-cuts
        // compartments after a revert), so its minted anchor is presentable again and the
        // mint guard must not false-fire. Here the re-cut store keeps a compartment whose
        // anchor IS present in the post-revert live array (partial revert: the covered
        // head survived, only the tail past it reverted).
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments(
            "ses",
            &[comp(0, 1, 1, "a", "S0"), comp(1, 2, 2, "t2", "S1")],
        )
        .unwrap();
        let live_full = vec![
            item("a", 1, "raw"),
            item("t2", 2, "turn two"),
            item("t3", 3, "tail"),
        ];
        let boot = run(&s, &req("ses", "cfg0", live_full), &spine());
        assert_eq!(boot.action, "HARD");
        assert_eq!(boot.boundary_id, "t2#0");

        // Revert removes t2 (the boundary) and t3; the historian re-cuts to just C0.
        let live_reverted = vec![item("a", 1, "raw"), item("t4", 2, "new turn")];
        let revert = run(&s, &req("ses", "cfg0", live_reverted.clone()), &spine());
        assert_eq!(revert.action, "SOFT+", "revert never busts on sight");
        assert!(revert.reconcile_pending);

        s.replace_compartments("ses", &[comp(0, 1, 1, "a", "S0")])
            .unwrap();
        let remat = run(&s, &req("ses", "cfg0", live_reverted), &spine());
        assert_eq!(
            remat.action, "HARD",
            "reconcile rematerializes without tripping the mint guard"
        );
        assert_eq!(remat.boundary_id, "a#0", "re-minted from the re-cut store");
        assert!(!remat.reconcile_pending);
        assert_eq!(tail_ids(&remat), vec!["t4"]);
    }

    #[test]
    fn reconcile_rematerialize_with_unrecut_store_truncates_and_refolds_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments(
            "ses",
            &[comp(1, 1, 1, "a", "S0"), comp(2, 2, 2, "t2", "S1")],
        )
        .unwrap();
        let live_full = vec![
            item("a", 1, "raw"),
            item("t2", 2, "turn two"),
            item("t3", 3, "tail"),
        ];
        let boot = run(&s, &req("ses", "cfg0", live_full), &spine());
        assert_eq!(boot.action, "HARD");
        assert_eq!(boot.boundary_id, "t2#0");

        let live_reverted = vec![item("a", 1, "raw"), item("t4", 2, "new turn")];
        let revert = run(&s, &req("ses", "cfg0", live_reverted.clone()), &spine());
        assert_eq!(revert.action, "SOFT+", "revert never busts on sight");
        assert!(revert.reconcile_pending);
        let loaded = s.load("ses").unwrap();
        let mut meta = loaded.meta.clone();
        meta.last_execute_ordinal = 99;
        s.commit("ses", loaded.row_version, &loaded.core, &meta)
            .unwrap();
        let before_recut = s.load("ses").unwrap().row_version.unwrap();

        let remat = run(&s, &req("ses", "cfg0", live_reverted.clone()), &spine());
        assert_eq!(remat.action, "HARD");
        assert_eq!(
            remat.boundary_id, "a#0",
            "the surviving prefix is re-minted"
        );
        assert_eq!(remat.coverage_ordinal, Some(1));
        assert!(!remat.reconcile_pending);
        assert_eq!(tail_ids(&remat), vec!["t4"]);
        let loaded = s.load("ses").unwrap();
        assert_eq!(loaded.meta.revert_epoch, 1);
        assert!(loaded
            .meta
            .last_recut
            .as_deref()
            .unwrap()
            .contains("dropped seq 2"));
        assert_eq!(loaded.meta.folded_compartment_seq, 1);
        assert_eq!(loaded.meta.last_execute_ordinal, 1);
        assert_eq!(loaded.row_version.unwrap(), before_recut + 2);
        assert_eq!(s.load_compartments("ses").unwrap().len(), 1);

        s.append_compartments("ses", &[comp(3, 2, 2, "t4", "S2")])
            .unwrap();
        let folded_again = run(&s, &req("ses", "cfg0", live_reverted), &spine());
        assert_eq!(folded_again.action, "SOFT");
        assert_eq!(folded_again.boundary_id, "t4#0");
        assert_eq!(folded_again.coverage_ordinal, Some(2));
        assert_eq!(tail_ids(&folded_again), Vec::<&str>::new());
    }

    #[test]
    fn reconcile_recut_nothing_survives_arms_pending_raw_without_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 2, "t2", "S0")])
            .unwrap();
        let live_full = vec![item("t2", 2, "turn two"), item("t3", 3, "tail")];
        let boot = run(&s, &req("ses", "cfg0", live_full), &spine());
        assert_eq!(boot.action, "HARD");
        assert_eq!(boot.boundary_id, "t2#0");
        let before_absent = s.load("ses").unwrap();
        let before_compartments = s.load_compartments("ses").unwrap();

        let live_absent = vec![item("t9", 9, "post-revert")];
        let armed = run(&s, &req("ses", "cfg0", live_absent.clone()), &spine());
        assert_eq!(armed.action, "PASSTHROUGH");
        assert!(armed.committed, "arming writes the one durable alarm row");
        assert!(!armed.reconcile_pending);
        assert_eq!(tail_ids(&armed), vec!["t9"]);
        let after_arm = s.load("ses").unwrap();
        assert_eq!(after_arm.core.boundary_id, before_absent.core.boundary_id);
        assert!(!after_arm.core.reconcile_pending);
        assert_eq!(after_arm.meta.revert_epoch, before_absent.meta.revert_epoch);
        assert_eq!(s.load_compartments("ses").unwrap(), before_compartments);
        assert!(after_arm.meta.pending_rewrite.is_some());
        assert!(after_arm
            .meta
            .pending_rewrite_last_failure
            .as_deref()
            .unwrap()
            .contains("upstream lineage-switch detection miss"));

        let row_after_arm = after_arm.row_version.unwrap();
        let repeat = run(&s, &req("ses", "cfg0", live_absent), &spine());
        assert_eq!(repeat.action, "PASSTHROUGH");
        assert!(!repeat.committed, "arm-once pending repeats are write-free");
        assert_eq!(repeat.row_version, row_after_arm);
        assert_eq!(tail_ids(&repeat), tail_ids(&armed));
        assert_eq!(s.load("ses").unwrap().row_version.unwrap(), row_after_arm);
        assert_eq!(s.load_compartments("ses").unwrap(), before_compartments);
    }

    #[test]
    fn fresh_key_share_nothing_bootstraps_without_pending() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let boot = run(
            &s,
            &req("fresh", "cfg0", vec![item("foreign", 9, "new session")]),
            &spine(),
        );
        assert_eq!(boot.action, "HARD");
        let loaded = s.load("fresh").unwrap();
        assert!(loaded.meta.pending_rewrite.is_none());
        assert!(loaded.row_version.is_some());
    }

    #[test]
    fn provider_extras_strip_canary_does_not_arm_pending_on_legitimate_extension() {
        // Coupling canary: MC's shape fingerprint uses flattened block bytes, while the
        // upstream lineage-switch detector keys only on role/kind. Per-turn-churning
        // provider fields must therefore be absent from both bases before MC sees the
        // array; changing either basis requires changing the other together.
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "a", "S0")])
            .unwrap();
        let initial = post_submit_strip_block_provider_extras(vec![
            item_with_block_provider_extras("a", 1, "raw", "first"),
            item_with_block_provider_extras("b", 2, "tail", "first"),
        ]);
        let boot = run(&s, &req("ses", "cfg0", initial), &spine());
        assert_eq!(boot.action, "HARD");
        assert_eq!(boot.boundary_id, "a#0");

        let extension = post_submit_strip_block_provider_extras(vec![
            item_with_block_provider_extras("a", 1, "raw", "changed"),
            item_with_block_provider_extras("b", 2, "tail", "changed"),
            item_with_block_provider_extras("c", 3, "extension", "changed"),
        ]);
        let pass = run(&s, &req("ses", "cfg0", extension), &spine());
        assert_eq!(pass.action, "SOFT+");
        assert_eq!(pass.boundary_id, "a#0");
        assert!(s.load("ses").unwrap().meta.pending_rewrite.is_none());
    }

    #[test]
    fn pending_rewrite_recovers_on_boundary_present_extension() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 2, "t2", "S0")])
            .unwrap();
        let live_present = vec![item("t2", 2, "turn two"), item("t3", 3, "tail")];
        let boot = run(&s, &req("ses", "cfg0", live_present.clone()), &spine());
        assert_eq!(boot.boundary_id, "t2#0");

        let absent = vec![item("foreign", 50, "other conversation")];
        let armed = run(&s, &req("ses", "cfg0", absent), &spine());
        assert_eq!(armed.action, "PASSTHROUGH");
        assert!(s.load("ses").unwrap().meta.pending_rewrite.is_some());

        let recovered_live = vec![
            item("t2", 2, "turn two"),
            item("t3", 3, "tail"),
            item("t4", 4, "extension"),
        ];
        let recovered = run(&s, &req("ses", "cfg0", recovered_live), &spine());
        assert_eq!(recovered.action, "SOFT+");
        assert_eq!(recovered.boundary_id, "t2#0");
        assert_eq!(tail_ids(&recovered), vec!["t3", "t4"]);
        let loaded = s.load("ses").unwrap();
        assert!(loaded.meta.pending_rewrite.is_none());
        assert!(!loaded.meta.pending_rewrite_ambiguous);
        assert_eq!(loaded.meta.revert_epoch, 0);
        assert_eq!(s.load_compartments("ses").unwrap().len(), 1);
    }

    #[test]
    fn pending_rewrite_interleave_sets_ambiguous_without_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 2, "t2", "S0")])
            .unwrap();
        let present = vec![item("t2", 2, "turn two"), item("t3", 3, "tail")];
        let boot = run(&s, &req("ses", "cfg0", present.clone()), &spine());
        assert_eq!(boot.boundary_id, "t2#0");

        for cycle in 0..3 {
            let absent = vec![item(&format!("foreign{cycle}"), 50 + cycle, "other")];
            let raw = run(&s, &req("ses", "cfg0", absent), &spine());
            assert_eq!(raw.action, "PASSTHROUGH");
            let normal = run(&s, &req("ses", "cfg0", present.clone()), &spine());
            assert_eq!(normal.action, "SOFT+");
        }

        let loaded = s.load("ses").unwrap();
        assert!(loaded.meta.pending_rewrite.is_none());
        assert!(loaded.meta.pending_rewrite_ambiguous);
        assert!(loaded
            .meta
            .pending_rewrite_last_failure
            .as_deref()
            .unwrap()
            .contains("ambiguous_pending_rewrite"));
        assert_eq!(loaded.meta.revert_epoch, 0);
        assert_eq!(s.load_compartments("ses").unwrap().len(), 1);
    }

    #[test]
    fn pending_rewrite_passes_isolate_ingress_meta_usage_and_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 2, "t2", "S0")])
            .unwrap();
        let boot = run(
            &s,
            &with_usage(
                req(
                    "ses",
                    "cfg0",
                    vec![item("t2", 2, "turn two"), item("t3", 3, "tail")],
                ),
                10,
                100,
            ),
            &spine(),
        );
        assert_eq!(boot.boundary_id, "t2#0");
        let before = s.load("ses").unwrap();

        let foreign_same_mid = vec![item("t3", 90, "foreign bytes with reused tail mid")];
        let armed = run(
            &s,
            &with_usage(req("ses", "cfg0", foreign_same_mid.clone()), 95, 100),
            &spine(),
        );
        assert_eq!(armed.action, "PASSTHROUGH");
        let after_arm = s.load("ses").unwrap();
        assert_eq!(
            after_arm.meta.block_identity_by_mid, before.meta.block_identity_by_mid,
            "foreign block identities are never adopted while arming pending"
        );
        assert_eq!(after_arm.meta.last_usage, before.meta.last_usage);
        assert_eq!(
            after_arm.core.reconcile_pending,
            before.core.reconcile_pending
        );

        let row_after_arm = after_arm.row_version.unwrap();
        let meta_after_arm = after_arm.meta.clone();
        let repeat = run(
            &s,
            &with_usage(req("ses", "cfg0", foreign_same_mid), 100, 100),
            &spine(),
        );
        assert_eq!(repeat.action, "PASSTHROUGH");
        let after_repeat = s.load("ses").unwrap();
        assert_eq!(after_repeat.row_version.unwrap(), row_after_arm);
        assert_eq!(after_repeat.meta, meta_after_arm);
        assert_eq!(
            after_repeat.core.reconcile_pending,
            before.core.reconcile_pending
        );
    }

    #[test]
    fn pending_rewrite_persists_across_store_restart() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = store(dir.path());
            s.replace_compartments("ses", &[comp(1, 1, 2, "t2", "S0")])
                .unwrap();
            let boot = run(
                &s,
                &req(
                    "ses",
                    "cfg0",
                    vec![item("t2", 2, "turn two"), item("t3", 3, "tail")],
                ),
                &spine(),
            );
            assert_eq!(boot.boundary_id, "t2#0");
            let raw = run(
                &s,
                &req("ses", "cfg0", vec![item("foreign", 90, "other")]),
                &spine(),
            );
            assert_eq!(raw.action, "PASSTHROUGH");
            assert!(s.load("ses").unwrap().meta.pending_rewrite.is_some());
        }

        let s = store(dir.path());
        let before = s.load("ses").unwrap();
        assert!(before.meta.pending_rewrite.is_some());
        let row = before.row_version.unwrap();
        let raw = run(
            &s,
            &req("ses", "cfg0", vec![item("foreign", 90, "other")]),
            &spine(),
        );
        assert_eq!(raw.action, "PASSTHROUGH");
        assert!(!raw.committed);
        assert_eq!(s.load("ses").unwrap().row_version.unwrap(), row);
        assert_eq!(s.load_compartments("ses").unwrap().len(), 1);
    }

    #[test]
    fn first_fold_error_leaves_state_unchanged_and_the_hard_retries_visibly() {
        // Fold-failure retry semantics: if the first-fold HARD fires and the fold itself
        // errors, the transform returns Err and commits NOTHING, so the persisted boundary
        // stays empty and the compartment stays present — meaning the next pass re-evaluates
        // the same guard, fires the HARD again, and surfaces the SAME error. A persistent
        // fold failure is therefore a stream of VISIBLE transform errors (fail-loud +
        // retry-by-construction), never a silent defer that buries a stranded compartment.
        //
        // The injected failure is a real fail-loud path: a compartment that leaves a LEADING
        // coverage gap (a live item ordinal-before the first covered ordinal) — compose
        // refuses to drop live context it cannot account for.
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // Compartment covers ordinals 5..=5, but the live array has ordinal 1 before it → gap.
        s.replace_compartments("ses", &[comp(0, 5, 5, "m5", "S")])
            .unwrap();
        let live = vec![
            item("early", 1, "before coverage"),
            item("m5", 5, "covered"),
        ];

        let ctx = pctx("git:proj", "/nonexistent-docs", 0);
        let first = transform(&s, &req("ses", "cfg0", live.clone()), &ctx);
        assert!(
            first.is_err(),
            "first-fold HARD hits the leading-gap fail-loud path"
        );
        // The failed pass wrote nothing → the guard re-fires and errors again (visible), it
        // does NOT silently fall through to a defer that strands the compartment.
        let retry = transform(&s, &req("ses", "cfg0", live), &ctx);
        assert!(
            retry.is_err(),
            "state unchanged after the failed fold → the HARD retries and stays visible"
        );
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
            r.boundary_id, "m10#0",
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
        // covered by no compartment, yet they sit below coverage_ordinal. build_output
        // would silently trim those raw messages, so the live coverage guard must fail loud.
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
        )
        .unwrap_err();
        assert!(
            matches!(err, TransformError::CoverageGap(_)),
            "a leading gap must fail loud, not silently drop the early live item: {err:?}"
        );
    }

    #[test]
    fn interior_live_coverage_gap_fails_loud_not_silent_drop() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments(
            "ses",
            &[comp(1, 1, 2, "m2", "S1"), comp(2, 6, 7, "m7", "S2")],
        )
        .unwrap();
        let items = vec![
            item("m1", 1, "covered one"),
            item("m2", 2, "covered two"),
            item("m4", 4, "present but uncovered"),
            item("m6", 6, "covered six"),
            item("m7", 7, "covered seven"),
            item("t8", 8, "tail"),
        ];
        let err = transform(
            &s,
            &req("ses", "cfg0", items),
            &pctx("git:proj", "/nonexistent-docs", 0),
        )
        .unwrap_err();
        assert!(matches!(err, TransformError::CoverageGap(_)));
        assert!(
            err.to_string().contains("m4"),
            "the uncovered live message should be named in the loud failure: {err:?}"
        );
    }

    #[test]
    fn leading_coverage_gap_exempts_pinned_system_message() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 2, "m2", "S")])
            .unwrap();
        let items = vec![
            system_item("sys0", 0, "identity lead"),
            item("m2", 2, "covered"),
            item("t3", 3, "tail"),
        ];
        let out = transform(
            &s,
            &req("ses", "cfg0", items),
            &pctx("git:proj", "/nonexistent-docs", 0),
        )
        .unwrap();
        assert_eq!(out.coverage_ordinal, Some(2));
    }

    #[test]
    fn leading_uncovered_system_passes_through_for_full_array_profiles() {
        for profile in [
            SerializerProfile::OwnedLlmRunner,
            SerializerProfile::Pi,
            SerializerProfile::OpencodeAiSdk,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let s = store(dir.path());
            let session = format!("lead-{}", profile.wire_id());
            s.replace_compartments(&session, &[comp(1, 1, 1, "m1", "SUMMARY")])
                .unwrap();
            let leading_system = system_item("sys0", 0, "identity lead");
            let r = run(
                &s,
                &profile_req(
                    profile,
                    &session,
                    "cfg0",
                    vec![
                        leading_system.clone(),
                        item("m1", 1, "covered"),
                        item("t2", 2, "tail"),
                    ],
                ),
                &spine(),
            );
            assert_eq!(r.action, "HARD");
            assert!(
                !m0_bytes(&r).contains("identity lead"),
                "{} must not absorb the uncovered leading system into m0",
                profile.wire_id()
            );
            let output_system = r
                .messages()
                .iter()
                .find(|message| message.role == "system")
                .unwrap_or_else(|| panic!("{} output lost the leading system", profile.wire_id()));
            assert_eq!(output_system, &leading_system.ck);
        }
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
            assert!(
                r.committed,
                "first-seen tail mids persist identity vectors even on a defer"
            );
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
    fn public_memory_update_rides_soft_not_hard() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let memory_id = s
            .insert_memory(memory_input("git:proj", "ARCHITECTURE", "original", 0))
            .unwrap();
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1msg", "SUMMARY")])
            .unwrap();
        let before = run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(before.action, "HARD");

        s.update_memory_content("git:proj", memory_id, "corrected", 1)
            .unwrap();
        let soft = run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(soft.action, "SOFT", "public update port should not HARD");
        assert_eq!(m0_bytes(&soft), m0_bytes(&before));
        assert!(
            m1_bytes(&soft).contains("<memory-updates>"),
            "{}",
            m1_bytes(&soft)
        );
        assert!(m1_bytes(&soft).contains("corrected"), "{}", m1_bytes(&soft));
    }

    #[test]
    fn public_memory_insert_rides_soft_not_hard() {
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

        s.insert_memory(memory_input(
            "git:proj",
            "ARCHITECTURE",
            "a durable rule",
            1,
        ))
        .unwrap();
        let soft = run(
            &s,
            &req("ses", "cfg0", vec![item("m1msg", 1, "raw")]),
            &spine(),
        );
        assert_eq!(soft.action, "SOFT", "public insert port should not HARD");
        assert_eq!(m0_bytes(&soft), m0_bytes(&before));
        assert!(
            m1_bytes(&soft).contains("<new-memories>"),
            "{}",
            m1_bytes(&soft)
        );
        assert!(
            m1_bytes(&soft).contains("a durable rule"),
            "{}",
            m1_bytes(&soft)
        );
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
        assert_eq!(boot.boundary_id, "m10#0");
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
            soft.boundary_id, "m20#0",
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

        // A share-nothing boundary absence is not a safe re-cut target. It degrades to
        // raw pass-through and arms the pending-rewrite alarm without touching lineage.
        let revert = run(
            &s,
            &req("ses", "cfg0", vec![item("z", 30, "other")]),
            &spine(),
        );
        assert_eq!(revert.action, "PASSTHROUGH");
        assert!(!revert.reconcile_pending);
        assert!(s.load("ses").unwrap().meta.pending_rewrite.is_some());
    }

    #[test]
    fn coverage_extending_soft_prunes_covered_red_units() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // m0 folds C1 (covers 1..=10); t11/t12 are live tail items.
        s.replace_compartments("ses", &[comp(1, 1, 10, "m10", "S1")])
            .unwrap();
        let items_v1 = vec![
            item("m10", 10, "raw"),
            item("t11", 11, "tool output"),
            item("t12", 12, "tail"),
        ];
        run(&s, &req("ses", "cfg0", items_v1.clone()), &spine());

        // A SOFT freezes a reduction on t11 (still in the tail).
        let reduced = run(
            &s,
            &req("ses", "cfg0", items_v1.clone()),
            &with_reductions(vec![reduce("t11", "drop", "[dropped]")]),
        );
        assert_eq!(reduced.action, "SOFT");
        assert_eq!(tail_bytes(&reduced, "t11"), "[dropped]");

        // C2 publishes covering through t12 → the next SOFT extends coverage past
        // When the next compartment extends coverage past t11's ordinal, the frozen
        // reduction red:t11#0 must be removed in the same update. Its target message
        // is no longer in the tail, so retaining the reduction would waste space and
        // could create spurious conflicts if a later revert reuses the same message id
        // with different content.
        s.replace_compartments(
            "ses",
            &[comp(1, 1, 10, "m10", "S1"), comp(2, 11, 12, "t12", "S2")],
        )
        .unwrap();
        let items_v2 = vec![
            item("m10", 10, "raw"),
            item("t11", 11, "tool output"),
            item("t12", 12, "tail"),
            item("t13", 13, "newest"),
        ];
        let folded = run(&s, &req("ses", "cfg0", items_v2.clone()), &spine());
        assert_eq!(folded.action, "SOFT", "new compartment rides a SOFT");
        assert_eq!(tail_ids(&folded), vec!["t13"], "coverage trimmed t11/t12");

        // The invariant (fail-loud form): after a coverage advance, no frozen red:*
        // unit may target a covered ordinal.
        let loaded = s.load("ses").unwrap();
        let core = loaded.core;
        let coverage = loaded.meta.coverage_ordinal.expect("coverage advanced");
        let covered_ordinals: std::collections::BTreeMap<String, u64> = items_v2
            .iter()
            .map(|i| (target_id(&i.mid), i.ordinal))
            .collect();
        for unit in &core.frozen_units {
            let Some(target) = unit.key.strip_prefix("red:") else {
                continue;
            };
            if let Some(&ord) = covered_ordinals.get(target) {
                assert!(
                    ord > coverage,
                    "frozen {} survived its target's coverage (ord {ord} <= coverage {coverage})",
                    unit.key
                );
            }
        }
        // And the pruned unit is gone specifically.
        assert!(
            core.frozen_units.iter().all(|u| u.key != "red:t11#0"),
            "red:t11#0 must be pruned by the coverage-extending SOFT"
        );

        // Defer replays byte-identical after the prune (the prune itself must not
        // perturb replay).
        let defer = run(&s, &req("ses", "cfg0", items_v2), &spine());
        assert_eq!(defer.action, "SOFT+");
        assert_eq!(m1_bytes(&defer), m1_bytes(&folded));
        assert_eq!(m0_bytes(&defer), m0_bytes(&folded));
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
    fn share_nothing_revert_arms_pending_instead_of_reconcile_rematerializing() {
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
        assert_eq!(before.boundary_id, "m10#0");

        let raw = run(
            &s,
            &req("ses", "cfg0", vec![item("z", 50, "other")]),
            &spine(),
        );
        assert_eq!(raw.action, "PASSTHROUGH");
        assert!(!raw.reconcile_pending);
        assert_eq!(tail_ids(&raw), vec!["z"]);
        let loaded = s.load("ses").unwrap();
        assert_eq!(loaded.core.boundary_id, "m10#0");
        assert!(!loaded.core.reconcile_pending);
        assert!(loaded.meta.pending_rewrite.is_some());
        assert_eq!(loaded.meta.revert_epoch, 0);
        assert_eq!(s.load_compartments("ses").unwrap().len(), 1);
    }

    #[test]
    fn legacy_baseline_migrates_to_clean_m0_m1() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        // seed a legacy single-"baseline"-unit state directly, initialized
        let legacy_core = CoreState {
            version: 1,
            boundary_id: "a#0".into(),
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
        // After a stored legacy baseline is cleared and rebuilt as the current m0/m1
        // shape, the response has no leftover baseline state: it contains exactly two
        // synthetic messages, and m0 was re-composed from store data.
        assert_eq!(r.messages().iter().filter(|m| m.meta.synthetic).count(), 2);
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
            boundary_id: "a#0".into(),
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
        let reserved = transform(&s, &req("ses", "cfg0", vec![item("mc_m0", 2, "x")]), &dc);
        assert!(matches!(reserved, Err(TransformError::ReservedId)));

        // non-monotonic ordinals
        let bad = transform(
            &s,
            &req("ses", "cfg0", vec![item("a", 5, "x"), item("b", 5, "y")]),
            &dc,
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
        let mut stale = item(M0_ID, 0, "STALE");
        stale.ck.meta.synthetic = true;
        let items = vec![stale, item("m1msg", 1, "raw"), item("t2", 2, "tail2")];
        let r = run(&s, &req("ses", "cfg0", items), &spine());
        // boundary m1msg still found (synthetic stripped), tail filter uncorrupted
        assert_eq!(r.action, "SOFT+");
        assert_eq!(tail_ids(&r), vec!["t2"]);
    }

    #[test]
    fn zero_block_tail_message_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("zero", &[comp(1, 1, 1, "a", "SUMMARY")])
            .unwrap();
        run(
            &s,
            &req("zero", "cfg0", vec![item("a", 1, "raw")]),
            &spine(),
        );

        let empty = empty_message("empty", 2);
        let r = run(
            &s,
            &req("zero", "cfg0", vec![item("a", 1, "raw"), empty.clone()]),
            &spine(),
        );

        assert_eq!(r.action, "SOFT+");
        assert_eq!(tail_ids(&r), vec!["empty"]);
        let emitted = r
            .messages()
            .iter()
            .find(|m| !m.meta.synthetic && m.meta.harness_id.as_deref() == Some("empty"))
            .expect("empty tail message emitted");
        assert_eq!(
            serde_json::to_value(emitted).unwrap(),
            serde_json::to_value(&empty.ck).unwrap()
        );
    }

    #[test]
    fn synthetic_todo_compose_at_bust_freezes_position_across_defer_tail_growth() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("freeze", &[comp(1, 1, 1, "a", "SUMMARY")])
            .unwrap();
        let todos = json!([{ "content": "Plan", "status": "pending", "priority": "high" }]);
        let bust_items = vec![
            item("a", 1, "raw"),
            todowrite_call("todo", 2, todos.clone()),
        ];
        let bust = run(&s, &req("freeze", "cfg0", bust_items.clone()), &spine());

        assert_eq!(bust.action, "HARD");
        assert_eq!(
            synthetic_todo_index(&bust),
            message_index(&bust, "todo") + 1
        );
        let bust_prefix = prefix_through_synthetic_todo(&bust);

        let defer_items = vec![
            item("a", 1, "raw"),
            todowrite_call("todo", 2, todos),
            item("later1", 3, "new tail 1"),
            item("later2", 4, "new tail 2"),
        ];
        let defer = run(&s, &req("freeze", "cfg0", defer_items), &spine());

        assert_eq!(defer.action, "SOFT+");
        assert_eq!(
            synthetic_todo_index(&defer),
            message_index(&defer, "todo") + 1
        );
        assert!(message_index(&defer, "later1") > synthetic_todo_index(&defer) + 1);
        assert!(message_index(&defer, "later2") > synthetic_todo_index(&defer) + 1);
        assert_eq!(prefix_through_synthetic_todo(&defer), bust_prefix);
    }

    #[test]
    fn synthetic_todo_keep_on_bust_does_not_relocate() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("keep", &[comp(1, 1, 1, "a", "SUMMARY")])
            .unwrap();
        let todos = json!([{ "content": "Keep", "status": "pending", "priority": "high" }]);
        let first_items = vec![
            item("a", 1, "raw"),
            todowrite_call("todo", 2, todos.clone()),
        ];
        let first = run(&s, &req("keep", "cfg0", first_items), &spine());
        let first_pair = synthetic_todo_pair_bytes(&first);
        let first_prefix = prefix_through_synthetic_todo(&first);

        let second_items = vec![
            item("a", 1, "raw"),
            todowrite_call("todo", 2, todos),
            item("later", 3, "tail grew"),
        ];
        let second = run(&s, &req("keep", "cfg1", second_items), &spine());

        assert_eq!(second.action, "HARD");
        assert_eq!(
            synthetic_todo_index(&second),
            message_index(&second, "todo") + 1
        );
        assert!(message_index(&second, "later") > synthetic_todo_index(&second) + 1);
        assert_eq!(synthetic_todo_pair_bytes(&second), first_pair);
        assert_eq!(prefix_through_synthetic_todo(&second), first_prefix);
        assert_eq!(
            s.load("keep")
                .unwrap()
                .meta
                .synthetic_todo
                .as_ref()
                .and_then(|pair| pair.anchor_mid.as_deref()),
            Some("todo")
        );
    }

    #[test]
    fn synthetic_todo_keep_reanchors_when_coverage_advance_folds_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("keep-fold", &[comp(1, 1, 1, "a", "SUMMARY-1")])
            .unwrap();
        let todos = json!([{ "content": "Fold", "status": "pending", "priority": "high" }]);
        let first = run(
            &s,
            &req(
                "keep-fold",
                "cfg0",
                vec![
                    item("a", 1, "raw"),
                    todowrite_call("todo", 2, todos.clone()),
                ],
            ),
            &spine(),
        );
        let first_pair = synthetic_todo_pair_bytes(&first);
        let first_call_id = synthetic_todo_call_id(&first);

        s.replace_compartments(
            "keep-fold",
            &[
                comp(1, 1, 1, "a", "SUMMARY-1"),
                comp(2, 2, 2, "todo", "SUMMARY-2"),
            ],
        )
        .unwrap();
        let moved = run(
            &s,
            &req(
                "keep-fold",
                "cfg0",
                vec![
                    item("a", 1, "raw"),
                    todowrite_call("todo", 2, todos),
                    item("t3", 3, "new tail end"),
                ],
            ),
            &spine(),
        );

        assert_eq!(moved.action, "SOFT");
        assert_eq!(synthetic_todo_call_id(&moved), first_call_id);
        assert_eq!(synthetic_todo_pair_bytes(&moved), first_pair);
        assert_eq!(
            synthetic_todo_index(&moved),
            message_index(&moved, "t3") + 1
        );
        assert_eq!(
            s.load("keep-fold")
                .unwrap()
                .meta
                .synthetic_todo
                .as_ref()
                .and_then(|pair| pair.anchor_mid.as_deref()),
            Some("t3")
        );
    }

    #[test]
    fn crash_reentry_after_recut_uses_coverage_shrink_for_todo_reanchor() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("shrink", &[comp(1, 1, 1, "a", "SUMMARY-1")])
            .unwrap();
        let todos = json!([{ "content": "Shrink", "status": "pending", "priority": "high" }]);
        let first = run(
            &s,
            &req(
                "shrink",
                "cfg0",
                vec![
                    item("a", 1, "raw"),
                    todowrite_call("todo", 2, todos.clone()),
                ],
            ),
            &spine(),
        );
        let first_pair = synthetic_todo_pair_bytes(&first);
        let first_call_id = synthetic_todo_call_id(&first);

        let loaded = s.load("shrink").unwrap();
        s.replace_compartments(
            "shrink",
            &[
                comp(1, 1, 1, "a", "SUMMARY-1"),
                comp(2, 2, 2, "todo", "SUMMARY-2"),
                comp(3, 3, 3, "gone", "SUMMARY-3"),
            ],
        )
        .unwrap();
        let mut core = loaded.core;
        core.boundary_id = "gone#0".to_string();
        core.reconcile_pending = true;
        let mut meta = loaded.meta;
        meta.coverage_ordinal = Some(3);
        meta.folded_compartment_seq = 3;
        meta.synthetic_todo
            .as_mut()
            .expect("first bust freezes a synthetic todo")
            .anchor_mid = Some("gone".to_string());
        let rv = s
            .commit("shrink", loaded.row_version, &core, &meta)
            .unwrap();

        s.truncate_compartments_for_revert("shrink", 1, Some(rv))
            .unwrap();
        let recovered = run(
            &s,
            &req(
                "shrink",
                "cfg0",
                vec![
                    item("a", 1, "raw"),
                    todowrite_call("todo", 2, todos),
                    item("tail", 4, "new post-revert tail"),
                ],
            ),
            &spine(),
        );

        assert_eq!(recovered.action, "HARD");
        assert_eq!(recovered.boundary_id, "a#0");
        assert_eq!(synthetic_todo_call_id(&recovered), first_call_id);
        assert_eq!(synthetic_todo_pair_bytes(&recovered), first_pair);
        assert_eq!(
            s.load("shrink")
                .unwrap()
                .meta
                .synthetic_todo
                .as_ref()
                .and_then(|pair| pair.anchor_mid.as_deref()),
            Some("tail")
        );
    }

    #[test]
    fn synthetic_todo_defer_after_keep_reanchor_replays_at_new_position() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("keep-fold-defer", &[comp(1, 1, 1, "a", "SUMMARY-1")])
            .unwrap();
        let todos = json!([{ "content": "Fold defer", "status": "pending", "priority": "high" }]);
        run(
            &s,
            &req(
                "keep-fold-defer",
                "cfg0",
                vec![
                    item("a", 1, "raw"),
                    todowrite_call("todo", 2, todos.clone()),
                ],
            ),
            &spine(),
        );

        s.replace_compartments(
            "keep-fold-defer",
            &[
                comp(1, 1, 1, "a", "SUMMARY-1"),
                comp(2, 2, 2, "todo", "SUMMARY-2"),
            ],
        )
        .unwrap();
        let moved_items = vec![
            item("a", 1, "raw"),
            todowrite_call("todo", 2, todos),
            item("t3", 3, "new tail end"),
        ];
        let moved = run(
            &s,
            &req("keep-fold-defer", "cfg0", moved_items.clone()),
            &spine(),
        );
        let moved_prefix = prefix_through_synthetic_todo(&moved);

        let defer = run(&s, &req("keep-fold-defer", "cfg0", moved_items), &spine());

        assert_eq!(defer.action, "SOFT+");
        assert_eq!(
            synthetic_todo_index(&defer),
            message_index(&defer, "t3") + 1
        );
        assert_eq!(prefix_through_synthetic_todo(&defer), moved_prefix);
    }

    #[test]
    fn synthetic_todo_replace_relocates_to_new_tail_end() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("replace", &[comp(1, 1, 1, "a", "SUMMARY")])
            .unwrap();
        let first_todos = json!([{ "content": "Old", "status": "pending", "priority": "high" }]);
        let first = run(
            &s,
            &req(
                "replace",
                "cfg0",
                vec![item("a", 1, "raw"), todowrite_call("todo", 2, first_todos)],
            ),
            &spine(),
        );
        let first_call_id = synthetic_todo_call_id(&first);

        let changed_todos = json!([{ "content": "New", "status": "pending", "priority": "high" }]);
        let second = run(
            &s,
            &req(
                "replace",
                "cfg1",
                vec![
                    item("a", 1, "raw"),
                    item("later", 3, "tail before changed todo"),
                    todowrite_call("todo2", 4, changed_todos),
                ],
            ),
            &spine(),
        );

        assert_eq!(second.action, "HARD");
        assert_ne!(synthetic_todo_call_id(&second), first_call_id);
        assert_eq!(
            synthetic_todo_index(&second),
            message_index(&second, "todo2") + 1
        );
    }

    #[test]
    fn synthetic_todo_clear_removes_pair_for_terminal_state() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("clear", &[comp(1, 1, 1, "a", "SUMMARY")])
            .unwrap();
        let active = json!([{ "content": "Active", "status": "pending", "priority": "high" }]);
        run(
            &s,
            &req(
                "clear",
                "cfg0",
                vec![item("a", 1, "raw"), todowrite_call("todo", 2, active)],
            ),
            &spine(),
        );

        let terminal = json!([
            { "content": "Done", "status": "completed", "priority": "high" },
            { "content": "Cancelled", "status": "cancelled", "priority": "low" }
        ]);
        let cleared = run(
            &s,
            &req(
                "clear",
                "cfg1",
                vec![item("a", 1, "raw"), todowrite_call("done", 3, terminal)],
            ),
            &spine(),
        );

        assert_eq!(cleared.action, "HARD");
        assert!(cleared.messages().iter().all(|m| {
            !matches!(
                m.content.first().map(|block| &block.kind),
                Some(ck_wire::CkKind::ToolCall { name, .. }) if name == "todowrite"
            ) || !m.meta.synthetic
        }));
        assert!(s.load("clear").unwrap().meta.synthetic_todo.is_none());
    }

    #[test]
    fn synthetic_todo_aged_out_capture_composes_from_meta_on_bust() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("aged", &[comp(1, 1, 2, "todo", "SUMMARY")])
            .unwrap();
        let todos = json!([{ "content": "Persisted", "status": "pending", "priority": "high" }]);
        run(
            &s,
            &req(
                "aged",
                "cfg0",
                vec![item("a", 1, "raw"), todowrite_call("todo", 2, todos)],
            ),
            &spine(),
        );
        let loaded = s.load("aged").unwrap();
        let mut meta = loaded.meta;
        meta.synthetic_todo = None;
        meta.last_render_config = "force a hard".to_string();
        s.commit("aged", loaded.row_version, &loaded.core, &meta)
            .unwrap();

        let aged = run(
            &s,
            &req(
                "aged",
                "cfg1",
                vec![
                    item("a", 1, "raw"),
                    todowrite_call(
                        "todo",
                        2,
                        json!([{ "content": "Persisted", "status": "pending", "priority": "high" }]),
                    ),
                ],
            ),
            &spine(),
        );

        assert_eq!(aged.action, "HARD");
        assert_eq!(tail_ids(&aged), Vec::<&str>::new());
        assert_eq!(
            synthetic_todo_index(&aged),
            2,
            "None anchor appends after m0/m1 when no real tail remains"
        );
        assert!(s.load("aged").unwrap().meta.synthetic_todo.is_some());
    }

    #[test]
    fn synthetic_todo_none_anchor_stays_before_grown_tail_on_defer() {
        // A pair frozen with anchor_mid = None (composed when the tail was empty) must be
        // pinned immediately after m0/m1, NOT floated to the end. A later defer that grows
        // the tail must leave the [m0, m1, pair] prefix byte-identical, with the new tail
        // message landing AFTER the pair — otherwise the None-anchor path reintroduces the
        // always-last floater the position-freeze exists to prevent.
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("none-anchor", &[comp(1, 1, 2, "todo", "SUMMARY")])
            .unwrap();
        let todos = json!([{ "content": "Persisted", "status": "pending", "priority": "high" }]);
        run(
            &s,
            &req(
                "none-anchor",
                "cfg0",
                vec![
                    item("a", 1, "raw"),
                    todowrite_call("todo", 2, todos.clone()),
                ],
            ),
            &spine(),
        );
        // Force a HARD with an empty live tail so the composed pair freezes anchor_mid = None.
        let loaded = s.load("none-anchor").unwrap();
        let mut meta = loaded.meta;
        meta.synthetic_todo = None;
        meta.last_render_config = "force a hard".to_string();
        s.commit("none-anchor", loaded.row_version, &loaded.core, &meta)
            .unwrap();
        let composed = run(
            &s,
            &req(
                "none-anchor",
                "cfg1",
                vec![
                    item("a", 1, "raw"),
                    todowrite_call("todo", 2, todos.clone()),
                ],
            ),
            &spine(),
        );
        assert_eq!(composed.action, "HARD");
        assert_eq!(tail_ids(&composed), Vec::<&str>::new());
        assert_eq!(synthetic_todo_index(&composed), 2);
        assert!(
            s.load("none-anchor")
                .unwrap()
                .meta
                .synthetic_todo
                .as_ref()
                .unwrap()
                .anchor_mid
                .is_none(),
            "the empty-tail compose must freeze anchor_mid = None"
        );
        let composed_prefix = prefix_through_synthetic_todo(&composed);

        // A defer that appends a new tail message (ordinal 3, above coverage 2).
        let defer = run(
            &s,
            &req(
                "none-anchor",
                "cfg1",
                vec![
                    item("a", 1, "raw"),
                    todowrite_call("todo", 2, todos),
                    item("t3", 3, "tail grew after the None-anchor compose"),
                ],
            ),
            &spine(),
        );

        assert_eq!(defer.action, "SOFT+");
        // The pair stays right after m0/m1; the new tail message lands AFTER it.
        assert_eq!(synthetic_todo_index(&defer), 2);
        assert!(message_index(&defer, "t3") > synthetic_todo_index(&defer));
        // The whole [m0, m1, pair] prefix is byte-identical to the compose pass.
        assert_eq!(prefix_through_synthetic_todo(&defer), composed_prefix);
    }

    #[test]
    fn synthetic_todo_defer_anchor_vanished_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("vanished", &[comp(1, 1, 1, "a", "SUMMARY")])
            .unwrap();
        let todos = json!([{ "content": "Anchor", "status": "pending", "priority": "high" }]);
        run(
            &s,
            &req(
                "vanished",
                "cfg0",
                vec![item("a", 1, "raw"), todowrite_call("todo", 2, todos)],
            ),
            &spine(),
        );
        let before = s.load("vanished").unwrap().row_version;

        let err = transform(
            &s,
            &req(
                "vanished",
                "cfg0",
                vec![item("a", 1, "raw"), item("later", 3, "tail without anchor")],
            ),
            &pctx("git:proj", "/nonexistent-docs", 0),
        )
        .unwrap_err();

        assert!(matches!(err, TransformError::SyntheticTodoAnchorMissing(mid) if mid == "todo"));
        assert_eq!(s.load("vanished").unwrap().row_version, before);
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
        assert_eq!(meta.coverage_start_ordinal, None);
        assert_eq!(meta.coverage_compartment_seq, None);
        assert_eq!(meta.expiry_cutoff_ms, 0);
        assert_eq!(meta.revert_epoch, 0);
        assert!(meta.last_recut.is_none());
        assert_eq!(meta.historian.expected_revert_epoch, 0);
        assert!(meta.synthetic_todo.is_none());
        assert!(!meta.cc_u1_active);
        assert!(meta.initialized);
    }

    // ===== slice 3: tail reducers =====

    fn reduce(target: &str, kind: &str, payload: &str) -> ReductionDecision {
        ReductionDecision {
            target_id: target_id(target),
            kind: kind.to_string(),
            payload: payload.to_string(),
        }
    }
    fn with_reductions(rs: Vec<ReductionDecision>) -> Vec<ReductionDecision> {
        rs
    }
    fn first_block_text(block: &ck_wire::CkWireBlock) -> Option<&str> {
        match &block.kind {
            ck_wire::CkKind::Text { text } => Some(text.as_str()),
            ck_wire::CkKind::ToolResult { output, .. } => match &output.kind {
                ck_wire::CkOutputKind::Text { text }
                | ck_wire::CkOutputKind::ErrorText { text } => Some(text.as_str()),
                ck_wire::CkOutputKind::Content { blocks } => blocks.iter().find_map(|block| {
                    if let ck_wire::ResultBlockKind::Text { text } = &block.kind {
                        Some(text.as_str())
                    } else {
                        None
                    }
                }),
                _ => None,
            },
            _ => None,
        }
    }
    /// The bytes of a tail item (non-synthetic) by id.
    fn tail_bytes<'a>(r: &'a TransformResponse, id: &str) -> &'a str {
        let msg = r
            .messages()
            .iter()
            .find(|m| !m.meta.synthetic && m.meta.harness_id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("no tail item {id}"));
        first_block_text(msg.content.first().unwrap()).unwrap()
    }

    /// Bootstrap a session whose m0 covers ordinal 1 (compartment ends at id "a"), so the
    /// boundary "a" is present and tail items (ordinal ≥ 2) are reducible.
    fn bootstrap_covering_a(s: &McStore) {
        s.replace_compartments("ses", &[comp(1, 1, 1, "a", "SUMMARY")])
            .unwrap();
        run(s, &req("ses", "cfg0", vec![item("a", 1, "raw")]), &spine());
    }

    #[test]
    fn tagging_gate_off_preserves_unreduced_golden_messages() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let response = run(
            &s,
            &req("inert", "cfg0", vec![item("m1", 1, "hello")]),
            &spine(),
        );
        assert_eq!(tail_bytes(&response, "m1"), "hello");
        assert!(s.load_tags_for_session("inert").unwrap().is_empty());
    }

    #[test]
    fn tagger_flip_hards_before_committed_identity_can_render_tags() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let mut request = cc_req("flip", "cfg0", vec![item("m1", 1, "hello")]);

        let before_flip = run(&s, &request, &spine());
        assert_eq!(before_flip.action, "HARD");
        assert_eq!(tail_bytes(&before_flip, "m1"), "hello");
        assert!(!s
            .load("flip")
            .unwrap()
            .meta
            .last_render_config
            .contains("tfe:"));

        request.tool_present = true;
        let transition = run(&s, &request, &spine());
        assert_eq!(transition.action, "HARD");
        assert_eq!(tail_bytes(&transition, "m1"), "hello");
        assert!(s.load_tags_for_session("flip").unwrap().is_empty());
        assert!(s
            .load("flip")
            .unwrap()
            .meta
            .last_render_config
            .contains("tfe:4:tfe1"));

        let after_commit = run(&s, &request, &spine());
        assert_eq!(after_commit.action, "SOFT+");
        assert_eq!(tail_bytes(&after_commit, "m1"), "§1§ hello");
        assert_eq!(s.load_tags_for_session("flip").unwrap().len(), 1);
    }

    #[test]
    fn tag_prefix_strip_is_a_byte_exact_inverse() {
        let corpus = [
            "",
            "plain",
            "  leading whitespace",
            "\t\nline",
            "§7§ forged other tag",
            "§42§ genuine same-looking content",
            "é日🙂",
            "§not-a-tag§ x",
        ];
        for content in corpus {
            let tagged = prepend_tag(42, content);
            assert_eq!(strip_tag_prefix(&tagged, 42).as_bytes(), content.as_bytes());
            if !content.starts_with("§42§ ") {
                assert_eq!(strip_tag_prefix(content, 42).as_bytes(), content.as_bytes());
            }
        }
        assert_eq!(
            strip_tag_prefix("§7§ forged other tag", 42),
            "§7§ forged other tag"
        );
        assert_eq!(
            strip_tag_prefix("§42§ §42§ user content", 42),
            "§42§ user content",
            "only one registered prefix occurrence is removed"
        );
    }

    #[test]
    fn mint_scope_matches_overlay_scope_and_captures_exact_source() {
        run_active_surface_test(|| {
            let content_text = ck_wire::ResultBlock {
                kind: ck_wire::ResultBlockKind::Text {
                    text: "  content §9§".to_string(),
                },
                provider_extras: ck_wire::ProviderExtras::new(),
            };
            let messages = vec![
                item("m1", 1, "  user §7§"),
                assistant_tool_call("call1", 2, "c1"),
                tool_result("text", 3, "c1", "  text output"),
                assistant_tool_call("call2", 4, "c2"),
                tool_result_with_output(
                    "error-text",
                    5,
                    "c2",
                    ck_wire::CkOutputKind::ErrorText {
                        text: "error output".to_string(),
                    },
                ),
                assistant_tool_call("call3", 6, "c3"),
                tool_result_with_output(
                    "json",
                    7,
                    "c3",
                    ck_wire::CkOutputKind::Json {
                        value: json!({"x": 1}),
                    },
                ),
                assistant_tool_call("call4", 8, "c4"),
                tool_result_with_output(
                    "error-json",
                    9,
                    "c4",
                    ck_wire::CkOutputKind::ErrorJson {
                        value: json!({"x": 2}),
                    },
                ),
                assistant_tool_call("call5", 10, "c5"),
                tool_result_with_output(
                    "denied",
                    11,
                    "c5",
                    ck_wire::CkOutputKind::ExecutionDenied {
                        reason: Some("no".to_string()),
                    },
                ),
                assistant_tool_call("call6", 12, "c6"),
                tool_result_with_output(
                    "content",
                    13,
                    "c6",
                    ck_wire::CkOutputKind::Content {
                        blocks: vec![content_text],
                    },
                ),
                assistant_tool_call("call7", 14, "c7"),
                tool_result_with_output(
                    "empty-content",
                    15,
                    "c7",
                    ck_wire::CkOutputKind::Content { blocks: Vec::new() },
                ),
            ];
            let request = active_cc_req("scope", "cfg0", messages.clone());
            let dir = tempfile::tempdir().unwrap();
            let s = store(dir.path());
            run(&s, &request, &spine());
            let response = run(&s, &request, &spine());
            let rows = s.load_tags_for_session("scope").unwrap();
            assert_eq!(
                rows.iter()
                    .map(|row| row.block_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["m1#0", "text#0", "error-text#0", "content#0"]
            );
            let projection = project_messages(&messages).unwrap();
            for row in &rows {
                let block = projection
                    .blocks
                    .iter()
                    .find(|block| block.id == row.block_id)
                    .unwrap();
                let (_, source) = taggable_source(block).expect("minted tags must be overlayable");
                assert_eq!(row.source_bytes, source.as_bytes());
            }
            assert_eq!(tail_bytes(&response, "m1"), "§1§   user §7§");
            assert_eq!(tail_bytes(&response, "text"), "§2§   text output");
            assert_eq!(tail_bytes(&response, "error-text"), "§3§ error output");
            assert_eq!(tail_bytes(&response, "content"), "§4§   content §9§");
        });
    }

    #[test]
    fn tag_minting_is_deterministic_monotonic_and_survives_rejected_pass() {
        run_active_surface_test(|| {
            let messages = vec![
                item("m1", 1, "hello"),
                assistant_tool_call("call", 2, "c1"),
                tool_result("result", 3, "c1", "tool output"),
            ];
            let dir_a = tempfile::tempdir().unwrap();
            let store_a = store(dir_a.path());
            run(
                &store_a,
                &active_cc_req("mint", "cfg0", messages.clone()),
                &spine(),
            );
            assert!(store_a.load_tags_for_session("mint").unwrap().is_empty());
            run(
                &store_a,
                &active_cc_req("mint", "cfg0", messages.clone()),
                &spine(),
            );
            let first = store_a.load_tags_for_session("mint").unwrap();
            assert_eq!(
                first
                    .iter()
                    .map(|row| (row.tag_number, row.block_id.as_str(), row.kind.as_str()))
                    .collect::<Vec<_>>(),
                vec![(1, "m1#0", "message"), (2, "result#0", "tool_result")]
            );
            run(
                &store_a,
                &active_cc_req("mint", "cfg0", messages.clone()),
                &spine(),
            );
            assert_eq!(store_a.load_tags_for_session("mint").unwrap(), first);

            let dir_b = tempfile::tempdir().unwrap();
            let store_b = store(dir_b.path());
            run(
                &store_b,
                &active_cc_req("mint", "cfg0", messages.clone()),
                &spine(),
            );
            run(&store_b, &active_cc_req("mint", "cfg0", messages), &spine());
            assert_eq!(store_b.load_tags_for_session("mint").unwrap(), first);

            let dir_c = tempfile::tempdir().unwrap();
            let store_c = store(dir_c.path());
            let stable = active_cc_req("reject", "cfg0", vec![item("m1", 1, "old")]);
            run(&store_c, &stable, &spine());
            run(&store_c, &stable, &spine());
            let err = transform(
                &store_c,
                &active_cc_req(
                    "reject",
                    "cfg0",
                    vec![item("m1", 1, "changed identity"), item("m2", 2, "new")],
                ),
                &pctx("git:proj", "/nonexistent-docs", 0),
            )
            .unwrap_err();
            assert!(matches!(err, TransformError::IdentityDrift(_)));
            let tags = store_c.load_tags_for_session("reject").unwrap();
            assert_eq!(
                tags.iter()
                    .map(|row| row.block_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["m1#0", "m2#0"]
            );
        });
    }

    #[test]
    fn tag_overlay_replays_stably_and_new_tail_gets_next_number() {
        run_active_surface_test(|| {
            let dir = tempfile::tempdir().unwrap();
            let s = store(dir.path());
            let first_req = active_cc_req("stable", "cfg0", vec![item("m1", 1, "alpha")]);
            let transition = run(&s, &first_req, &spine());
            assert_eq!(tail_bytes(&transition, "m1"), "alpha");
            let first = run(&s, &first_req, &spine());
            let replay = run(&s, &first_req, &spine());
            assert_eq!(tail_bytes(&first, "m1"), "§1§ alpha");
            assert_eq!(tail_bytes(&first, "m1"), tail_bytes(&replay, "m1"));

            let extended = run(
                &s,
                &active_cc_req(
                    "stable",
                    "cfg0",
                    vec![item("m1", 1, "alpha"), item("m2", 2, "beta")],
                ),
                &spine(),
            );
            assert_eq!(tail_bytes(&extended, "m1"), "§1§ alpha");
            assert_eq!(tail_bytes(&extended, "m2"), "§2§ beta");
        });
    }

    #[test]
    fn reduced_block_renders_placeholder_without_tag_prefix() {
        run_active_surface_test(|| {
            let dir = tempfile::tempdir().unwrap();
            let s = store(dir.path());
            s.replace_compartments("drop-tag", &[comp(1, 1, 1, "a", "SUMMARY")])
                .unwrap();
            let request = active_cc_req(
                "drop-tag",
                "cfg0",
                vec![
                    item("a", 1, "covered"),
                    item("m1", 2, "drop me"),
                    item("m2", 3, "keep me"),
                ],
            );
            run(&s, &request, &spine());
            let active = run(&s, &request, &spine());
            assert_eq!(tail_bytes(&active, "m1"), "§2§ drop me");
            let reductions = with_reductions(vec![reduce("m1#0", "drop", "[dropped]")]);
            let response = run(&s, &request, &reductions);
            assert_eq!(response.action, "SOFT");
            assert_eq!(tail_bytes(&response, "m1"), "[dropped §2§]");
            assert_eq!(tail_bytes(&response, "m2"), "§3§ keep me");
        });
    }

    #[test]
    fn channel1_nudge_replays_and_suppresses_refire() {
        run_active_surface_test(|| {
            let dir = tempfile::tempdir().unwrap();
            let s = store(dir.path());
            let huge = "word ".repeat(20_000);
            let messages = vec![
                assistant_tool_call("call1", 1, "c1"),
                tool_result("result1", 2, "c1", &huge),
                assistant_tool_call("call2", 3, "c2"),
                tool_result("result2", 4, "c2", &huge),
            ];
            let request = with_usage(active_cc_req("nudge", "cfg0", messages.clone()), 900, 1024);
            run(&s, &request, &spine());
            let first = run(&s, &request, &spine());
            let first_result = tail_bytes(&first, "result2").to_string();
            assert!(first_result.contains("<system-reminder>"));
            assert_eq!(s.load_channel1_appends("nudge").unwrap().len(), 1);

            let replay = run(&s, &request, &spine());
            assert_eq!(tail_bytes(&replay, "result2"), first_result);
            assert_eq!(s.load_channel1_appends("nudge").unwrap().len(), 1);

            let mut loaded = s.load("nudge").unwrap();
            loaded.meta.channel1_reduce_suppressed = true;
            s.commit("nudge", loaded.row_version, &loaded.core, &loaded.meta)
                .unwrap();
            let mut extended_messages = messages;
            extended_messages.push(assistant_tool_call("call3", 5, "c3"));
            extended_messages.push(tool_result("result3", 6, "c3", &huge));
            let suppressed = run(
                &s,
                &with_usage(active_cc_req("nudge", "cfg0", extended_messages), 900, 1024),
                &spine(),
            );
            assert!(tail_bytes(&suppressed, "result2").contains("<system-reminder>"));
            assert!(!tail_bytes(&suppressed, "result3").contains("<system-reminder>"));
            assert_eq!(s.load_channel1_appends("nudge").unwrap().len(), 1);
        });
    }

    #[test]
    fn subc_reversibility_false_pass_has_no_tag_bytes_in_any_durable_input() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store
            .replace_compartments("false-bytes", &[comp(1, 1, 1, "a", "summary")])
            .unwrap();
        let request = cc_req(
            "false-bytes",
            "cfg0",
            vec![item("a", 1, "covered"), item("b", 2, "live")],
        );

        let response = run(&store, &request, &spine());
        let loaded = store.load("false-bytes").unwrap();
        let compartments = store.load_compartments("false-bytes").unwrap();
        assert!(!compartments.is_empty());
        for durable_or_output in [
            serde_json::to_string(&request).unwrap(),
            serde_json::to_string(&response).unwrap(),
            serde_json::to_string(&loaded.core).unwrap(),
            serde_json::to_string(&loaded.meta).unwrap(),
            format!("{compartments:?}"),
        ] {
            assert!(!durable_or_output.contains('§'), "{durable_or_output}");
        }
        assert!(store
            .load_tags_for_session("false-bytes")
            .unwrap()
            .is_empty());
        assert_eq!(response.surface_state, SurfaceState::Inactive);
    }

    #[test]
    fn subc_reversibility_surface_flip_coordinates_exactly_one_hard() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let messages = vec![item("m1", 1, "hello")];
        let mut inactive = cc_req("surface-flip", "profile␟tf0␟gno-reduce", messages.clone());
        let initial = run(&store, &inactive, &spine());
        assert_eq!(initial.action, "HARD");
        assert_eq!(initial.surface_state, SurfaceState::Inactive);

        let active = active_cc_req("surface-flip", "profile␟tf1␟gfull", messages.clone());
        let on_transition = run(&store, &active, &spine());
        assert_eq!(on_transition.action, "HARD");
        assert_eq!(on_transition.surface_state, SurfaceState::Transition);
        let on_config = store.load("surface-flip").unwrap().meta.last_render_config;
        assert!(on_config.contains("tf1"));
        assert!(on_config.contains("gfull"));
        assert!(on_config.contains("tfe:4:tfe1"));
        let on_steady = run(&store, &active, &spine());
        assert_ne!(on_steady.action, "HARD");
        assert_eq!(on_steady.surface_state, SurfaceState::Active);

        inactive.render_config = "profile␟tf0␟gno-reduce".to_string();
        let off_transition = run(&store, &inactive, &spine());
        assert_eq!(off_transition.action, "HARD");
        assert_eq!(off_transition.surface_state, SurfaceState::Transition);
        let off_config = store.load("surface-flip").unwrap().meta.last_render_config;
        assert!(off_config.contains("tf0"));
        assert!(off_config.contains("gno-reduce"));
        assert!(!off_config.contains("tfe:"));
        let off_steady = run(&store, &inactive, &spine());
        assert_ne!(off_steady.action, "HARD");
        assert_eq!(off_steady.surface_state, SurfaceState::Inactive);

        assert!(active.tool_present && !inactive.tool_present);
    }

    #[test]
    fn subc_reversibility_pending_drop_survives_an_entire_false_window() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let messages = (1..=30)
            .map(|ordinal| item(&format!("m{ordinal}"), ordinal, "old live content"))
            .collect::<Vec<_>>();
        let mut inactive = cc_req("false-window", "cfg0", messages.clone());
        run(&store, &inactive, &spine());
        store
            .append_pending_agent_drops("false-window", &["m1#0".to_string()], 1)
            .unwrap();

        for pass in 1..=4 {
            inactive.render_config = format!("cfg{pass}");
            let response = run(&store, &with_usage(inactive.clone(), 99, 100), &spine());
            assert_eq!(response.action, "HARD");
            assert_eq!(
                frozen_red_payload(&store.load("false-window").unwrap().core, "m1#0"),
                None
            );
            assert_eq!(
                store
                    .load_pending_agent_drops("false-window")
                    .unwrap()
                    .len(),
                1
            );
        }

        let active = active_cc_req("false-window", &inactive.render_config, messages);
        let applied = run(&store, &active, &spine());
        assert_eq!(applied.action, "HARD");
        assert_eq!(
            frozen_red_payload(&store.load("false-window").unwrap().core, "m1#0"),
            Some("[dropped]")
        );
        assert!(store
            .load_pending_agent_drops("false-window")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn subc_reversibility_true_reduction_stays_canonical_after_false_flip() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store
            .replace_compartments("canonical-flip", &[comp(1, 1, 1, "a", "summary")])
            .unwrap();
        let active = active_cc_req(
            "canonical-flip",
            "cfg0",
            vec![item("a", 1, "covered"), item("m1", 2, "drop me")],
        );
        run(&store, &active, &spine());
        run(&store, &active, &spine());
        let reduced = run(&store, &active, &[reduce("m1#0", "drop", "[dropped]")]);
        assert_eq!(tail_bytes(&reduced, "m1"), "[dropped §2§]");
        assert_eq!(
            frozen_red_payload(&store.load("canonical-flip").unwrap().core, "m1#0"),
            Some("[dropped]")
        );

        let inactive = cc_req(
            "canonical-flip",
            "cfg0",
            vec![item("a", 1, "covered"), item("m1", 2, "drop me")],
        );
        let transition = run(&store, &inactive, &spine());
        assert_eq!(transition.action, "HARD");
        assert_eq!(transition.surface_state, SurfaceState::Transition);
        assert_eq!(tail_bytes(&transition, "m1"), "[dropped]");
        let loaded = store.load("canonical-flip").unwrap();
        assert_eq!(frozen_red_payload(&loaded.core, "m1#0"), Some("[dropped]"));
        assert!(loaded
            .core
            .frozen_units
            .iter()
            .any(|unit| unit.key == "red:m1#0"));
    }

    #[test]
    fn newest_tag_block_set_isolates_protected_and_applied_pending_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        // The protected set is the newest 20 ACTIVE tags as exact block ids.
        // This fixture is built so every cheaper implementation class fails at
        // the rank-20 boundary itself, not just at the extremes:
        //   - ACTIVE numbers have holes: m23#0 is stale-provenance and three
        //     "ghost" rows hold the TOP numbers (26-28) for blocks absent from
        //     the array, so any numeric-threshold cutoff (from the active max
        //     or the global max) lands on the wrong rows.
        //   - Ordinal 24 carries TWO tagged blocks (m24#0, m24#1), so
        //     one-block-per-ordinal counting shifts the boundary by one.
        // Active numbers: {1..22, 24, 25} (24 rows). Newest 20 by number:
        // {5..22, 24, 25} — the boundary pair is number 5 (rank 20, protected)
        // vs number 4 (rank 21, applied), and both carry pending drops.
        let mut messages = (1..=24)
            .map(|ordinal| item(&format!("m{ordinal}"), ordinal, &format!("text {ordinal}")))
            .collect::<Vec<_>>();
        messages[23] = two_block_item("m24", 24, "text 24", "attachment 24");

        let mut tags = (1..=24)
            .map(|ordinal| TagMintInput {
                block_id: format!("m{ordinal}#0"),
                kind: "message".to_string(),
                token_count: 1,
                source_bytes: if ordinal == 23 {
                    // Stale provenance: stored bytes no longer match the live
                    // carrier, so this row must not occupy a protected slot.
                    b"text from a previous life".to_vec()
                } else {
                    format!("text {ordinal}").into_bytes()
                },
            })
            .collect::<Vec<_>>();
        tags.push(TagMintInput {
            block_id: "m24#1".to_string(),
            kind: "message".to_string(),
            token_count: 1,
            source_bytes: b"attachment 24".to_vec(),
        });
        for ghost in 1..=3 {
            tags.push(TagMintInput {
                block_id: format!("ghost{ghost}#0"),
                kind: "message".to_string(),
                token_count: 1,
                source_bytes: b"ghost".to_vec(),
            });
        }
        store.mint_or_get_tags("protected", &tags, 1).unwrap();
        store
            .append_pending_agent_drops(
                "protected",
                &["m4#0".to_string(), "m5#0".to_string(), "m24#1".to_string()],
                99,
            )
            .unwrap();

        let response = run(
            &store,
            &active_cc_req("protected", "cfg0", messages),
            &spine(),
        );
        assert_eq!(response.action, "HARD");
        let loaded = store.load("protected").unwrap();
        // Rank 21 (number 4) is just outside the protected set: applied.
        assert_eq!(frozen_red_payload(&loaded.core, "m4#0"), Some("[dropped]"));
        // Rank 20 (number 5) is the last protected slot: retained. Under a
        // threshold cutoff (active-max 25 - 20, or global-max 28 - 20) or
        // one-per-ordinal counting this row loses protection and applies.
        assert_eq!(frozen_red_payload(&loaded.core, "m5#0"), None);
        // The second block on ordinal 24 is itself active and protected; an
        // implementation that counts one block per ordinal applies this drop.
        assert_eq!(frozen_red_payload(&loaded.core, "m24#1"), None);
        let mut retained = store
            .load_pending_agent_drops("protected")
            .unwrap()
            .into_iter()
            .map(|row| row.target_id)
            .collect::<Vec<_>>();
        retained.sort();
        assert_eq!(retained, vec!["m24#1".to_string(), "m5#0".to_string()]);
    }

    #[test]
    fn frozen_row_does_not_consume_a_protection_slot() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        // 21 tagged live blocks, numbers 1..21. m21#0 is frozen by an earlier
        // active pass, so the ACTIVE set is exactly m1..m20 — all 20 fit the
        // protected window and the oldest row's pending drop must be retained.
        // If frozen rows still consumed slots, m21 would take one, m1 would
        // fall to rank 21, and its drop would apply.
        let messages = (1..=21)
            .map(|ordinal| item(&format!("m{ordinal}"), ordinal, &format!("text {ordinal}")))
            .collect::<Vec<_>>();
        let tags = (1..=21)
            .map(|ordinal| TagMintInput {
                block_id: format!("m{ordinal}#0"),
                kind: "message".to_string(),
                token_count: 1,
                source_bytes: format!("text {ordinal}").into_bytes(),
            })
            .collect::<Vec<_>>();
        store.mint_or_get_tags("slot-free", &tags, 1).unwrap();

        // Pass 1 bootstraps, then the freeze is seeded directly in durable state
        // (selection itself refuses to reduce a protected newest tag, so a
        // pre-existing freeze — e.g. from an earlier phase with more tags — is
        // the realistic way a high-ranked block arrives already frozen).
        let request = active_cc_req("slot-free", "cfg0", messages);
        run(&store, &request, &spine());
        let seeded = store.load("slot-free").unwrap();
        let mut core = seeded.core.clone();
        core.frozen_units
            .push(red_unit("m21#0", "drop", "[dropped]"));
        store
            .commit("slot-free", seeded.row_version, &core, &seeded.meta)
            .unwrap();
        assert_eq!(
            frozen_red_payload(&store.load("slot-free").unwrap().core, "m21#0"),
            Some("[dropped]"),
            "precondition: m21#0 frozen before the tested pass"
        );

        // Pass 2 (tested): a render-config change forces a producing HARD so the
        // pending row is genuinely selected against the protected set — a defer
        // pass would retain it regardless and prove nothing.
        store
            .append_pending_agent_drops("slot-free", &["m1#0".to_string()], 99)
            .unwrap();
        let producing = active_cc_req(
            "slot-free",
            "cfg1",
            (1..=21)
                .map(|ordinal| item(&format!("m{ordinal}"), ordinal, &format!("text {ordinal}")))
                .collect::<Vec<_>>(),
        );
        let tested = run(&store, &producing, &spine());
        assert_eq!(tested.action, "HARD", "tested pass must produce");
        let loaded = store.load("slot-free").unwrap();
        assert_eq!(frozen_red_payload(&loaded.core, "m1#0"), None);
        let pending = store.load_pending_agent_drops("slot-free").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target_id, "m1#0");
    }

    #[test]
    fn newest_tag_block_set_excludes_stale_provenance_from_slots() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        // Exactly 21 tagged live blocks. The NEWEST row (m21#0) has stale
        // provenance. Under exact semantics it cannot hold a slot, so the
        // protected 20 are m20..m1 and a pending drop on m1#0 (rank 21 by
        // number, rank 20 among ACTIVE rows) is PROTECTED. An implementation
        // that skips the provenance re-check frees m1#0 and applies the drop.
        let messages = (1..=21)
            .map(|ordinal| item(&format!("m{ordinal}"), ordinal, &format!("text {ordinal}")))
            .collect::<Vec<_>>();
        let tags = (1..=21)
            .map(|ordinal| TagMintInput {
                block_id: format!("m{ordinal}#0"),
                kind: "message".to_string(),
                token_count: 1,
                source_bytes: if ordinal == 21 {
                    b"stale bytes".to_vec()
                } else {
                    format!("text {ordinal}").into_bytes()
                },
            })
            .collect::<Vec<_>>();
        store.mint_or_get_tags("stale-slot", &tags, 1).unwrap();
        store
            .append_pending_agent_drops("stale-slot", &["m1#0".to_string()], 2)
            .unwrap();

        let response = run(
            &store,
            &active_cc_req("stale-slot", "cfg0", messages),
            &spine(),
        );
        assert_eq!(response.action, "HARD");
        let loaded = store.load("stale-slot").unwrap();
        assert_eq!(frozen_red_payload(&loaded.core, "m1#0"), None);
        let pending = store.load_pending_agent_drops("stale-slot").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target_id, "m1#0");
    }

    #[test]
    fn dormant_forced_hard_retains_pending_then_active_hard_applies_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let messages = vec![item("a", 1, "drop me")];
        let inactive = cc_req("dormant-hard", "cfg0", messages.clone());
        run(&store, &inactive, &spine());
        store
            .append_pending_agent_drops("dormant-hard", &["a#0".to_string()], 1)
            .unwrap();
        let forced = cc_req("dormant-hard", "cfg1", messages.clone());
        let dormant = run(&store, &forced, &spine());
        assert_eq!(dormant.action, "HARD");
        assert_eq!(
            frozen_red_payload(&store.load("dormant-hard").unwrap().core, "a#0"),
            None
        );
        assert_eq!(
            store
                .load_pending_agent_drops("dormant-hard")
                .unwrap()
                .len(),
            1
        );

        let active = run(
            &store,
            &active_cc_req("dormant-hard", "cfg1", messages),
            &spine(),
        );
        assert_eq!(active.action, "HARD");
        assert_eq!(
            frozen_red_payload(&store.load("dormant-hard").unwrap().core, "a#0"),
            Some("[dropped]")
        );
        assert!(store
            .load_pending_agent_drops("dormant-hard")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn obsolete_pending_row_commits_consumption_without_core_or_meta_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        // "gone" is PRESENT in the array but covered by the compartment boundary:
        // provably retired (coverage only advances), so its pending row is
        // consumable even though nothing else about the pass changes state.
        store
            .replace_compartments("consume-only", &[comp(1, 1, 1, "gone", "summary")])
            .unwrap();
        let request = cc_req(
            "consume-only",
            "cfg0",
            vec![item("gone", 1, "folded away"), item("live", 2, "hello")],
        );
        run(&store, &request, &spine());
        store
            .append_pending_agent_drops("consume-only", &["gone#0".to_string()], 1)
            .unwrap();
        let before = store.load("consume-only").unwrap();

        let response = run(&store, &request, &spine());
        let after = store.load("consume-only").unwrap();
        assert_eq!(response.action, "SOFT+");
        assert!(response.committed);
        assert_eq!(after.core, before.core);
        assert_eq!(after.meta, before.meta);
        assert_eq!(
            after.row_version,
            before.row_version.map(|version| version + 1)
        );
        assert!(store
            .load_pending_agent_drops("consume-only")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn absent_target_pending_row_survives_subset_pass() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let full = active_cc_req(
            "subset-safety",
            "cfg0",
            vec![item("a", 1, "first"), item("m9", 9, "drop me")],
        );
        run(&store, &full, &spine());
        store
            .append_pending_agent_drops("subset-safety", &["m9#0".to_string()], 1)
            .unwrap();

        // An interactive side-request arrives as a SUBSET of the session: the
        // pending target is absent from this pass entirely. Absence proves
        // nothing (the full array returns next pass), so the row must survive.
        let subset = active_cc_req("subset-safety", "cfg0", vec![item("a", 1, "first")]);
        run(&store, &subset, &spine());
        let survived = store.load_pending_agent_drops("subset-safety").unwrap();
        assert_eq!(survived.len(), 1, "absent-target row must not be consumed");
        assert_eq!(survived[0].target_id, "m9#0");

        // The full array returns: the target is live-tail again and its row is
        // still queued for the next producing pass (drop application itself is
        // covered by the dormant/active drain tests).
        run(&store, &full, &spine());
        let requeued = store.load_pending_agent_drops("subset-safety").unwrap();
        assert_eq!(requeued.len(), 1);
        assert_eq!(requeued[0].target_id, "m9#0");
    }

    #[test]
    fn owned_profile_drains_pending_drops_without_request_surface() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        // Owned-leg consumers never send the reduction-surface field; their
        // profile default alone must keep the durable queue draining.
        let request = req(
            "owned-drain",
            "cfg0",
            vec![item("a", 1, "first"), item("m2", 2, "drop me")],
        );
        run(&store, &request, &spine());
        store
            .append_pending_agent_drops("owned-drain", &["m2#0".to_string()], 1)
            .unwrap();

        let producing = req(
            "owned-drain",
            "cfg1",
            vec![item("a", 1, "first"), item("m2", 2, "drop me")],
        );
        let response = run(&store, &producing, &spine());
        assert_eq!(tail_bytes(&response, "m2"), "[dropped]");
        assert!(
            store
                .load_pending_agent_drops("owned-drain")
                .unwrap()
                .is_empty(),
            "profile-default tail reclaim must drain the queue"
        );
    }

    #[test]
    fn mixed_legacy_numbered_and_new_canonical_freezes_replay_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store
            .replace_compartments("mixed-freezes", &[comp(1, 1, 1, "a", "summary")])
            .unwrap();
        let request = active_cc_req(
            "mixed-freezes",
            "cfg0",
            vec![
                item("a", 1, "covered"),
                item("m1", 2, "legacy"),
                item("m2", 3, "new"),
            ],
        );
        run(&store, &request, &spine());
        run(&store, &request, &spine());

        let mut loaded = store.load("mixed-freezes").unwrap();
        loaded
            .core
            .frozen_units
            .push(red_unit("m1#0", "drop", "[dropped §7§]"));
        store
            .commit(
                "mixed-freezes",
                loaded.row_version,
                &loaded.core,
                &loaded.meta,
            )
            .unwrap();

        let response = run(&store, &request, &[reduce("m2#0", "drop", "[dropped]")]);
        assert_eq!(tail_bytes(&response, "m1"), "[dropped §7§]");
        assert_eq!(tail_bytes(&response, "m2"), "[dropped §3§]");
        let durable = store.load("mixed-freezes").unwrap().core;
        assert_eq!(frozen_red_payload(&durable, "m1#0"), Some("[dropped §7§]"));
        assert_eq!(frozen_red_payload(&durable, "m2#0"), Some("[dropped]"));
    }

    #[test]
    fn absent_tool_matches_explicit_false_across_profile_pass_matrix() {
        for profile in [
            SerializerProfile::ClaudeCodeAnthropic,
            SerializerProfile::OwnedBroca,
            SerializerProfile::Pi,
            SerializerProfile::OpencodeAiSdk,
        ] {
            let left_dir = tempfile::tempdir().unwrap();
            let right_dir = tempfile::tempdir().unwrap();
            let left = store(left_dir.path());
            let right = store(right_dir.path());
            let initial = vec![comp(1, 1, 1, "a", "first")];
            left.replace_compartments("identity", &initial).unwrap();
            right.replace_compartments("identity", &initial).unwrap();
            let messages = vec![
                item("a", 1, "one"),
                item("b", 2, "two"),
                item("c", 3, "three"),
            ];
            // Both arms must deserialize from the wire (a direct struct skips the
            // original-value retention that wire requests carry), but they must be
            // constructed INDEPENDENTLY: the explicit arm's JSON carries a literal
            // tool_present:false, the absent arm's JSON omits the key entirely.
            // Cloning the deserialized absent request instead would inherit
            // whatever default serde applied, making the comparison tautological
            // under a wrong default.
            let (absent, explicit) = wire_pair_absent_and_explicit_false(profile_req(
                profile,
                "identity",
                "cfg0",
                messages.clone(),
            ));

            for expected_action in ["HARD", "SOFT+"] {
                let left_response = run(&left, &absent, &spine());
                let right_response = run(&right, &explicit, &spine());
                assert_eq!(left_response.action, expected_action, "{profile:?}");
                assert_eq!(
                    serde_json::to_value(left_response).unwrap(),
                    serde_json::to_value(right_response).unwrap(),
                    "{profile:?}"
                );
            }

            let extended = vec![comp(1, 1, 1, "a", "first"), comp(2, 2, 2, "b", "second")];
            left.replace_compartments("identity", &extended).unwrap();
            right.replace_compartments("identity", &extended).unwrap();
            let left_soft = run(&left, &absent, &spine());
            let right_soft = run(&right, &explicit, &spine());
            assert_eq!(left_soft.action, "SOFT", "{profile:?}");
            assert_eq!(
                serde_json::to_value(left_soft).unwrap(),
                serde_json::to_value(right_soft).unwrap(),
                "{profile:?}"
            );

            // Rebuild BOTH arms independently for the fold leg too: mutating one
            // and cloning the other would reintroduce the shared-default hazard.
            let (absent, explicit_fold) = wire_pair_absent_and_explicit_false(profile_req(
                profile,
                "identity",
                "cfg1",
                messages.clone(),
            ));
            let left_fold = run(&left, &absent, &spine());
            let right_fold = run(&right, &explicit_fold, &spine());
            assert_eq!(left_fold.action, "HARD", "{profile:?}");
            assert_eq!(
                serde_json::to_value(left_fold).unwrap(),
                serde_json::to_value(right_fold).unwrap(),
                "{profile:?}"
            );
            assert_eq!(
                left.load("identity").unwrap().core,
                right.load("identity").unwrap().core
            );
            assert_eq!(
                left.load("identity").unwrap().meta,
                right.load("identity").unwrap().meta
            );
        }
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
            assert!(
                r.committed,
                "first-seen tail mids persist identity vectors even on a defer"
            );
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
        // A compartment now covers ordinal 2, summarizing t2. A later HARD pass
        // re-composes m0 and removes red:t2#0 because its ordinal is now covered.
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
        assert_eq!(r.boundary_id, "t2#0", "anchor = last compartment end id");
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
            !reloaded
                .core
                .frozen_units
                .iter()
                .any(|u| u.key == "red:t2#0"),
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
    fn reverted_orphan_reduction_gcd_on_surviving_prefix_reconcile_hard() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "a", "S1"), comp(2, 2, 2, "b", "S2")])
            .unwrap();
        let live = vec![
            item("a", 1, "raw a"),
            item("b", 2, "raw b"),
            item("t3", 3, "OUT"),
        ];
        let boot = run(&s, &req("ses", "cfg0", live.clone()), &spine());
        assert_eq!(boot.action, "HARD");
        assert_eq!(boot.boundary_id, "b#0");

        // Freeze a drop on t3. The later revert keeps compartment a, so this exercises
        // the surviving-prefix re-cut path rather than the share-nothing raw alarm.
        let d = with_reductions(vec![reduce("t3", "drop", "[dropped 1]")]);
        let soft = run(&s, &req("ses", "cfg0", live), &d);
        assert_eq!(soft.action, "SOFT");
        assert_eq!(tail_bytes(&soft, "t3"), "[dropped 1]");

        let reverted = vec![item("a", 1, "raw a"), item("z", 9, "other")];
        let revert = run(&s, &req("ses", "cfg0", reverted.clone()), &spine());
        assert_eq!(revert.action, "SOFT+");
        assert!(revert.reconcile_pending);

        let remat = run(&s, &req("ses", "cfg0", reverted), &spine());
        assert_eq!(remat.action, "HARD");
        assert_eq!(remat.boundary_id, "a#0");
        assert!(
            !m0_bytes(&remat).contains("[dropped 1]"),
            "no orphaned reduction in m0"
        );
        let reloaded = s.load("ses").unwrap();
        assert_eq!(s.load_compartments("ses").unwrap().len(), 1);
        assert!(
            !reloaded
                .core
                .frozen_units
                .iter()
                .any(|u| u.key == "red:t3#0"),
            "orphan red:t3#0 GC'd"
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
        let mut ctx = pctx("git:proj", "/nonexistent-docs", 0);
        ctx.injected_reductions = bad;
        let err = transform(&s, &req("ses", "cfg0", items), &ctx).unwrap_err();
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
            boundary_id: "a#0".into(),
            frozen_units: vec![
                synth_region("m0", "BASE".into()),
                red_unit("t2#0", "drop", "[dropped 1]"),
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
        let err = transform(&s, &req("ses", "cfg0", vec![item("a", 1, "BASE")]), &dc).unwrap_err();
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
            boundary_id: "a#0".into(),
            frozen_units: vec![
                synth_region("m0", "BASE".into()),
                synth_region("m1", M1_PLACEHOLDER.into()),
                red_unit("t2#0", "drop", "[dropped 1]"),
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
                memory_render_epoch: format!("mre{}", crate::MEMORY_RENDER_FORMAT_EPOCH),
                compartment_render_epoch: format!("cre{}", crate::COMPARTMENT_RENDER_FORMAT_EPOCH),
                profile_render_epoch: String::new(),
                tagger_feature_epoch: String::new(),
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
        let ok = transform(&s, &req("ses2", "cfg0", vec![item("a", 1, "BASE")]), &dc).unwrap();
        assert_eq!(ok.action, "SOFT+", "m0+m1+red is a valid shape");
    }

    #[test]
    fn token_estimator_is_hard_only_never_called_on_soft_or_defer() {
        // The load-bearing cache claim behind wiring the real BPE estimator: it is
        // reachable ONLY on the HARD m0 compose (the decay budget guard), never on a
        // SOFT (m1 composes at fixed tier 1) or a defer (frozen replay). If it were
        // ever called on a non-HARD pass, activating a real (non-zero) estimator could
        // change bytes on a pass that must replay byte-identically. Prove it with a
        // call-counting estimator: the counter must be >0 after a HARD and EXACTLY 0
        // after a SOFT and a defer.
        use std::cell::Cell;
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let calls = Cell::new(0usize);
        let counting = |text: &str| -> usize {
            calls.set(calls.get() + 1);
            mc_tokenizer::estimate_tokens(text)
        };
        let ctx = pctx("git:proj", "/nonexistent-docs", 0);

        // HARD bootstrap: m0 folds C1, so compose_m0_from_store runs the decay renderer
        // whose budget guard evaluates the estimator at least once (non-empty pool).
        s.replace_compartments("ses", &[comp(1, 1, 10, "m10", "S1")])
            .unwrap();
        let boot = apply_once_with_estimator(
            &s,
            &req(
                "ses",
                "cfg0",
                vec![item("m10", 10, "raw"), item("t11", 11, "tail")],
            ),
            &ctx,
            counting,
        )
        .unwrap();
        assert_eq!(boot.response.action, "HARD");
        assert!(
            calls.get() > 0,
            "the HARD m0 compose must exercise the estimator (budget guard)"
        );

        // SOFT: a second compartment rides m1 at fixed tier 1 (no decay budget guard).
        s.replace_compartments(
            "ses",
            &[comp(1, 1, 10, "m10", "S1"), comp(2, 11, 20, "m20", "S2")],
        )
        .unwrap();
        calls.set(0);
        let soft_items = vec![
            item("m10", 10, "raw"),
            item("m20", 20, "raw2"),
            item("t21", 21, "tail"),
        ];
        let soft =
            apply_once_with_estimator(&s, &req("ses", "cfg0", soft_items.clone()), &ctx, counting)
                .unwrap();
        assert_eq!(soft.response.action, "SOFT");
        assert_eq!(
            calls.get(),
            0,
            "a SOFT composes m1 without the m0 decay budget guard → estimator must NOT be called"
        );

        // defer: replays frozen m0/m1, composes nothing.
        calls.set(0);
        let defer =
            apply_once_with_estimator(&s, &req("ses", "cfg0", soft_items), &ctx, counting).unwrap();
        assert_eq!(defer.response.action, "SOFT+");
        assert_eq!(
            calls.get(),
            0,
            "a defer replays frozen m0/m1 → estimator must NOT be called"
        );
    }
    fn profile_req(
        profile: SerializerProfile,
        session: &str,
        cfg: &str,
        messages: Vec<CkIngressMessage>,
    ) -> TransformRequest {
        let mut r = req(session, cfg, messages);
        r.serializer_profile = profile.wire_id().to_string();
        r
    }

    fn cc_req(session: &str, cfg: &str, messages: Vec<CkIngressMessage>) -> TransformRequest {
        profile_req(
            SerializerProfile::ClaudeCodeAnthropic,
            session,
            cfg,
            messages,
        )
    }

    fn active_cc_req(
        session: &str,
        cfg: &str,
        messages: Vec<CkIngressMessage>,
    ) -> TransformRequest {
        let mut request = cc_req(session, cfg, messages);
        request.tool_present = true;
        request
    }

    fn effective_render_config_with_epochs(
        store: &McStore,
        cfg: &str,
        memory_render_epoch: String,
        compartment_render_epoch: String,
        profile_render_epoch: String,
        tagger_feature_epoch: String,
    ) -> String {
        fold_m0_content_epoch(
            cfg,
            &M0ContentEpoch {
                workspace_fingerprint: store.workspace_fingerprint("git:proj").unwrap(),
                upgrade_state: String::new(),
                memory_content_epoch: String::new(),
                memory_render_epoch,
                compartment_render_epoch,
                profile_render_epoch,
                tagger_feature_epoch,
            },
        )
    }

    fn global_epoch_effective_render_config(store: &McStore, cfg: &str) -> String {
        effective_render_config_with_epochs(
            store,
            cfg,
            format!("mre{}", crate::MEMORY_RENDER_FORMAT_EPOCH),
            format!("cre{}", crate::COMPARTMENT_RENDER_FORMAT_EPOCH),
            String::new(),
            String::new(),
        )
    }

    /// The byte-splice consumer keeps tail bytes verbatim, so the module must not
    /// mutate the tail for its profile under ANY pass class: mutations it froze
    /// would never reach the real context (phantom reclaim). Drives execute-class
    /// and emergency-class passes over reclaim-eligible content and asserts the
    /// output tail is byte-identical to the input on every pass.
    #[test]
    fn verbatim_tail_profile_never_mutates_tail_bytes_on_any_pass_class() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1", "SUMMARY")])
            .unwrap();

        let tail = vec![
            item("m1", 1, "covered raw"),
            assistant_tool_call("m2", 2, "call_old"),
            tool_result("m3", 3, "call_old", "big old tool output that age-drops"),
            todowrite_call(
                "m4",
                4,
                serde_json::json!([{ "content": "x", "status": "done" }]),
            ),
            item("m9", 9, "newest user text"),
        ];
        // A queued agent drop targeting the old tool result: appendable always,
        // consumable never (for this profile).
        s.append_pending_agent_drops("ses", &["m3#0".to_string()], 1)
            .unwrap();

        let expected_tail: Vec<Vec<u8>> = tail[1..]
            .iter()
            .map(|m| serde_json::to_vec(&m.ck).unwrap())
            .collect();

        // Bootstrap fold (HARD), then execute-class (70%) and emergency-class (96%)
        // passes: each must leave tail bytes untouched and consume nothing.
        let ctx = pctx("git:proj", "/nonexistent-docs", 0);
        for (usage, limit) in [(1u64, 100u64), (70, 100), (96, 100)] {
            let r = transform(
                &s,
                &with_usage(cc_req("ses", "cfg0", tail.clone()), usage, limit),
                &ctx,
            )
            .unwrap();
            let out_tail: Vec<Vec<u8>> = r
                .messages()
                .iter()
                .filter(|m| !m.meta.synthetic)
                .map(|m| serde_json::to_vec(m).unwrap())
                .collect();
            assert_eq!(
                out_tail, expected_tail,
                "tail bytes must be verbatim at usage {usage}%"
            );
        }

        let loaded = s.load("ses").unwrap();
        assert!(
            loaded
                .core
                .frozen_units
                .iter()
                .all(|u| !u.key.starts_with("red:")),
            "no reduction may freeze under a verbatim-tail profile"
        );
        assert!(
            loaded.meta.synthetic_todo.is_none(),
            "no synthetic todo may be captured under a verbatim-tail profile"
        );
        assert_eq!(
            s.load_pending_agent_drops("ses").unwrap().len(),
            1,
            "queued agent drops stay durable (append accepted, drain gated)"
        );
        // The FOLD is not a tail mutation: the prefix compacted normally.
        assert!(loaded.core.frozen_units.iter().any(|u| u.key == "m0"));
    }

    /// A leading system message is exempt from coverage continuity checks: the chunk
    /// builder never summarizes system content, so a pinned prompt before the first
    /// chunk is not a live-coverage gap.
    fn declared_trim_fixture() -> (
        tempfile::TempDir,
        McStore,
        TransformRequest,
        ProducerContext<'static>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let core = CoreState {
            version: 1,
            boundary_id: "b#0".to_string(),
            reconcile_pending: false,
            frozen_units: vec![
                synth_region("m0", "m0".to_string()),
                synth_region("m1", M1_PLACEHOLDER.to_string()),
            ],
            pending_changes: Vec::new(),
        };
        let meta = ModuleMeta {
            initialized: true,
            last_render_config: fold_m0_content_epoch(
                "cfg",
                &M0ContentEpoch {
                    workspace_fingerprint: String::new(),
                    upgrade_state: String::new(),
                    memory_content_epoch: String::new(),
                    memory_render_epoch: format!("mre{}", crate::MEMORY_RENDER_FORMAT_EPOCH),
                    compartment_render_epoch: format!(
                        "cre{}",
                        crate::COMPARTMENT_RENDER_FORMAT_EPOCH
                    ),
                    profile_render_epoch: String::new(),
                    tagger_feature_epoch: String::new(),
                },
            ),
            coverage_ordinal: Some(0),
            folded_compartment_seq: 0,
            m1_revision: m1_revision_signal(&store, "git:proj", "decl").unwrap(),
            ..Default::default()
        };
        store.commit("decl", None, &core, &meta).unwrap();
        store
            .replace_compartments("decl", &[comp(0, 0, 0, "b", "summary")])
            .unwrap();
        let mut request = req("decl", "cfg", vec![item("c", 1, "tail")]);
        request.declared_trim = Some(DeclaredTrim {
            flat_boundary_id: "b#0".to_string(),
            boundary_bare_message_id: "b".to_string(),
            boundary_absolute_ordinal: 0,
            next_absolute_ordinal: 1,
        });
        (
            dir,
            store,
            request,
            pctx("git:proj", "/nonexistent-docs", 0),
        )
    }

    #[test]
    fn seeded_boundary_validates_declared_trim_before_the_first_shadow_fold() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let compartments = vec![StoredCompartment {
            sequence: 4,
            start_message: 1,
            end_message: 2,
            start_message_id: "a#0".to_string(),
            end_message_id: "b#0".to_string(),
            title: "seeded".to_string(),
            content: "seeded summary".to_string(),
            p1: Some("seeded summary".to_string()),
            importance: 50,
            ..Default::default()
        }];
        store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: "seeded-trim",
                shadow_project_path: "git:proj",
                shadow_generation: 0,
                expected_shadow_seq: 0,
                seed_boundary_id: Some("b#0"),
                compartments: &compartments,
                memories: &[],
                memory_mutations: &[],
                workspace: None,
                last_todo_state: None,
                acked_watermarks: Value::Null,
            })
            .unwrap();
        let seeded = store.load("seeded-trim").unwrap();
        assert_eq!(seeded.core.boundary_id, "b#0");
        assert_eq!(seeded.meta.coverage_ordinal, Some(2));
        assert_eq!(seeded.meta.coverage_start_ordinal, Some(1));
        assert_eq!(seeded.meta.coverage_compartment_seq, Some(4));
        assert_eq!(seeded.meta.folded_compartment_seq, 4);

        let mut request = req("seeded-trim", "cfg", vec![item("c", 3, "live tail")]);
        request.declared_trim = Some(DeclaredTrim {
            flat_boundary_id: "b#0".to_string(),
            boundary_bare_message_id: "b".to_string(),
            boundary_absolute_ordinal: 2,
            next_absolute_ordinal: 3,
        });
        let ctx = pctx("git:proj", dir.path().to_str().unwrap(), 1);
        let first = transform_with_projection(&store, &request, &ctx).unwrap();
        assert_eq!(first.boundary_state, BoundaryState::DeclaredTrimValidated);
        assert_eq!(first.trim_mismatch, None);
        assert!(!store.load("seeded-trim").unwrap().meta.shadow_quarantined);

        let second = transform_with_projection(&store, &request, &ctx).unwrap();
        assert_eq!(second.boundary_state, BoundaryState::DeclaredTrimValidated);
        assert_eq!(first.response.ck_messages, second.response.ck_messages);
    }

    #[test]
    fn declared_trim_validates_absent_boundary_and_preserves_defer_path() {
        let (_dir, store, request, ctx) = declared_trim_fixture();
        let result = transform_with_projection(&store, &request, &ctx).unwrap();
        assert_eq!(result.boundary_state, BoundaryState::DeclaredTrimValidated);
        assert!(result.trim_mismatch.is_none());
        assert_eq!(result.response.action, "SOFT+");
        assert_eq!(store.load("decl").unwrap().meta.pending_rewrite, None);
    }

    #[test]
    fn declared_trim_predicate_failures_are_absent_with_trim_mismatch() {
        type TrimCase = (
            &'static str,
            Box<dyn FnOnce(&mut TransformRequest, &McStore)>,
        );
        let cases: Vec<TrimCase> = vec![
            (
                "boundary_identity",
                Box::new(|request, _| {
                    request.declared_trim.as_mut().unwrap().flat_boundary_id =
                        "wrong#0".to_string();
                }),
            ),
            (
                "coverage_ordinal",
                Box::new(|request, _| {
                    request
                        .declared_trim
                        .as_mut()
                        .unwrap()
                        .boundary_absolute_ordinal = 99;
                }),
            ),
            (
                "tail_compartment",
                Box::new(|_, store| {
                    store
                        .replace_compartments("decl", &[comp(0, 0, 0, "other", "summary")])
                        .unwrap();
                }),
            ),
            (
                "continuity",
                Box::new(|request, _| {
                    request
                        .declared_trim
                        .as_mut()
                        .unwrap()
                        .next_absolute_ordinal = 2;
                }),
            ),
        ];
        for (predicate, mutate) in cases {
            let (_dir, store, mut request, ctx) = declared_trim_fixture();
            mutate(&mut request, &store);
            let result = transform_with_projection(&store, &request, &ctx).unwrap();
            assert_eq!(result.boundary_state, BoundaryState::Absent, "{predicate}");
            assert_eq!(
                result.trim_mismatch.as_ref().map(|m| m.predicate),
                Some(predicate),
                "{predicate}"
            );
        }
    }

    #[test]
    fn declared_trim_continuity_exempts_covered_system_head() {
        let (_dir, store, mut request, ctx) = declared_trim_fixture();
        request
            .messages
            .insert(0, system_item("sys", 0, "covered system"));
        let result = transform_with_projection(&store, &request, &ctx).unwrap();
        assert_eq!(result.boundary_state, BoundaryState::DeclaredTrimValidated);
        assert!(result.trim_mismatch.is_none());
    }

    #[test]
    fn declared_trim_allows_minted_absent_anchor_only_when_validated() {
        let (_dir, store, mut request, ctx) = declared_trim_fixture();
        let mut loaded = store.load("decl").unwrap();
        loaded.meta.last_render_config = "old".to_string();
        store
            .commit("decl", loaded.row_version, &loaded.core, &loaded.meta)
            .unwrap();
        request.render_config = "new".to_string();
        let result = transform_with_projection(&store, &request, &ctx).unwrap();
        assert_eq!(result.response.action, "HARD");
        assert_eq!(result.boundary_state, BoundaryState::DeclaredTrimValidated);

        let (_dir, store, mut invalid, ctx) = declared_trim_fixture();
        let mut loaded = store.load("decl").unwrap();
        loaded.meta.last_render_config = "old".to_string();
        store
            .commit("decl", loaded.row_version, &loaded.core, &loaded.meta)
            .unwrap();
        invalid.render_config = "new".to_string();
        invalid
            .declared_trim
            .as_mut()
            .unwrap()
            .next_absolute_ordinal = 2;
        let invalid_result = transform_with_projection(&store, &invalid, &ctx).unwrap();
        assert_eq!(invalid_result.boundary_state, BoundaryState::Absent);
        assert_eq!(
            invalid_result.trim_mismatch.as_ref().map(|m| m.predicate),
            Some("continuity")
        );
    }

    fn covered_system_entries(m0: &str) -> Vec<String> {
        const OPEN: &str = "<covered-system-message>";
        const CLOSE: &str = "</covered-system-message>";
        let mut entries = Vec::new();
        let mut rest = m0;
        while let Some(open_index) = rest.find(OPEN) {
            let after_open = &rest[open_index + OPEN.len()..];
            let close_index = after_open.find(CLOSE).expect("covered system close tag");
            entries.push(after_open[..close_index].to_string());
            rest = &after_open[close_index + CLOSE.len()..];
        }
        entries
    }

    fn assert_no_system_before_tail_system(response: &TransformResponse, tail_text: &str) {
        let messages = response.messages();
        let tail_index = messages
            .iter()
            .position(|message| {
                message.role == "system" && ck_wire::text_from_message(message) == Some(tail_text)
            })
            .expect("tail system survives in output");
        assert!(
            messages[..tail_index]
                .iter()
                .all(|message| message.role != "system"),
            "covered systems must not appear in the synthetic prefix: {messages:#?}"
        );
    }

    #[test]
    fn covered_system_fold_output_matches_byte_golden() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 0, 3, "m3", "SUMMARY")])
            .unwrap();
        let tail_system = system_item("sys4", 4, "tail identity");
        let items = vec![
            system_item("sys0", 0, "identity alpha"),
            system_item("sys1", 1, "identity beta"),
            system_item("sys2", 2, "identity alpha"),
            item("m3", 3, "covered"),
            tail_system.clone(),
            item("t5", 5, "tail"),
        ];

        let r = run(&s, &cc_req("ses", "cfg0", items), &spine());
        assert_eq!(r.action, "HARD");
        let expected: Value = serde_json::from_str(include_str!(
            "../testdata/covered-system-transform-golden.json"
        ))
        .unwrap();
        assert_eq!(serde_json::to_value(r.messages()).unwrap(), expected);
        assert_eq!(
            covered_system_entries(m0_bytes(&r)),
            vec!["identity alpha".to_string(), "identity beta".to_string()]
        );
        assert_no_system_before_tail_system(&r, "tail identity");
        let tail_index = r
            .messages()
            .iter()
            .position(|message| {
                message.role == "system"
                    && ck_wire::text_from_message(message) == Some("tail identity")
            })
            .unwrap();
        assert_eq!(&r.messages()[tail_index], &tail_system.ck);
    }

    #[test]
    fn covered_systems_absorb_into_m0_and_tail_system_survives() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 3, 3, "m3", "SUMMARY")])
            .unwrap();
        let tail_system = system_item("sys4", 4, "tail identity");
        let items = vec![
            system_item("sys0", 0, "identity alpha"),
            system_item("sys1", 1, "identity beta"),
            system_item("sys2", 2, "identity alpha"),
            item("m3", 3, "covered"),
            tail_system.clone(),
            item("t5", 5, "tail"),
        ];

        let r = run(&s, &cc_req("ses", "cfg0", items.clone()), &spine());
        assert_eq!(r.action, "HARD");
        assert_eq!(r.coverage_ordinal, Some(3));
        assert_eq!(
            covered_system_entries(m0_bytes(&r)),
            vec!["identity alpha".to_string(), "identity beta".to_string()],
            "m0 carries deduplicated covered systems in first-ordinal order"
        );
        assert_no_system_before_tail_system(&r, "tail identity");
        let messages = r.messages();
        let tail_index = messages
            .iter()
            .position(|message| {
                message.role == "system"
                    && ck_wire::text_from_message(message) == Some("tail identity")
            })
            .unwrap();
        assert_eq!(&messages[tail_index], &tail_system.ck);

        let defer_one = run(&s, &cc_req("ses", "cfg0", items.clone()), &spine());
        let defer_two = run(&s, &cc_req("ses", "cfg0", items), &spine());
        assert_eq!(defer_one.action, "SOFT+");
        assert_eq!(defer_two.action, "SOFT+");
        assert_eq!(
            serde_json::to_vec(&defer_one.ck_messages).unwrap(),
            serde_json::to_vec(&r.ck_messages).unwrap(),
            "first defer replays the frozen m0 block byte-identically"
        );
        assert_eq!(
            serde_json::to_vec(&defer_two.ck_messages).unwrap(),
            serde_json::to_vec(&defer_one.ck_messages).unwrap(),
            "second defer is byte-identical to the first defer"
        );
    }

    #[test]
    fn empty_covered_system_set_omits_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1", "SUMMARY")])
            .unwrap();
        let r = run(
            &s,
            &cc_req(
                "ses",
                "cfg0",
                vec![item("m1", 1, "covered"), item("t2", 2, "tail")],
            ),
            &spine(),
        );
        assert_eq!(r.action, "HARD");
        assert!(
            !m0_bytes(&r).contains("<covered-system-messages>"),
            "empty covered system set must not render empty block bytes: {}",
            m0_bytes(&r)
        );
    }

    #[test]
    fn coverage_advance_over_system_promotes_to_hard_and_rederives_m0_block() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 3, 3, "m3", "S1")])
            .unwrap();
        let items = vec![
            system_item("sys0", 0, "identity alpha"),
            system_item("sys1", 1, "identity beta"),
            system_item("sys2", 2, "identity alpha"),
            item("m3", 3, "covered one"),
            system_item("sys4", 4, "identity gamma"),
            item("m5", 5, "covered two"),
            system_item("sys6", 6, "tail identity"),
        ];
        let first = run(&s, &cc_req("ses", "cfg0", items.clone()), &spine());
        assert_eq!(
            covered_system_entries(m0_bytes(&first)),
            vec!["identity alpha".to_string(), "identity beta".to_string()]
        );

        s.replace_compartments(
            "ses",
            &[comp(1, 3, 3, "m3", "S1"), comp(2, 4, 5, "m5", "S2")],
        )
        .unwrap();
        let advanced = run(&s, &cc_req("ses", "cfg0", items), &spine());
        assert_eq!(
            advanced.action, "HARD",
            "coverage advance over a system message must recompose m0, not ride m1"
        );
        assert_eq!(advanced.coverage_ordinal, Some(5));
        assert_eq!(
            covered_system_entries(m0_bytes(&advanced)),
            vec![
                "identity alpha".to_string(),
                "identity beta".to_string(),
                "identity gamma".to_string(),
            ],
            "new m0 re-derives the larger covered-system set while preserving prior order"
        );
        assert_no_system_before_tail_system(&advanced, "tail identity");
    }

    #[test]
    fn covered_system_content_drift_fails_identity_guard() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1", "SUMMARY")])
            .unwrap();
        let original = vec![
            system_item("sys0", 0, "identity alpha"),
            item("m1", 1, "covered"),
            item("t2", 2, "tail"),
        ];
        let first = run(&s, &cc_req("ses", "cfg0", original), &spine());
        assert_eq!(first.action, "HARD");

        let drift = transform(
            &s,
            &cc_req(
                "ses",
                "cfg0",
                vec![
                    system_item("sys0", 0, "identity beta"),
                    item("m1", 1, "covered"),
                    item("t2", 2, "tail"),
                ],
            ),
            &pctx("git:proj", "/nonexistent-docs", 0),
        )
        .unwrap_err();
        assert!(
            matches!(drift, TransformError::IdentityDrift(ref mid) if mid == "sys0"),
            "covered system content drift must fail via IdentityDrift, got {drift:?}"
        );
    }

    #[test]
    fn profile_epoch_fold_hards_epoch_zero_cc_state_once() {
        assert_eq!(crate::PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC, 1);
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.replace_compartments("ses", &[comp(1, 1, 1, "m1", "SUMMARY")])
            .unwrap();
        let messages = vec![
            system_item("sys0", 0, "identity alpha"),
            item("m1", 1, "covered"),
            item("t2", 2, "tail"),
        ];
        let current = cc_req("ses", "cfg0", messages.clone());
        let first = run(&s, &current, &spine());
        assert_eq!(first.action, "HARD");

        let mut loaded = s.load("ses").unwrap();
        // Keep the shared memory-render epoch current so this fixture isolates
        // the claude-code profile epoch transition.
        loaded.meta.last_render_config = global_epoch_effective_render_config(&s, "cfg0");
        loaded
            .core
            .frozen_units
            .iter_mut()
            .find(|unit| unit.key == "m0")
            .unwrap()
            .frozen_payload = "OLD-M0-WITHOUT-COVERED-SYSTEMS".to_string();
        s.commit("ses", loaded.row_version, &loaded.core, &loaded.meta)
            .unwrap();

        let transitioned = run(&s, &current, &spine());
        assert_eq!(
            transitioned.action, "HARD",
            "module-side profile epoch folding must hard once even when the caller's cfg is static"
        );
        assert!(
            m0_bytes(&transitioned).contains("<covered-system-message>identity alpha"),
            "the transition fold must recompose m0 with the covered-system block: {}",
            m0_bytes(&transitioned)
        );
        assert!(!m0_bytes(&transitioned).contains("OLD-M0"));

        let one_shot = run(&s, &current, &spine());
        assert_eq!(
            one_shot.action, "SOFT+",
            "after last_render_config records mpe1, the profile fold must not loop"
        );
    }

    #[test]
    fn global_memory_render_epoch_hards_all_profiles_once_then_stabilizes() {
        assert_eq!(crate::MEMORY_RENDER_FORMAT_EPOCH, 2);
        for profile in [
            SerializerProfile::OwnedLlmRunner,
            SerializerProfile::Pi,
            SerializerProfile::OpencodeAiSdk,
            SerializerProfile::ClaudeCodeAnthropic,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let s = store(dir.path());
            let session = format!("ses-{}", profile.wire_id());
            s.replace_compartments(&session, &[comp(1, 1, 1, "m1", "SUMMARY")])
                .unwrap();
            let request = profile_req(
                profile,
                &session,
                "cfg0",
                vec![item("m1", 1, "covered"), item("t2", 2, "tail")],
            );
            let first = run(&s, &request, &spine());
            assert_eq!(first.action, "HARD");

            let mut loaded = s.load(&session).unwrap();
            let profile_epoch = crate::profile_render_epoch(profile);
            let profile_epoch_component = if profile_epoch == 0 {
                String::new()
            } else {
                format!("mpe{profile_epoch}")
            };
            let current_cfg = effective_render_config_with_epochs(
                &s,
                "cfg0",
                format!("mre{}", crate::MEMORY_RENDER_FORMAT_EPOCH),
                format!("cre{}", crate::COMPARTMENT_RENDER_FORMAT_EPOCH),
                profile_epoch_component.clone(),
                String::new(),
            );
            assert_eq!(loaded.meta.last_render_config, current_cfg);
            assert!(
                current_cfg.contains(&format!("mre:4:mre{}", crate::MEMORY_RENDER_FORMAT_EPOCH))
            );

            loaded.meta.last_render_config = effective_render_config_with_epochs(
                &s,
                "cfg0",
                String::new(),
                format!("cre{}", crate::COMPARTMENT_RENDER_FORMAT_EPOCH),
                profile_epoch_component,
                String::new(),
            );
            loaded
                .core
                .frozen_units
                .iter_mut()
                .find(|unit| unit.key == "m0")
                .unwrap()
                .frozen_payload = "OLD-PROJECT-MEMORY-FORMAT".to_string();
            s.commit(&session, loaded.row_version, &loaded.core, &loaded.meta)
                .unwrap();

            let transitioned = run(&s, &request, &spine());
            assert_eq!(
                transitioned.action,
                "HARD",
                "{} must fold the shared memory render epoch",
                profile.wire_id()
            );
            assert!(!m0_bytes(&transitioned).contains("OLD-PROJECT-MEMORY-FORMAT"));

            let steady = run(&s, &request, &spine());
            assert_eq!(
                steady.action,
                "SOFT+",
                "{} must not loop the memory render fold",
                profile.wire_id()
            );
            assert!(!steady.committed);
        }
    }

    #[test]
    fn compartment_render_epoch_hards_all_profiles_once_then_stabilizes() {
        assert_eq!(crate::COMPARTMENT_RENDER_FORMAT_EPOCH, 2);
        for profile in [
            SerializerProfile::OwnedLlmRunner,
            SerializerProfile::Pi,
            SerializerProfile::OpencodeAiSdk,
            SerializerProfile::ClaudeCodeAnthropic,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let s = store(dir.path());
            let session = format!("cre-session-{}", profile.wire_id());
            s.replace_compartments(&session, &[comp(1, 1, 1, "m1", "SUMMARY")])
                .unwrap();
            let request = profile_req(
                profile,
                &session,
                "cfg0",
                vec![item("m1", 1, "covered"), item("t2", 2, "tail")],
            );
            assert_eq!(run(&s, &request, &spine()).action, "HARD");

            let mut loaded = s.load(&session).unwrap();
            let profile_epoch = crate::profile_render_epoch(profile);
            let profile_epoch_component = if profile_epoch == 0 {
                String::new()
            } else {
                format!("mpe{profile_epoch}")
            };
            assert!(loaded.meta.last_render_config.contains("cre:4:cre2"));
            loaded.meta.last_render_config = effective_render_config_with_epochs(
                &s,
                "cfg0",
                format!("mre{}", crate::MEMORY_RENDER_FORMAT_EPOCH),
                String::new(),
                profile_epoch_component,
                String::new(),
            );
            loaded
                .core
                .frozen_units
                .iter_mut()
                .find(|unit| unit.key == "m0")
                .unwrap()
                .frozen_payload =
                "<session-history><compartment title=\"old\" /></session-history>".to_string();
            s.commit(&session, loaded.row_version, &loaded.core, &loaded.meta)
                .unwrap();

            let transitioned = run(&s, &request, &spine());
            assert_eq!(
                transitioned.action,
                "HARD",
                "{} must fold the shared compartment render epoch",
                profile.wire_id()
            );
            assert!(m0_bytes(&transitioned).contains("## 1-1 · C1"));
            assert!(!m0_bytes(&transitioned).contains("<compartment"));

            let steady = run(&s, &request, &spine());
            assert_eq!(
                steady.action,
                "SOFT+",
                "{} must not loop the compartment render fold",
                profile.wire_id()
            );
            assert!(!steady.committed);
        }
    }
}
