//! The Magic Context subc module.
//!
//! Harness-agnostic cache-stability transform: receives already-decoded CK items,
//! classifies the pass, drives `cortexkit-cache-core` through `mc-core`, and
//! persists per-session state in the single-writer `mc-store`. Served over the subc
//! wire via `subc-client-rs`'s `serve` (provider role).
//!
//! Lifecycle (handled by `serve`): read `--subc <connection-file>`, authenticate,
//! send HELLO{manifest}, await HELLO_ACK. [`McHandler::on_hello_ack`] is the storage
//! seam — it resolves the descriptor (from `ack.storage`, else a local dev path) and
//! opens the store EXACTLY ONCE (single-writer lease held for the module lifetime).
//!
//! Slice-1 scope: the cache-stability spine. `handle` answers `transform` (the
//! CK-in/CK-out pass: classify → cache-core step → conditional commit), `health`
//! (proves the store opened), and echoes otherwise.

#![forbid(unsafe_code)]

pub mod boundary;
pub mod ck_wire;
pub mod classify;
pub mod codec;
pub mod compartment_coverage;
pub mod config;
pub mod decay_render;
pub mod healing;
pub mod historian;
pub mod historian_chunk;
pub mod historian_producer;
pub mod historian_prompt;
pub mod historian_validate;
pub mod injection;
pub mod m0_compose;
pub mod m1_compose;
pub mod memory_render;
pub mod memory_tool;
pub mod project_docs;
pub mod scheduler;
pub mod selection;
pub mod session_resolver;
pub mod transform;

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use chrono::{Local, TimeZone};
use cortexkit_store_types::{sqlite_store_path, Isolation, StorageBackend, StorageDescriptor};
use mc_store::{
    canonical_root, validate_state_import_compartments, HistorianPhase, InsertMemoryInput,
    MappingUpdate, McStore, McStoreError, NoteCasOutcome, NoteEvaluationInput, NoteInput,
    NoteWriteInput, RecordWrapupCommandOutcome, ShadowDivergenceRecord, ShadowDropSeedRow,
    ShadowMemoryMutationRow, ShadowMemoryRow, ShadowStateSyncError, ShadowStateSyncRequest,
    ShadowWorkspaceMemberRow, ShadowWorkspaceRow, StateImportError, StateImportPreflight,
    StateImportValidationError, StoredChunkTranscript, StoredCompartment, StoredMemoryMutation,
    StoredNote, TodoStateSetOutcome, VerificationUpdate, WrapupCommandRecord,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use subc_client_rs::{
    async_trait, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest, RouteHandle,
};

use boundary::{BoundaryBlock, BoundaryContext, BoundaryMsg, Role, TriggerContext};
use classify::{
    child_session_id, CLASSIFY_AWAIT_TIMEOUT, CLASSIFY_MAX_OUTPUT_TOKENS,
    CLASSIFY_RECOVERY_TIMEOUT, CLASSIFY_SYSTEM_PROMPT, CLASSIFY_TASK, CLASSIFY_TEMPERATURE,
    MAX_CLASSIFY_PROMPT_BYTES,
};
use config::{ConfigCache, McModuleConfig};
use healing::{tail_reclaim, SerializerProfile};
use historian::{reattach_historian_producer, run_historian_firing, HistorianProducerDriver};
use historian_chunk::{
    assemble_historian_firing, AssembleHistorianFiringOutcome, AssembledHistorianFiring,
    HistorianAssemblerConfig,
};
use historian_producer::{HistorianProducer, HistorianProducerConfig, HistorianProducerError};
use scheduler::MIN_PLAUSIBLE_CONTEXT_LIMIT;
use selection::SelKind;
#[cfg(test)]
use session_resolver::ResolvedSession;
use session_resolver::{
    MissingSessionResolver, RealSessionResolver, SessionResolveError, SessionResolver,
};
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ConsumerRole, ExecutionMode, IdentityBinding, IdentityScope,
        ModuleManifest, ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
    },
    ModuleHelloAckBody, PROTOCOL_VERSION,
};
#[cfg(test)]
use transform::ReductionDecision;
use transform::{transform_with_projection, DeclaredTrim, HistorianDiagnostics, TransformRequest};

/// The per-route binding: the project, harness, session-slot value, and fallback render
/// budget frozen at bind. Transform routes carry the durable session in `session`. Facade
/// routes have two identity modes: the OpenCode Rust route binds its durable session directly,
/// while the Claude Code wrapper binds an instance token that must be resolved before touching
/// the store. The project is NEVER taken from a per-pass request field — a crafted request could
/// spoof it to read another project's memories — so it lives here, keyed by the route channel
/// the daemon controls.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionBinding {
    pub project_root: PathBuf,
    pub harness: String,
    pub session: String,
    pub model_key: Option<String>,
    pub config: McModuleConfig,
    /// The fallback history budget (tokens) frozen at bind. A transform request may carry
    /// a newer harness-resolved value because config can change while the route remains open.
    pub history_budget_tokens: f64,
}

/// Why a transform request can't be served: the route isn't bound, or the request's
/// session doesn't match the channel's bound session. Both fail LOUD — never default to
/// a project (a default would be a cross-project read of another project's store).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingError {
    /// No `on_bind` for this channel (or it was torn down). Reject the transform.
    Unbound,
    /// The request's session_id doesn't match the channel's bound session — a poison
    /// cross-check (the channel is the daemon-controlled identity; the request's session
    /// must agree with it).
    SessionMismatch,
}

/// Canonical module id (overridable via `SUBC_MODULE_ID_ENV` at boot).
pub const DEFAULT_MODULE_ID: &str = "magic-context";

/// Render-config epoch members, co-owned with the byte-splice consumer (thalamus gateway).
/// Consumers fold these into the opaque render_config string they populate per
/// request; the module compares render_config as opaque bytes against durable state
/// and forces a HARD fold on any change. Bumping an epoch here is therefore the
/// coordinated-deploy mechanism for byte-affecting behavior flips: every in-flight
/// session folds once on the same lineage instead of straddling the feature boundary.
/// Consumers read these at attach via the status op and refuse to serve on mismatch
/// with their hardcoded fallbacks, so a diverged epoch map cannot silently skip the
/// safety fold.
/// Bumps when the shared project-memory render changes. Epoch 1 is the compact,
/// category-grouped `#id: fact` format and applies to every serializer profile.
pub const MEMORY_RENDER_FORMAT_EPOCH: u32 = 2;
/// Bumps when the shared compartment render changes. Epoch 1 replaces rendered
/// `<compartment>` elements with markdown headings in m0 and m1; epoch 2 sanitizes
/// historian-authored titles before placing them inside the session-history wrapper.
pub const COMPARTMENT_RENDER_FORMAT_EPOCH: u32 = 2;
/// Bumps when the rendered m0 prefix format changes for the claude-code-anthropic
/// profile; epoch 1 includes covered system messages in m0 instead of sending them as
/// separate system-role messages.
pub const PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC: u32 = 1;
/// Bumps when any active tag overlay changes provider-visible bytes. Epoch 3 freezes
/// temporal-marker decisions in durable rows instead of deriving them from each request array.
/// Every change requires one cache-breaking fold before the new overlay can render. Inactive
/// requests omit the component and retain their identity.
pub const TAGGER_FEATURE_EPOCH: u32 = 3;

/// The module-owned rendered-prefix format epoch for a serializer profile.
///
/// Future profile-specific m0 format epochs slot in here so the module folds them into
/// its effective render_config even when a consumer sends a static base render_config.
pub const fn profile_render_epoch(profile: SerializerProfile) -> u32 {
    match profile {
        SerializerProfile::ClaudeCodeAnthropic => PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC,
        SerializerProfile::OwnedLlmRunner
        | SerializerProfile::OwnedBroca
        | SerializerProfile::OpencodeAiSdk
        | SerializerProfile::Pi => 0,
    }
}

/// Normalize the request-local Claude Code surface signal once. This remains limited to
/// Claude Code mechanics such as the Thalamus acknowledgement contract and guidance variant.
pub const fn cc_u1_active(profile: Option<SerializerProfile>, tool_present: bool) -> bool {
    matches!(profile, Some(SerializerProfile::ClaudeCodeAnthropic)) && tool_present
}

/// Return whether the provider-visible tagging and reduction overlay may be enabled.
/// OpenCode uses the same overlay when its session exposes ctx_reduce.
pub const fn tagging_surface_active(
    profile: Option<SerializerProfile>,
    tool_present: bool,
) -> bool {
    matches!(
        profile,
        Some(SerializerProfile::ClaudeCodeAnthropic | SerializerProfile::OpencodeAiSdk)
    ) && tool_present
}

/// The tagger component of the effective render identity. A false request contributes
/// no component, preserving the render identity used before the capability existed.
pub const fn tagger_feature_epoch(tagging_surface_active: bool) -> u32 {
    if tagging_surface_active {
        TAGGER_FEATURE_EPOCH
    } else {
        0
    }
}

/// Storage namespace for the cache-state domain.
const STORAGE_NAMESPACE: &str = "mc_cache";
const GUIDANCE_TEXT: &str = include_str!("../assets/guidance_primary.txt");
/// Guidance variant for surfaces where ctx_reduce is not callable: the tagging and
/// reduction sections are removed entirely, because instructing the model to reduce
/// with a tool it cannot reach is a model-facing coherence bug. Consumers pick the
/// variant that matches the live tool surface; the two variants have different
/// content hashes, so a surface widening folds the prefix by construction.
const GUIDANCE_TEXT_NO_REDUCE: &str = include_str!("../assets/guidance_no_reduce.txt");
const CTX_REDUCE_ACKNOWLEDGEMENT: &str = "Queued for context compaction.";
/// Mirrors packages/plugin/src/config/schema/magic-context.ts commit_cluster_trigger.enabled default.
const DEFAULT_COMMIT_CLUSTER_TRIGGER_ENABLED: bool = true;
/// Mirrors packages/plugin/src/config/schema/magic-context.ts commit_cluster_trigger.min_clusters default.
const DEFAULT_MIN_COMMIT_CLUSTERS: usize = 3;
/// Mirrors packages/plugin/src/hooks/magic-context/derive-budgets.ts with the default
/// 128K historian context fallback: clamp(128_000 × 0.25, 8_000, 50_000) = 32_000.
const DEFAULT_HISTORIAN_CHUNK_TOKENS: usize = 32_000;
/// Secondary assembler guard; TS trigger sizing is authoritative, this only rejects tiny chunks.
const DEFAULT_HISTORIAN_MIN_CHUNK_TOKENS: usize = 512;
/// Thalamus clamps the prompt argument to the same range before forwarding it.
/// Changing either bound requires a coordinated module and Thalamus update so a
/// retried prompt resolves to the same keep watermark on both sides.
const WRAPUP_KEEP_MIN: usize = 5;
const WRAPUP_KEEP_MAX: usize = 100;
/// Maximum number of newly published compartments returned by one status page.
const SESSION_STATUS_COMPARTMENT_PAGE_LIMIT: usize = 50;
/// After a historian abandon, suppress refires for this long so a persistently
/// failing model does not burn a full summarization pass on every transform.
const HISTORIAN_FAILURE_BACKOFF_MS: i64 = historian::HISTORIAN_FAILURE_BACKOFF_MS;
const SESSION_UNRESOLVED_MESSAGE: &str =
    "session unresolved; launch Claude Code through the CortexKit wrapper so ctx_* can bind to this conversation";
/// OpenCode's Rust-mode tool route binds the real harness session, unlike the Claude Code
/// wrapper route whose binding is an instance-token namespace.
const OPENCODE_HARNESS: &str = "opencode";
const SHADOW_SESSION_PREFIX: &str = mc_store::SHADOW_SESSION_PREFIX;
const SHADOW_COMPARE_PREFIX_LIMIT: usize = 4096;
const SHADOW_SEED_MAX_ID_BYTES: usize = 128;
const SHADOW_SEED_MAX_STAGED_BYTES: usize = 32 * 1024 * 1024;
const SHADOW_SEED_MAX_PENDING: usize = 64;
// These limits are shared by authority and shadow transform pages. The names retain the
// shadow prefix because the wire was introduced by that lane, but the coordinator is
// intentionally handler-wide so a real-session sender cannot bypass the same budget.
const SHADOW_TRANSFORM_PAGE_MAX_BYTES: usize = 512 * 1024;
const SHADOW_TRANSFORM_PAGE_MAX_STAGED_BYTES: usize = 128 * 1024 * 1024;
const SHADOW_TRANSFORM_PAGE_MAX_PENDING: usize = 64;
const SHADOW_TRANSFORM_PAGE_MAX_ID_BYTES: usize = 128;
const SHADOW_ITEM_CONTINUATION_KEY: &str = "__shadow_item_continuation";
const TRANSFORM_PAGE_FIELDS: [&str; 6] = [
    "transform_page_id",
    "transform_generation",
    "transform_page_index",
    "transform_page_total",
    "transform_page_complete",
    "transform_page_digest",
];
const TRANSFORM_PAGE_ARRAY_FIELDS: [&str; 6] = [
    "input",
    "messages",
    "native_messages",
    "ts_output",
    "ts_ck_messages",
    "normalizations",
];
const STATE_IMPORT_MAX_ID_BYTES: usize = 128;
const STATE_IMPORT_MAX_STAGED_BYTES: usize = 32 * 1024 * 1024;
const STATE_IMPORT_MAX_PENDING: usize = 64;
const STATE_IMPORT_STALE_AFTER: Duration = Duration::from_secs(5 * 60);
const TRANSFORM_SNAPSHOT_BUDGET_BYTES: usize = 64 * 1024 * 1024;
const ACTIVE_SNAPSHOT_LEASE_BUDGET_BYTES: usize = TRANSFORM_SNAPSHOT_BUDGET_BYTES;
const MAX_ACTIVE_SNAPSHOT_LEASES: usize = 8;
/// InFlight snapshot markers have no byte charge, so they need their own count bound:
/// a marker is minted per transform start and only replaced on success, so unique
/// failing sessions would otherwise accumulate for the process lifetime.
const MAX_IN_FLIGHT_SNAPSHOT_ENTRIES: usize = 4_096;
const WRAPUP_REQUEST_MARGIN: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
struct ShadowStateSyncWire {
    #[serde(default)]
    session_id: Option<String>,
    shadow_generation: u64,
    expected_shadow_seq: u64,
    #[serde(default)]
    seed_id: Option<String>,
    #[serde(default)]
    seed_generation: Option<u64>,
    #[serde(default)]
    seed_batch_index: Option<usize>,
    #[serde(default)]
    seed_batch_total: Option<usize>,
    #[serde(default)]
    seed_complete: Option<bool>,
    #[serde(default)]
    seed_boundary_id: Option<String>,
    #[serde(default)]
    compartments: Vec<ShadowCompartmentWire>,
    #[serde(default)]
    memories: Vec<ShadowMemoryWire>,
    #[serde(default)]
    memory_mutations: Vec<ShadowMemoryMutationWire>,
    #[serde(default)]
    user_profile: Vec<String>,
    #[serde(default)]
    workspace: Option<ShadowWorkspaceWire>,
    #[serde(default)]
    last_todo_state: Option<String>,
    #[serde(default)]
    acked_watermarks: Option<Value>,
    #[serde(default)]
    drop_seeds: Vec<ShadowDropSeedWire>,
    #[serde(default)]
    drop_seed_skipped: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct ShadowDropSeedWire {
    #[serde(alias = "target_id")]
    block_id: String,
    #[serde(default)]
    related_block_ids: Vec<String>,
    #[serde(alias = "mode")]
    drop_mode: String,
    #[serde(default)]
    payload: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateSyncLane {
    Authority,
    Shadow,
}

impl StateSyncLane {
    const fn is_shadow(self) -> bool {
        matches!(self, Self::Shadow)
    }
}

/// State-sync errors use the shared subc code/message envelope. Authority callers receive
/// the durable sequence as JSON in the message so a fresh process can adopt it and rebuild
/// its watermarks. Shadow callers retain the reset-on-mismatch error shape because a stale
/// mirror may be poisoned rather than merely restarted.
fn state_sync_seq_mismatch_error(lane: StateSyncLane, expected: u64, found: u64) -> HandlerOutcome {
    if lane.is_shadow() {
        HandlerOutcome::Error {
            code: "shadow_seq_mismatch".to_string(),
            message: format!(
                "expected_shadow_seq {expected} did not match durable shadow_seq {found}"
            ),
        }
    } else {
        HandlerOutcome::Error {
            code: "authority_seq_mismatch".to_string(),
            message: json!({
                "code": "authority_seq_mismatch",
                "expected_authority_seq": expected,
                "durable_authority_seq": found,
            })
            .to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransformLane {
    Authority,
    Shadow,
}

impl TransformLane {
    const fn is_shadow(self) -> bool {
        matches!(self, Self::Shadow)
    }
}

#[derive(Debug, Deserialize)]
struct ShadowResetWire {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    shadow_generation: Option<u64>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StateImportWire {
    v: u64,
    session_id: String,
    import_id: String,
    batch_seq: usize,
    batch_count: usize,
    compartments: Vec<StateImportCompartmentWire>,
}

#[derive(Debug, Clone, Deserialize)]
struct StateImportCompartmentWire {
    seq: i64,
    start_message: i64,
    end_message: i64,
    end_message_id: String,
    title: String,
    p1: String,
    #[serde(default)]
    p2: Option<String>,
    #[serde(default)]
    p3: Option<String>,
    #[serde(default)]
    p4: Option<String>,
    #[serde(default = "default_importance")]
    importance: i32,
    #[serde(default)]
    episode_type: Option<String>,
    #[serde(default)]
    start_date: Option<String>,
    #[serde(default)]
    end_date: Option<String>,
}

impl StateImportCompartmentWire {
    fn into_stored(self, created_at: i64) -> StoredCompartment {
        StoredCompartment {
            sequence: self.seq,
            start_message: self.start_message,
            end_message: self.end_message,
            end_message_id: self.end_message_id,
            start_date: self.start_date,
            end_date: self.end_date,
            title: self.title,
            content: self.p1.clone(),
            p1: Some(self.p1),
            p2: self.p2,
            p3: self.p3,
            p4: self.p4,
            importance: self.importance,
            episode_type: self.episode_type,
            legacy: 0,
            created_at,
            ..Default::default()
        }
    }
}

#[derive(Debug)]
struct PendingShadowSeed {
    seed_id: String,
    generation: u64,
    expected_seq: u64,
    total: usize,
    next_index: usize,
    digests: Vec<String>,
    batches: Vec<ShadowStateSyncWire>,
    bytes: usize,
}

#[derive(Debug)]
enum ShadowSeedPhase {
    Idle,
    AwaitingSeed { generation: u64, expected_seq: u64 },
    Collecting(PendingShadowSeed),
    Applying { seed_id: String, bytes: usize },
}

#[derive(Debug)]
struct CompletedShadowSeed {
    seed_id: String,
    final_digest: String,
    generation: u64,
    expected_seq: u64,
    total: usize,
    result: Vec<u8>,
}

#[derive(Debug)]
struct ShadowSeedSession {
    phase: ShadowSeedPhase,
    completed: Option<CompletedShadowSeed>,
}

impl Default for ShadowSeedSession {
    fn default() -> Self {
        Self {
            phase: ShadowSeedPhase::Idle,
            completed: None,
        }
    }
}

#[derive(Debug)]
struct ShadowSeedCoordinator {
    sessions: HashMap<String, ShadowSeedSession>,
    total_staged_bytes: usize,
    pending_seed_count: usize,
    max_staged_bytes: usize,
    max_pending_seeds: usize,
}

impl Default for ShadowSeedCoordinator {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            total_staged_bytes: 0,
            pending_seed_count: 0,
            max_staged_bytes: SHADOW_SEED_MAX_STAGED_BYTES,
            max_pending_seeds: SHADOW_SEED_MAX_PENDING,
        }
    }
}

impl ShadowSeedCoordinator {
    fn phase_bytes(phase: &ShadowSeedPhase) -> usize {
        match phase {
            ShadowSeedPhase::Collecting(seed) => seed.bytes,
            ShadowSeedPhase::Applying { bytes, .. } => *bytes,
            ShadowSeedPhase::Idle | ShadowSeedPhase::AwaitingSeed { .. } => 0,
        }
    }

    fn is_pending(phase: &ShadowSeedPhase) -> bool {
        !matches!(phase, ShadowSeedPhase::Idle)
    }

    fn discard_pending(&mut self, session_id: &str) {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        if Self::is_pending(&session.phase) {
            self.pending_seed_count = self.pending_seed_count.saturating_sub(1);
            self.total_staged_bytes = self
                .total_staged_bytes
                .saturating_sub(Self::phase_bytes(&session.phase));
            session.phase = ShadowSeedPhase::Idle;
        }
    }

    fn release_phase(&mut self, phase: &ShadowSeedPhase) {
        if Self::is_pending(phase) {
            self.pending_seed_count = self.pending_seed_count.saturating_sub(1);
            self.total_staged_bytes = self
                .total_staged_bytes
                .saturating_sub(Self::phase_bytes(phase));
        }
    }

    fn set_phase(&mut self, session_id: &str, phase: ShadowSeedPhase) {
        self.sessions
            .entry(session_id.to_string())
            .or_default()
            .phase = phase;
    }

    fn evict(&mut self, session_id: &str) {
        self.discard_pending(session_id);
        self.sessions.remove(session_id);
    }

    fn arm_after_reset(&mut self, session_id: &str, generation: u64, expected_seq: u64) -> bool {
        self.discard_pending(session_id);
        let session = self.sessions.entry(session_id.to_string()).or_default();
        session.completed = None;
        if self.pending_seed_count >= self.max_pending_seeds {
            session.phase = ShadowSeedPhase::Idle;
            return false;
        }
        session.phase = ShadowSeedPhase::AwaitingSeed {
            generation,
            expected_seq,
        };
        self.pending_seed_count += 1;
        true
    }
}

#[derive(Debug)]
struct PendingTransformPage {
    transform_id: String,
    generation: u64,
    total: usize,
    next_index: usize,
    digests: Vec<String>,
    pages: Vec<Value>,
    bytes: usize,
}

#[derive(Debug)]
enum TransformPagePhase {
    Idle,
    Collecting(PendingTransformPage),
    Applying { transform_id: String, bytes: usize },
}

#[derive(Debug)]
struct CompletedTransformPage {
    transform_id: String,
    generation: u64,
    final_digest: String,
    result: Vec<u8>,
}

#[derive(Debug)]
struct TransformPageSession {
    phase: TransformPagePhase,
    completed: Option<CompletedTransformPage>,
}

impl Default for TransformPageSession {
    fn default() -> Self {
        Self {
            phase: TransformPagePhase::Idle,
            completed: None,
        }
    }
}

/// Authority and shadow transform pages share this coordinator so every session has one
/// in-flight attempt and every sender contributes to the same bounded staging budget.
#[derive(Debug)]
struct TransformPageCoordinator {
    sessions: HashMap<String, TransformPageSession>,
    total_staged_bytes: usize,
    pending_transform_count: usize,
    max_staged_bytes: usize,
    max_pending_transforms: usize,
}

impl Default for TransformPageCoordinator {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            total_staged_bytes: 0,
            pending_transform_count: 0,
            max_staged_bytes: SHADOW_TRANSFORM_PAGE_MAX_STAGED_BYTES,
            max_pending_transforms: SHADOW_TRANSFORM_PAGE_MAX_PENDING,
        }
    }
}

enum TransformPageStageAction {
    Ack(usize),
    Apply {
        pages: Vec<Value>,
        transform_id: String,
        generation: u64,
        final_digest: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransformPageStageError {
    AttemptMismatch,
    DigestMismatch,
    OrderMismatch,
    BufferOverflow,
    InProgress,
}

impl TransformPageCoordinator {
    fn phase_bytes(phase: &TransformPagePhase) -> usize {
        match phase {
            TransformPagePhase::Collecting(pending) => pending.bytes,
            TransformPagePhase::Applying { bytes, .. } => *bytes,
            TransformPagePhase::Idle => 0,
        }
    }

    fn is_pending(phase: &TransformPagePhase) -> bool {
        !matches!(phase, TransformPagePhase::Idle)
    }

    fn release_phase(&mut self, phase: &TransformPagePhase) {
        if Self::is_pending(phase) {
            self.pending_transform_count = self.pending_transform_count.saturating_sub(1);
            self.total_staged_bytes = self
                .total_staged_bytes
                .saturating_sub(Self::phase_bytes(phase));
        }
    }

    fn discard(&mut self, session_id: &str) {
        let phase = self.sessions.get_mut(session_id).map(|session| {
            session.completed = None;
            std::mem::replace(&mut session.phase, TransformPagePhase::Idle)
        });
        if let Some(phase) = phase {
            self.release_phase(&phase);
        }
    }

    fn set_phase(&mut self, session_id: &str, phase: TransformPagePhase) {
        self.sessions
            .entry(session_id.to_string())
            .or_default()
            .phase = phase;
    }

    fn completed(&self, session_id: &str, transform_id: &str) -> Option<&CompletedTransformPage> {
        self.sessions
            .get(session_id)
            .and_then(|session| session.completed.as_ref())
            .filter(|completed| completed.transform_id == transform_id)
    }

    #[allow(clippy::too_many_arguments)]
    fn stage(
        &mut self,
        session_id: &str,
        transform_id: String,
        generation: u64,
        page_index: usize,
        page_total: usize,
        page_digest: String,
        page: Value,
        page_bytes: usize,
        page_complete: bool,
    ) -> Result<TransformPageStageAction, TransformPageStageError> {
        if self.pending_transform_count >= self.max_pending_transforms
            && !self.sessions.contains_key(session_id)
        {
            return Err(TransformPageStageError::BufferOverflow);
        }
        let phase = {
            let session = self.sessions.entry(session_id.to_string()).or_default();
            std::mem::replace(&mut session.phase, TransformPagePhase::Idle)
        };
        match phase {
            TransformPagePhase::Idle => {
                if page_index != 0 {
                    return Err(TransformPageStageError::AttemptMismatch);
                }
                if page_bytes > self.max_staged_bytes
                    || self
                        .total_staged_bytes
                        .checked_add(page_bytes)
                        .is_none_or(|bytes| bytes > self.max_staged_bytes)
                {
                    return Err(TransformPageStageError::BufferOverflow);
                }
                self.total_staged_bytes += page_bytes;
                self.pending_transform_count += 1;
                if page_complete {
                    self.set_phase(
                        session_id,
                        TransformPagePhase::Applying {
                            transform_id: transform_id.clone(),
                            bytes: page_bytes,
                        },
                    );
                    Ok(TransformPageStageAction::Apply {
                        pages: vec![page],
                        transform_id,
                        generation,
                        final_digest: page_digest,
                    })
                } else {
                    self.set_phase(
                        session_id,
                        TransformPagePhase::Collecting(PendingTransformPage {
                            transform_id,
                            generation,
                            total: page_total,
                            next_index: 1,
                            digests: vec![page_digest],
                            pages: vec![page],
                            bytes: page_bytes,
                        }),
                    );
                    Ok(TransformPageStageAction::Ack(1))
                }
            }
            TransformPagePhase::Applying {
                transform_id: active,
                bytes,
            } => {
                self.set_phase(
                    session_id,
                    TransformPagePhase::Applying {
                        transform_id: active,
                        bytes,
                    },
                );
                Err(TransformPageStageError::InProgress)
            }
            TransformPagePhase::Collecting(mut pending) => {
                if pending.transform_id != transform_id
                    || pending.generation != generation
                    || pending.total != page_total
                {
                    self.release_phase(&TransformPagePhase::Collecting(pending));
                    return Err(TransformPageStageError::AttemptMismatch);
                }
                if page_index < pending.next_index {
                    let matches = pending
                        .digests
                        .get(page_index)
                        .is_some_and(|accepted| accepted == &page_digest);
                    let next_index = pending.next_index;
                    if !matches {
                        self.release_phase(&TransformPagePhase::Collecting(pending));
                        return Err(TransformPageStageError::DigestMismatch);
                    }
                    self.set_phase(session_id, TransformPagePhase::Collecting(pending));
                    return Ok(TransformPageStageAction::Ack(next_index));
                }
                if page_index > pending.next_index {
                    self.release_phase(&TransformPagePhase::Collecting(pending));
                    return Err(TransformPageStageError::OrderMismatch);
                }
                let next_bytes = pending.bytes.checked_add(page_bytes);
                let total_bytes = self.total_staged_bytes.checked_add(page_bytes);
                if next_bytes.is_none_or(|bytes| bytes > self.max_staged_bytes)
                    || total_bytes.is_none_or(|bytes| bytes > self.max_staged_bytes)
                {
                    self.release_phase(&TransformPagePhase::Collecting(pending));
                    return Err(TransformPageStageError::BufferOverflow);
                }
                pending.bytes = next_bytes.unwrap_or(usize::MAX);
                self.total_staged_bytes = total_bytes.unwrap_or(usize::MAX);
                pending.next_index += 1;
                pending.digests.push(page_digest.clone());
                pending.pages.push(page);
                if page_complete {
                    let bytes = pending.bytes;
                    let pages = std::mem::take(&mut pending.pages);
                    let active_id = pending.transform_id.clone();
                    let active_generation = pending.generation;
                    self.set_phase(
                        session_id,
                        TransformPagePhase::Applying {
                            transform_id: active_id.clone(),
                            bytes,
                        },
                    );
                    Ok(TransformPageStageAction::Apply {
                        pages,
                        transform_id: active_id,
                        generation: active_generation,
                        final_digest: page_digest,
                    })
                } else {
                    let next_index = pending.next_index;
                    self.set_phase(session_id, TransformPagePhase::Collecting(pending));
                    Ok(TransformPageStageAction::Ack(next_index))
                }
            }
        }
    }
}

#[derive(Debug)]
struct PendingStateImport {
    import_id: String,
    batch_count: usize,
    next_seq: usize,
    digests: Vec<String>,
    compartments: Vec<StoredCompartment>,
    bytes: usize,
    last_activity: Instant,
}

#[derive(Debug)]
enum StateImportPhase {
    Collecting(PendingStateImport),
    Applying { import_id: String, bytes: usize },
}

#[derive(Debug)]
struct StateImportCoordinator {
    sessions: HashMap<String, StateImportPhase>,
    total_staged_bytes: usize,
    pending_import_count: usize,
    max_staged_bytes: usize,
    max_pending_imports: usize,
    stale_after: Duration,
}

impl Default for StateImportCoordinator {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            total_staged_bytes: 0,
            pending_import_count: 0,
            max_staged_bytes: STATE_IMPORT_MAX_STAGED_BYTES,
            max_pending_imports: STATE_IMPORT_MAX_PENDING,
            stale_after: STATE_IMPORT_STALE_AFTER,
        }
    }
}

#[derive(Debug)]
enum StateImportStageOutcome {
    Staged(usize),
    Apply {
        import_id: String,
        compartments: Vec<StoredCompartment>,
    },
}

#[derive(Debug)]
enum StateImportStageError {
    Protocol {
        code: &'static str,
        message: &'static str,
    },
    Validation(StateImportValidationError),
}

impl StateImportCoordinator {
    fn phase_bytes(phase: &StateImportPhase) -> usize {
        match phase {
            StateImportPhase::Collecting(pending) => pending.bytes,
            StateImportPhase::Applying { bytes, .. } => *bytes,
        }
    }

    fn discard(&mut self, session_id: &str) {
        if let Some(phase) = self.sessions.remove(session_id) {
            self.pending_import_count = self.pending_import_count.saturating_sub(1);
            self.total_staged_bytes = self
                .total_staged_bytes
                .saturating_sub(Self::phase_bytes(&phase));
        }
    }

    fn evict_stale(&mut self, now: Instant) {
        let stale = self
            .sessions
            .iter()
            .filter_map(|(session_id, phase)| match phase {
                StateImportPhase::Collecting(pending)
                    if now.saturating_duration_since(pending.last_activity) >= self.stale_after =>
                {
                    Some(session_id.clone())
                }
                StateImportPhase::Collecting(_) | StateImportPhase::Applying { .. } => None,
            })
            .collect::<Vec<_>>();
        for session_id in stale {
            self.discard(&session_id);
        }
    }

    fn complete(&mut self, session_id: &str, import_id: &str) {
        if self.sessions.get(session_id).is_some_and(|phase| {
            matches!(
                phase,
                StateImportPhase::Applying {
                    import_id: active,
                    ..
                } if active == import_id
            )
        }) {
            self.discard(session_id);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn stage(
        &mut self,
        session_id: &str,
        import_id: String,
        batch_seq: usize,
        batch_count: usize,
        batch_digest: String,
        batch_bytes: usize,
        compartments: Vec<StoredCompartment>,
        now: Instant,
    ) -> Result<StateImportStageOutcome, StateImportStageError> {
        self.evict_stale(now);
        let phase = self.sessions.remove(session_id);
        match phase {
            Some(StateImportPhase::Applying {
                import_id: active,
                bytes,
            }) => {
                self.sessions.insert(
                    session_id.to_string(),
                    StateImportPhase::Applying {
                        import_id: active,
                        bytes,
                    },
                );
                Err(StateImportStageError::Protocol {
                    code: "state_import_in_progress",
                    message: "the final state import batch is being applied",
                })
            }
            Some(StateImportPhase::Collecting(mut pending)) => {
                if pending.import_id != import_id || pending.batch_count != batch_count {
                    self.total_staged_bytes = self.total_staged_bytes.saturating_sub(pending.bytes);
                    self.pending_import_count = self.pending_import_count.saturating_sub(1);
                    return Err(StateImportStageError::Protocol {
                        code: "state_import_attempt_mismatch",
                        message: "the import id or batch count changed during staging",
                    });
                }
                if batch_seq < pending.next_seq {
                    let matches = pending
                        .digests
                        .get(batch_seq)
                        .is_some_and(|accepted| accepted == &batch_digest);
                    let staged = pending.compartments.len();
                    if matches {
                        pending.last_activity = now;
                        self.sessions.insert(
                            session_id.to_string(),
                            StateImportPhase::Collecting(pending),
                        );
                        return Ok(StateImportStageOutcome::Staged(staged));
                    }
                    self.total_staged_bytes = self.total_staged_bytes.saturating_sub(pending.bytes);
                    self.pending_import_count = self.pending_import_count.saturating_sub(1);
                    return Err(StateImportStageError::Protocol {
                        code: "state_import_digest_mismatch",
                        message: "a redriven state import batch changed content",
                    });
                }
                if batch_seq > pending.next_seq {
                    self.total_staged_bytes = self.total_staged_bytes.saturating_sub(pending.bytes);
                    self.pending_import_count = self.pending_import_count.saturating_sub(1);
                    return Err(StateImportStageError::Protocol {
                        code: "batch_seq_mismatch",
                        message: "state import batches must arrive contiguously",
                    });
                }
                if let Some(previous) = pending.compartments.last() {
                    if let Some(current) = compartments.first() {
                        if current.sequence <= previous.sequence {
                            self.total_staged_bytes =
                                self.total_staged_bytes.saturating_sub(pending.bytes);
                            self.pending_import_count = self.pending_import_count.saturating_sub(1);
                            return Err(StateImportStageError::Validation(
                                StateImportValidationError::SeqNotIncreasing {
                                    previous: previous.sequence,
                                    current: current.sequence,
                                },
                            ));
                        }
                        if current.start_message <= previous.end_message {
                            self.total_staged_bytes =
                                self.total_staged_bytes.saturating_sub(pending.bytes);
                            self.pending_import_count = self.pending_import_count.saturating_sub(1);
                            return Err(StateImportStageError::Validation(
                                StateImportValidationError::RangesOverlap {
                                    previous: previous.sequence,
                                    current: current.sequence,
                                },
                            ));
                        }
                    }
                }
                let next_bytes = pending.bytes.checked_add(batch_bytes);
                let next_total = self.total_staged_bytes.checked_add(batch_bytes);
                if next_bytes.is_none_or(|bytes| bytes > self.max_staged_bytes)
                    || next_total.is_none_or(|bytes| bytes > self.max_staged_bytes)
                {
                    self.total_staged_bytes = self.total_staged_bytes.saturating_sub(pending.bytes);
                    self.pending_import_count = self.pending_import_count.saturating_sub(1);
                    return Err(StateImportStageError::Protocol {
                        code: "state_import_buffer_overflow",
                        message: "state import staging exceeded the handler-wide byte cap",
                    });
                }
                pending.bytes = next_bytes.unwrap_or(usize::MAX);
                self.total_staged_bytes = next_total.unwrap_or(usize::MAX);
                pending.next_seq += 1;
                pending.digests.push(batch_digest);
                pending.compartments.extend(compartments);
                pending.last_activity = now;
                let staged = pending.compartments.len();
                if batch_seq + 1 == batch_count {
                    let bytes = pending.bytes;
                    let compartments = pending.compartments;
                    self.sessions.insert(
                        session_id.to_string(),
                        StateImportPhase::Applying {
                            import_id: import_id.clone(),
                            bytes,
                        },
                    );
                    Ok(StateImportStageOutcome::Apply {
                        import_id,
                        compartments,
                    })
                } else {
                    self.sessions.insert(
                        session_id.to_string(),
                        StateImportPhase::Collecting(pending),
                    );
                    Ok(StateImportStageOutcome::Staged(staged))
                }
            }
            None => {
                if batch_seq != 0 {
                    return Err(StateImportStageError::Protocol {
                        code: "batch_seq_mismatch",
                        message: "the first state import batch must have batch_seq 0",
                    });
                }
                if self.pending_import_count >= self.max_pending_imports {
                    return Err(StateImportStageError::Protocol {
                        code: "state_import_capacity",
                        message: "too many state imports are already pending",
                    });
                }
                if batch_bytes > self.max_staged_bytes
                    || self
                        .total_staged_bytes
                        .checked_add(batch_bytes)
                        .is_none_or(|bytes| bytes > self.max_staged_bytes)
                {
                    return Err(StateImportStageError::Protocol {
                        code: "state_import_buffer_overflow",
                        message: "state import staging exceeded the handler-wide byte cap",
                    });
                }
                self.pending_import_count += 1;
                self.total_staged_bytes += batch_bytes;
                let staged = compartments.len();
                if batch_count == 1 {
                    self.sessions.insert(
                        session_id.to_string(),
                        StateImportPhase::Applying {
                            import_id: import_id.clone(),
                            bytes: batch_bytes,
                        },
                    );
                    Ok(StateImportStageOutcome::Apply {
                        import_id,
                        compartments,
                    })
                } else {
                    self.sessions.insert(
                        session_id.to_string(),
                        StateImportPhase::Collecting(PendingStateImport {
                            import_id,
                            batch_count,
                            next_seq: 1,
                            digests: vec![batch_digest],
                            compartments,
                            bytes: batch_bytes,
                            last_activity: now,
                        }),
                    );
                    Ok(StateImportStageOutcome::Staged(staged))
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ShadowTransformWire {
    #[serde(default)]
    session_id: Option<String>,
    shadow_generation: u64,
    #[serde(default)]
    seed_pass: bool,
    #[serde(default)]
    pass_seq: Option<u64>,
    #[serde(default)]
    serializer_profile: Option<String>,
    #[serde(default)]
    render_config: Option<String>,
    #[serde(default)]
    full_array_fingerprint: Option<String>,
    #[serde(default)]
    input: Vec<Value>,
    #[serde(default)]
    messages: Vec<crate::ck_wire::CkIngressMessage>,
    #[serde(default)]
    ts_output: Vec<Value>,
    #[serde(default)]
    ts_ck_messages: Vec<crate::ck_wire::CkWireMessage>,
    pass_inputs: ShadowPassInputs,
    #[serde(default)]
    ts_decision: Value,
    #[serde(default)]
    declared_trim: Option<DeclaredTrim>,
    #[serde(default)]
    normalizations: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct ShadowPassInputs {
    now_ms: i64,
    #[serde(default)]
    model_key: Option<String>,
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    usage: Option<ShadowUsageWire>,
    #[serde(
        alias = "effective_execute_threshold",
        alias = "execute_threshold_percentage"
    )]
    effective_execute_threshold: f64,
    #[serde(default)]
    history_budget_tokens: Option<f64>,
    #[serde(default = "default_clear_reasoning_age")]
    clear_reasoning_age: u64,
    #[serde(default = "default_cache_ttl")]
    cache_ttl: String,
    #[serde(default)]
    provider_error: Option<String>,
    /// True only for shadow passes whose newest assistant is still streaming.
    #[serde(default)]
    mid_turn: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ShadowUsageWire {
    #[serde(alias = "current_total_input_tokens")]
    input_tokens: u64,
    #[serde(alias = "context_limit_tokens")]
    limit: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct ShadowCompartmentWire {
    sequence: i64,
    start_message: i64,
    end_message: i64,
    #[serde(default)]
    start_message_id: String,
    #[serde(default)]
    end_message_id: String,
    #[serde(default)]
    start_date: Option<String>,
    #[serde(default)]
    end_date: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    p1: Option<String>,
    #[serde(default)]
    p2: Option<String>,
    #[serde(default)]
    p3: Option<String>,
    #[serde(default)]
    p4: Option<String>,
    #[serde(default = "default_importance")]
    importance: i32,
    #[serde(default)]
    episode_type: Option<String>,
    #[serde(default)]
    legacy: i32,
    #[serde(default)]
    created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct ShadowWorkspaceWire {
    fingerprint: String,
    members: Vec<ShadowWorkspaceMemberWire>,
}

#[derive(Debug, Clone, Deserialize)]
struct ShadowWorkspaceMemberWire {
    project_path: String,
    #[serde(default)]
    share_categories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ShadowMemoryWire {
    id: i64,
    #[serde(default)]
    project_path: Option<String>,
    #[serde(default)]
    category: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    normalized_hash: Option<String>,
    #[serde(default)]
    importance: Option<i32>,
    #[serde(default = "default_memory_scope")]
    scope: String,
    #[serde(default)]
    shareable: i32,
    #[serde(default)]
    source_session_id: Option<String>,
    #[serde(default)]
    source_type: Option<String>,
    #[serde(default = "default_seen_count")]
    seen_count: i64,
    #[serde(default)]
    retrieval_count: i64,
    #[serde(default)]
    first_seen_at: i64,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    last_seen_at: i64,
    #[serde(default)]
    last_retrieved_at: Option<i64>,
    #[serde(default = "default_memory_status")]
    status: String,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default = "default_verification_status")]
    verification_status: String,
    #[serde(default)]
    verified_at: Option<i64>,
    #[serde(default)]
    classified_at: Option<i64>,
    #[serde(default)]
    superseded_by_memory_id: Option<i64>,
    #[serde(default)]
    merged_from: Option<String>,
    #[serde(default)]
    metadata_json: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ShadowMemoryMutationWire {
    id: i64,
    #[serde(default)]
    project_path: Option<String>,
    mutation_type: String,
    target_memory_id: i64,
    #[serde(default)]
    superseded_by_id: Option<i64>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    new_content: Option<String>,
    #[serde(default)]
    queued_at: i64,
}

#[derive(Debug, Serialize)]
struct ShadowReport {
    ok: bool,
    shadow_generation: u64,
    shadow_seq: u64,
    pass_seq: u64,
    quarantined: bool,
    compared: bool,
    class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_mid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_field: Option<String>,
    ts_decision: Value,
    rs_decision: Value,
    state_hash: String,
    normalizations: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay: Option<Value>,
}

#[derive(Debug)]
struct CompareOutcome {
    class: String,
    hard: bool,
    compared: bool,
    first_mid: Option<String>,
    first_block: Option<String>,
    first_field: Option<String>,
    ts_prefix: String,
    rs_prefix: String,
    first_diff_offset: Option<u64>,
    ts_window: String,
    rs_window: String,
}

struct ShadowReportInput {
    shadow_generation: u64,
    pass_seq: u64,
    outcome: CompareOutcome,
    normalizations: Vec<Value>,
    ts_decision: Value,
    rs_decision: Value,
    state_hash: String,
    replay: Option<Value>,
}

struct FacadeScope {
    /// MC project identity for module-store reads and writes.
    memory_project_path: String,
    /// Daemon-bound filesystem path retained only for route-vocabulary enforcement.
    route_project_root: String,
    conversation_key: String,
    memory_enabled: bool,
}

fn default_cache_ttl() -> String {
    "5m".to_string()
}

fn default_clear_reasoning_age() -> u64 {
    50
}

fn default_importance() -> i32 {
    50
}

fn default_memory_scope() -> String {
    "project".to_string()
}

fn default_seen_count() -> i64 {
    1
}

fn default_memory_status() -> String {
    "active".to_string()
}

fn default_verification_status() -> String {
    "unverified".to_string()
}

impl From<ShadowCompartmentWire> for StoredCompartment {
    fn from(value: ShadowCompartmentWire) -> Self {
        StoredCompartment {
            sequence: value.sequence,
            start_message: value.start_message,
            end_message: value.end_message,
            start_message_id: value.start_message_id,
            end_message_id: value.end_message_id,
            start_date: value.start_date,
            end_date: value.end_date,
            title: value.title,
            content: value.content,
            p1: value.p1,
            p2: value.p2,
            p3: value.p3,
            p4: value.p4,
            importance: value.importance,
            episode_type: value.episode_type,
            legacy: value.legacy,
            created_at: value.created_at,
        }
    }
}

impl ShadowMemoryWire {
    fn into_row(self, project_path: String) -> ShadowMemoryRow {
        let normalized_hash = self
            .normalized_hash
            .unwrap_or_else(|| mc_store::compute_normalized_memory_hash(&self.content));
        ShadowMemoryRow {
            id: self.id,
            project_path,
            category: self.category,
            content: self.content,
            normalized_hash,
            importance: self.importance,
            scope: self.scope,
            shareable: self.shareable,
            source_session_id: self.source_session_id,
            source_type: self.source_type,
            seen_count: self.seen_count,
            retrieval_count: self.retrieval_count,
            first_seen_at: self.first_seen_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
            last_seen_at: self.last_seen_at,
            last_retrieved_at: self.last_retrieved_at,
            status: self.status,
            expires_at: self.expires_at,
            verification_status: self.verification_status,
            verified_at: self.verified_at,
            classified_at: self.classified_at,
            superseded_by_memory_id: self.superseded_by_memory_id,
            merged_from: self.merged_from,
            metadata_json: self.metadata_json,
        }
    }
}

impl ShadowMemoryMutationWire {
    fn into_row(self, project_path: String) -> ShadowMemoryMutationRow {
        ShadowMemoryMutationRow {
            project_path,
            mutation: StoredMemoryMutation {
                id: self.id,
                mutation_type: self.mutation_type,
                target_memory_id: self.target_memory_id,
                superseded_by_id: self.superseded_by_id,
                category: self.category,
                new_content: self.new_content,
                queued_at: self.queued_at,
            },
        }
    }
}

enum TransformSnapshot {
    InFlight {
        generation: u64,
    },
    Ready {
        generation: u64,
        request: Arc<TransformRequest>,
        revert_epoch: u64,
        retained_bytes: usize,
    },
}

struct SnapshotLeaseBudget {
    bytes: usize,
    count: usize,
    max_bytes: usize,
    max_count: usize,
}

struct SnapshotLease {
    generation: u64,
    request: Arc<TransformRequest>,
    revert_epoch: u64,
    retained_bytes: usize,
    budget: Arc<Mutex<SnapshotLeaseBudget>>,
}

impl Drop for SnapshotLease {
    fn drop(&mut self) {
        let mut budget = self.budget.lock().expect("snapshot lease budget mutex");
        budget.bytes = budget.bytes.saturating_sub(self.retained_bytes);
        budget.count = budget.count.saturating_sub(1);
    }
}

enum TransformSnapshotLookup {
    Missing,
    InFlight,
    LeaseBudgetExceeded,
    Ready(SnapshotLease),
}

struct TransformSnapshotCache {
    entries: HashMap<String, TransformSnapshot>,
    ready_lru: VecDeque<String>,
    /// Insertion-ordered InFlight sessions. Failed or rejected transforms never
    /// reach `finish_ready`, so without its own bound this class of entry would
    /// grow with every unique failing session for the process lifetime.
    in_flight_lru: VecDeque<String>,
    ready_bytes: usize,
    next_generation: u64,
    max_ready_bytes: usize,
    max_in_flight_entries: usize,
    // Map eviction cannot reclaim requests held by active wrapups. This shared budget
    // follows those Arc leases independently until their RAII guards are dropped.
    active_leases: Arc<Mutex<SnapshotLeaseBudget>>,
}

impl TransformSnapshotCache {
    fn new(max_ready_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            ready_lru: VecDeque::new(),
            in_flight_lru: VecDeque::new(),
            ready_bytes: 0,
            next_generation: 0,
            max_ready_bytes,
            max_in_flight_entries: MAX_IN_FLIGHT_SNAPSHOT_ENTRIES,
            active_leases: Arc::new(Mutex::new(SnapshotLeaseBudget {
                bytes: 0,
                count: 0,
                max_bytes: ACTIVE_SNAPSHOT_LEASE_BUDGET_BYTES,
                max_count: MAX_ACTIVE_SNAPSHOT_LEASES,
            })),
        }
    }

    fn remove_ready_charge(&mut self, session_id: &str) {
        if let Some(TransformSnapshot::Ready { retained_bytes, .. }) = self.entries.get(session_id)
        {
            self.ready_bytes = self.ready_bytes.saturating_sub(*retained_bytes);
        }
        self.ready_lru.retain(|candidate| candidate != session_id);
    }

    fn begin(&mut self, session_id: &str) -> u64 {
        self.remove_ready_charge(session_id);
        self.next_generation = self.next_generation.saturating_add(1);
        let generation = self.next_generation;
        self.entries.insert(
            session_id.to_string(),
            TransformSnapshot::InFlight { generation },
        );
        self.in_flight_lru
            .retain(|candidate| candidate != session_id);
        self.in_flight_lru.push_back(session_id.to_string());
        // Evicting an InFlight entry to Missing is correctness-safe: wrapup refuses
        // Missing, and finish_ready's generation match refuses to resurrect an
        // evicted session's stale snapshot.
        while self.in_flight_lru.len() > self.max_in_flight_entries {
            let Some(oldest) = self.in_flight_lru.pop_front() else {
                break;
            };
            if matches!(
                self.entries.get(&oldest),
                Some(TransformSnapshot::InFlight { .. })
            ) {
                self.entries.remove(&oldest);
            }
        }
        generation
    }

    fn finish_ready(
        &mut self,
        session_id: &str,
        generation: u64,
        request: Arc<TransformRequest>,
        revert_epoch: u64,
        retained_bytes: usize,
    ) {
        let matches_current = matches!(
            self.entries.get(session_id),
            Some(TransformSnapshot::InFlight { generation: current }) if *current == generation
        );
        if !matches_current {
            return;
        }
        self.in_flight_lru
            .retain(|candidate| candidate != session_id);
        if retained_bytes > self.max_ready_bytes {
            self.entries.remove(session_id);
            return;
        }

        self.entries.insert(
            session_id.to_string(),
            TransformSnapshot::Ready {
                generation,
                request,
                revert_epoch,
                retained_bytes,
            },
        );
        self.ready_lru.push_back(session_id.to_string());
        self.ready_bytes = self.ready_bytes.saturating_add(retained_bytes);
        while self.ready_bytes > self.max_ready_bytes {
            let Some(oldest) = self.ready_lru.pop_front() else {
                break;
            };
            if let Some(TransformSnapshot::Ready { retained_bytes, .. }) =
                self.entries.remove(&oldest)
            {
                self.ready_bytes = self.ready_bytes.saturating_sub(retained_bytes);
            }
        }
    }

    fn get(&mut self, session_id: &str) -> TransformSnapshotLookup {
        match self.entries.get(session_id) {
            Some(TransformSnapshot::InFlight { .. }) => TransformSnapshotLookup::InFlight,
            Some(TransformSnapshot::Ready {
                generation,
                request,
                revert_epoch,
                retained_bytes,
            }) => {
                let mut budget = self
                    .active_leases
                    .lock()
                    .expect("snapshot lease budget mutex");
                let Some(next_bytes) = budget.bytes.checked_add(*retained_bytes) else {
                    return TransformSnapshotLookup::LeaseBudgetExceeded;
                };
                if budget.count >= budget.max_count || next_bytes > budget.max_bytes {
                    return TransformSnapshotLookup::LeaseBudgetExceeded;
                }
                budget.bytes = next_bytes;
                budget.count += 1;
                let ready = SnapshotLease {
                    generation: *generation,
                    request: Arc::clone(request),
                    revert_epoch: *revert_epoch,
                    retained_bytes: *retained_bytes,
                    budget: Arc::clone(&self.active_leases),
                };
                drop(budget);
                self.ready_lru.retain(|candidate| candidate != session_id);
                self.ready_lru.push_back(session_id.to_string());
                TransformSnapshotLookup::Ready(ready)
            }
            None => TransformSnapshotLookup::Missing,
        }
    }

    fn ready_generation_matches(&self, session_id: &str, generation: u64) -> bool {
        matches!(
            self.entries.get(session_id),
            Some(TransformSnapshot::Ready { generation: current, .. }) if *current == generation
        )
    }

    fn generation_present_in_flight_or_ready(&self, session_id: &str, generation: u64) -> bool {
        matches!(
            self.entries.get(session_id),
            Some(TransformSnapshot::InFlight { generation: current })
                | Some(TransformSnapshot::Ready {
                    generation: current,
                    ..
                }) if *current == generation
        )
    }

    fn remove(&mut self, session_id: &str) {
        self.remove_ready_charge(session_id);
        self.in_flight_lru
            .retain(|candidate| candidate != session_id);
        self.entries.remove(session_id);
    }
}

/// The module handler. Holds the single store handle (opened once in `on_hello_ack`)
/// and the per-route session bindings (route channel → {project, session}).
pub struct McHandler {
    store: OnceLock<Arc<McStore>>,
    producer_factory: Arc<dyn HistorianProducerFactory>,
    session_resolver: Arc<dyn SessionResolver>,
    config: Mutex<ConfigCache>,
    #[cfg(test)]
    fixed_config: Option<McModuleConfig>,
    reattaching_sessions: Arc<Mutex<HashSet<String>>>,
    live_historian_sessions: Arc<Mutex<HashMap<String, LiveHistorianSession>>>,
    wrapup_sessions: Arc<Mutex<HashMap<String, LiveWrapupSession>>>,
    recomp_sessions: Arc<Mutex<HashSet<String>>>,
    transform_snapshots: Arc<Mutex<TransformSnapshotCache>>,
    scheduler_observations: Mutex<HashMap<String, SchedulerObservation>>,
    guidance_dates: Mutex<HashMap<String, String>>,
    #[cfg(test)]
    guidance_now_ms: Mutex<Option<i64>>,
    #[cfg(test)]
    reduction_injection: Mutex<HashMap<String, Vec<ReductionDecision>>>,
    /// Test-only interleave seam: runs once between the request's transform and the
    /// Emergency95 prepare, where a concurrent publish is otherwise impossible to place
    /// deterministically.
    #[cfg(test)]
    between_transform_and_prepare: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    wrapup_operation_budget: Mutex<Option<Duration>>,
    #[cfg(test)]
    unknown_module_retry_delay: Mutex<Option<Duration>>,
    #[cfg(test)]
    status_snapshot_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    classification_before_apply_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    connect_failure_commit_hook: ConnectFailureCommitHook,
    #[cfg(test)]
    publication_fence_write_hook: ConnectFailureCommitHook,
    /// Route channel → its session binding. Populated at `on_bind`, removed at
    /// `on_route_gone`. The SDK validates the route handle's epoch before dispatching a
    /// request, so a channel key cannot resolve a stale route. A `Mutex<HashMap>` (not a
    /// lock-free map) is appropriate because writes are rare (once per route open/close)
    /// and reads are one cheap lookup per transform.
    bindings: Mutex<HashMap<u16, SessionBinding>>,
    /// Validated transform channel → (session, route root). The root is part of provenance;
    /// a cache row for the same session cannot authenticate a facade opened on another root.
    transform_route_channels: Mutex<HashMap<u16, (String, PathBuf)>>,
    /// Roots previously observed on a validated transform for each session. This survives route
    /// teardown so durable cache state remains usable only along an authenticated route lineage.
    transform_session_roots: Mutex<HashMap<String, HashSet<PathBuf>>>,
    shadow_seeds: Mutex<ShadowSeedCoordinator>,
    transform_pages: Mutex<TransformPageCoordinator>,
    state_imports: Mutex<StateImportCoordinator>,
    /// Module-minted zero-tool dreamer sessions. Prefixes are diagnostics only;
    /// only registered ids may bypass transform after route validation.
    active_dreamer_runs: Arc<Mutex<HashSet<String>>>,
}

#[async_trait]
pub trait HistorianProducerFactory: Send + Sync {
    async fn connect(
        &self,
        project_root: &Path,
    ) -> Result<Box<dyn HistorianProducerDriver + Send>, HistorianProducerError>;
}

struct RealHistorianProducerFactory {
    connection_file: PathBuf,
}

#[async_trait]
impl HistorianProducerFactory for RealHistorianProducerFactory {
    async fn connect(
        &self,
        project_root: &Path,
    ) -> Result<Box<dyn HistorianProducerDriver + Send>, HistorianProducerError> {
        Ok(Box::new(
            HistorianProducer::connect(HistorianProducerConfig {
                handshake_timeout: Duration::from_secs(2),
                ..HistorianProducerConfig::new(
                    self.connection_file.clone(),
                    project_root,
                    "opencode",
                )
            })
            .await?,
        ))
    }
}

struct MissingProducerFactory;

struct DreamerRunGuard {
    registry: Arc<Mutex<HashSet<String>>>,
    session_id: String,
}

impl Drop for DreamerRunGuard {
    fn drop(&mut self) {
        self.registry
            .lock()
            .expect("dreamer registry mutex")
            .remove(&self.session_id);
    }
}

struct StringSetGuard {
    sessions: Arc<Mutex<HashSet<String>>>,
    session_id: String,
}

impl Drop for StringSetGuard {
    fn drop(&mut self) {
        self.sessions
            .lock()
            .expect("session set mutex")
            .remove(&self.session_id);
    }
}

type LiveHistorianCompletionWait = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Clone)]
struct LiveHistorianSession {
    token: Arc<()>,
    completion: Arc<Notify>,
}

struct SessionSetGuard {
    sessions: Arc<Mutex<HashMap<String, LiveHistorianSession>>>,
    session_id: String,
    token: Arc<()>,
    completion: Arc<Notify>,
}

impl Drop for SessionSetGuard {
    fn drop(&mut self) {
        let mut sessions = self.sessions.lock().expect("session set mutex");
        self.completion.notify_waiters();
        let matches_current = sessions
            .get(&self.session_id)
            .is_some_and(|live| Arc::ptr_eq(&live.token, &self.token));
        if matches_current {
            sessions.remove(&self.session_id);
        }
    }
}

enum LiveHistorianSessionClaim {
    Acquired(SessionSetGuard),
    Busy(LiveHistorianCompletionWait),
}

struct PreparedHistorianFiring {
    diagnostics: HistorianDiagnostics,
    task: HistorianFiringTask,
}

enum PreparedHistorianAction {
    Complete(HistorianDiagnostics),
    Busy {
        diagnostics: HistorianDiagnostics,
        completion: LiveHistorianCompletionWait,
    },
    FireReady(Box<PreparedHistorianFiring>),
}

struct HistorianPrepareContext {
    now: i64,
    snapshot_generation: u64,
}

struct WrapupPrepareContext {
    now: i64,
    project_path: String,
    allow_unknown_module_retry: bool,
}

#[derive(Clone)]
struct LiveWrapupSession {
    token: Arc<()>,
    rounds: usize,
}

struct WrapupSessionGuard {
    sessions: Arc<Mutex<HashMap<String, LiveWrapupSession>>>,
    session_id: String,
    token: Arc<()>,
}

impl WrapupSessionGuard {
    fn set_rounds(&self, rounds: usize) {
        let mut sessions = self.sessions.lock().expect("wrapup sessions mutex");
        if let Some(session) = sessions
            .get_mut(&self.session_id)
            .filter(|session| Arc::ptr_eq(&session.token, &self.token))
        {
            session.rounds = rounds;
        }
    }
}

impl Drop for WrapupSessionGuard {
    fn drop(&mut self) {
        let mut sessions = self.sessions.lock().expect("wrapup sessions mutex");
        let matches_current = sessions
            .get(&self.session_id)
            .is_some_and(|session| Arc::ptr_eq(&session.token, &self.token));
        if matches_current {
            sessions.remove(&self.session_id);
        }
    }
}

enum PreparedWrapupAction {
    Busy(LiveHistorianCompletionWait),
    Nothing(String),
    FireReady(Box<HistorianFiringTask>),
    Failed(String),
}

struct TerminalWrapupResponse {
    disposition: &'static str,
    rounds: usize,
    summary: String,
    reason: Option<&'static str>,
    detail: Option<String>,
    include_rounds_without_command: bool,
}

#[derive(Debug)]
enum WrapupFiringError {
    Retryable(RetryableWrapupReason, String),
    UnknownModule(String),
    Terminal {
        reason: &'static str,
        detail: String,
    },
}

const TERMINAL_WRAPUP_FAILURE_PREFIX: &str = "mc-terminal-wrapup-failure:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryableWrapupReason {
    BackoffActive,
    SnapshotUnavailable,
    SnapshotStale,
    BudgetExhausted,
}

impl RetryableWrapupReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::BackoffActive => "backoff_active",
            Self::SnapshotUnavailable => "snapshot_unavailable",
            Self::SnapshotStale => "snapshot_stale",
            Self::BudgetExhausted => "budget_exhausted",
        }
    }
}

fn terminal_wrapup_failure_summary(reason: &str, summary: &str, detail: &str) -> String {
    format!(
        "{TERMINAL_WRAPUP_FAILURE_PREFIX}{}",
        json!({ "reason": reason, "summary": summary, "detail": detail })
    )
}

fn terminal_wrapup_failure_fields(summary: &str) -> Option<(String, String, String)> {
    let encoded = summary.strip_prefix(TERMINAL_WRAPUP_FAILURE_PREFIX)?;
    let value: Value = serde_json::from_str(encoded).ok()?;
    Some((
        value.get("reason")?.as_str()?.to_string(),
        value.get("summary")?.as_str()?.to_string(),
        value.get("detail")?.as_str()?.to_string(),
    ))
}

type ConnectFailureCommitHook = Arc<Mutex<Option<Box<dyn FnMut() + Send>>>>;

struct WrapupSnapshotPublicationFence {
    snapshots: Arc<Mutex<TransformSnapshotCache>>,
    session_id: String,
    generation: u64,
    #[cfg(test)]
    after_store_publish: ConnectFailureCommitHook,
}

impl historian::HistorianPublicationFence for WrapupSnapshotPublicationFence {
    fn publish(
        &self,
        store: &McStore,
        request: mc_store::HistorianPublishRequest<'_>,
    ) -> Result<mc_store::HistorianPublishResult, mc_store::HistorianPublishError> {
        // Validation and the bounded local SQLite write share this lock so a transform
        // cannot retire the cached raw snapshot between the check and additive writes.
        let snapshots = self.snapshots.lock().expect("transform snapshots mutex");
        if !snapshots.ready_generation_matches(&self.session_id, self.generation) {
            return Err(mc_store::HistorianPublishError::FenceRejected {
                reason: "transform snapshot generation changed before publication".to_string(),
            });
        }
        let published = store.publish_historian_chunk(request);
        #[cfg(test)]
        if let Some(hook) = self
            .after_store_publish
            .lock()
            .expect("publication fence write hook mutex")
            .as_mut()
        {
            hook();
        }
        published
    }
}

struct ReattachSnapshotPublicationFence {
    snapshots: Arc<Mutex<TransformSnapshotCache>>,
    session_id: String,
    generation: u64,
    #[cfg(test)]
    after_store_publish: ConnectFailureCommitHook,
}

impl historian::HistorianPublicationFence for ReattachSnapshotPublicationFence {
    fn publish(
        &self,
        store: &McStore,
        request: mc_store::HistorianPublishRequest<'_>,
    ) -> Result<mc_store::HistorianPublishResult, mc_store::HistorianPublishError> {
        // Keep the cache check and database write under one lock. A later transform
        // then cannot replace the messages selected by this request before the
        // corresponding history rows are stored.
        let snapshots = self.snapshots.lock().expect("transform snapshots mutex");
        if !snapshots.generation_present_in_flight_or_ready(&self.session_id, self.generation) {
            return Err(mc_store::HistorianPublishError::FenceRejected {
                reason: "transform snapshot state changed after reattach started".to_string(),
            });
        }
        let published = store.publish_historian_chunk(request);
        #[cfg(test)]
        if let Some(hook) = self
            .after_store_publish
            .lock()
            .expect("publication fence write hook mutex")
            .as_mut()
        {
            hook();
        }
        published
    }
}

struct HistorianFiringTask {
    store: Arc<McStore>,
    session_id: String,
    project_path: String,
    project_root: PathBuf,
    project_slug: String,
    firing: AssembledHistorianFiring,
    live_guard: SessionSetGuard,
    connect_failure_commit_hook: ConnectFailureCommitHook,
    publication_fence: Option<Arc<dyn historian::HistorianPublicationFence>>,
}

#[derive(Debug, Clone, Copy)]
struct SchedulerObservation {
    last_response_at_ms: i64,
    observed_in_process: bool,
}

#[async_trait]
impl HistorianProducerFactory for MissingProducerFactory {
    async fn connect(
        &self,
        _project_root: &Path,
    ) -> Result<Box<dyn HistorianProducerDriver + Send>, HistorianProducerError> {
        Err(HistorianProducerError::NoEndpoint {
            path: PathBuf::from("<missing --subc>"),
        })
    }
}

impl McHandler {
    pub fn new() -> Self {
        Self::new_with_connection_file(None)
    }

    pub fn new_with_connection_file(connection_file: Option<PathBuf>) -> Self {
        let producer_factory: Arc<dyn HistorianProducerFactory> = match connection_file.clone() {
            Some(path) => Arc::new(RealHistorianProducerFactory {
                connection_file: path,
            }),
            None => Arc::new(MissingProducerFactory),
        };
        let session_resolver: Arc<dyn SessionResolver> = match connection_file {
            Some(path) => Arc::new(RealSessionResolver::new(path)),
            None => Arc::new(MissingSessionResolver),
        };
        McHandler {
            store: OnceLock::new(),
            producer_factory,
            session_resolver,
            config: Mutex::new(ConfigCache::default()),
            #[cfg(test)]
            fixed_config: None,
            reattaching_sessions: Arc::new(Mutex::new(HashSet::new())),
            live_historian_sessions: Arc::new(Mutex::new(HashMap::new())),
            wrapup_sessions: Arc::new(Mutex::new(HashMap::new())),
            recomp_sessions: Arc::new(Mutex::new(HashSet::new())),
            transform_snapshots: Arc::new(Mutex::new(TransformSnapshotCache::new(
                TRANSFORM_SNAPSHOT_BUDGET_BYTES,
            ))),
            scheduler_observations: Mutex::new(HashMap::new()),
            guidance_dates: Mutex::new(HashMap::new()),
            #[cfg(test)]
            guidance_now_ms: Mutex::new(None),
            #[cfg(test)]
            reduction_injection: Mutex::new(HashMap::new()),
            #[cfg(test)]
            between_transform_and_prepare: Mutex::new(None),
            #[cfg(test)]
            wrapup_operation_budget: Mutex::new(None),
            #[cfg(test)]
            unknown_module_retry_delay: Mutex::new(None),
            #[cfg(test)]
            status_snapshot_hook: Mutex::new(None),
            #[cfg(test)]
            classification_before_apply_hook: Mutex::new(None),
            connect_failure_commit_hook: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            publication_fence_write_hook: Arc::new(Mutex::new(None)),
            bindings: Mutex::new(HashMap::new()),
            transform_route_channels: Mutex::new(HashMap::new()),
            transform_session_roots: Mutex::new(HashMap::new()),
            shadow_seeds: Mutex::new(ShadowSeedCoordinator::default()),
            transform_pages: Mutex::new(TransformPageCoordinator::default()),
            state_imports: Mutex::new(StateImportCoordinator::default()),
            active_dreamer_runs: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn with_producer_factory(factory: Arc<dyn HistorianProducerFactory>) -> Self {
        Self::with_producer_factory_and_config(
            factory,
            McModuleConfig {
                model_chain: vec!["test/model".to_string()],
                execute_threshold_percentage: 65.0,
                memory_enabled: true,
                smart_drops: false,
                cache_ttl: "5m".to_string(),
                shadow_enabled: true,
            },
        )
    }

    #[cfg(test)]
    fn with_producer_factory_and_config(
        factory: Arc<dyn HistorianProducerFactory>,
        config: McModuleConfig,
    ) -> Self {
        Self::with_producer_factory_config_resolver(
            factory,
            config,
            Arc::new(MissingSessionResolver),
        )
    }

    #[cfg(test)]
    fn with_producer_factory_config_resolver(
        factory: Arc<dyn HistorianProducerFactory>,
        config: McModuleConfig,
        session_resolver: Arc<dyn SessionResolver>,
    ) -> Self {
        McHandler {
            store: OnceLock::new(),
            producer_factory: factory,
            session_resolver,
            config: Mutex::new(ConfigCache::default()),
            fixed_config: Some(config),
            reattaching_sessions: Arc::new(Mutex::new(HashSet::new())),
            live_historian_sessions: Arc::new(Mutex::new(HashMap::new())),
            wrapup_sessions: Arc::new(Mutex::new(HashMap::new())),
            recomp_sessions: Arc::new(Mutex::new(HashSet::new())),
            transform_snapshots: Arc::new(Mutex::new(TransformSnapshotCache::new(
                TRANSFORM_SNAPSHOT_BUDGET_BYTES,
            ))),
            scheduler_observations: Mutex::new(HashMap::new()),
            guidance_dates: Mutex::new(HashMap::new()),
            guidance_now_ms: Mutex::new(None),
            reduction_injection: Mutex::new(HashMap::new()),
            between_transform_and_prepare: Mutex::new(None),
            wrapup_operation_budget: Mutex::new(None),
            unknown_module_retry_delay: Mutex::new(None),
            status_snapshot_hook: Mutex::new(None),
            classification_before_apply_hook: Mutex::new(None),
            connect_failure_commit_hook: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            publication_fence_write_hook: Arc::new(Mutex::new(None)),
            bindings: Mutex::new(HashMap::new()),
            transform_route_channels: Mutex::new(HashMap::new()),
            transform_session_roots: Mutex::new(HashMap::new()),
            shadow_seeds: Mutex::new(ShadowSeedCoordinator::default()),
            transform_pages: Mutex::new(TransformPageCoordinator::default()),
            state_imports: Mutex::new(StateImportCoordinator::default()),
            active_dreamer_runs: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Record the route's session binding (called from `on_bind`). Last write wins for a
    /// reused channel — the daemon won't reuse a channel without a `route.gone` first, so
    /// this only overwrites a stale entry that somehow survived (defensive).
    fn bind_route(&self, channel: u16, binding: SessionBinding) {
        self.transform_route_channels
            .lock()
            .expect("transform route channels mutex")
            .remove(&channel);
        let replacement = {
            let mut bindings = self.bindings.lock().expect("bindings mutex");
            let previous = bindings.insert(channel, binding);
            previous.and_then(|previous| {
                let still_bound = bindings
                    .values()
                    .any(|candidate| candidate.session == previous.session);
                (!still_bound).then_some(previous.session)
            })
        };
        if let Some(session_id) = replacement {
            self.shadow_seeds
                .lock()
                .expect("shadow seed mutex")
                .evict(&session_id);
            self.transform_pages
                .lock()
                .expect("transform page mutex")
                .discard(&session_id);
            self.state_imports
                .lock()
                .expect("state import mutex")
                .discard(&session_id);
            self.transform_snapshots
                .lock()
                .expect("transform snapshots mutex")
                .remove(&session_id);
        }
    }

    fn discard_shadow_seed(&self, session_id: &str) {
        self.shadow_seeds
            .lock()
            .expect("shadow seed mutex")
            .discard_pending(session_id);
    }

    fn shadow_seed_in_progress(&self, session_id: &str) -> bool {
        self.shadow_seeds
            .lock()
            .expect("shadow seed mutex")
            .sessions
            .get(session_id)
            .is_some_and(|state| ShadowSeedCoordinator::is_pending(&state.phase))
    }

    fn discard_transform_pages(&self, session_id: &str) {
        self.transform_pages
            .lock()
            .expect("transform page mutex")
            .discard(session_id);
    }

    fn transform_page_in_progress(&self, session_id: &str) -> bool {
        self.transform_pages
            .lock()
            .expect("transform page mutex")
            .sessions
            .get(session_id)
            .is_some_and(|state| TransformPageCoordinator::is_pending(&state.phase))
    }

    /// Remove a route and evict process-local session state after its final binding closes.
    fn unbind_route(&self, channel: u16) {
        self.transform_route_channels
            .lock()
            .expect("transform route channels mutex")
            .remove(&channel);
        let last_session_route = {
            let mut bindings = self.bindings.lock().expect("bindings mutex");
            bindings.remove(&channel).and_then(|binding| {
                let still_bound = bindings
                    .values()
                    .any(|candidate| candidate.session == binding.session);
                (!still_bound).then_some(binding.session)
            })
        };
        if let Some(session) = last_session_route {
            if session.starts_with("mc-dreamer:") {
                self.unregister_dreamer_run(&session);
            }
            self.scheduler_observations
                .lock()
                .expect("scheduler observations mutex")
                .remove(&session);
            self.shadow_seeds
                .lock()
                .expect("shadow seed mutex")
                .evict(&session);
            self.transform_pages
                .lock()
                .expect("transform page mutex")
                .discard(&session);
            self.state_imports
                .lock()
                .expect("state import mutex")
                .discard(&session);
            self.transform_snapshots
                .lock()
                .expect("transform snapshots mutex")
                .remove(&session);
        }
    }

    /// Resolve the binding for a transform request on `channel`, FAIL-LOUD: the channel
    /// must be bound AND its bound session must match the request's `session_id`. Returns
    /// the full binding (project_root + frozen budget) the caller keys its store reads off,
    /// never a default. The resolve-or-reject is enforced from the start, and it changes no
    /// transform output — a correctly-bound request resolves and proceeds identically.
    fn resolve_binding(
        &self,
        channel: u16,
        request_session: &str,
    ) -> Result<SessionBinding, BindingError> {
        let map = self.bindings.lock().expect("bindings mutex");
        let binding = map.get(&channel).ok_or(BindingError::Unbound)?;
        if binding.session != request_session {
            return Err(BindingError::SessionMismatch);
        }
        Ok(binding.clone())
    }

    fn shadow_binding(
        &self,
        channel: u16,
        request_session: Option<&str>,
    ) -> Result<SessionBinding, HandlerOutcome> {
        let binding = self
            .bindings
            .lock()
            .expect("bindings mutex")
            .get(&channel)
            .cloned()
            .ok_or_else(|| HandlerOutcome::Error {
                code: "route_unbound".to_string(),
                message: "shadow op on a channel with no session binding".to_string(),
            })?;
        if let Some(request_session) = request_session {
            if binding.session != request_session {
                return Err(HandlerOutcome::Error {
                    code: "session_mismatch".to_string(),
                    message: "request session_id does not match the channel's bound session"
                        .to_string(),
                });
            }
        }
        if !is_shadow_session(&binding.session) {
            return Err(HandlerOutcome::Error {
                code: "shadow_binding_required".to_string(),
                message: "shadow ops require a route bound as shadow:<real_session>".to_string(),
            });
        }
        Ok(binding)
    }

    fn state_sync_binding(
        &self,
        channel: u16,
        request_session: Option<&str>,
    ) -> Result<SessionBinding, HandlerOutcome> {
        let binding = self
            .bindings
            .lock()
            .expect("bindings mutex")
            .get(&channel)
            .cloned()
            .ok_or_else(|| HandlerOutcome::Error {
                code: "route_unbound".to_string(),
                message: "state sync on a channel with no session binding".to_string(),
            })?;
        if let Some(request_session) = request_session {
            if binding.session != request_session {
                return Err(HandlerOutcome::Error {
                    code: "session_mismatch".to_string(),
                    message: "request session_id does not match the channel's bound session"
                        .to_string(),
                });
            }
        }
        Ok(binding)
    }

    fn evaluate_shadow_historian(
        &self,
        store: &McStore,
        parsed: &TransformRequest,
        projection: &crate::ck_wire::FlatProjection,
        pass_inputs: &ShadowPassInputs,
    ) -> HistorianDiagnostics {
        let loaded = match store.load(&parsed.session_id) {
            Ok(loaded) => loaded,
            Err(e) => {
                return HistorianDiagnostics {
                    fired: false,
                    reason: None,
                    no_fire: Some(format!("state_load_failed:{e}")),
                    state: "unknown".to_string(),
                    progress: None,
                    last_failure: None,
                }
            }
        };
        let state = loaded.meta.historian.state.as_str().to_string();
        let last_failure = loaded.meta.historian.last_failure.clone();
        if loaded.meta.pending_rewrite.is_some() {
            return HistorianDiagnostics {
                fired: false,
                reason: None,
                no_fire: Some("pending_rewrite".to_string()),
                state,
                progress: None,
                last_failure,
            };
        }
        let boundary_messages = boundary_messages(parsed, projection);
        let last_compartment_end_ordinal = store
            .load_compartments(&parsed.session_id)
            .ok()
            .and_then(|cs| cs.iter().map(|c| c.end_message as u64).max());
        let (context_limit, input_tokens, usage_percentage) = usage_numbers(parsed.usage.as_ref());
        let serializer_profile = SerializerProfile::parse(&parsed.serializer_profile)
            .expect("serializer_profile validated upstream");
        let fold_is_only_reclaim = !tail_reclaim(serializer_profile);
        let trigger = boundary::check_compartment_trigger(
            &boundary_messages,
            &TriggerContext {
                boundary: BoundaryContext {
                    context_limit,
                    execute_threshold_percentage: pass_inputs.effective_execute_threshold,
                    usage_percentage,
                    usage_input_tokens: input_tokens,
                    last_compartment_end_ordinal,
                    prior_boundary_ordinal: last_compartment_end_ordinal.unwrap_or(0),
                    migration_floor_active: last_compartment_end_ordinal.unwrap_or(0) > 0,
                    emergency_tail_scale: None,
                    trigger_budget: None,
                    fold_is_only_reclaim,
                },
                projected_post_drop_percentage: None,
                compartment_in_progress: loaded.meta.historian.state != HistorianPhase::Idle,
                commit_cluster_trigger_enabled: DEFAULT_COMMIT_CLUSTER_TRIGGER_ENABLED,
                min_commit_clusters: DEFAULT_MIN_COMMIT_CLUSTERS,
            },
        );
        let progress = trigger
            .progress
            .as_ref()
            .map(|p| transform::HistorianTriggerProgress {
                eligible_chunk_tokens: p.eligible_chunk_tokens,
                tail_size_bar: p.tail_size_bar,
                protected_tail_n_tokens: p.n_tokens,
                protected_start_ordinal: p.protected_start_ordinal,
            });
        HistorianDiagnostics {
            fired: trigger.fire,
            reason: trigger.reason.map(|r| r.as_str().to_string()),
            no_fire: (!trigger.fire).then_some(
                if loaded.meta.historian.state == HistorianPhase::Idle {
                    "trigger_false".to_string()
                } else {
                    "busy".to_string()
                },
            ),
            state,
            progress,
            last_failure,
        }
    }

    fn record_shadow_report(
        &self,
        store: &McStore,
        session_id: &str,
        input: ShadowReportInput,
    ) -> HandlerOutcome {
        let ShadowReportInput {
            shadow_generation,
            pass_seq,
            outcome,
            normalizations,
            ts_decision,
            rs_decision,
            state_hash,
            replay,
        } = input;
        let normalizations_json = serde_json::to_string(&normalizations).unwrap_or_default();
        let ts_decision_json = serde_json::to_string(&ts_decision).unwrap_or_default();
        let rs_decision_json = serde_json::to_string(&rs_decision).unwrap_or_default();
        let quarantined = if outcome.class == "identical" {
            store
                .load(session_id)
                .map(|state| state.meta.shadow_quarantined)
                .unwrap_or(false)
        } else {
            match store.record_shadow_divergence(ShadowDivergenceRecord {
                session_id,
                shadow_generation,
                pass_seq,
                class: &outcome.class,
                first_mid: outcome.first_mid.as_deref(),
                first_block: outcome.first_block.as_deref(),
                first_field: outcome.first_field.as_deref(),
                ts_prefix: &outcome.ts_prefix,
                rs_prefix: &outcome.rs_prefix,
                first_diff_offset: outcome.first_diff_offset,
                ts_window: &outcome.ts_window,
                rs_window: &outcome.rs_window,
                normalizations_json: &normalizations_json,
                ts_decision_json: &ts_decision_json,
                rs_decision_json: &rs_decision_json,
                state_hash: &state_hash,
                created_at_ms: now_ms(),
                quarantine: outcome.hard,
            }) {
                Ok(write) => write.quarantined,
                Err(e) => {
                    return HandlerOutcome::Error {
                        code: "shadow_divergence_write_failed".to_string(),
                        message: e.to_string(),
                    }
                }
            }
        };
        let shadow_seq = store
            .load(session_id)
            .map(|state| state.meta.shadow_seq)
            .unwrap_or(0);
        respond(
            serde_json::to_value(ShadowReport {
                ok: true,
                shadow_generation,
                shadow_seq,
                pass_seq,
                quarantined,
                compared: outcome.compared,
                class: outcome.class,
                first_mid: outcome.first_mid,
                first_block: outcome.first_block,
                first_field: outcome.first_field,
                ts_decision,
                rs_decision,
                state_hash,
                normalizations,
                replay,
            })
            .unwrap_or(Value::Null),
        )
    }

    /// Return the channel binding without comparing a request session. OpenCode Rust facade
    /// routes bind a real session id, while Claude Code facade routes bind an instance token;
    /// `resolve_facade_scope` applies the corresponding identity mode before touching the store.
    fn facade_binding(&self, channel: u16) -> Result<SessionBinding, BindingError> {
        self.bindings
            .lock()
            .expect("bindings mutex")
            .get(&channel)
            .cloned()
            .ok_or(BindingError::Unbound)
    }

    fn module_knows_transform_session(&self, session_id: &str, project_root: &Path) -> bool {
        let canonical_project_root = canonical_root(project_root);
        let root_observed = self
            .transform_session_roots
            .lock()
            .expect("transform session roots mutex")
            .get(session_id)
            .is_some_and(|roots| {
                roots
                    .iter()
                    .any(|root| canonical_root(root) == canonical_project_root)
            });
        if !root_observed {
            let Some(store) = self.store.get() else {
                return false;
            };
            let durable_root_observed = canonical_project_root.to_str().is_some_and(|root| {
                store
                    .knows_transform_session_root(session_id, root)
                    .unwrap_or(false)
            });
            if !durable_root_observed || !store.has_cache_state(session_id).unwrap_or(false) {
                return false;
            }
            // Cache the durable proof after a process restart. The row pairs the canonical root
            // with the accepted transform commit, so a genuinely different root cannot authorize
            // the same session.
            self.transform_session_roots
                .lock()
                .expect("transform session roots mutex")
                .entry(session_id.to_string())
                .or_default()
                .insert(canonical_project_root.clone());
            return true;
        }
        if self
            .store
            .get()
            .is_some_and(|store| store.has_cache_state(session_id).unwrap_or(false))
        {
            return true;
        }
        self.transform_route_channels
            .lock()
            .expect("transform route channels mutex")
            .values()
            .any(|(session, root)| {
                session == session_id && canonical_root(root) == canonical_project_root
            })
    }

    /// Persist the route's transport-to-identity mapping when a route becomes bound to an
    /// authority-managed project. Unbound administrative calls have no route vocabulary to
    /// record and remain valid.
    fn bind_authority_route(
        &self,
        store: &McStore,
        channel: u16,
        context_store_uuid: &str,
        project: &str,
    ) -> Result<(), McStoreError> {
        let Ok(binding) = self.facade_binding(channel) else {
            return Ok(());
        };
        store.bind_authority_route(
            context_store_uuid,
            project,
            binding.project_root.to_string_lossy().as_ref(),
        )
    }

    /// Check whether the shadow lane is enabled. The cached configuration value is checked
    /// on every dispatch, so toggling it and restarting this module can stop mirror traffic
    /// without restarting the full harness. Authority state sync is separate because it
    /// updates the module's internal transform state.
    fn shadow_lane_enabled(&self) -> bool {
        // effective_config honors the test seam, so tests never read the real
        // user config file (a developer's live kill-switch flip must not turn
        // the suite's shadow coverage off).
        self.effective_config(Path::new("/")).shadow_enabled
    }

    fn state_sync_targets_shadow(&self, channel: u16, request: &Value) -> bool {
        request
            .get("session_id")
            .and_then(Value::as_str)
            .map(is_shadow_session)
            .unwrap_or_else(|| {
                self.bindings
                    .lock()
                    .expect("bindings mutex")
                    .get(&channel)
                    .is_some_and(|binding| is_shadow_session(&binding.session))
            })
    }

    fn effective_config(&self, project_root: &Path) -> McModuleConfig {
        #[cfg(test)]
        if let Some(config) = &self.fixed_config {
            return config.clone();
        }
        self.config
            .lock()
            .expect("config mutex")
            .effective_for_project(project_root)
    }

    fn observed_last_response_at_ms(&self, store: &McStore, session_id: &str) -> Option<i64> {
        let mut observations = self
            .scheduler_observations
            .lock()
            .expect("scheduler observations mutex");
        if let Some(observation) = observations.get(session_id) {
            return observation
                .observed_in_process
                .then_some(observation.last_response_at_ms);
        }
        let anchor = store
            .load(session_id)
            .ok()
            .map(|state| state.meta.last_committed_pass_at_ms)
            .unwrap_or(0);
        observations.insert(
            session_id.to_string(),
            SchedulerObservation {
                last_response_at_ms: anchor,
                observed_in_process: false,
            },
        );
        None
    }

    fn record_response_observation(&self, session_id: &str, now: i64) {
        self.scheduler_observations
            .lock()
            .expect("scheduler observations mutex")
            .insert(
                session_id.to_string(),
                SchedulerObservation {
                    last_response_at_ms: now,
                    observed_in_process: true,
                },
            );
    }

    fn guidance_now_ms(&self) -> i64 {
        #[cfg(test)]
        if let Some(now) = *self.guidance_now_ms.lock().expect("guidance clock mutex") {
            return now;
        }
        now_ms()
    }

    fn guidance_date_line(&self) -> String {
        self.guidance_date_line_for_ms(self.guidance_now_ms())
    }

    fn guidance_date_line_for_ms(&self, ms: i64) -> String {
        let date = Local
            .timestamp_millis_opt(ms)
            .single()
            .unwrap_or_else(Local::now)
            .format("%a %b %d %Y");
        format!("Today's date: {date}")
    }

    fn guidance_date_for_transform(&self, session_id: &str, pass_now: i64) -> String {
        self.guidance_dates
            .lock()
            .expect("guidance date mutex")
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| self.guidance_date_line_for_ms(pass_now))
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn set_guidance_now_ms_for_test(&self, now_ms: i64) {
        *self.guidance_now_ms.lock().expect("guidance clock mutex") = Some(now_ms);
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn inject_reductions_for_test(&self, session_id: &str, reductions: Vec<ReductionDecision>) {
        self.reduction_injection
            .lock()
            .expect("reduction injection mutex")
            .insert(session_id.to_string(), reductions);
    }

    fn live_historian_completion_wait(
        &self,
        session_id: &str,
    ) -> Option<LiveHistorianCompletionWait> {
        let live = self
            .live_historian_sessions
            .lock()
            .expect("live historian mutex");
        live.get(session_id).map(|entry| {
            Box::pin(entry.completion.clone().notified_owned()) as LiveHistorianCompletionWait
        })
    }

    fn try_claim_live_historian_session(&self, session_id: &str) -> LiveHistorianSessionClaim {
        let mut live = self
            .live_historian_sessions
            .lock()
            .expect("live historian mutex");
        if let Some(entry) = live.get(session_id) {
            return LiveHistorianSessionClaim::Busy(Box::pin(
                entry.completion.clone().notified_owned(),
            ));
        }
        let token = Arc::new(());
        let completion = Arc::new(Notify::new());
        live.insert(
            session_id.to_string(),
            LiveHistorianSession {
                token: Arc::clone(&token),
                completion: Arc::clone(&completion),
            },
        );
        LiveHistorianSessionClaim::Acquired(SessionSetGuard {
            sessions: Arc::clone(&self.live_historian_sessions),
            session_id: session_id.to_string(),
            token,
            completion,
        })
    }

    fn try_claim_recomp_session(&self, session_id: &str) -> Result<StringSetGuard, ()> {
        let mut sessions = self.recomp_sessions.lock().expect("recomp sessions mutex");
        if !sessions.insert(session_id.to_string()) {
            return Err(());
        }
        Ok(StringSetGuard {
            sessions: Arc::clone(&self.recomp_sessions),
            session_id: session_id.to_string(),
        })
    }

    fn try_claim_wrapup_session(&self, session_id: &str) -> Result<WrapupSessionGuard, usize> {
        let mut sessions = self.wrapup_sessions.lock().expect("wrapup sessions mutex");
        if let Some(session) = sessions.get(session_id) {
            return Err(session.rounds);
        }
        let token = Arc::new(());
        sessions.insert(
            session_id.to_string(),
            LiveWrapupSession {
                token: Arc::clone(&token),
                rounds: 0,
            },
        );
        Ok(WrapupSessionGuard {
            sessions: Arc::clone(&self.wrapup_sessions),
            session_id: session_id.to_string(),
            token,
        })
    }

    fn maybe_spawn_reattach(
        &self,
        store: Arc<McStore>,
        parsed: &TransformRequest,
        snapshot_generation: u64,
        project_path: String,
        projection: &crate::ck_wire::FlatProjection,
        now: i64,
    ) -> Option<&'static str> {
        let Ok(loaded) = store.load(&parsed.session_id) else {
            return Some("recovery_load_failed");
        };
        let phase = loaded.meta.historian.state.clone();
        if phase == HistorianPhase::Idle {
            return Some("recovered");
        }
        if self
            .live_historian_sessions
            .lock()
            .expect("live historian mutex")
            .contains_key(&parsed.session_id)
        {
            return None;
        }
        let mut latch = self.reattaching_sessions.lock().expect("reattach mutex");
        if !latch.insert(parsed.session_id.clone()) {
            return Some(match phase {
                HistorianPhase::AwaitingProducer => "reattaching",
                HistorianPhase::Firing
                | HistorianPhase::Validating
                | HistorianPhase::Publishing => "recovering",
                HistorianPhase::Idle => "recovered",
            });
        }
        drop(latch);

        let session_id = parsed.session_id.clone();
        let latch = Arc::clone(&self.reattaching_sessions);
        let guard = StringSetGuard {
            sessions: Arc::clone(&latch),
            session_id: session_id.clone(),
        };

        match phase {
            HistorianPhase::AwaitingProducer => {
                let publication_fence = Arc::new(ReattachSnapshotPublicationFence {
                    snapshots: Arc::clone(&self.transform_snapshots),
                    session_id: session_id.clone(),
                    generation: snapshot_generation,
                    #[cfg(test)]
                    after_store_publish: Arc::clone(&self.publication_fence_write_hook),
                });
                let factory = Arc::clone(&self.producer_factory);
                let project_root = PathBuf::from(&project_path);
                let live: Vec<_> = projection
                    .blocks
                    .iter()
                    .filter(|b| !b.synthetic)
                    .cloned()
                    .collect();
                let Some(range) = loaded.meta.historian.chunk_range.clone() else {
                    drop(guard);
                    return Some("recovering");
                };
                let chunk = historian_chunk::build_historian_chunk(
                    parsed.messages.as_slice(),
                    &live,
                    range.from_ordinal,
                    DEFAULT_HISTORIAN_CHUNK_TOKENS,
                    range.to_ordinal.saturating_add(1),
                );
                let prior_compartments = match store.load_compartments(&session_id) {
                    Ok(cs) => cs
                        .iter()
                        .map(historian_chunk::stored_range)
                        .collect::<Vec<_>>(),
                    Err(_) => Vec::new(),
                };
                let fingerprint_items: Vec<_> =
                    chunk.snapshot.iter().map(|item| item.as_item()).collect();
                let observed = historian::compute_chunk_fingerprint(&fingerprint_items);
                tokio::spawn(async move {
                    let _guard = guard;
                    let result = async {
                        let action = historian::handle_restart_load(
                            &store,
                            &session_id,
                            now + HISTORIAN_FAILURE_BACKOFF_MS,
                        )?;
                        match action {
                            historian::RestartAction::Done => {
                                return Ok(historian::HistorianReattachOutcome::Done)
                            }
                            historian::RestartAction::AbandonedAndRefireEligible { firing_seq } => {
                                return Ok(historian::HistorianReattachOutcome::RefireEligible {
                                    firing_seq,
                                })
                            }
                            historian::RestartAction::ReattachProducer { .. } => {}
                        }
                        let mut producer = factory.connect(&project_root).await?;
                        reattach_historian_producer(
                            &mut *producer,
                            historian::HistorianReattachRequest {
                                store: &store,
                                session_id: &session_id,
                                project_path: &project_path,
                                observed_chunk_fingerprint: &observed,
                                validation_chunk: &chunk.chunk,
                                chunk_transcript: &chunk.text,
                                prior_compartments: &prior_compartments,
                                validate_options: historian_validate::ValidateOptions {
                                    sequence_offset: prior_compartments.len() as u64 + 1,
                                    in_emergency: false,
                                },
                                publication_floor_ordinal: range.to_ordinal,
                                now_ms: now,
                                failure_backoff_at_ms: now + HISTORIAN_FAILURE_BACKOFF_MS,
                                completion_now_ms: now_ms,
                                publication_fence: Some(publication_fence.as_ref()),
                            },
                        )
                        .await
                    }
                    .await;
                    if let Err(e) = result {
                        eprintln!("mc-module: historian reattach failed for {session_id}: {e}");
                    }
                });
                Some("reattaching")
            }
            HistorianPhase::Firing | HistorianPhase::Validating | HistorianPhase::Publishing => {
                tokio::spawn(async move {
                    let _guard = guard;
                    if let Err(e) = historian::handle_restart_load(
                        &store,
                        &session_id,
                        now + HISTORIAN_FAILURE_BACKOFF_MS,
                    ) {
                        eprintln!(
                            "mc-module: historian restart recovery failed for {session_id}: {e}"
                        );
                    }
                });
                Some("recovering")
            }
            HistorianPhase::Idle => Some("recovered"),
        }
    }

    fn prepare_historian_fire(
        &self,
        store: Arc<McStore>,
        parsed: &TransformRequest,
        binding: &SessionBinding,
        project_path: &str,
        projection: &crate::ck_wire::FlatProjection,
        prepare: HistorianPrepareContext,
    ) -> PreparedHistorianAction {
        let HistorianPrepareContext {
            now,
            snapshot_generation,
        } = prepare;
        let loaded = match store.load(&parsed.session_id) {
            Ok(loaded) => loaded,
            Err(e) => {
                return PreparedHistorianAction::Complete(HistorianDiagnostics {
                    fired: false,
                    reason: None,
                    no_fire: Some(format!("state_load_failed:{e}")),
                    state: "unknown".to_string(),
                    progress: None,
                    last_failure: None,
                })
            }
        };
        let state = loaded.meta.historian.state.as_str().to_string();
        let last_failure = loaded.meta.historian.last_failure.clone();
        if loaded.meta.pending_rewrite.is_some() {
            return PreparedHistorianAction::Complete(HistorianDiagnostics {
                fired: false,
                reason: None,
                no_fire: Some("pending_rewrite".to_string()),
                state,
                progress: None,
                last_failure,
            });
        }
        if let Some(completion) = self.live_historian_completion_wait(&parsed.session_id) {
            return PreparedHistorianAction::Busy {
                diagnostics: HistorianDiagnostics {
                    fired: false,
                    reason: None,
                    no_fire: Some("busy".to_string()),
                    state,
                    progress: None,
                    last_failure,
                },
                completion,
            };
        }
        if loaded.meta.historian.state != HistorianPhase::Idle {
            let no_fire = self
                .maybe_spawn_reattach(
                    Arc::clone(&store),
                    parsed,
                    snapshot_generation,
                    project_path.to_string(),
                    projection,
                    now,
                )
                .unwrap_or("busy");
            return PreparedHistorianAction::Complete(HistorianDiagnostics {
                fired: false,
                reason: None,
                no_fire: Some(no_fire.to_string()),
                state,
                progress: None,
                last_failure,
            });
        }
        let cfg = self.effective_config(&binding.project_root);
        let boundary_messages = boundary_messages(parsed, projection);
        let last_compartment_end_ordinal = store
            .load_compartments(&parsed.session_id)
            .ok()
            .and_then(|cs| cs.iter().map(|c| c.end_message as u64).max());
        let (context_limit, input_tokens, usage_percentage) = usage_numbers(parsed.usage.as_ref());
        let serializer_profile = SerializerProfile::parse(&parsed.serializer_profile)
            .expect("serializer_profile validated upstream");
        let fold_is_only_reclaim = !tail_reclaim(serializer_profile);
        let trigger = boundary::check_compartment_trigger(
            &boundary_messages,
            &TriggerContext {
                boundary: BoundaryContext {
                    context_limit,
                    execute_threshold_percentage: cfg.execute_threshold_percentage,
                    usage_percentage,
                    usage_input_tokens: input_tokens,
                    last_compartment_end_ordinal,
                    prior_boundary_ordinal: last_compartment_end_ordinal.unwrap_or(0),
                    migration_floor_active: last_compartment_end_ordinal.unwrap_or(0) > 0,
                    emergency_tail_scale: None,
                    trigger_budget: None,
                    fold_is_only_reclaim,
                },
                projected_post_drop_percentage: None,
                compartment_in_progress: loaded.meta.historian.state != HistorianPhase::Idle,
                commit_cluster_trigger_enabled: DEFAULT_COMMIT_CLUSTER_TRIGGER_ENABLED,
                min_commit_clusters: DEFAULT_MIN_COMMIT_CLUSTERS,
            },
        );
        let progress = trigger
            .progress
            .as_ref()
            .map(|p| transform::HistorianTriggerProgress {
                eligible_chunk_tokens: p.eligible_chunk_tokens,
                tail_size_bar: p.tail_size_bar,
                protected_tail_n_tokens: p.n_tokens,
                protected_start_ordinal: p.protected_start_ordinal,
            });
        if !trigger.fire {
            let reason = if loaded.meta.historian.state == HistorianPhase::Idle {
                "trigger_false"
            } else {
                "busy"
            };
            if reason == "trigger_false" {
                // Carry the measurement, not just the branch: a bare trigger_false is
                // not actionable from a state dump (is the bar honestly uncrossed, or
                // is eligible measuring zero against real content?). Sizes quantize to
                // the nearest 1k so routine content growth keeps the change-gate
                // effective instead of rewriting the row on every pass.
                let detail = match trigger.progress.as_ref() {
                    Some(p) => format!(
                        "trigger_false{{eligible~{}k,bar~{}k,protected_n~{}k,ctx_limit={}}}",
                        (p.eligible_chunk_tokens / 1000.0).round(),
                        (p.tail_size_bar / 1000.0).round(),
                        (p.n_tokens / 1000.0).round(),
                        context_limit,
                    ),
                    None => "trigger_false".to_string(),
                };
                self.record_no_fire(&store, &parsed.session_id, &loaded, &detail);
            }
            return PreparedHistorianAction::Complete(HistorianDiagnostics {
                fired: false,
                reason: None,
                no_fire: Some(reason.to_string()),
                state,
                progress,
                last_failure,
            });
        }
        let trigger_reason = trigger.reason.map(|r| r.as_str().to_string());
        if cfg.model_chain.is_empty() {
            self.record_no_fire(&store, &parsed.session_id, &loaded, "no_models");
            return PreparedHistorianAction::Complete(HistorianDiagnostics {
                fired: false,
                reason: trigger_reason,
                no_fire: Some("no_models".to_string()),
                state,
                progress: progress.clone(),
                last_failure,
            });
        }
        let Some(boundary) = trigger.boundary.clone() else {
            self.record_no_fire(&store, &parsed.session_id, &loaded, "missing_boundary");
            return PreparedHistorianAction::Complete(HistorianDiagnostics {
                fired: false,
                reason: None,
                no_fire: Some("missing_boundary".to_string()),
                state,
                progress: progress.clone(),
                last_failure,
            });
        };
        if loaded
            .meta
            .historian
            .failure_backoff_at_ms
            .is_some_and(|backoff_at_ms| now < backoff_at_ms)
        {
            self.record_no_fire(&store, &parsed.session_id, &loaded, "backoff");
            return PreparedHistorianAction::Complete(HistorianDiagnostics {
                fired: false,
                reason: trigger_reason,
                no_fire: Some("backoff".to_string()),
                state,
                progress: progress.clone(),
                last_failure,
            });
        }
        let live: Vec<_> = projection
            .blocks
            .iter()
            .filter(|block| !block.synthetic)
            .cloned()
            .collect();
        let project_slug = project_slug(&binding.project_root);
        if fold_is_only_reclaim {
            // CC sessions are born on this profile; tail reducers never run, so no frozen
            // `red:*` units should exist when the fold is the sole reclaim path.
            debug_assert!(
                !loaded
                    .core
                    .frozen_units
                    .iter()
                    .any(|u| u.key.starts_with("red:")),
                "fold-only profile must not carry frozen tail reductions"
            );
        }
        let assemble = assemble_historian_firing(
            &store,
            &parsed.messages,
            &live,
            HistorianAssemblerConfig {
                session_id: parsed.session_id.clone(),
                project_path: project_path.to_string(),
                project_slug: project_slug.clone(),
                model_chain: cfg.model_chain.clone(),
                token_budget: DEFAULT_HISTORIAN_CHUNK_TOKENS,
                boundary,
                memory_enabled: cfg.memory_enabled,
                extraction_free: false,
                in_emergency: usage_percentage >= 95.0,
                fold_is_only_reclaim,
                failure_backoff_at_ms: now + HISTORIAN_FAILURE_BACKOFF_MS,
                min_chunk_tokens: DEFAULT_HISTORIAN_MIN_CHUNK_TOKENS,
            },
            now,
        );
        let firing = match assemble {
            Ok(AssembleHistorianFiringOutcome::Fire(firing)) => *firing,
            Ok(AssembleHistorianFiringOutcome::NoFire(reason)) => {
                self.record_no_fire(
                    &store,
                    &parsed.session_id,
                    &loaded,
                    &format!("assemble:{reason:?}"),
                );
                return PreparedHistorianAction::Complete(HistorianDiagnostics {
                    fired: false,
                    reason: trigger_reason,
                    no_fire: Some(format!("assemble:{reason:?}")),
                    state,
                    progress: progress.clone(),
                    last_failure,
                });
            }
            Err(e) => {
                self.record_no_fire(
                    &store,
                    &parsed.session_id,
                    &loaded,
                    &format!("assemble_failed:{e}"),
                );
                return PreparedHistorianAction::Complete(HistorianDiagnostics {
                    fired: false,
                    reason: trigger_reason,
                    no_fire: Some(format!("assemble_failed:{e}")),
                    state,
                    progress: progress.clone(),
                    last_failure,
                });
            }
        };
        let diagnostics = HistorianDiagnostics {
            fired: true,
            reason: trigger_reason,
            no_fire: None,
            state: state.clone(),
            progress: progress.clone(),
            last_failure: last_failure.clone(),
        };
        let live_guard = match self.try_claim_live_historian_session(&parsed.session_id) {
            LiveHistorianSessionClaim::Acquired(live_guard) => live_guard,
            LiveHistorianSessionClaim::Busy(completion) => {
                return PreparedHistorianAction::Busy {
                    diagnostics: HistorianDiagnostics {
                        fired: false,
                        reason: diagnostics.reason,
                        no_fire: Some("busy".to_string()),
                        state,
                        progress,
                        last_failure,
                    },
                    completion,
                };
            }
        };
        PreparedHistorianAction::FireReady(Box::new(PreparedHistorianFiring {
            diagnostics,
            task: HistorianFiringTask {
                store,
                session_id: parsed.session_id.clone(),
                project_path: project_path.to_string(),
                project_root: binding.project_root.clone(),
                project_slug,
                firing,
                live_guard,
                connect_failure_commit_hook: Arc::clone(&self.connect_failure_commit_hook),
                // Organic pressure firings assemble and publish in one continuous drive
                // while the live-session guard is held. They do not depend on a cached raw
                // snapshot, so a transform-snapshot generation fence would reject valid work.
                publication_fence: None,
            },
        }))
    }

    fn prepare_wrapup_fire(
        &self,
        store: Arc<McStore>,
        parsed: &TransformRequest,
        binding: &SessionBinding,
        projection: &crate::ck_wire::FlatProjection,
        boundary: &boundary::BoundaryResolution,
        context: WrapupPrepareContext,
    ) -> PreparedWrapupAction {
        let WrapupPrepareContext {
            now,
            project_path,
            allow_unknown_module_retry,
        } = context;
        let loaded = match store.load(&parsed.session_id) {
            Ok(loaded) => loaded,
            Err(error) => {
                return PreparedWrapupAction::Failed(format!("state load failed: {error}"))
            }
        };
        if loaded.meta.pending_rewrite.is_some() {
            return PreparedWrapupAction::Failed("a boundary rewrite is pending".to_string());
        }
        if let Some(completion) = self.live_historian_completion_wait(&parsed.session_id) {
            return PreparedWrapupAction::Busy(completion);
        }
        if loaded.meta.historian.state != HistorianPhase::Idle {
            return PreparedWrapupAction::Failed(format!(
                "historian recovery is required from {}",
                loaded.meta.historian.state.as_str()
            ));
        }
        if !allow_unknown_module_retry {
            if let Some(until) = loaded.meta.historian.failure_backoff_at_ms {
                if until > now {
                    return PreparedWrapupAction::Failed(format!(
                        "historian failure backoff active for {} ms",
                        until.saturating_sub(now)
                    ));
                }
            }
        }

        let cfg = self.effective_config(&binding.project_root);
        if cfg.model_chain.is_empty() {
            return PreparedWrapupAction::Failed("no historian models are configured".to_string());
        }
        let live = projection
            .blocks
            .iter()
            .filter(|block| !block.synthetic)
            .cloned()
            .collect::<Vec<_>>();
        let project_slug = project_slug(&binding.project_root);
        let assemble = assemble_historian_firing(
            &store,
            &parsed.messages,
            &live,
            HistorianAssemblerConfig {
                session_id: parsed.session_id.clone(),
                project_path: project_path.clone(),
                project_slug: project_slug.clone(),
                model_chain: cfg.model_chain,
                token_budget: DEFAULT_HISTORIAN_CHUNK_TOKENS,
                boundary: boundary.clone(),
                memory_enabled: cfg.memory_enabled,
                extraction_free: false,
                in_emergency: false,
                // Explicit wrapup is the only reclaim mechanism on this surface, so a
                // small final chunk must not be rejected by the substance floor.
                fold_is_only_reclaim: true,
                failure_backoff_at_ms: now + HISTORIAN_FAILURE_BACKOFF_MS,
                min_chunk_tokens: DEFAULT_HISTORIAN_MIN_CHUNK_TOKENS,
            },
            now,
        );
        let firing = match assemble {
            Ok(AssembleHistorianFiringOutcome::Fire(firing)) => *firing,
            Ok(AssembleHistorianFiringOutcome::NoFire(reason)) => {
                return PreparedWrapupAction::Nothing(format!("{reason:?}"))
            }
            Err(error) => return PreparedWrapupAction::Failed(format!("assembly failed: {error}")),
        };
        let live_guard = match self.try_claim_live_historian_session(&parsed.session_id) {
            LiveHistorianSessionClaim::Acquired(guard) => guard,
            LiveHistorianSessionClaim::Busy(completion) => {
                return PreparedWrapupAction::Busy(completion)
            }
        };
        PreparedWrapupAction::FireReady(Box::new(HistorianFiringTask {
            store,
            session_id: parsed.session_id.clone(),
            project_path,
            project_root: binding.project_root.clone(),
            project_slug,
            firing,
            live_guard,
            connect_failure_commit_hook: Arc::clone(&self.connect_failure_commit_hook),
            publication_fence: None,
        }))
    }

    fn refresh_historian_diagnostics(
        &self,
        store: &McStore,
        session_id: &str,
        mut diagnostics: HistorianDiagnostics,
    ) -> HistorianDiagnostics {
        if let Ok(loaded) = store.load(session_id) {
            diagnostics.state = loaded.meta.historian.state.as_str().to_string();
            diagnostics.last_failure = loaded.meta.historian.last_failure.clone();
        }
        diagnostics
    }

    /// Persist the skip-branch discriminant so a supervised rig can read WHY the
    /// historian declined to fire from the state dump (the transform response's
    /// diagnostics block never reaches disk). Change-gated: steady-state passes that
    /// skip for the same reason write nothing, so this stays off the hot path. A CAS
    /// conflict just drops the diagnostic; it must never fail a pass.
    fn record_no_fire(
        &self,
        store: &McStore,
        session_id: &str,
        loaded: &mc_store::LoadedState,
        reason: &str,
    ) {
        if loaded.meta.historian.last_no_fire.as_deref() == Some(reason) {
            return;
        }
        let mut meta = loaded.meta.clone();
        meta.historian.last_no_fire = Some(reason.to_string());
        let _ = store.commit(session_id, loaded.row_version, &loaded.core, &meta);
    }

    async fn execute_historian_firing_task(
        factory: Arc<dyn HistorianProducerFactory>,
        task: HistorianFiringTask,
    ) -> Result<historian::HistorianDriveOutcome, historian::HistorianDriveError> {
        let HistorianFiringTask {
            store,
            session_id,
            project_path,
            project_root,
            project_slug,
            firing,
            live_guard,
            connect_failure_commit_hook,
            publication_fence,
        } = task;
        let _guard = live_guard;
        let failure_started_at_ms = firing.now_ms;
        let configured_failure_backoff_at_ms = firing.failure_backoff_at_ms;
        match factory.connect(&project_root).await {
            Ok(mut producer) => {
                let mut request =
                    firing.as_fire_request(&store, &session_id, &project_path, &project_slug);
                request.publication_fence = publication_fence.as_deref();
                run_historian_firing(&mut *producer, request).await
            }
            Err(err) => {
                let failure_backoff_at_ms = historian::completion_failure_backoff_at_ms(
                    failure_started_at_ms,
                    configured_failure_backoff_at_ms,
                    now_ms(),
                );
                let backoff_error = record_historian_connect_failure(
                    &store,
                    &session_id,
                    failure_backoff_at_ms,
                    &format!("producer connect: {err}"),
                    &connect_failure_commit_hook,
                )
                .err()
                .map(Box::new);
                Err(historian::HistorianDriveError::ProducerConnect {
                    source: Box::new(err),
                    backoff_error,
                })
            }
        }
    }

    /// Drive a firing for an emergency pass, bounded by the completion-wait budget.
    ///
    /// The firing runs as a SPAWNED task and this method awaits its JoinHandle with a
    /// timeout, for two reasons:
    /// - The drive's own wall clock is bounded only per model attempt (producer await +
    ///   recovery re-drain), so a fallback chain could hold the request open for
    ///   attempt-budget × chain-length. Transform consumers need a hard per-request
    ///   ceiling to set their call deadlines against.
    /// - On timeout the spawned firing KEEPS RUNNING (a JoinHandle timeout does not
    ///   cancel the task): the request degrades to its already-computed emergency
    ///   output and a later pass picks up the published fold. Cancelling mid-drive
    ///   would instead strand durable state for crash recovery to repair.
    async fn run_historian_firing_inline(
        &self,
        task: HistorianFiringTask,
    ) -> Result<historian::HistorianDriveOutcome, historian::HistorianDriveError> {
        let factory = Arc::clone(&self.producer_factory);
        let handle = tokio::spawn(Self::execute_historian_firing_task(factory, task));
        match tokio::time::timeout(historian::completion_wait_budget(), handle).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(join_err)) => Err(historian::HistorianDriveError::Producer(
                HistorianProducerError::RunFailed {
                    run_id: String::new(),
                    detail: format!("inline firing task panicked: {join_err}"),
                    classification: None,
                    class_field_present: false,
                },
            )),
            Err(_elapsed) => Err(historian::HistorianDriveError::Producer(
                HistorianProducerError::TimedOut,
            )),
        }
    }

    async fn await_live_historian_completion(
        &self,
        completion: LiveHistorianCompletionWait,
    ) -> bool {
        tokio::time::timeout(historian::completion_wait_budget(), completion)
            .await
            .is_ok()
    }

    fn wrapup_operation_budget(&self) -> Duration {
        #[cfg(test)]
        if let Some(budget) = *self
            .wrapup_operation_budget
            .lock()
            .expect("wrapup operation budget mutex")
        {
            return budget;
        }
        historian::MAX_WRAPUP_REQUEST_BUDGET
            .checked_sub(WRAPUP_REQUEST_MARGIN)
            .unwrap_or(Duration::ZERO)
    }

    fn unknown_module_retry_delay(&self) -> Duration {
        #[cfg(test)]
        if let Some(delay) = *self
            .unknown_module_retry_delay
            .lock()
            .expect("unknown module retry delay mutex")
        {
            return delay;
        }
        Duration::from_secs(30)
    }

    fn remaining_wrapup_budget(deadline: Instant) -> Option<Duration> {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }

    async fn run_wrapup_firing(
        &self,
        task: HistorianFiringTask,
        deadline: Instant,
    ) -> Result<historian::HistorianDriveOutcome, WrapupFiringError> {
        let Some(remaining) = Self::remaining_wrapup_budget(deadline) else {
            return Err(WrapupFiringError::Retryable(
                RetryableWrapupReason::BudgetExhausted,
                "wrapup request budget expired before historian round".to_string(),
            ));
        };
        let wait = historian::wrapup_round_wait_budget().min(remaining);
        let factory = Arc::clone(&self.producer_factory);
        let handle = tokio::spawn(Self::execute_historian_firing_task(factory, task));
        match tokio::time::timeout(wait, handle).await {
            Ok(Ok(Ok(outcome))) => Ok(outcome),
            Ok(Ok(Err(error))) => {
                if matches!(&error, historian::HistorianDriveError::NoModels) {
                    return Err(WrapupFiringError::Terminal {
                        reason: "no_models",
                        detail: error.to_string(),
                    });
                }
                if matches!(
                    &error,
                    historian::HistorianDriveError::Producer(error)
                        if error.is_unknown_module()
                ) || matches!(
                    &error,
                    historian::HistorianDriveError::ProducerConnect { source, .. }
                        if source.is_unknown_module()
                ) {
                    return Err(WrapupFiringError::UnknownModule(error.to_string()));
                }
                let reason = match &error {
                    historian::HistorianDriveError::State(
                        historian::HistorianStateError::Publish(
                            mc_store::HistorianPublishError::CasConflict { .. }
                            | mc_store::HistorianPublishError::FenceRejected { .. },
                        ),
                    )
                    | historian::HistorianDriveError::State(
                        historian::HistorianStateError::Store(McStoreError::CasConflict { .. }),
                    ) => RetryableWrapupReason::SnapshotStale,
                    historian::HistorianDriveError::ProducerConnect {
                        backoff_error: None,
                        ..
                    }
                    | historian::HistorianDriveError::Producer(_)
                    | historian::HistorianDriveError::Validation(_) => {
                        RetryableWrapupReason::BackoffActive
                    }
                    historian::HistorianDriveError::ProducerConnect {
                        backoff_error: Some(_),
                        ..
                    } => RetryableWrapupReason::SnapshotUnavailable,
                    _ => RetryableWrapupReason::SnapshotUnavailable,
                };
                Err(WrapupFiringError::Retryable(reason, error.to_string()))
            }
            Ok(Err(error)) => Err(WrapupFiringError::Retryable(
                RetryableWrapupReason::SnapshotUnavailable,
                format!("historian task failed: {error}"),
            )),
            Err(_) if Instant::now() >= deadline => Err(WrapupFiringError::Retryable(
                RetryableWrapupReason::BudgetExhausted,
                "wrapup request budget expired during historian round".to_string(),
            )),
            Err(_) => Err(WrapupFiringError::Retryable(
                RetryableWrapupReason::SnapshotUnavailable,
                "historian round timed out after 600 seconds".to_string(),
            )),
        }
    }

    async fn await_wrapup_historian_completion(
        &self,
        completion: LiveHistorianCompletionWait,
        deadline: Instant,
    ) -> Result<(), String> {
        let Some(remaining) = Self::remaining_wrapup_budget(deadline) else {
            return Err("wrapup request budget expired before joining historian".to_string());
        };
        let wait = historian::completion_wait_budget().min(remaining);
        match tokio::time::timeout(wait, completion).await {
            Ok(()) => Ok(()),
            Err(_) if Instant::now() >= deadline => {
                Err("wrapup request budget expired while joining historian".to_string())
            }
            Err(_) => Err("timed out while joining the active historian run".to_string()),
        }
    }

    fn spawn_historian_firing(&self, task: HistorianFiringTask) {
        let factory = Arc::clone(&self.producer_factory);
        tokio::spawn(async move {
            let session_id = task.session_id.clone();
            let result = Self::execute_historian_firing_task(factory, task).await;
            match result {
                Ok(outcome) => {
                    eprintln!("mc-module: historian firing finished for {session_id}: {outcome:?}")
                }
                Err(e) => eprintln!("mc-module: historian firing failed for {session_id}: {e}"),
            }
        });
    }

    fn handle_state_import_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let raw_session_id = request
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let batch_bytes = match serde_json::to_vec(&request) {
            Ok(bytes) if bytes.len() <= MAX_FACADE_FRAME_BYTES => bytes.len(),
            Ok(_) => {
                if let Some(session_id) = raw_session_id.as_deref() {
                    self.state_imports
                        .lock()
                        .expect("state import mutex")
                        .discard(session_id);
                }
                return invalid_params_error("request body exceeds the 1 MiB limit");
            }
            Err(error) => return invalid_params_error(error.to_string()),
        };
        let parsed: StateImportWire = match serde_json::from_value(request.clone()) {
            Ok(parsed) => parsed,
            Err(error) => {
                if let Some(session_id) = raw_session_id.as_deref() {
                    self.state_imports
                        .lock()
                        .expect("state import mutex")
                        .discard(session_id);
                }
                return invalid_params_error(error.to_string());
            }
        };
        let discard = |handler: &McHandler| {
            handler
                .state_imports
                .lock()
                .expect("state import mutex")
                .discard(&parsed.session_id);
        };
        if parsed.v != 1 {
            discard(self);
            return HandlerOutcome::Error {
                code: "state_import_version".to_string(),
                message: "state_import requires v=1".to_string(),
            };
        }
        if parsed.session_id.trim().is_empty() {
            discard(self);
            return invalid_params_error("state_import requires a nonempty session_id");
        }
        if parsed.import_id.is_empty() || parsed.import_id.len() > STATE_IMPORT_MAX_ID_BYTES {
            discard(self);
            return invalid_params_error(format!(
                "import_id must contain 1..={STATE_IMPORT_MAX_ID_BYTES} bytes"
            ));
        }
        if parsed.batch_count == 0 || parsed.batch_seq >= parsed.batch_count {
            discard(self);
            return HandlerOutcome::Error {
                code: "batch_seq_mismatch".to_string(),
                message: "batch_seq must be inside a nonempty batch_count".to_string(),
            };
        }

        let binding = match self.resolve_binding(channel, &parsed.session_id) {
            Ok(binding) => binding,
            Err(BindingError::Unbound) => {
                discard(self);
                return HandlerOutcome::Error {
                    code: "route_unbound".to_string(),
                    message: "state_import on a channel with no session binding".to_string(),
                };
            }
            Err(BindingError::SessionMismatch) => {
                discard(self);
                return HandlerOutcome::Error {
                    code: "session_mismatch".to_string(),
                    message: "request session_id does not match the channel's bound session"
                        .to_string(),
                };
            }
        };
        if is_shadow_session(&binding.session) {
            discard(self);
            return HandlerOutcome::Error {
                code: "non_shadow_op_on_shadow_binding".to_string(),
                message: "state_import is not accepted on shadow routes".to_string(),
            };
        }
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => {
                discard(self);
                return store_unavailable_error();
            }
        };
        match store.preflight_state_import(&parsed.session_id, &parsed.import_id) {
            Ok(StateImportPreflight::Duplicate { imported }) => {
                discard(self);
                return respond(json!({
                    "ok": true,
                    "imported": imported,
                    "duplicate": true,
                }));
            }
            Ok(StateImportPreflight::Ready) => {}
            Err(StateImportError::SessionNotEmpty) => {
                discard(self);
                return HandlerOutcome::Error {
                    code: "session_not_empty".to_string(),
                    message: "state_import only accepts a session with no durable state"
                        .to_string(),
                };
            }
            Err(error) => {
                discard(self);
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: error.to_string(),
                };
            }
        }

        let created_at = now_ms();
        let compartments = parsed
            .compartments
            .into_iter()
            .map(|compartment| compartment.into_stored(created_at))
            .collect::<Vec<_>>();
        if let Err(error) = validate_state_import_compartments(&compartments) {
            discard(self);
            return state_import_validation_error(error);
        }
        let digest = sha256_hex(canonical_value(&request).as_bytes());
        let action = self
            .state_imports
            .lock()
            .expect("state import mutex")
            .stage(
                &parsed.session_id,
                parsed.import_id,
                parsed.batch_seq,
                parsed.batch_count,
                digest,
                batch_bytes,
                compartments,
                Instant::now(),
            );
        match action {
            Ok(StateImportStageOutcome::Staged(staged)) => {
                respond(json!({ "ok": true, "staged": staged }))
            }
            Ok(StateImportStageOutcome::Apply {
                import_id,
                compartments,
            }) => {
                let outcome = store.commit_state_import(
                    &parsed.session_id,
                    &import_id,
                    &compartments,
                    created_at,
                );
                self.state_imports
                    .lock()
                    .expect("state import mutex")
                    .complete(&parsed.session_id, &import_id);
                match outcome {
                    Ok(result) => respond(json!({
                        "ok": true,
                        "imported": result.imported,
                        "duplicate": result.duplicate,
                    })),
                    Err(StateImportError::SessionNotEmpty) => HandlerOutcome::Error {
                        code: "session_not_empty".to_string(),
                        message: "state_import only accepts a session with no durable state"
                            .to_string(),
                    },
                    Err(StateImportError::Validation(error)) => {
                        state_import_validation_error(error)
                    }
                    Err(StateImportError::Store(error)) => HandlerOutcome::Error {
                        code: "store_write_failed".to_string(),
                        message: error.to_string(),
                    },
                }
            }
            Err(StateImportStageError::Validation(error)) => state_import_validation_error(error),
            Err(StateImportStageError::Protocol { code, message }) => HandlerOutcome::Error {
                code: code.to_string(),
                message: message.to_string(),
            },
        }
    }

    fn handle_agent_drops_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let Some(session_id) = request.get("session_id").and_then(Value::as_str) else {
            return HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: "agent_drops.append requires session_id".to_string(),
            };
        };
        let command_id = match command_id_from_agent_drops_request(&request) {
            Ok(command_id) => command_id,
            Err(message) => {
                return HandlerOutcome::Error {
                    code: "bad_request".to_string(),
                    message,
                }
            }
        };
        let Some(raw_drop) = request.get("drop").and_then(Value::as_str) else {
            return HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: "'drop' must be a nonempty string".to_string(),
            };
        };
        if raw_drop.trim().is_empty() {
            return HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: "'drop' must be a nonempty string".to_string(),
            };
        }
        let numbers = match parse_tag_range_string(raw_drop) {
            Ok(numbers) => numbers,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "bad_request".to_string(),
                    message: format!("invalid drop range syntax: {error}"),
                }
            }
        };
        let binding = match self.resolve_binding(channel, session_id) {
            Ok(binding) => binding,
            Err(BindingError::Unbound) => {
                return HandlerOutcome::Error {
                    code: "route_unbound".to_string(),
                    message: "agent_drops.append on a channel with no session binding".to_string(),
                }
            }
            Err(BindingError::SessionMismatch) => {
                return HandlerOutcome::Error {
                    code: "session_mismatch".to_string(),
                    message: "request session_id does not match the channel's bound session"
                        .to_string(),
                }
            }
        };
        if is_shadow_session(&binding.session) {
            return HandlerOutcome::Error {
                code: "non_shadow_op_on_shadow_binding".to_string(),
                message: "agent_drops.append is not accepted on shadow routes".to_string(),
            };
        }
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => return store_unavailable_error(),
        };
        let tags = match store.load_tags_for_session(session_id) {
            Ok(tags) => tags,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_write_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        let by_number = tags
            .iter()
            .map(|row| (row.tag_number, &row.block_id))
            .collect::<HashMap<_, _>>();
        let mut drop_ids = numbers
            .into_iter()
            .filter_map(|number| by_number.get(&(number as i64)).map(|id| (*id).clone()))
            .collect::<Vec<_>>();
        drop_ids.sort();
        drop_ids.dedup();

        match store.append_pending_agent_drops_with_command(
            session_id,
            Some(&command_id),
            &drop_ids,
            now_ms(),
            drop_ids.is_empty(),
        ) {
            Ok(outcome) if outcome.duplicate => {
                respond(json!({ "ok": true, "queued": 0, "duplicate": true }))
            }
            Ok(outcome) => {
                let mut resp = json!({ "ok": true, "queued": outcome.queued });
                if let Some(disposition) = &outcome.disposition {
                    resp["disposition"] = json!(disposition);
                }
                respond(resp)
            }
            Err(error) => HandlerOutcome::Error {
                code: "store_write_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    fn management_binding(
        &self,
        channel: u16,
        request: &Value,
        operation: &str,
    ) -> Result<(String, SessionBinding), HandlerOutcome> {
        if request.get("v").and_then(Value::as_u64) != Some(1) {
            return Err(HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: format!("{operation} requires v=1"),
            });
        }
        let Some(session_id) = request.get("session_id").and_then(Value::as_str) else {
            return Err(HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: format!("{operation} requires session_id"),
            });
        };
        if session_id.trim().is_empty() {
            return Err(HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: format!("{operation} requires a nonempty session_id"),
            });
        }
        let binding = match self.resolve_binding(channel, session_id) {
            Ok(binding) => binding,
            Err(BindingError::Unbound) => {
                return Err(HandlerOutcome::Error {
                    code: "route_unbound".to_string(),
                    message: format!("{operation} on a channel with no session binding"),
                })
            }
            Err(BindingError::SessionMismatch) => {
                return Err(HandlerOutcome::Error {
                    code: "session_mismatch".to_string(),
                    message: "request session_id does not match the channel's bound session"
                        .to_string(),
                })
            }
        };
        if is_shadow_session(&binding.session) {
            return Err(HandlerOutcome::Error {
                code: "non_shadow_op_on_shadow_binding".to_string(),
                message: format!("{operation} is not accepted on shadow routes"),
            });
        }
        Ok((session_id.to_string(), binding))
    }

    fn handle_todo_state_set_value(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let (session_id, _binding) =
            match self.management_binding(channel, request, "todo_state.set") {
                Ok(scope) => scope,
                Err(outcome) => return outcome,
            };
        let Some(state_json) = request.get("state_json").and_then(Value::as_str) else {
            return invalid_params_error("todo_state.set requires state_json");
        };
        if state_json.len() > MAX_FACADE_FRAME_BYTES {
            return invalid_params_error("state_json exceeds the 1 MiB limit");
        }
        let Some(owner_message_id) = request.get("owner_message_id").and_then(Value::as_str) else {
            return invalid_params_error("todo_state.set requires owner_message_id");
        };
        if owner_message_id.is_empty() || owner_message_id.len() > 128 {
            return invalid_params_error("owner_message_id must contain 1..=128 bytes");
        }
        let Some(normalized) = crate::injection::normalize_todo_state_json(state_json) else {
            return invalid_params_error("state_json must be a JSON todo array");
        };
        let state_hash = sha256_hex(normalized.as_bytes());
        let store = match self.store.get() {
            Some(store) => store,
            None => return store_unavailable_error(),
        };
        match store.set_todo_state(&session_id, &normalized, owner_message_id, &state_hash) {
            Ok(TodoStateSetOutcome::Updated { .. }) | Ok(TodoStateSetOutcome::Noop) => {
                respond(json!({ "ok": true }))
            }
            Err(error) => HandlerOutcome::Error {
                code: "store_write_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    fn handle_session_flush_value(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let (session_id, _binding) =
            match self.management_binding(channel, request, "session.flush") {
                Ok(scope) => scope,
                Err(outcome) => return outcome,
            };
        let store = match self.store.get() {
            Some(store) => store,
            None => return store_unavailable_error(),
        };
        match store.arm_soft_refresh(&session_id) {
            Ok(armed) => respond(json!({ "ok": true, "armed": armed })),
            Err(error) => HandlerOutcome::Error {
                code: "store_write_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    fn handle_session_recomp_value(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let (session_id, _binding) =
            match self.management_binding(channel, request, "session.recomp") {
                Ok(scope) => scope,
                Err(outcome) => return outcome,
            };
        let Some(command_id) = request.get("command_id").and_then(Value::as_str) else {
            return invalid_params_error("session.recomp requires command_id");
        };
        if command_id.is_empty() || command_id.len() > 128 {
            return invalid_params_error("command_id must contain 1..=128 bytes");
        }
        let store = match self.store.get() {
            Some(store) => store,
            None => return store_unavailable_error(),
        };
        match store.load_recomp_command(&session_id, command_id) {
            Ok(Some(row)) => {
                return respond(json!({
                    "ok": true,
                    "disposition": row.disposition,
                }))
            }
            Ok(None) => {}
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: error.to_string(),
                }
            }
        }
        let _guard = match self.try_claim_recomp_session(&session_id) {
            Ok(guard) => guard,
            Err(()) => {
                return respond(json!({
                    "ok": true,
                    "disposition": "already_in_progress",
                }))
            }
        };

        let loaded = match store.load(&session_id) {
            Ok(loaded) => loaded,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        let compartments = match store.load_compartments(&session_id) {
            Ok(compartments) => compartments,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        let never_minted = compartments.is_empty() && loaded.core.boundary_id.trim().is_empty();
        if never_minted {
            return match store.record_recomp_command(
                &session_id,
                command_id,
                "nothing_to_do",
                now_ms(),
            ) {
                Ok(row) => respond(json!({
                    "ok": true,
                    "disposition": row.disposition,
                })),
                Err(error) => HandlerOutcome::Error {
                    code: "store_write_failed".to_string(),
                    message: error.to_string(),
                },
            };
        }

        let _reset = match store.reset_session_for_recomp(&session_id, loaded.row_version) {
            Ok(reset) => reset,
            Err(error @ McStoreError::CasConflict { .. }) => {
                // A transform may have committed between the status reads and the reset.
                // The recomp latch remains held; ask the caller to retry rather than
                // claiming a reset that did not use the observed cache version.
                return HandlerOutcome::Error {
                    code: "store_conflict".to_string(),
                    message: error.to_string(),
                };
            }
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_write_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        // A transform generation is an in-memory fence for cached raw snapshots. Marking
        // this session in-flight prevents an already assembled historian from acquiring
        // a ready snapshot after the durable revert epoch has been bumped.
        self.transform_snapshots
            .lock()
            .expect("transform snapshots mutex")
            .begin(&session_id);
        match store.record_recomp_command(&session_id, command_id, "started", now_ms()) {
            Ok(row) => respond(json!({
                "ok": true,
                "disposition": row.disposition,
            })),
            Err(error) => HandlerOutcome::Error {
                code: "store_write_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    fn handle_session_status_value(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let (session_id, _binding) =
            match self.management_binding(channel, request, "session.status") {
                Ok(scope) => scope,
                Err(outcome) => return outcome,
            };
        let store = match self.store.get() {
            Some(store) => store,
            None => return store_unavailable_error(),
        };
        let sample_wrapup_latch = || {
            self.wrapup_sessions
                .lock()
                .expect("wrapup sessions mutex")
                .get(&session_id)
                .map(|session| (Arc::as_ptr(&session.token) as usize, session.rounds))
        };
        let latch_before = sample_wrapup_latch();
        let mut snapshot = match store.load_session_status_snapshot(&session_id) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        #[cfg(test)]
        if let Some(hook) = self
            .status_snapshot_hook
            .lock()
            .expect("status snapshot hook mutex")
            .take()
        {
            hook();
        }
        let mut wrapup_latch = sample_wrapup_latch();
        if latch_before != wrapup_latch {
            // Holding the latch mutex across SQLite I/O would block wrapup progress. A single
            // bounded re-read instead places the durable snapshot after the observed latch edge.
            snapshot = match store.load_session_status_snapshot(&session_id) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "store_load_failed".to_string(),
                        message: error.to_string(),
                    }
                }
            };
            wrapup_latch = sample_wrapup_latch();
        }
        let loaded = snapshot.loaded;
        let compartment_count = snapshot.compartment_count;
        let pending_drop_count = snapshot.pending_drop_count;
        let tag_count = snapshot.tag_count;
        let pass_trace = snapshot.pass_trace;
        let coverage = loaded
            .meta
            .coverage_ordinal
            .map(|ordinal| ordinal.to_string())
            .unwrap_or_else(|| "none".to_string());
        let boundary = if loaded.core.boundary_id.trim().is_empty() {
            "absent"
        } else {
            "present"
        };
        let surface = if loaded.meta.tagging_surface_active || loaded.meta.cc_u1_active {
            "active"
        } else {
            "inactive"
        };
        let historian = historian_status_summary(&loaded.meta.historian);
        // When the Rust module is active, it manages the frozen m0 in its own store
        // instead of the harness SQLite cache. Report the exact session-history slice so
        // status attribution does not estimate size by summing all raw-history p1 rows.
        let compartment_tokens = loaded
            .core
            .frozen_units
            .iter()
            .find(|unit| unit.key == "m0")
            .and_then(|unit| {
                decay_render::extract_m0_block(&unit.frozen_payload, "session-history")
            })
            .map(|block| mc_tokenizer::estimate_tokens(&block))
            .unwrap_or(0);
        let newest_pass_at = pass_trace
            .as_ref()
            .map(|trace| {
                trace
                    .last_received_at_ms
                    .max(trace.last_completed_at_ms)
                    .max(trace.last_reject_at_ms.unwrap_or(0))
            })
            .unwrap_or(0)
            .max(loaded.meta.last_committed_pass_at_ms);
        let short_session = session_id.chars().take(12).collect::<String>();
        let age = format_traffic_age(newest_pass_at, now_ms());
        // Status can outlive the caller's current lineage. Naming the subject and its
        // durable traffic age makes a stale read visible instead of silently ambiguous.
        let summary = sanitize_status_text(
            &format!(
                "session {short_session} (last active {age}): {} {}, coverage ordinal {coverage}, boundary {boundary}, {} pending {}, {} {}, last historian: {historian}, surface {surface}",
                compartment_count,
                plural_word(compartment_count, "compartment"),
                pending_drop_count,
                plural_word(pending_drop_count, "drop"),
                tag_count,
                plural_word(tag_count, "tag"),
            ),
            500,
        );
        // Structured fields beside the prose: reconcilers must never parse summary
        // text, and a retained delivered-command row needs coverage/row_version plus
        // the live wrapup latch to decide completion without a second op.
        let wrapup_active = wrapup_latch.map(|(_, rounds)| rounds);
        let mut response = json!({
            "ok": true,
            "summary": summary,
            "wrapup_active": wrapup_active.is_some(),
            "wrapup_rounds": wrapup_active,
            "coverage_ordinal": loaded.meta.coverage_ordinal,
            "row_version": loaded.row_version,
            "boundary_present": !loaded.core.boundary_id.trim().is_empty(),
            "compartment_count": compartment_count,
            "compartment_tokens": compartment_tokens,
            "pending_drop_count": pending_drop_count,
            "usage": {
                "current_total_input_tokens": loaded.meta.last_usage.as_ref().map_or(0, |usage| usage.current_total_input_tokens),
                "context_limit_tokens": loaded.meta.last_usage.as_ref().map_or(0, |usage| usage.context_limit_tokens),
            },
        });
        if let Some(after_sequence) = request.get("include_compartments_after_seq") {
            let Some(after_sequence) = after_sequence.as_i64().filter(|value| *value >= -1) else {
                return invalid_params_error(
                    "include_compartments_after_seq must be an integer >= -1",
                );
            };
            let page = match store.load_compartments_after(
                &session_id,
                after_sequence,
                SESSION_STATUS_COMPARTMENT_PAGE_LIMIT,
            ) {
                Ok(page) => page,
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "store_load_failed".to_string(),
                        message: error.to_string(),
                    }
                }
            };
            let compartments = page
                .compartments
                .into_iter()
                .map(|compartment| {
                    json!({
                        "sequence": compartment.sequence,
                        "start_message": compartment.start_message,
                        "end_message": compartment.end_message,
                        "start_message_id": compartment.start_message_id,
                        "end_message_id": compartment.end_message_id,
                        "title": compartment.title,
                        "content": compartment.content,
                        "p1": compartment.p1,
                        "p2": compartment.p2,
                        "p3": compartment.p3,
                        "p4": compartment.p4,
                        "importance": compartment.importance,
                        "episode_type": compartment.episode_type,
                        "created_at": compartment.created_at,
                    })
                })
                .collect::<Vec<_>>();
            let body = response
                .as_object_mut()
                .expect("session.status response is an object");
            body.insert("compartments".to_string(), Value::Array(compartments));
            body.insert("max_sequence".to_string(), json!(page.max_sequence));
        }
        respond(response)
    }

    fn wrapup_snapshot_is_current(
        &self,
        store: &McStore,
        session_id: &str,
        generation: u64,
        revert_epoch: u64,
    ) -> Result<bool, McStoreError> {
        let generation_current = self
            .transform_snapshots
            .lock()
            .expect("transform snapshots mutex")
            .ready_generation_matches(session_id, generation);
        if !generation_current {
            return Ok(false);
        }
        Ok(store.load(session_id)?.meta.revert_epoch == revert_epoch)
    }

    fn retryable_wrapup_response(
        reason: RetryableWrapupReason,
        summary: impl Into<String>,
    ) -> HandlerOutcome {
        respond(json!({
            "ok": false,
            "disposition": "retryable",
            "reason": reason.as_str(),
            "summary": summary.into(),
        }))
    }

    fn terminal_wrapup_response(
        &self,
        store: &McStore,
        session_id: &str,
        command_id: Option<&str>,
        expected_generation: u64,
        expected_revert_epoch: u64,
        response: TerminalWrapupResponse,
    ) -> HandlerOutcome {
        let TerminalWrapupResponse {
            disposition,
            rounds,
            summary,
            reason,
            detail,
            include_rounds_without_command,
        } = response;
        debug_assert!(matches!(
            disposition,
            "completed" | "nothing_to_compact" | "failed"
        ));
        let ok = disposition != "failed";
        let (disposition, rounds, summary) = if let Some(command_id) = command_id {
            // The generation check and fenced SQLite insert share this short critical
            // section. The local write is bounded and prevents a transform from starting
            // after validation but before its terminal ledger row becomes durable.
            let snapshots = self
                .transform_snapshots
                .lock()
                .expect("transform snapshots mutex");
            if !snapshots.ready_generation_matches(session_id, expected_generation) {
                return Self::retryable_wrapup_response(
                    RetryableWrapupReason::SnapshotStale,
                    "wrapup snapshot changed before terminal result recording",
                );
            }
            let current = match store.load(session_id) {
                Ok(current) if current.meta.revert_epoch == expected_revert_epoch => current,
                Ok(_) => {
                    return Self::retryable_wrapup_response(
                        RetryableWrapupReason::SnapshotStale,
                        "wrapup state changed before terminal result recording",
                    )
                }
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "store_load_failed".to_string(),
                        message: error.to_string(),
                    }
                }
            };
            let ledger_summary = if disposition == "failed" {
                terminal_wrapup_failure_summary(
                    reason.unwrap_or("unknown"),
                    &summary,
                    detail.as_deref().unwrap_or("terminal wrapup failure"),
                )
            } else {
                summary.clone()
            };
            match store.record_wrapup_command_if_current(WrapupCommandRecord {
                session_id,
                command_id,
                disposition,
                rounds,
                summary: &ledger_summary,
                created_at: now_ms(),
                expected_row_version: current.row_version,
                expected_revert_epoch,
            }) {
                Ok(RecordWrapupCommandOutcome::Recorded(row)) => {
                    if disposition == "failed" {
                        (row.disposition, row.rounds, summary)
                    } else {
                        (row.disposition, row.rounds, row.summary)
                    }
                }
                Ok(RecordWrapupCommandOutcome::Stale { .. }) => {
                    return Self::retryable_wrapup_response(
                        RetryableWrapupReason::SnapshotStale,
                        "wrapup state changed before terminal result recording",
                    )
                }
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "store_commit_failed".to_string(),
                        message: format!("could not record terminal wrapup result: {error}"),
                    }
                }
            }
        } else {
            (disposition.to_string(), rounds, summary)
        };
        let mut payload = json!({
            "ok": ok,
            "disposition": disposition,
            "summary": summary,
        });
        if let Some(reason) = reason {
            payload["reason"] = json!(reason);
        }
        if let Some(detail) = detail {
            payload["detail"] = json!(detail);
        }
        if command_id.is_some() || include_rounds_without_command {
            payload["rounds"] = json!(rounds);
        }
        respond(payload)
    }

    fn replayed_wrapup_response(row: mc_store::WrapupCommandRow) -> HandlerOutcome {
        if row.disposition == "failed" {
            if let Some((reason, summary, detail)) = terminal_wrapup_failure_fields(&row.summary) {
                return respond(json!({
                    "ok": false,
                    "disposition": "failed",
                    "rounds": row.rounds,
                    "summary": summary,
                    "reason": reason,
                    "detail": detail,
                    "replayed": true,
                }));
            }
        }
        respond(json!({
            "ok": row.disposition != "failed",
            "disposition": row.disposition,
            "rounds": row.rounds,
            "summary": row.summary,
            "replayed": true,
        }))
    }

    async fn handle_session_wrapup_value(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let deadline = Instant::now()
            .checked_add(self.wrapup_operation_budget())
            .unwrap_or_else(Instant::now);
        let (session_id, binding) =
            match self.management_binding(channel, request, "session.wrapup") {
                Ok(scope) => scope,
                Err(outcome) => return outcome,
            };
        let command_id = match request.get("command_id") {
            None => None,
            Some(value) => match value.as_str() {
                // Empty ids are rejected so every retrying caller cannot collide on one
                // shared durable ledger key.
                Some(command_id) if !command_id.is_empty() && command_id.len() <= 128 => {
                    Some(command_id)
                }
                _ => return HandlerOutcome::Error {
                    code: "bad_request".to_string(),
                    message:
                        "session.wrapup command_id must be a nonempty string of at most 128 bytes"
                            .to_string(),
                },
            },
        };
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => return store_unavailable_error(),
        };
        let route_project_root = binding.project_root.to_string_lossy().to_string();
        let project_path = match store.authority_project_for_route(&route_project_root, "memories")
        {
            Ok(Some(project)) => project,
            Ok(None) => route_project_root,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "authority_project_resolution_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        if let Some(command_id) = command_id {
            match store.load_wrapup_command(&session_id, command_id) {
                // Rows written by the current terminal-failure path carry a marker in their
                // summary, while older failed rows remain eligible for a successful retry.
                Ok(Some(row))
                    if row.disposition == "failed"
                        && terminal_wrapup_failure_fields(&row.summary).is_none() => {}
                Ok(Some(row)) => return Self::replayed_wrapup_response(row),
                Ok(None) => {}
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "store_load_failed".to_string(),
                        message: error.to_string(),
                    }
                }
            }
        }
        let keep = match request.get("keep") {
            None => 20,
            // Any signed integer clamps into [WRAPUP_KEEP_MIN, WRAPUP_KEEP_MAX]; a negative
            // keep is a clamp input, not an error, matching the boundary-side contract.
            Some(value) => match value.as_i64() {
                Some(value) => usize::try_from(value.max(0))
                    .unwrap_or(usize::MAX)
                    .clamp(WRAPUP_KEEP_MIN, WRAPUP_KEEP_MAX),
                None => {
                    return HandlerOutcome::Error {
                        code: "bad_request".to_string(),
                        message: "session.wrapup keep must be an integer".to_string(),
                    }
                }
            },
        };

        // The module owns the only store writer, so a process-local per-session latch is
        // sufficient to prevent duplicate producer drives. Durable historian state still
        // protects publication if the process exits while a round is running.
        let wrapup_guard = match self.try_claim_wrapup_session(&session_id) {
            Ok(guard) => guard,
            Err(rounds) => {
                return respond(json!({
                    "ok": true,
                    "disposition": "already_in_progress",
                    "rounds": rounds,
                    "summary": format!("wrapup already in progress, {rounds} rounds done"),
                }))
            }
        };

        let entry_state = match store.load(&session_id) {
            Ok(loaded) => loaded,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        let entry_now = now_ms();
        if let Some(until) = entry_state.meta.historian.failure_backoff_at_ms {
            if until > entry_now {
                return Self::retryable_wrapup_response(
                    RetryableWrapupReason::BackoffActive,
                    format!(
                        "historian failure backoff active for {} ms",
                        until.saturating_sub(entry_now)
                    ),
                );
            }
        }

        let snapshot = self
            .transform_snapshots
            .lock()
            .expect("transform snapshots mutex")
            .get(&session_id);
        let ready = match snapshot {
            TransformSnapshotLookup::Ready(ready) => ready,
            TransformSnapshotLookup::LeaseBudgetExceeded => {
                return Self::retryable_wrapup_response(
                    RetryableWrapupReason::SnapshotUnavailable,
                    "too many concurrent wrapups",
                )
            }
            TransformSnapshotLookup::Missing | TransformSnapshotLookup::InFlight => {
                return Self::retryable_wrapup_response(
                    RetryableWrapupReason::SnapshotUnavailable,
                    "wrapup unavailable until a full session transform has been observed",
                )
            }
        };
        let parsed = Arc::clone(&ready.request);
        let initial_snapshot = match store.load_historian_assembly_snapshot(&session_id) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        if ready.revert_epoch != initial_snapshot.revert_epoch {
            return Self::retryable_wrapup_response(
                RetryableWrapupReason::SnapshotStale,
                "wrapup unavailable until a full session transform has been observed",
            );
        }
        let projection = match crate::ck_wire::project_messages(&parsed.messages) {
            Ok(projection) => projection,
            Err(error) => {
                return Self::retryable_wrapup_response(
                    RetryableWrapupReason::SnapshotUnavailable,
                    format!("wrapup boundary assembly failed: {error}"),
                )
            }
        };
        let boundary_messages = wrapup_boundary_messages(&parsed, &projection);
        let initial_compartments = initial_snapshot.compartments;
        let initial_end = initial_compartments
            .iter()
            .map(|compartment| compartment.end_message as u64)
            .max();
        let plan = boundary::resolve_wrapup_boundary(&boundary_messages, initial_end, keep);
        let target = plan.target_protected_start_ordinal;
        match self.wrapup_snapshot_is_current(
            &store,
            &session_id,
            ready.generation,
            ready.revert_epoch,
        ) {
            Ok(true) => {}
            Ok(false) => {
                return Self::retryable_wrapup_response(
                    RetryableWrapupReason::SnapshotStale,
                    "wrapup unavailable until a full session transform has been observed",
                )
            }
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: error.to_string(),
                }
            }
        }
        if plan.raw_messages_above_last_compartment <= keep
            || !wrapup_has_remaining_messages(&parsed.messages, initial_end, target)
        {
            return self.terminal_wrapup_response(
                &store,
                &session_id,
                command_id,
                ready.generation,
                ready.revert_epoch,
                TerminalWrapupResponse {
                    disposition: "nothing_to_compact",
                    rounds: 0,
                    summary: format!(
                        "nothing to compact; {} raw messages already fit within the keep watermark of {keep}",
                        plan.raw_messages_above_last_compartment,
                    ),
                    reason: None,
                    detail: None,
                    include_rounds_without_command: true,
                },
            );
        }

        let mut rounds = 0usize;
        let mut failure: Option<(RetryableWrapupReason, String)> = None;
        let mut terminal_failure: Option<(&'static str, String)> = None;
        // This observation is local to the current runner loop execution. A runner that is
        // still starting gets one bounded chance before a repeated route absence becomes terminal.
        let mut unknown_module_observed_at = None;
        while rounds < historian::MAX_WRAPUP_ROUNDS {
            if Self::remaining_wrapup_budget(deadline).is_none() {
                failure = Some((
                    RetryableWrapupReason::BudgetExhausted,
                    "wrapup request budget expired before the next round".to_string(),
                ));
                break;
            }
            if !self
                .transform_snapshots
                .lock()
                .expect("transform snapshots mutex")
                .ready_generation_matches(&session_id, ready.generation)
            {
                failure = Some((
                    RetryableWrapupReason::SnapshotStale,
                    "wrapup unavailable because a newer full session transform started".to_string(),
                ));
                break;
            }
            let current_state = match store.load(&session_id) {
                Ok(loaded) => loaded,
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "store_load_failed".to_string(),
                        message: error.to_string(),
                    };
                }
            };
            if current_state.meta.revert_epoch != ready.revert_epoch {
                failure = Some((
                    RetryableWrapupReason::SnapshotStale,
                    "wrapup unavailable until a full session transform has been observed"
                        .to_string(),
                ));
                break;
            }
            let round_now = now_ms();
            if unknown_module_observed_at.is_none() {
                if let Some(until) = current_state.meta.historian.failure_backoff_at_ms {
                    if until > round_now {
                        failure = Some((
                            RetryableWrapupReason::BackoffActive,
                            format!(
                                "historian failure backoff active for {} ms",
                                until.saturating_sub(round_now)
                            ),
                        ));
                        break;
                    }
                }
            }
            let current_end = match store.load_compartments(&session_id) {
                Ok(compartments) => compartments
                    .iter()
                    .map(|compartment| compartment.end_message as u64)
                    .max(),
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "store_load_failed".to_string(),
                        message: error.to_string(),
                    };
                }
            };
            if !wrapup_has_remaining_messages(&parsed.messages, current_end, target) {
                break;
            }
            // Assembly re-reads one historian snapshot for this round. The firing carries
            // that snapshot's revert epoch, and publication loads the current row version
            // immediately before its store CAS, so a transform re-cut during production is
            // rejected at publication rather than publishing against retired state.
            let prepared = self.prepare_wrapup_fire(
                Arc::clone(&store),
                &parsed,
                &binding,
                &projection,
                &plan.boundary,
                WrapupPrepareContext {
                    now: round_now,
                    project_path: project_path.clone(),
                    allow_unknown_module_retry: unknown_module_observed_at.is_some(),
                },
            );
            // Assembly performs store reads, so verify the raw-history generation again
            // before joining or driving the action it produced. A transform that started
            // during assembly must invalidate this round.
            if !self
                .transform_snapshots
                .lock()
                .expect("transform snapshots mutex")
                .ready_generation_matches(&session_id, ready.generation)
            {
                failure = Some((
                    RetryableWrapupReason::SnapshotStale,
                    "wrapup unavailable because a newer full session transform started".to_string(),
                ));
                break;
            }
            match prepared {
                PreparedWrapupAction::Busy(completion) => {
                    if let Err(reason) = self
                        .await_wrapup_historian_completion(completion, deadline)
                        .await
                    {
                        let retry_reason = if reason.contains("request budget expired") {
                            RetryableWrapupReason::BudgetExhausted
                        } else {
                            RetryableWrapupReason::SnapshotUnavailable
                        };
                        failure = Some((retry_reason, reason));
                        break;
                    }
                }
                PreparedWrapupAction::Nothing(reason) => {
                    failure = Some((
                        RetryableWrapupReason::SnapshotUnavailable,
                        format!("historian made no forward progress: {reason}"),
                    ));
                    break;
                }
                PreparedWrapupAction::Failed(reason) => {
                    if reason == "no historian models are configured" {
                        terminal_failure = Some(("no_models", reason));
                        break;
                    }
                    let retry_reason = if reason.contains("backoff") {
                        RetryableWrapupReason::BackoffActive
                    } else {
                        RetryableWrapupReason::SnapshotUnavailable
                    };
                    failure = Some((retry_reason, reason));
                    break;
                }
                PreparedWrapupAction::FireReady(task) => {
                    let mut task = *task;
                    task.publication_fence = Some(Arc::new(WrapupSnapshotPublicationFence {
                        snapshots: Arc::clone(&self.transform_snapshots),
                        session_id: session_id.clone(),
                        generation: ready.generation,
                        #[cfg(test)]
                        after_store_publish: Arc::clone(&self.publication_fence_write_hook),
                    }));
                    match self.run_wrapup_firing(task, deadline).await {
                        Ok(historian::HistorianDriveOutcome::Completed(_)) => {
                            let after_end = store.load_compartments(&session_id).ok().and_then(
                                |compartments| {
                                    compartments
                                        .iter()
                                        .map(|compartment| compartment.end_message as u64)
                                        .max()
                                },
                            );
                            if after_end <= current_end {
                                failure = Some((
                                    RetryableWrapupReason::SnapshotUnavailable,
                                    "historian completed without advancing the compartment boundary"
                                        .to_string(),
                                ));
                                break;
                            }
                            rounds += 1;
                            wrapup_guard.set_rounds(rounds);
                        }
                        Ok(historian::HistorianDriveOutcome::Busy(_)) => {
                            failure = Some((
                                RetryableWrapupReason::SnapshotUnavailable,
                                "historian became busy before the round started".to_string(),
                            ));
                            break;
                        }
                        Err(WrapupFiringError::Retryable(reason, detail)) => {
                            failure = Some((reason, detail));
                            break;
                        }
                        Err(WrapupFiringError::Terminal { reason, detail }) => {
                            terminal_failure = Some((reason, detail));
                            break;
                        }
                        Err(WrapupFiringError::UnknownModule(detail)) => {
                            if unknown_module_observed_at.is_some() {
                                terminal_failure = Some(("runner_module_unavailable", detail));
                                break;
                            }
                            unknown_module_observed_at = Some(Instant::now());
                            let delay = self.unknown_module_retry_delay();
                            let Some(remaining) = Self::remaining_wrapup_budget(deadline) else {
                                failure = Some((
                                    RetryableWrapupReason::BudgetExhausted,
                                    "wrapup request budget expired before retrying an unavailable runner module"
                                        .to_string(),
                                ));
                                break;
                            };
                            if delay > remaining {
                                failure = Some((
                                    RetryableWrapupReason::BudgetExhausted,
                                    "wrapup request budget expired before retrying an unavailable runner module"
                                        .to_string(),
                                ));
                                break;
                            }
                            tokio::time::sleep(delay).await;
                        }
                    }
                }
            }
        }

        let final_compartments = match store.load_compartments(&session_id) {
            Ok(compartments) => compartments,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        let final_end = final_compartments
            .iter()
            .map(|compartment| compartment.end_message as u64)
            .max();
        let compacted_messages = parsed
            .messages
            .iter()
            .filter(|message| !message.ck.meta.synthetic)
            .filter(|message| initial_end.is_none_or(|end| message.ordinal > end))
            .filter(|message| final_end.is_some_and(|end| message.ordinal <= end))
            .count();
        let compartments_created = final_compartments
            .len()
            .saturating_sub(initial_compartments.len());
        let remaining = wrapup_has_remaining_messages(&parsed.messages, final_end, target);
        if let Some((reason, detail)) = terminal_failure {
            return self.terminal_wrapup_response(
                &store,
                &session_id,
                command_id,
                ready.generation,
                ready.revert_epoch,
                TerminalWrapupResponse {
                    disposition: "failed",
                    rounds,
                    summary: format!(
                        "compacted {compacted_messages} messages into {compartments_created} compartments; wrapup stopped permanently"
                    ),
                    reason: Some(reason),
                    detail: Some(detail),
                    include_rounds_without_command: true,
                },
            );
        }
        if failure.is_none() && remaining {
            failure = Some((
                RetryableWrapupReason::SnapshotUnavailable,
                format!(
                    "stopped at the {}-round cap before the keep watermark",
                    historian::MAX_WRAPUP_ROUNDS
                ),
            ));
        }
        let effect = "takes effect on your next message";
        match failure {
            Some((reason, detail)) => Self::retryable_wrapup_response(
                reason,
                format!(
                    "compacted {compacted_messages} messages into {compartments_created} compartments; {detail}; {effect}"
                ),
            ),
            None if rounds == 0 => self.terminal_wrapup_response(
                &store,
                &session_id,
                command_id,
                ready.generation,
                ready.revert_epoch,
                TerminalWrapupResponse {
                    disposition: "nothing_to_compact",
                    rounds: 0,
                     summary:
                         "nothing to compact; the tail is already within the keep watermark"
                             .to_string(),
                     reason: None,
                     detail: None,
                     include_rounds_without_command: true,
                },
            ),
            None => self.terminal_wrapup_response(
                &store,
                &session_id,
                command_id,
                ready.generation,
                ready.revert_epoch,
                TerminalWrapupResponse {
                    disposition: "completed",
                    rounds,
                     summary: format!(
                         "compacted {compacted_messages} messages into {compartments_created} compartments; {effect}"
                     ),
                     reason: None,
                     detail: None,
                     include_rounds_without_command: true,
                },
            ),
        }
    }

    fn handle_authority_status_value(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(store) = self.store.get() else {
            return store_unavailable_error();
        };
        let Some((context_store_uuid, project, domain)) = authority_request_key(request) else {
            return invalid_params_error(
                "authority.status requires context_store_uuid, project, and domain",
            );
        };
        match store.authority_status(context_store_uuid, project, domain) {
            Ok(Some(row)) => {
                if row.state == "MODULE" {
                    if let Err(error) =
                        self.bind_authority_route(store, channel, context_store_uuid, project)
                    {
                        return HandlerOutcome::Error {
                            code: "authority_route_binding_failed".to_string(),
                            message: error.to_string(),
                        };
                    }
                }
                respond(json!({ "ok": true, "authority": row }))
            }
            Ok(None) => respond(json!({ "ok": true, "authority": null })),
            Err(error) => HandlerOutcome::Error {
                code: "authority_status_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    fn handle_authority_prepare_value(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(store) = self.store.get() else {
            return store_unavailable_error();
        };
        let Some((context_store_uuid, project, domain)) = authority_request_key(request) else {
            return invalid_params_error(
                "authority.prepare requires context_store_uuid, project, and domain",
            );
        };
        let phase = request
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("begin");
        let result = match phase {
            "begin" => store.authority_begin_prepare(context_store_uuid, project, domain),
            "complete" => {
                let Some(expected_generation) = request.get("generation").and_then(Value::as_u64)
                else {
                    return invalid_params_error("authority.prepare complete requires generation");
                };
                let expected = request
                    .get("checksum_expected")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let actual =
                    match store.authority_seed_checksum(context_store_uuid, project, domain) {
                        Ok(checksum) => checksum,
                        Err(error) => {
                            return HandlerOutcome::Error {
                                code: "authority_checksum_failed".to_string(),
                                message: error.to_string(),
                            }
                        }
                    };
                store.authority_verify_prepare(
                    context_store_uuid,
                    project,
                    domain,
                    expected_generation,
                    expected,
                    &actual,
                )
            }
            "ack" => {
                let Some(expected_generation) = request.get("generation").and_then(Value::as_u64)
                else {
                    return invalid_params_error("authority.prepare ack requires generation");
                };
                store.authority_ack_prepare(
                    context_store_uuid,
                    project,
                    domain,
                    expected_generation,
                )
            }
            "abort" => {
                let Some(expected_generation) = request.get("generation").and_then(Value::as_u64)
                else {
                    return invalid_params_error("authority.prepare abort requires generation");
                };
                store.authority_abort_prepare(
                    context_store_uuid,
                    project,
                    domain,
                    expected_generation,
                )
            }
            _ => {
                return invalid_params_error(
                    "authority.prepare phase must be begin, complete, ack, or abort",
                )
            }
        };
        match result {
            Ok(row) => {
                if row.state == "MODULE" {
                    if let Err(error) =
                        self.bind_authority_route(store, channel, context_store_uuid, project)
                    {
                        return HandlerOutcome::Error {
                            code: "authority_route_binding_failed".to_string(),
                            message: error.to_string(),
                        };
                    }
                }
                respond(json!({ "ok": true, "authority": row }))
            }
            Err(error) => HandlerOutcome::Error {
                code: "authority_prepare_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    fn handle_authority_seed_value(&self, request: &Value) -> HandlerOutcome {
        let Some(store) = self.store.get() else {
            return store_unavailable_error();
        };
        let Some((context_store_uuid, project, domain)) = authority_request_key(request) else {
            return invalid_params_error(
                "authority.seed requires context_store_uuid, project, and domain",
            );
        };
        let Some(rows) = request.get("rows").and_then(Value::as_array) else {
            return invalid_params_error("authority.seed requires a rows array");
        };
        let mut seeded = 0usize;
        let mut module_row_ids = Vec::with_capacity(rows.len());
        for row in rows {
            let source_row_id = row
                .get("source_row_id")
                .and_then(Value::as_i64)
                .or_else(|| {
                    row.get("snapshot")
                        .and_then(|value| value.get("id"))
                        .and_then(Value::as_i64)
                });
            let Some(source_row_id) = source_row_id else {
                return invalid_params_error("authority.seed rows require source_row_id");
            };
            let snapshot = row.get("snapshot").unwrap_or(row);
            if snapshot.get("project_path").and_then(Value::as_str) != Some(project) {
                return HandlerOutcome::Error {
                    code: "authority_seed_project_mismatch".to_string(),
                    message: "seed snapshot project_path did not match the authority project"
                        .to_string(),
                };
            }
            let module_row_id =
                match store.seed_authority_row(context_store_uuid, domain, source_row_id, snapshot)
                {
                    Ok(id) => id,
                    Err(error) => {
                        return HandlerOutcome::Error {
                            code: "authority_seed_failed".to_string(),
                            message: error.to_string(),
                        };
                    }
                };
            module_row_ids.push(module_row_id);
            seeded += 1;
        }
        respond(json!({ "ok": true, "seeded": seeded, "module_row_ids": module_row_ids }))
    }

    fn handle_authority_drain_value(&self, request: &Value, method: &str) -> HandlerOutcome {
        let Some(store) = self.store.get() else {
            return store_unavailable_error();
        };
        let Some((context_store_uuid, project, domain)) = authority_request_key(request) else {
            return invalid_params_error(
                "authority drain requires context_store_uuid, project, and domain",
            );
        };
        let action = request
            .get("action")
            .and_then(Value::as_str)
            .or_else(|| method.strip_prefix("authority.drain."))
            .unwrap_or("step");
        let result = match action {
            "begin" => {
                let lease = request.get("lease").and_then(Value::as_str).unwrap_or("");
                let expires = request
                    .get("lease_expires_at")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let started_at = request
                    .get("lease_started_at")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(now_ms);
                store.authority_begin_drain(
                    context_store_uuid,
                    project,
                    domain,
                    lease,
                    expires,
                    started_at,
                )
            }
            "finish" | "flip" => {
                let Some(generation) = request.get("generation").and_then(Value::as_u64) else {
                    return invalid_params_error("authority drain finish requires generation");
                };
                let token = request
                    .get("coordinator_token")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let now = request
                    .get("now_ms")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(now_ms);
                store.authority_finish_drain(
                    context_store_uuid,
                    project,
                    domain,
                    generation,
                    request
                        .get("checksum_expected")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    request
                        .get("checksum_actual")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    request
                        .get("verified")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    token,
                    now,
                )
            }
            step => {
                let Some(generation) = request.get("generation").and_then(Value::as_u64) else {
                    return invalid_params_error("authority drain step requires generation");
                };
                let step = step.strip_prefix("drain_").unwrap_or(step);
                let token = request
                    .get("coordinator_token")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let now = request
                    .get("now_ms")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(now_ms);
                store.authority_drain_step(
                    context_store_uuid,
                    project,
                    domain,
                    generation,
                    step,
                    request.get("cursor").and_then(Value::as_i64),
                    token,
                    now,
                )
            }
        };
        match result {
            Ok(row) => respond(json!({ "ok": true, "authority": row })),
            Err(McStoreError::AuthorityFeedHeadAdvanced { captured, found }) => {
                HandlerOutcome::Error {
                    code: "authority_feed_head_advanced".to_string(),
                    message: format!(
                        "authority_feed_head_advanced: captured {captured}, found {found}"
                    ),
                }
            }
            Err(error) => HandlerOutcome::Error {
                code: "authority_drain_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    fn handle_mirror_pull_value(&self, request: &Value) -> HandlerOutcome {
        let Some(store) = self.store.get() else {
            return store_unavailable_error();
        };
        let Some(domain) = request.get("domain").and_then(Value::as_str) else {
            return invalid_params_error("mirror.pull requires domain");
        };
        let cursor = request.get("cursor").and_then(Value::as_i64).unwrap_or(0);
        let limit = request.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
        let page = if request
            .get("live_only")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if domain != "memories" {
                return invalid_params_error("live mirror snapshots currently support memories");
            }
            store.pull_live_memory_snapshot(cursor, limit)
        } else {
            store.pull_changefeed(domain, cursor, limit)
        };
        match page {
            Ok(page) => respond(json!({ "ok": true, "page": page })),
            Err(error) => HandlerOutcome::Error {
                code: "mirror_pull_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    fn handle_guidance_value(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => return store_unavailable_error(),
        };
        let Some(session_id) = request.get("session_id").and_then(Value::as_str) else {
            return HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: "guidance.get requires session_id".to_string(),
            };
        };
        if let Err(error) = self.resolve_binding(channel, session_id) {
            return match error {
                BindingError::Unbound => HandlerOutcome::Error {
                    code: "route_unbound".to_string(),
                    message: "guidance.get on a channel with no session binding".to_string(),
                },
                BindingError::SessionMismatch => HandlerOutcome::Error {
                    code: "session_mismatch".to_string(),
                    message: "request session_id does not match the channel's bound session"
                        .to_string(),
                },
            };
        }

        let tool_present = match request.get("tool_present") {
            None => false,
            Some(value) => match value.as_bool() {
                Some(value) => value,
                None => {
                    return HandlerOutcome::Error {
                        code: "bad_request".to_string(),
                        message: "guidance.get tool_present must be a boolean".to_string(),
                    }
                }
            },
        };
        let profile = match request.get("serializer_profile").and_then(Value::as_str) {
            None => Some(SerializerProfile::ClaudeCodeAnthropic),
            Some(value) => match SerializerProfile::parse(value) {
                Some(profile) => Some(profile),
                None => return unknown_serializer_profile_error(),
            },
        };
        let active = cc_u1_active(profile, tool_present);
        let expected_variant = if active { "full" } else { "no_reduce" };
        if let Some(variant) = request.get("variant").and_then(Value::as_str) {
            if variant != expected_variant {
                return HandlerOutcome::Error {
                    code: "bad_request".to_string(),
                    message: format!(
                        "guidance variant {variant:?} contradicts tool_present={tool_present}"
                    ),
                };
            }
        }
        let date_line = match self.guidance_date_for_session(&store, session_id) {
            Ok(date) => date,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_write_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        let base_text = if active {
            GUIDANCE_TEXT
        } else {
            GUIDANCE_TEXT_NO_REDUCE
        };
        let language_text = request
            .get("language")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|language| !language.is_empty());
        let language_directive = language_text.and_then(primary_language_directive);
        let localized_text = language_directive
            .as_deref()
            .map(|directive| format!("{base_text}\n\n{directive}"));
        let text_for_bytes = localized_text.as_deref().unwrap_or(base_text);
        let bytes = guidance_bytes_for(text_for_bytes, &date_line);
        respond(json!({
            "ok": true,
            "bytes": bytes,
            "hash": sha256_hex(bytes.as_bytes()),
            // The guidance text is the only part reflected in render_config. The session
            // date line changes every day, so content_hash excludes it; otherwise a
            // date-only change would trigger cache refreshes even when guidance is
            // unchanged.
            "content_hash": sha256_hex(text_for_bytes.as_bytes()),
        }))
    }

    fn guidance_date_for_session(
        &self,
        store: &McStore,
        session_id: &str,
    ) -> Result<String, mc_store::McStoreError> {
        for _ in 0..2 {
            let loaded = store.load(session_id)?;
            if !loaded.meta.guidance_date.is_empty() {
                self.guidance_dates
                    .lock()
                    .expect("guidance date mutex")
                    .remove(session_id);
                return Ok(loaded.meta.guidance_date);
            }
            let date_line = self
                .guidance_dates
                .lock()
                .expect("guidance date mutex")
                .entry(session_id.to_string())
                .or_insert_with(|| self.guidance_date_line())
                .clone();
            let Some(expected) = loaded.row_version else {
                return Ok(date_line);
            };
            let mut meta = loaded.meta.clone();
            meta.guidance_date.clone_from(&date_line);
            match store.commit(session_id, Some(expected), &loaded.core, &meta) {
                Ok(_) => return Ok(date_line),
                Err(mc_store::McStoreError::CasConflict { .. }) => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(self
            .guidance_dates
            .lock()
            .expect("guidance date mutex")
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| self.guidance_date_line()))
    }

    fn handle_status_value(&self, request: &Value) -> HandlerOutcome {
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => return store_unavailable_error(),
        };
        let Some(session_id) = request.get("session_id").and_then(Value::as_str) else {
            return match store.load("__health__") {
                Ok(state) => respond(json!({
                    "ok": true,
                    "store_open": true,
                    "initialized": state.meta.initialized,
                    "row_version": state.row_version,
                    "epochs": {
                        "memory_render_epoch": MEMORY_RENDER_FORMAT_EPOCH,
                        "compartment_render_epoch": COMPARTMENT_RENDER_FORMAT_EPOCH,
                        "profile_epoch": PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC,
                        "tagger_epoch": TAGGER_FEATURE_EPOCH,
                    },
                })),
                Err(e) => HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: e.to_string(),
                },
            };
        };
        let loaded = match store.load(session_id) {
            Ok(loaded) => loaded,
            Err(e) => {
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: e.to_string(),
                }
            }
        };
        let pass_trace = match store.load_pass_trace(session_id) {
            Ok(pass_trace) => pass_trace,
            Err(e) => {
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: e.to_string(),
                }
            }
        };
        respond(json!({
            "ok": true,
            "store_open": true,
            "session_id": session_id,
            "initialized": loaded.meta.initialized,
            "row_version": loaded.row_version,
            "historian": loaded.meta.historian,
            "publication_floor_ordinal": loaded.meta.publication_floor_ordinal,
            "pass_trace": pass_trace,
            "epochs": {
                "memory_render_epoch": MEMORY_RENDER_FORMAT_EPOCH,
                "compartment_render_epoch": COMPARTMENT_RENDER_FORMAT_EPOCH,
                "profile_epoch": PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC,
                "tagger_epoch": TAGGER_FEATURE_EPOCH,
            },
        }))
    }

    async fn handle_transform_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        self.handle_transform_unpaged_value(channel, request, false)
            .await
    }

    async fn handle_transform_unpaged_value(
        &self,
        channel: u16,
        request: Value,
        from_page_apply: bool,
    ) -> HandlerOutcome {
        let parsed: TransformRequest = match serde_json::from_value(request.clone()) {
            Ok(req) => req,
            Err(e) => {
                return HandlerOutcome::Error {
                    code: "bad_request".to_string(),
                    message: e.to_string(),
                }
            }
        };
        let serializer_profile = SerializerProfile::parse(&parsed.serializer_profile);
        if serializer_profile.is_none() {
            return unknown_serializer_profile_error();
        }
        if parsed.serve_native && serializer_profile != Some(SerializerProfile::OpencodeAiSdk) {
            return serve_native_unsupported_profile_error(&parsed.serializer_profile);
        }
        if parsed.tail_delta.is_some() {
            return need_full_sync_response(parsed.full_array_fingerprint.clone());
        }
        // The module's own producer sessions must NEVER be transformed: the historian's
        // request is a raw structured-extraction call whose [system, user] shape is part
        // of the prompt calibration. Identity pass-through, no store reads, no historian
        // evaluation (a transform here would recurse the historian into itself).
        if parsed
            .session_id
            .starts_with(historian::MC_CHILD_SESSION_PREFIX)
        {
            // The established historian namespace remains accepted for compatibility with
            // existing producer sessions. Dreamer IDs instead require registration and route
            // validation before they may bypass the transform.
            let mut response = transform::TransformResponse::passthrough(
                parsed.messages.iter().map(|m| m.ck.clone()).collect(),
                parsed.full_array_fingerprint.clone(),
            );
            attach_native_messages(&mut response, &parsed, 0);
            return respond(serde_json::to_value(response).unwrap_or(Value::Null));
        }
        if self.dreamer_run_registered(&parsed.session_id) {
            // Registration is the authority for a dreamer exemption. Validate the route
            // before trusting it so a stale or cross-project channel cannot bypass transform.
            match self.resolve_binding(channel, &parsed.session_id) {
                Ok(binding) if !is_shadow_session(&binding.session) => {
                    let mut response = transform::TransformResponse::passthrough(
                        parsed.messages.iter().map(|m| m.ck.clone()).collect(),
                        parsed.full_array_fingerprint.clone(),
                    );
                    attach_native_messages(&mut response, &parsed, 0);
                    return respond(serde_json::to_value(response).unwrap_or(Value::Null));
                }
                Ok(_) => {
                    return HandlerOutcome::Error {
                        code: "plain_transform_on_shadow_binding".to_string(),
                        message: "registered dreamer session cannot use a shadow route".to_string(),
                    }
                }
                Err(BindingError::Unbound) => {
                    return HandlerOutcome::Error {
                        code: "route_unbound".to_string(),
                        message: "registered dreamer session has no bound route".to_string(),
                    }
                }
                Err(BindingError::SessionMismatch) => {
                    return HandlerOutcome::Error {
                        code: "session_mismatch".to_string(),
                        message: "registered dreamer session does not match the bound route"
                            .to_string(),
                    }
                }
            }
        }
        let parsed = Arc::new(parsed);
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => {
                return HandlerOutcome::Error {
                    code: "store_unavailable".to_string(),
                    message: "store not opened (no HELLO_ACK storage seam)".to_string(),
                }
            }
        };
        let binding = match self.resolve_binding(channel, &parsed.session_id) {
            Ok(b) => b,
            Err(BindingError::Unbound) => {
                return HandlerOutcome::Error {
                    code: "route_unbound".to_string(),
                    message: "transform on a channel with no session binding".to_string(),
                }
            }
            Err(BindingError::SessionMismatch) => {
                return HandlerOutcome::Error {
                    code: "session_mismatch".to_string(),
                    message: "request session_id does not match the channel's bound session"
                        .to_string(),
                }
            }
        };
        if is_shadow_session(&binding.session) {
            return HandlerOutcome::Error {
                code: "plain_transform_on_shadow_binding".to_string(),
                message: "use shadow_transform for routes bound as shadow:<real_session>"
                    .to_string(),
            };
        }
        let lineage_root = canonical_root(&binding.project_root);
        self.transform_route_channels
            .lock()
            .expect("transform route channels mutex")
            .insert(channel, (binding.session.clone(), lineage_root.clone()));
        self.transform_session_roots
            .lock()
            .expect("transform session roots mutex")
            .entry(binding.session.clone())
            .or_default()
            .insert(lineage_root);
        if !from_page_apply && self.transform_page_in_progress(&binding.session) {
            return HandlerOutcome::Error {
                code: "authority_transform_page_in_progress".to_string(),
                message: "transform is blocked until all transform pages arrive".to_string(),
            };
        }
        // A newer full transform invalidates the prior wrapup snapshot before any store
        // mutation. If this pass later rejects, wrapup must not pair old raw bytes with
        // the state that the rejected pass may already have re-cut.
        let snapshot_generation = self
            .transform_snapshots
            .lock()
            .expect("transform snapshots mutex")
            .begin(&parsed.session_id);
        let route_project_root = binding.project_root.to_string_lossy().to_string();
        // Resolve the route root to the memory and note owner keys before any store read.
        // Keep the filesystem directory only for project documents and configuration below.
        let project_path = match store.authority_project_for_route(&route_project_root, "memories")
        {
            Ok(Some(project)) => project,
            Ok(None) => route_project_root.clone(),
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "authority_project_resolution_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        let note_project_path =
            match store.authority_project_for_route(&route_project_root, "notes") {
                Ok(Some(project)) => project,
                Ok(None) => route_project_root.clone(),
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "authority_project_resolution_failed".to_string(),
                        message: error.to_string(),
                    }
                }
            };
        let pass_now = now_ms();
        // This trace is intentionally outside the fenced cache-state commit: a rejected
        // pass must still leave a durable breadcrumb, and a trace failure must never
        // change the transform result.
        let _ = store.trace_pass_received(&parsed.session_id, pass_now);
        let run_transform = || {
            let producer_ctx = transform::ProducerContext {
                project_path: &project_path,
                note_project_path: &note_project_path,
                project_directory: &route_project_root,
                // The authority adapter resolves this from the model context limit and
                // sends it on each pass. Keep the bind-time value only for older callers
                // that omit the field, and reject unusable values without disabling decay.
                history_budget_tokens: parsed
                    .history_budget_tokens
                    .filter(|budget| budget.is_finite() && *budget >= 0.0)
                    .unwrap_or(binding.history_budget_tokens),
                memory_enabled: binding.config.memory_enabled,
                now_ms: pass_now,
                execute_threshold_percentage: binding.config.execute_threshold_percentage,
                smart_drops: binding.config.smart_drops,
                cache_ttl: binding.config.cache_ttl.clone(),
                model_key: binding.model_key.clone(),
                observed_last_response_at_ms: self
                    .observed_last_response_at_ms(&store, &parsed.session_id),
                guidance_date: Some(self.guidance_date_for_transform(&parsed.session_id, pass_now)),
                #[cfg(test)]
                injected_reductions: self
                    .reduction_injection
                    .lock()
                    .expect("reduction injection mutex")
                    .remove(&parsed.session_id)
                    .unwrap_or_default(),
            };
            transform_with_projection(&store, &parsed, &producer_ctx)
        };
        let reject_transform = |e: crate::transform::TransformError| {
            let message = e.to_string();
            let _ = store.trace_pass_rejected(&parsed.session_id, &message, now_ms());
            HandlerOutcome::Error {
                code: "transform_failed".to_string(),
                message,
            }
        };
        let mut result = match run_transform() {
            Ok(result) => result,
            Err(e) => return reject_transform(e),
        };
        let mut emergency_pre_floor =
            if result.scheduler_pass == scheduler::PassDecision::Emergency95 {
                store
                    .load(&parsed.session_id)
                    .map(|state| state.meta.publication_floor_ordinal)
                    .unwrap_or(None)
            } else {
                None
            };
        #[cfg(test)]
        if let Some(hook) = self
            .between_transform_and_prepare
            .lock()
            .expect("interleave hook mutex")
            .take()
        {
            hook();
        }
        let diagnostics = if result.scheduler_pass == scheduler::PassDecision::Emergency95 {
            match self.prepare_historian_fire(
                Arc::clone(&store),
                &parsed,
                &binding,
                &project_path,
                &result.projection,
                HistorianPrepareContext {
                    now: pass_now,
                    snapshot_generation,
                },
            ) {
                PreparedHistorianAction::Complete(diagnostics) => diagnostics,
                PreparedHistorianAction::Busy {
                    diagnostics,
                    completion,
                } => {
                    if self.await_live_historian_completion(completion).await {
                        result = match run_transform() {
                            Ok(result) => result,
                            Err(e) => return reject_transform(e),
                        };
                        emergency_pre_floor = store
                            .load(&parsed.session_id)
                            .map(|state| state.meta.publication_floor_ordinal)
                            .unwrap_or(None);
                        match self.prepare_historian_fire(
                            Arc::clone(&store),
                            &parsed,
                            &binding,
                            &project_path,
                            &result.projection,
                            HistorianPrepareContext {
                                now: pass_now,
                                snapshot_generation,
                            },
                        ) {
                            PreparedHistorianAction::Complete(diagnostics) => diagnostics,
                            PreparedHistorianAction::Busy { diagnostics, .. } => diagnostics,
                            PreparedHistorianAction::FireReady(prepared) => {
                                let diagnostics = prepared.diagnostics.clone();
                                match self.run_historian_firing_inline(prepared.task).await {
                                    Ok(_) => {
                                        result = match run_transform() {
                                            Ok(result) => result,
                                            Err(e) => return reject_transform(e),
                                        };
                                        emergency_pre_floor = store
                                            .load(&parsed.session_id)
                                            .map(|state| state.meta.publication_floor_ordinal)
                                            .unwrap_or(None);
                                        diagnostics
                                    }
                                    Err(_) => self.refresh_historian_diagnostics(
                                        &store,
                                        &parsed.session_id,
                                        diagnostics,
                                    ),
                                }
                            }
                        }
                    } else {
                        diagnostics
                    }
                }
                PreparedHistorianAction::FireReady(prepared) => {
                    let diagnostics = prepared.diagnostics.clone();
                    match self.run_historian_firing_inline(prepared.task).await {
                        Ok(_) => {
                            result = match run_transform() {
                                Ok(result) => result,
                                Err(e) => return reject_transform(e),
                            };
                            emergency_pre_floor = store
                                .load(&parsed.session_id)
                                .map(|state| state.meta.publication_floor_ordinal)
                                .unwrap_or(None);
                            diagnostics
                        }
                        Err(_) => self.refresh_historian_diagnostics(
                            &store,
                            &parsed.session_id,
                            diagnostics,
                        ),
                    }
                }
            }
        } else {
            match self.prepare_historian_fire(
                Arc::clone(&store),
                &parsed,
                &binding,
                &project_path,
                &result.projection,
                HistorianPrepareContext {
                    now: pass_now,
                    snapshot_generation,
                },
            ) {
                PreparedHistorianAction::Complete(diagnostics) => diagnostics,
                PreparedHistorianAction::Busy { diagnostics, .. } => diagnostics,
                PreparedHistorianAction::FireReady(prepared) => {
                    let diagnostics = prepared.diagnostics.clone();
                    self.spawn_historian_firing(prepared.task);
                    diagnostics
                }
            }
        };
        // Emergency passes must return the freshest fold obtainable in this request: an
        // active run can publish between this request's transform and any of the arms
        // above (live entry already released, inline attempt failed, busy wait timed
        // out). One final check catches every such interleaving — a PUBLISH is the only
        // event that advances the publication floor (an abandon also bumps the row
        // version, so row advancement alone would re-run spuriously after a failed
        // inline drive); if the floor moved past what this request's transform saw,
        // re-run once so the response carries the published fold instead of pre-fold
        // bytes.
        if result.scheduler_pass == scheduler::PassDecision::Emergency95 {
            let floor_advanced = store
                .load(&parsed.session_id)
                .map(|state| state.meta.publication_floor_ordinal != emergency_pre_floor)
                .unwrap_or(false);
            if floor_advanced {
                result = match run_transform() {
                    Ok(result) => result,
                    Err(e) => return reject_transform(e),
                };
            }
        }
        let mut response = result.response;
        if response.committed {
            self.guidance_dates
                .lock()
                .expect("guidance date mutex")
                .remove(&parsed.session_id);
        }
        response.historian = Some(diagnostics);
        let reasoning_watermark = store
            .load(&parsed.session_id)
            .map(|state| state.meta.reasoning_cleared_through_ordinal)
            .unwrap_or(0);
        attach_native_messages(&mut response, &parsed, reasoning_watermark);
        let _ = store.trace_pass_completed(&parsed.session_id, now_ms());
        self.record_response_observation(&parsed.session_id, now_ms());
        // Management requests carry identity but not raw history. A successful full pass
        // retains its decoded request together with the durable revert epoch it observed.
        // Serialization accounts the payload bytes for the cross-session LRU bound.
        let retained_bytes = serde_json::to_vec(parsed.as_ref())
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if let Ok(loaded) = store.load(&parsed.session_id) {
            self.transform_snapshots
                .lock()
                .expect("transform snapshots mutex")
                .finish_ready(
                    &parsed.session_id,
                    snapshot_generation,
                    Arc::clone(&parsed),
                    loaded.meta.revert_epoch,
                    retained_bytes,
                );
        }
        respond(serde_json::to_value(response).unwrap_or(Value::Null))
    }

    #[cfg(test)]
    async fn handle_transform_for_test(&self, channel: u16, request: Value) -> HandlerOutcome {
        self.handle_transform_value(channel, request).await
    }

    fn handle_state_sync_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        const ENVELOPE_FIELDS: [&str; 5] = [
            "seed_id",
            "seed_generation",
            "seed_batch_index",
            "seed_batch_total",
            "seed_complete",
        ];
        let envelope_fields_present = ENVELOPE_FIELDS
            .iter()
            .filter(|field| request.get(**field).is_some())
            .count();
        let parsed: ShadowStateSyncWire = match serde_json::from_value(request.clone()) {
            Ok(req) => req,
            Err(error) => {
                if envelope_fields_present > 0 {
                    if let Ok(binding) = self.state_sync_binding(channel, None) {
                        self.discard_shadow_seed(&binding.session);
                    }
                }
                return invalid_params_error(error.to_string());
            }
        };
        let binding = match self.state_sync_binding(channel, parsed.session_id.as_deref()) {
            Ok(binding) => binding,
            Err(outcome) => return outcome,
        };
        let lane = if is_shadow_session(&binding.session) {
            StateSyncLane::Shadow
        } else {
            StateSyncLane::Authority
        };
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => return store_unavailable_error(),
        };

        if envelope_fields_present == 0 {
            let awaiting_attempt = {
                let seeds = self.shadow_seeds.lock().expect("shadow seed mutex");
                match seeds
                    .sessions
                    .get(&binding.session)
                    .map(|state| &state.phase)
                {
                    Some(ShadowSeedPhase::Collecting(_) | ShadowSeedPhase::Applying { .. }) => {
                        return HandlerOutcome::Error {
                            code: "shadow_seed_in_progress".to_string(),
                            message: "a paged shadow seed is already in progress".to_string(),
                        };
                    }
                    Some(ShadowSeedPhase::AwaitingSeed {
                        generation,
                        expected_seq,
                    }) => Some((*generation, *expected_seq)),
                    Some(ShadowSeedPhase::Idle) | None => None,
                }
            };
            let outcome = self.apply_state_sync_wire(&binding, &store, parsed, lane);
            if let Some((generation, expected_seq)) = awaiting_attempt {
                let mut seeds = self.shadow_seeds.lock().expect("shadow seed mutex");
                let still_same_attempt =
                    seeds.sessions.get(&binding.session).is_some_and(|state| {
                        matches!(
                            state.phase,
                            ShadowSeedPhase::AwaitingSeed {
                                generation: current_generation,
                                expected_seq: current_seq,
                            } if current_generation == generation && current_seq == expected_seq
                        )
                    });
                if still_same_attempt {
                    seeds.discard_pending(&binding.session);
                }
            }
            return outcome;
        }

        if envelope_fields_present != ENVELOPE_FIELDS.len() {
            self.discard_shadow_seed(&binding.session);
            return invalid_params_error("seed envelope must be all-or-none");
        }
        let seed_id = parsed.seed_id.clone().unwrap_or_default();
        let seed_generation = parsed.seed_generation.unwrap_or_default();
        let batch_index = parsed.seed_batch_index.unwrap_or_default();
        let batch_total = parsed.seed_batch_total.unwrap_or_default();
        let seed_complete = parsed.seed_complete.unwrap_or(false);
        if seed_id.is_empty() || seed_id.len() > SHADOW_SEED_MAX_ID_BYTES {
            self.discard_shadow_seed(&binding.session);
            return invalid_params_error(format!(
                "seed_id must contain 1..={SHADOW_SEED_MAX_ID_BYTES} bytes"
            ));
        }
        let digest = shadow_seed_content_digest(&request);
        {
            let seeds = self.shadow_seeds.lock().expect("shadow seed mutex");
            if let Some(completed) = seeds
                .sessions
                .get(&binding.session)
                .and_then(|state| state.completed.as_ref())
                .filter(|completed| completed.seed_id == seed_id)
            {
                if completed.final_digest == digest {
                    return HandlerOutcome::Response(completed.result.clone());
                }
                return HandlerOutcome::Error {
                    code: "shadow_seed_digest_mismatch".to_string(),
                    message: format!(
                        "completed seed content changed (generation={}, seq={}, total={})",
                        completed.generation, completed.expected_seq, completed.total
                    ),
                };
            }
        }
        if parsed.shadow_generation != seed_generation {
            self.discard_shadow_seed(&binding.session);
            return HandlerOutcome::Error {
                code: "shadow_seed_attempt_mismatch".to_string(),
                message: "shadow_generation must match seed_generation".to_string(),
            };
        }
        if batch_total == 0 || batch_index >= batch_total {
            self.discard_shadow_seed(&binding.session);
            return HandlerOutcome::Error {
                code: "shadow_seed_protocol_mismatch".to_string(),
                message: "seed batch index/total is invalid".to_string(),
            };
        }
        if seed_complete != (batch_index + 1 == batch_total) {
            self.discard_shadow_seed(&binding.session);
            return HandlerOutcome::Error {
                code: "shadow_seed_protocol_mismatch".to_string(),
                message: "seed_complete disagrees with the final batch index".to_string(),
            };
        }
        let scalar_tail_fields = [
            "seed_boundary_id",
            "workspace",
            "last_todo_state",
            "acked_watermarks",
        ]
        .iter()
        .filter(|field| {
            request
                .as_object()
                .is_some_and(|object| object.contains_key(**field))
        })
        .count();
        let drop_seed_skipped_present = request
            .as_object()
            .is_some_and(|object| object.contains_key("drop_seed_skipped"));
        if !seed_complete && (scalar_tail_fields != 0 || drop_seed_skipped_present)
            || seed_complete && scalar_tail_fields != 4
        {
            self.discard_shadow_seed(&binding.session);
            return HandlerOutcome::Error {
                code: "shadow_seed_protocol_mismatch".to_string(),
                message: "seed scalar tail must appear only and completely on the final batch"
                    .to_string(),
            };
        }

        let batch_bytes = match serde_json::to_vec(&request) {
            Ok(bytes) => bytes.len(),
            Err(error) => {
                self.discard_shadow_seed(&binding.session);
                return invalid_params_error(error.to_string());
            }
        };
        // Batch zero is checked against durable metadata before the process-local state is
        // touched. A stale retry therefore cannot evict or allocate another live attempt.
        if batch_index == 0 {
            let loaded = match store.load(&binding.session) {
                Ok(loaded) => loaded,
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "store_load_failed".to_string(),
                        message: error.to_string(),
                    };
                }
            };
            if loaded.meta.shadow_generation != seed_generation {
                return HandlerOutcome::Error {
                    code: "shadow_generation_mismatch".to_string(),
                    message: format!(
                        "seed_generation {seed_generation} did not match durable generation {}",
                        loaded.meta.shadow_generation
                    ),
                };
            }
            if loaded.meta.shadow_seq != parsed.expected_shadow_seq {
                return state_sync_seq_mismatch_error(
                    lane,
                    parsed.expected_shadow_seq,
                    loaded.meta.shadow_seq,
                );
            }
        }

        enum StageAction {
            Ack(usize),
            Apply {
                batches: Vec<ShadowStateSyncWire>,
                seed_id: String,
                final_digest: String,
                generation: u64,
                expected_seq: u64,
                total: usize,
            },
        }

        let action = {
            let mut seeds = self.shadow_seeds.lock().expect("shadow seed mutex");
            let phase = {
                let state = seeds.sessions.entry(binding.session.clone()).or_default();
                std::mem::replace(&mut state.phase, ShadowSeedPhase::Idle)
            };
            // Authority callers can send a paged initial feed without a separate reset.
            // Their bound real session already owns the current generation, so arm the
            // same bounded collector automatically for the first batch.
            let phase = match phase {
                ShadowSeedPhase::Idle if !lane.is_shadow() && batch_index == 0 => {
                    ShadowSeedPhase::AwaitingSeed {
                        generation: seed_generation,
                        expected_seq: parsed.expected_shadow_seq,
                    }
                }
                phase => phase,
            };
            match phase {
                ShadowSeedPhase::Idle => {
                    seeds.set_phase(&binding.session, ShadowSeedPhase::Idle);
                    return HandlerOutcome::Error {
                        code: "shadow_seed_not_armed".to_string(),
                        message: "paged shadow seeds require a committed shadow_reset".to_string(),
                    };
                }
                applying @ ShadowSeedPhase::Applying { .. } => {
                    seeds.set_phase(&binding.session, applying);
                    return HandlerOutcome::Error {
                        code: "shadow_seed_in_progress".to_string(),
                        message: "the final shadow seed batch is being applied".to_string(),
                    };
                }
                awaiting @ ShadowSeedPhase::AwaitingSeed {
                    generation,
                    expected_seq,
                } => {
                    if batch_index != 0
                        || generation != seed_generation
                        || expected_seq != parsed.expected_shadow_seq
                    {
                        seeds.release_phase(&awaiting);
                        return HandlerOutcome::Error {
                            code: "shadow_seed_attempt_mismatch".to_string(),
                            message: "seed batch does not match the reset-armed attempt"
                                .to_string(),
                        };
                    }
                    if batch_bytes > seeds.max_staged_bytes
                        || seeds
                            .total_staged_bytes
                            .checked_add(batch_bytes)
                            .is_none_or(|bytes| bytes > seeds.max_staged_bytes)
                    {
                        seeds.release_phase(&awaiting);
                        return HandlerOutcome::Error {
                            code: "shadow_seed_buffer_overflow".to_string(),
                            message: "shadow seed staging exceeded the handler-wide byte cap"
                                .to_string(),
                        };
                    }
                    seeds.total_staged_bytes += batch_bytes;
                    if seed_complete {
                        seeds.set_phase(
                            &binding.session,
                            ShadowSeedPhase::Applying {
                                seed_id: seed_id.clone(),
                                bytes: batch_bytes,
                            },
                        );
                        StageAction::Apply {
                            batches: vec![parsed],
                            seed_id: seed_id.clone(),
                            final_digest: digest,
                            generation: seed_generation,
                            expected_seq,
                            total: batch_total,
                        }
                    } else {
                        seeds.set_phase(
                            &binding.session,
                            ShadowSeedPhase::Collecting(PendingShadowSeed {
                                seed_id: seed_id.clone(),
                                generation: seed_generation,
                                expected_seq,
                                total: batch_total,
                                next_index: 1,
                                digests: vec![digest],
                                batches: vec![parsed],
                                bytes: batch_bytes,
                            }),
                        );
                        StageAction::Ack(1)
                    }
                }
                ShadowSeedPhase::Collecting(mut pending) => {
                    if pending.seed_id != seed_id
                        || pending.generation != seed_generation
                        || pending.expected_seq != parsed.expected_shadow_seq
                        || pending.total != batch_total
                    {
                        let discarded = ShadowSeedPhase::Collecting(pending);
                        seeds.release_phase(&discarded);
                        return HandlerOutcome::Error {
                            code: "shadow_seed_attempt_mismatch".to_string(),
                            message: "seed envelope changed during collection".to_string(),
                        };
                    }
                    if batch_index < pending.next_index {
                        let matches = pending
                            .digests
                            .get(batch_index)
                            .is_some_and(|accepted| accepted == &digest);
                        if matches {
                            let next_index = pending.next_index;
                            seeds.set_phase(&binding.session, ShadowSeedPhase::Collecting(pending));
                            StageAction::Ack(next_index)
                        } else {
                            let discarded = ShadowSeedPhase::Collecting(pending);
                            seeds.release_phase(&discarded);
                            return HandlerOutcome::Error {
                                code: "shadow_seed_digest_mismatch".to_string(),
                                message: "redriven seed batch content changed".to_string(),
                            };
                        }
                    } else if batch_index > pending.next_index {
                        let discarded = ShadowSeedPhase::Collecting(pending);
                        seeds.release_phase(&discarded);
                        return HandlerOutcome::Error {
                            code: "shadow_seed_order_mismatch".to_string(),
                            message: "seed batches must arrive in strict index order".to_string(),
                        };
                    } else {
                        let next_seed_bytes = pending.bytes.checked_add(batch_bytes);
                        let next_total_bytes = seeds.total_staged_bytes.checked_add(batch_bytes);
                        if next_seed_bytes.is_none_or(|bytes| bytes > seeds.max_staged_bytes)
                            || next_total_bytes.is_none_or(|bytes| bytes > seeds.max_staged_bytes)
                        {
                            let discarded = ShadowSeedPhase::Collecting(pending);
                            seeds.release_phase(&discarded);
                            return HandlerOutcome::Error {
                                code: "shadow_seed_buffer_overflow".to_string(),
                                message: "shadow seed staging exceeded the handler-wide byte cap"
                                    .to_string(),
                            };
                        }
                        pending.bytes = next_seed_bytes.unwrap_or(usize::MAX);
                        seeds.total_staged_bytes = next_total_bytes.unwrap_or(usize::MAX);
                        pending.next_index += 1;
                        pending.digests.push(digest.clone());
                        pending.batches.push(parsed);
                        if seed_complete {
                            let bytes = pending.bytes;
                            let batches = std::mem::take(&mut pending.batches);
                            let generation = pending.generation;
                            let expected_seq = pending.expected_seq;
                            let total = pending.total;
                            let completed_seed_id = pending.seed_id.clone();
                            seeds.set_phase(
                                &binding.session,
                                ShadowSeedPhase::Applying {
                                    seed_id: completed_seed_id.clone(),
                                    bytes,
                                },
                            );
                            StageAction::Apply {
                                batches,
                                seed_id: completed_seed_id,
                                final_digest: digest,
                                generation,
                                expected_seq,
                                total,
                            }
                        } else {
                            let next_index = pending.next_index;
                            seeds.set_phase(&binding.session, ShadowSeedPhase::Collecting(pending));
                            StageAction::Ack(next_index)
                        }
                    }
                }
            }
        };

        match action {
            StageAction::Ack(next_expected_index) => respond(json!({
                "ok": true,
                "staged": true,
                "next_expected_index": next_expected_index,
            })),
            StageAction::Apply {
                batches,
                seed_id,
                final_digest,
                generation,
                expected_seq,
                total,
            } => {
                let assembled = assemble_shadow_seed(batches, generation, expected_seq);
                let outcome = self.apply_state_sync_wire(&binding, &store, assembled, lane);
                let completed_result = match &outcome {
                    HandlerOutcome::Response(bytes) => Some(bytes.clone()),
                    HandlerOutcome::Error { .. } | HandlerOutcome::Streamed => None,
                };
                let mut seeds = self.shadow_seeds.lock().expect("shadow seed mutex");
                let phase = {
                    let state = seeds.sessions.entry(binding.session.clone()).or_default();
                    std::mem::replace(&mut state.phase, ShadowSeedPhase::Idle)
                };
                match phase {
                    ShadowSeedPhase::Applying {
                        seed_id: applying_seed_id,
                        bytes,
                    } if applying_seed_id == seed_id => {
                        seeds.release_phase(&ShadowSeedPhase::Applying {
                            seed_id: applying_seed_id,
                            bytes,
                        });
                        if let Some(result) = completed_result {
                            seeds
                                .sessions
                                .entry(binding.session.clone())
                                .or_default()
                                .completed = Some(CompletedShadowSeed {
                                seed_id,
                                final_digest,
                                generation,
                                expected_seq,
                                total,
                                result,
                            });
                        }
                    }
                    current => seeds.set_phase(&binding.session, current),
                }
                outcome
            }
        }
    }

    fn apply_state_sync_wire(
        &self,
        binding: &SessionBinding,
        store: &McStore,
        parsed: ShadowStateSyncWire,
        lane: StateSyncLane,
    ) -> HandlerOutcome {
        let drop_seeds: Vec<ShadowDropSeedRow> = parsed
            .drop_seeds
            .into_iter()
            .map(|seed| ShadowDropSeedRow {
                block_id: seed.block_id,
                related_block_ids: seed.related_block_ids,
                drop_mode: seed.drop_mode,
                payload: seed.payload,
            })
            .collect();
        let drop_seed_skipped = parsed.drop_seed_skipped;
        let compartments: Vec<StoredCompartment> = parsed
            .compartments
            .into_iter()
            .map(StoredCompartment::from)
            .collect();
        let has_workspace = parsed.workspace.is_some();
        // The route binding supplies the key for the authority transform. Incoming memory
        // rows may contain the plugin's stable project identity, so this mapper translates
        // that identity to the bound key before writing the regular tables.
        let root_path = if lane.is_shadow() {
            shadow_project_path(&binding.session)
        } else {
            binding.project_root.to_string_lossy().to_string()
        };
        let authority_project = if lane.is_shadow() {
            None
        } else {
            match store.authority_project_for_route(&root_path, "memories") {
                Ok(project) => project,
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "authority_project_resolution_failed".to_string(),
                        message: error.to_string(),
                    }
                }
            }
        };
        let store_project_path = authority_project.as_deref().unwrap_or(&root_path);
        let (workspace, member_paths) = match if lane.is_shadow() {
            prepare_shadow_workspace(&binding.session, parsed.workspace)
        } else {
            prepare_authority_workspace(store_project_path, parsed.workspace)
        } {
            Ok(prepared) => prepared,
            Err(error) => return invalid_params_error(error),
        };
        let memories: Vec<ShadowMemoryRow> = match parsed
            .memories
            .into_iter()
            .map(|memory| {
                let project_path = if lane.is_shadow() {
                    shadow_source_path(
                        memory.project_path.as_deref(),
                        &root_path,
                        &member_paths,
                        has_workspace,
                    )?
                } else {
                    authority_source_path(
                        memory.project_path.as_deref(),
                        store_project_path,
                        &member_paths,
                        has_workspace,
                    )?
                };
                Ok(memory.into_row(project_path))
            })
            .collect::<Result<Vec<_>, String>>()
        {
            Ok(memories) => memories,
            Err(error) => return invalid_params_error(error),
        };
        let memory_mutations: Vec<ShadowMemoryMutationRow> = match parsed
            .memory_mutations
            .into_iter()
            .map(|mutation| {
                let project_path = if lane.is_shadow() {
                    shadow_source_path(
                        mutation.project_path.as_deref(),
                        &root_path,
                        &member_paths,
                        has_workspace,
                    )?
                } else {
                    authority_source_path(
                        mutation.project_path.as_deref(),
                        store_project_path,
                        &member_paths,
                        has_workspace,
                    )?
                };
                Ok(mutation.into_row(project_path))
            })
            .collect::<Result<Vec<_>, String>>()
        {
            Ok(mutations) => mutations,
            Err(error) => return invalid_params_error(error),
        };
        let acked_watermarks = parsed.acked_watermarks.unwrap_or_else(|| {
            json!({
                "compartment_seq": compartments.iter().map(|c| c.sequence).max(),
                "memory_id": memories.iter().map(|m| m.id).max(),
                "memory_mutation_id": memory_mutations
                    .iter()
                    .map(|m| m.mutation.id)
                    .max(),
                "last_todo_state": parsed.last_todo_state.is_some(),
            })
        });
        let result = if lane.is_shadow() {
            store.apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: &binding.session,
                shadow_project_path: &root_path,
                shadow_generation: parsed.shadow_generation,
                expected_shadow_seq: parsed.expected_shadow_seq,
                seed_boundary_id: parsed.seed_boundary_id.as_deref(),
                drop_seeds: &drop_seeds,
                drop_seed_skipped,
                compartments: &compartments,
                memories: &memories,
                memory_mutations: &memory_mutations,
                user_profile: &parsed.user_profile,
                workspace: workspace.as_ref(),
                last_todo_state: parsed.last_todo_state,
                acked_watermarks,
            })
        } else {
            store.apply_authority_state_sync(ShadowStateSyncRequest {
                session_id: &binding.session,
                shadow_project_path: &root_path,
                shadow_generation: parsed.shadow_generation,
                expected_shadow_seq: parsed.expected_shadow_seq,
                seed_boundary_id: parsed.seed_boundary_id.as_deref(),
                drop_seeds: &drop_seeds,
                drop_seed_skipped,
                compartments: &compartments,
                memories: &memories,
                memory_mutations: &memory_mutations,
                user_profile: &parsed.user_profile,
                workspace: workspace.as_ref(),
                last_todo_state: parsed.last_todo_state,
                acked_watermarks,
            })
        };
        match result {
            Ok(result) => respond(json!({
                "ok": true,
                "shadow_generation": result.shadow_generation,
                "shadow_seq": result.shadow_seq,
                "row_version": result.row_version,
                "memories_skipped": result.memories_skipped,
                "drop_seeds_skipped": result.drop_seeds_skipped,
            })),
            Err(ShadowStateSyncError::GenerationMismatch { expected, found }) => {
                HandlerOutcome::Error {
                    code: "shadow_generation_mismatch".to_string(),
                    message: format!(
                        "shadow_generation {expected} is stale; current generation is {found}"
                    ),
                }
            }
            Err(ShadowStateSyncError::SeqMismatch { expected, found }) => HandlerOutcome::Error {
                code: "shadow_seq_mismatch".to_string(),
                message: format!(
                    "expected_shadow_seq {expected} did not match current shadow_seq {found}"
                ),
            },
            Err(ShadowStateSyncError::AuthoritySeqMismatch { expected, found }) => {
                state_sync_seq_mismatch_error(StateSyncLane::Authority, expected, found)
            }
            Err(ShadowStateSyncError::InvalidSeedBoundary { declared, detail }) => {
                HandlerOutcome::Error {
                    code: "shadow_seed_boundary_mismatch".to_string(),
                    message: format!("seed boundary {declared:?} rejected: {detail}"),
                }
            }
            Err(error) => HandlerOutcome::Error {
                code: "shadow_state_sync_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    fn handle_shadow_reset_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let parsed: ShadowResetWire = match serde_json::from_value(request) {
            Ok(req) => req,
            Err(error) => return invalid_params_error(error.to_string()),
        };
        let binding = match self.shadow_binding(channel, parsed.session_id.as_deref()) {
            Ok(binding) => binding,
            Err(outcome) => return outcome,
        };
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => return store_unavailable_error(),
        };
        self.discard_transform_pages(&binding.session);
        match store.reset_shadow_session(&binding.session, &shadow_project_path(&binding.session)) {
            Ok(result) => {
                let armed = self
                    .shadow_seeds
                    .lock()
                    .expect("shadow seed mutex")
                    .arm_after_reset(
                        &binding.session,
                        result.shadow_generation,
                        result.shadow_seq,
                    );
                if !armed {
                    return HandlerOutcome::Error {
                        code: "shadow_seed_buffer_overflow".to_string(),
                        message: "too many shadow seed attempts are already pending".to_string(),
                    };
                }
                respond(json!({
                    "ok": true,
                    "shadow_generation": result.shadow_generation,
                    "shadow_seq": result.shadow_seq,
                    "row_version": result.row_version,
                    "previous_shadow_generation": parsed.shadow_generation,
                    "reason": parsed.reason,
                }))
            }
            Err(error) => HandlerOutcome::Error {
                code: "shadow_reset_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    async fn handle_transform_page_value(
        &self,
        channel: u16,
        request: Value,
        lane: TransformLane,
    ) -> HandlerOutcome {
        let present = TRANSFORM_PAGE_FIELDS
            .iter()
            .filter(|field| request.get(**field).is_some())
            .count();
        let session_id = request.get("session_id").and_then(Value::as_str);
        // This accessor intentionally accepts both real and shadow bindings. The lane is
        // checked below, after the route identity is resolved, so authority pages cannot be
        // accidentally forced through shadow-only binding validation.
        let binding = match self.state_sync_binding(channel, session_id) {
            Ok(binding) => binding,
            Err(outcome) => return outcome,
        };
        let bound_lane = if is_shadow_session(&binding.session) {
            TransformLane::Shadow
        } else {
            TransformLane::Authority
        };
        if bound_lane != lane {
            return if lane.is_shadow() {
                HandlerOutcome::Error {
                    code: "shadow_binding_required".to_string(),
                    message: "shadow ops require a route bound as shadow:<real_session>"
                        .to_string(),
                }
            } else {
                HandlerOutcome::Error {
                    code: "plain_transform_on_shadow_binding".to_string(),
                    message: "use shadow_transform for routes bound as shadow:<real_session>"
                        .to_string(),
                }
            };
        }
        if present != TRANSFORM_PAGE_FIELDS.len() {
            self.discard_transform_pages(&binding.session);
            return invalid_params_error("transform page envelope must be all-or-none");
        }
        let transform_id = match request.get("transform_page_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() && id.len() <= SHADOW_TRANSFORM_PAGE_MAX_ID_BYTES => {
                id.to_string()
            }
            _ => {
                self.discard_transform_pages(&binding.session);
                return invalid_params_error(format!(
                    "transform_page_id must contain 1..={SHADOW_TRANSFORM_PAGE_MAX_ID_BYTES} bytes"
                ));
            }
        };
        let Some(generation) = request.get("transform_generation").and_then(Value::as_u64) else {
            self.discard_transform_pages(&binding.session);
            return invalid_params_error("transform_generation must be an unsigned integer");
        };
        let Some(page_index) = request.get("transform_page_index").and_then(Value::as_u64) else {
            self.discard_transform_pages(&binding.session);
            return invalid_params_error("transform_page_index must be an unsigned integer");
        };
        let Some(page_total) = request.get("transform_page_total").and_then(Value::as_u64) else {
            self.discard_transform_pages(&binding.session);
            return invalid_params_error("transform_page_total must be an unsigned integer");
        };
        let Some(page_complete) = request
            .get("transform_page_complete")
            .and_then(Value::as_bool)
        else {
            self.discard_transform_pages(&binding.session);
            return invalid_params_error("transform_page_complete must be a boolean");
        };
        let Some(page_digest) = request.get("transform_page_digest").and_then(Value::as_str) else {
            self.discard_transform_pages(&binding.session);
            return invalid_params_error("transform_page_digest must be a string");
        };
        let page_index = usize::try_from(page_index).unwrap_or(usize::MAX);
        let page_total = usize::try_from(page_total).unwrap_or(usize::MAX);
        if page_total == 0 || page_index >= page_total {
            self.discard_transform_pages(&binding.session);
            return transform_page_error(
                lane,
                "protocol_mismatch",
                "transform page index/total is invalid",
            );
        }
        if page_complete != (page_index + 1 == page_total) {
            self.discard_transform_pages(&binding.session);
            return transform_page_error(
                lane,
                "protocol_mismatch",
                "transform page completion disagrees with the final page index",
            );
        }
        if !page_complete
            && request.as_object().is_some_and(|object| {
                object.keys().any(|key| {
                    !["method", "session_id", "shadow_generation"]
                        .into_iter()
                        .chain(TRANSFORM_PAGE_FIELDS)
                        .chain(TRANSFORM_PAGE_ARRAY_FIELDS)
                        .any(|allowed| allowed == key.as_str())
                })
            })
        {
            self.discard_transform_pages(&binding.session);
            return transform_page_error(
                lane,
                "protocol_mismatch",
                "non-final transform pages may carry only message arrays",
            );
        }
        if transform_page_content_digest(&request) != page_digest {
            self.discard_transform_pages(&binding.session);
            return transform_page_error(
                lane,
                "digest_mismatch",
                "transform page content digest did not match the supplied digest",
            );
        }
        let page_bytes = match serde_json::to_vec(&request) {
            Ok(bytes) => bytes.len(),
            Err(error) => {
                self.discard_transform_pages(&binding.session);
                return invalid_params_error(error.to_string());
            }
        };
        if page_bytes > SHADOW_TRANSFORM_PAGE_MAX_BYTES {
            self.discard_transform_pages(&binding.session);
            return transform_page_error(
                lane,
                "buffer_overflow",
                "transform page exceeded the 512 KiB page cap",
            );
        }
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => return store_unavailable_error(),
        };
        if lane.is_shadow() {
            if request.get("shadow_generation").and_then(Value::as_u64) != Some(generation) {
                self.discard_transform_pages(&binding.session);
                return transform_page_error(
                    lane,
                    "attempt_mismatch",
                    "shadow_generation must match transform_generation",
                );
            }
            if page_index == 0 {
                let loaded = match store.load(&binding.session) {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        self.discard_transform_pages(&binding.session);
                        return HandlerOutcome::Error {
                            code: "store_load_failed".to_string(),
                            message: error.to_string(),
                        };
                    }
                };
                if loaded.meta.shadow_generation != generation
                    || loaded.meta.shadow_generation
                        != request
                            .get("shadow_generation")
                            .and_then(Value::as_u64)
                            .unwrap_or_default()
                {
                    self.discard_transform_pages(&binding.session);
                    return transform_page_error(
                        lane,
                        "attempt_mismatch",
                        "transform page generation did not match durable shadow generation",
                    );
                }
            }
        }
        {
            let transforms = self.transform_pages.lock().expect("transform page mutex");
            if let Some(completed) = transforms.completed(&binding.session, &transform_id) {
                if completed.generation == generation
                    && page_complete
                    && completed.final_digest == page_digest
                {
                    return HandlerOutcome::Response(completed.result.clone());
                }
                return transform_page_error(
                    lane,
                    "digest_mismatch",
                    "completed transform page content changed",
                );
            }
        }
        let action = {
            let mut transforms = self.transform_pages.lock().expect("transform page mutex");
            match transforms.stage(
                &binding.session,
                transform_id,
                generation,
                page_index,
                page_total,
                page_digest.to_string(),
                request,
                page_bytes,
                page_complete,
            ) {
                Ok(action) => action,
                Err(error) => {
                    let (suffix, message) = match error {
                        TransformPageStageError::AttemptMismatch => (
                            "attempt_mismatch",
                            "transform page generation or envelope changed during collection",
                        ),
                        TransformPageStageError::DigestMismatch => {
                            ("digest_mismatch", "redriven transform page content changed")
                        }
                        TransformPageStageError::OrderMismatch => (
                            "order_mismatch",
                            "transform pages must arrive in strict index order",
                        ),
                        TransformPageStageError::BufferOverflow => (
                            "buffer_overflow",
                            "transform page staging exceeded the handler-wide byte cap",
                        ),
                        TransformPageStageError::InProgress => {
                            ("in_progress", "the final transform page is being applied")
                        }
                    };
                    return transform_page_error(lane, suffix, message);
                }
            }
        };
        match action {
            TransformPageStageAction::Ack(next_expected_index) => respond(json!({
                "ok": true,
                "staged": true,
                "next_expected_index": next_expected_index,
            })),
            TransformPageStageAction::Apply {
                pages,
                transform_id,
                generation,
                final_digest,
            } => {
                let assembled = match assemble_transform_pages(pages) {
                    Ok(assembled) => assembled,
                    Err(message) => {
                        self.discard_transform_pages(&binding.session);
                        return transform_page_error(lane, "protocol_mismatch", message);
                    }
                };
                let outcome = match lane {
                    TransformLane::Authority => {
                        self.handle_transform_unpaged_value(channel, assembled, true)
                            .await
                    }
                    TransformLane::Shadow => {
                        self.handle_shadow_transform_unpaged_value(channel, assembled, true)
                            .await
                    }
                };
                let completed_result = match &outcome {
                    HandlerOutcome::Response(bytes) => Some(bytes.clone()),
                    HandlerOutcome::Error { .. } | HandlerOutcome::Streamed => None,
                };
                let mut transforms = self.transform_pages.lock().expect("transform page mutex");
                let phase = {
                    let session = transforms
                        .sessions
                        .entry(binding.session.clone())
                        .or_default();
                    std::mem::replace(&mut session.phase, TransformPagePhase::Idle)
                };
                match phase {
                    TransformPagePhase::Applying {
                        transform_id: applying_id,
                        bytes,
                    } if applying_id == transform_id => {
                        transforms.release_phase(&TransformPagePhase::Applying {
                            transform_id: applying_id,
                            bytes,
                        });
                        if let Some(result) = completed_result {
                            transforms
                                .sessions
                                .entry(binding.session.clone())
                                .or_default()
                                .completed = Some(CompletedTransformPage {
                                transform_id,
                                generation,
                                final_digest,
                                result,
                            });
                        }
                    }
                    current => transforms.set_phase(&binding.session, current),
                }
                outcome
            }
        }
    }

    async fn handle_shadow_transform_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        const PAGE_FIELDS: [&str; 6] = [
            "transform_page_id",
            "transform_generation",
            "transform_page_index",
            "transform_page_total",
            "transform_page_complete",
            "transform_page_digest",
        ];
        if PAGE_FIELDS
            .iter()
            .any(|field| request.get(*field).is_some())
        {
            return self
                .handle_transform_page_value(channel, request, TransformLane::Shadow)
                .await;
        }
        self.handle_shadow_transform_unpaged_value(channel, request, false)
            .await
    }

    async fn handle_shadow_transform_unpaged_value(
        &self,
        channel: u16,
        request: Value,
        from_page_apply: bool,
    ) -> HandlerOutcome {
        let parsed: ShadowTransformWire = match serde_json::from_value(request.clone()) {
            Ok(req) => req,
            Err(e) => return invalid_params_error(e.to_string()),
        };
        let binding = match self.shadow_binding(channel, parsed.session_id.as_deref()) {
            Ok(binding) => binding,
            Err(outcome) => return outcome,
        };
        if !from_page_apply && self.transform_page_in_progress(&binding.session) {
            return HandlerOutcome::Error {
                code: "transform_page_in_progress".to_string(),
                message: "shadow_transform is blocked until all transform pages arrive".to_string(),
            };
        }
        if self.shadow_seed_in_progress(&binding.session) {
            return HandlerOutcome::Error {
                code: "shadow_seed_in_progress".to_string(),
                message: "shadow_transform is blocked until the seed commits or is discarded"
                    .to_string(),
            };
        }
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => return store_unavailable_error(),
        };
        let loaded = match store.load(&binding.session) {
            Ok(loaded) => loaded,
            Err(e) => {
                return HandlerOutcome::Error {
                    code: "store_load_failed".to_string(),
                    message: e.to_string(),
                }
            }
        };
        if loaded.meta.shadow_generation != parsed.shadow_generation {
            return HandlerOutcome::Error {
                code: "shadow_generation_mismatch".to_string(),
                message: format!(
                    "shadow_generation {} is stale; current generation is {}",
                    parsed.shadow_generation, loaded.meta.shadow_generation
                ),
            };
        }
        let pass_seq = parsed.pass_seq.unwrap_or(loaded.meta.shadow_seq);
        if loaded.meta.shadow_quarantined {
            let state_hash = shadow_state_hash(&store, &binding.session).unwrap_or_default();
            let rs_decision = json!({ "class": "quarantined", "byte_compare": false });
            let report = self.record_shadow_report(
                &store,
                &binding.session,
                ShadowReportInput {
                    shadow_generation: parsed.shadow_generation,
                    pass_seq,
                    outcome: CompareOutcome {
                        class: "quarantined".to_string(),
                        hard: false,
                        compared: false,
                        first_mid: None,
                        first_block: None,
                        first_field: None,
                        ts_prefix: String::new(),
                        rs_prefix: String::new(),
                        first_diff_offset: None,
                        ts_window: String::new(),
                        rs_window: String::new(),
                    },
                    normalizations: parsed.normalizations,
                    ts_decision: parsed.ts_decision,
                    rs_decision,
                    state_hash,
                    replay: None,
                },
            );
            return report;
        }

        let shadow_input = match shadow_input_messages(&parsed) {
            Ok(messages) => messages,
            Err(message) => {
                return HandlerOutcome::Error {
                    code: "bad_shadow_input".to_string(),
                    message,
                }
            }
        };
        let serializer_profile = parsed
            .serializer_profile
            .clone()
            .unwrap_or_else(|| SerializerProfile::OpencodeAiSdk.wire_id().to_string());
        if SerializerProfile::parse(&serializer_profile).is_none() {
            return unknown_serializer_profile_error();
        }
        let usage = parsed
            .pass_inputs
            .usage
            .as_ref()
            .map(|usage| mc_store::ModuleUsage {
                current_total_input_tokens: usage.input_tokens,
                context_limit_tokens: usage.limit,
            });
        let transform_request = TransformRequest {
            kind: "transform".to_string(),
            v: 2,
            serializer_profile,
            session_id: binding.session.clone(),
            render_config: parsed.render_config.clone().unwrap_or_default(),
            provider_id: parsed.pass_inputs.provider_id.clone(),
            model_key: parsed.pass_inputs.model_key.clone(),
            clear_reasoning_age: parsed.pass_inputs.clear_reasoning_age,
            tool_present: false,
            serve_native: false,
            native_messages: None,
            full_array_fingerprint: parsed.full_array_fingerprint.clone(),
            messages: shadow_input,
            tail_delta: None,
            usage,
            provider_error: parsed.pass_inputs.provider_error.clone(),
            mid_turn: parsed.pass_inputs.mid_turn,
            prev_response_completed_at_ms: None,
            request_observed_at_ms: None,
            history_budget_tokens: None,
            declared_trim: parsed.declared_trim.clone(),
        };
        let shadow_project = shadow_project_path(&binding.session);
        let project_path = binding.project_root.to_string_lossy().to_string();
        let producer_ctx = transform::ProducerContext {
            project_path: &shadow_project,
            note_project_path: &shadow_project,
            project_directory: &project_path,
            history_budget_tokens: parsed
                .pass_inputs
                .history_budget_tokens
                .unwrap_or(binding.history_budget_tokens),
            memory_enabled: binding.config.memory_enabled,
            now_ms: parsed.pass_inputs.now_ms,
            execute_threshold_percentage: parsed.pass_inputs.effective_execute_threshold,
            smart_drops: binding.config.smart_drops,
            cache_ttl: parsed.pass_inputs.cache_ttl.clone(),
            model_key: parsed.pass_inputs.model_key.clone(),
            observed_last_response_at_ms: None,
            guidance_date: Some(self.guidance_date_line_for_ms(parsed.pass_inputs.now_ms)),
            #[cfg(test)]
            injected_reductions: self
                .reduction_injection
                .lock()
                .expect("reduction injection mutex")
                .remove(&binding.session)
                .unwrap_or_default(),
        };
        let result = match transform_with_projection(&store, &transform_request, &producer_ctx) {
            Ok(result) => result,
            Err(transform::TransformError::IdentityDrift(mid)) => {
                return HandlerOutcome::Error {
                    code: "shadow_identity_drift".to_string(),
                    message: format!("CK message block identity drift for mid {mid}"),
                }
            }
            Err(e) if e.is_deterministic_reject() => {
                return HandlerOutcome::Error {
                    code: "shadow_validation_reject".to_string(),
                    message: e.to_string(),
                }
            }
            Err(e) => {
                return HandlerOutcome::Error {
                    code: "shadow_transform_failed".to_string(),
                    message: e.to_string(),
                }
            }
        };
        let historian = self.evaluate_shadow_historian(
            &store,
            &transform_request,
            &result.projection,
            &parsed.pass_inputs,
        );
        let mut rs_decision = json!({
            "class": rs_decision_class(&result.response.action),
            "action": result.response.action,
            "scheduler_pass": format!("{:?}", result.scheduler_pass),
            "boundary_id": result.response.boundary_id,
            "coverage_ordinal": result.response.coverage_ordinal,
            "boundary_state": format!("{:?}", result.boundary_state),
            "historian": historian,
        });
        if let Some(trim) = &result.trim_mismatch {
            rs_decision["trim_mismatch"] = serde_json::to_value(trim).unwrap_or(Value::Null);
        }
        if parsed.seed_pass && !loaded.meta.initialized {
            // A fresh shadow lineage must take its own first-render path even when the
            // source lane is already warm. Committing that pass calibrates durable cache
            // and boundary state; initialized state makes every later pass compare even
            // if a sender repeats the seed flag.
            let state_hash = shadow_state_hash(&store, &binding.session).unwrap_or_default();
            return self.record_shadow_report(
                &store,
                &binding.session,
                ShadowReportInput {
                    shadow_generation: parsed.shadow_generation,
                    pass_seq,
                    outcome: CompareOutcome {
                        class: "identical".to_string(),
                        hard: false,
                        compared: false,
                        first_mid: None,
                        first_block: None,
                        first_field: None,
                        ts_prefix: String::new(),
                        rs_prefix: String::new(),
                        first_diff_offset: None,
                        ts_window: String::new(),
                        rs_window: String::new(),
                    },
                    normalizations: parsed.normalizations,
                    ts_decision: parsed.ts_decision,
                    rs_decision,
                    state_hash,
                    replay: None,
                },
            );
        }
        let ts_messages = match shadow_ts_messages(&parsed) {
            Ok(messages) => messages,
            Err(message) => {
                return HandlerOutcome::Error {
                    code: "bad_shadow_ts_output".to_string(),
                    message,
                }
            }
        };
        let rs_messages = result.response.ck_messages.clone().unwrap_or_default();
        let state_hash = shadow_state_hash(&store, &binding.session).unwrap_or_default();
        let compare = compare_shadow_outputs(
            &ts_messages,
            &rs_messages,
            &parsed.ts_decision,
            &rs_decision,
            &parsed.normalizations,
            result.trim_mismatch.as_ref(),
        );
        let replay = compare.hard.then(|| {
            json!({
                "input": request.get("input").cloned().unwrap_or(Value::Null),
                "pass_inputs": request.get("pass_inputs").cloned().unwrap_or(Value::Null),
                "declared_trim": request.get("declared_trim").cloned().unwrap_or(Value::Null),
                "ts_output": request.get("ts_output").cloned().unwrap_or(Value::Null),
                "rs_output": rs_messages,
            })
        });
        self.record_shadow_report(
            &store,
            &binding.session,
            ShadowReportInput {
                shadow_generation: parsed.shadow_generation,
                pass_seq,
                outcome: compare,
                normalizations: parsed.normalizations,
                ts_decision: parsed.ts_decision,
                rs_decision,
                state_hash,
                replay,
            },
        )
    }

    fn register_dreamer_run(&self, session_id: &str) -> DreamerRunGuard {
        self.active_dreamer_runs
            .lock()
            .expect("dreamer registry mutex")
            .insert(session_id.to_string());
        DreamerRunGuard {
            registry: Arc::clone(&self.active_dreamer_runs),
            session_id: session_id.to_string(),
        }
    }

    fn unregister_dreamer_run(&self, session_id: &str) {
        self.active_dreamer_runs
            .lock()
            .expect("dreamer registry mutex")
            .remove(session_id);
    }

    fn dreamer_run_registered(&self, session_id: &str) -> bool {
        self.active_dreamer_runs
            .lock()
            .expect("dreamer registry mutex")
            .contains(session_id)
    }

    async fn handle_dreamer_run_task(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let (ledger_session, binding) =
            match self.management_binding(channel, request, "dreamer.run_task") {
                Ok(value) => value,
                Err(outcome) => return outcome,
            };
        let Some(store) = self.store.get().cloned() else {
            return store_unavailable_error();
        };
        let Some(task) = request.get("task").and_then(Value::as_str) else {
            return invalid_params_error("dreamer.run_task requires task");
        };
        if task != CLASSIFY_TASK {
            // Enumerating the task here is a capability boundary: callers cannot use this
            // route to select an arbitrary system prompt, model, or tool-enabled run.
            return invalid_params_error(format!("unknown dreamer task {task:?}"));
        }
        let Some(command_id) = request.get("command_id").and_then(Value::as_str) else {
            return invalid_params_error("dreamer.run_task requires command_id");
        };
        if command_id.trim().is_empty() || command_id.len() > 256 {
            return invalid_params_error("dreamer.run_task command_id must be 1-256 bytes");
        }
        let Some(authority_generation) =
            request.get("authority_generation").and_then(Value::as_u64)
        else {
            return invalid_params_error("dreamer.run_task requires authority_generation");
        };
        let route_root = binding.project_root.to_string_lossy().to_string();
        let Some(project) = (match store.authority_project_for_route(&route_root, "memories") {
            Ok(project) => project,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "authority_lookup_failed".to_string(),
                    message: error.to_string(),
                }
            }
        }) else {
            return HandlerOutcome::Error {
                code: "authority_not_module".to_string(),
                message: "memories authority for this route is not MODULE".to_string(),
            };
        };
        let Some((context_store_uuid, authority_project)) =
            (match store.module_authority_for_project(&project, "memories") {
                Ok(authority) => authority,
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "authority_lookup_failed".to_string(),
                        message: error.to_string(),
                    }
                }
            })
        else {
            return HandlerOutcome::Error {
                code: "authority_not_module".to_string(),
                message: "memories authority for this route is not MODULE".to_string(),
            };
        };
        let authority =
            match store.authority_status(&context_store_uuid, &authority_project, "memories") {
                Ok(Some(authority)) => authority,
                Ok(None) => {
                    return HandlerOutcome::Error {
                        code: "authority_not_module".to_string(),
                        message: "memories authority row is missing".to_string(),
                    }
                }
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "authority_lookup_failed".to_string(),
                        message: error.to_string(),
                    }
                }
            };
        if authority.state != "MODULE" {
            return HandlerOutcome::Error {
                code: "authority_not_module".to_string(),
                message: format!("memories authority is {}", authority.state),
            };
        }
        if authority.generation != authority_generation {
            return HandlerOutcome::Error {
                code: "authority_generation_mismatch".to_string(),
                message: format!(
                    "authority generation is {}, request used {authority_generation}",
                    authority.generation
                ),
            };
        }
        let Some(payload) = request.get("payload").and_then(Value::as_object) else {
            return invalid_params_error("dreamer.run_task requires an object payload");
        };
        let Some(prompt_body) = payload.get("prompt_body").and_then(Value::as_str) else {
            return invalid_params_error("classify payload requires prompt_body");
        };
        if prompt_body.len() > MAX_CLASSIFY_PROMPT_BYTES {
            return HandlerOutcome::Error {
                code: "payload_too_large".to_string(),
                message: format!("classify prompt_body exceeds {MAX_CLASSIFY_PROMPT_BYTES} bytes"),
            };
        }
        if payload.get("items").and_then(Value::as_array).is_none() {
            return invalid_params_error("classify payload requires items");
        }

        if let Ok(Some(recorded)) = store.load_dream_task_command(&ledger_session, command_id) {
            return replay_dream_task_response(&recorded.response_json);
        }

        let child_session = child_session_id(&authority_project, command_id);
        let _dreamer_run_guard = self.register_dreamer_run(&child_session);
        let mut attempts = 0usize;
        let mut last_error = String::new();
        let mut output = None;
        for model in &binding.config.model_chain {
            attempts += 1;
            let mut producer = match self.producer_factory.connect(&binding.project_root).await {
                Ok(producer) => producer,
                Err(error) => {
                    last_error = error.to_string();
                    continue;
                }
            };
            let started = producer
                .start_with_generation(
                    &child_session,
                    CLASSIFY_SYSTEM_PROMPT,
                    prompt_body,
                    model,
                    CLASSIFY_MAX_OUTPUT_TOKENS,
                    CLASSIFY_TEMPERATURE,
                )
                .await;
            let attempt_output = match started {
                Ok(handle) => match producer
                    .await_output_with_timeout(&handle.run_id, CLASSIFY_AWAIT_TIMEOUT)
                    .await
                {
                    Ok(result) => Ok(result),
                    Err(HistorianProducerError::TimedOut) => {
                        producer
                            .redrain_output_with_timeout(&handle.run_id, CLASSIFY_RECOVERY_TIMEOUT)
                            .await
                    }
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            match attempt_output {
                Ok(result) => {
                    // The module never parses manifests. A capped result is still returned
                    // with truncated=true so the host's fail-closed parser remains the
                    // authority for output validity.
                    output = Some((model.clone(), result));
                    producer.purge_session(&child_session).await;
                    break;
                }
                Err(error) => last_error = error.to_string(),
            }
            producer.purge_session(&child_session).await;
        }
        if output.is_none() {
            let response = json!({
                "ok": false,
                "code": "dreamer_run_failed",
                "message": if last_error.is_empty() { "classify producer has no usable model" } else { &last_error },
            });
            let _ = store.record_dream_task_command(
                &ledger_session,
                command_id,
                &response.to_string(),
                now_ms(),
            );
            return HandlerOutcome::Error {
                code: "dreamer_run_failed".to_string(),
                message: last_error,
            };
        }
        let (model, result) = output.expect("classifier output set");
        let response = json!({
            "ok": true,
            "manifest_text": result.text,
            "truncated": result.length_capped,
            "diagnostics": {
                "task": CLASSIFY_TASK,
                "model": model,
                "attempts": attempts,
                "child_session_id": child_session,
                "temperature": CLASSIFY_TEMPERATURE,
                "max_output_tokens": CLASSIFY_MAX_OUTPUT_TOKENS,
                "await_timeout_ms": CLASSIFY_AWAIT_TIMEOUT.as_millis(),
                "recovery_timeout_ms": CLASSIFY_RECOVERY_TIMEOUT.as_millis(),
            }
        });
        match store.record_dream_task_command(
            &ledger_session,
            command_id,
            &response.to_string(),
            now_ms(),
        ) {
            Ok(recorded) => replay_dream_task_response(&recorded.response_json),
            Err(error) => HandlerOutcome::Error {
                code: "dreamer_ledger_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    async fn handle_memory_set_classification(
        &self,
        channel: u16,
        request: &Value,
    ) -> HandlerOutcome {
        let Some(args) = facade_arguments(request, &["rows"]) else {
            return invalid_params_error("memory.set_classification requires arguments");
        };
        let Some(memory_project) = non_empty_string_arg(&args, "memory_project") else {
            return invalid_params_error("memory.set_classification requires memory_project");
        };
        if let Err(outcome) = self.bind_facade_route_for_write(channel, &args, "memories") {
            return outcome;
        }
        let binding = match self.facade_binding(channel) {
            Ok(binding) => binding,
            Err(_) => return session_unresolved_error(),
        };
        let route_root = binding.project_root.to_string_lossy().to_string();
        let Some(store) = self.store.get() else {
            return store_unavailable_error();
        };
        let authority_project =
            match store.authority_project_state_for_route(&route_root, "memories") {
                Ok(Some((project, state))) if state == "MODULE" => project,
                Ok(Some(_)) => return authority_draining_error("memories"),
                Ok(None) => {
                    return HandlerOutcome::Error {
                        code: "authority_not_module".to_string(),
                        message: "classification requires MODULE memories authority".to_string(),
                    }
                }
                Err(error) => {
                    return HandlerOutcome::Error {
                        code: "authority_lookup_failed".to_string(),
                        message: error.to_string(),
                    }
                }
            };
        if authority_project != memory_project {
            return HandlerOutcome::Error {
                code: "facade_project_vocabulary_mismatch".to_string(),
                message: format!(
                    "classification route is owned by {authority_project}, not {memory_project}"
                ),
            };
        }
        let Some(context_store_uuid) = args.get("context_store_uuid").and_then(Value::as_str)
        else {
            return invalid_params_error("memory.set_classification requires context_store_uuid");
        };
        let Some(authority_generation) = args.get("authority_generation").and_then(Value::as_u64)
        else {
            return invalid_params_error("memory.set_classification requires authority_generation");
        };
        let Some(rows) = args.get("rows").and_then(Value::as_array) else {
            return invalid_params_error("memory.set_classification requires rows");
        };
        let mut updates = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(row) = row.as_object() else {
                return invalid_params_error("classification rows must be objects");
            };
            let Some(memory_id) = row.get("memory_id").and_then(Value::as_i64) else {
                return invalid_params_error("classification row requires memory_id");
            };
            let Some(hash) = row.get("content_hash_at_prompt").and_then(Value::as_str) else {
                return invalid_params_error("classification row requires content_hash_at_prompt");
            };
            updates.push(mc_store::ClassificationUpdate {
                memory_id,
                content_hash_at_prompt: hash.to_string(),
                importance: row
                    .get("importance")
                    .and_then(Value::as_i64)
                    .map(|value| value as i32),
                scope: row
                    .get("scope")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                shareable: row.get("shareable").and_then(Value::as_bool),
            });
        }
        #[cfg(test)]
        if let Some(hook) = self
            .classification_before_apply_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            hook();
        }
        match store.with_facade_mutation(&route_root, "memories", || {
            store.set_memory_classification(
                context_store_uuid,
                memory_project,
                authority_generation,
                &updates,
                now_ms(),
            )
        }) {
            Ok(result) => respond(json!({
                "accepted": result.accepted,
                "rejected": result.rejected.iter().map(|row| json!({ "memory_id": row.memory_id, "reason": row.reason })).collect::<Vec<_>>(),
            })),
            Err(McStoreError::AuthorityGenerationMismatch { expected, found }) => {
                HandlerOutcome::Error {
                    code: "authority_generation_mismatch".to_string(),
                    message: format!("authority generation is {found}, request used {expected}"),
                }
            }
            Err(McStoreError::AuthorityStateMismatch { found, .. }) if found == "DRAINING" => {
                authority_draining_error("memories")
            }
            Err(McStoreError::AuthorityStateMismatch { expected, found }) => {
                HandlerOutcome::Error {
                    code: "authority_state_mismatch".to_string(),
                    message: format!("authority state is {found}, expected {expected}"),
                }
            }
            Err(error) if store_error_is_authority_draining(&error) => {
                authority_draining_error("memories")
            }
            Err(error) => HandlerOutcome::Error {
                code: "classification_apply_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    async fn handle_memory_set_verification(
        &self,
        channel: u16,
        request: &Value,
    ) -> HandlerOutcome {
        let Some(args) = facade_arguments(request, &["rows"]) else {
            return invalid_params_error("memory.set_verification requires arguments");
        };
        let Some(memory_project) = non_empty_string_arg(&args, "memory_project") else {
            return invalid_params_error("memory.set_verification requires memory_project");
        };
        if let Err(outcome) = self.bind_facade_route_for_write(channel, &args, "memories") {
            return outcome;
        }
        let binding = match self.facade_binding(channel) {
            Ok(binding) => binding,
            Err(_) => return session_unresolved_error(),
        };
        let route_root = binding.project_root.to_string_lossy().to_string();
        let Some(store) = self.store.get() else {
            return store_unavailable_error();
        };
        let command_id = args
            .get("command_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        if let Some(command_id) = command_id {
            if let Ok(Some(recorded)) = store.load_dream_task_command(&binding.session, command_id)
            {
                return replay_dream_task_response(&recorded.response_json);
            }
        }
        let Some(context_store_uuid) = args.get("context_store_uuid").and_then(Value::as_str)
        else {
            return invalid_params_error("memory.set_verification requires context_store_uuid");
        };
        let Some(authority_generation) = args.get("authority_generation").and_then(Value::as_u64)
        else {
            return invalid_params_error("memory.set_verification requires authority_generation");
        };
        let Some(rows) = args.get("rows").and_then(Value::as_array) else {
            return invalid_params_error("memory.set_verification requires rows");
        };
        let mut updates = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(row) = row.as_object() else {
                return invalid_params_error("verification rows must be objects");
            };
            let Some(memory_id) = row.get("memory_id").and_then(Value::as_i64) else {
                return invalid_params_error("verification row requires memory_id");
            };
            let Some(hash) = row.get("content_hash_at_prompt").and_then(Value::as_str) else {
                return invalid_params_error("verification row requires content_hash_at_prompt");
            };
            let Some(status) = row.get("verification_status").and_then(Value::as_str) else {
                return invalid_params_error("verification row requires verification_status");
            };
            updates.push(VerificationUpdate {
                memory_id,
                content_hash_at_prompt: hash.into(),
                verification_status: status.into(),
                updated_content: row
                    .get("updated_content")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                archive_reason: row
                    .get("archive_reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
        match store.with_facade_mutation(&route_root, "memories", || {
            store.set_memory_verification(
                context_store_uuid,
                memory_project,
                authority_generation,
                &updates,
                now_ms(),
            )
        }) {
            Ok(result) => {
                let response = json!({ "ok": true, "accepted": result.accepted, "rejected": result.rejected.iter().map(|row| json!({"memory_id": row.memory_id, "reason": row.reason})).collect::<Vec<_>>() });
                if let Some(command_id) = command_id {
                    match store.record_dream_task_command(
                        &binding.session,
                        command_id,
                        &response.to_string(),
                        now_ms(),
                    ) {
                        Ok(recorded) => replay_dream_task_response(&recorded.response_json),
                        Err(error) => HandlerOutcome::Error {
                            code: "dreamer_ledger_failed".into(),
                            message: error.to_string(),
                        },
                    }
                } else {
                    respond(response)
                }
            }
            Err(McStoreError::AuthorityGenerationMismatch { expected, found }) => {
                HandlerOutcome::Error {
                    code: "authority_generation_mismatch".into(),
                    message: format!("authority generation is {found}, request used {expected}"),
                }
            }
            Err(McStoreError::AuthorityStateMismatch { found, .. }) if found == "DRAINING" => {
                authority_draining_error("memories")
            }
            Err(error) => HandlerOutcome::Error {
                code: "verification_apply_failed".into(),
                message: error.to_string(),
            },
        }
    }

    async fn handle_memory_set_mapping(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(args) = facade_arguments(request, &["rows"]) else {
            return invalid_params_error("memory.set_mapping requires arguments");
        };
        let Some(memory_project) = non_empty_string_arg(&args, "memory_project") else {
            return invalid_params_error("memory.set_mapping requires memory_project");
        };
        if let Err(outcome) = self.bind_facade_route_for_write(channel, &args, "memories") {
            return outcome;
        }
        let binding = match self.facade_binding(channel) {
            Ok(binding) => binding,
            Err(_) => return session_unresolved_error(),
        };
        let route_root = binding.project_root.to_string_lossy().to_string();
        let Some(store) = self.store.get() else {
            return store_unavailable_error();
        };
        let command_id = args
            .get("command_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        if let Some(command_id) = command_id {
            if let Ok(Some(recorded)) = store.load_dream_task_command(&binding.session, command_id)
            {
                return replay_dream_task_response(&recorded.response_json);
            }
        }
        let Some(context_store_uuid) = args.get("context_store_uuid").and_then(Value::as_str)
        else {
            return invalid_params_error("memory.set_mapping requires context_store_uuid");
        };
        let Some(authority_generation) = args.get("authority_generation").and_then(Value::as_u64)
        else {
            return invalid_params_error("memory.set_mapping requires authority_generation");
        };
        let Some(rows) = args.get("rows").and_then(Value::as_array) else {
            return invalid_params_error("memory.set_mapping requires rows");
        };
        let mut updates = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(row) = row.as_object() else {
                return invalid_params_error("mapping rows must be objects");
            };
            let Some(memory_id) = row.get("memory_id").and_then(Value::as_i64) else {
                return invalid_params_error("mapping row requires memory_id");
            };
            let Some(hash) = row.get("content_hash_at_prompt").and_then(Value::as_str) else {
                return invalid_params_error("mapping row requires content_hash_at_prompt");
            };
            let mapped_files = match row.get("mapped_files") {
                Some(Value::Null) | None => None,
                Some(Value::Array(files)) => Some(
                    files
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                ),
                _ => return invalid_params_error("mapped_files must be an array or null"),
            };
            updates.push(MappingUpdate {
                memory_id,
                content_hash_at_prompt: hash.into(),
                mapped_files,
            });
        }
        match store.with_facade_mutation(&route_root, "memories", || {
            store.set_memory_mapping(
                context_store_uuid,
                memory_project,
                authority_generation,
                &updates,
                now_ms(),
            )
        }) {
            Ok(result) => {
                let response = json!({ "ok": true, "accepted": result.accepted, "rejected": result.rejected.iter().map(|row| json!({"memory_id": row.memory_id, "reason": row.reason})).collect::<Vec<_>>() });
                if let Some(command_id) = command_id {
                    match store.record_dream_task_command(
                        &binding.session,
                        command_id,
                        &response.to_string(),
                        now_ms(),
                    ) {
                        Ok(recorded) => replay_dream_task_response(&recorded.response_json),
                        Err(error) => HandlerOutcome::Error {
                            code: "dreamer_ledger_failed".into(),
                            message: error.to_string(),
                        },
                    }
                } else {
                    respond(response)
                }
            }
            Err(McStoreError::AuthorityGenerationMismatch { expected, found }) => {
                HandlerOutcome::Error {
                    code: "authority_generation_mismatch".into(),
                    message: format!("authority generation is {found}, request used {expected}"),
                }
            }
            Err(McStoreError::AuthorityStateMismatch { found, .. }) if found == "DRAINING" => {
                authority_draining_error("memories")
            }
            Err(error) => HandlerOutcome::Error {
                code: "mapping_apply_failed".into(),
                message: error.to_string(),
            },
        }
    }

    async fn handle_facade_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let Some(name) = request.get("name").and_then(Value::as_str) else {
            return unrecognized_request_error(&request);
        };
        match name {
            "memory.set_classification" => {
                self.handle_memory_set_classification(channel, &request)
                    .await
            }
            "memory.set_verification" => {
                self.handle_memory_set_verification(channel, &request).await
            }
            "memory.set_mapping" => self.handle_memory_set_mapping(channel, &request).await,
            "ctx_memory" => self.handle_ctx_memory_facade(channel, &request).await,
            "ctx_search" => self.handle_ctx_search_facade(channel, &request).await,
            "ctx_expand" => self.handle_ctx_expand_facade(channel, &request).await,
            "ctx_reduce" => self.handle_ctx_reduce_facade(channel, &request).await,
            "ctx_note" => self.handle_ctx_note_facade(channel, &request).await,
            _ => unrecognized_request_error(&request),
        }
    }

    fn bind_facade_route_for_write(
        &self,
        channel: u16,
        arguments: &Map<String, Value>,
        authority_domain: &str,
    ) -> Result<(), HandlerOutcome> {
        let Some(requested_project) = non_empty_string_arg(arguments, "memory_project") else {
            return Ok(());
        };
        let binding = self
            .facade_binding(channel)
            .map_err(|_| session_unresolved_error())?;
        let route_project_root = binding.project_root.to_string_lossy().to_string();
        let Some(store) = self.store.get() else {
            return Err(store_unavailable_error());
        };
        let authority = store
            .facade_authority_for_project(requested_project, authority_domain)
            .map_err(|error| HandlerOutcome::Error {
                code: "authority_route_lookup_failed".to_string(),
                message: error.to_string(),
            })?;
        if let Some((context_store_uuid, project, state)) = authority {
            if state != "MODULE" {
                return Err(authority_draining_error(authority_domain));
            }
            store
                .bind_authority_route(&context_store_uuid, &project, &route_project_root)
                .map_err(|error| HandlerOutcome::Error {
                    code: "authority_route_bind_failed".to_string(),
                    message: error.to_string(),
                })?;
        }
        Ok(())
    }

    async fn resolve_facade_scope(
        &self,
        channel: u16,
        arguments: Option<&Map<String, Value>>,
        authority_domain: &str,
        bind_authority_for_write: bool,
    ) -> Result<FacadeScope, HandlerOutcome> {
        let binding = self
            .facade_binding(channel)
            .map_err(|_| session_unresolved_error())?;
        let bound_session = binding.session.trim();
        if bound_session.is_empty() {
            return Err(session_unresolved_error());
        }

        // Harness labels are client-supplied routing hints, not authentication. OpenCode may
        // bypass session.resolve only after server-observed cache state or a live transform
        // route proves that this exact session belongs to the module; wrapper token namespaces
        // cannot satisfy that provenance check.
        let conversation_key = if binding.harness == OPENCODE_HARNESS
            && !is_shadow_session(bound_session)
            && self.module_knows_transform_session(bound_session, &binding.project_root)
        {
            bound_session.to_string()
        } else {
            match self
                .session_resolver
                .resolve_session(&binding.project_root, &binding.harness, bound_session)
                .await
            {
                Ok(Some(resolved)) => resolved.session_id,
                Ok(None) => return Err(session_unresolved_error()),
                Err(SessionResolveError::Timeout) => {
                    return Err(HandlerOutcome::Error {
                        code: "session_resolve_timeout".to_string(),
                        message: "session.resolve timed out after 2s".to_string(),
                    })
                }
                Err(error) => {
                    return Err(HandlerOutcome::Error {
                        code: "session_resolve_failed".to_string(),
                        message: error.to_string(),
                    })
                }
            }
        };

        let route_project_root = binding.project_root.to_string_lossy().to_string();
        if bind_authority_for_write {
            if let Some(arguments) = arguments {
                self.bind_facade_route_for_write(channel, arguments, authority_domain)?;
            }
        }
        let requested_project =
            arguments.and_then(|arguments| non_empty_string_arg(arguments, "memory_project"));
        let memory_project_path = match self.store.get() {
            Some(store) => match store
                .authority_project_state_for_route(&route_project_root, authority_domain)
            {
                Ok(Some((authority_project, authority_state))) => {
                    if requested_project.is_some_and(|requested| requested != authority_project) {
                        return Err(HandlerOutcome::Error {
                            code: "facade_project_vocabulary_mismatch".to_string(),
                            message: format!(
                                "{authority_domain} facade route {route_project_root} is authority-managed as {authority_project}, but the request supplied {}",
                                requested_project.unwrap_or_default()
                            ),
                        });
                    }
                    if bind_authority_for_write && authority_state != "MODULE" {
                        // Reads and transforms may keep using the module identity while authority
                        // drains, but facade mutations must retry instead of writing after ownership changes.
                        return Err(authority_draining_error(authority_domain));
                    }
                    authority_project
                }
                // A route without an authority binding remains path-scoped. Lookup failures are
                // retryable errors: silently using the route could read or write the wrong owner.
                Ok(None) => route_project_root.clone(),
                Err(error) => {
                    return Err(HandlerOutcome::Error {
                        code: "authority_project_resolution_failed".to_string(),
                        message: error.to_string(),
                    })
                }
            },
            None => route_project_root.clone(),
        };
        Ok(FacadeScope {
            memory_project_path,
            route_project_root,
            conversation_key,
            memory_enabled: binding.config.memory_enabled,
        })
    }

    async fn handle_ctx_reduce_facade(&self, _channel: u16, request: &Value) -> HandlerOutcome {
        // Parse the optional reduced-call envelope a model may repeat from context, even though
        // this endpoint only acknowledges the request; the response observer performs delivery.
        let _ = facade_arguments(request, &["drop"]);
        // The MCP-facing route is acknowledgement-only. Destructive delivery is owned by
        // the response observer, so this path must return before identity or storage work.
        mcp_text_result(CTX_REDUCE_ACKNOWLEDGEMENT.to_string(), false)
    }

    async fn handle_ctx_memory_facade(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(args) = facade_arguments(request, &["action"]) else {
            return invalid_params_error("ctx_memory arguments must be an object");
        };
        let args = &args;
        let Some(action) = string_arg(args, "action") else {
            return invalid_params_error("ctx_memory requires an action");
        };
        if let Err(error) = validate_memory_id_arguments(args)
            .and_then(|_| validate_string_cap(args, "content", MAX_MEMORY_CONTENT_BYTES))
            .and_then(|_| validate_string_cap(args, "reason", MAX_SHORT_FIELD_BYTES))
        {
            return tool_error_result(format!("Error: {error}."));
        }
        let facade_scope = match self
            .resolve_facade_scope(
                channel,
                Some(args),
                "memories",
                matches!(action, "write" | "update" | "archive" | "merge"),
            )
            .await
        {
            Ok(scope) => scope,
            Err(outcome) => return outcome,
        };
        if !facade_scope.memory_enabled {
            return tool_error_result("Error: memory is disabled for this project.".to_string());
        }
        let store = match self.store.get() {
            Some(store) => store,
            None => return store_unavailable_error(),
        };
        let memory_project = facade_scope.memory_project_path.as_str();
        let conversation_key = facade_scope.conversation_key.as_str();
        if matches!(action, "write" | "update" | "archive" | "merge") {
            if let Err(error) = store.enforce_facade_project_vocabulary(
                facade_scope.route_project_root.as_str(),
                memory_project,
                "memories",
            ) {
                return tool_error_result(format!("Error: {error}"));
            }
        }
        match action {
            "write" => {
                let Some(category) = non_empty_string_arg(args, "category") else {
                    return tool_error_result(
                        "Error: 'category' is required when action is 'write'.",
                    );
                };
                if !MEMORY_CATEGORIES.contains(&category) {
                    return tool_error_result(format!(
                        "Error: category must be one of {}.",
                        MEMORY_CATEGORIES.join(", ")
                    ));
                }
                let Some(content) = non_empty_string_arg(args, "content") else {
                    return tool_error_result(
                        "Error: 'content' is required when action is 'write'.",
                    );
                };
                match store.with_facade_mutation(
                    facade_scope.route_project_root.as_str(),
                    "memories",
                    || {
                        store.insert_memory(InsertMemoryInput {
                            project_path: memory_project,
                            route_project_root: Some(facade_scope.route_project_root.as_str()),
                            category,
                            content,
                            source_session_id: Some(conversation_key),
                            source_type: Some("agent"),
                            importance: Some(50),
                            expires_at: None,
                            metadata_json: None,
                            now_ms: now_ms(),
                        })
                    },
                ) {
                    Ok(id) => {
                        mcp_text_result(format!("Saved memory [ID: {id}] in {category}."), false)
                    }
                    Err(error) if store_error_is_authority_draining(&error) => {
                        authority_draining_error("memories")
                    }
                    Err(error) => HandlerOutcome::Error {
                        code: "memory_store_failed".to_string(),
                        message: error.to_string(),
                    },
                }
            }
            "update" => {
                let Some(id) = single_memory_id(args, "update") else {
                    return tool_error_result(
                        "Error: provide exactly one memory id when action is 'update'.",
                    );
                };
                let Some(content) = non_empty_string_arg(args, "content") else {
                    return tool_error_result(
                        "Error: 'content' is required when action is 'update'.",
                    );
                };
                match store.with_facade_mutation(
                    facade_scope.route_project_root.as_str(),
                    "memories",
                    || memory_tool::update_memory(store, memory_project, id, content, now_ms()),
                ) {
                    Ok(memory) => mcp_text_result(
                        format!("Updated memory [ID: {}] in {}.", memory.id, memory.category),
                        false,
                    ),
                    Err(error) if store_error_is_authority_draining(&error) => {
                        authority_draining_error("memories")
                    }
                    Err(error) => tool_error_result(format!("Error: {error}")),
                }
            }
            "archive" => {
                let ids = memory_ids(args, "archive");
                if ids.is_empty() {
                    return tool_error_result(
                        "Error: provide at least one memory id when action is 'archive'.",
                    );
                }
                let reason = string_arg(args, "reason");
                let archived = match store.with_facade_mutation(
                    facade_scope.route_project_root.as_str(),
                    "memories",
                    || memory_tool::archive_memories(store, memory_project, &ids, reason, now_ms()),
                ) {
                    Ok(archived) => archived,
                    Err(error) if store_error_is_authority_draining(&error) => {
                        return authority_draining_error("memories");
                    }
                    Err(error) => return tool_error_result(format!("Error: {error}")),
                };
                if archived.is_empty() {
                    mcp_text_result("No active memories needed archiving.".to_string(), false)
                } else {
                    mcp_text_result(
                        format!("Archived memory IDs [{}].", join_i64s(&archived)),
                        false,
                    )
                }
            }
            "merge" => {
                let Some((target_id, source_ids)) = merge_ids(args) else {
                    return tool_error_result(
                        "Error: provide target_id plus source_ids, or ids with the target first, when action is 'merge'.",
                    );
                };
                let Some(content) = non_empty_string_arg(args, "content") else {
                    return tool_error_result(
                        "Error: 'content' is required when action is 'merge'.",
                    );
                };
                match store.with_facade_mutation(
                    facade_scope.route_project_root.as_str(),
                    "memories",
                    || {
                        memory_tool::merge_memories(
                            store,
                            memory_project,
                            target_id,
                            &source_ids,
                            content,
                            now_ms(),
                        )
                    },
                ) {
                    Ok(memory) => mcp_text_result(
                        format!(
                            "Merged memories into [ID: {}] in {}; superseded [{}].",
                            memory.id,
                            memory.category,
                            join_i64s(&source_ids)
                        ),
                        false,
                    ),
                    Err(error) if store_error_is_authority_draining(&error) => {
                        authority_draining_error("memories")
                    }
                    Err(error) => tool_error_result(format!("Error: {error}")),
                }
            }
            "get" => {
                let ids = memory_ids(args, "get");
                match memory_tool::get_memories(store, memory_project, &ids) {
                    Ok(memories) => {
                        let by_id = memories
                            .into_iter()
                            .map(|memory| (memory.id, memory))
                            .collect::<std::collections::HashMap<_, _>>();
                        let lines = ids
                            .iter()
                            .map(|id| match by_id.get(id) {
                                Some(memory) => format!(
                                    "Memory [ID: {}] in {} (status: {}): {}",
                                    memory.id, memory.category, memory.status, memory.content
                                ),
                                None => {
                                    format!("id {id}: not found or not visible from this project")
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        mcp_text_result(lines, false)
                    }
                    Err(error) => tool_error_result(format!("Error: {error}")),
                }
            }
            _ => tool_error_result("Error: Unknown ctx_memory action.".to_string()),
        }
    }

    async fn handle_ctx_search_facade(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(args) = facade_arguments(request, &["query"]) else {
            return invalid_params_error("ctx_search arguments must be an object");
        };
        let args = &args;
        let Some(query) = non_empty_string_arg(args, "query") else {
            return tool_error_result("Error: 'query' is required for ctx_search.");
        };
        if let Err(error) = validate_string_cap(args, "query", MAX_QUERY_BYTES) {
            return tool_error_result(format!("Error: {error}."));
        }
        let limit = usize_arg(args, "limit").unwrap_or(8).clamp(1, 25);
        let facade_scope = match self
            .resolve_facade_scope(channel, Some(args), "memories", false)
            .await
        {
            Ok(scope) => scope,
            Err(outcome) => return outcome,
        };
        let store = match self.store.get() {
            Some(store) => store,
            None => return store_unavailable_error(),
        };
        let memory_project = facade_scope.memory_project_path.as_str();
        let conversation_key = facade_scope.conversation_key.as_str();
        match memory_tool::search_memories_and_compartments_for_session(
            store,
            memory_project,
            conversation_key,
            query,
            limit,
            facade_scope.memory_enabled,
        ) {
            Ok(results) => {
                let rendered = results
                    .into_iter()
                    .map(|result| {
                        json!({
                            "source": match result.source_kind {
                                memory_tool::MemorySearchSourceKind::Memory => "memory",
                                memory_tool::MemorySearchSourceKind::CompartmentTitle => "compartment_title",
                                memory_tool::MemorySearchSourceKind::CompartmentBody => "compartment_body",
                                memory_tool::MemorySearchSourceKind::Note => "note",
                            },
                            "id": result.id,
                            "snippet": result.snippet,
                            "category": result.category,
                            "sequence": result.sequence,
                            "title": result.title,
                            "status": result.note_status,
                            "surface_condition": result.surface_condition,
                        })
                    })
                    .collect::<Vec<_>>();
                mcp_text_result(Value::Array(rendered).to_string(), false)
            }
            Err(error) => tool_error_result(format!("Error: {error}")),
        }
    }

    async fn handle_ctx_expand_facade(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(args) = facade_arguments(request, &["message", "start"]) else {
            return invalid_params_error("ctx_expand arguments must be an object");
        };
        let args = &args;
        let facade_scope = match self
            .resolve_facade_scope(channel, Some(args), "memories", false)
            .await
        {
            Ok(scope) => scope,
            Err(outcome) => return outcome,
        };
        let store = match self.store.get() {
            Some(store) => store,
            None => return store_unavailable_error(),
        };
        let session_id = facade_scope.conversation_key.as_str();
        if let Some(message) = i64_arg(args, "message").filter(|value| *value > 0) {
            return match store.load_chunk_transcript_for_message(session_id, message) {
                Ok(Some(row)) => mcp_text_result(render_message_expand(row, message), false),
                Ok(None) => mcp_text_result(
                    format!(
                        "Message {message} is no longer recoverable from persisted chunk transcripts. The span was evicted or was compacted before transcript capture."
                    ),
                    false,
                ),
                Err(error) => tool_error_result(format!("Error: {error}")),
            };
        }
        let Some(start) = i64_arg(args, "start") else {
            return tool_error_result(
                "Error: provide either message=<ordinal>, or start and end (positive integers, start <= end).",
            );
        };
        let Some(end) = i64_arg(args, "end") else {
            return tool_error_result(
                "Error: provide either message=<ordinal>, or start and end (positive integers, start <= end).",
            );
        };
        if start < 1 || end < start {
            return tool_error_result(
                "Error: provide either message=<ordinal>, or start and end (positive integers, start <= end).",
            );
        }
        let compartments = match store.load_compartments(session_id) {
            Ok(compartments) => compartments,
            Err(error) => return tool_error_result(format!("Error: {error}")),
        };
        let transcripts = match store.load_chunk_transcripts_for_range(session_id, start, end) {
            Ok(transcripts) => transcripts,
            Err(error) => return tool_error_result(format!("Error: {error}")),
        };
        mcp_text_result(
            render_range_expand(start, end, &compartments, &transcripts),
            false,
        )
    }

    async fn handle_note_evaluation_value(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(note_id) = request
            .get("note_id")
            .and_then(Value::as_i64)
            .filter(|id| *id > 0)
        else {
            return HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: "note.evaluate requires a positive note_id".to_string(),
            };
        };
        let Some(session_id) = request.get("session_id").and_then(Value::as_str) else {
            return HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: "note.evaluate requires session_id".to_string(),
            };
        };
        let scope = match self
            .resolve_facade_scope(channel, None, "notes", false)
            .await
        {
            Ok(scope) => scope,
            Err(outcome) => return outcome,
        };
        if scope.conversation_key != session_id {
            return HandlerOutcome::Error {
                code: "session_mismatch".to_string(),
                message: "note.evaluate session_id does not match the channel binding".to_string(),
            };
        }
        let Some(store) = self.store.get() else {
            return store_unavailable_error();
        };
        let Some(source_revision) = request.get("source_revision").and_then(Value::as_i64) else {
            return HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: "note.evaluate requires source_revision".to_string(),
            };
        };
        let input = NoteEvaluationInput {
            project_path: scope.memory_project_path.as_str(),
            note_id,
            source_revision,
            verdict: request
                .get("verdict")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            compiled_check: request.get("compiled_check").and_then(Value::as_str),
            manifest_json: request.get("manifest_json").and_then(Value::as_str),
            check_hash: request.get("check_hash").and_then(Value::as_str),
            next_due_at: request.get("next_due_at").and_then(Value::as_i64),
            now_ms: now_ms(),
        };
        match store.write_note_evaluation(input) {
            Ok(NoteCasOutcome::Applied(note)) => respond(json!({
                "ok": true,
                "note_id": note.id,
                "status": note.status,
                "status_version": note.status_version,
            })),
            Ok(NoteCasOutcome::Conflict { current }) => respond(json!({
                "ok": false,
                "conflict": true,
                "note": current.map(|note| json!({
                    "id": note.id,
                    "status": note.status,
                    "status_version": note.status_version,
                })),
            })),
            Err(error) => HandlerOutcome::Error {
                code: "note_store_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    async fn handle_note_delivery_value(
        &self,
        channel: u16,
        request: &Value,
        ack: bool,
    ) -> HandlerOutcome {
        let Some(session_id) = request.get("session_id").and_then(Value::as_str) else {
            return HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: "transform delivery acknowledgement requires session_id".to_string(),
            };
        };
        let pass_id = request
            .get("transform_pass_id")
            .and_then(Value::as_str)
            .or_else(|| request.get("pass_id").and_then(Value::as_str));
        let Some(pass_id) = pass_id.filter(|id| !id.trim().is_empty()) else {
            return HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: "transform delivery acknowledgement requires transform_pass_id"
                    .to_string(),
            };
        };
        let scope = match self
            .resolve_facade_scope(channel, None, "notes", false)
            .await
        {
            Ok(scope) => scope,
            Err(outcome) => return outcome,
        };
        if scope.conversation_key != session_id {
            return HandlerOutcome::Error {
                code: "session_mismatch".to_string(),
                message: "delivery acknowledgement session_id does not match the channel binding"
                    .to_string(),
            };
        }
        let Some(store) = self.store.get() else {
            return store_unavailable_error();
        };
        let result = if ack {
            store.ack_note_delivery(
                scope.memory_project_path.as_str(),
                session_id,
                pass_id,
                now_ms(),
            )
        } else {
            store.nack_note_delivery(
                scope.memory_project_path.as_str(),
                session_id,
                pass_id,
                now_ms(),
            )
        };
        match result {
            Ok(changed) => respond(json!({ "ok": true, "updated": changed })),
            Err(error) => HandlerOutcome::Error {
                code: "note_store_failed".to_string(),
                message: error.to_string(),
            },
        }
    }

    async fn handle_ctx_note_facade(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(args) = facade_arguments(request, &["action", "content"]) else {
            return invalid_params_error("ctx_note arguments must be an object");
        };
        let args = &args;
        if let Err(error) = validate_string_cap(args, "content", MAX_NOTE_CONTENT_BYTES)
            .and_then(|_| validate_string_cap(args, "surface_condition", MAX_SHORT_FIELD_BYTES))
        {
            return tool_error_result(format!("Error: {error}."));
        }
        let action = string_arg(args, "action")
            .or_else(|| non_empty_string_arg(args, "content").map(|_| "write"))
            .unwrap_or("read");
        let facade_scope = match self
            .resolve_facade_scope(
                channel,
                Some(args),
                "notes",
                matches!(action, "write" | "update" | "dismiss"),
            )
            .await
        {
            Ok(scope) => scope,
            Err(outcome) => return outcome,
        };
        let store = match self.store.get() {
            Some(store) => store,
            None => return store_unavailable_error(),
        };
        let project = facade_scope.memory_project_path.as_str();
        let session = facade_scope.conversation_key.as_str();
        let filter = string_arg(args, "filter");
        let now = now_ms();
        if matches!(action, "write" | "update" | "dismiss") {
            if let Err(error) = store.enforce_facade_project_vocabulary(
                facade_scope.route_project_root.as_str(),
                project,
                "notes",
            ) {
                return tool_error_result(format!("Error: {error}"));
            }
        }

        match action {
            "write" => {
                let Some(content) = non_empty_string_arg(args, "content") else {
                    return tool_error_result(
                        "Error: 'content' is required when action is 'write'.",
                    );
                };
                let anchor = store
                    .load(session)
                    .ok()
                    .and_then(|loaded| loaded.meta.newest_live_block_id);
                let condition = string_arg(args, "surface_condition")
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                if let Some(condition) = condition {
                    match store.with_facade_mutation(
                        facade_scope.route_project_root.as_str(),
                        "notes",
                        || {
                            store.insert_project_note(NoteWriteInput {
                                project_path: project,
                                route_project_root: Some(facade_scope.route_project_root.as_str()),
                                session_id: Some(session),
                                content,
                                surface_condition: Some(condition),
                                anchor_block_id: anchor.as_deref(),
                                anchor_ordinal: None,
                                now_ms: now,
                            })
                        },
                    ) {
                        Ok(note) => mcp_text_result(
                            format!(
                                "Created smart note #{}. Dreamer will evaluate the condition during nightly runs:\n- Content: {}\n- Condition: {}",
                                note.id, note.content, condition
                            ),
                            false,
                        ),
                        Err(error) if store_error_is_authority_draining(&error) => {
                            authority_draining_error("notes")
                        }
                        Err(error) => tool_error_result(format!("Error: {error}")),
                    }
                } else {
                    match store.with_facade_mutation(
                        facade_scope.route_project_root.as_str(),
                        "notes",
                        || {
                            store.insert_note(NoteInput {
                                project_path: project,
                                route_project_root: Some(facade_scope.route_project_root.as_str()),
                                session_id: session,
                                content,
                                surface_condition: None,
                                anchor_block_id: anchor.as_deref(),
                                now_ms: now,
                            })
                        },
                    ) {
                        Ok(note) => {
                            mcp_text_result(format!("Saved session note #{}.", note.id), false)
                        }
                        Err(error) if store_error_is_authority_draining(&error) => {
                            authority_draining_error("notes")
                        }
                        Err(error) => tool_error_result(format!("Error: {error}")),
                    }
                }
            }
            "read" => {
                let limit = usize_arg(args, "limit").unwrap_or(25).clamp(1, 100);
                let offset = usize_arg(args, "offset").unwrap_or(0);
                let statuses: Vec<&str> = match filter {
                    None => vec!["active", "ready"],
                    Some("active") => vec!["active"],
                    Some("pending") => vec!["pending"],
                    Some("ready") => vec!["ready"],
                    Some("dismissed") => vec!["dismissed"],
                    Some("all") => vec![
                        "active",
                        "pending",
                        "ready",
                        "surfacing",
                        "surfaced",
                        "dismissed",
                    ],
                    Some(_) => return tool_error_result(
                        "Error: filter must be one of all, active, pending, ready, or dismissed."
                            .to_string(),
                    ),
                };
                let session_statuses = if filter.is_none() {
                    vec!["active"]
                } else {
                    statuses.clone()
                };
                let smart_statuses = if filter.is_none() {
                    vec!["ready"]
                } else {
                    statuses
                };
                let session_notes = match store.read_project_notes(
                    project,
                    Some(session),
                    &session_statuses,
                    limit,
                    offset,
                ) {
                    Ok(notes) => notes,
                    Err(error) => return tool_error_result(format!("Error: {error}")),
                };
                let smart_notes =
                    match store.read_smart_notes(project, &smart_statuses, limit, offset) {
                        Ok(notes) => notes,
                        Err(error) => return tool_error_result(format!("Error: {error}")),
                    };
                let session_total = match store.count_notes_by_type(
                    project,
                    "session",
                    Some(session),
                    &session_statuses,
                ) {
                    Ok(total) => total,
                    Err(error) => return tool_error_result(format!("Error: {error}")),
                };
                let smart_total =
                    match store.count_notes_by_type(project, "smart", None, &smart_statuses) {
                        Ok(total) => total,
                        Err(error) => return tool_error_result(format!("Error: {error}")),
                    };
                mcp_text_result(
                    render_notes(
                        session_notes,
                        smart_notes,
                        session_total,
                        smart_total,
                        offset,
                        filter.is_none(),
                    ),
                    false,
                )
            }
            "update" => {
                let Some(note_id) = i64_arg(args, "note_id").filter(|id| *id > 0) else {
                    return tool_error_result(
                        "Error: 'note_id' is required when action is 'update'.",
                    );
                };
                let content = non_empty_string_arg(args, "content");
                let condition = string_arg(args, "surface_condition")
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                if content.is_none() && condition.is_none() {
                    return tool_error_result(
                        "Error: Provide 'content' and/or 'surface_condition' to update."
                            .to_string(),
                    );
                }
                let current = match store.get_note_by_id(project, session, note_id) {
                    Ok(note) => note.filter(|note| {
                        matches!(
                            note.status.as_str(),
                            "active" | "pending" | "ready" | "surfacing" | "surfaced"
                        )
                    }),
                    Err(error) => return tool_error_result(format!("Error: {error}")),
                };
                let Some(current) = current else {
                    return tool_error_result(format!(
                        "Error: Note #{note_id} not found in your session/project or has no compatible fields to update."
                    ));
                };
                match store.with_facade_mutation(
                    facade_scope.route_project_root.as_str(),
                    "notes",
                    || {
                        store.update_note_cas(
                            project,
                            note_id,
                            &current.status,
                            current.status_version,
                            content,
                            condition.map(Some),
                            now,
                        )
                    },
                ) {
                    Ok(NoteCasOutcome::Applied(note)) => mcp_text_result(
                        format!("Updated note #{}: {}", note.id, note.content),
                        false,
                    ),
                    Ok(NoteCasOutcome::Conflict { .. }) => tool_error_result(format!(
                        "Error: Note #{note_id} changed concurrently; retry with a fresh read."
                    )),
                    Err(error) if store_error_is_authority_draining(&error) => {
                        authority_draining_error("notes")
                    }
                    Err(error) => tool_error_result(format!("Error: {error}")),
                }
            }
            "dismiss" => {
                let Some(note_id) = i64_arg(args, "note_id").filter(|id| *id > 0) else {
                    return tool_error_result(
                        "Error: 'note_id' is required when action is 'dismiss'.",
                    );
                };
                match store.with_facade_mutation(
                    facade_scope.route_project_root.as_str(),
                    "notes",
                    || {
                        store.dismiss_note(
                            project,
                            session,
                            note_id,
                            string_arg(args, "content"),
                            now,
                        )
                    },
                ) {
                    Ok(Some(_)) => mcp_text_result(format!("Note #{note_id} dismissed."), false),
                    Ok(None) => tool_error_result(format!(
                        "Error: Note #{note_id} not found in your session/project or already dismissed."
                    )),
                    Err(error) if store_error_is_authority_draining(&error) => {
                        authority_draining_error("notes")
                    }
                    Err(error) => tool_error_result(format!("Error: {error}")),
                }
            }
            _ => tool_error_result("Error: Unknown ctx_note action.".to_string()),
        }
    }
}

impl Default for McHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModuleHandler for McHandler {
    /// The storage seam: HELLO_ACK carries the resolved descriptor (or none in
    /// standalone dev). Open the store ONCE here — never at construction, because the
    /// path isn't known until the ACK lands.
    ///
    /// Guard the OPEN itself, not just the `set`: `McStore::open` acquires the
    /// single-writer lease, so on a reconnect ack (if serve re-fires this) an
    /// unconditional open briefly takes a second lease on a process that already
    /// holds one. `on_hello_ack` is called serially by serve, so the get/open/set
    /// is race-free without a lock.
    async fn on_hello_ack(&self, ack: &ModuleHelloAckBody) {
        if self.store.get().is_some() {
            return;
        }
        let descriptor = resolve_descriptor(ack.storage.as_ref());
        match McStore::open(&descriptor) {
            Ok(store) => {
                let _ = self.store.set(Arc::new(store));
            }
            Err(e) => {
                eprintln!("mc-module: store open failed: {e}");
            }
        }
    }

    /// Record the route's {project_root, session} so the transform path can resolve the
    /// project from the daemon-controlled channel (never a per-pass request field). Accept
    /// every route — project resolution, not authorization, is the concern here.
    async fn on_bind(&self, req: &RouteBindRequest) -> subc_client_rs::BindDecision {
        let config = self.effective_config(&req.identity.project_root);
        self.bind_route(
            req.handle.channel,
            SessionBinding {
                project_root: req.identity.project_root.clone(),
                harness: req.identity.harness.clone(),
                session: req.identity.session.clone(),
                model_key: None,
                config,
                // Older callers may omit the per-pass budget. Keep a safe fallback on the
                // route, while authority requests carry the harness-resolved value.
                history_budget_tokens: memory_render::DEFAULT_HISTORY_BUDGET_TOKENS,
            },
        );
        subc_client_rs::BindDecision::accept()
    }

    /// Drop the route's binding on teardown so a reused channel can't resolve a stale
    /// project and the map doesn't leak.
    async fn on_route_gone(&self, handle: &RouteHandle) {
        self.unbind_route(handle.channel);
    }

    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        if let Err(outcome) = enforce_request_byte_cap(&body) {
            return outcome;
        }
        let request = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
        self.dispatch_value(ctx.route_handle().channel, request)
            .await
    }
}

impl McHandler {
    /// Route a parsed request body to its handler. Split from `handle()` so the
    /// routing arms are unit-testable (`RequestCtx` cannot be constructed
    /// outside the transport).
    async fn dispatch_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .or_else(|| request.get("kind").and_then(Value::as_str));
        if let Some(method) = method {
            return match method {
                // Proves the store opened end-to-end and, when a session_id is supplied,
                // returns the session's stored trace state directly from the module.
                "health" | "status" | "diagnostics" => self.handle_status_value(&request),
                "authority.status" => self.handle_authority_status_value(channel, &request),
                "authority.prepare" => self.handle_authority_prepare_value(channel, &request),
                "authority.seed" => self.handle_authority_seed_value(&request),
                "authority.drain.begin"
                | "authority.drain.step"
                | "authority.drain.finish"
                | "authority.drain_seed"
                | "authority.drain_memories"
                | "authority.drain_notes"
                | "authority.drain_compartments"
                | "authority.drain_reconcile"
                | "authority.drain_verify"
                | "authority.drain_flip"
                | "authority.drain_finish" => self.handle_authority_drain_value(&request, method),
                "mirror.pull" => self.handle_mirror_pull_value(&request),
                "guidance.get" => self.handle_guidance_value(channel, &request),
                "dreamer.run_task" => self.handle_dreamer_run_task(channel, &request).await,
                // Handle transform requests: decode the incoming context array, update
                // cache state, and return the rewritten array for the caller.
                "transform" => {
                    if has_transform_page_fields(&request) {
                        self.handle_transform_page_value(channel, request, TransformLane::Authority)
                            .await
                    } else {
                        self.handle_transform_value(channel, request).await
                    }
                }
                "state_sync" => {
                    // State sync is shared by both the authority and shadow lanes. Only the
                    // shadow lane is controlled by the mirror kill switch; the authority lane
                    // must keep receiving its own state updates when mirror traffic is stopped.
                    if self.state_sync_targets_shadow(channel, &request)
                        && !self.shadow_lane_enabled()
                    {
                        HandlerOutcome::Error {
                            code: "shadow_disabled".to_string(),
                            message: "shadow state sync is disabled by configuration".to_string(),
                        }
                    } else {
                        let authority = self
                            .bindings
                            .lock()
                            .expect("bindings mutex")
                            .get(&channel)
                            .is_some_and(|binding| !is_shadow_session(&binding.session));
                        let outcome = self.handle_state_sync_value(channel, request);
                        if authority {
                            authority_state_sync_outcome(outcome)
                        } else {
                            outcome
                        }
                    }
                }
                "shadow_transform" | "shadow_reset" if !self.shadow_lane_enabled() => {
                    HandlerOutcome::Error {
                        code: "shadow_disabled".to_string(),
                        message: "shadow lane is disabled by configuration".to_string(),
                    }
                }
                "shadow_transform" => self.handle_shadow_transform_value(channel, request).await,
                "shadow_reset" => self.handle_shadow_reset_value(channel, request),
                "state_import" => self.handle_state_import_value(channel, request),
                "agent_drops.append" => self.handle_agent_drops_value(channel, request),
                "note.evaluate" => self.handle_note_evaluation_value(channel, &request).await,
                "transform.ack" => {
                    self.handle_note_delivery_value(channel, &request, true)
                        .await
                }
                "transform.nack" => {
                    self.handle_note_delivery_value(channel, &request, false)
                        .await
                }
                "todo_state.set" => self.handle_todo_state_set_value(channel, &request),
                "session.flush" => self.handle_session_flush_value(channel, &request),
                "session.recomp" => self.handle_session_recomp_value(channel, &request),
                "session.status" => self.handle_session_status_value(channel, &request),
                "session.wrapup" => self.handle_session_wrapup_value(channel, &request).await,
                // Explicit wire-debugging echo. Opt-in only: echoing every
                // unrecognized body would silently swallow misrouted requests
                // (a caller can "succeed" against an echo while testing nothing),
                // so unknown shapes fail loud below instead.
                "echo" => respond(json!({ "ok": true, "echo": request })),
                _ => unrecognized_request_error(&request),
            };
        }
        if request.get("name").is_some() && request.get("arguments").is_some() {
            return self.handle_facade_value(channel, request).await;
        }
        unrecognized_request_error(&request)
    }
}

// The wire validator uses the same field names and validation paths as the shadow lane.
// Translate only rejected authority responses so diagnostics identify the lane that failed.
fn authority_state_sync_outcome(outcome: HandlerOutcome) -> HandlerOutcome {
    match outcome {
        HandlerOutcome::Error { code, message } => HandlerOutcome::Error {
            code: code.replace("shadow", "authority"),
            message: message.replace("shadow", "authority"),
        },
        other => other,
    }
}

fn has_transform_page_fields(request: &Value) -> bool {
    TRANSFORM_PAGE_FIELDS
        .iter()
        .any(|field| request.get(*field).is_some())
}

fn transform_page_error(
    lane: TransformLane,
    suffix: &str,
    message: impl Into<String>,
) -> HandlerOutcome {
    let code = if lane.is_shadow() && suffix == "in_progress" {
        "transform_page_in_progress".to_string()
    } else {
        let prefix = if lane.is_shadow() {
            "shadow"
        } else {
            "authority"
        };
        format!("{prefix}_transform_page_{suffix}")
    };
    HandlerOutcome::Error {
        code,
        message: message.into(),
    }
}

/// Classify a request that matched no known `method`/`kind`. Two distinct
/// errors so a misroute is diagnosable from the error code alone:
/// - `{name, arguments}` without `method`/`kind` is the shape of an MCP
///   tools/call envelope. Only ctx_memory and ctx_search are accepted on that
///   surface; unsupported names keep a distinct error so a policy or routing
///   mistake is diagnosable from the code alone.
/// - Anything else names the discriminator fields we looked for and the
///   top-level keys we actually got.
fn unrecognized_request_error(request: &Value) -> HandlerOutcome {
    let has_mcp_shape = request.get("name").is_some() && request.get("arguments").is_some();
    if has_mcp_shape {
        return HandlerOutcome::Error {
            code: "facade_envelope_not_supported".to_string(),
            message: "MCP tools/call envelope ({name, arguments}) names a tool this module \
                      does not route on the facade; other module commands use flat bodies \
                      with a top-level `kind` field"
                .to_string(),
        };
    }
    let got_keys = match request.as_object() {
        Some(map) => {
            let keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.join(", ")
        }
        None => format!("non-object JSON ({})", json_type_name(request)),
    };
    HandlerOutcome::Error {
        code: "unrecognized_request_shape".to_string(),
        message: format!(
            "no `method` or `kind` field matched a known request; got top-level keys: [{got_keys}]"
        ),
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The wall-clock now in ms. Used ONLY to set the frozen expiry cutoff on a HARD (the
/// first materialization freezes it into meta); every later pass reads the frozen value,
/// never this, so expiry never drifts the rendered bytes between passes.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn unknown_serializer_profile_error() -> HandlerOutcome {
    HandlerOutcome::Error {
        code: "unknown_serializer_profile".to_string(),
        message: "missing or unknown serializer_profile".to_string(),
    }
}

fn serve_native_unsupported_profile_error(profile: &str) -> HandlerOutcome {
    HandlerOutcome::Error {
        code: "serve_native_unsupported_profile".to_string(),
        message: format!("serve_native requires serializer_profile opencode-aisdk, got {profile}"),
    }
}

fn attach_native_messages(
    response: &mut transform::TransformResponse,
    request: &TransformRequest,
    reasoning_watermark: u64,
) {
    if !request.serve_native {
        return;
    }
    let sidecar = request
        .native_messages
        .as_deref()
        .map(codec::decode_opencode)
        .map(|decoded| decoded.sidecar)
        .unwrap_or_else(|| codec::DecodeSidecar::new("opencode"));
    let mut native_messages = codec::encode_opencode_with_session(
        response.messages(),
        &sidecar,
        Some(&request.session_id),
    );
    if let Some(profile) = SerializerProfile::parse(&request.serializer_profile) {
        transform::clear_served_native_reasoning(
            profile,
            transform::request_accepts_empty_content(request),
            &mut native_messages,
            response.messages(),
            &request.messages,
            reasoning_watermark,
            request.mid_turn,
        );
    }
    response.native_messages = Some(native_messages);
}

fn state_import_validation_error(error: StateImportValidationError) -> HandlerOutcome {
    HandlerOutcome::Error {
        code: error.code().to_string(),
        message: error.to_string(),
    }
}

fn need_full_sync_response(full_array_fingerprint: Option<String>) -> HandlerOutcome {
    respond(
        serde_json::to_value(transform::TransformResponse::need_full_sync(
            full_array_fingerprint,
        ))
        .unwrap_or(Value::Null),
    )
}

fn replay_dream_task_response(response_json: &str) -> HandlerOutcome {
    let Ok(response) = serde_json::from_str::<Value>(response_json) else {
        return HandlerOutcome::Error {
            code: "dreamer_ledger_corrupt".to_string(),
            message: "recorded dreamer response is not valid JSON".to_string(),
        };
    };
    if response.get("ok").and_then(Value::as_bool) == Some(false) {
        return HandlerOutcome::Error {
            code: response
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("dreamer_run_failed")
                .to_string(),
            message: response
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("dreamer task failed")
                .to_string(),
        };
    }
    respond(response)
}

fn respond(value: Value) -> HandlerOutcome {
    match serde_json::to_vec(&value) {
        Ok(bytes) => HandlerOutcome::Response(bytes),
        Err(e) => HandlerOutcome::Error {
            code: "encode_failed".to_string(),
            message: e.to_string(),
        },
    }
}

fn guidance_bytes_for(text: &str, date_line: &str) -> String {
    format!("{text}\n{date_line}")
}

fn primary_language_directive(language: &str) -> Option<String> {
    if language.len() != 2 || !language.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    let name = match language.to_ascii_lowercase().as_str() {
        "ar" => "Arabic (العربية)",
        "cs" => "Czech (Čeština)",
        "da" => "Danish (Dansk)",
        "de" => "German (Deutsch)",
        "el" => "Greek (Ελληνικά)",
        "en" => "English",
        "es" => "Spanish (Español)",
        "fi" => "Finnish (Suomi)",
        "fr" => "French (Français)",
        "he" => "Hebrew (עברית)",
        "hi" => "Hindi (हिन्दी)",
        "hu" => "Hungarian (Magyar)",
        "id" => "Indonesian",
        "it" => "Italian (Italiano)",
        "ja" => "Japanese (日本語)",
        "ko" => "Korean (한국어)",
        "nl" => "Dutch (Nederlands)",
        "no" => "Norwegian (Norsk)",
        "pl" => "Polish (Polski)",
        "pt" => "Portuguese (Português)",
        "ro" => "Romanian (Română)",
        "ru" => "Russian (Русский)",
        "sk" => "Slovak (Slovenčina)",
        "sv" => "Swedish (Svenska)",
        "th" => "Thai (ไทย)",
        "tr" => "Turkish (Türkçe)",
        "uk" => "Ukrainian (Українська)",
        "vi" => "Vietnamese (Tiếng Việt)",
        "zh" => "Chinese (中文)",
        _ => return None,
    };
    Some(format!(
        "Use {name} for your natural-language replies to the user unless the user explicitly asks for another language. Keep code, identifiers, file paths, commands, logs, and quoted text verbatim."
    ))
}

fn shadow_seed_content_digest(request: &Value) -> String {
    let mut content = request.clone();
    if let Some(object) = content.as_object_mut() {
        for field in [
            "shadow_generation",
            "seed_id",
            "seed_generation",
            "expected_shadow_seq",
            "seed_batch_index",
            "seed_batch_total",
            "seed_complete",
        ] {
            object.remove(field);
        }
    }
    sha256_hex(canonical_value(&content).as_bytes())
}

fn transform_page_content_digest(request: &Value) -> String {
    let mut content = Map::new();
    for field in TRANSFORM_PAGE_ARRAY_FIELDS {
        if let Some(value) = request.get(field) {
            content.insert(field.to_string(), value.clone());
        }
    }
    sha256_hex(canonical_value(&Value::Object(content)).as_bytes())
}

fn assemble_transform_page_field(field: &str, values: Vec<Value>) -> Result<Vec<Value>, String> {
    let mut assembled = Vec::new();
    let mut index = 0usize;
    while index < values.len() {
        let Some(marker) = values[index]
            .get(SHADOW_ITEM_CONTINUATION_KEY)
            .and_then(Value::as_object)
        else {
            assembled.push(values[index].clone());
            index += 1;
            continue;
        };
        let marker_field = marker
            .get("field")
            .and_then(Value::as_str)
            .ok_or_else(|| "transform continuation marker is missing its field".to_string())?;
        let item_index = marker
            .get("item_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| "transform continuation marker has an invalid item index".to_string())?;
        let chunk_index = marker
            .get("chunk_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| {
                "transform continuation marker has an invalid chunk index".to_string()
            })?;
        let chunk_total = marker
            .get("chunk_total")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| {
                "transform continuation marker has an invalid chunk total".to_string()
            })?;
        if marker_field != field
            || item_index != assembled.len()
            || chunk_index != 0
            || chunk_total == 0
        {
            return Err(
                "transform continuation marker does not match its array position".to_string(),
            );
        }

        let mut serialized = String::new();
        for expected_chunk in 0..chunk_total {
            let Some(chunk_value) = values.get(index + expected_chunk) else {
                return Err(
                    "transform continuation item ended before all chunks arrived".to_string(),
                );
            };
            let Some(chunk_marker) = chunk_value
                .get(SHADOW_ITEM_CONTINUATION_KEY)
                .and_then(Value::as_object)
            else {
                return Err("transform continuation item was interrupted".to_string());
            };
            let same_item = chunk_marker.get("field").and_then(Value::as_str) == Some(field)
                && chunk_marker.get("item_index").and_then(Value::as_u64)
                    == Some(item_index as u64)
                && chunk_marker.get("chunk_index").and_then(Value::as_u64)
                    == Some(expected_chunk as u64)
                && chunk_marker.get("chunk_total").and_then(Value::as_u64)
                    == Some(chunk_total as u64);
            if !same_item {
                return Err("transform continuation chunks were reordered".to_string());
            }
            let chunk = chunk_value
                .get("chunk")
                .and_then(Value::as_str)
                .ok_or_else(|| "transform continuation marker is missing its chunk".to_string())?;
            serialized.push_str(chunk);
        }
        let item = serde_json::from_str::<Value>(&serialized)
            .map_err(|error| format!("transform continuation item was not valid JSON: {error}"))?;
        assembled.push(item);
        index += chunk_total;
    }
    Ok(assembled)
}

fn assemble_transform_pages(mut pages: Vec<Value>) -> Result<Value, String> {
    let mut final_page = pages
        .pop()
        .ok_or_else(|| "transform page collection was empty".to_string())?;
    if !final_page.is_object() {
        return Err("transform page was not an object".to_string());
    }
    if let Some(object) = final_page.as_object_mut() {
        for field in [
            "transform_page_id",
            "transform_generation",
            "transform_page_index",
            "transform_page_total",
            "transform_page_complete",
            "transform_page_digest",
        ] {
            object.remove(field);
        }
    }
    for field in TRANSFORM_PAGE_ARRAY_FIELDS {
        let had_field = final_page.get(field).is_some();
        let mut values = Vec::new();
        for page in pages.iter().chain(std::iter::once(&final_page)) {
            if let Some(items) = page.get(field).and_then(Value::as_array) {
                values.extend(items.iter().cloned());
            }
        }
        if had_field || !values.is_empty() {
            let values = assemble_transform_page_field(field, values)?;
            let Some(object) = final_page.as_object_mut() else {
                return Err("transform page was not an object".to_string());
            };
            object.insert(field.to_string(), Value::Array(values));
        }
    }
    Ok(final_page)
}

fn assemble_shadow_seed(
    mut batches: Vec<ShadowStateSyncWire>,
    generation: u64,
    expected_seq: u64,
) -> ShadowStateSyncWire {
    let mut final_batch = batches.pop().expect("final seed batch");
    let mut compartments = Vec::new();
    let mut memories = Vec::new();
    let mut memory_mutations = Vec::new();
    let mut drop_seeds = Vec::new();
    let mut user_profile = Vec::new();
    for mut batch in batches {
        compartments.append(&mut batch.compartments);
        memories.append(&mut batch.memories);
        memory_mutations.append(&mut batch.memory_mutations);
        drop_seeds.append(&mut batch.drop_seeds);
        user_profile.append(&mut batch.user_profile);
    }
    compartments.append(&mut final_batch.compartments);
    memories.append(&mut final_batch.memories);
    memory_mutations.append(&mut final_batch.memory_mutations);
    drop_seeds.append(&mut final_batch.drop_seeds);
    user_profile.append(&mut final_batch.user_profile);
    ShadowStateSyncWire {
        session_id: final_batch.session_id,
        shadow_generation: generation,
        expected_shadow_seq: expected_seq,
        seed_id: None,
        seed_generation: None,
        seed_batch_index: None,
        seed_batch_total: None,
        seed_complete: None,
        seed_boundary_id: final_batch.seed_boundary_id,
        compartments,
        memories,
        memory_mutations,
        user_profile,
        workspace: final_batch.workspace,
        last_todo_state: final_batch.last_todo_state,
        acked_watermarks: final_batch.acked_watermarks,
        drop_seeds,
        drop_seed_skipped: final_batch.drop_seed_skipped,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn mcp_text_result(text: String, is_error: bool) -> HandlerOutcome {
    respond(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    }))
}

fn tool_error_result(message: impl Into<String>) -> HandlerOutcome {
    mcp_text_result(message.into(), true)
}

fn session_unresolved_error() -> HandlerOutcome {
    HandlerOutcome::Error {
        code: "session_unresolved".to_string(),
        message: SESSION_UNRESOLVED_MESSAGE.to_string(),
    }
}

fn authority_draining_error(domain: &str) -> HandlerOutcome {
    HandlerOutcome::Error {
        code: "authority_draining".to_string(),
        message: format!("{domain} authority is draining; retry after the ownership transition"),
    }
}

fn store_error_is_authority_draining(error: &impl std::fmt::Display) -> bool {
    error.to_string().contains("authority_draining")
}

fn authority_request_key(request: &Value) -> Option<(&str, &str, &str)> {
    let uuid = request.get("context_store_uuid").and_then(Value::as_str)?;
    let project = request.get("project").and_then(Value::as_str)?;
    let domain = request.get("domain").and_then(Value::as_str)?;
    if uuid.is_empty() || project.is_empty() || domain.is_empty() {
        return None;
    }
    Some((uuid, project, domain))
}

fn invalid_params_error(message: impl Into<String>) -> HandlerOutcome {
    HandlerOutcome::Error {
        code: "invalid_params".to_string(),
        message: message.into(),
    }
}

fn store_unavailable_error() -> HandlerOutcome {
    HandlerOutcome::Error {
        code: "store_unavailable".to_string(),
        message: "store not opened (no HELLO_ACK storage seam yet)".to_string(),
    }
}

const MAX_FACADE_FRAME_BYTES: usize = 1024 * 1024;
/// Transform-class requests carry a session's full message array. The transport
/// frame ceiling is 64 MiB; half that leaves headroom for envelope overhead while
/// still admitting the largest observed live sessions (multi-MiB CK arrays with
/// retained ingress bytes).
const MAX_TRANSFORM_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Minimal probe deserialized from an oversized body ONLY to pick the right byte
/// cap. serde ignores every other field, so this stays cheap relative to a full
/// Value parse of a multi-MiB array.
#[derive(Deserialize)]
struct RequestMethodProbe {
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

impl RequestMethodProbe {
    fn is_transform_class(&self) -> bool {
        let named = |value: &Option<String>, name: &str| value.as_deref() == Some(name);
        named(&self.kind, "transform")
            || named(&self.method, "shadow_transform")
            // Paged shadow seeds stay under the sender's 512 KiB batch budget by
            // design, but a single stored compartment or memory row can exceed the
            // facade cap on its own; the wider cap keeps such rows syncable.
            || named(&self.method, "state_sync")
    }
}

/// The facade byte budget exists for agent tool calls; transform-class requests
/// legitimately carry a session's full message array (multi-MiB on large
/// sessions), so they get the wider cap. Method sniffing on raw bytes avoids
/// parsing multi-MiB JSON just to reject it.
fn enforce_request_byte_cap(body: &[u8]) -> Result<(), HandlerOutcome> {
    if body.len() <= MAX_FACADE_FRAME_BYTES {
        return Ok(());
    }
    let transform_class = serde_json::from_slice::<RequestMethodProbe>(body)
        .map(|probe| probe.is_transform_class())
        .unwrap_or(false);
    if transform_class {
        if body.len() <= MAX_TRANSFORM_FRAME_BYTES {
            return Ok(());
        }
        return Err(invalid_params_error(
            "request body exceeds the 32 MiB transform limit",
        ));
    }
    Err(invalid_params_error("request body exceeds the 1 MiB limit"))
}
const MAX_AGENT_DROPS_COMMAND_ID_BYTES: usize = 128;
const MAX_MEMORY_CONTENT_BYTES: usize = 64 * 1024;
const MAX_NOTE_CONTENT_BYTES: usize = 64 * 1024;
const MAX_SHORT_FIELD_BYTES: usize = 4 * 1024;
const MAX_QUERY_BYTES: usize = 1024;
const MAX_MEMORY_IDS: usize = 100;
const CTX_EXPAND_BYTE_BUDGET: usize = 15_000 * 4;
/// Accepted write categories — the canonical V2 taxonomy, single-sourced from the
/// renderer's category order so the facade and the render path never disagree.
use crate::memory_render::MEMORY_CATEGORY_ORDER as MEMORY_CATEGORIES;

fn validate_string_cap(
    args: &Map<String, Value>,
    key: &str,
    max_bytes: usize,
) -> Result<(), String> {
    if let Some(value) = args.get(key).and_then(Value::as_str) {
        if value.len() > max_bytes {
            return Err(format!("'{key}' exceeds the {max_bytes}-byte limit"));
        }
    }
    Ok(())
}

fn validate_memory_id_arguments(args: &Map<String, Value>) -> Result<(), String> {
    for key in ["id", "target_id"] {
        if let Some(value) = args.get(key) {
            if value.as_i64().is_none_or(|id| id <= 0) {
                return Err(format!("'{key}' must be a positive 64-bit integer"));
            }
        }
    }
    for key in ["ids", "source_ids"] {
        if let Some(value) = args.get(key) {
            let Some(values) = value.as_array() else {
                return Err(format!(
                    "'{key}' must be an array of positive 64-bit integers"
                ));
            };
            if values.len() > MAX_MEMORY_IDS {
                return Err(format!("'{key}' exceeds the {MAX_MEMORY_IDS}-item limit"));
            }
            if values
                .iter()
                .any(|value| value.as_i64().is_none_or(|id| id <= 0))
            {
                return Err(format!(
                    "'{key}' must contain only positive 64-bit integers"
                ));
            }
        }
    }
    if let (Some(target), Some(sources)) = (
        args.get("target_id").and_then(Value::as_i64),
        args.get("source_ids").and_then(Value::as_array),
    ) {
        if sources.iter().any(|source| source.as_i64() == Some(target)) {
            return Err("merge target must not appear in source_ids".to_string());
        }
    }
    Ok(())
}

/// Recover the intended argument object when a model repeats the reduced-call
/// envelope it saw in context. Only unwrap when no real primary field is present,
/// so explicit tool arguments always take precedence.
fn facade_arguments(request: &Value, primary_fields: &[&str]) -> Option<Map<String, Value>> {
    let arguments = request.get("arguments")?.as_object()?;
    if primary_fields
        .iter()
        .any(|field| arguments.contains_key(*field))
        || arguments.get("reduced") != Some(&Value::Bool(true))
    {
        return Some(arguments.clone());
    }
    let Some(summary) = arguments.get("summary").and_then(Value::as_str) else {
        return Some(arguments.clone());
    };
    match serde_json::from_str::<Value>(summary) {
        Ok(Value::Object(unwrapped)) => Some(unwrapped),
        _ => Some(arguments.clone()),
    }
}

fn string_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn non_empty_string_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    string_arg(args, key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn i64_arg(args: &Map<String, Value>, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn usize_arg(args: &Map<String, Value>, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn truncate_expand_output(mut output: String) -> String {
    if output.len() <= CTX_EXPAND_BYTE_BUDGET {
        return output;
    }
    let mut boundary = CTX_EXPAND_BYTE_BUDGET;
    while !output.is_char_boundary(boundary) {
        boundary -= 1;
    }
    output.truncate(boundary);
    output.push_str("\n\n[truncated at the ~15,000-token ctx_expand budget]");
    output
}

fn render_message_expand(row: StoredChunkTranscript, message: i64) -> String {
    let mut lines = vec![
        format!(
            "Message {message} is covered by compartment {} ({}-{}).",
            row.compartment_seq, row.start_ordinal, row.end_ordinal
        ),
        "This Claude Code leg can recover the historian chunk-builder view, not full raw messages; tool calls may be summarized and long text may have been truncated before summarization.".to_string(),
        String::new(),
        format!(
            "### Compartment {} ({}-{})",
            row.compartment_seq, row.start_ordinal, row.end_ordinal
        ),
    ];
    lines.push(row.transcript.unwrap_or_else(|| {
        "[no longer recoverable: transcript bytes could not be decompressed]".to_string()
    }));
    truncate_expand_output(lines.join("\n"))
}

fn render_range_expand(
    start: i64,
    end: i64,
    compartments: &[StoredCompartment],
    transcripts: &[StoredChunkTranscript],
) -> String {
    let mut matching = compartments
        .iter()
        .filter(|comp| comp.end_message >= start && comp.start_message <= end)
        .collect::<Vec<_>>();
    matching.sort_by_key(|comp| comp.sequence);
    if matching.is_empty() {
        return format!(
            "No compacted compartments found in range {start}-{end}. The range may be live tail, outside this session's history, or compacted before transcript capture."
        );
    }
    let mut lines = vec![format!(
        "Messages {start}-{end} from persisted historian chunk transcripts:"
    )];
    for compartment in matching {
        lines.push(String::new());
        lines.push(format!(
            "### Compartment {} ({}-{})",
            compartment.sequence, compartment.start_message, compartment.end_message
        ));
        match transcripts
            .iter()
            .find(|row| row.compartment_seq == compartment.sequence)
        {
            Some(row) => lines.push(row.transcript.clone().unwrap_or_else(|| {
                "[no longer recoverable: transcript bytes could not be decompressed]".to_string()
            })),
            None => lines.push(
                "[no longer recoverable: this compartment transcript was evicted or was not recorded]"
                    .to_string(),
            ),
        }
    }
    truncate_expand_output(lines.join("\n"))
}

fn render_notes(
    session_notes: Vec<StoredNote>,
    smart_notes: Vec<StoredNote>,
    session_total: usize,
    smart_total: usize,
    offset: usize,
    default_sections: bool,
) -> String {
    if session_notes.is_empty() && smart_notes.is_empty() {
        return "## Notes\n\nNo session notes or smart notes.".to_string();
    }
    let format_note = |note: &StoredNote| {
        let status_suffix = if note.status == "active" {
            String::new()
        } else {
            format!(" ({})", note.status)
        };
        let anchor = note
            .anchor_ordinal
            .map(|ordinal| format!(" ↳ @msg {ordinal}"))
            .unwrap_or_default();
        if note.type_name == "smart" {
            let condition = if note.status == "ready" {
                note.ready_reason
                    .as_deref()
                    .or(note.surface_condition.as_deref())
                    .unwrap_or("Condition satisfied")
            } else {
                note.surface_condition
                    .as_deref()
                    .unwrap_or("No condition recorded")
            };
            format!(
                "- **#{}**{}: {}{}\n  {}: {}",
                note.id,
                status_suffix,
                note.content,
                anchor,
                if note.status == "ready" {
                    "Condition met"
                } else {
                    "Condition"
                },
                condition
            )
        } else {
            format!(
                "- **#{}**{}: {}{}",
                note.id, status_suffix, note.content, anchor
            )
        }
    };
    let footer = |total: usize, shown: usize| {
        let remaining = total.saturating_sub(offset.saturating_add(shown));
        (remaining > 0).then(|| {
            format!(
                "Showing {shown} of {total} (newest first) — {remaining} older: ctx_note(action=\"read\", offset={})",
                offset.saturating_add(shown)
            )
        })
    };
    let mut sections = Vec::new();
    if !session_notes.is_empty() {
        let mut section = format!(
            "## Session Notes\n\n{}",
            session_notes
                .iter()
                .map(&format_note)
                .collect::<Vec<_>>()
                .join("\n")
        );
        if let Some(footer) = footer(session_total, session_notes.len()) {
            section.push_str("\n\n");
            section.push_str(&footer);
        }
        sections.push(section);
    }
    if !smart_notes.is_empty() {
        let mut section = format!(
            "{}\n\n{}",
            if default_sections {
                "## 🔔 Ready Smart Notes"
            } else {
                "## Smart Notes"
            },
            smart_notes
                .iter()
                .map(&format_note)
                .collect::<Vec<_>>()
                .join("\n\n")
        );
        if let Some(footer) = footer(smart_total, smart_notes.len()) {
            section.push_str("\n\n");
            section.push_str(&footer);
        }
        sections.push(section);
    }
    let body = sections.join("\n\n");
    let anchor_hint = if body.contains("↳ @msg ") {
        "\n\n↳ @msg N marks the conversation tail when a note was written. To see what led to it: ctx_expand(start=N-x, end=N) (pick x for how far back to look)."
    } else {
        ""
    };
    format!(
        "{body}{anchor_hint}\n\nTo dismiss a stale note: ctx_note(action=\"dismiss\", note_id=N)"
    )
}

// The facade never panics on agent input; an absent or malformed id stays a typed tool error.
fn single_memory_id(args: &Map<String, Value>, action: &str) -> Option<i64> {
    if let Some(id) = i64_arg(args, "id") {
        return Some(id);
    }
    let ids = memory_ids(args, action);
    ids.first().copied().filter(|_| ids.len() == 1)
}

fn memory_ids(args: &Map<String, Value>, _action: &str) -> Vec<i64> {
    let mut ids = Vec::new();
    if let Some(id) = i64_arg(args, "id") {
        ids.push(id);
    }
    if let Some(values) = args.get("ids").and_then(Value::as_array) {
        for value in values {
            if let Some(id) = value.as_i64() {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

fn merge_ids(args: &Map<String, Value>) -> Option<(i64, Vec<i64>)> {
    if let Some(target_id) = i64_arg(args, "target_id") {
        let source_ids = args
            .get("source_ids")
            .and_then(Value::as_array)?
            .iter()
            .filter_map(Value::as_i64)
            .collect::<Vec<_>>();
        if source_ids.is_empty() {
            return None;
        }
        return Some((target_id, dedup_i64s(source_ids)));
    }
    let ids = memory_ids(args, "merge");
    if ids.len() < 2 {
        return None;
    }
    Some((ids[0], ids[1..].to_vec()))
}

fn dedup_i64s(ids: Vec<i64>) -> Vec<i64> {
    let mut seen = HashSet::with_capacity(ids.len());
    ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

fn join_i64s(ids: &[i64]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_tag_range_string(input: &str) -> Result<Vec<u64>, String> {
    const MAX_RANGE_ELEMENTS: u64 = 1000;
    let trimmed = input.replace('§', "").trim().to_string();
    if trimmed.is_empty() {
        return Err("Range string must not be empty".to_string());
    }
    let mut numbers = BTreeSet::new();
    for segment in trimmed.split(',') {
        let part = segment.trim();
        if part.contains('-') {
            let Some((start_raw, end_raw)) = part.split_once('-') else {
                return Err(format!("Invalid range \"{part}\""));
            };
            let start = parse_tag_integer(start_raw.trim())?;
            let end = parse_tag_integer(end_raw.trim())?;
            if start > end {
                return Err(format!(
                    "Invalid range \"{part}\": start ({start}) must be <= end ({end})"
                ));
            }
            // Endpoints are positive and i64-bounded, but keep the size calculation
            // checked so malformed facade input can never wrap before the allocation guard.
            let range_size = end
                .checked_sub(start)
                .and_then(|difference| difference.checked_add(1))
                .ok_or_else(|| format!("Invalid range \"{part}\": size overflow"))?;
            if range_size > MAX_RANGE_ELEMENTS {
                return Err(format!(
                    "Range \"{part}\" exceeds maximum size of {MAX_RANGE_ELEMENTS} elements (got {range_size})"
                ));
            }
            for value in start..=end {
                numbers.insert(value);
            }
        } else {
            numbers.insert(parse_tag_integer(part)?);
        }
    }
    if numbers.len() as u64 > MAX_RANGE_ELEMENTS {
        return Err(format!(
            "Total range size exceeds maximum of {MAX_RANGE_ELEMENTS} elements (got {})",
            numbers.len()
        ));
    }
    Ok(numbers.into_iter().collect())
}

fn parse_tag_integer(raw: &str) -> Result<u64, String> {
    if raw.is_empty() || !raw.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(format!("Invalid integer: \"{raw}\""));
    }
    let value = raw
        .parse::<u64>()
        .map_err(|_| format!("Invalid integer: \"{raw}\""))?;
    if value == 0 || value > i64::MAX as u64 {
        return Err(format!(
            "Invalid integer: \"{raw}\" (tag numbers must be between 1 and {})",
            i64::MAX
        ));
    }
    Ok(value)
}

fn command_id_from_agent_drops_request(request: &Value) -> Result<String, String> {
    let Some(command_id) = request.get("command_id").and_then(Value::as_str) else {
        return Err("'command_id' must be a nonempty string".to_string());
    };
    let command_id = command_id.trim();
    if command_id.is_empty() {
        return Err("'command_id' must be a nonempty string".to_string());
    }
    if command_id.len() > MAX_AGENT_DROPS_COMMAND_ID_BYTES {
        return Err(format!(
            "'command_id' exceeds the {MAX_AGENT_DROPS_COMMAND_ID_BYTES}-byte limit"
        ));
    }
    Ok(command_id.to_string())
}

fn is_shadow_session(session_id: &str) -> bool {
    session_id.starts_with(SHADOW_SESSION_PREFIX)
}

fn shadow_project_path(session_id: &str) -> String {
    session_id.to_string()
}

fn shadow_member_path(session_id: &str, real_project_path: &str) -> String {
    format!(
        "{session_id}:member:{}",
        sha256_hex(real_project_path.as_bytes())
    )
}

fn shadow_source_path(
    source_path: Option<&str>,
    root_path: &str,
    member_paths: &HashMap<String, String>,
    has_workspace: bool,
) -> Result<String, String> {
    let Some(source_path) = source_path else {
        return Ok(root_path.to_string());
    };
    if !has_workspace {
        return Ok(root_path.to_string());
    }
    member_paths
        .get(source_path)
        .cloned()
        .ok_or_else(|| format!("shadow memory project is not a workspace member: {source_path}"))
}

fn authority_source_path(
    source_path: Option<&str>,
    store_project_path: &str,
    member_paths: &HashMap<String, String>,
    has_workspace: bool,
) -> Result<String, String> {
    let Some(source_path) = source_path else {
        return Ok(store_project_path.to_string());
    };
    if !has_workspace {
        // Wire paths are assertions, not alternate keys: accepting a route root or a third
        // identity here would let one atomic state-sync mint rows outside the bound owner.
        return (source_path == store_project_path)
            .then(|| store_project_path.to_string())
            .ok_or_else(|| {
                format!(
                    "authority memory project must equal the resolved project key {store_project_path}: {source_path}"
                )
            });
    }
    member_paths
        .get(source_path)
        .cloned()
        .ok_or_else(|| format!("authority memory project is not a workspace member: {source_path}"))
}

fn prepare_shadow_workspace(
    session_id: &str,
    workspace: Option<ShadowWorkspaceWire>,
) -> Result<(Option<ShadowWorkspaceRow>, HashMap<String, String>), String> {
    let Some(workspace) = workspace else {
        return Ok((None, HashMap::new()));
    };
    let Some(owner) = workspace.members.first() else {
        return Err("shadow workspace must include its owning project first".to_string());
    };
    let share_categories = owner.share_categories.clone();
    if workspace
        .members
        .iter()
        .any(|member| member.share_categories != share_categories)
    {
        return Err("shadow workspace members must carry one consistent share policy".to_string());
    }

    let root_path = shadow_project_path(session_id);
    let mut member_paths = HashMap::new();
    let mut members = Vec::with_capacity(workspace.members.len());
    for (index, member) in workspace.members.into_iter().enumerate() {
        if member.project_path.is_empty() {
            return Err("shadow workspace member project_path must not be empty".to_string());
        }
        let namespaced = if index == 0 {
            root_path.clone()
        } else {
            shadow_member_path(session_id, &member.project_path)
        };
        if !namespaced.starts_with(SHADOW_SESSION_PREFIX) {
            return Err("shadow workspace path escaped the reserved namespace".to_string());
        }
        if member_paths
            .insert(member.project_path.clone(), namespaced.clone())
            .is_some()
        {
            return Err("shadow workspace contains a duplicate member".to_string());
        }
        let display_name = Path::new(&member.project_path)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(&member.project_path)
            .to_string();
        members.push(ShadowWorkspaceMemberRow {
            project_path: namespaced,
            display_name,
            display_path: member.project_path,
        });
    }
    let name = format!(
        "shadow-workspace-{}-{}",
        sha256_hex(session_id.as_bytes()),
        workspace.fingerprint
    );
    Ok((
        Some(ShadowWorkspaceRow {
            name,
            share_categories,
            members,
        }),
        member_paths,
    ))
}

fn prepare_authority_workspace(
    authority_project_path: &str,
    workspace: Option<ShadowWorkspaceWire>,
) -> Result<(Option<ShadowWorkspaceRow>, HashMap<String, String>), String> {
    let Some(workspace) = workspace else {
        return Ok((None, HashMap::new()));
    };
    let Some(owner) = workspace.members.first() else {
        return Err("authority workspace must include its owning project first".to_string());
    };
    let share_categories = owner.share_categories.clone();
    if workspace
        .members
        .iter()
        .any(|member| member.share_categories != share_categories)
    {
        return Err(
            "authority workspace members must carry one consistent share policy".to_string(),
        );
    }

    let mut member_paths = HashMap::new();
    let mut members = Vec::with_capacity(workspace.members.len());
    for (index, member) in workspace.members.into_iter().enumerate() {
        if member.project_path.is_empty() {
            return Err("authority workspace member project_path must not be empty".to_string());
        }
        let stored_path = if index == 0 {
            authority_project_path.to_string()
        } else {
            member.project_path.clone()
        };
        if member_paths
            .insert(member.project_path.clone(), stored_path.clone())
            .is_some()
        {
            return Err("authority workspace contains a duplicate member".to_string());
        }
        let display_name = Path::new(&member.project_path)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(&member.project_path)
            .to_string();
        members.push(ShadowWorkspaceMemberRow {
            project_path: stored_path,
            display_name,
            display_path: member.project_path,
        });
    }
    let name = format!(
        "authority-workspace-{}-{}",
        sha256_hex(authority_project_path.as_bytes()),
        workspace.fingerprint
    );
    Ok((
        Some(ShadowWorkspaceRow {
            name,
            share_categories,
            members,
        }),
        member_paths,
    ))
}

fn shadow_input_messages(
    parsed: &ShadowTransformWire,
) -> Result<Vec<crate::ck_wire::CkIngressMessage>, String> {
    if !parsed.messages.is_empty() {
        return Ok(parsed.messages.clone());
    }
    if parsed.input.is_empty() {
        return Err("shadow_transform requires input or messages".to_string());
    }
    let ordinals = parsed
        .input
        .iter()
        .map(absolute_ordinal)
        .collect::<Result<Vec<_>, _>>()?;
    let decoded = crate::codec::opencode::decode_opencode(&parsed.input);
    if decoded.messages.len() != ordinals.len() {
        return Err("opencode decode changed the message count".to_string());
    }
    Ok(decoded
        .messages
        .into_iter()
        .zip(ordinals)
        .map(|(mut message, ordinal)| {
            message.ordinal = ordinal;
            message.ck.meta.ordinal = Some(ordinal);
            message
        })
        .collect())
}

fn shadow_ts_messages(
    parsed: &ShadowTransformWire,
) -> Result<Vec<crate::ck_wire::CkWireMessage>, String> {
    if !parsed.ts_ck_messages.is_empty() {
        return Ok(parsed.ts_ck_messages.clone());
    }
    let decoded = crate::codec::opencode::decode_opencode(&parsed.ts_output);
    Ok(decoded
        .messages
        .into_iter()
        .map(|message| message.ck)
        .collect())
}

fn absolute_ordinal(value: &Value) -> Result<u64, String> {
    ["absolute_ordinal", "absoluteOrdinal"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
        .or_else(|| {
            let info = value.get("info")?;
            ["absolute_ordinal", "absoluteOrdinal"]
                .iter()
                .find_map(|key| info.get(*key).and_then(Value::as_u64))
        })
        .ok_or_else(|| "shadow input message is missing absolute_ordinal".to_string())
}

fn rs_decision_class(action: &str) -> &'static str {
    match action {
        "HARD" => "hard",
        "SOFT" => "soft",
        "SOFT+" | "PASSTHROUGH" => "defer",
        _ => "error",
    }
}

fn comparable_decision_class(decision: &Value, rust_vocabulary: bool) -> Option<&str> {
    if rust_vocabulary {
        if let Some(action) = decision.get("action").and_then(Value::as_str) {
            return Some(rs_decision_class(action));
        }
    }
    decision.get("class").and_then(Value::as_str)
}

fn compare_shadow_outputs(
    ts_messages: &[crate::ck_wire::CkWireMessage],
    rs_messages: &[crate::ck_wire::CkWireMessage],
    ts_decision: &Value,
    rs_decision: &Value,
    normalizations: &[Value],
    trim_mismatch: Option<&transform::TrimMismatch>,
) -> CompareOutcome {
    let ts_canonical = canonical_messages(ts_messages);
    let rs_canonical = canonical_messages(rs_messages);
    // TypeScript reports decision classes, while Rust reports action names. Map both to
    // one vocabulary so Rust's SOFT+ action and TypeScript's defer class compare equally.
    let decision_mismatch = match (
        comparable_decision_class(ts_decision, false),
        comparable_decision_class(rs_decision, true),
    ) {
        (Some(ts_class), Some(rs_class)) => ts_class != rs_class,
        (Some(_), None) => true,
        _ => false,
    };

    if let Some(trim) = trim_mismatch {
        let first = first_message_hint(ts_messages, rs_messages);
        return CompareOutcome {
            class: "trim-mismatch".to_string(),
            hard: true,
            compared: true,
            first_mid: first.0,
            first_block: first.1,
            first_field: Some(trim.predicate.to_string()),
            ts_prefix: bounded_prefix(&ts_canonical),
            rs_prefix: bounded_prefix(&rs_canonical),
            first_diff_offset: None,
            ts_window: String::new(),
            rs_window: String::new(),
        };
    }

    if ts_canonical == rs_canonical && !decision_mismatch {
        return CompareOutcome {
            class: "identical".to_string(),
            hard: false,
            compared: true,
            first_mid: None,
            first_block: None,
            first_field: None,
            ts_prefix: String::new(),
            rs_prefix: String::new(),
            first_diff_offset: None,
            ts_window: String::new(),
            rs_window: String::new(),
        };
    }

    if decision_mismatch {
        let first = first_message_hint(ts_messages, rs_messages);
        return CompareOutcome {
            class: "decision-mismatch".to_string(),
            hard: true,
            compared: true,
            first_mid: first.0,
            first_block: first.1,
            first_field: Some("class".to_string()),
            ts_prefix: bounded_prefix(&canonical_value(ts_decision)),
            rs_prefix: bounded_prefix(&canonical_value(rs_decision)),
            first_diff_offset: None,
            ts_window: String::new(),
            rs_window: String::new(),
        };
    }

    if synthetic_todo_equivalent(ts_messages, rs_messages) {
        return CompareOutcome {
            class: "synthetic-todo".to_string(),
            hard: false,
            compared: true,
            first_mid: None,
            first_block: None,
            first_field: Some("synthetic_todo_shape".to_string()),
            ts_prefix: bounded_prefix(&ts_canonical),
            rs_prefix: bounded_prefix(&rs_canonical),
            first_diff_offset: None,
            ts_window: String::new(),
            rs_window: String::new(),
        };
    }

    if normalizations.iter().any(|value| {
        value
            .as_str()
            .map(|s| s.contains("agent-drop") || s.contains("agent_drop"))
            .unwrap_or_else(|| {
                value.to_string().contains("agent-drop") || value.to_string().contains("agent_drop")
            })
    }) {
        return CompareOutcome {
            class: "agent-drop".to_string(),
            hard: false,
            compared: true,
            first_mid: None,
            first_block: None,
            first_field: Some("normalization".to_string()),
            ts_prefix: bounded_prefix(&ts_canonical),
            rs_prefix: bounded_prefix(&rs_canonical),
            first_diff_offset: None,
            ts_window: String::new(),
            rs_window: String::new(),
        };
    }

    let diff = first_diff(ts_messages, rs_messages);
    let first_diff_offset = first_diff_byte_offset(&ts_canonical, &rs_canonical)
        .expect("non-identical canonical messages must have a differing byte");
    CompareOutcome {
        class: "byte-mismatch".to_string(),
        hard: true,
        compared: true,
        first_mid: diff.0,
        first_block: diff.1,
        first_field: diff.2,
        ts_prefix: bounded_prefix(&ts_canonical),
        rs_prefix: bounded_prefix(&rs_canonical),
        first_diff_offset: Some(first_diff_offset as u64),
        ts_window: centered_diff_window(&ts_canonical, first_diff_offset),
        rs_window: centered_diff_window(&rs_canonical, first_diff_offset),
    }
}

fn shadow_state_hash(store: &McStore, session_id: &str) -> Result<String, mc_store::McStoreError> {
    let loaded = store.load(session_id)?;
    let value = json!({ "core": loaded.core, "meta": loaded.meta });
    let canonical = canonical_value(&value);
    let digest = Sha256::digest(canonical.as_bytes());
    Ok(format!("{digest:x}"))
}

fn canonical_messages(messages: &[crate::ck_wire::CkWireMessage]) -> String {
    let values = messages
        .iter()
        .map(canonical_message_value)
        .collect::<Vec<_>>();
    canonical_value(&Value::Array(values))
}

fn canonical_message_value(message: &crate::ck_wire::CkWireMessage) -> Value {
    let mut message = message.clone();
    message.mark_modified();
    serde_json::to_value(message).unwrap_or(Value::Null)
}

fn canonical_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(v) => v.to_string(),
        Value::Number(n) => canonical_number(n),
        Value::String(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()),
        Value::Array(values) => {
            let inner = values
                .iter()
                .map(canonical_value)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{inner}]")
        }
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            let inner = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        canonical_value(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
    }
}

fn canonical_number(number: &serde_json::Number) -> String {
    if let Some(i) = number.as_i64() {
        i.to_string()
    } else if let Some(u) = number.as_u64() {
        u.to_string()
    } else if let Some(f) = number.as_f64() {
        if f.fract() == 0.0 {
            format!("{f:.0}")
        } else {
            let mut s = format!("{f}");
            if s.contains('.') {
                while s.ends_with('0') {
                    s.pop();
                }
                if s.ends_with('.') {
                    s.push('0');
                }
            }
            s
        }
    } else {
        number.to_string()
    }
}

fn bounded_prefix(value: &str) -> String {
    value.chars().take(SHADOW_COMPARE_PREFIX_LIMIT).collect()
}

fn first_diff_byte_offset(left: &str, right: &str) -> Option<usize> {
    let shared = left.len().min(right.len());
    left.as_bytes()[..shared]
        .iter()
        .zip(&right.as_bytes()[..shared])
        .position(|(left, right)| left != right)
        .or_else(|| (left.len() != right.len()).then_some(shared))
}

fn centered_diff_window(value: &str, diff_offset: usize) -> String {
    let center = diff_offset.min(value.len());
    let mut start = center.saturating_sub(300);
    while !value.is_char_boundary(start) {
        start = start.saturating_sub(1);
    }
    let mut end = center.saturating_add(900).min(value.len());
    while end < value.len() && !value.is_char_boundary(end) {
        end += 1;
    }
    value[start..end].to_string()
}

fn first_message_hint(
    ts_messages: &[crate::ck_wire::CkWireMessage],
    rs_messages: &[crate::ck_wire::CkWireMessage],
) -> (Option<String>, Option<String>) {
    let index = (0..ts_messages.len().max(rs_messages.len()))
        .find(|idx| ts_messages.get(*idx) != rs_messages.get(*idx))
        .unwrap_or(0);
    let mid = ts_messages
        .get(index)
        .or_else(|| rs_messages.get(index))
        .and_then(|message| message.meta.harness_id.clone());
    (mid, Some(index.to_string()))
}

fn first_differing_block_id(
    ts: &crate::ck_wire::CkWireMessage,
    rs: &crate::ck_wire::CkWireMessage,
    message_index: usize,
) -> Option<String> {
    let block_index = (0..ts.content.len().max(rs.content.len()))
        .find(|index| ts.content.get(*index) != rs.content.get(*index))?;
    let mid = ts
        .meta
        .harness_id
        .as_deref()
        .or(rs.meta.harness_id.as_deref());
    Some(match mid {
        Some(mid) => crate::ck_wire::block_id(mid, block_index),
        None => format!("{message_index}#{block_index}"),
    })
}

fn first_available_block_id(
    message: &crate::ck_wire::CkWireMessage,
    message_index: usize,
) -> Option<String> {
    (!message.content.is_empty()).then(|| match message.meta.harness_id.as_deref() {
        Some(mid) => crate::ck_wire::block_id(mid, 0),
        None => format!("{message_index}#0"),
    })
}

fn first_diff(
    ts_messages: &[crate::ck_wire::CkWireMessage],
    rs_messages: &[crate::ck_wire::CkWireMessage],
) -> (Option<String>, Option<String>, Option<String>) {
    for index in 0..ts_messages.len().max(rs_messages.len()) {
        match (ts_messages.get(index), rs_messages.get(index)) {
            (Some(ts), Some(rs)) => {
                let ts_value = canonical_message_value(ts);
                let rs_value = canonical_message_value(rs);
                if ts_value != rs_value {
                    return (
                        ts.meta
                            .harness_id
                            .clone()
                            .or_else(|| rs.meta.harness_id.clone()),
                        first_differing_block_id(ts, rs, index),
                        first_value_diff(&ts_value, &rs_value, "message"),
                    );
                }
            }
            (Some(ts), None) => {
                return (
                    ts.meta.harness_id.clone(),
                    first_available_block_id(ts, index),
                    Some("missing_rs_message".to_string()),
                )
            }
            (None, Some(rs)) => {
                return (
                    rs.meta.harness_id.clone(),
                    first_available_block_id(rs, index),
                    Some("missing_ts_message".to_string()),
                )
            }
            (None, None) => break,
        }
    }
    (None, None, None)
}

fn first_value_diff(left: &Value, right: &Value, path: &str) -> Option<String> {
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => {
            let mut keys = l.keys().chain(r.keys()).collect::<Vec<_>>();
            keys.sort();
            keys.dedup();
            for key in keys {
                match (l.get(key), r.get(key)) {
                    (Some(a), Some(b)) if a == b => {}
                    (Some(a), Some(b)) => {
                        return first_value_diff(a, b, &format!("{path}.{key}"));
                    }
                    (Some(_), None) | (None, Some(_)) => return Some(format!("{path}.{key}")),
                    (None, None) => {}
                }
            }
            Some(path.to_string())
        }
        (Value::Array(l), Value::Array(r)) => {
            for index in 0..l.len().max(r.len()) {
                match (l.get(index), r.get(index)) {
                    (Some(a), Some(b)) if a == b => {}
                    (Some(a), Some(b)) => {
                        return first_value_diff(a, b, &format!("{path}[{index}]"));
                    }
                    (Some(_), None) | (None, Some(_)) => return Some(format!("{path}[{index}]")),
                    (None, None) => {}
                }
            }
            Some(path.to_string())
        }
        _ => Some(path.to_string()),
    }
}

fn synthetic_todo_equivalent(
    ts_messages: &[crate::ck_wire::CkWireMessage],
    rs_messages: &[crate::ck_wire::CkWireMessage],
) -> bool {
    let (ts_without, ts_todo) = split_synthetic_todo(ts_messages);
    let (rs_without, rs_todo) = split_synthetic_todo(rs_messages);
    !ts_todo.is_empty()
        && !rs_todo.is_empty()
        && canonical_value(&Value::Array(ts_without)) == canonical_value(&Value::Array(rs_without))
        && sorted_strings(ts_todo) == sorted_strings(rs_todo)
}

fn split_synthetic_todo(messages: &[crate::ck_wire::CkWireMessage]) -> (Vec<Value>, Vec<String>) {
    let mut kept = Vec::new();
    let mut todo = Vec::new();
    for message in messages {
        let mut message = message.clone();
        message.mark_modified();
        let mut kept_blocks = Vec::new();
        for block in &message.content {
            let is_todo = match &block.kind {
                crate::ck_wire::CkKind::ToolCall { id, name, .. } => {
                    crate::injection::is_synthetic_todo_id(id) || name == "todowrite"
                }
                crate::ck_wire::CkKind::ToolResult { id, tool_name, .. } => {
                    crate::injection::is_synthetic_todo_id(id) || tool_name == "todowrite"
                }
                _ => false,
            };
            if is_todo {
                todo.push(canonical_value(
                    &serde_json::to_value(block).unwrap_or(Value::Null),
                ));
            } else {
                kept_blocks.push(block.clone());
            }
        }
        if !kept_blocks.is_empty() || message.content.is_empty() {
            message.content = kept_blocks;
            kept.push(serde_json::to_value(message).unwrap_or(Value::Null));
        }
    }
    (kept, todo)
}

fn sorted_strings(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

fn plural_word(count: usize, singular: &'static str) -> String {
    if count == 1 {
        singular.to_string()
    } else {
        format!("{singular}s")
    }
}

fn format_traffic_age(observed_at_ms: i64, now: i64) -> String {
    if observed_at_ms <= 0 {
        return "unknown".to_string();
    }
    let elapsed_seconds = now.saturating_sub(observed_at_ms).max(0) / 1_000;
    if elapsed_seconds < 60 {
        format!("{elapsed_seconds}s ago")
    } else if elapsed_seconds < 60 * 60 {
        format!("{}m ago", elapsed_seconds / 60)
    } else if elapsed_seconds < 24 * 60 * 60 {
        format!("{}h ago", elapsed_seconds / (60 * 60))
    } else {
        format!("{}d ago", elapsed_seconds / (24 * 60 * 60))
    }
}

fn sanitize_status_text(text: &str, limit: usize) -> String {
    let controls_removed = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    controls_removed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

fn compact_status_detail(detail: &str) -> String {
    sanitize_status_text(detail, 120)
}

fn historian_status_summary(state: &mc_store::HistorianDurableState) -> String {
    if state.state != HistorianPhase::Idle {
        return format!("fire seq {} {}", state.firing_seq, state.state.as_str());
    }
    if let Some(reason) = state.last_no_fire.as_deref() {
        return format!("no fire: {}", compact_status_detail(reason));
    }
    if let Some(reason) = state.last_failure.as_deref() {
        return format!("failure: {}", compact_status_detail(reason));
    }
    if state.firing_seq > 0 {
        return format!("published seq {}", state.firing_seq);
    }
    "none".to_string()
}

fn wrapup_has_remaining_messages(
    messages: &[crate::ck_wire::CkIngressMessage],
    last_compartment_end: Option<u64>,
    protected_start: u64,
) -> bool {
    messages.iter().any(|message| {
        !message.ck.meta.synthetic
            && message.ordinal < protected_start
            && last_compartment_end.is_none_or(|end| message.ordinal > end)
    })
}

fn wrapup_boundary_messages(
    parsed: &TransformRequest,
    projection: &crate::ck_wire::FlatProjection,
) -> Vec<BoundaryMsg> {
    parsed
        .messages
        .iter()
        .filter(|message| !message.ck.meta.synthetic)
        .map(|message| BoundaryMsg {
            message_ordinal: message.ordinal,
            message_id: message.mid.clone(),
            role: Role::from_provider(&message.ck.role),
            blocks: projection
                .blocks
                .iter()
                .filter(|block| block.mid == message.mid && !block.synthetic)
                .map(|block| BoundaryBlock {
                    id: block.id.clone(),
                    ordinal: block.ordinal,
                    kind: sel_kind_for_flat(block),
                    provider_executed: block.provider_executed,
                    byte_size: block.bytes.len(),
                    arc_id: block.arc_id.clone(),
                    original: block.bytes.clone(),
                    rendered: None,
                    ignored: false,
                })
                .collect(),
        })
        .collect()
}

fn boundary_messages(
    parsed: &TransformRequest,
    projection: &crate::ck_wire::FlatProjection,
) -> Vec<BoundaryMsg> {
    parsed
        .messages
        .iter()
        .filter(|message| !message.ck.meta.synthetic && message.ck.role != "system")
        .map(|message| BoundaryMsg {
            message_ordinal: message.ordinal,
            message_id: message.mid.clone(),
            role: Role::from_provider(&message.ck.role),
            blocks: projection
                .blocks
                .iter()
                .filter(|block| block.mid == message.mid && !block.synthetic)
                .map(|block| BoundaryBlock {
                    id: block.id.clone(),
                    ordinal: block.ordinal,
                    kind: sel_kind_for_flat(block),
                    provider_executed: block.provider_executed,
                    byte_size: block.bytes.len(),
                    arc_id: block.arc_id.clone(),
                    original: block.bytes.clone(),
                    rendered: None,
                    ignored: false,
                })
                .collect(),
        })
        .collect()
}

fn sel_kind_for_flat(block: &crate::ck_wire::FlatBlock) -> SelKind {
    match block.kind_tag.as_str() {
        "tool_call" => SelKind::ToolCall {
            name: block.name.clone().unwrap_or_default(),
            input: block.tool_input.clone().unwrap_or(Value::Null),
        },
        "tool_result" => SelKind::ToolResult {
            tool_name: block.name.clone().unwrap_or_default(),
        },
        "reasoning" => SelKind::Reasoning,
        "redacted_reasoning" => SelKind::RedactedReasoning,
        "media" => SelKind::Media,
        "opaque" => SelKind::Opaque,
        _ => SelKind::Text,
    }
}

fn usage_numbers(usage: Option<&mc_store::ModuleUsage>) -> (f64, f64, f64) {
    let input = usage
        .map(|u| u.current_total_input_tokens as f64)
        .unwrap_or(0.0);
    let limit = usage
        .map(|u| u.context_limit_tokens as f64)
        .filter(|limit| *limit >= MIN_PLAUSIBLE_CONTEXT_LIMIT as f64)
        .unwrap_or(200_000.0);
    let pct = if limit > 0.0 {
        input / limit * 100.0
    } else {
        0.0
    };
    (limit, input, pct)
}

fn project_slug(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("project")
        .to_string()
}

fn record_historian_connect_failure(
    store: &McStore,
    session_id: &str,
    failure_backoff_at_ms: i64,
    detail: &str,
    before_commit: &ConnectFailureCommitHook,
) -> Result<(), McStoreError> {
    for attempt in 0..2 {
        let loaded = store.load(session_id)?;
        let mut meta = loaded.meta.clone();
        if meta.historian.state == HistorianPhase::Idle {
            // Connection failures happen before the historian transitions out of Idle, but
            // they still need the same durable cooldown as failures after preparation.
            meta.historian.last_failure = Some(detail.to_string());
            meta.historian.failure_backoff_at_ms = Some(failure_backoff_at_ms);
        } else {
            meta.historian = historian::abandon_with_detail(
                &meta.historian,
                failure_backoff_at_ms,
                Some(detail.to_string()),
            );
        }
        if let Some(hook) = before_commit
            .lock()
            .expect("connect failure commit hook mutex")
            .as_mut()
        {
            hook();
        }
        match store.commit(session_id, loaded.row_version, &loaded.core, &meta) {
            Ok(_) => return Ok(()),
            Err(McStoreError::CasConflict { .. }) if attempt == 0 => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!("the connect-failure CAS loop returns from both attempts")
}

/// Resolve the storage descriptor: prefer the daemon-provided `ack.storage`, else
/// fall back to a local dev path (standalone / no managed storage configured).
pub fn resolve_descriptor(storage: Option<&Value>) -> StorageDescriptor {
    if let Some(value) = storage {
        if let Ok(descriptor) = serde_json::from_value::<StorageDescriptor>(value.clone()) {
            return descriptor;
        }
    }
    dev_descriptor()
}

fn dev_descriptor() -> StorageDescriptor {
    let data_home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            format!("{home}/.local/share")
        });
    dev_descriptor_at(&data_home)
}

/// The store descriptor the module uses for a given data-home — the SAME path computation
/// the running module performs from `XDG_DATA_HOME`. Exposed so the acceptance harness can
/// seed the very store the spawned module will open (under its single-writer lease).
pub fn dev_descriptor_at(data_home: &str) -> StorageDescriptor {
    StorageDescriptor {
        module_id: DEFAULT_MODULE_ID.to_string(),
        storage_namespace: STORAGE_NAMESPACE.to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: sqlite_store_path(data_home, DEFAULT_MODULE_ID),
        },
    }
}

fn ctx_memory_description() -> String {
    "Save and maintain durable project memories for facts that should stay useful in later turns. Use write for a new standalone fact, update when an existing memory changed, archive when a memory is wrong or obsolete, and merge when several memories describe the same fact. Keep each memory concise and understandable without this chat's surrounding context.".to_string()
}

fn ctx_search_description() -> String {
    "Keyword-search saved project memories, session notes, and summarized conversation history. This is literal word or phrase search, not semantic search; use it to find remembered facts or prior discussion snippets before answering.".to_string()
}

fn ctx_expand_description() -> String {
    "Recover persisted historian chunk transcripts for compacted conversation ranges. This Claude Code leg serves the chunk-builder U:/A:/TC: view, not full raw message recovery.".to_string()
}

fn ctx_note_description() -> String {
    "Save or inspect durable session notes for future follow-ups. surface_condition is accepted and recorded, but condition evaluation arrives later on this leg.".to_string()
}

fn ctx_memory_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {
                "type": "string",
                "enum": ["write", "update", "archive", "merge", "get"],
                "description": "Operation to perform."
            },
            "category": {
                "type": "string",
                "description": "Memory category for a new memory: one of PROJECT_RULES, ARCHITECTURE, CONSTRAINTS, CONFIG_VALUES, or NAMING. Required for write."
            },
            "content": {
                "type": "string",
                "maxLength": 65536,
                "description": "Standalone memory text. Required for write, update, and merge."
            },
            "id": {
                "type": "integer",
                "minimum": 1,
                "description": "Single memory id for update or archive."
            },
            "ids": {
                "type": "array",
                "maxItems": 100,
                "items": { "type": "integer", "minimum": 1 },
                "description": "Memory ids. For update provide exactly one. For archive provide one or more. For merge, the first id is kept and updated, and the remaining ids are superseded. For get provide one to twenty ids."
            },
            "target_id": {
                "type": "integer",
                "minimum": 1,
                "description": "Merge form: memory id to keep and update."
            },
            "source_ids": {
                "type": "array",
                "maxItems": 100,
                "items": { "type": "integer", "minimum": 1 },
                "description": "Merge form: memory ids to supersede into target_id."
            },
            "reason": {
                "type": "string",
                "maxLength": 4096,
                "description": "Optional short reason for archive."
            },
            "memory_project": {
                "type": "string",
                "description": "Resolved MC project identity supplied by the host transport."
            },
            "reduced": { "type": "boolean" },
            "summary": { "type": "string" }
        }
    })
}

fn ctx_search_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {
                "type": "string",
                "maxLength": 1024,
                "description": "Literal keyword or phrase to find in memories and summarized history."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 25,
                "default": 8,
                "description": "Maximum number of matches to return."
            },
            "reduced": { "type": "boolean" },
            "summary": { "type": "string" }
        }
    })
}

fn ctx_expand_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "start": { "type": "integer", "minimum": 1, "description": "First message ordinal to expand." },
            "end": { "type": "integer", "minimum": 1, "description": "Last message ordinal to expand, inclusive." },
            "message": { "type": "integer", "minimum": 1, "description": "Recover the single persisted chunk transcript covering this message ordinal." },
            "reduced": { "type": "boolean" },
            "summary": { "type": "string" }
        }
    })
}

fn ctx_note_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": { "type": "string", "enum": ["write", "read", "update", "dismiss"], "description": "Operation to perform. Defaults to write when content is provided, otherwise read." },
            "content": { "type": "string", "maxLength": 65536, "description": "Note text for write/update, or optional dismissal resolution when action is dismiss." },
            "note_id": { "type": "integer", "minimum": 1, "description": "Note id for update or dismiss." },
            "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 25, "description": "Maximum active notes to return." },
            "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "Skip this many newest notes in each section." },
            "filter": { "type": "string", "enum": ["all", "active", "pending", "ready", "dismissed"], "description": "Optional read filter. Defaults to active session notes plus ready smart notes." },
            "surface_condition": { "type": "string", "maxLength": 4096, "description": "Optional externally checkable condition to record with the note. Evaluation arrives later." },
            "memory_project": { "type": "string", "description": "Resolved MC project identity supplied by the host transport." },
            "reduced": { "type": "boolean" },
            "summary": { "type": "string" }
        }
    })
}

/// The module manifest registered at HELLO. Slice-1 declares the transform provider
/// role + a project-scoped sqlite storage binding; the surface widens with the spine.
pub fn manifest(module_id: &str) -> ModuleManifest {
    ModuleManifest {
        module_id: module_id.to_string(),
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
            tools: vec![
                Tool {
                    name: "transform".to_string(),
                    description: Some(
                        "Cache-stable context transform: folds compacted history into m0/m1 and applies frozen reductions".to_string(),
                    ),
                    execution_mode: ExecutionMode::Pure,
                    schema: json!({ "type": "object" }),
                },
                Tool {
                    name: "ctx_reduce".to_string(),
                    description: Some(
                        "Acknowledge a tagged reduction request for asynchronous delivery".to_string(),
                    ),
                    execution_mode: ExecutionMode::Pure,
                    schema: json!({
                        "type": "object",
                        "properties": {
                            "drop": { "type": "string" },
                            "reduced": { "type": "boolean" },
                            "summary": { "type": "string" }
                        },
                        "additionalProperties": false
                    }),
                },
                Tool {
                    name: "ctx_memory".to_string(),
                    description: Some(ctx_memory_description()),
                    execution_mode: ExecutionMode::Mutating,
                    schema: ctx_memory_schema(),
                },
                Tool {
                    name: "ctx_search".to_string(),
                    description: Some(ctx_search_description()),
                    execution_mode: ExecutionMode::Pure,
                    schema: ctx_search_schema(),
                },
                Tool {
                    name: "ctx_expand".to_string(),
                    description: Some(ctx_expand_description()),
                    execution_mode: ExecutionMode::Pure,
                    schema: ctx_expand_schema(),
                },
                Tool {
                    name: "ctx_note".to_string(),
                    description: Some(ctx_note_description()),
                    execution_mode: ExecutionMode::Mutating,
                    schema: ctx_note_schema(),
                },
            ],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency: Concurrency::ModuleManaged,
            emits_push: false,
            sub_supervises: false,
        }],
        consumes: vec![ConsumerRole::ServiceClient {
            of: vec!["thalamus".to_string()],
        }],
        scheduled_tasks: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: true,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::ck_wire::{
        CkIngressMessage, CkKind, CkOutputKind, CkToolOutput, CkWireBlock, CkWireMessage,
        HarnessMeta, ProviderExtras,
    };
    use historian_producer::{ProducerOutput, RunHandle, RunState};
    use mc_core::CoreState;
    use mc_store::{
        HistorianChunkRange, HistorianDurableState, ModuleMeta, ModuleUsage, StoredCompartment,
        TagMintInput,
    };
    use tokio::sync::Notify;

    #[test]
    fn usage_numbers_rejects_implausible_context_limit() {
        let tiny = ModuleUsage {
            current_total_input_tokens: 50_000,
            context_limit_tokens: 500,
        };
        let (limit, _, pct) = usage_numbers(Some(&tiny));
        assert_eq!(limit, 200_000.0);
        assert!((pct - 25.0).abs() < 0.01, "pct={pct}");

        let ok = ModuleUsage {
            current_total_input_tokens: 133_000,
            context_limit_tokens: 167_000,
        };
        let (limit, _, pct) = usage_numbers(Some(&ok));
        assert_eq!(limit, 167_000.0);
        assert!((pct - 79.64).abs() < 0.1, "pct={pct}");

        let one_m = ModuleUsage {
            current_total_input_tokens: 800_000,
            context_limit_tokens: 1_000_000,
        };
        let (limit, _, pct) = usage_numbers(Some(&one_m));
        assert_eq!(limit, 1_000_000.0);
        assert!((pct - 80.0).abs() < 0.01, "pct={pct}");
    }

    #[test]
    fn profile_render_epoch_is_profile_specific_and_zero_for_unchanged_profiles() {
        assert_eq!(MEMORY_RENDER_FORMAT_EPOCH, 2);
        assert_eq!(
            profile_render_epoch(SerializerProfile::ClaudeCodeAnthropic),
            PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC
        );
        assert_eq!(profile_render_epoch(SerializerProfile::OwnedLlmRunner), 0);
        assert_eq!(profile_render_epoch(SerializerProfile::Pi), 0);
        assert_eq!(profile_render_epoch(SerializerProfile::OpencodeAiSdk), 0);
    }

    #[test]
    fn dev_descriptor_used_when_ack_has_no_storage() {
        let d = resolve_descriptor(None);
        assert_eq!(d.module_id, DEFAULT_MODULE_ID);
        assert_eq!(d.storage_namespace, STORAGE_NAMESPACE);
        match d.backend {
            StorageBackend::Sqlite { path } => assert!(path.ends_with("store.db")),
            other => panic!("expected sqlite backend, got {other:?}"),
        }
    }

    #[test]
    fn ack_storage_is_preferred_when_present() {
        let provided = StorageDescriptor {
            module_id: "magic-context".to_string(),
            storage_namespace: "mc_cache".to_string(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: "/managed/path/store.db".to_string(),
            },
        };
        let value = serde_json::to_value(&provided).unwrap();
        let resolved = resolve_descriptor(Some(&value));
        match resolved.backend {
            StorageBackend::Sqlite { path } => assert_eq!(path, "/managed/path/store.db"),
            other => panic!("expected sqlite backend, got {other:?}"),
        }
    }

    #[test]
    fn manifest_declares_module_id_and_storage() {
        let m = manifest("magic-context");
        assert_eq!(m.module_id, "magic-context");
        assert_eq!(m.protocol_ver, PROTOCOL_VERSION);
        assert_eq!(
            m.consumes,
            vec![ConsumerRole::ServiceClient {
                of: vec!["thalamus".to_string()]
            }]
        );
        let ProviderRole::ToolProvider { tools, .. } = &m.provides[0] else {
            panic!("magic-context must expose a tool provider role");
        };
        let by_name = tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool))
            .collect::<HashMap<_, _>>();
        for name in ["ctx_memory", "ctx_search"] {
            let tool = by_name
                .get(name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            assert_eq!(
                tool.name, name,
                "mcp.jsonc overrides use the bare tool name"
            );
            assert_eq!(tool.schema["type"], "object");
            assert!(tool.schema["properties"].is_object());
            assert!(tool.description.as_deref().is_some_and(|text| {
                !text.contains("CortexKit") && !text.contains("transform") && !text.contains('§')
            }));
        }
        assert_eq!(
            by_name["ctx_memory"].execution_mode,
            ExecutionMode::Mutating
        );
        assert_eq!(by_name["ctx_search"].execution_mode, ExecutionMode::Pure);
    }

    fn binding(root: &str, session: &str) -> SessionBinding {
        binding_with_harness(root, "mc-module-test", session)
    }

    fn binding_with_harness(root: &str, harness: &str, session: &str) -> SessionBinding {
        SessionBinding {
            project_root: PathBuf::from(root),
            harness: harness.to_string(),
            session: session.to_string(),
            model_key: None,
            config: default_test_config(),
            history_budget_tokens: memory_render::DEFAULT_HISTORY_BUDGET_TOKENS,
        }
    }

    // Diagnostic driver: replays captured module-request JSONs from a dump directory
    // (MC_REPLAY_DIR) through the real dispatch path against a fresh store, printing
    // per-capture outcome and wall time. No-op unless MC_REPLAY_DIR is set.
    //
    // The run doubles as cache-stability evidence for byte-splice consumers moving to
    // full-array apply: the same capture sequence is driven through TWO independent
    // fresh stores and each pass's serialized output is byte-compared across runs
    // (same input + same durable-state lineage must produce identical bytes), and
    // within a run each non-busting pass's output prefix must reproduce the previous
    // pass's output prefix byte-identically (the property the provider prompt cache
    // keys on).
    #[tokio::test]
    async fn replay_module_request_dump() {
        let Ok(dir) = std::env::var("MC_REPLAY_DIR") else {
            return;
        };
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with("-module-req.json"))
            })
            .collect();
        files.sort();

        async fn drive_run(files: &[PathBuf]) -> Vec<(String, String, Option<Value>, u128)> {
            let state = Arc::new(ProducerState::default());
            // Build the handler manually (instead of handler_with_store) so an optional
            // production-store snapshot (MC_REPLAY_STORE=<sqlite3 .backup output>) can be
            // copied into place BEFORE the store opens — the store takes a single-writer
            // lease at open, so a post-open swap is impossible.
            let dir = tempfile::tempdir().unwrap();
            let data_home = dir.path().join("data");
            std::fs::create_dir_all(&data_home).unwrap();
            if let Ok(seed) = std::env::var("MC_REPLAY_STORE") {
                let StorageBackend::Sqlite { path: target } =
                    dev_descriptor_at(data_home.to_str().unwrap()).backend
                else {
                    panic!("replay seed requires the sqlite dev descriptor");
                };
                std::fs::create_dir_all(std::path::Path::new(&target).parent().unwrap()).unwrap();
                std::fs::copy(&seed, &target).unwrap();
            }
            let store =
                Arc::new(McStore::open(&dev_descriptor_at(data_home.to_str().unwrap())).unwrap());
            let handler = McHandler::with_producer_factory_config_resolver(
                Arc::new(TestProducerFactory { state }),
                default_test_config(),
                Arc::new(MissingSessionResolver),
            );
            handler.store.set(Arc::clone(&store)).ok().unwrap();
            let project = dir.path().join("project");
            std::fs::create_dir_all(&project).unwrap();
            let _dir = dir;
            let mut channels: HashMap<String, u16> = HashMap::new();
            let mut next_channel: u16 = 9;
            let mut outcomes = Vec::new();
            for path in files {
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                let raw = std::fs::read_to_string(path).unwrap();
                let value: Value = serde_json::from_str(&raw).unwrap();
                let session = value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or("replay-session")
                    .to_string();
                let channel = *channels.entry(session.clone()).or_insert_with(|| {
                    let ch = next_channel;
                    next_channel += 1;
                    handler.bind_route(ch, binding(project.to_str().unwrap(), &session));
                    ch
                });
                let started = std::time::Instant::now();
                let outcome = handler.dispatch_value(channel, value.clone()).await;
                let ms = started.elapsed().as_millis();
                match outcome {
                    HandlerOutcome::Response(bytes) => {
                        // Optional: dump the raw TransformResponse bytes per pass so a
                        // consumer (e.g. the gateway plan_outcome harness) can consume the
                        // module's EXACT returned bytes (MC_REPLAY_OUT_DIR=<dir>).
                        if let Ok(out_dir) = std::env::var("MC_REPLAY_OUT_DIR") {
                            let _ = std::fs::create_dir_all(&out_dir);
                            let _ = std::fs::write(
                                std::path::Path::new(&out_dir)
                                    .join(format!("{name}.response.json")),
                                &bytes,
                            );
                        }
                        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                        let action = parsed
                            .get("action")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_string();
                        outcomes.push((name, action, Some(parsed), ms));
                    }
                    HandlerOutcome::Error { code, message } => {
                        println!("[replay] {name} ERROR code={code} ms={ms} message={message}");
                        outcomes.push((name, format!("ERROR:{code}"), None, ms));
                    }
                    other => {
                        println!("[replay] {name} OTHER {other:?} ms={ms}");
                        outcomes.push((name, "OTHER".to_string(), None, ms));
                    }
                }
            }
            outcomes
        }

        let run_a = drive_run(&files).await;
        let run_b = drive_run(&files).await;

        let mut determinism_ok = 0usize;
        let mut determinism_bad = Vec::new();
        for (a, b) in run_a.iter().zip(run_b.iter()) {
            let bytes_a = a.3;
            let _ = bytes_a;
            match (&a.2, &b.2) {
                (Some(va), Some(vb)) => {
                    let sa = serde_json::to_string(va.get("ck_messages").unwrap_or(&Value::Null))
                        .unwrap();
                    let sb = serde_json::to_string(vb.get("ck_messages").unwrap_or(&Value::Null))
                        .unwrap();
                    if sa == sb {
                        determinism_ok += 1;
                    } else {
                        determinism_bad.push(a.0.clone());
                    }
                }
                _ => determinism_bad.push(a.0.clone()),
            }
        }

        // Prefix stability within run A: on non-busting passes, the previous output's
        // message sequence must reappear byte-identically as a prefix of this output.
        let mut prefix_ok = 0usize;
        let mut prefix_bad = Vec::new();
        for w in run_a.windows(2) {
            let (prev, cur) = (&w[0], &w[1]);
            let (Some(pv), Some(cv)) = (&prev.2, &cur.2) else {
                continue;
            };
            if cur.1 != "SOFT+" {
                continue;
            }
            let pm = pv
                .get("ck_messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let cm = cv
                .get("ck_messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // The previous pass's synthetic tail anchors (e.g. todo pair) may relocate
            // relative to NEW tail messages; compare the non-synthetic sequence.
            let strip = |arr: &[Value]| -> Vec<String> {
                arr.iter()
                    .filter(|m| {
                        !m.get("ck")
                            .and_then(|c| c.get("meta"))
                            .and_then(|m| m.get("synthetic"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    })
                    .map(|m| serde_json::to_string(m).unwrap())
                    .collect()
            };
            let ps = strip(&pm);
            let cs = strip(&cm);
            if cs.len() >= ps.len() && cs[..ps.len()] == ps[..] {
                prefix_ok += 1;
            } else {
                let diverge = ps
                    .iter()
                    .zip(cs.iter())
                    .position(|(x, y)| x != y)
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "len".to_string());
                prefix_bad.push(format!("{}@{}", cur.0, diverge));
            }
        }

        for (name, action, parsed, ms) in &run_a {
            let n_out = parsed
                .as_ref()
                .and_then(|p| p.get("ck_messages"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            println!("[replay] {name} action={action} out_msgs={n_out} ms={ms}");
        }
        println!(
            "[replay] DETERMINISM cross-run: {determinism_ok}/{} identical, divergent: {:?}",
            run_a.len(),
            determinism_bad
        );
        println!(
            "[replay] PREFIX-STABILITY (SOFT+ passes): {prefix_ok} stable, divergent: {:?}",
            prefix_bad
        );
    }

    #[derive(Clone)]
    enum FakeResolve {
        Hit(String),
        None,
        Timeout,
    }

    #[derive(Default)]
    struct FakeSessionResolver {
        responses: Mutex<HashMap<String, FakeResolve>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeSessionResolver {
        fn with(pairs: &[(&str, FakeResolve)]) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(
                    pairs
                        .iter()
                        .map(|(token, response)| ((*token).to_string(), response.clone()))
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("resolver calls mutex").clone()
        }
    }

    #[async_trait]
    impl SessionResolver for FakeSessionResolver {
        async fn resolve_session(
            &self,
            _project_root: &Path,
            _harness: &str,
            instance_token: &str,
        ) -> Result<Option<ResolvedSession>, SessionResolveError> {
            self.calls
                .lock()
                .expect("resolver calls mutex")
                .push(instance_token.to_string());
            match self
                .responses
                .lock()
                .expect("resolver responses mutex")
                .get(instance_token)
                .cloned()
                .unwrap_or(FakeResolve::None)
            {
                FakeResolve::Hit(session_id) => Ok(Some(ResolvedSession {
                    session_id,
                    last_traffic_ms: 123,
                })),
                FakeResolve::None => Ok(None),
                FakeResolve::Timeout => Err(SessionResolveError::Timeout),
            }
        }
    }

    /// Resolve just the project_root (the binding's identity) for the resolve assertions.
    fn resolved_root(h: &McHandler, channel: u16, session: &str) -> Result<PathBuf, BindingError> {
        h.resolve_binding(channel, session).map(|b| b.project_root)
    }

    #[test]
    fn route_binding_bind_resolve_unbind() {
        let h = McHandler::new();
        h.bind_route(7, binding("/repo/proj", "ses_a"));

        // resolve succeeds when the channel is bound AND the session matches
        assert_eq!(
            resolved_root(&h, 7, "ses_a").unwrap(),
            PathBuf::from("/repo/proj")
        );

        // a teardown removes the binding → a later resolve fails loud (no stale project)
        h.unbind_route(7);
        assert_eq!(resolved_root(&h, 7, "ses_a"), Err(BindingError::Unbound));
    }

    #[test]
    fn resolve_fails_loud_unbound_and_on_session_mismatch() {
        let h = McHandler::new();
        // never bound → Unbound (NEVER a default project, which would be a cross-project read)
        assert_eq!(resolved_root(&h, 3, "ses_x"), Err(BindingError::Unbound));

        h.bind_route(3, binding("/repo/own", "ses_own"));
        // bound, but a request claiming a DIFFERENT session on this channel → SessionMismatch
        assert_eq!(
            resolved_root(&h, 3, "ses_other"),
            Err(BindingError::SessionMismatch)
        );
        // the matching session still resolves
        assert_eq!(
            resolved_root(&h, 3, "ses_own").unwrap(),
            PathBuf::from("/repo/own")
        );
    }

    #[test]
    fn rebind_overwrites_stale_channel_entry() {
        let h = McHandler::new();
        h.bind_route(5, binding("/a", "s1"));
        // a reused channel re-binds to a new session → last write wins (no stale leak)
        h.bind_route(5, binding("/b", "s2"));
        assert_eq!(resolved_root(&h, 5, "s2").unwrap(), PathBuf::from("/b"));
        assert_eq!(
            resolved_root(&h, 5, "s1"),
            Err(BindingError::SessionMismatch)
        );
    }

    #[test]
    fn in_flight_snapshot_entries_are_count_bounded_and_cannot_resurrect() {
        let mut cache = TransformSnapshotCache::new(1024);
        cache.max_in_flight_entries = 8;
        let request = |session_id: &str| {
            let mut request = transform_request(vec![ck("m1", 1, "text")], 1, 200_000);
            request.session_id = session_id.to_string();
            Arc::new(request)
        };
        // Unique failing sessions: begin() without finish_ready, far past the cap.
        let mut first_generation = 0;
        for index in 0..64 {
            let generation = cache.begin(&format!("failed-{index}"));
            if index == 0 {
                first_generation = generation;
            }
        }
        assert!(
            cache.entries.len() <= 8,
            "InFlight entries must stay bounded"
        );
        assert!(cache.in_flight_lru.len() <= 8);
        // Evicted sessions read Missing, and a late finish_ready for an evicted
        // generation cannot resurrect a snapshot.
        assert!(matches!(
            cache.get("failed-0"),
            TransformSnapshotLookup::Missing
        ));
        cache.finish_ready("failed-0", first_generation, request("failed-0"), 0, 16);
        assert!(matches!(
            cache.get("failed-0"),
            TransformSnapshotLookup::Missing
        ));
        // Ready entries do not occupy InFlight slots: completing one frees its slot.
        let generation = cache.begin("completes");
        cache.finish_ready("completes", generation, request("completes"), 0, 16);
        assert!(!cache
            .in_flight_lru
            .iter()
            .any(|candidate| candidate == "completes"));
        assert!(matches!(
            cache.get("completes"),
            TransformSnapshotLookup::Ready(_)
        ));
    }

    #[test]
    fn request_byte_cap_widens_for_transform_class_only() {
        let pad = |method: &str, key: &str, bytes: usize| {
            format!(
                "{{\"{key}\":\"{method}\",\"pad\":\"{}\"}}",
                "x".repeat(bytes)
            )
            .into_bytes()
        };
        // Under the facade cap everything passes without parsing.
        assert!(enforce_request_byte_cap(b"{}").is_ok());
        // Oversized transform-class bodies pass up to the transform cap.
        let two_mib = 2 * 1024 * 1024;
        assert!(enforce_request_byte_cap(&pad("transform", "kind", two_mib)).is_ok());
        assert!(enforce_request_byte_cap(&pad("shadow_transform", "method", two_mib)).is_ok());
        assert!(enforce_request_byte_cap(&pad("state_sync", "method", two_mib)).is_ok());
        // Oversized facade bodies still reject at 1 MiB.
        assert!(enforce_request_byte_cap(&pad("ctx_memory", "method", two_mib)).is_err());
        // Unparseable oversized bodies reject conservatively.
        assert!(enforce_request_byte_cap(&vec![b'x'; two_mib]).is_err());
        // The transform cap itself is still a hard ceiling.
        assert!(
            enforce_request_byte_cap(&pad("transform", "kind", MAX_TRANSFORM_FRAME_BYTES)).is_err()
        );
    }

    #[test]
    fn transform_snapshot_cache_is_generation_safe_and_lru_bounded() {
        let request = |session_id: &str| {
            let mut request = transform_request(vec![ck("m1", 1, "text")], 1, 200_000);
            request.session_id = session_id.to_string();
            Arc::new(request)
        };
        let mut cache = TransformSnapshotCache::new(10);
        assert!(!cache.generation_present_in_flight_or_ready("a", 1));
        let a = cache.begin("a");
        assert!(cache.generation_present_in_flight_or_ready("a", a));
        cache.finish_ready("a", a, request("a"), 1, 5);
        assert!(cache.generation_present_in_flight_or_ready("a", a));
        let b = cache.begin("b");
        cache.finish_ready("b", b, request("b"), 2, 5);
        assert!(matches!(cache.get("a"), TransformSnapshotLookup::Ready(_)));

        let c = cache.begin("c");
        cache.finish_ready("c", c, request("c"), 3, 5);
        assert!(matches!(cache.get("b"), TransformSnapshotLookup::Missing));
        assert!(matches!(cache.get("a"), TransformSnapshotLookup::Ready(_)));
        assert!(matches!(cache.get("c"), TransformSnapshotLookup::Ready(_)));
        assert_eq!(cache.ready_bytes, 10);

        let stale = cache.begin("a");
        let current = cache.begin("a");
        cache.finish_ready("a", stale, request("stale"), 4, 1);
        assert!(matches!(cache.get("a"), TransformSnapshotLookup::InFlight));
        cache.finish_ready("a", current, request("a"), 5, 4);
        assert!(cache.ready_generation_matches("a", current));
        assert!(cache.ready_bytes <= cache.max_ready_bytes);

        let oversized = cache.begin("oversized");
        cache.finish_ready("oversized", oversized, request("oversized"), 0, 11);
        assert!(matches!(
            cache.get("oversized"),
            TransformSnapshotLookup::Missing
        ));
    }

    #[test]
    fn snapshot_lease_budget_survives_cache_churn_and_releases_exact_charge() {
        let request = |session_id: &str| {
            let mut request = transform_request(vec![ck("m1", 1, "text")], 1, 200_000);
            request.session_id = session_id.to_string();
            Arc::new(request)
        };
        let mut cache = TransformSnapshotCache::new(10);
        {
            let mut budget = cache
                .active_leases
                .lock()
                .expect("snapshot lease budget mutex");
            budget.max_bytes = 10;
            budget.max_count = 1;
        }
        let a = cache.begin("a");
        cache.finish_ready("a", a, request("a"), 0, 5);
        let b = cache.begin("b");
        cache.finish_ready("b", b, request("b"), 0, 5);
        let lease = match cache.get("a") {
            TransformSnapshotLookup::Ready(lease) => lease,
            _ => panic!("first lease must fit"),
        };

        // Churning the map can evict the map entry, but the leased Arc remains charged
        // until its guard drops and neither accounting domain may exceed its own bound.
        let c = cache.begin("c");
        cache.finish_ready("c", c, request("c"), 0, 6);
        assert!(cache.ready_bytes <= cache.max_ready_bytes);
        assert!(matches!(
            cache.get("c"),
            TransformSnapshotLookup::LeaseBudgetExceeded
        ));
        {
            let budget = cache
                .active_leases
                .lock()
                .expect("snapshot lease budget mutex");
            assert_eq!((budget.count, budget.bytes), (1, 5));
            assert!(budget.count <= budget.max_count);
            assert!(budget.bytes <= budget.max_bytes);
        }
        assert_eq!(lease.request.session_id, "a");
        drop(lease);
        {
            let budget = cache
                .active_leases
                .lock()
                .expect("snapshot lease budget mutex");
            assert_eq!((budget.count, budget.bytes), (0, 0));
        }
        assert!(matches!(cache.get("c"), TransformSnapshotLookup::Ready(_)));
    }

    #[test]
    fn snapshot_lease_budget_rejects_second_lease_on_bytes_alone() {
        let request = |session_id: &str| {
            let mut request = transform_request(vec![ck("m1", 1, "text")], 1, 200_000);
            request.session_id = session_id.to_string();
            Arc::new(request)
        };
        let mut cache = TransformSnapshotCache::new(16);
        {
            let mut budget = cache
                .active_leases
                .lock()
                .expect("snapshot lease budget mutex");
            budget.max_bytes = 9;
            budget.max_count = 8;
        }
        let first_generation = cache.begin("first");
        cache.finish_ready("first", first_generation, request("first"), 0, 5);
        let second_generation = cache.begin("second");
        cache.finish_ready("second", second_generation, request("second"), 0, 5);

        let first = match cache.get("first") {
            TransformSnapshotLookup::Ready(lease) => lease,
            _ => panic!("first lease must fit within both budgets"),
        };
        assert!(matches!(
            cache.get("second"),
            TransformSnapshotLookup::LeaseBudgetExceeded
        ));
        {
            let budget = cache
                .active_leases
                .lock()
                .expect("snapshot lease budget mutex");
            assert_eq!(budget.count, 1);
            assert!(
                budget.count < budget.max_count,
                "the count cap must not decide this case"
            );
            assert_eq!(budget.bytes, 5);
        }

        drop(first);
        assert!(matches!(
            cache.get("second"),
            TransformSnapshotLookup::Ready(_)
        ));
    }

    #[derive(Default)]
    struct ProducerState {
        connects: AtomicUsize,
        starts: AtomicUsize,
        start_errors: Mutex<VecDeque<Result<RunHandle, HistorianProducerError>>>,
        binds: AtomicUsize,
        statuses: AtomicUsize,
        await_outputs: AtomicUsize,
        block_output: std::sync::atomic::AtomicBool,
        notify: Notify,
        connect_errors: Mutex<VecDeque<HistorianProducerError>>,
        await_results: Mutex<VecDeque<Result<ProducerOutput, HistorianProducerError>>>,
        outputs: Mutex<VecDeque<String>>,
        next_fact: Mutex<Option<String>>,
        prompts: Mutex<Vec<String>>,
        on_await_output: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    struct TestProducerFactory {
        state: Arc<ProducerState>,
    }

    #[async_trait]
    impl HistorianProducerFactory for TestProducerFactory {
        async fn connect(
            &self,
            _project_root: &Path,
        ) -> Result<Box<dyn HistorianProducerDriver + Send>, HistorianProducerError> {
            self.state.connects.fetch_add(1, Ordering::SeqCst);
            if let Some(err) = self
                .state
                .connect_errors
                .lock()
                .expect("connect errors mutex")
                .pop_front()
            {
                return Err(err);
            }
            Ok(Box::new(TestProducer {
                state: Arc::clone(&self.state),
            }))
        }
    }

    struct TestProducer {
        state: Arc<ProducerState>,
    }

    #[async_trait]
    impl HistorianProducerDriver for TestProducer {
        async fn bind_session(&mut self, _session_id: &str) -> Result<(), HistorianProducerError> {
            self.state.binds.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn start(
            &mut self,
            _session_id: &str,
            _system: &str,
            prompt: &str,
            _model: &str,
        ) -> Result<RunHandle, HistorianProducerError> {
            let n = self.state.starts.fetch_add(1, Ordering::SeqCst) + 1;
            self.state
                .prompts
                .lock()
                .expect("prompts mutex")
                .push(prompt.to_string());
            if let Some(result) = self
                .state
                .start_errors
                .lock()
                .expect("start errors mutex")
                .pop_front()
            {
                return result;
            }
            let output = match self.state.next_fact.lock().expect("next fact mutex").take() {
                Some(fact) => {
                    let (start, end) = prompt_ordinal_range(prompt).unwrap_or((1, 3));
                    historian_output_with_fact(start, end, &fact)
                }
                None => historian_output_for_prompt(prompt),
            };
            self.state
                .outputs
                .lock()
                .expect("outputs mutex")
                .push_back(output);
            Ok(RunHandle {
                run_id: format!("run-{n}"),
            })
        }

        async fn await_output(
            &mut self,
            _run_id: &str,
        ) -> Result<ProducerOutput, HistorianProducerError> {
            self.state.await_outputs.fetch_add(1, Ordering::SeqCst);
            if let Some(hook) = self
                .state
                .on_await_output
                .lock()
                .expect("await-output hook mutex")
                .take()
            {
                hook();
            }
            while self.state.block_output.load(Ordering::SeqCst) {
                self.state.notify.notified().await;
            }
            if let Some(result) = self
                .state
                .await_results
                .lock()
                .expect("await results mutex")
                .pop_front()
            {
                return result;
            }
            let text = self
                .state
                .outputs
                .lock()
                .expect("outputs mutex")
                .pop_front()
                .unwrap_or_else(|| historian_output(1, 3, "reattached summary"));
            Ok(ProducerOutput {
                text,
                length_capped: false,
            })
        }

        async fn status(&mut self, _run_id: &str) -> Result<RunState, HistorianProducerError> {
            self.state.statuses.fetch_add(1, Ordering::SeqCst);
            Ok(RunState::Active)
        }

        async fn cancel(&mut self, _run_id: &str) -> Result<(), HistorianProducerError> {
            Ok(())
        }

        async fn close(&mut self) {}
    }

    fn handler_with_store(
        state: Arc<ProducerState>,
        config: McModuleConfig,
    ) -> (McHandler, Arc<McStore>, tempfile::TempDir, PathBuf) {
        handler_with_store_and_resolver(state, config, Arc::new(MissingSessionResolver))
    }

    fn handler_with_store_and_resolver(
        state: Arc<ProducerState>,
        config: McModuleConfig,
        resolver: Arc<dyn SessionResolver>,
    ) -> (McHandler, Arc<McStore>, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let data_home = dir.path().join("data");
        std::fs::create_dir_all(&data_home).unwrap();
        let store =
            Arc::new(McStore::open(&dev_descriptor_at(data_home.to_str().unwrap())).unwrap());
        let handler = McHandler::with_producer_factory_config_resolver(
            Arc::new(TestProducerFactory { state }),
            config,
            resolver,
        );
        handler.store.set(Arc::clone(&store)).ok().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        handler.bind_route(7, binding(project.to_str().unwrap(), "ses"));
        (handler, store, dir, project)
    }

    fn default_test_config() -> McModuleConfig {
        McModuleConfig {
            model_chain: vec!["test/model".to_string()],
            execute_threshold_percentage: 65.0,
            memory_enabled: true,
            smart_drops: false,
            cache_ttl: "5m".to_string(),
            shadow_enabled: true,
        }
    }

    fn ck_with_role(mid: &str, ordinal: u64, role: &str, text: &str) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                role,
                vec![CkWireBlock::bare(CkKind::Text { text: text.into() })],
                None,
                ProviderExtras::new(),
                HarnessMeta {
                    harness_id: Some(mid.to_string()),
                    ..Default::default()
                },
            ),
        }
    }

    fn ck(mid: &str, ordinal: u64, text: &str) -> CkIngressMessage {
        ck_with_role(mid, ordinal, "user", text)
    }

    fn ck_reasoning(mid: &str, ordinal: u64, text: &str) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "assistant",
                vec![CkWireBlock::bare(CkKind::Reasoning {
                    text: text.to_string(),
                    signature: Some(format!("signature-{mid}")),
                })],
                None,
                ProviderExtras::new(),
                HarnessMeta {
                    harness_id: Some(mid.to_string()),
                    ..Default::default()
                },
            ),
        }
    }

    fn assistant_tool_call(mid: &str, ordinal: u64) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "assistant",
                vec![CkWireBlock::bare(CkKind::ToolCall {
                    id: if mid.starts_with("call-") {
                        mid.to_string()
                    } else {
                        format!("call-{mid}")
                    },
                    name: "bash".to_string(),
                    input: json!({ "command": "printf output" }),
                    provider_executed: false,
                })],
                None,
                ProviderExtras::new(),
                HarnessMeta {
                    harness_id: Some(mid.to_string()),
                    ..Default::default()
                },
            ),
        }
    }

    fn tool_result(mid: &str, ordinal: u64, text: &str) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "tool",
                vec![CkWireBlock::bare(CkKind::ToolResult {
                    id: mid
                        .strip_prefix("result-")
                        .map(|suffix| format!("call-{suffix}"))
                        .unwrap_or_else(|| format!("call-{mid}")),
                    tool_name: "bash".to_string(),
                    output: CkToolOutput::bare(CkOutputKind::Text {
                        text: text.to_string(),
                    }),
                    provider_executed: false,
                })],
                None,
                ProviderExtras::new(),
                HarnessMeta {
                    harness_id: Some(mid.to_string()),
                    ..Default::default()
                },
            ),
        }
    }

    fn stored_comp(seq: i64, start: i64, end: i64, end_mid: &str, p1: &str) -> StoredCompartment {
        StoredCompartment {
            sequence: seq,
            start_message: start,
            end_message: end,
            end_message_id: format!("{end_mid}#0"),
            title: format!("C{seq}"),
            content: p1.to_string(),
            p1: Some(p1.to_string()),
            importance: 50,
            ..Default::default()
        }
    }

    fn imported_compartment(
        seq: i64,
        start_message: i64,
        end_message: i64,
        end_message_id: &str,
        p1: &str,
    ) -> Value {
        json!({
            "seq": seq,
            "start_message": start_message,
            "end_message": end_message,
            "end_message_id": end_message_id,
            "title": format!("Imported {seq}"),
            "p1": p1,
        })
    }

    fn state_import_request(
        import_id: &str,
        batch_seq: usize,
        batch_count: usize,
        compartments: Vec<Value>,
    ) -> Value {
        json!({
            "kind": "state_import",
            "v": 1,
            "session_id": "ses",
            "import_id": import_id,
            "batch_seq": batch_seq,
            "batch_count": batch_count,
            "compartments": compartments,
        })
    }

    fn big_messages_from(start_ordinal: u64) -> Vec<CkIngressMessage> {
        (0..80)
            .map(|idx| {
                let ordinal = start_ordinal + idx;
                ck(
                    &format!("m{ordinal}"),
                    ordinal,
                    &format!("message {ordinal} {}", "word ".repeat(800)),
                )
            })
            .collect()
    }

    fn big_messages() -> Vec<CkIngressMessage> {
        big_messages_from(1)
    }

    fn zero_based_messages_with_system_lead() -> Vec<CkIngressMessage> {
        let mut messages = vec![ck_with_role("m0", 0, "system", "identity lead")];
        messages.extend((1..=80).map(|ordinal| {
            ck(
                &format!("m{ordinal}"),
                ordinal,
                &format!("message {ordinal} {}", "word ".repeat(800)),
            )
        }));
        messages
    }

    fn request_with_usage(
        messages: Vec<CkIngressMessage>,
        current_total_input_tokens: u64,
        context_limit_tokens: u64,
    ) -> Value {
        json!({
            "kind": "transform",
            "v": 2,
            "serializer_profile": "owned-llmrunner",
            "session_id": "ses",
            "render_config": "cfg0",
            "usage": ModuleUsage {
                current_total_input_tokens,
                context_limit_tokens,
            },
            "messages": messages,
        })
    }

    fn request(messages: Vec<CkIngressMessage>) -> Value {
        request_with_usage(messages, 45_000, 50_000)
    }

    fn transform_request(
        messages: Vec<CkIngressMessage>,
        current_total_input_tokens: u64,
        context_limit_tokens: u64,
    ) -> TransformRequest {
        serde_json::from_value(request_with_usage(
            messages,
            current_total_input_tokens,
            context_limit_tokens,
        ))
        .unwrap()
    }

    async fn call_transform_request(handler: &McHandler, request: Value) -> Value {
        call_transform_request_on_channel(handler, 7, request).await
    }

    async fn call_transform_request_on_channel(
        handler: &McHandler,
        channel: u16,
        request: Value,
    ) -> Value {
        match handler.handle_transform_for_test(channel, request).await {
            HandlerOutcome::Response(bytes) => serde_json::from_slice(&bytes).unwrap(),
            other => panic!("unexpected handler outcome: {other:?}"),
        }
    }

    async fn call_transform_outcome(handler: &McHandler, request: Value) -> HandlerOutcome {
        handler.handle_transform_for_test(7, request).await
    }

    async fn call_dispatch_request(handler: &McHandler, request: Value) -> Value {
        match handler.dispatch_value(7, request).await {
            HandlerOutcome::Response(bytes) => serde_json::from_slice(&bytes).unwrap(),
            other => panic!("unexpected handler outcome: {other:?}"),
        }
    }

    fn error_frame(outcome: HandlerOutcome) -> (String, String) {
        match outcome {
            HandlerOutcome::Error { code, message } => (code, message),
            other => panic!("expected error outcome, got {other:?}"),
        }
    }

    fn error_code(outcome: HandlerOutcome) -> String {
        error_frame(outcome).0
    }

    async fn call_facade(handler: &McHandler, name: &str, arguments: Value) -> HandlerOutcome {
        call_facade_on_channel(handler, 7, name, arguments).await
    }

    async fn call_facade_on_channel(
        handler: &McHandler,
        channel: u16,
        name: &str,
        arguments: Value,
    ) -> HandlerOutcome {
        handler
            .dispatch_value(channel, json!({ "name": name, "arguments": arguments }))
            .await
    }

    fn tool_body(outcome: HandlerOutcome) -> Value {
        match outcome {
            HandlerOutcome::Response(bytes) => serde_json::from_slice(&bytes).unwrap(),
            other => panic!("expected tool response, got {other:?}"),
        }
    }

    fn tool_is_error(outcome: HandlerOutcome) -> bool {
        tool_body(outcome)["isError"].as_bool().unwrap_or(false)
    }

    fn tool_text(outcome: HandlerOutcome) -> String {
        tool_body(outcome)["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn tool_json_array(outcome: HandlerOutcome) -> Vec<Value> {
        let body = tool_body(outcome);
        let text = body["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool response missing text: {body}"));
        serde_json::from_str(text).unwrap_or_else(|error| panic!("tool text was not JSON: {error}"))
    }

    fn insert_memory(
        store: &McStore,
        project: &str,
        category: &str,
        content: &str,
        now: i64,
    ) -> i64 {
        store
            .insert_memory(InsertMemoryInput {
                project_path: project,
                route_project_root: None,
                category,
                content,
                source_session_id: Some(project),
                source_type: Some("test"),
                importance: Some(50),
                expires_at: None,
                metadata_json: None,
                now_ms: now,
            })
            .unwrap()
    }

    fn activate_module_authority(
        store: &McStore,
        context_store_uuid: &str,
        identity: &str,
        route_project_root: &str,
        domain: &str,
    ) {
        let preparing = store
            .authority_begin_prepare(context_store_uuid, identity, domain)
            .unwrap();
        let checksum = store
            .authority_seed_checksum(context_store_uuid, identity, domain)
            .unwrap();
        store
            .authority_verify_prepare(
                context_store_uuid,
                identity,
                domain,
                preparing.generation,
                &checksum,
                &checksum,
            )
            .unwrap();
        let module = store
            .authority_ack_prepare(context_store_uuid, identity, domain, preparing.generation)
            .unwrap();
        assert_eq!(module.state, "MODULE");
        store
            .bind_authority_route(context_store_uuid, identity, route_project_root)
            .unwrap();
    }

    fn seed_workspace(store: &McStore, own: &str, foreign: &str) {
        store
            .seed_workspace_member("ws", own, "[\"CONSTRAINTS\"]")
            .unwrap();
        store
            .seed_workspace_member("ws", foreign, "[\"CONSTRAINTS\"]")
            .unwrap();
    }

    fn synthetic_text(response: &Value, index: usize) -> String {
        response["ck_messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["meta"]["synthetic"] == json!(true))
            .nth(index)
            .and_then(|message| message["content"][0]["kind"]["text"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    async fn call_transform(handler: &McHandler, messages: Vec<CkIngressMessage>) -> Value {
        call_transform_request(handler, request(messages)).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serve_native_false_is_response_byte_identical_for_all_profiles() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());
        for profile in [
            "owned-llmrunner",
            "owned-broca",
            "claude-code-anthropic",
            "opencode-aisdk",
            "pi",
        ] {
            let session_id = format!("{}serve-native-false", historian::MC_CHILD_SESSION_PREFIX);
            let mut absent = request(vec![ck("m1", 1, "hello")]);
            absent["session_id"] = json!(session_id);
            absent["serializer_profile"] = json!(profile);
            let mut explicit_false = absent.clone();
            explicit_false["serve_native"] = json!(false);
            let absent_response = call_transform_request(&handler, absent).await;
            let explicit_response = call_transform_request(&handler, explicit_false).await;
            assert_eq!(
                serde_json::to_vec(&absent_response).unwrap(),
                serde_json::to_vec(&explicit_response).unwrap(),
                "profile {profile} changed the legacy response when serve_native=false"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serve_native_rejects_non_opencode_profiles() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());
        let mut request = request(vec![ck("m1", 1, "hello")]);
        request["serve_native"] = json!(true);
        let outcome = call_transform_outcome(&handler, request).await;
        assert_eq!(error_code(outcome), "serve_native_unsupported_profile");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serve_native_adds_opencode_messages_without_changing_ck_response() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());
        let mut request = request(vec![ck("m1", 1, "hello")]);
        request["serializer_profile"] = json!("opencode-aisdk");
        request["serve_native"] = json!(true);
        request["native_messages"] = json!([
            {
                "info": {
                    "id": "m1",
                    "sessionID": "ses",
                    "role": "user",
                    "customInfo": "preserve-me"
                },
                "parts": [{ "type": "text", "text": "hello", "customPart": 7 }]
            }
        ]);

        let first = call_transform_request(&handler, request.clone()).await;
        assert_eq!(first["status"], "ok");
        assert_eq!(
            first["native_messages"].as_array().unwrap().last().unwrap(),
            &request["native_messages"][0]
        );
        assert!(first["native_messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| { message["parts"][0]["synthetic"] == json!(true) }));
        assert!(first.get("ck_messages").is_some());

        let second = call_transform_request(&handler, request).await;
        assert_eq!(second["status"], "ok");
        assert_eq!(second["action"], "SOFT+");
        assert_eq!(
            second["native_messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["info"]["customInfo"],
            "preserve-me"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn live_shaped_opencode_reasoning_clear_attaches_on_the_same_pass() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let mut request = request(vec![
            ck_reasoning("assistant-old", 1, "signed historical thinking"),
            ck("user-new", 100, "new prompt"),
        ]);
        request["serializer_profile"] = json!("opencode-aisdk");
        request["serve_native"] = json!(true);
        // This is the current built-plugin wire shape: provider_id and clear_reasoning_age
        // are absent, so the module must use native OpenCode metadata as its fallback.
        request["native_messages"] = json!([
            {
                "info": {
                    "id": "assistant-old",
                    "role": "assistant",
                    "providerID": "anthropic",
                    "modelID": "claude-opus-4-8"
                },
                "parts": [{
                    "type": "reasoning",
                    "text": "signed historical thinking",
                    "time": { "start": 1, "end": 2 },
                    "metadata": { "anthropic": { "signature": "signature-assistant-old" } }
                }]
            },
            {
                "info": {
                    "id": "user-new",
                    "role": "user",
                    "model": { "providerID": "anthropic", "modelID": "claude-opus-4-8" }
                },
                "parts": [{ "type": "text", "text": "new prompt" }]
            }
        ]);

        let parsed: TransformRequest = serde_json::from_value(request.clone()).unwrap();
        assert_eq!(
            SerializerProfile::parse(&parsed.serializer_profile),
            Some(SerializerProfile::OpencodeAiSdk)
        );
        assert!(parsed.provider_id.is_none());
        assert_eq!(parsed.clear_reasoning_age, 50);
        assert!(transform::request_accepts_empty_content(&parsed));

        let response = call_transform_request(&handler, request).await;
        assert_eq!(response["status"], "ok");
        assert_eq!(
            store
                .load("ses")
                .unwrap()
                .meta
                .reasoning_cleared_through_ordinal,
            50,
            "the attach load must observe the watermark committed by this pass"
        );
        let old = response["native_messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["info"]["id"] == json!("assistant-old"))
            .expect("served assistant must retain its harness id");
        assert_eq!(
            old["parts"][0],
            json!({ "type": "reasoning", "text": "" }),
            "clear preserves the typed reasoning shell while dropping signed metadata"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn v2_wire_echoes_fingerprint_on_normal_and_child_passthrough() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());

        let mut normal_req = request_with_usage(vec![ck("m1", 1, "hello")], 1, 100);
        normal_req["full_array_fingerprint"] = json!("fp-normal");
        let normal = call_transform_request(&handler, normal_req).await;
        assert_eq!(normal["status"], "ok");
        assert_eq!(normal["served_from"], "transform");
        assert_eq!(normal["full_array_fingerprint"], "fp-normal");

        let child_session = format!("{}child", historian::MC_CHILD_SESSION_PREFIX);
        let child = call_transform_request(
            &handler,
            json!({
                "kind": "transform",
                "v": 2,
                "serializer_profile": "owned-llmrunner",
                "session_id": child_session,
                "render_config": "cfg0",
                "full_array_fingerprint": "fp-child",
                "messages": [ck("child-msg", 1, "raw child prompt")],
            }),
        )
        .await;
        assert_eq!(child["status"], "ok");
        assert_eq!(child["served_from"], "transform");
        assert_eq!(child["full_array_fingerprint"], "fp-child");
        assert_eq!(child["action"], "PASSTHROUGH");
        assert_eq!(child["ck_messages"].as_array().unwrap().len(), 1);
        assert_eq!(child["ck_messages"][0]["role"], "user");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn channel2_host_directive_is_deterministic_due_gated_and_profile_scoped() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());
        let output = "tool output ".repeat(8_000);
        let mut messages = Vec::new();
        for index in 0..25u64 {
            let call_mid = format!("call-{index}");
            let result_mid = format!("result-{index}");
            messages.push(assistant_tool_call(&call_mid, index * 2 + 1));
            messages.push(tool_result(&result_mid, index * 2 + 2, &output));
        }
        let opencode_request = json!({
            "kind": "transform",
            "v": 2,
            "serializer_profile": "opencode-aisdk",
            "session_id": "ses",
            "render_config": "cfg0",
            "usage": { "current_total_input_tokens": 90_000, "context_limit_tokens": 100_000 },
            "messages": messages,
        });
        let first = call_transform_request(&handler, opencode_request.clone()).await;
        let first_text = first["host_directives"]["channel2_nudge"]["text"]
            .as_str()
            .expect("due OpenCode pass must carry channel2 text");
        assert!(first_text.contains("Routine context housekeeping is near"));
        let second = call_transform_request(&handler, opencode_request).await;
        assert_eq!(
            second["host_directives"]["channel2_nudge"]["text"],
            first_text
        );

        let mut not_due = request_with_usage(vec![ck("short", 1, "small")], 10_000, 100_000);
        not_due["serializer_profile"] = json!("opencode-aisdk");
        let not_due_response = call_transform_request(&handler, not_due).await;
        assert!(not_due_response.get("host_directives").is_none());

        handler.bind_route(8, binding("/tmp/cc", "cc-ses"));
        let mut cc_request = json!({
            "kind": "transform",
            "v": 2,
            "serializer_profile": "claude-code-anthropic",
            "tool_present": true,
            "session_id": "cc-ses",
            "render_config": "cfg0",
            "usage": { "current_total_input_tokens": 90_000, "context_limit_tokens": 100_000 },
            "messages": first["ck_messages"],
        });
        // The CC response is used only to prove the additive directive remains profile-gated;
        // its transformed messages are not a request fixture for the OpenCode lane.
        cc_request["messages"] = json!([ck("cc-short", 1, "small")]);
        let cc_response = call_transform_request_on_channel(&handler, 8, cc_request).await;
        assert!(cc_response.get("host_directives").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transform_reject_records_trace_without_advancing_row_version() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let loaded = store.load("ses").unwrap();
        let seeded_row_version = store
            .commit("ses", loaded.row_version, &loaded.core, &loaded.meta)
            .unwrap();

        let (code, message) = error_frame(
            call_transform_outcome(
                &handler,
                request(vec![ck("m2", 2, "two"), ck("m1", 1, "one")]),
            )
            .await,
        );
        assert_eq!(code, "transform_failed");
        assert_eq!(message, "live-source ordinals not strictly increasing");

        let after = store.load("ses").unwrap();
        assert_eq!(after.row_version, Some(seeded_row_version));
        let trace = store.load_pass_trace("ses").unwrap().unwrap();
        assert_eq!(trace.receive_count, 1);
        assert_eq!(trace.reject_count, 1);
        assert_eq!(trace.last_reject_error.as_deref(), Some(message.as_str()));
        assert_eq!(trace.last_completed_at_ms, 0);
        assert!(trace.last_received_at_ms > 0);
        assert!(trace.last_reject_at_ms.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transform_success_records_received_and_completed_trace() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());

        let response = call_transform(&handler, vec![ck("m1", 1, "hello")]).await;
        assert_eq!(response["status"], "ok");

        let trace = store.load_pass_trace("ses").unwrap().unwrap();
        assert_eq!(trace.receive_count, 1);
        assert_eq!(trace.reject_count, 0);
        assert_eq!(trace.last_reject_error, None);
        assert_eq!(trace.last_reject_at_ms, None);
        assert!(trace.last_completed_at_ms >= trace.last_received_at_ms);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_rejects_increment_trace_and_overwrite_last_error() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());

        let _ = error_frame(
            call_transform_outcome(
                &handler,
                request(vec![ck("m2", 2, "two"), ck("m1", 1, "one")]),
            )
            .await,
        );
        let first = store.load_pass_trace("ses").unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;

        let (code, message) = error_frame(
            call_transform_outcome(&handler, request(vec![ck("mc_bad", 1, "reserved")])).await,
        );
        assert_eq!(code, "transform_failed");
        assert_eq!(message, "non-synthetic item used a reserved mc_* id");

        let second = store.load_pass_trace("ses").unwrap().unwrap();
        assert_eq!(second.receive_count, 2);
        assert_eq!(second.reject_count, 2);
        assert_eq!(second.last_reject_error.as_deref(), Some(message.as_str()));
        assert!(second.last_reject_at_ms.unwrap() >= first.last_reject_at_ms.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sequential_failing_passes_trace_every_reject_while_cache_state_stays_frozen() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let loaded = store.load("ses").unwrap();
        let seeded_row_version = store
            .commit("ses", loaded.row_version, &loaded.core, &loaded.meta)
            .unwrap();

        for _ in 0..4 {
            let (code, message) = error_frame(
                call_transform_outcome(
                    &handler,
                    request(vec![ck("m2", 2, "two"), ck("m1", 1, "one")]),
                )
                .await,
            );
            assert_eq!(code, "transform_failed");
            assert_eq!(message, "live-source ordinals not strictly increasing");
        }

        let after = store.load("ses").unwrap();
        assert_eq!(after.row_version, Some(seeded_row_version));
        let trace = store.load_pass_trace("ses").unwrap().unwrap();
        assert_eq!(trace.receive_count, 4);
        assert_eq!(trace.reject_count, 4);
        assert_eq!(trace.last_completed_at_ms, 0);
        assert_eq!(
            trace.last_reject_error.as_deref(),
            Some("live-source ordinals not strictly increasing")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn status_and_health_surface_pass_trace_for_rejected_sessions() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());

        let _ = error_frame(
            call_transform_outcome(
                &handler,
                request(vec![ck("m2", 2, "two"), ck("m1", 1, "one")]),
            )
            .await,
        );

        let status =
            call_dispatch_request(&handler, json!({ "kind": "status", "session_id": "ses" })).await;
        assert_eq!(status["ok"], true);
        assert_eq!(status["store_open"], true);
        assert_eq!(status["session_id"], "ses");
        assert_eq!(status["row_version"], Value::Null);
        assert_eq!(status["historian"]["last_no_fire"], Value::Null);
        assert_eq!(PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC, 1);
        assert_eq!(
            status["epochs"]["memory_render_epoch"],
            json!(MEMORY_RENDER_FORMAT_EPOCH)
        );
        assert_eq!(
            status["epochs"]["compartment_render_epoch"],
            json!(COMPARTMENT_RENDER_FORMAT_EPOCH)
        );
        assert_eq!(status["epochs"]["profile_epoch"], json!(1));
        assert_eq!(
            status["epochs"]["tagger_epoch"],
            json!(TAGGER_FEATURE_EPOCH)
        );
        assert_eq!(status["pass_trace"]["receive_count"], 1);
        assert_eq!(status["pass_trace"]["reject_count"], 1);
        assert_eq!(
            status["pass_trace"]["last_reject_error"],
            json!("live-source ordinals not strictly increasing")
        );

        let health =
            call_dispatch_request(&handler, json!({ "kind": "health", "session_id": "ses" })).await;
        assert_eq!(health["pass_trace"], status["pass_trace"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn guidance_variant_no_reduce_omits_reduce_and_hashes_differ() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());
        let full = call_dispatch_request(
            &handler,
            json!({ "kind": "guidance.get", "session_id": "ses", "tool_present": true }),
        )
        .await;
        let trimmed = call_dispatch_request(
            &handler,
            json!({ "kind": "guidance.get", "session_id": "ses", "variant": "no_reduce" }),
        )
        .await;
        let full_bytes = full["bytes"].as_str().unwrap();
        let trimmed_bytes = trimmed["bytes"].as_str().unwrap();
        assert!(full_bytes.contains("ctx_reduce"));
        assert!(!trimmed_bytes.contains("ctx_reduce"));
        assert!(!trimmed_bytes.contains("\u{a7}")); // no tag-sigil references
        assert!(trimmed_bytes.contains("ctx_memory"));
        assert!(trimmed_bytes.contains("ctx_expand"));
        assert_ne!(full["content_hash"], trimmed["content_hash"]);
        // Both variants share the session's frozen date line.
        let date = full_bytes.lines().last().unwrap();
        assert_eq!(date, trimmed_bytes.lines().last().unwrap());
        let unknown = handler
            .dispatch_value(
                7,
                json!({ "kind": "guidance.get", "session_id": "ses", "variant": "bogus" }),
            )
            .await;
        assert_eq!(error_code(unknown), "bad_request");
        let contradictory = handler
            .dispatch_value(
                7,
                json!({
                    "kind": "guidance.get",
                    "session_id": "ses",
                    "tool_present": false,
                    "variant": "full"
                }),
            )
            .await;
        assert_eq!(error_code(contradictory), "bad_request");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn guidance_language_directive_is_present_in_both_serializer_variants() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());
        for request in [
            json!({
                "kind": "guidance.get",
                "session_id": "ses",
                "serializer_profile": "claude-code-anthropic",
                "tool_present": true,
                "language": "tr",
            }),
            json!({
                "kind": "guidance.get",
                "session_id": "ses",
                "serializer_profile": "opencode-aisdk",
                "language": "tr",
            }),
        ] {
            let response = call_dispatch_request(&handler, request).await;
            let bytes = response["bytes"].as_str().unwrap();
            assert!(bytes.contains("Use Turkish (Türkçe) for your natural-language replies"));
            assert_eq!(response["hash"], json!(sha256_hex(bytes.as_bytes())));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn guidance_get_freezes_hashes_and_advances_only_on_busting_commit() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        handler.guidance_dates.lock().unwrap().insert(
            "ses".to_string(),
            "Today's date: Fri Jan 01 2016".to_string(),
        );

        let first = call_dispatch_request(
            &handler,
            json!({ "kind": "guidance.get", "session_id": "ses", "tool_present": true }),
        )
        .await;
        let first_bytes = first["bytes"].as_str().unwrap().to_string();
        assert!(first_bytes.ends_with("Today's date: Fri Jan 01 2016"));
        assert_eq!(
            first["content_hash"],
            json!(sha256_hex(GUIDANCE_TEXT.as_bytes()))
        );
        assert_eq!(
            first["hash"],
            json!(sha256_hex(first_bytes.as_bytes())),
            "hash covers exact returned bytes"
        );
        let repeated = call_dispatch_request(
            &handler,
            json!({ "kind": "guidance.get", "session_id": "ses", "tool_present": true }),
        )
        .await;
        assert_eq!(repeated["bytes"], first["bytes"]);
        assert_eq!(repeated["hash"], first["hash"]);

        let still_frozen = call_dispatch_request(
            &handler,
            json!({ "kind": "guidance.get", "session_id": "ses", "tool_present": true }),
        )
        .await;
        assert_eq!(still_frozen["bytes"], first["bytes"]);

        handler.guidance_dates.lock().unwrap().insert(
            "ses".to_string(),
            "Today's date: Sat Jan 02 2016".to_string(),
        );
        let _ = call_transform(&handler, vec![ck("m1", 1, "hello")]).await;
        let advanced = call_dispatch_request(
            &handler,
            json!({ "kind": "guidance.get", "session_id": "ses", "tool_present": true }),
        )
        .await;
        assert!(advanced["bytes"]
            .as_str()
            .unwrap()
            .ends_with("Today's date: Sat Jan 02 2016"));
        assert_ne!(advanced["hash"], first["hash"]);
        assert_eq!(advanced["content_hash"], first["content_hash"]);

        handler.bind_route(8, binding("/tmp/other", "other"));
        handler.guidance_dates.lock().unwrap().insert(
            "other".to_string(),
            "Today's date: Sun Jan 03 2016".to_string(),
        );
        let other = match handler
            .dispatch_value(
                8,
                json!({ "kind": "guidance.get", "session_id": "other", "tool_present": true }),
            )
            .await
        {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert_ne!(other["hash"], advanced["hash"]);
        assert_eq!(other["content_hash"], advanced["content_hash"]);
        assert!(store.load("other").unwrap().row_version.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ctx_expand_and_ctx_note_facades_are_session_scoped() {
        let resolver = FakeSessionResolver::with(&[("ses", FakeResolve::Hit("ses".to_string()))]);
        let (handler, store, _dir, project) = handler_with_store_and_resolver(
            Arc::new(ProducerState::default()),
            default_test_config(),
            resolver,
        );
        let meta = ModuleMeta {
            historian: HistorianDurableState {
                state: HistorianPhase::Publishing,
                firing_seq: 7,
                chunk_range: Some(HistorianChunkRange {
                    from_ordinal: 10,
                    to_ordinal: 12,
                }),
                chunk_fingerprint: "fp".to_string(),
                producer_session_id: Some("producer".to_string()),
                producer_run_id: Some("run".to_string()),
                fired_at_ms: Some(1),
                expected_revert_epoch: 0,
                failure_backoff_at_ms: None,
                last_failure: None,
                last_no_fire: None,
            },
            ..Default::default()
        };
        store
            .commit("ses", None, &CoreState::default(), &meta)
            .unwrap();
        store
            .publish_historian_chunk(mc_store::HistorianPublishRequest {
                session_id: "ses",
                expected_row_version: Some(1),
                expected_revert_epoch: 0,
                predicate: &mc_store::HistorianPublishPredicate {
                    firing_seq: 7,
                    producer_run_id: "run".to_string(),
                    chunk_fingerprint: "fp".to_string(),
                },
                project_path: project.to_str().unwrap(),
                compartments: &[stored_comp(1, 10, 12, "m12#0", "summary")],
                facts: &[],
                publication_floor_ordinal: 12,
                chunk_transcript: Some("U: exact prompt text\nA: exact answer"),
            })
            .unwrap();

        let expanded =
            tool_text(call_facade(&handler, "ctx_expand", json!({"start": 10, "end": 12})).await);
        assert!(expanded.contains("Compartment 1 (10-12)"));
        assert!(expanded.contains("U: exact prompt text"));
        let message = tool_text(call_facade(&handler, "ctx_expand", json!({"message": 11})).await);
        assert!(message.contains("chunk-builder view"));

        let write = tool_text(
            call_facade(
                &handler,
                "ctx_note",
                json!({"action": "write", "content": "remember the lattice", "surface_condition": "when tag v2 exists"}),
            )
            .await,
        );
        assert!(write.contains("Created smart note"));
        let write = tool_text(
            call_facade(
                &handler,
                "ctx_note",
                json!({"action": "write", "content": "remember the lattice"}),
            )
            .await,
        );
        assert!(write.contains("Saved session note"));
        let read = tool_text(call_facade(&handler, "ctx_note", json!({"action": "read"})).await);
        assert!(read.contains("remember the lattice"));
        let hits =
            tool_json_array(call_facade(&handler, "ctx_search", json!({"query": "lattice"})).await);
        assert!(hits.iter().any(|hit| hit["source"] == "note"));

        let note_id = store
            .search_notes_like(project.to_str().unwrap(), "ses", "lattice")
            .unwrap()[0]
            .id;
        let _ = call_facade(
            &handler,
            "ctx_note",
            json!({"action": "update", "note_id": note_id, "content": "remember the updated lattice"}),
        )
        .await;
        let _ = call_facade(
            &handler,
            "ctx_note",
            json!({"action": "dismiss", "note_id": note_id, "content": "finished"}),
        )
        .await;
        let dismissed = store
            .search_notes_like(project.to_str().unwrap(), "ses", "finished")
            .unwrap();
        assert_eq!(dismissed[0].status, "dismissed");
        assert!(store
            .search_notes_like("/different/project", "ses", "lattice")
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opencode_facade_uses_bound_session_without_session_resolver() {
        let resolver = FakeSessionResolver::with(&[]);
        let (handler, store, _dir, project) = handler_with_store_and_resolver(
            Arc::new(ProducerState::default()),
            default_test_config(),
            resolver.clone(),
        );
        let project_root = project.to_str().unwrap();
        handler.bind_route(
            7,
            binding_with_harness(project_root, OPENCODE_HARNESS, "opencode-session"),
        );
        handler.transform_route_channels.lock().unwrap().insert(
            7,
            ("opencode-session".to_string(), canonical_root(project_root)),
        );
        handler
            .transform_session_roots
            .lock()
            .unwrap()
            .entry("opencode-session".to_string())
            .or_default()
            .insert(canonical_root(project_root));

        let memory = call_facade(
            &handler,
            "ctx_memory",
            json!({
                "action": "write",
                "category": "CONSTRAINTS",
                "content": "OpenCode route identity is durable",
                "memory_project": project_root,
            }),
        )
        .await;
        assert!(!tool_is_error(memory));

        let note = call_facade(
            &handler,
            "ctx_note",
            json!({
                "action": "write",
                "content": "OpenCode route identity is durable",
                "memory_project": project_root,
            }),
        )
        .await;
        assert!(!tool_is_error(note));
        assert!(resolver.calls().is_empty());
        assert_eq!(
            store
                .search_notes_like(project_root, "opencode-session", "route identity")
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn opencode_facade_lineage_matches_transform_across_symlink_spellings() {
        use std::os::unix::fs::symlink;

        let (handler, _store, dir, project) = handler_with_store_and_resolver(
            Arc::new(ProducerState::default()),
            default_test_config(),
            Arc::new(MissingSessionResolver),
        );
        let link = dir.path().join("project-link");
        symlink(&project, &link).unwrap();
        let target_text = project.to_str().unwrap();
        let link_text = link.to_str().unwrap();

        // The transform lane binds through the symlink spelling, while the facade lane binds to
        // the canonical target. Both route bindings identify the same filesystem lineage.
        handler.bind_route(7, binding_with_harness(link_text, OPENCODE_HARNESS, "ses"));
        let transformed =
            call_transform_request_on_channel(&handler, 7, request(vec![ck("m0", 0, "a")])).await;
        assert_eq!(transformed["action"], "HARD");
        handler.bind_route(
            8,
            binding_with_harness(target_text, OPENCODE_HARNESS, "ses"),
        );

        let outcome = call_facade_on_channel(
            &handler,
            8,
            "ctx_memory",
            json!({
                "action": "write",
                "category": "CONSTRAINTS",
                "content": "symlink lineage resolves",
                "memory_project": target_text,
            }),
        )
        .await;
        assert!(!tool_is_error(outcome));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn opencode_facade_lineage_matches_transform_in_reverse_symlink_direction() {
        use std::os::unix::fs::symlink;

        let (handler, _store, dir, project) = handler_with_store_and_resolver(
            Arc::new(ProducerState::default()),
            default_test_config(),
            Arc::new(MissingSessionResolver),
        );
        let link = dir.path().join("project-link");
        symlink(&project, &link).unwrap();
        let target_text = project.to_str().unwrap();
        let link_text = link.to_str().unwrap();

        // Reverse the lane spellings: transform uses the target and facade uses the symlink.
        handler.bind_route(
            7,
            binding_with_harness(target_text, OPENCODE_HARNESS, "ses"),
        );
        let transformed =
            call_transform_request_on_channel(&handler, 7, request(vec![ck("m0", 0, "a")])).await;
        assert_eq!(transformed["action"], "HARD");
        handler.bind_route(8, binding_with_harness(link_text, OPENCODE_HARNESS, "ses"));

        let outcome = call_facade_on_channel(
            &handler,
            8,
            "ctx_memory",
            json!({
                "action": "write",
                "category": "CONSTRAINTS",
                "content": "reverse symlink lineage resolves",
                "memory_project": link_text,
            }),
        )
        .await;
        assert!(!tool_is_error(outcome));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn claimed_opencode_harness_cannot_bypass_resolution_for_unknown_session() {
        let resolver = FakeSessionResolver::with(&[("wrapper-instance", FakeResolve::None)]);
        let (handler, store, _dir, project) = handler_with_store_and_resolver(
            Arc::new(ProducerState::default()),
            default_test_config(),
            resolver.clone(),
        );
        handler.bind_route(
            7,
            binding_with_harness(
                project.to_str().unwrap(),
                OPENCODE_HARNESS,
                "wrapper-instance",
            ),
        );

        let outcome = call_facade(
            &handler,
            "ctx_note",
            json!({ "action": "write", "content": "must not be token keyed" }),
        )
        .await;
        assert_eq!(error_code(outcome), "session_unresolved");
        assert_eq!(resolver.calls(), vec!["wrapper-instance"]);
        assert!(store
            .search_notes_like(project.to_str().unwrap(), "wrapper-instance", "token keyed")
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opencode_cache_provenance_cannot_rebind_a_second_project_root() {
        let resolver = FakeSessionResolver::with(&[("ses", FakeResolve::None)]);
        let (handler, store, _dir, project) = handler_with_store_and_resolver(
            Arc::new(ProducerState::default()),
            default_test_config(),
            resolver.clone(),
        );
        let root_a = project.to_str().unwrap();
        handler.bind_route(7, binding_with_harness(root_a, OPENCODE_HARNESS, "ses"));
        let transformed =
            call_transform_request_on_channel(&handler, 7, request(vec![ck("m0", 0, "a")])).await;
        assert_eq!(transformed["action"], "HARD");
        activate_module_authority(&store, "context", "git:identity", root_a, "memories");

        let root_b = project.join("other-root");
        std::fs::create_dir_all(&root_b).unwrap();
        let root_b = root_b.to_str().unwrap();
        handler.bind_route(8, binding_with_harness(root_b, OPENCODE_HARNESS, "ses"));
        let outcome = call_facade_on_channel(
            &handler,
            8,
            "ctx_memory",
            json!({
                "action": "write",
                "category": "CONSTRAINTS",
                "content": "must not cross roots",
                "memory_project": "git:identity",
            }),
        )
        .await;
        assert_eq!(error_code(outcome), "session_unresolved");
        assert_eq!(resolver.calls(), vec!["ses"]);
        assert_eq!(
            store
                .authority_project_for_route(root_b, "memories")
                .unwrap(),
            None
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opencode_transform_root_lineage_survives_a_real_handler_restart() {
        let dir = tempfile::tempdir().unwrap();
        let data_home = dir.path().join("data");
        std::fs::create_dir_all(&data_home).unwrap();
        let root_a = dir.path().join("project-a");
        let root_b = dir.path().join("project-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let descriptor = dev_descriptor_at(data_home.to_str().unwrap());
        let root_a_text = root_a.to_str().unwrap();
        let identity = "git:restart-lineage";

        {
            let store = Arc::new(McStore::open(&descriptor).unwrap());
            let handler = McHandler::with_producer_factory_config_resolver(
                Arc::new(TestProducerFactory {
                    state: Arc::new(ProducerState::default()),
                }),
                default_test_config(),
                Arc::new(MissingSessionResolver),
            );
            handler.store.set(Arc::clone(&store)).ok().unwrap();
            handler.bind_route(
                7,
                binding_with_harness(root_a_text, OPENCODE_HARNESS, "ses"),
            );
            let transformed =
                call_transform_request_on_channel(&handler, 7, request(vec![ck("m0", 0, "a")]))
                    .await;
            assert_eq!(transformed["action"], "HARD");
            assert!(store
                .knows_transform_session_root("ses", root_a_text)
                .unwrap());
            for domain in ["memories", "notes"] {
                activate_module_authority(&store, "context", identity, root_a_text, domain);
            }
        }

        let resolver = FakeSessionResolver::with(&[("ses", FakeResolve::None)]);
        let store = Arc::new(McStore::open(&descriptor).unwrap());
        let handler = McHandler::with_producer_factory_config_resolver(
            Arc::new(TestProducerFactory {
                state: Arc::new(ProducerState::default()),
            }),
            default_test_config(),
            resolver.clone(),
        );
        handler.store.set(Arc::clone(&store)).ok().unwrap();
        handler.bind_route(
            7,
            binding_with_harness(root_a_text, OPENCODE_HARNESS, "ses"),
        );

        let memory = call_facade(
            &handler,
            "ctx_memory",
            json!({
                "action": "write",
                "category": "CONSTRAINTS",
                "content": "durable root proof",
                "memory_project": identity,
            }),
        )
        .await;
        assert!(!tool_is_error(memory));
        let note = call_facade(
            &handler,
            "ctx_note",
            json!({
                "action": "write",
                "content": "durable note proof",
                "memory_project": identity,
            }),
        )
        .await;
        assert!(!tool_is_error(note));
        assert!(resolver.calls().is_empty());

        handler.bind_route(
            8,
            binding_with_harness(root_b.to_str().unwrap(), OPENCODE_HARNESS, "ses"),
        );
        let cross_root = call_facade_on_channel(
            &handler,
            8,
            "ctx_memory",
            json!({
                "action": "write",
                "category": "CONSTRAINTS",
                "content": "must not cross roots",
                "memory_project": identity,
            }),
        )
        .await;
        assert_eq!(error_code(cross_root), "session_unresolved");
        assert_eq!(resolver.calls(), vec!["ses"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn claude_code_facade_resolves_instance_token_before_store_access() {
        let resolver = FakeSessionResolver::with(&[(
            "claude-instance-token",
            FakeResolve::Hit("claude-conversation".to_string()),
        )]);
        let (handler, store, _dir, _project) = handler_with_store_and_resolver(
            Arc::new(ProducerState::default()),
            default_test_config(),
            resolver.clone(),
        );
        handler.bind_route(
            7,
            binding_with_harness("/repo", "claude-code", "claude-instance-token"),
        );

        let note = call_facade(
            &handler,
            "ctx_note",
            json!({
                "action": "write",
                "content": "Claude Code keeps resolver semantics",
            }),
        )
        .await;
        assert!(!tool_is_error(note));
        assert_eq!(resolver.calls(), vec!["claude-instance-token"]);
        assert_eq!(
            store
                .search_notes_like("/repo", "claude-conversation", "resolver semantics")
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn note_evaluator_verdict_transitions_a_module_note_to_ready_and_visible() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, _project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(7, binding("/repo", "token"));
        let note = store
            .insert_project_note(NoteWriteInput {
                project_path: "/repo",
                route_project_root: None,
                session_id: Some("session"),
                content: "surface after evaluation",
                surface_condition: Some("when ready"),
                anchor_block_id: None,
                anchor_ordinal: None,
                now_ms: 1,
            })
            .unwrap();

        let evaluated = call_dispatch_request(
            &handler,
            json!({
                "method": "note.evaluate",
                "session_id": "session",
                "note_id": note.id,
                "source_revision": note.status_version,
                "verdict": true
            }),
        )
        .await;
        assert_eq!(evaluated["status"], "ready");
        let output = tool_text(call_facade(&handler, "ctx_note", json!({"action": "read"})).await);
        assert!(output.contains("surface after evaluation"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authority_note_lifecycle_resolves_identity_for_evaluate_render_and_ack() {
        let resolver = FakeSessionResolver::with(&[("token", FakeResolve::Hit("ses".to_string()))]);
        let (handler, store, _dir, _project) = handler_with_store_and_resolver(
            Arc::new(ProducerState::default()),
            default_test_config(),
            resolver,
        );
        handler.bind_route(7, binding("/repo", "token"));
        handler.bind_route(8, binding_with_harness("/repo", OPENCODE_HARNESS, "ses"));
        activate_module_authority(&store, "context", "git:identity", "/repo", "notes");
        let note = store
            .insert_project_note(NoteWriteInput {
                project_path: "git:identity",
                route_project_root: Some("/repo"),
                session_id: Some("ses"),
                content: "identity note lifecycle",
                surface_condition: Some("when ready"),
                anchor_block_id: None,
                anchor_ordinal: None,
                now_ms: 1,
            })
            .unwrap();

        let evaluated = handler
            .dispatch_value(
                7,
                json!({
                    "method": "note.evaluate",
                    "session_id": "ses",
                    "note_id": note.id,
                    "source_revision": note.status_version,
                    "verdict": true
                }),
            )
            .await;
        assert!(matches!(evaluated, HandlerOutcome::Response(_)));

        let rendered = call_transform_request_on_channel(
            &handler,
            8,
            request(vec![ck("m0", 0, "live input")]),
        )
        .await;
        assert!(synthetic_text(&rendered, 1).contains("identity note lifecycle"));
        let pass_id = rendered["note_deliveries"][0]["transform_pass_id"]
            .as_str()
            .unwrap();
        let ack = handler
            .dispatch_value(
                8,
                json!({
                    "method": "transform.ack",
                    "session_id": "ses",
                    "transform_pass_id": pass_id
                }),
            )
            .await;
        assert!(matches!(ack, HandlerOutcome::Response(_)));
        assert_eq!(
            store
                .get_note_by_id("git:identity", "ses", note.id)
                .unwrap()
                .unwrap()
                .status,
            "surfaced"
        );
        assert!(store
            .search_notes_like("/repo", "ses", "identity note lifecycle")
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn note_facade_reads_a_preexisting_seeded_ts_note_after_authority_flip() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, _project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(7, binding("/repo", "token"));
        store
            .seed_authority_row(
                "context-db",
                "notes",
                42,
                &json!({
                    "type": "smart",
                    "project_path": "/repo",
                    "session_id": "session",
                    "content": "seeded before Rust mode",
                    "status": "ready",
                    "surface_condition": "condition",
                    "ready_reason": "condition met",
                    "status_version": 2,
                    "created_at": 1,
                    "updated_at": 2
                }),
            )
            .unwrap();

        let output = tool_text(call_facade(&handler, "ctx_note", json!({"action": "read"})).await);
        assert!(output.contains("## 🔔 Ready Smart Notes"));
        assert!(output.contains("seeded before Rust mode"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn note_facade_pages_ready_notes_beyond_one_hundred_with_shared_offset_semantics() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, _project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(7, binding("/repo", "token"));
        for index in 0..105 {
            let note = store
                .insert_project_note(NoteWriteInput {
                    project_path: "/repo",
                    route_project_root: None,
                    session_id: Some("session"),
                    content: &format!("ready note {index}"),
                    surface_condition: Some("condition"),
                    anchor_block_id: None,
                    anchor_ordinal: None,
                    now_ms: index,
                })
                .unwrap();
            store
                .write_note_evaluation(NoteEvaluationInput {
                    project_path: "/repo",
                    note_id: note.id,
                    source_revision: note.status_version,
                    verdict: true,
                    compiled_check: None,
                    manifest_json: None,
                    check_hash: None,
                    next_due_at: None,
                    now_ms: index,
                })
                .unwrap();
        }
        let page = tool_text(
            call_facade(
                &handler,
                "ctx_note",
                json!({"action": "read", "filter": "ready", "limit": 5, "offset": 100}),
            )
            .await,
        );
        assert!(page.contains("ready note 4"));
        assert!(page.contains("ready note 0"));
        assert!(!page.contains("ready note 104"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emergency_absent_shape_pending_suppresses_historian_fire() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        store
            .replace_compartments("ses", &[stored_comp(1, 1, 1, "m1", "S0")])
            .unwrap();

        let boot = call_transform_request(
            &handler,
            request_with_usage(vec![ck("m1", 1, "raw"), ck("m2", 2, "tail")], 1, 100),
        )
        .await;
        assert_eq!(boot["action"], "HARD");
        assert_eq!(boot["boundary_id"], "m1#0");

        let raw = call_transform_request(
            &handler,
            request_with_usage(vec![ck("foreign", 90, "other conversation")], 95, 100),
        )
        .await;
        assert_eq!(raw["action"], "PASSTHROUGH");
        assert_eq!(raw["historian"]["no_fire"], "pending_rewrite");
        assert_eq!(producer.connects.load(Ordering::SeqCst), 0);
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);
        assert!(store.load("ses").unwrap().meta.pending_rewrite.is_some());
        assert_eq!(store.load_compartments("ses").unwrap().len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn composite_session_keys_scope_lineage_and_do_not_match_child_prefix_suffixes() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, project) = handler_with_store(producer, default_test_config());
        let key_a = "conversation:root|agent:alpha";
        let key_b = "conversation:root|agent:beta";
        let suffix_key = "conversation:root|scope:mc-historian:child";
        handler.bind_route(8, binding(project.to_str().unwrap(), key_a));
        handler.bind_route(9, binding(project.to_str().unwrap(), key_b));
        handler.bind_route(10, binding(project.to_str().unwrap(), suffix_key));

        store
            .replace_compartments(key_a, &[stored_comp(1, 1, 1, "a1", "A")])
            .unwrap();
        store
            .replace_compartments(key_b, &[stored_comp(1, 1, 1, "b1", "B")])
            .unwrap();

        let a = call_transform_request_on_channel(
            &handler,
            8,
            json!({
                "kind": "transform",
                "v": 2,
                "serializer_profile": "owned-llmrunner",
                "session_id": key_a,
                "render_config": "cfg0",
                "usage": ModuleUsage { current_total_input_tokens: 1, context_limit_tokens: 100 },
                "messages": [ck("a1", 1, "raw a"), ck("a2", 2, "tail a")],
            }),
        )
        .await;
        let b = call_transform_request_on_channel(
            &handler,
            9,
            json!({
                "kind": "transform",
                "v": 2,
                "serializer_profile": "owned-llmrunner",
                "session_id": key_b,
                "render_config": "cfg0",
                "usage": ModuleUsage { current_total_input_tokens: 1, context_limit_tokens: 100 },
                "messages": [ck("b1", 1, "raw b"), ck("b2", 2, "tail b")],
            }),
        )
        .await;
        assert_eq!(a["boundary_id"], "a1#0");
        assert_eq!(b["boundary_id"], "b1#0");
        assert_eq!(store.load(key_a).unwrap().core.boundary_id, "a1#0");
        assert_eq!(store.load(key_b).unwrap().core.boundary_id, "b1#0");

        let suffix = call_transform_request_on_channel(
            &handler,
            10,
            json!({
                "kind": "transform",
                "v": 2,
                "serializer_profile": "owned-llmrunner",
                "session_id": suffix_key,
                "render_config": "cfg0",
                "usage": ModuleUsage { current_total_input_tokens: 1, context_limit_tokens: 100 },
                "messages": [ck("s1", 1, "not a child producer session")],
            }),
        )
        .await;
        assert_eq!(suffix["action"], "HARD");
        assert!(store.load(suffix_key).unwrap().row_version.is_some());
        assert!(!suffix_key.starts_with(historian::MC_CHILD_SESSION_PREFIX));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_routes_each_envelope_class_to_a_distinct_arm() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());

        // A flat body with kind="transform" routes to the transform handler.
        let transform = handler
            .dispatch_value(7, request(vec![ck("m1", 1, "hello")]))
            .await;
        let transform_body: Value = match transform {
            HandlerOutcome::Response(bytes) => serde_json::from_slice(&bytes).unwrap(),
            other => panic!("transform should respond, got {other:?}"),
        };
        assert_eq!(transform_body["status"], "ok");

        // Explicit echo: opt-in debugging arm still works when asked for by name.
        let echo = handler
            .dispatch_value(7, json!({ "kind": "echo", "probe": 42 }))
            .await;
        let echo_body: Value = match echo {
            HandlerOutcome::Response(bytes) => serde_json::from_slice(&bytes).unwrap(),
            other => panic!("echo should respond, got {other:?}"),
        };
        assert_eq!(echo_body["ok"], json!(true));
        assert_eq!(echo_body["echo"]["probe"], json!(42));

        // MCP tools/call envelope ({name, arguments}, no method/kind): a
        // DISTINCT error so a facade misroute is diagnosable from the code.
        let facade = handler
            .dispatch_value(
                7,
                json!({ "name": "ctx_unknown", "arguments": { "drop": "1-3" } }),
            )
            .await;
        assert_eq!(error_code(facade), "facade_envelope_not_supported");

        // Anything else: fail loud, never a silent echo. The message names the
        // keys that were present so a misrouted request is diagnosable.
        let garbage = handler
            .dispatch_value(7, json!({ "foo": 1, "bar": 2 }))
            .await;
        match garbage {
            HandlerOutcome::Error { code, message } => {
                assert_eq!(code, "unrecognized_request_shape");
                assert!(message.contains("foo"), "message names got keys: {message}");
            }
            other => panic!("expected error outcome, got {other:?}"),
        }

        // Non-object bodies get the same loud failure with the JSON type named.
        let non_object = handler.dispatch_value(7, json!("just a string")).await;
        match non_object {
            HandlerOutcome::Error { code, message } => {
                assert_eq!(code, "unrecognized_request_shape");
                assert!(message.contains("string"), "message names type: {message}");
            }
            other => panic!("expected error outcome, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_session_resolve_errors_are_typed() {
        let producer = Arc::new(ProducerState::default());
        let resolver = FakeSessionResolver::with(&[
            ("missing-map", FakeResolve::None),
            ("slow-map", FakeResolve::Timeout),
        ]);
        let (handler, _store, _dir, _project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver.clone());

        handler.bind_route(7, binding_with_harness("/repo", "claude-code", ""));
        let no_token = call_facade(
            &handler,
            "ctx_search",
            json!({ "query": "anything", "limit": 1 }),
        )
        .await;
        match no_token {
            HandlerOutcome::Error { code, message } => {
                assert_eq!(code, "session_unresolved");
                assert_eq!(message, SESSION_UNRESOLVED_MESSAGE);
            }
            other => panic!("expected unresolved error, got {other:?}"),
        }
        assert_eq!(resolver.calls(), Vec::<String>::new());

        handler.bind_route(
            7,
            binding_with_harness("/repo", "claude-code", "missing-map"),
        );
        let none = call_facade(
            &handler,
            "ctx_search",
            json!({ "query": "anything", "limit": 1 }),
        )
        .await;
        assert_eq!(error_code(none), "session_unresolved");

        handler.bind_route(7, binding_with_harness("/repo", "claude-code", "slow-map"));
        let timeout = call_facade(
            &handler,
            "ctx_search",
            json!({ "query": "anything", "limit": 1 }),
        )
        .await;
        assert_eq!(error_code(timeout), "session_resolve_timeout");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_authority_lookup_failure_is_retryable_and_never_falls_back() {
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, project) = handler_with_store_and_resolver(
            Arc::new(ProducerState::default()),
            default_test_config(),
            resolver,
        );
        let project_root = project.to_str().unwrap();
        handler.bind_route(7, binding(project_root, "token"));
        store.fail_next_authority_project_resolution_for_test();
        let arguments = json!({
            "action": "write",
            "category": "CONSTRAINTS",
            "content": "retry after authority lookup",
        });

        let failed = call_facade(&handler, "ctx_memory", arguments.clone()).await;
        assert_eq!(error_code(failed), "authority_project_resolution_failed");
        assert!(store
            .load_active_memories(project_root, now_ms())
            .unwrap()
            .is_empty());

        let retried = call_facade(&handler, "ctx_memory", arguments).await;
        assert!(!tool_is_error(retried));
        assert_eq!(
            store
                .load_active_memories(project_root, now_ms())
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_multi_instance_shares_memory_pool_and_splits_compartment_scope() {
        let producer = Arc::new(ProducerState::default());
        let project_root = "/same/repo";
        let key_a = "pm_a5ee3bf8/session-A/epoch-1";
        let key_b = "pm_a5ee3bf8/session-B/epoch-1";
        let resolver = FakeSessionResolver::with(&[
            ("token-a", FakeResolve::Hit(key_a.to_string())),
            ("token-b", FakeResolve::Hit(key_b.to_string())),
        ]);
        let (handler, store, _dir, _project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(7, binding(project_root, "token-a"));
        handler.bind_route(8, binding(project_root, "token-b"));
        store
            .replace_compartments(
                key_a,
                &[stored_comp(1, 1, 10, "a10", "alpha-compartment-only")],
            )
            .unwrap();
        store
            .replace_compartments(
                key_b,
                &[stored_comp(1, 1, 10, "b10", "beta-compartment-only")],
            )
            .unwrap();

        let a = call_facade_on_channel(
            &handler,
            7,
            "ctx_memory",
            json!({ "action": "write", "category": "CONSTRAINTS", "content": "shared project fact" }),
        )
        .await;
        assert!(!tool_is_error(a));
        let b_search = tool_json_array(
            call_facade_on_channel(
                &handler,
                8,
                "ctx_search",
                json!({ "query": "shared project fact", "limit": 5 }),
            )
            .await,
        );
        assert!(b_search.iter().any(|row| row["source"] == "memory"));

        let b = call_facade_on_channel(
            &handler,
            8,
            "ctx_memory",
            json!({ "action": "write", "category": "CONSTRAINTS", "content": "second shared fact" }),
        )
        .await;
        assert!(!tool_is_error(b));

        let project_rows = store.load_active_memories(project_root, now_ms()).unwrap();
        assert_eq!(project_rows.len(), 2);
        assert!(project_rows
            .iter()
            .any(|memory| memory.content == "shared project fact"));
        assert!(project_rows
            .iter()
            .any(|memory| memory.content == "second shared fact"));
        let first = store.get_memory_full(project_rows[0].id).unwrap().unwrap();
        assert_eq!(first.source_session_id.as_deref(), Some(key_a));
        assert!(store
            .load_active_memories(key_a, now_ms())
            .unwrap()
            .is_empty());
        assert!(store
            .load_active_memories(key_b, now_ms())
            .unwrap()
            .is_empty());

        let a_comp = tool_json_array(
            call_facade_on_channel(
                &handler,
                7,
                "ctx_search",
                json!({ "query": "alpha-compartment-only", "limit": 5 }),
            )
            .await,
        );
        assert!(a_comp.iter().any(|row| row["source"] == "compartment_body"));
        let a_cannot_see_b = tool_json_array(
            call_facade_on_channel(
                &handler,
                7,
                "ctx_search",
                json!({ "query": "beta-compartment-only", "limit": 5 }),
            )
            .await,
        );
        assert!(a_cannot_see_b.is_empty());
        let b_comp = tool_json_array(
            call_facade_on_channel(
                &handler,
                8,
                "ctx_search",
                json!({ "query": "beta-compartment-only", "limit": 5 }),
            )
            .await,
        );
        assert!(b_comp.iter().any(|row| row["source"] == "compartment_body"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_flat_envelope_precedence_keeps_kind_arm_and_gates_ctx_reduce_name() {
        let producer = Arc::new(ProducerState::default());
        let resolver = FakeSessionResolver::with(&[(
            "token",
            FakeResolve::Hit("composite-session".to_string()),
        )]);
        let (handler, _store, _dir, _project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(7, binding("/repo", "token"));

        let echo = handler
            .dispatch_value(
                7,
                json!({ "kind": "echo", "name": "ctx_memory", "arguments": { "action": "write" } }),
            )
            .await;
        let echo_body = tool_body(echo);
        assert_eq!(echo_body["ok"], json!(true));
        assert_eq!(echo_body["echo"]["kind"], "echo");

        let reduce = tool_text(call_facade(&handler, "ctx_reduce", json!({ "drop": "1-3" })).await);
        assert_eq!(reduce, CTX_REDUCE_ACKNOWLEDGEMENT);
    }

    #[test]
    fn ctx_reduce_range_parser_rejects_unbounded_and_oversized_ranges() {
        assert!(parse_tag_range_string("0-18446744073709551615").is_err());
        assert!(parse_tag_range_string("5-3").is_err());
        assert_eq!(parse_tag_range_string("3-5,8").unwrap(), vec![3, 4, 5, 8]);
        assert!(parse_tag_range_string("1-1001").is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_ctx_reduce_is_inert_before_resolution_or_storage() {
        let producer = Arc::new(ProducerState::default());
        let resolver = FakeSessionResolver::with(&[("unresolvable", FakeResolve::None)]);
        let handler = McHandler::with_producer_factory_config_resolver(
            Arc::new(TestProducerFactory { state: producer }),
            default_test_config(),
            resolver.clone(),
        );
        handler.bind_route(8, binding("/path/that/does/not/exist", "unresolvable"));

        let response = tool_text(
            call_facade_on_channel(&handler, 8, "ctx_reduce", json!({ "drop": "not parsed" }))
                .await,
        );
        assert_eq!(response, CTX_REDUCE_ACKNOWLEDGEMENT);
        assert!(
            resolver.calls().is_empty(),
            "the inert facade must not resolve tokens"
        );
        assert!(
            handler.store.get().is_none(),
            "the inert facade must not open or read storage"
        );
    }

    #[test]
    fn ctx_reduce_manifest_schema_is_closed_and_accepts_reduced_envelopes() {
        let manifest = manifest("magic-context");
        let ProviderRole::ToolProvider { tools, .. } = &manifest.provides[0] else {
            panic!("tool provider manifest entry");
        };
        let tool = tools
            .iter()
            .find(|tool| tool.name == "ctx_reduce")
            .expect("ctx_reduce manifest entry");
        assert_eq!(
            tool.schema,
            json!({
                "type": "object",
                "properties": {
                    "drop": { "type": "string" },
                    "reduced": { "type": "boolean" },
                    "summary": { "type": "string" }
                },
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn expand_output_is_bounded_to_the_typescript_token_budget() {
        let output = truncate_expand_output("x".repeat(CTX_EXPAND_BYTE_BUDGET * 2));
        assert!(output.len() <= CTX_EXPAND_BYTE_BUDGET + 64);
        assert!(output.contains("~15,000-token ctx_expand budget"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authority_complete_uses_the_module_seed_digest_before_ack() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let begin = call_dispatch_request(
            &handler,
            json!({
                "method": "authority.prepare",
                "phase": "begin",
                "context_store_uuid": "store-uuid",
                "project": "/repo",
                "domain": "memories"
            }),
        )
        .await;
        let generation = begin["authority"]["generation"].as_u64().unwrap();
        let _ = call_dispatch_request(
            &handler,
            json!({
                "method": "authority.seed",
                "context_store_uuid": "store-uuid",
                "project": "/repo",
                "domain": "memories",
                "rows": [{
                    "source_row_id": 1,
                    "snapshot": {
                        "id": 1,
                        "project_path": "/repo",
                        "category": "CONSTRAINTS",
                        "content": "seeded",
                        "normalized_hash": "hash"
                    }
                }]
            }),
        )
        .await;
        let actual = store
            .authority_seed_checksum("store-uuid", "/repo", "memories")
            .unwrap();
        let verified = call_dispatch_request(
            &handler,
            json!({
                "method": "authority.prepare",
                "phase": "complete",
                "context_store_uuid": "store-uuid",
                "project": "/repo",
                "domain": "memories",
                "generation": generation,
                "checksum_expected": actual
            }),
        )
        .await;
        assert_eq!(verified["authority"]["state"], "PREPARING");
        assert_eq!(verified["authority"]["checksum_ok"], true);
        let acked = call_dispatch_request(
            &handler,
            json!({
                "method": "authority.prepare",
                "phase": "ack",
                "context_store_uuid": "store-uuid",
                "project": "/repo",
                "domain": "memories",
                "generation": generation
            }),
        )
        .await;
        assert_eq!(acked["authority"]["state"], "MODULE");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_never_panics_on_malformed_memory_arguments() {
        let producer = Arc::new(ProducerState::default());
        let resolver = FakeSessionResolver::with(&[(
            "token",
            FakeResolve::Hit("opaque-own-conversation".to_string()),
        )]);
        let (handler, _store, _dir, _project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(7, binding("/repo", "token"));

        let malformed = [
            json!({ "action": "update", "content": "edited" }),
            json!({ "action": "update", "ids": [], "content": "edited" }),
            json!({ "action": "update", "ids": ["x"], "content": "edited" }),
            json!({ "action": "update", "id": -1, "content": "edited" }),
            json!({ "action": "update", "id": u64::MAX, "content": "edited" }),
            json!({ "action": "archive" }),
            json!({ "action": "archive", "ids": [] }),
            json!({ "action": "archive", "ids": ["x"] }),
            json!({ "action": "archive", "id": -1 }),
            json!({ "action": "archive", "id": u64::MAX }),
            json!({ "action": "merge", "content": "merged" }),
            json!({ "action": "merge", "ids": [], "content": "merged" }),
            json!({ "action": "merge", "ids": ["x"], "content": "merged" }),
            json!({ "action": "merge", "ids": [-1, -2], "content": "merged" }),
            json!({ "action": "merge", "ids": [u64::MAX, 1], "content": "merged" }),
        ];

        for arguments in malformed {
            let outcome = call_facade(&handler, "ctx_memory", arguments.clone()).await;
            assert!(
                tool_is_error(outcome),
                "malformed arguments must return a typed tool error: {arguments}"
            );
        }
        let oversized = call_facade(
            &handler,
            "ctx_memory",
            json!({
                "action": "write",
                "category": "CONSTRAINTS",
                "content": "x".repeat(MAX_MEMORY_CONTENT_BYTES + 1),
            }),
        )
        .await;
        assert!(tool_is_error(oversized));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_unwraps_imitated_reduced_arguments_without_overriding_real_values() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, _project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(7, binding("/repo", "token"));
        let id = insert_memory(&store, "/repo", "CONSTRAINTS", "Run focused tests.", 1);

        let plain = tool_text(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "action": "get", "ids": [id] }),
            )
            .await,
        );
        let imitated = tool_text(
            call_facade(
                &handler,
                "ctx_memory",
                json!({
                    "reduced": true,
                    "summary": json!({ "action": "get", "ids": [id] }).to_string(),
                }),
            )
            .await,
        );
        let real_arguments = tool_text(
            call_facade(
                &handler,
                "ctx_memory",
                json!({
                    "action": "get",
                    "ids": [id],
                    "reduced": true,
                    "summary": json!({ "action": "archive", "ids": [id] }).to_string(),
                }),
            )
            .await,
        );
        let malformed = error_frame(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "reduced": true, "summary": "not JSON" }),
            )
            .await,
        );
        let plain_missing = error_frame(call_facade(&handler, "ctx_memory", json!({})).await);
        let reduce_plain =
            tool_text(call_facade(&handler, "ctx_reduce", json!({ "drop": "1" })).await);
        let reduce_imitated = tool_text(
            call_facade(
                &handler,
                "ctx_reduce",
                json!({
                    "reduced": true,
                    "summary": json!({ "drop": "1" }).to_string(),
                }),
            )
            .await,
        );

        assert_eq!(imitated, plain);
        assert_eq!(real_arguments, plain);
        assert_eq!(store.get_memory_full(id).unwrap().unwrap().status, "active");
        assert_eq!(malformed, plain_missing);
        assert_eq!(reduce_imitated, reduce_plain);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn memory_facade_routes_all_authority_actions_into_store_and_changefeed() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, _project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(7, binding("/repo", "token"));

        for arguments in [
            json!({"action": "write", "category": "CONSTRAINTS", "content": "first"}),
            json!({"action": "update", "ids": [1], "content": "first updated"}),
            json!({"action": "write", "category": "CONSTRAINTS", "content": "second"}),
            json!({"action": "merge", "ids": [1, 2], "content": "merged"}),
            json!({"action": "get", "ids": [1]}),
            json!({"action": "archive", "ids": [1]}),
        ] {
            let outcome = call_facade(&handler, "ctx_memory", arguments.clone()).await;
            assert!(!tool_is_error(outcome), "action failed: {arguments}");
        }
        let memory = store.get_memory_full(1).unwrap().unwrap();
        assert_eq!(memory.content, "merged");
        assert_eq!(memory.status, "archived");
        assert!(store
            .get_memory_full(2)
            .unwrap()
            .unwrap()
            .superseded_by_memory_id
            .is_some());
        let feed = store.pull_changefeed("memories", 0, 100).unwrap();
        assert!(
            feed.rows.len() >= 6,
            "every mutation must append changefeed state"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn classification_facade_hash_fences_rows_and_keeps_m1_revision_stable() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        let route_project_root = project.to_str().unwrap();
        handler.bind_route(7, binding(route_project_root, "token"));
        activate_module_authority(
            &store,
            "context",
            "git:classify",
            route_project_root,
            "memories",
        );
        let fresh_id = insert_memory(
            &store,
            "git:classify",
            "PROJECT_RULES",
            "fresh classification",
            1,
        );
        let stale_id = insert_memory(
            &store,
            "git:classify",
            "PROJECT_RULES",
            "stale classification",
            1,
        );
        let fresh_hash = store
            .get_memory_full(fresh_id)
            .unwrap()
            .unwrap()
            .normalized_hash;
        let generation = store
            .authority_status("context", "git:classify", "memories")
            .unwrap()
            .unwrap()
            .generation;
        let before =
            crate::m1_compose::m1_revision_signal(&store, "git:classify", "session").unwrap();
        let outcome = call_facade(
            &handler,
            "memory.set_classification",
            json!({
                "memory_project": "git:classify",
                "context_store_uuid": "context",
                "authority_generation": generation,
                "rows": [
                    {
                        "memory_id": fresh_id,
                        "content_hash_at_prompt": fresh_hash,
                        "importance": 91,
                        "scope": "project",
                        "shareable": true
                    },
                    {
                        "memory_id": stale_id,
                        "content_hash_at_prompt": "stale-hash",
                        "importance": 1,
                        "scope": "project",
                        "shareable": true
                    }
                ]
            }),
        )
        .await;
        let response = match outcome {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("classification facade failed: {other:?}"),
        };
        assert_eq!(response["accepted"], json!([fresh_id]));
        assert_eq!(response["rejected"][0]["reason"], "stale");
        assert_eq!(
            store.get_memory_full(fresh_id).unwrap().unwrap().importance,
            Some(91)
        );
        let after =
            crate::m1_compose::m1_revision_signal(&store, "git:classify", "session").unwrap();
        assert_eq!(before, after, "classification metadata must not change m1");
        let feed = store.pull_changefeed("memories", 0, 100).unwrap();
        assert!(feed
            .rows
            .iter()
            .any(|row| row.op == "update" && row.module_row_id == fresh_id));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verification_and_mapping_facades_are_fenced_hash_guarded_and_idempotent() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        let route_root = project.to_str().unwrap();
        let identity = "git:dreamer-applies";
        handler.bind_route(7, binding(route_root, "token"));
        activate_module_authority(&store, "context", identity, route_root, "memories");
        let verified_id = insert_memory(&store, identity, "CONSTRAINTS", "verified", 1);
        let updated_id = insert_memory(&store, identity, "CONSTRAINTS", "updated", 1);
        let archived_id = insert_memory(&store, identity, "CONSTRAINTS", "archived", 1);
        let foreign_id = insert_memory(&store, "git:other", "CONSTRAINTS", "foreign", 1);
        let generation = store
            .authority_status("context", identity, "memories")
            .unwrap()
            .unwrap()
            .generation;
        let hash = |id: i64| store.get_memory_full(id).unwrap().unwrap().normalized_hash;
        let before_verified =
            crate::m1_compose::m1_revision_signal(&store, identity, "session").unwrap();
        let verified = call_facade(&handler, "memory.set_verification", json!({
            "memory_project": identity, "context_store_uuid": "context", "authority_generation": generation,
            "command_id": "verify-once", "rows": [
                {"memory_id": verified_id, "content_hash_at_prompt": hash(verified_id), "verification_status": "verified"},
                {"memory_id": updated_id, "content_hash_at_prompt": "stale", "verification_status": "verified"},
                {"memory_id": 999999, "content_hash_at_prompt": "missing", "verification_status": "verified"},
                {"memory_id": foreign_id, "content_hash_at_prompt": hash(foreign_id), "verification_status": "verified"}
            ]
        })).await;
        let verified_body = match verified {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("verification facade failed: {other:?}"),
        };
        assert_eq!(verified_body["accepted"], json!([verified_id]));
        assert_eq!(verified_body["rejected"].as_array().unwrap().len(), 3);
        let after_verified =
            crate::m1_compose::m1_revision_signal(&store, identity, "session").unwrap();
        assert_eq!(
            before_verified, after_verified,
            "verification stamps are cache-neutral"
        );
        let replay = call_facade(&handler, "memory.set_verification", json!({
            "memory_project": identity, "context_store_uuid": "context", "authority_generation": generation,
            "command_id": "verify-once", "rows": [{"memory_id": verified_id, "content_hash_at_prompt": hash(verified_id), "verification_status": "verified"}]
        })).await;
        let replay_body = match replay {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("verification replay failed: {other:?}"),
        };
        assert_eq!(
            replay_body, verified_body,
            "command replay must not append another mutation"
        );

        let before_update =
            crate::m1_compose::m1_revision_signal(&store, identity, "session").unwrap();
        let update = call_facade(&handler, "memory.set_verification", json!({
            "memory_project": identity, "context_store_uuid": "context", "authority_generation": generation,
            "rows": [{"memory_id": updated_id, "content_hash_at_prompt": hash(updated_id), "verification_status": "update", "updated_content": "updated by verifier"}]
        })).await;
        let update_body = match update {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("update facade failed: {other:?}"),
        };
        let after_update =
            crate::m1_compose::m1_revision_signal(&store, identity, "session").unwrap();
        assert_eq!(update_body["accepted"], json!([updated_id]));
        assert_ne!(
            after_update, before_update,
            "content updates advance the m1 mutation signal"
        );

        let before_archive =
            crate::m1_compose::m1_revision_signal(&store, identity, "session").unwrap();
        let archive = call_facade(&handler, "memory.set_verification", json!({
            "memory_project": identity, "context_store_uuid": "context", "authority_generation": generation,
            "rows": [{"memory_id": archived_id, "content_hash_at_prompt": hash(archived_id), "verification_status": "archive", "archive_reason": "obsolete"}]
        })).await;
        assert!(matches!(archive, HandlerOutcome::Response(_)));
        let after_archive =
            crate::m1_compose::m1_revision_signal(&store, identity, "session").unwrap();
        assert!(
            after_archive > before_archive,
            "archives advance the m1 mutation signal"
        );

        let mapping = call_facade(&handler, "memory.set_mapping", json!({
            "memory_project": identity, "context_store_uuid": "context", "authority_generation": generation,
            "command_id": "mapping-once", "rows": [{"memory_id": verified_id, "content_hash_at_prompt": hash(verified_id), "mapped_files": ["src/lib.rs", "src/lib.rs"]}]
        })).await;
        assert!(matches!(mapping, HandlerOutcome::Response(_)));
        let mapping_replay = call_facade(&handler, "memory.set_mapping", json!({
            "memory_project": identity, "context_store_uuid": "context", "authority_generation": generation,
            "command_id": "mapping-once", "rows": [{"memory_id": verified_id, "content_hash_at_prompt": "stale", "mapped_files": null}]
        })).await;
        assert!(
            matches!(mapping_replay, HandlerOutcome::Response(_)),
            "mapping command replay must be idempotent"
        );
        let feed = store.pull_changefeed("memories", 0, 1000).unwrap();
        assert!(feed
            .rows
            .iter()
            .any(|row| row.full_row_snapshot.get("mapping").is_some()));
        let generation_error = call_facade(&handler, "memory.set_mapping", json!({
            "memory_project": identity, "context_store_uuid": "context", "authority_generation": generation - 1,
            "rows": []
        })).await;
        assert_eq!(
            error_code(generation_error),
            "authority_generation_mismatch"
        );
        store
            .authority_begin_drain("context", identity, "memories", "test-drain", 9_999_999, 1)
            .unwrap();
        let draining = call_facade(&handler, "memory.set_verification", json!({
            "memory_project": identity, "context_store_uuid": "context", "authority_generation": generation,
            "rows": []
        })).await;
        assert_eq!(error_code(draining), "authority_draining");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn raced_classification_drain_returns_the_transition_specific_code() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        let route_root = project.to_str().unwrap();
        let identity = "git:classification-race";
        handler.bind_route(7, binding(route_root, "token"));
        activate_module_authority(&store, "context", identity, route_root, "memories");
        let memory_id = insert_memory(&store, identity, "CONSTRAINTS", "classify me", 1);
        let before = store.get_memory_full(memory_id).unwrap().unwrap();
        let generation = store
            .authority_status("context", identity, "memories")
            .unwrap()
            .unwrap()
            .generation;
        let feed_head = store
            .pull_changefeed("memories", 0, 100)
            .unwrap()
            .next_cursor;
        let hook_store = Arc::clone(&store);
        *handler
            .classification_before_apply_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(move || {
            hook_store
                .authority_begin_drain(
                    "context",
                    identity,
                    "memories",
                    "classification-race",
                    i64::MAX,
                    2,
                )
                .unwrap();
        }));

        let outcome = call_facade(
            &handler,
            "memory.set_classification",
            json!({
                "memory_project": identity,
                "context_store_uuid": "context",
                "authority_generation": generation,
                "rows": [{
                    "memory_id": memory_id,
                    "content_hash_at_prompt": before.normalized_hash.clone(),
                    "importance": 99
                }]
            }),
        )
        .await;

        assert_eq!(error_code(outcome), "authority_draining");
        assert_eq!(
            store
                .get_memory_full(memory_id)
                .unwrap()
                .unwrap()
                .importance,
            before.importance
        );
        assert_eq!(
            store
                .pull_changefeed("memories", 0, 100)
                .unwrap()
                .next_cursor,
            feed_head
        );
        assert_eq!(
            store
                .authority_status("context", identity, "memories")
                .unwrap()
                .unwrap()
                .state,
            "DRAINING"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn draining_authority_rejects_every_facade_mutation_but_keeps_reads_resolved() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        let route_root = project.to_str().unwrap();
        let identity = "git:draining";
        handler.bind_route(7, binding(route_root, "token"));
        for domain in ["memories", "notes"] {
            activate_module_authority(&store, "context", identity, route_root, domain);
        }
        let first = insert_memory(&store, identity, "CONSTRAINTS", "first", 1);
        let second = insert_memory(&store, identity, "CONSTRAINTS", "second", 1);
        let note = store
            .insert_note(NoteInput {
                project_path: identity,
                route_project_root: Some(route_root),
                session_id: "session",
                content: "note",
                surface_condition: None,
                anchor_block_id: None,
                now_ms: 1,
            })
            .unwrap();
        let memory_drain = store
            .authority_begin_drain("context", identity, "memories", "lease-memory", i64::MAX, 2)
            .unwrap();
        store
            .authority_begin_drain("context", identity, "notes", "lease-notes", i64::MAX, 2)
            .unwrap();
        let memory_head = store
            .pull_changefeed("memories", 0, 100)
            .unwrap()
            .next_cursor;
        let note_head = store.pull_changefeed("notes", 0, 100).unwrap().next_cursor;

        for arguments in [
            json!({"action": "write", "category": "CONSTRAINTS", "content": "late"}),
            json!({"action": "update", "ids": [first], "content": "late"}),
            json!({"action": "archive", "ids": [first]}),
            json!({"action": "merge", "ids": [first, second], "content": "late"}),
        ] {
            assert_eq!(
                error_code(call_facade(&handler, "ctx_memory", arguments).await),
                "authority_draining"
            );
        }
        for arguments in [
            json!({"action": "write", "content": "late"}),
            json!({"action": "update", "note_id": note.id, "content": "late"}),
            json!({"action": "dismiss", "note_id": note.id}),
        ] {
            assert_eq!(
                error_code(call_facade(&handler, "ctx_note", arguments).await),
                "authority_draining"
            );
        }
        assert_eq!(
            error_code(
                call_facade(
                    &handler,
                    "memory.set_classification",
                    json!({
                        "memory_project": identity,
                        "context_store_uuid": "context",
                        "authority_generation": memory_drain.generation,
                        "rows": []
                    }),
                )
                .await
            ),
            "authority_draining"
        );
        assert!(!tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({"action": "get", "ids": [first]})
            )
            .await
        ));
        assert!(!tool_is_error(
            call_facade(&handler, "ctx_note", json!({"action": "read"})).await
        ));
        assert_eq!(
            store
                .pull_changefeed("memories", 0, 100)
                .unwrap()
                .next_cursor,
            memory_head
        );
        assert_eq!(
            store.pull_changefeed("notes", 0, 100).unwrap().next_cursor,
            note_head
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_dreamer_run_unregisters_its_child_session() {
        let producer = Arc::new(ProducerState::default());
        producer.block_output.store(true, Ordering::SeqCst);
        let (handler, store, _dir, project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let route_root = project.to_str().unwrap();
        handler.bind_route(7, binding(route_root, "parent"));
        activate_module_authority(&store, "context", "git:identity", route_root, "memories");
        let generation = store
            .authority_status("context", "git:identity", "memories")
            .unwrap()
            .unwrap()
            .generation;
        let child_session = child_session_id("git:identity", "cancel-command");
        let handler = Arc::new(handler);
        let running_handler = Arc::clone(&handler);
        let task = tokio::spawn(async move {
            running_handler
                .handle_dreamer_run_task(
                    7,
                    &json!({
                        "v": 1,
                        "session_id": "parent",
                        "task": CLASSIFY_TASK,
                        "command_id": "cancel-command",
                        "authority_generation": generation,
                        "payload": { "prompt_body": "classify", "items": [] },
                    }),
                )
                .await
        });
        wait_for_count(&producer.await_outputs, 1).await;
        assert!(handler.dreamer_run_registered(&child_session));

        task.abort();
        let _ = task.await;
        assert!(!handler.dreamer_run_registered(&child_session));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn module_authority_facade_write_uses_identity_not_route_path() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        let route_project_root = project.to_str().unwrap();
        handler.bind_route(7, binding(route_project_root, "token"));
        activate_module_authority(
            &store,
            "context",
            "git:identity",
            route_project_root,
            "memories",
        );

        let outcome = call_facade(
            &handler,
            "ctx_memory",
            json!({
                "action": "write",
                "category": "CONSTRAINTS",
                "content": "identity scoped",
                "memory_project": "git:identity",
            }),
        )
        .await;
        assert!(!tool_is_error(outcome));
        assert_eq!(
            store.get_memory_full(1).unwrap().unwrap().project_path,
            "git:identity"
        );
        assert!(store
            .load_active_memories(route_project_root, 10)
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authority_activation_moves_render_reads_once_through_the_m1_revision() {
        let (handler, store, _dir, project) =
            handler_with_store(Arc::new(ProducerState::default()), default_test_config());
        let route_project_root = project.to_str().unwrap();
        store
            .replace_compartments("ses", &[stored_comp(1, 0, 0, "m0", "initial summary")])
            .unwrap();
        let request = request(vec![ck("m0", 0, "live input")]);
        let initial = call_transform_request(&handler, request.clone()).await;
        assert_eq!(initial["action"], "HARD");

        insert_memory(
            &store,
            "git:identity",
            "CONSTRAINTS",
            "identity-only memory",
            1,
        );
        assert_eq!(store.load("ses").unwrap().meta.max_memory_id, 0);
        assert_eq!(
            store
                .load_active_memories("git:identity", now_ms())
                .unwrap()
                .len(),
            1
        );
        activate_module_authority(
            &store,
            "context",
            "git:identity",
            route_project_root,
            "memories",
        );

        assert_eq!(
            store
                .authority_project_for_route(route_project_root, "memories")
                .unwrap()
                .as_deref(),
            Some("git:identity")
        );
        let before_transition = store.load("ses").unwrap();
        let identity_revision =
            crate::m1_compose::m1_revision_signal(&store, "git:identity", "ses").unwrap();
        assert_ne!(before_transition.meta.m1_revision, identity_revision);
        let direct_m1 = crate::m1_compose::compose_m1_from_store(
            &store,
            "git:identity",
            route_project_root,
            "ses",
            &before_transition.meta,
            before_transition.meta.expiry_cutoff_ms,
        )
        .unwrap();
        assert!(
            direct_m1.body.contains("identity-only memory"),
            "{}",
            direct_m1.body
        );
        let transition = call_transform_request(&handler, request.clone()).await;
        assert_eq!(transition["action"], "SOFT");
        assert!(
            transition.to_string().contains("identity-only memory"),
            "the coordinated soft pass must read the identity-keyed row: {transition}"
        );
        let stable = call_transform_request(&handler, request).await;
        assert_eq!(stable["action"], "SOFT+");
        assert_eq!(transition["ck_messages"], stable["ck_messages"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authority_historian_publication_promotes_facts_under_the_identity_key() {
        let producer = Arc::new(ProducerState::default());
        producer.block_output.store(true, Ordering::SeqCst);
        let (handler, store, _dir, project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let route_project_root = project.to_str().unwrap();
        activate_module_authority(
            &store,
            "context",
            "git:identity",
            route_project_root,
            "memories",
        );

        let response = call_transform(&handler, big_messages()).await;
        assert_eq!(response["historian"]["fired"], true);
        wait_for_count(&producer.starts, 1).await;
        let prompt = producer.prompts.lock().unwrap()[0].clone();
        let (start, end) = prompt_ordinal_range(&prompt).unwrap();
        {
            let mut outputs = producer.outputs.lock().unwrap();
            outputs.clear();
            outputs.push_back(historian_output_with_fact(
                start,
                end,
                "identity historian fact",
            ));
        }
        producer.block_output.store(false, Ordering::SeqCst);
        producer.notify.notify_waiters();
        wait_for_idle(&store).await;

        assert!(store
            .load_active_memories("git:identity", 0)
            .unwrap()
            .iter()
            .any(|memory| memory.content == "identity historian fact"));
        assert!(store
            .load_active_memories(route_project_root, 0)
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn module_authority_rejects_mismatched_facade_project_vocabulary() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        let route_project_root = project.to_str().unwrap();
        handler.bind_route(7, binding(route_project_root, "token"));
        activate_module_authority(
            &store,
            "context",
            "git:identity",
            route_project_root,
            "memories",
        );

        let (code, message) = error_frame(
            call_facade(
                &handler,
                "ctx_memory",
                json!({
                    "action": "write",
                    "category": "CONSTRAINTS",
                    "content": "must fail",
                    "memory_project": route_project_root,
                }),
            )
            .await,
        );
        assert_eq!(code, "facade_project_vocabulary_mismatch");
        assert!(message.contains("git:identity"));
        assert!(message.contains(route_project_root));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_without_authority_keeps_path_scoped_writes() {
        let producer = Arc::new(ProducerState::default());
        let resolver =
            FakeSessionResolver::with(&[("token", FakeResolve::Hit("session".to_string()))]);
        let (handler, store, _dir, project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        let route_project_root = project.to_str().unwrap();
        handler.bind_route(7, binding(route_project_root, "token"));

        let outcome = call_facade(
            &handler,
            "ctx_memory",
            json!({
                "action": "write",
                "category": "CONSTRAINTS",
                "content": "path scoped",
            }),
        )
        .await;
        assert!(!tool_is_error(outcome));
        assert_eq!(
            store.get_memory_full(1).unwrap().unwrap().project_path,
            route_project_root
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn memory_disabled_rejects_mutation_and_excludes_memory_search() {
        let producer = Arc::new(ProducerState::default());
        let resolver = FakeSessionResolver::with(&[("token", FakeResolve::Hit("ses".to_string()))]);
        let mut config = default_test_config();
        config.memory_enabled = false;
        let (handler, store, _dir, _project) =
            handler_with_store_and_resolver(producer, config, resolver);
        let mut disabled_binding = binding("/repo", "token");
        disabled_binding.config.memory_enabled = false;
        handler.bind_route(7, disabled_binding);
        insert_memory(&store, "/repo", "CONSTRAINTS", "hidden needle", 1);

        assert!(tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({"action": "write", "category": "CONSTRAINTS", "content": "blocked"}),
            )
            .await
        ));
        let results =
            tool_json_array(call_facade(&handler, "ctx_search", json!({"query": "needle"})).await);
        assert!(results.iter().all(|result| result["source"] != "memory"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_security_guards_run_through_public_handler_path() {
        let producer = Arc::new(ProducerState::default());
        let own = "/repo";
        let foreign = "opaque-foreign-key";
        let resolver = FakeSessionResolver::with(&[(
            "token",
            FakeResolve::Hit("opaque-own-conversation".to_string()),
        )]);
        let (handler, store, _dir, _project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(7, binding(own, "token"));
        seed_workspace(&store, own, foreign);

        let foreign_private_update =
            insert_memory(&store, foreign, "PREFERENCES", "private update", 1);
        let foreign_private_archive =
            insert_memory(&store, foreign, "PREFERENCES", "private archive", 1);
        let foreign_private_merge =
            insert_memory(&store, foreign, "PREFERENCES", "private merge", 1);
        let own_private = insert_memory(&store, own, "PREFERENCES", "own private", 1);

        assert!(tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "action": "update", "id": foreign_private_update, "content": "edited" }),
            )
            .await
        ));
        assert!(tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "action": "archive", "ids": [foreign_private_archive] }),
            )
            .await
        ));
        assert!(tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "action": "merge", "ids": [foreign_private_merge, own_private], "content": "merged" }),
            )
            .await
        ));

        let shared_update = insert_memory(&store, foreign, "CONSTRAINTS", "shared update", 1);
        let shared_archive = insert_memory(&store, foreign, "CONSTRAINTS", "shared archive", 1);
        let shared_target = insert_memory(&store, foreign, "CONSTRAINTS", "shared target", 1);
        let shared_source = insert_memory(&store, foreign, "CONSTRAINTS", "shared source", 1);

        assert!(tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "action": "update", "id": shared_update, "content": "shared edited" }),
            )
            .await
        ));
        assert!(tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "action": "archive", "ids": [shared_archive] }),
            )
            .await
        ));
        assert!(tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "action": "merge", "ids": [shared_target, shared_source], "content": "shared merged" }),
            )
            .await
        ));
        assert_eq!(
            store
                .get_memory_full(shared_source)
                .unwrap()
                .unwrap()
                .superseded_by_memory_id,
            None
        );

        let cross_target = insert_memory(&store, own, "CONSTRAINTS", "constraint", 1);
        let cross_source = insert_memory(&store, own, "PREFERENCES", "preference", 1);
        assert!(tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "action": "merge", "ids": [cross_target, cross_source], "content": "bad merge" }),
            )
            .await
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_cache_mutations_drive_soft_memory_deltas_through_public_path() {
        let producer = Arc::new(ProducerState::default());
        let project_root = "/repo/cache-project";
        let scope = "pm_a5ee3bf8/parent-session/epoch-1";
        let additive_project_root = "/repo/additive-project";
        let additive_scope = "pm_a5ee3bf8/additive-session/epoch-1";
        let resolver = FakeSessionResolver::with(&[
            ("token", FakeResolve::Hit(scope.to_string())),
            (
                "token-additive",
                FakeResolve::Hit(additive_scope.to_string()),
            ),
        ]);
        let (handler, store, _dir, _project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(7, binding(project_root, "token"));
        handler.bind_route(8, binding(project_root, scope));
        store
            .replace_compartments(scope, &[stored_comp(1, 1, 10, "m10", "SUMMARY")])
            .unwrap();
        let memory_id = insert_memory(&store, project_root, "CONSTRAINTS", "original rule", 1);

        let mut boot_req = request(vec![ck("m10", 10, "raw covered")]);
        boot_req["session_id"] = json!(scope);
        let boot = call_transform_request_on_channel(&handler, 8, boot_req.clone()).await;
        assert_eq!(boot["action"], "HARD");
        assert!(synthetic_text(&boot, 0).contains("original rule"));

        let before = store
            .max_memory_mutation_id(&[project_root.to_string()])
            .unwrap();
        assert!(!tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "action": "update", "id": memory_id, "content": "updated rule" }),
            )
            .await
        ));
        let after = store
            .max_memory_mutation_id(&[project_root.to_string()])
            .unwrap();
        assert!(
            after > before,
            "facade update must advance the mutation log"
        );

        let update_delta = call_transform_request_on_channel(&handler, 8, boot_req.clone()).await;
        assert_eq!(update_delta["action"], "SOFT");
        assert!(synthetic_text(&update_delta, 1).contains("<memory-updates>"));
        assert!(synthetic_text(&update_delta, 1).contains("updated rule"));

        handler.bind_route(9, binding(additive_project_root, additive_scope));
        store
            .replace_compartments(
                additive_scope,
                &[stored_comp(1, 1, 10, "m10", "ADDITIVE-SUMMARY")],
            )
            .unwrap();
        let mut add_req = request(vec![ck("m10", 10, "raw covered")]);
        add_req["session_id"] = json!(additive_scope);
        let add_boot = call_transform_request_on_channel(&handler, 9, add_req.clone()).await;
        assert_eq!(add_boot["action"], "HARD");
        let add_before = store
            .max_memory_mutation_id(&[additive_project_root.to_string()])
            .unwrap();
        handler.bind_route(7, binding(additive_project_root, "token-additive"));
        let resolver_scope_memory = call_facade(
            &handler,
            "ctx_memory",
            json!({ "action": "write", "category": "CONSTRAINTS", "content": "new additive memory" }),
        )
        .await;
        assert!(!tool_is_error(resolver_scope_memory));
        assert_eq!(
            store
                .max_memory_mutation_id(&[additive_project_root.to_string()])
                .unwrap(),
            add_before,
            "additive writes must not append mutation-log rows"
        );
        assert!(store
            .load_active_memories(additive_project_root, now_ms())
            .unwrap()
            .iter()
            .any(|memory| {
                memory.content == "new additive memory"
                    && store
                        .get_memory_full(memory.id)
                        .unwrap()
                        .unwrap()
                        .source_session_id
                        .as_deref()
                        == Some(additive_scope)
            }));
        let add_delta = call_transform_request_on_channel(&handler, 9, add_req).await;
        assert_eq!(add_delta["action"], "SOFT");
        assert!(synthetic_text(&add_delta, 1).contains("<new-memories>"));
        assert!(synthetic_text(&add_delta, 1).contains("new additive memory"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_or_unknown_serializer_profile_is_typed_and_does_not_write_store() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());

        let mut missing = request(vec![ck("m1", 1, "hello")]);
        missing
            .as_object_mut()
            .unwrap()
            .remove("serializer_profile");
        let before_missing = store.load("ses").unwrap().row_version;
        assert_eq!(
            error_code(call_transform_outcome(&handler, missing).await),
            "unknown_serializer_profile"
        );
        assert_eq!(store.load("ses").unwrap().row_version, before_missing);

        let mut unknown = request(vec![ck("m2", 2, "hello")]);
        unknown["serializer_profile"] = json!("not-a-profile");
        let before_unknown = store.load("ses").unwrap().row_version;
        assert_eq!(
            error_code(call_transform_outcome(&handler, unknown).await),
            "unknown_serializer_profile"
        );
        assert_eq!(store.load("ses").unwrap().row_version, before_unknown);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tail_delta_returns_need_full_sync_success_without_store_write() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());

        let mut delta = request(vec![ck("m1", 1, "hello")]);
        delta["tail_delta"] = json!({ "after": "fp-old", "messages": [ck("m2", 2, "tail")] });
        delta["full_array_fingerprint"] = json!("fp-delta");
        let before = store.load("ses").unwrap().row_version;
        let response = call_transform_request(&handler, delta).await;

        assert_eq!(response["status"], "need_full_sync");
        assert_eq!(response["served_from"], "transform");
        assert_eq!(response["full_array_fingerprint"], "fp-delta");
        assert_eq!(response["surface_state"], "inactive");
        assert!(response["row_version"].is_u64());
        // The array field must be ABSENT, not empty: the consumer discriminates
        // structurally on presence, and an empty array would be a third
        // ambiguous state between "transformed to nothing" and "re-send".
        assert!(response.get("ck_messages").is_none());
        assert_eq!(store.load("ses").unwrap().row_version, before);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fingerprint_absent_success_omits_echo_field() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());

        let response = call_transform_request(
            &handler,
            request_with_usage(vec![ck("m1", 1, "hello")], 1, 100),
        )
        .await;
        assert_eq!(response["status"], "ok");
        assert!(response.get("full_array_fingerprint").is_none());
        assert_eq!(response["surface_state"], "inactive");
        assert!(response["row_version"].is_u64());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transform_ok_wire_reports_each_surface_state_and_row_version() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());
        let mut request = request(vec![ck("m1", 1, "hello")]);
        request["serializer_profile"] = json!("claude-code-anthropic");

        let inactive = call_transform_request(&handler, request.clone()).await;
        assert_eq!(inactive["surface_state"], "inactive");
        let v_inactive = inactive["row_version"].as_u64().expect("row_version u64");

        request["tool_present"] = json!(true);
        let on_transition = call_transform_request(&handler, request.clone()).await;
        assert_eq!(on_transition["surface_state"], "transition");
        let v_on = on_transition["row_version"]
            .as_u64()
            .expect("row_version u64");
        // The surface flip rides a committing HARD, so the version must advance.
        assert!(v_on > v_inactive, "flip-on HARD must bump row_version");

        let active = call_transform_request(&handler, request.clone()).await;
        assert_eq!(active["surface_state"], "active");
        let v_active = active["row_version"].as_u64().expect("row_version u64");
        // Steady-state replay commits nothing: equal versions are legitimate,
        // regression (a lower version) never is.
        assert!(v_active >= v_on, "row_version must be nondecreasing");

        request["tool_present"] = json!(false);
        let off_transition = call_transform_request(&handler, request.clone()).await;
        assert_eq!(off_transition["surface_state"], "transition");
        let v_off = off_transition["row_version"]
            .as_u64()
            .expect("row_version u64");
        assert!(v_off > v_active, "flip-off HARD must bump row_version");

        let inactive_again = call_transform_request(&handler, request).await;
        assert_eq!(inactive_again["surface_state"], "inactive");
        let v_final = inactive_again["row_version"]
            .as_u64()
            .expect("row_version u64");
        assert!(v_final >= v_off, "row_version must be nondecreasing");
    }

    async fn call_transform_with_usage(
        handler: &McHandler,
        messages: Vec<CkIngressMessage>,
        current_total_input_tokens: u64,
        context_limit_tokens: u64,
    ) -> Value {
        call_transform_request(
            handler,
            request_with_usage(messages, current_total_input_tokens, context_limit_tokens),
        )
        .await
    }

    fn mint_drop_tag(store: &McStore, target_id: &str) {
        store
            .seed_tags_for_test(
                "ses",
                &[TagMintInput {
                    block_id: target_id.to_string(),
                    kind: "message".to_string(),
                    token_count: 1,
                    source_bytes: b"source".to_vec(),
                }],
                1,
            )
            .unwrap();
    }

    fn wrapup_messages(count: u64, words_per_message: usize) -> Vec<CkIngressMessage> {
        (1..=count)
            .map(|ordinal| {
                ck_with_role(
                    &format!("m{ordinal}"),
                    ordinal,
                    if ordinal % 2 == 0 {
                        "assistant"
                    } else {
                        "user"
                    },
                    &format!("message {ordinal} {}", "word ".repeat(words_per_message)),
                )
            })
            .collect()
    }

    fn cache_wrapup_messages(handler: &McHandler, messages: Vec<CkIngressMessage>) {
        cache_wrapup_messages_for_session(handler, "ses", messages);
    }

    fn cache_wrapup_messages_for_session(
        handler: &McHandler,
        session_id: &str,
        messages: Vec<CkIngressMessage>,
    ) {
        let mut parsed = transform_request(messages, 1, 200_000);
        parsed.session_id = session_id.to_string();
        parsed.serializer_profile = SerializerProfile::ClaudeCodeAnthropic.wire_id().to_string();
        let retained_bytes = serde_json::to_vec(&parsed).unwrap().len();
        let revert_epoch = handler
            .store
            .get()
            .unwrap()
            .load(session_id)
            .unwrap()
            .meta
            .revert_epoch;
        let mut snapshots = handler
            .transform_snapshots
            .lock()
            .expect("transform snapshots mutex");
        let generation = snapshots.begin(session_id);
        snapshots.finish_ready(
            session_id,
            generation,
            Arc::new(parsed),
            revert_epoch,
            retained_bytes,
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    struct HistorianAdditiveRows {
        compartment_count: usize,
        max_compartment_seq: i64,
        transcript_count: usize,
        max_transcript_seq: i64,
        fact_count: usize,
        max_fact_id: i64,
        publication_floor_ordinal: Option<u64>,
    }

    fn historian_additive_rows(
        store: &McStore,
        session_id: &str,
        project_path: &Path,
    ) -> HistorianAdditiveRows {
        let compartments = store.load_compartments(session_id).unwrap();
        let transcripts = store
            .load_chunk_transcripts_for_range(session_id, 0, i64::MAX)
            .unwrap();
        let project_path = project_path.to_string_lossy().to_string();
        let facts = store.load_active_memories(&project_path, 0).unwrap();
        HistorianAdditiveRows {
            compartment_count: compartments.len(),
            max_compartment_seq: store.max_compartment_seq(session_id).unwrap(),
            transcript_count: transcripts.len(),
            max_transcript_seq: transcripts
                .iter()
                .map(|transcript| transcript.compartment_seq)
                .max()
                .unwrap_or(0),
            fact_count: facts.len(),
            max_fact_id: store.max_memory_id(&[project_path]).unwrap(),
            publication_floor_ordinal: store
                .load(session_id)
                .unwrap()
                .meta
                .publication_floor_ordinal,
        }
    }

    fn queue_drop_command_with_id(handler: &McHandler, command_id: &str) -> Value {
        match handler.handle_agent_drops_value(
            7,
            json!({
                "method": "agent_drops.append",
                "session_id": "ses",
                "drop": "1",
                "command_id": command_id,
            }),
        ) {
            HandlerOutcome::Response(bytes) => serde_json::from_slice(&bytes).unwrap(),
            other => panic!("unexpected handler outcome: {other:?}"),
        }
    }

    fn historian_output_for_prompt(prompt: &str) -> String {
        let (start, end) = prompt_ordinal_range(prompt).unwrap_or((1, 3));
        historian_output(start, end, "autonomous summary")
    }

    fn historian_output(start: u64, end: u64, p1: &str) -> String {
        format!(
            r#"<output><compartments><compartment start="{start}" end="{end}" title="autonomous arc" episode_type="feature" importance="60"><p1>{p1}</p1><p2>short summary</p2><p3>arc</p3><p4 /></compartment></compartments><meta><messages_processed>{start}-{end}</messages_processed><unprocessed_from>{}</unprocessed_from></meta></output>"#,
            end + 1
        )
    }

    fn historian_output_with_fact(start: u64, end: u64, fact: &str) -> String {
        format!(
            r#"<output><compartments><compartment start="{start}" end="{end}" title="autonomous arc" episode_type="feature" importance="60"><p1>autonomous summary</p1><p2>short summary</p2><p3>arc</p3><p4 /></compartment></compartments><facts><PROJECT_RULES>* {fact}</PROJECT_RULES></facts><meta><messages_processed>{start}-{end}</messages_processed><unprocessed_from>{}</unprocessed_from></meta></output>"#,
            end + 1
        )
    }

    fn prompt_ordinal_range(prompt: &str) -> Option<(u64, u64)> {
        let mut ordinals = Vec::new();
        let bytes = prompt.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'[' {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1 {
                    if let Ok(value) = prompt[i + 1..j].parse::<u64>() {
                        ordinals.push(value);
                    }
                    if j < bytes.len() && bytes[j] == b'-' {
                        let mut k = j + 1;
                        while k < bytes.len() && bytes[k].is_ascii_digit() {
                            k += 1;
                        }
                        if k > j + 1 {
                            if let Ok(value) = prompt[j + 1..k].parse::<u64>() {
                                ordinals.push(value);
                            }
                        }
                    }
                }
            }
            i += 1;
        }
        Some((*ordinals.iter().min()?, *ordinals.iter().max()?))
    }

    /// Wall-clock budget for test waits on spawned-task progress. Iteration-bounded
    /// yield loops flake under parallel test load: 200 bare yields are microseconds,
    /// and a spawned task that loses the CPU race for that window fails the wait even
    /// though it completes fine. Time-bounded polling makes the wait load-immune.
    const TEST_WAIT_BUDGET: Duration = Duration::from_secs(10);
    const TEST_WAIT_POLL: Duration = Duration::from_millis(2);

    async fn wait_for_idle(store: &McStore) {
        let deadline = std::time::Instant::now() + TEST_WAIT_BUDGET;
        while std::time::Instant::now() < deadline {
            if store.load("ses").unwrap().meta.historian.state == HistorianPhase::Idle {
                return;
            }
            tokio::time::sleep(TEST_WAIT_POLL).await;
        }
        panic!("historian did not return to idle");
    }

    async fn wait_for_count(value: &AtomicUsize, expected: usize) {
        let deadline = std::time::Instant::now() + TEST_WAIT_BUDGET;
        while std::time::Instant::now() < deadline {
            if value.load(Ordering::SeqCst) >= expected {
                return;
            }
            tokio::time::sleep(TEST_WAIT_POLL).await;
        }
        panic!("counter did not reach {expected}");
    }

    async fn wait_for_historian_state<F>(store: &McStore, predicate: F)
    where
        F: Fn(&HistorianDurableState) -> bool,
    {
        let deadline = std::time::Instant::now() + TEST_WAIT_BUDGET;
        while std::time::Instant::now() < deadline {
            let state = store.load("ses").unwrap().meta.historian;
            if predicate(&state) {
                return;
            }
            tokio::time::sleep(TEST_WAIT_POLL).await;
        }
        panic!("historian state predicate did not become true");
    }

    fn m0_text(response: &Value) -> String {
        response["ck_messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["meta"]["synthetic"] == json!(true))
            .and_then(|message| message["content"][0]["kind"]["text"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authority_transform_uses_request_history_budget_on_hard() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        store
            .replace_compartments(
                "ses",
                &[
                    stored_comp(1, 1, 40, "m40", &"OLD ".repeat(200)),
                    stored_comp(2, 41, 80, "m80", &"NEW ".repeat(200)),
                ],
            )
            .unwrap();
        let mut request = request(big_messages());
        request["history_budget_tokens"] = json!(300.0);

        let response = call_transform_request(&handler, request).await;
        assert_eq!(response["action"], "HARD");
        let m0 = m0_text(&response);
        assert!(m0.contains("NEW"), "newest compartment remains at P1: {m0}");
        assert!(
            !m0.contains("OLD"),
            "request budget must reach the HARD decay renderer: {m0}"
        );
        let status = tool_body(handler.handle_session_status_value(
            7,
            &json!({ "method": "session.status", "v": 1, "session_id": "ses" }),
        ));
        let history = decay_render::extract_m0_block(&m0, "session-history").unwrap();
        assert_eq!(
            status["compartment_tokens"],
            json!(mc_tokenizer::estimate_tokens(&history))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_full_autonomous_cycle_fires_publishes_and_next_pass_folds() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();

        let first = call_transform(&handler, messages.clone()).await;
        assert_eq!(first["historian"]["fired"], true);
        wait_for_count(&producer.starts, 1).await;
        let prompt = producer.prompts.lock().unwrap()[0].clone();
        assert_eq!(prompt_ordinal_range(&prompt).unwrap().0, 1);
        wait_for_idle(&store).await;
        let compartments = store.load_compartments("ses").unwrap();
        assert_eq!(compartments.len(), 1);
        assert_eq!(compartments[0].start_message, 1);
        assert_eq!(producer.starts.load(Ordering::SeqCst), 1);

        let second = call_transform(&handler, messages).await;
        assert_eq!(second["action"], "HARD");
        assert!(second["boundary_id"]
            .as_str()
            .unwrap_or_default()
            .contains('#'));
        assert!(m0_text(&second).contains("autonomous summary"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_zero_based_autonomous_cycle_covers_ordinal_zero_and_folds() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages_from(0);

        let first = call_transform(&handler, messages.clone()).await;
        assert_eq!(first["historian"]["fired"], true);
        wait_for_count(&producer.starts, 1).await;
        let prompt = producer.prompts.lock().unwrap()[0].clone();
        assert_eq!(prompt_ordinal_range(&prompt).unwrap().0, 0);
        wait_for_idle(&store).await;
        let compartments = store.load_compartments("ses").unwrap();
        assert_eq!(compartments.len(), 1);
        assert_eq!(compartments[0].start_message, 0);

        let second = call_transform(&handler, messages).await;
        assert_eq!(second["action"], "HARD");
        assert!(second["boundary_id"]
            .as_str()
            .unwrap_or_default()
            .contains("#"));
        assert!(m0_text(&second).contains("autonomous summary"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_zero_based_system_lead_starts_chunk_at_first_user_and_folds() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = zero_based_messages_with_system_lead();

        let first = call_transform(&handler, messages.clone()).await;
        assert_eq!(first["historian"]["fired"], true);
        wait_for_count(&producer.starts, 1).await;
        let prompt = producer.prompts.lock().unwrap()[0].clone();
        assert_eq!(prompt_ordinal_range(&prompt).unwrap().0, 1);
        wait_for_idle(&store).await;
        let compartments = store.load_compartments("ses").unwrap();
        assert_eq!(compartments.len(), 1);
        assert_eq!(compartments[0].start_message, 1);

        let second = call_transform(&handler, messages).await;
        assert_eq!(second["action"], "HARD");
        assert!(m0_text(&second).contains("autonomous summary"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_import_two_batches_bootstrap_hard_folds_and_mints_tail_anchor() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());

        let staged = call_dispatch_request(
            &handler,
            state_import_request(
                "bundle-a",
                0,
                2,
                vec![imported_compartment(10, 1, 10, "m10#0", "summary one")],
            ),
        )
        .await;
        assert_eq!(staged, json!({ "ok": true, "staged": 1 }));
        assert!(store.load_compartments("ses").unwrap().is_empty());

        let committed = call_dispatch_request(
            &handler,
            state_import_request(
                "bundle-a",
                1,
                2,
                vec![imported_compartment(20, 11, 20, "m20#0", "summary two")],
            ),
        )
        .await;
        assert_eq!(
            committed,
            json!({ "ok": true, "imported": 2, "duplicate": false })
        );
        assert_eq!(store.load_compartments("ses").unwrap().len(), 2);
        assert!(store.has_compartments("ses").unwrap());
        let before_fold = store.load("ses").unwrap();
        assert!(before_fold.core.boundary_id.is_empty());
        assert!(before_fold.row_version.is_none());

        let response =
            call_transform_request(&handler, request_with_usage(big_messages(), 1_000, 50_000))
                .await;
        assert_eq!(response["action"], "HARD");
        assert_eq!(response["boundary_id"], "m20#0");
        let after_fold = store.load("ses").unwrap();
        assert_eq!(after_fold.core.boundary_id, "m20#0");
        assert!(after_fold.row_version.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_import_refuses_nonempty_session_without_writes() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let bootstrap = store.load("ses").unwrap();
        store
            .commit("ses", None, &bootstrap.core, &bootstrap.meta)
            .unwrap();
        let before = store.load("ses").unwrap().row_version;

        let outcome = handler
            .dispatch_value(
                7,
                state_import_request(
                    "bundle-a",
                    0,
                    1,
                    vec![imported_compartment(1, 1, 1, "m1#0", "summary")],
                ),
            )
            .await;
        assert_eq!(error_code(outcome), "session_not_empty");
        assert_eq!(store.load("ses").unwrap().row_version, before);
        assert!(store.load_compartments("ses").unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_import_id_is_durable_and_wins_before_nonempty_check() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let request = state_import_request(
            "bundle-a",
            0,
            1,
            vec![imported_compartment(1, 1, 1, "m1#0", "summary")],
        );
        let first = call_dispatch_request(&handler, request.clone()).await;
        assert_eq!(
            first,
            json!({ "ok": true, "imported": 1, "duplicate": false })
        );

        let bootstrap = store.load("ses").unwrap();
        let row_version = store
            .commit("ses", None, &bootstrap.core, &bootstrap.meta)
            .unwrap();
        let duplicate = call_dispatch_request(&handler, request).await;
        assert_eq!(
            duplicate,
            json!({ "ok": true, "imported": 1, "duplicate": true })
        );
        assert_eq!(store.load("ses").unwrap().row_version, Some(row_version));

        let different = handler
            .dispatch_value(
                7,
                state_import_request(
                    "bundle-b",
                    0,
                    1,
                    vec![imported_compartment(1, 1, 1, "m1#0", "other")],
                ),
            )
            .await;
        assert_eq!(error_code(different), "session_not_empty");
        assert_eq!(store.load("ses").unwrap().row_version, Some(row_version));
        assert_eq!(
            store.load_compartments("ses").unwrap()[0].content,
            "summary"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_import_batch_gap_and_staleness_evict_partial_attempts() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let staged = call_dispatch_request(
            &handler,
            state_import_request(
                "gap",
                0,
                3,
                vec![imported_compartment(1, 1, 1, "m1#0", "first")],
            ),
        )
        .await;
        assert_eq!(staged["staged"], 1);
        let gap = handler
            .dispatch_value(
                7,
                state_import_request(
                    "gap",
                    2,
                    3,
                    vec![imported_compartment(3, 3, 3, "m3#0", "third")],
                ),
            )
            .await;
        assert_eq!(error_code(gap), "batch_seq_mismatch");

        let restaged = call_dispatch_request(
            &handler,
            state_import_request(
                "stale",
                0,
                2,
                vec![imported_compartment(1, 1, 1, "m1#0", "first")],
            ),
        )
        .await;
        assert_eq!(restaged["staged"], 1, "gap rejection released the session");
        handler
            .state_imports
            .lock()
            .expect("state import mutex")
            .stale_after = Duration::ZERO;
        let imported = call_dispatch_request(
            &handler,
            state_import_request(
                "fresh",
                0,
                1,
                vec![imported_compartment(5, 1, 5, "m5#0", "replacement")],
            ),
        )
        .await;
        assert_eq!(
            imported,
            json!({ "ok": true, "imported": 1, "duplicate": false })
        );
        assert_eq!(
            store.load_compartments("ses").unwrap()[0].content,
            "replacement"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_import_structural_rejections_name_rules_and_leave_session_empty() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let cases = vec![
            (
                "overlap",
                vec![
                    imported_compartment(1, 1, 5, "m5#0", "first"),
                    imported_compartment(2, 5, 8, "m8#0", "second"),
                ],
                "ranges_overlap",
            ),
            (
                "seq",
                vec![
                    imported_compartment(2, 1, 2, "m2#0", "first"),
                    imported_compartment(1, 3, 4, "m4#0", "second"),
                ],
                "seq_not_increasing",
            ),
            (
                "empty-p1",
                vec![imported_compartment(1, 1, 1, "m1#0", "   ")],
                "p1_empty",
            ),
            (
                "bad-id",
                vec![imported_compartment(1, 1, 1, "m1", "summary")],
                "end_message_id_invalid",
            ),
            (
                "bad-range",
                vec![imported_compartment(1, 2, 1, "m1#0", "summary")],
                "range_invalid",
            ),
        ];
        for (import_id, compartments, expected_code) in cases {
            let outcome = handler
                .dispatch_value(7, state_import_request(import_id, 0, 1, compartments))
                .await;
            assert_eq!(error_code(outcome), expected_code, "{import_id}");
            assert!(store.load_compartments("ses").unwrap().is_empty());
            assert!(store.load("ses").unwrap().row_version.is_none());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_import_p1_only_shape_defaults_and_renders() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let imported = call_dispatch_request(
            &handler,
            state_import_request(
                "legacy-p1-only",
                0,
                1,
                vec![imported_compartment(
                    1,
                    1,
                    10,
                    "m10#0",
                    "P1-ONLY-IMPORTED-SUMMARY",
                )],
            ),
        )
        .await;
        assert_eq!(imported["imported"], 1);
        let row = &store.load_compartments("ses").unwrap()[0];
        assert_eq!(row.p1.as_deref(), Some("P1-ONLY-IMPORTED-SUMMARY"));
        assert_eq!(row.p2, None);
        assert_eq!(row.p3, None);
        assert_eq!(row.p4, None);
        assert_eq!(row.importance, 50);

        let response =
            call_transform_request(&handler, request_with_usage(big_messages(), 1_000, 50_000))
                .await;
        assert_eq!(response["action"], "HARD");
        assert!(m0_text(&response).contains("P1-ONLY-IMPORTED-SUMMARY"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn management_drop_alias_routes_are_rejected() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());
        for alias in ["ctx_reduce", "append_agent_drops"] {
            let outcome = handler
                .dispatch_value(
                    7,
                    json!({
                        "method": alias,
                        "session_id": "ses",
                        "drop": "1",
                        "command_id": "command"
                    }),
                )
                .await;
            assert_eq!(error_code(outcome), "unrecognized_request_shape", "{alias}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn management_todo_flush_and_recomp_contracts_are_replay_safe() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let todo = json!({
            "method": "todo_state.set",
            "v": 1,
            "session_id": "ses",
            "state_json": "[{\"content\":\"ship module\",\"status\":\"in_progress\",\"priority\":\"high\"}]",
            "owner_message_id": "m1",
        });
        assert_eq!(
            call_dispatch_request(&handler, todo.clone()).await,
            json!({ "ok": true })
        );
        let first = store.load("ses").unwrap();
        assert_eq!(
            first.meta.last_todo_state_owner_message_id.as_deref(),
            Some("m1")
        );
        assert!(first.meta.last_todo_state_hash.is_some());
        let first_version = first.row_version;
        assert_eq!(
            call_dispatch_request(&handler, todo).await,
            json!({ "ok": true })
        );
        assert_eq!(store.load("ses").unwrap().row_version, first_version);

        let flush = call_dispatch_request(
            &handler,
            json!({ "method": "session.flush", "v": 1, "session_id": "ses" }),
        )
        .await;
        assert_eq!(flush, json!({ "ok": true, "armed": true }));
        let flushed = store.load("ses").unwrap();
        assert!(flushed.meta.soft_refresh_pending);

        let recomp = call_dispatch_request(
            &handler,
            json!({
                "method": "session.recomp",
                "v": 1,
                "session_id": "ses",
                "command_id": "recomp-1",
            }),
        )
        .await;
        assert_eq!(
            recomp,
            json!({ "ok": true, "disposition": "nothing_to_do" })
        );
        let replay = call_dispatch_request(
            &handler,
            json!({
                "method": "session.recomp",
                "v": 1,
                "session_id": "ses",
                "command_id": "recomp-1",
            }),
        )
        .await;
        assert_eq!(replay, recomp);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_status_compartment_pages_are_bounded_and_contract_shaped() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        store
            .commit_state_import(
                "ses",
                "status-page-seed",
                &(1..=55)
                    .map(|sequence| {
                        stored_comp(
                            sequence,
                            sequence,
                            sequence,
                            &format!("m{sequence}"),
                            "body",
                        )
                    })
                    .collect::<Vec<_>>(),
                55,
            )
            .unwrap();

        let body = tool_body(handler.handle_session_status_value(
            7,
            &json!({
                "method": "session.status",
                "v": 1,
                "session_id": "ses",
                "include_compartments_after_seq": -1,
            }),
        ));
        let compartments = body["compartments"].as_array().unwrap();
        assert_eq!(compartments.len(), 50);
        assert_eq!(compartments[0]["sequence"], json!(1));
        assert_eq!(compartments[49]["sequence"], json!(50));
        assert_eq!(body["max_sequence"], json!(55));
        assert!(compartments[0].get("start_date").is_none());
        assert!(compartments[0].get("legacy").is_none());

        let tail = tool_body(handler.handle_session_status_value(
            7,
            &json!({
                "method": "session.status",
                "v": 1,
                "session_id": "ses",
                "include_compartments_after_seq": 50,
            }),
        ));
        assert_eq!(tail["compartments"].as_array().unwrap().len(), 5);
        assert_eq!(tail["max_sequence"], json!(55));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_recomp_resets_cache_boundary_and_replays_started() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        store
            .commit_state_import(
                "ses",
                "recomp-seed",
                &[stored_comp(1, 1, 5, "m5", "seed")],
                1,
            )
            .unwrap();
        let before = store.load("ses").unwrap();
        let mut core = before.core.clone();
        core.boundary_id = "m5#0".to_string();
        let mut meta = before.meta.clone();
        meta.initialized = true;
        meta.coverage_ordinal = Some(5);
        store
            .commit("ses", before.row_version, &core, &meta)
            .unwrap();
        let before_reset = store.load("ses").unwrap();

        let first = call_dispatch_request(
            &handler,
            json!({
                "method": "session.recomp",
                "v": 1,
                "session_id": "ses",
                "command_id": "recomp-reset",
            }),
        )
        .await;
        assert_eq!(first, json!({ "ok": true, "disposition": "started" }));
        let after = store.load("ses").unwrap();
        assert!(store.load_compartments("ses").unwrap().is_empty());
        assert!(after.core.boundary_id.is_empty());
        assert!(!after.meta.initialized);
        assert!(after.meta.coverage_ordinal.is_none());
        assert_eq!(after.meta.revert_epoch, before_reset.meta.revert_epoch + 1);
        assert_eq!(
            after.row_version,
            Some(before_reset.row_version.unwrap() + 1)
        );

        let replay = call_dispatch_request(
            &handler,
            json!({
                "method": "session.recomp",
                "v": 1,
                "session_id": "ses",
                "command_id": "recomp-reset",
            }),
        )
        .await;
        assert_eq!(replay, first);
        assert_eq!(store.load("ses").unwrap().row_version, after.row_version);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_flush_consumes_once_as_soft_without_forcing_hard() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        store
            .commit_state_import(
                "ses",
                "flush-seed",
                &[stored_comp(1, 1, 1, "m1", "seed")],
                1,
            )
            .unwrap();
        let first = call_transform(&handler, vec![ck("m1", 1, "hello")]).await;
        assert_eq!(first["action"], "HARD");
        let armed = call_dispatch_request(
            &handler,
            json!({ "method": "session.flush", "v": 1, "session_id": "ses" }),
        )
        .await;
        assert_eq!(armed, json!({ "ok": true, "armed": true }));
        let next = call_transform(&handler, vec![ck("m1", 1, "hello")]).await;
        assert_eq!(next["action"], "SOFT");
        assert!(!store.load("ses").unwrap().meta.soft_refresh_pending);
        let replay = call_transform(&handler, vec![ck("m1", 1, "hello")]).await;
        assert_ne!(replay["action"], "HARD");
    }

    #[test]
    fn session_status_rejects_unbound_and_mismatched_routes() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(producer, default_test_config());
        let request = json!({ "method": "session.status", "v": 1, "session_id": "ses" });

        assert_eq!(
            error_code(handler.handle_session_status_value(8, &request)),
            "route_unbound"
        );
        let mismatch = json!({
            "method": "session.status",
            "v": 1,
            "session_id": "other",
        });
        assert_eq!(
            error_code(handler.handle_session_status_value(7, &mismatch)),
            "session_mismatch"
        );
    }

    #[test]
    fn session_status_summarizes_seeded_store_with_identity_and_durable_age() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, project) = handler_with_store(producer, default_test_config());
        let session_id = "ccm-8518e338-extra";
        handler.bind_route(7, binding(project.to_str().unwrap(), session_id));
        store
            .commit_state_import(
                session_id,
                "status-seed",
                &[
                    stored_comp(1, 1, 5, "m5", "first"),
                    stored_comp(2, 6, 10, "m10", "second"),
                ],
                1,
            )
            .unwrap();
        let loaded = store.load(session_id).unwrap();
        let mut core = loaded.core.clone();
        core.boundary_id = "m10#0".to_string();
        let mut meta = loaded.meta.clone();
        meta.coverage_ordinal = Some(10);
        meta.cc_u1_active = true;
        meta.last_committed_pass_at_ms = now_ms() - 125_000;
        meta.historian.firing_seq = 3;
        store
            .commit(session_id, loaded.row_version, &core, &meta)
            .unwrap();
        store
            .append_pending_agent_drops(session_id, &["m8#0".to_string()], 2)
            .unwrap();
        store
            .seed_tags_for_test(
                session_id,
                &[
                    TagMintInput {
                        block_id: "m8#0".to_string(),
                        kind: "message".to_string(),
                        token_count: 2,
                        source_bytes: b"eight".to_vec(),
                    },
                    TagMintInput {
                        block_id: "m9#0".to_string(),
                        kind: "message".to_string(),
                        token_count: 2,
                        source_bytes: b"nine".to_vec(),
                    },
                ],
                2,
            )
            .unwrap();

        let body = tool_body(handler.handle_session_status_value(
            7,
            &json!({ "method": "session.status", "v": 1, "session_id": session_id }),
        ));
        let summary = body["summary"].as_str().unwrap();

        assert!(summary.starts_with("session ccm-8518e338 (last active 2m ago):"));
        assert!(summary.contains("2 compartments"));
        assert!(summary.contains("coverage ordinal 10"));
        assert!(summary.contains("boundary present"));
        assert!(summary.contains("1 pending drop"));
        assert!(summary.contains("2 tags"));
        assert!(summary.contains("last historian: published seq 3"));
        assert!(summary.ends_with("surface active"));
    }

    #[test]
    fn session_status_rereads_when_wrapup_latch_changes_during_snapshot() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let handler = Arc::new(handler);
        let hook_handler = Arc::clone(&handler);
        let hook_store = Arc::clone(&store);
        *handler
            .status_snapshot_hook
            .lock()
            .expect("status snapshot hook mutex") = Some(Box::new(move || {
            let loaded = hook_store.load("ses").unwrap();
            let mut meta = loaded.meta.clone();
            meta.coverage_ordinal = Some(7);
            hook_store
                .commit("ses", loaded.row_version, &loaded.core, &meta)
                .unwrap();
            hook_handler
                .wrapup_sessions
                .lock()
                .expect("wrapup sessions mutex")
                .insert(
                    "ses".to_string(),
                    LiveWrapupSession {
                        token: Arc::new(()),
                        rounds: 3,
                    },
                );
        }));

        let body = tool_body(handler.handle_session_status_value(
            7,
            &json!({ "method": "session.status", "v": 1, "session_id": "ses" }),
        ));
        assert_eq!(body["wrapup_active"], json!(true));
        assert_eq!(body["wrapup_rounds"], json!(3));
        assert_eq!(body["coverage_ordinal"], json!(7));
        assert_eq!(body["row_version"], json!(1));
    }

    #[test]
    fn session_status_sanitizes_controls_and_caps_summary() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let loaded = store.load("ses").unwrap();
        let mut meta = loaded.meta.clone();
        meta.historian.last_failure = Some(format!("bad\0line\n{}", "x".repeat(2_000)));
        store
            .commit("ses", loaded.row_version, &loaded.core, &meta)
            .unwrap();

        let body = tool_body(handler.handle_session_status_value(
            7,
            &json!({ "method": "session.status", "v": 1, "session_id": "ses" }),
        ));
        let summary = body["summary"].as_str().unwrap();
        assert!(!summary.chars().any(char::is_control));
        assert!(!summary.contains('\n'));
        assert!(summary.chars().count() <= 500);
    }

    #[test]
    fn agent_drops_append_rejects_missing_command_id() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        mint_drop_tag(&store, "a#0");

        let outcome = handler.handle_agent_drops_value(
            7,
            json!({
                "method": "agent_drops.append",
                "session_id": "ses",
                "drop": "1",
            }),
        );
        assert_eq!(error_code(outcome), "bad_request");
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());
    }

    #[test]
    fn ctx_reduce_command_id_is_idempotent_while_drops_are_pending() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());

        mint_drop_tag(&store, "a#0");
        let first = queue_drop_command_with_id(&handler, "tool-use-1");
        assert_eq!(first, json!({ "ok": true, "queued": 1 }));
        let pending = store.load_pending_agent_drops("ses").unwrap();

        let retry = queue_drop_command_with_id(&handler, "tool-use-1");
        assert_eq!(retry, json!({ "ok": true, "queued": 0, "duplicate": true }));
        assert_eq!(store.load_pending_agent_drops("ses").unwrap(), pending);
    }

    #[test]
    fn ctx_reduce_no_targets_append_records_terminal_disposition() {
        // When a drop range references tag numbers that don't exist in the session,
        // the ledger row is still recorded (for idempotency) but with disposition='no_targets'
        // and queued=0. A retry of the same command_id still dedupes.
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());

        // No tags minted, so drop "1" resolves zero targets.
        let outcome = handler.handle_agent_drops_value(
            7,
            json!({
                "method": "agent_drops.append",
                "session_id": "ses",
                "drop": "1",
                "command_id": "no-target-cmd",
            }),
        );
        let body: Value = match outcome {
            HandlerOutcome::Response(bytes) => serde_json::from_slice(&bytes).unwrap(),
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert_eq!(
            body,
            json!({ "ok": true, "queued": 0, "disposition": "no_targets" })
        );
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());

        // Retry of the same command_id must still dedupe (idempotency).
        let retry = handler.handle_agent_drops_value(
            7,
            json!({
                "method": "agent_drops.append",
                "session_id": "ses",
                "drop": "1",
                "command_id": "no-target-cmd",
            }),
        );
        let retry_body: Value = match retry {
            HandlerOutcome::Response(bytes) => serde_json::from_slice(&bytes).unwrap(),
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert_eq!(
            retry_body,
            json!({ "ok": true, "queued": 0, "duplicate": true })
        );
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opencode_raw_drop_range_resolves_minted_tags_and_drains_on_next_bust() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let messages = (1..=25u64)
            .map(|ordinal| {
                let mid = format!("m{ordinal}");
                let text = format!("output {ordinal}");
                ck(&mid, ordinal, &text)
            })
            .collect::<Vec<_>>();
        let mut transform_request = request(messages);
        transform_request["serializer_profile"] = json!("opencode-aisdk");
        transform_request["tool_present"] = json!(true);
        transform_request["serve_native"] = json!(true);

        let transition = call_transform_request(&handler, transform_request.clone()).await;
        assert_eq!(transition["surface_state"], "transition");
        let tagged = call_transform_request(&handler, transform_request.clone()).await;
        assert_eq!(tagged["surface_state"], "active");
        let tagged_bytes = serde_json::to_string(&tagged["ck_messages"]).unwrap();
        assert!(tagged_bytes.contains("§1§ output 1"));
        assert!(tagged_bytes.contains("§2§ output 2"));
        assert!(tagged_bytes.contains("§3§ output 3"));
        assert_eq!(store.load_tags_for_session("ses").unwrap().len(), 25);

        let queued = match handler.handle_agent_drops_value(
            7,
            json!({
                "method": "agent_drops.append",
                "session_id": "ses",
                "drop": "1-3",
                "command_id": "opencode-drop-range",
            }),
        ) {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected handler outcome: {other:?}"),
        };
        assert_eq!(queued, json!({ "ok": true, "queued": 3 }));

        transform_request["render_config"] = json!("cfg1");
        let drained = call_transform_request(&handler, transform_request).await;
        let drained_bytes = serde_json::to_string(&drained["ck_messages"]).unwrap();
        assert!(drained_bytes.contains("[dropped §1§]"));
        assert!(drained_bytes.contains("[dropped §2§]"));
        assert!(drained_bytes.contains("[dropped §3§]"));
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ctx_reduce_command_id_survives_transform_drain() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        mint_drop_tag(&store, "a#0");
        let queued = queue_drop_command_with_id(&handler, "tool-use-1");
        assert_eq!(queued, json!({ "ok": true, "queued": 1 }));
        assert_eq!(store.load_pending_agent_drops("ses").unwrap().len(), 1);

        let mut transform_request = request(vec![ck("a", 1, "drop me")]);
        transform_request["serializer_profile"] = json!("claude-code-anthropic");
        transform_request["tool_present"] = json!(true);
        let response = call_transform_request(&handler, transform_request).await;
        assert!(serde_json::to_string(&response)
            .unwrap()
            .contains("[dropped]"));
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());

        let retry = queue_drop_command_with_id(&handler, "tool-use-1");
        assert_eq!(retry, json!({ "ok": true, "queued": 0, "duplicate": true }));
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());

        let new_request = queue_drop_command_with_id(&handler, "tool-use-2");
        assert_eq!(new_request, json!({ "ok": true, "queued": 1 }));
        assert_eq!(store.load_pending_agent_drops("ses").unwrap().len(), 1);
    }

    #[test]
    fn ctx_reduce_command_rejects_empty_and_oversized_command_ids() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());

        for command_id in [json!(""), json!(" \t "), json!("x".repeat(129))] {
            let outcome = handler.handle_agent_drops_value(
                7,
                json!({
                    "method": "agent_drops.append",
                    "session_id": "ses",
                    "drop": "1",
                    "command_id": command_id,
                }),
            );
            assert_eq!(error_code(outcome), "bad_request");
        }
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());
    }

    #[test]
    fn agent_drops_append_rejects_missing_or_empty_raw_drop() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        for drop in [Value::Null, json!(""), json!("  "), json!(["1"])] {
            let outcome = handler.handle_agent_drops_value(
                7,
                json!({
                    "method": "agent_drops.append",
                    "session_id": "ses",
                    "drop": drop,
                    "command_id": "command"
                }),
            );
            assert_eq!(error_code(outcome), "bad_request");
        }
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ctx_reduce_command_raw_drop_string_canonicalizes_server_side() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        store
            .seed_tags_for_test(
                "ses",
                &[
                    TagMintInput {
                        block_id: "a#0".to_string(),
                        kind: "tool_result".to_string(),
                        token_count: 10,
                        source_bytes: b"one".to_vec(),
                    },
                    TagMintInput {
                        block_id: "b#0".to_string(),
                        kind: "tool_result".to_string(),
                        token_count: 10,
                        source_bytes: b"two".to_vec(),
                    },
                ],
                1_000,
            )
            .unwrap();

        // Range syntax plus an unknown tag number: known tags queue, the unknown
        // number is skipped (the tee replays whatever the model said).
        let response = match handler.handle_agent_drops_value(
            7,
            json!({
                "method": "agent_drops.append",
                "session_id": "ses",
                "drop": "§1§-2, 99",
                "command_id": "raw-1",
            }),
        ) {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected handler outcome: {other:?}"),
        };
        assert_eq!(response, json!({ "ok": true, "queued": 2 }));
        let pending = store.load_pending_agent_drops("ses").unwrap();
        assert_eq!(pending.len(), 2);

        // Re-sending the same raw string is idempotent (structural INSERT OR IGNORE).
        let repeat = match handler.handle_agent_drops_value(
            7,
            json!({
                "method": "agent_drops.append",
                "session_id": "ses",
                "drop": "1-2",
                "command_id": "raw-2",
            }),
        ) {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected handler outcome: {other:?}"),
        };
        assert_eq!(repeat, json!({ "ok": true, "queued": 0 }));

        // Malformed range syntax is a typed bad_request, nothing partially queued.
        match handler.handle_agent_drops_value(
            7,
            json!({
                "method": "agent_drops.append",
                "session_id": "ses",
                "drop": "3-1",
                "command_id": "raw-bad",
            }),
        ) {
            HandlerOutcome::Error { code, .. } => assert_eq!(code, "bad_request"),
            other => panic!("expected bad_request, got: {other:?}"),
        }
        assert_eq!(store.load_pending_agent_drops("ses").unwrap().len(), 2);
    }

    #[test]
    fn ctx_reduce_command_id_works_with_raw_drop_string() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        store
            .seed_tags_for_test(
                "ses",
                &[TagMintInput {
                    block_id: "a#0".to_string(),
                    kind: "tool_result".to_string(),
                    token_count: 10,
                    source_bytes: b"one".to_vec(),
                }],
                1_000,
            )
            .unwrap();

        let first = match handler.handle_agent_drops_value(
            7,
            json!({
                "method": "agent_drops.append",
                "session_id": "ses",
                "drop": "1-3",
                "command_id": " tool-use-raw ",
            }),
        ) {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected handler outcome: {other:?}"),
        };
        assert_eq!(first, json!({ "ok": true, "queued": 1 }));

        let retry = match handler.handle_agent_drops_value(
            7,
            json!({
                "method": "agent_drops.append",
                "session_id": "ses",
                "drop": "1-3",
                "command_id": "tool-use-raw",
            }),
        ) {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected handler outcome: {other:?}"),
        };
        assert_eq!(retry, json!({ "ok": true, "queued": 0, "duplicate": true }));
        assert_eq!(store.load_pending_agent_drops("ses").unwrap().len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retryable_snapshot_conditions_do_not_poison_command_ids() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let missing_request = json!({
            "method": "session.wrapup",
            "v": 1,
            "session_id": "ses",
            "command_id": "missing-retry"
        });
        let missing = tool_body(handler.dispatch_value(7, missing_request.clone()).await);
        assert_eq!(missing["disposition"], json!("retryable"));
        assert_eq!(missing["reason"], json!("snapshot_unavailable"));
        assert!(store
            .load_wrapup_command("ses", "missing-retry")
            .unwrap()
            .is_none());
        cache_wrapup_messages(&handler, wrapup_messages(20, 40));
        let retry = tool_body(handler.dispatch_value(7, missing_request).await);
        assert_eq!(retry["disposition"], json!("nothing_to_compact"));
        assert!(store
            .load_wrapup_command("ses", "missing-retry")
            .unwrap()
            .is_some());

        let mut malformed = transform_request(wrapup_messages(20, 40), 0, 200_000);
        malformed.messages[0].mid = "reserved#mid".to_string();
        let generation = handler
            .transform_snapshots
            .lock()
            .expect("transform snapshots mutex")
            .begin("ses");
        let revert_epoch = store.load("ses").unwrap().meta.revert_epoch;
        handler
            .transform_snapshots
            .lock()
            .expect("transform snapshots mutex")
            .finish_ready("ses", generation, Arc::new(malformed), revert_epoch, 1);
        let malformed_request = json!({
            "method": "session.wrapup",
            "v": 1,
            "session_id": "ses",
            "command_id": "malformed-retry"
        });
        let malformed_response =
            tool_body(handler.dispatch_value(7, malformed_request.clone()).await);
        assert_eq!(malformed_response["disposition"], json!("retryable"));
        assert_eq!(malformed_response["reason"], json!("snapshot_unavailable"));
        assert!(store
            .load_wrapup_command("ses", "malformed-retry")
            .unwrap()
            .is_none());
        cache_wrapup_messages(&handler, wrapup_messages(20, 40));
        let retry = tool_body(handler.dispatch_value(7, malformed_request).await);
        assert_eq!(retry["disposition"], json!("nothing_to_compact"));
        assert!(store
            .load_wrapup_command("ses", "malformed-retry")
            .unwrap()
            .is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_wrapup_no_models_is_terminal_and_retains_command() {
        let producer = Arc::new(ProducerState::default());
        let mut config = default_test_config();
        config.model_chain.clear();
        let (handler, store, _dir, _project) = handler_with_store(Arc::clone(&producer), config);
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        let request = json!({
            "method": "session.wrapup",
            "v": 1,
            "session_id": "ses",
            "command_id": "no-models"
        });

        let response = tool_body(handler.dispatch_value(7, request.clone()).await);
        assert_eq!(response["ok"], json!(false), "{response}");
        assert_eq!(response["disposition"], json!("failed"), "{response}");
        assert_eq!(response["reason"], json!("no_models"), "{response}");
        assert!(
            response["rounds"].is_u64(),
            "terminal failure carries rounds: {response}"
        );
        assert!(response["detail"]
            .as_str()
            .unwrap()
            .contains("no historian models"));
        let row = store
            .load_wrapup_command("ses", "no-models")
            .unwrap()
            .expect("terminal failure row");
        assert_eq!(row.disposition, "failed");
        assert_eq!(row.rounds, response["rounds"].as_u64().unwrap() as usize);
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);

        let replay = tool_body(handler.dispatch_value(7, request).await);
        assert_eq!(replay["ok"], json!(false), "{replay}");
        assert_eq!(replay["reason"], json!("no_models"), "{replay}");
        assert_eq!(replay["replayed"], json!(true));
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_wrapup_unknown_module_retries_once_then_is_terminal() {
        let producer = Arc::new(ProducerState::default());
        producer
            .start_errors
            .lock()
            .expect("start errors mutex")
            .extend([
                Err(HistorianProducerError::Subc(
                    historian_producer::ProducerErrorBody::untagged(
                        "unknown_module",
                        "runner module broca is unavailable",
                    ),
                )),
                Err(HistorianProducerError::Subc(
                    historian_producer::ProducerErrorBody::untagged(
                        "unknown_module",
                        "runner module broca is unavailable",
                    ),
                )),
            ]);
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        *handler
            .unknown_module_retry_delay
            .lock()
            .expect("unknown module retry delay mutex") = Some(Duration::ZERO);
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        let request = json!({
            "method": "session.wrapup",
            "v": 1,
            "session_id": "ses",
            "command_id": "runner-missing"
        });

        let response = tool_body(handler.dispatch_value(7, request.clone()).await);
        assert_eq!(response["ok"], json!(false), "{response}");
        assert_eq!(response["disposition"], json!("failed"), "{response}");
        assert_eq!(
            response["reason"],
            json!("runner_module_unavailable"),
            "{response}"
        );
        assert!(
            response["rounds"].is_u64(),
            "terminal failure carries rounds: {response}"
        );
        assert!(response["detail"]
            .as_str()
            .unwrap()
            .contains("unknown_module"));
        assert_eq!(producer.starts.load(Ordering::SeqCst), 2);
        assert_eq!(
            store
                .load_wrapup_command("ses", "runner-missing")
                .unwrap()
                .unwrap()
                .disposition,
            "failed"
        );

        let replay = tool_body(handler.dispatch_value(7, request).await);
        assert_eq!(replay["replayed"], json!(true));
        assert_eq!(replay["reason"], json!("runner_module_unavailable"));
        assert_eq!(producer.starts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_wrapup_drives_to_keep_watermark_and_reinvoke_is_idle() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));

        let body = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses" }),
                )
                .await,
        );

        assert_eq!(body["ok"], json!(true), "{body}");
        assert_eq!(body["disposition"], json!("completed"), "{body}");
        assert!(body["summary"]
            .as_str()
            .unwrap()
            .contains("takes effect on your next message"));
        let starts = producer.starts.load(Ordering::SeqCst);
        assert!((2..=historian::MAX_WRAPUP_ROUNDS).contains(&starts));
        let final_end = store
            .load_compartments("ses")
            .unwrap()
            .iter()
            .map(|compartment| compartment.end_message)
            .max()
            .unwrap();
        assert_eq!(final_end, 60);

        let retry = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses" }),
                )
                .await,
        );
        assert_eq!(retry["ok"], json!(true));
        assert_eq!(retry["disposition"], json!("nothing_to_compact"));
        assert!(retry["summary"]
            .as_str()
            .unwrap()
            .contains("nothing to compact"));
        assert_eq!(producer.starts.load(Ordering::SeqCst), starts);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authority_wrapup_publication_promotes_facts_under_the_identity_key() {
        let producer = Arc::new(ProducerState::default());
        *producer.next_fact.lock().unwrap() = Some("identity wrapup fact".to_string());
        let (handler, store, _dir, project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let route_project_root = project.to_str().unwrap();
        activate_module_authority(
            &store,
            "context",
            "git:identity",
            route_project_root,
            "memories",
        );
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));

        let response = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses" }),
                )
                .await,
        );

        assert_eq!(response["disposition"], json!("completed"), "{response}");
        assert!(store
            .load_active_memories("git:identity", 0)
            .unwrap()
            .iter()
            .any(|memory| memory.content == "identity wrapup fact"));
        assert!(store
            .load_active_memories(route_project_root, 0)
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrapup_handler_contract_edges_hold() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));

        // Empty command_id must reject: every retrying caller would otherwise share
        // one durable ledger key.
        let empty_id = handler
            .dispatch_value(
                7,
                json!({
                    "method": "session.wrapup",
                    "v": 1,
                    "session_id": "ses",
                    "command_id": ""
                }),
            )
            .await;
        match empty_id {
            HandlerOutcome::Error { code, message } => {
                assert_eq!(code, "bad_request");
                assert!(message.contains("nonempty"), "{message}");
            }
            other => panic!("empty command_id must reject, got {other:?}"),
        }

        // Negative keep is a clamp input, not an error: [WRAPUP_KEEP_MIN, WRAPUP_KEEP_MAX].
        let negative_keep = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({
                        "method": "session.wrapup",
                        "v": 1,
                        "session_id": "ses",
                        "keep": -3
                    }),
                )
                .await,
        );
        assert_eq!(negative_keep["ok"], json!(true), "{negative_keep}");
        assert_eq!(negative_keep["disposition"], json!("completed"));
        // keep=-3 clamps to WRAPUP_KEEP_MIN=5: coverage must reach past keep=20's
        // watermark (60), proving the clamp floor was used, not the default or a reject.
        let final_end = _store
            .load_compartments("ses")
            .unwrap()
            .iter()
            .map(|compartment| compartment.end_message)
            .max()
            .unwrap();
        assert!(
            final_end > 60,
            "keep must clamp to MIN=5, got end {final_end}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn already_in_progress_response_carries_rounds() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        let guard = handler
            .try_claim_wrapup_session("ses")
            .expect("first claim");
        let busy = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses" }),
                )
                .await,
        );
        drop(guard);
        assert_eq!(busy["disposition"], json!("already_in_progress"), "{busy}");
        assert!(
            busy["rounds"].is_u64(),
            "machine contract: rounds must ride every disposition, got {busy}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_wrapup_command_replays_verbatim_without_a_second_drive() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        let request = json!({
            "method": "session.wrapup",
            "v": 1,
            "session_id": "ses",
            "command_id": "wrapup-one"
        });

        let first = tool_body(handler.dispatch_value(7, request.clone()).await);
        assert_eq!(first["disposition"], json!("completed"), "{first}");
        let starts = producer.starts.load(Ordering::SeqCst);
        let stored = store
            .load_wrapup_command("ses", "wrapup-one")
            .unwrap()
            .expect("terminal command row");
        assert_eq!(stored.disposition, first["disposition"].as_str().unwrap());
        assert_eq!(json!(stored.rounds), first["rounds"]);
        assert_eq!(stored.summary, first["summary"].as_str().unwrap());

        let mut retry = request;
        retry["keep"] = json!("ignored-on-replay");
        let replay = tool_body(handler.dispatch_value(7, retry).await);
        assert_eq!(replay["replayed"], json!(true));
        assert_eq!(replay["disposition"], first["disposition"]);
        assert_eq!(replay["rounds"], first["rounds"]);
        assert_eq!(replay["summary"], first["summary"]);
        assert_eq!(producer.starts.load(Ordering::SeqCst), starts);

        let different = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({
                        "method": "session.wrapup",
                        "v": 1,
                        "session_id": "ses",
                        "command_id": "wrapup-two"
                    }),
                )
                .await,
        );
        assert_eq!(different["disposition"], json!("nothing_to_compact"));
        assert_eq!(different.get("replayed"), None);
        assert!(store
            .load_wrapup_command("ses", "wrapup-two")
            .unwrap()
            .is_some());
        assert_eq!(producer.starts.load(Ordering::SeqCst), starts);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn legacy_failed_wrapup_row_is_replaced_and_response_loss_replays() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        store
            .seed_legacy_wrapup_command_for_test(
                "ses",
                "legacy-failed",
                "failed",
                1,
                "old failure",
                17,
            )
            .unwrap();
        let request = json!({
            "method": "session.wrapup",
            "v": 1,
            "session_id": "ses",
            "keep": 5,
            "command_id": "legacy-failed"
        });

        let first = tool_body(handler.dispatch_value(7, request.clone()).await);
        assert_ne!(first.get("replayed"), Some(&json!(true)), "{first}");
        assert_eq!(first["disposition"], json!("completed"), "{first}");
        let starts_after_lost_response = producer.starts.load(Ordering::SeqCst);
        assert!(starts_after_lost_response > 0);
        let stored = store
            .load_wrapup_command("ses", "legacy-failed")
            .unwrap()
            .expect("successful result replaces the failed audit row");
        assert_eq!(stored.disposition, "completed");
        assert_eq!(stored.rounds, first["rounds"].as_u64().unwrap() as usize);
        assert_eq!(stored.summary, first["summary"].as_str().unwrap());
        assert!(stored.summary.ends_with("; replaced failed record from 17"));

        // Simulate losing the first response. The same command id must replay the
        // durable terminal result without opening another producer run.
        let replay = tool_body(handler.dispatch_value(7, request).await);
        assert_eq!(replay["replayed"], json!(true), "{replay}");
        assert_eq!(replay["disposition"], first["disposition"]);
        assert_eq!(replay["rounds"], first["rounds"]);
        assert_eq!(replay["summary"], first["summary"]);
        assert_eq!(
            producer.starts.load(Ordering::SeqCst),
            starts_after_lost_response
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrapup_connect_failure_without_durable_backoff_is_snapshot_unavailable() {
        let producer = Arc::new(ProducerState::default());
        producer
            .connect_errors
            .lock()
            .expect("connect errors mutex")
            .push_back(HistorianProducerError::Connect {
                endpoint: "127.0.0.1:1".to_string(),
                source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
            });
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        let conflict_store = Arc::clone(&store);
        *handler
            .connect_failure_commit_hook
            .lock()
            .expect("connect failure commit hook mutex") = Some(Box::new(move || {
            let loaded = conflict_store.load("ses").unwrap();
            conflict_store
                .commit("ses", loaded.row_version, &loaded.core, &loaded.meta)
                .unwrap();
        }));

        let response = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({
                        "method": "session.wrapup",
                        "v": 1,
                        "session_id": "ses",
                        "keep": 5,
                        "command_id": "connect-conflict"
                    }),
                )
                .await,
        );
        assert_eq!(response["disposition"], json!("retryable"), "{response}");
        assert_eq!(response["reason"], json!("snapshot_unavailable"));
        assert!(store
            .load("ses")
            .unwrap()
            .meta
            .historian
            .failure_backoff_at_ms
            .is_none());
        assert!(store
            .load_wrapup_command("ses", "connect-conflict")
            .unwrap()
            .is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrapup_connect_failure_retries_one_cas_conflict_and_arms_backoff() {
        let producer = Arc::new(ProducerState::default());
        producer
            .connect_errors
            .lock()
            .expect("connect errors mutex")
            .push_back(HistorianProducerError::Connect {
                endpoint: "127.0.0.1:1".to_string(),
                source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
            });
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        let conflict_store = Arc::clone(&store);
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let hook_calls_for_hook = Arc::clone(&hook_calls);
        *handler
            .connect_failure_commit_hook
            .lock()
            .expect("connect failure commit hook mutex") = Some(Box::new(move || {
            if hook_calls_for_hook.fetch_add(1, Ordering::SeqCst) == 0 {
                let loaded = conflict_store.load("ses").unwrap();
                conflict_store
                    .commit("ses", loaded.row_version, &loaded.core, &loaded.meta)
                    .unwrap();
            }
        }));

        let response = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({
                        "method": "session.wrapup",
                        "v": 1,
                        "session_id": "ses",
                        "keep": 5,
                        "command_id": "connect-retry"
                    }),
                )
                .await,
        );
        assert_eq!(response["disposition"], json!("retryable"), "{response}");
        assert_eq!(response["reason"], json!("backoff_active"));
        assert_eq!(hook_calls.load(Ordering::SeqCst), 2);
        assert!(store
            .load("ses")
            .unwrap()
            .meta
            .historian
            .failure_backoff_at_ms
            .is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrapup_publish_recut_is_retryable_and_never_recorded() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        let hook_store = Arc::clone(&store);
        *producer
            .on_await_output
            .lock()
            .expect("await-output hook mutex") = Some(Box::new(move || {
            let loaded = hook_store.load("ses").unwrap();
            let mut meta = loaded.meta.clone();
            meta.revert_epoch = meta.revert_epoch.saturating_add(1);
            hook_store
                .commit("ses", loaded.row_version, &loaded.core, &meta)
                .unwrap();
        }));
        let request = json!({
            "method": "session.wrapup",
            "v": 1,
            "session_id": "ses",
            "keep": 5,
            "command_id": "recut-retry"
        });

        let stale = tool_body(handler.dispatch_value(7, request.clone()).await);
        assert_eq!(stale["disposition"], json!("retryable"), "{stale}");
        assert_eq!(stale["reason"], json!("snapshot_stale"));
        assert!(store
            .load_wrapup_command("ses", "recut-retry")
            .unwrap()
            .is_none());

        cache_wrapup_messages(&handler, wrapup_messages(20, 40));
        let loaded = store.load("ses").unwrap();
        let mut meta = loaded.meta.clone();
        meta.historian.failure_backoff_at_ms = None;
        store
            .commit("ses", loaded.row_version, &loaded.core, &meta)
            .unwrap();
        let retry = tool_body(handler.dispatch_value(7, request).await);
        assert!(matches!(
            retry["disposition"].as_str(),
            Some("completed" | "nothing_to_compact")
        ));
        assert!(store
            .load_wrapup_command("ses", "recut-retry")
            .unwrap()
            .is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_generation_is_fenced_before_historian_additive_writes() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let handler = Arc::new(handler);
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        let before = historian_additive_rows(&store, "ses", &project);
        let hook_handler = Arc::clone(&handler);
        let hook_producer = Arc::clone(&producer);
        *producer
            .on_await_output
            .lock()
            .expect("await-output hook mutex") = Some(Box::new(move || {
            let prompt = hook_producer
                .prompts
                .lock()
                .expect("prompts mutex")
                .last()
                .cloned()
                .expect("producer prompt");
            let (start, end) = prompt_ordinal_range(&prompt).expect("prompt ordinal range");
            hook_producer
                .await_results
                .lock()
                .expect("await results mutex")
                .push_back(Ok(ProducerOutput {
                    text: historian_output_with_fact(
                        start,
                        end,
                        "A stale snapshot must not publish this fact.",
                    ),
                    length_capped: false,
                }));
            hook_handler
                .transform_snapshots
                .lock()
                .expect("transform snapshots mutex")
                .begin("ses");
        }));
        let response = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({
                        "method": "session.wrapup",
                        "v": 1,
                        "session_id": "ses",
                        "keep": 5,
                        "command_id": "generation-retry"
                    }),
                )
                .await,
        );
        assert_eq!(response["disposition"], json!("retryable"), "{response}");
        assert_eq!(response["reason"], json!("snapshot_stale"));
        assert!(store
            .load_wrapup_command("ses", "generation-retry")
            .unwrap()
            .is_none());
        assert_eq!(
            historian_additive_rows(&store, "ses", &project),
            before,
            "the generation fence must run before compartments, transcripts, facts, or the publication floor change"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrapup_publication_holds_snapshot_lock_through_store_write() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let handler = Arc::new(handler);
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        let before = historian_additive_rows(&store, "ses", &project);
        let snapshots = Arc::clone(&handler.transform_snapshots);
        let observed_lock = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed_lock_for_hook = Arc::clone(&observed_lock);
        *handler
            .publication_fence_write_hook
            .lock()
            .expect("publication fence write hook mutex") =
            Some(Box::new(move || match snapshots.try_lock() {
                Err(std::sync::TryLockError::WouldBlock) => {
                    observed_lock_for_hook.store(true, Ordering::SeqCst);
                }
                Ok(_) => panic!("snapshot lock was released before the store write"),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    panic!("transform snapshots mutex poisoned")
                }
            }));

        let response = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses", "keep": 5 }),
                )
                .await,
        );
        assert!(
            matches!(
                response["disposition"].as_str(),
                Some("completed" | "nothing_to_compact")
            ),
            "{response}"
        );
        assert!(observed_lock.load(Ordering::SeqCst));
        assert_ne!(historian_additive_rows(&store, "ses", &project), before);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fence_cleanup_uses_fenced_abandon_without_cooldown() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let handler = Arc::new(handler);
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        let before = historian_additive_rows(&store, "ses", &project);
        let hook_handler = Arc::clone(&handler);
        let hook_producer = Arc::clone(&producer);
        *producer
            .on_await_output
            .lock()
            .expect("await-output hook mutex") = Some(Box::new(move || {
            let prompt = hook_producer
                .prompts
                .lock()
                .expect("prompts mutex")
                .last()
                .cloned()
                .expect("producer prompt");
            let (start, end) = prompt_ordinal_range(&prompt).expect("prompt ordinal range");
            hook_producer
                .await_results
                .lock()
                .expect("await results mutex")
                .push_back(Ok(ProducerOutput {
                    text: historian_output_with_fact(start, end, "Atomic cleanup race fact."),
                    length_capped: false,
                }));
            hook_handler
                .transform_snapshots
                .lock()
                .expect("transform snapshots mutex")
                .begin("ses");
        }));
        let abandon_hook_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let abandon_hook_calls_for_hook = Arc::clone(&abandon_hook_calls);
        store.set_abandon_historian_hook(Box::new(move || {
            abandon_hook_calls_for_hook.fetch_add(1, Ordering::SeqCst);
        }));

        let stale = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses", "keep": 5 }),
                )
                .await,
        );
        assert_eq!(stale["disposition"], json!("retryable"), "{stale}");
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(state.state, HistorianPhase::Idle);
        assert_eq!(state.failure_backoff_at_ms, None);
        assert_eq!(
            abandon_hook_calls.load(Ordering::SeqCst),
            1,
            "the in-transaction abandon hook must run before cleanup commits"
        );
        assert_eq!(historian_additive_rows(&store, "ses", &project), before);

        cache_wrapup_messages(&handler, wrapup_messages(20, 40));
        let starts_before_retry = producer.starts.load(Ordering::SeqCst);
        let retry = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses", "keep": 5 }),
                )
                .await,
        );
        assert!(
            producer.starts.load(Ordering::SeqCst) > starts_before_retry,
            "immediate retry was not admitted: {retry}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fence_rejection_leaves_no_backoff_and_immediate_retry_executes() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let handler = Arc::new(handler);
        cache_wrapup_messages(&handler, wrapup_messages(80, 800));
        let hook_handler = Arc::clone(&handler);
        let hook_producer = Arc::clone(&producer);
        *producer
            .on_await_output
            .lock()
            .expect("await-output hook mutex") = Some(Box::new(move || {
            let prompt = hook_producer
                .prompts
                .lock()
                .expect("prompts mutex")
                .last()
                .cloned()
                .expect("producer prompt");
            let (start, end) = prompt_ordinal_range(&prompt).expect("prompt ordinal range");
            hook_producer
                .await_results
                .lock()
                .expect("await results mutex")
                .push_back(Ok(ProducerOutput {
                    text: historian_output_with_fact(start, end, "Fence race fact."),
                    length_capped: false,
                }));
            hook_handler
                .transform_snapshots
                .lock()
                .expect("transform snapshots mutex")
                .begin("ses");
        }));
        let stale = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses", "keep": 5 }),
                )
                .await,
        );
        assert_eq!(stale["disposition"], json!("retryable"), "{stale}");
        assert_eq!(stale["reason"], json!("snapshot_stale"));
        // The fence race is not a producer failure: no cooldown may be armed, so an
        // immediate retry with a fresh snapshot executes instead of backoff_active.
        assert_eq!(
            store
                .load("ses")
                .unwrap()
                .meta
                .historian
                .failure_backoff_at_ms,
            None,
            "fence rejection must not arm the failure backoff"
        );
        cache_wrapup_messages(&handler, wrapup_messages(20, 40));
        // The retry must be ADMITTED (no cooldown at entry) and drive the producer.
        // The mock's canned output no longer matches the window, so the retry may
        // fail validation afterwards; being turned away at the door is the bug.
        let starts_before_retry = producer.starts.load(Ordering::SeqCst);
        let retry = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses", "keep": 5 }),
                )
                .await,
        );
        assert!(
            producer.starts.load(Ordering::SeqCst) > starts_before_retry,
            "immediate retry must be admitted and drive the producer, got {retry}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn active_snapshot_lease_limit_rejects_then_releases_for_later_wrapup() {
        const ACTIVE_LEASE_LIMIT: usize = 8;
        assert_eq!(ACTIVE_LEASE_LIMIT, MAX_ACTIVE_SNAPSHOT_LEASES);

        let producer = Arc::new(ProducerState::default());
        producer.block_output.store(true, Ordering::SeqCst);
        let (handler, _store, _dir, project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let handler = Arc::new(handler);
        for index in 0..=ACTIVE_LEASE_LIMIT {
            let session_id = format!("lease-{index}");
            let channel = 20 + index as u16;
            handler.bind_route(
                channel,
                binding(project.to_str().unwrap(), session_id.as_str()),
            );
            cache_wrapup_messages_for_session(
                &handler,
                &session_id,
                (1..=10)
                    .map(|ordinal| {
                        ck(
                            &format!("{session_id}-m{ordinal}"),
                            ordinal,
                            "lease budget work",
                        )
                    })
                    .collect(),
            );
        }

        let mut blocked = Vec::new();
        for index in 0..ACTIVE_LEASE_LIMIT {
            let handler = Arc::clone(&handler);
            blocked.push(tokio::spawn(async move {
                handler
                    .dispatch_value(
                        20 + index as u16,
                        json!({
                            "method": "session.wrapup",
                            "v": 1,
                            "session_id": format!("lease-{index}"),
                            "keep": 5
                        }),
                    )
                    .await
            }));
            wait_for_count(&producer.starts, index + 1).await;
        }

        let overflow = tokio::time::timeout(
            Duration::from_millis(100),
            handler.dispatch_value(
                20 + ACTIVE_LEASE_LIMIT as u16,
                json!({
                    "method": "session.wrapup",
                    "v": 1,
                    "session_id": format!("lease-{ACTIVE_LEASE_LIMIT}"),
                    "keep": 5
                }),
            ),
        )
        .await
        .expect("the lease over the global limit must reject before producer output");
        let overflow = tool_body(overflow);
        assert_eq!(overflow["disposition"], json!("retryable"), "{overflow}");
        assert_eq!(overflow["reason"], json!("snapshot_unavailable"));
        assert_eq!(overflow["summary"], "too many concurrent wrapups");
        assert_eq!(producer.starts.load(Ordering::SeqCst), ACTIVE_LEASE_LIMIT);

        producer.block_output.store(false, Ordering::SeqCst);
        producer.notify.notify_waiters();
        for drive in blocked {
            let response = tool_body(drive.await.unwrap());
            assert_eq!(response["ok"], json!(true), "{response}");
        }

        let later = tool_body(
            handler
                .dispatch_value(
                    20 + ACTIVE_LEASE_LIMIT as u16,
                    json!({
                        "method": "session.wrapup",
                        "v": 1,
                        "session_id": format!("lease-{ACTIVE_LEASE_LIMIT}"),
                        "keep": 5
                    }),
                )
                .await,
        );
        assert!(
            matches!(
                later["disposition"].as_str(),
                Some("completed" | "nothing_to_compact")
            ),
            "{later}"
        );
        assert_eq!(
            producer.starts.load(Ordering::SeqCst),
            ACTIVE_LEASE_LIMIT + 1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_session_wrapup_returns_progress_without_double_fire() {
        let producer = Arc::new(ProducerState::default());
        producer.block_output.store(true, Ordering::SeqCst);
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(
            &handler,
            (1..=10).map(|n| ck(&format!("m{n}"), n, "work")).collect(),
        );
        let handler = Arc::new(handler);
        let first_handler = Arc::clone(&handler);
        let first = tokio::spawn(async move {
            first_handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses", "keep": 1 }),
                )
                .await
        });
        wait_for_count(&producer.starts, 1).await;

        let second = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({
                        "method": "session.wrapup",
                        "v": 1,
                        "session_id": "ses",
                        "keep": 1,
                        "command_id": "joined-command"
                    }),
                )
                .await,
        );
        assert_eq!(second["ok"], json!(true));
        assert_eq!(
            second["summary"],
            "wrapup already in progress, 0 rounds done"
        );
        assert_eq!(second["disposition"], json!("already_in_progress"));
        assert_eq!(producer.starts.load(Ordering::SeqCst), 1);
        assert!(store
            .load_wrapup_command("ses", "joined-command")
            .unwrap()
            .is_none());

        producer.block_output.store(false, Ordering::SeqCst);
        producer.notify.notify_waiters();
        let first = tool_body(first.await.unwrap());
        assert_eq!(first["ok"], json!(true), "{first}");
        assert_eq!(producer.starts.load(Ordering::SeqCst), 1);

        let later = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({
                        "method": "session.wrapup",
                        "v": 1,
                        "session_id": "ses",
                        "keep": 1,
                        "command_id": "joined-command"
                    }),
                )
                .await,
        );
        assert_ne!(later["disposition"], json!("already_in_progress"));
        assert!(store
            .load_wrapup_command("ses", "joined-command")
            .unwrap()
            .is_some());
        assert_eq!(producer.starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_wrapup_stops_at_the_five_round_cap() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(&handler, wrapup_messages(11, 36_000));

        let body = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses", "keep": 1 }),
                )
                .await,
        );

        assert_eq!(body["ok"], json!(false), "{body}");
        assert_eq!(body["disposition"], json!("retryable"));
        assert_eq!(body["reason"], json!("snapshot_unavailable"));
        assert!(body["summary"]
            .as_str()
            .unwrap()
            .contains("stopped at the 5-round cap"));
        assert_eq!(producer.starts.load(Ordering::SeqCst), 5);
        assert_eq!(store.load_compartments("ses").unwrap().len(), 5);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_wrapup_stops_on_normal_producer_failure_state() {
        let producer = Arc::new(ProducerState::default());
        producer.await_results.lock().unwrap().extend([
            Err(HistorianProducerError::TimedOut),
            Err(HistorianProducerError::TimedOut),
        ]);
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(
            &handler,
            (1..=10).map(|n| ck(&format!("m{n}"), n, "work")).collect(),
        );

        let body = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses", "keep": 1 }),
                )
                .await,
        );

        assert_eq!(body["ok"], json!(false), "{body}");
        assert_eq!(body["disposition"], json!("retryable"));
        assert_eq!(body["reason"], json!("backoff_active"));
        assert!(body["summary"].as_str().unwrap().contains("producer"));
        let durable = store.load("ses").unwrap().meta.historian;
        assert_eq!(durable.state, HistorianPhase::Idle);
        assert!(durable.last_failure.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejected_post_recut_transform_keeps_wrapup_snapshot_in_flight() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let loaded = store.load("ses").unwrap();
        store
            .commit("ses", loaded.row_version, &loaded.core, &loaded.meta)
            .unwrap();
        store
            .replace_compartments(
                "ses",
                &[
                    stored_comp(1, 1, 2, "m2", "first"),
                    stored_comp(2, 3, 4, "m4", "second"),
                ],
            )
            .unwrap();
        cache_wrapup_messages(&handler, wrapup_messages(20, 40));
        let generation = handler
            .transform_snapshots
            .lock()
            .expect("transform snapshots mutex")
            .begin("ses");
        let before = store.load("ses").unwrap();
        let recut = store
            .truncate_compartments_for_revert("ses", 1, before.row_version)
            .unwrap();
        assert_eq!(recut.revert_epoch, 1);
        assert!(matches!(
            handler
                .transform_snapshots
                .lock()
                .expect("transform snapshots mutex")
                .get("ses"),
            TransformSnapshotLookup::InFlight
        ));
        assert!(generation > 0);

        let body = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses" }),
                )
                .await,
        );
        assert_eq!(body["disposition"], json!("retryable"));
        assert_eq!(body["reason"], json!("snapshot_unavailable"));
        assert_eq!(
            body["summary"],
            "wrapup unavailable until a full session transform has been observed"
        );
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrapup_refuses_epoch_mismatch_and_lru_eviction_honestly() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(&handler, wrapup_messages(20, 40));
        let loaded = store.load("ses").unwrap();
        let mut meta = loaded.meta.clone();
        meta.revert_epoch = 1;
        store
            .commit("ses", loaded.row_version, &loaded.core, &meta)
            .unwrap();
        let mismatch = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({
                        "method": "session.wrapup",
                        "v": 1,
                        "session_id": "ses",
                        "command_id": "stale-retry"
                    }),
                )
                .await,
        );
        assert_eq!(mismatch["disposition"], json!("retryable"));
        assert_eq!(mismatch["reason"], json!("snapshot_stale"));
        assert_eq!(
            mismatch["summary"],
            "wrapup unavailable until a full session transform has been observed"
        );
        assert!(store
            .load_wrapup_command("ses", "stale-retry")
            .unwrap()
            .is_none());
        cache_wrapup_messages(&handler, wrapup_messages(20, 40));
        let retry = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({
                        "method": "session.wrapup",
                        "v": 1,
                        "session_id": "ses",
                        "command_id": "stale-retry"
                    }),
                )
                .await,
        );
        assert_eq!(retry["disposition"], json!("nothing_to_compact"));
        assert!(store
            .load_wrapup_command("ses", "stale-retry")
            .unwrap()
            .is_some());

        handler
            .transform_snapshots
            .lock()
            .expect("transform snapshots mutex")
            .max_ready_bytes = 0;
        cache_wrapup_messages(&handler, wrapup_messages(20, 40));
        let evicted = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses" }),
                )
                .await,
        );
        assert_eq!(evicted["disposition"], json!("retryable"));
        assert_eq!(evicted["reason"], json!("snapshot_unavailable"));
        assert_eq!(
            evicted["summary"],
            "wrapup unavailable until a full session transform has been observed"
        );
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrapup_budget_bounds_busy_join_without_double_drive() {
        let producer = Arc::new(ProducerState::default());
        producer.block_output.store(true, Ordering::SeqCst);
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();
        let transformed = call_transform(&handler, messages).await;
        assert_eq!(transformed["historian"]["fired"], json!(true));
        wait_for_count(&producer.starts, 1).await;
        *handler
            .wrapup_operation_budget
            .lock()
            .expect("wrapup operation budget mutex") = Some(Duration::from_millis(40));

        let request = json!({
            "method": "session.wrapup",
            "v": 1,
            "session_id": "ses",
            "keep": 5,
            "command_id": "budget-retry"
        });
        let body = tool_body(handler.dispatch_value(7, request.clone()).await);
        assert_eq!(body["disposition"], json!("retryable"), "{body}");
        assert_eq!(body["reason"], json!("budget_exhausted"));
        assert!(store
            .load_wrapup_command("ses", "budget-retry")
            .unwrap()
            .is_none());
        assert!(body["summary"]
            .as_str()
            .unwrap()
            .contains("wrapup request budget expired"));
        assert_eq!(producer.starts.load(Ordering::SeqCst), 1);

        producer.block_output.store(false, Ordering::SeqCst);
        producer.notify.notify_waiters();
        wait_for_idle(&store).await;
        *handler
            .wrapup_operation_budget
            .lock()
            .expect("wrapup operation budget mutex") = None;
        let retry = tool_body(handler.dispatch_value(7, request).await);
        assert!(matches!(
            retry["disposition"].as_str(),
            Some("completed" | "nothing_to_compact")
        ));
        assert!(store
            .load_wrapup_command("ses", "budget-retry")
            .unwrap()
            .is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrapup_refuses_active_historian_failure_backoff_at_entry() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        cache_wrapup_messages(&handler, wrapup_messages(20, 40));
        seed_abandoned_idle(
            &store,
            now_ms() + HISTORIAN_FAILURE_BACKOFF_MS,
            "producer failed",
        );

        let request = json!({
            "method": "session.wrapup",
            "v": 1,
            "session_id": "ses",
            "command_id": "backoff-retry"
        });
        let body = tool_body(handler.dispatch_value(7, request.clone()).await);
        assert_eq!(body["disposition"], json!("retryable"));
        assert_eq!(body["reason"], json!("backoff_active"));
        assert!(body["summary"]
            .as_str()
            .unwrap()
            .contains("historian failure backoff active for"));
        assert!(store
            .load_wrapup_command("ses", "backoff-retry")
            .unwrap()
            .is_none());
        assert_eq!(producer.connects.load(Ordering::SeqCst), 0);
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);

        let loaded = store.load("ses").unwrap();
        let mut meta = loaded.meta.clone();
        meta.historian.failure_backoff_at_ms = None;
        store
            .commit("ses", loaded.row_version, &loaded.core, &meta)
            .unwrap();
        let retry = tool_body(handler.dispatch_value(7, request).await);
        assert_eq!(retry["disposition"], json!("nothing_to_compact"));
        assert!(store
            .load_wrapup_command("ses", "backoff-retry")
            .unwrap()
            .is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrapup_coverage_beyond_cached_terminal_is_nothing_to_compact() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let loaded = store.load("ses").unwrap();
        store
            .commit("ses", loaded.row_version, &loaded.core, &loaded.meta)
            .unwrap();
        store
            .replace_compartments("ses", &[stored_comp(1, 1, 10, "m10", "covered")])
            .unwrap();
        cache_wrapup_messages(
            &handler,
            vec![ck("m1", 1, "one"), ck("m2", 2, "two"), ck("m3", 3, "three")],
        );

        let body = tool_body(
            handler
                .dispatch_value(
                    7,
                    json!({ "method": "session.wrapup", "v": 1, "session_id": "ses" }),
                )
                .await,
        );
        assert_eq!(body["disposition"], json!("nothing_to_compact"));
        assert_eq!(body["rounds"], json!(0));
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_busy_dedups_while_firing_is_in_progress() {
        let producer = Arc::new(ProducerState::default());
        producer.block_output.store(true, Ordering::SeqCst);
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();

        let first = call_transform(&handler, messages.clone()).await;
        assert_eq!(first["historian"]["fired"], true);
        wait_for_count(&producer.starts, 1).await;

        let busy = call_transform(&handler, messages).await;
        assert_eq!(busy["historian"]["no_fire"], "busy");
        assert_eq!(producer.connects.load(Ordering::SeqCst), 1);
        assert_eq!(producer.starts.load(Ordering::SeqCst), 1);
        assert_eq!(producer.binds.load(Ordering::SeqCst), 0);
        assert_eq!(producer.statuses.load(Ordering::SeqCst), 0);

        producer.block_output.store(false, Ordering::SeqCst);
        producer.notify.notify_waiters();
        wait_for_idle(&store).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_emergency_inline_drive_folds_in_the_same_response() {
        let producer = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();

        let response = call_transform_with_usage(&handler, messages, 48_000, 50_000).await;

        assert_eq!(response["action"], "HARD");
        assert_eq!(response["historian"]["fired"], true);
        assert!(m0_text(&response).contains("autonomous summary"));
        assert_eq!(producer.starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_emergency_busy_waits_for_the_active_run_and_then_refolds() {
        let producer = Arc::new(ProducerState::default());
        producer.block_output.store(true, Ordering::SeqCst);
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();

        let first = call_transform(&handler, messages.clone()).await;
        assert_eq!(first["historian"]["fired"], true);
        wait_for_count(&producer.starts, 1).await;

        let mut blocked = Box::pin(call_transform_with_usage(
            &handler, messages, 48_000, 50_000,
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), blocked.as_mut())
                .await
                .is_err(),
            ">=95% requests must wait for the active historian run instead of returning early"
        );

        producer.block_output.store(false, Ordering::SeqCst);
        producer.notify.notify_waiters();
        let response = blocked.await;

        assert!(response["action"].is_string());
        assert!(m0_text(&response).contains("autonomous summary"));
        wait_for_idle(&store).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_emergency_refolds_when_active_run_publishes_before_live_wait_capture() {
        let producer = Arc::new(ProducerState::default());
        producer.block_output.store(true, Ordering::SeqCst);
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let handler = Arc::new(handler);
        let messages = big_messages();

        let first = call_transform(&handler, messages.clone()).await;
        assert_eq!(first["historian"]["fired"], true);
        wait_for_count(&producer.starts, 1).await;

        // Place the publish in the exact race window: the interleave seam runs between
        // the request's (pre-fold) transform and the Emergency95 prepare, so by the time
        // prepare checks the live map the run has published and released its entry —
        // the Complete arm's row-advance check is the only thing that can fold this
        // response.
        {
            let producer = Arc::clone(&producer);
            let store_for_hook = Arc::clone(&store);
            *handler
                .between_transform_and_prepare
                .lock()
                .expect("interleave hook mutex") = Some(Box::new(move || {
                producer.block_output.store(false, Ordering::SeqCst);
                producer.notify.notify_waiters();
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    let idle = store_for_hook
                        .load("ses")
                        .map(|s| s.meta.historian.state == HistorianPhase::Idle)
                        .unwrap_or(false);
                    if idle {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "active run must publish within the hook window"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }));
        }

        let response = call_transform_with_usage(&handler, messages, 48_000, 50_000).await;
        assert!(
            m0_text(&response).contains("autonomous summary"),
            "a fold published between the transform and the live-map check must land in this response"
        );
        // A second producer start is legitimate here: after the first fold publishes,
        // this fixture still has enough eligible content to cross the trigger bar, and
        // an emergency pass drains continuously. The load-bearing assertion is the fold
        // in the response, not the run count.
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_emergency_inline_failure_degrades_to_the_emergency_selection_output() {
        let producer = Arc::new(ProducerState::default());
        producer.await_results.lock().unwrap().extend([
            Err(HistorianProducerError::TimedOut),
            Err(HistorianProducerError::TimedOut),
        ]);
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();
        let (_baseline_handler, baseline_store, _baseline_dir, baseline_project) =
            handler_with_store(Arc::new(ProducerState::default()), default_test_config());
        let expected_request = transform_request(messages.clone(), 48_000, 50_000);
        let baseline_project_path = baseline_project.to_string_lossy().to_string();
        let expected = transform::transform(
            &baseline_store,
            &expected_request,
            &transform::ProducerContext {
                project_path: &baseline_project_path,
                note_project_path: &baseline_project_path,
                project_directory: &baseline_project_path,
                history_budget_tokens: memory_render::DEFAULT_HISTORY_BUDGET_TOKENS,
                memory_enabled: true,
                now_ms: now_ms(),
                execute_threshold_percentage: 65.0,
                smart_drops: false,
                cache_ttl: "5m".to_string(),
                model_key: None,
                observed_last_response_at_ms: None,
                guidance_date: Some("Today's date: Thu Jan 01 1970".to_string()),
                injected_reductions: Vec::new(),
            },
        )
        .unwrap();
        let expected_value = serde_json::to_value(expected).unwrap();

        let response = call_transform_with_usage(&handler, messages, 48_000, 50_000).await;

        assert_eq!(response["action"], expected_value["action"]);
        assert_eq!(response["ck_messages"], expected_value["ck_messages"]);
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(state.state, HistorianPhase::Idle);
        assert!(
            state
                .failure_backoff_at_ms
                .is_some_and(|backoff_at_ms| backoff_at_ms > now_ms()),
            "an inline failure must abandon with a future backoff instead of silently clearing state"
        );
        assert!(
            state
                .last_failure
                .as_deref()
                .is_some_and(|detail| detail.contains("timed out")),
            "the durable failure detail should explain why the inline historian run degraded"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_below_emergency_usage_still_spawns_without_blocking() {
        let producer = Arc::new(ProducerState::default());
        producer.block_output.store(true, Ordering::SeqCst);
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();

        let response = tokio::time::timeout(
            Duration::from_millis(50),
            call_transform_with_usage(&handler, messages, 45_000, 50_000),
        )
        .await
        .expect("<95% requests should return while the background historian run is still active");

        assert_eq!(response["historian"]["fired"], true);
        assert!(!m0_text(&response).contains("autonomous summary"));
        wait_for_count(&producer.starts, 1).await;
        producer.block_output.store(false, Ordering::SeqCst);
        producer.notify.notify_waiters();
        wait_for_idle(&store).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn live_historian_session_installs_the_completion_notify_before_busy_is_visible() {
        let handler = McHandler::new();
        let guard = match handler.try_claim_live_historian_session("ses") {
            LiveHistorianSessionClaim::Acquired(guard) => guard,
            LiveHistorianSessionClaim::Busy(_) => panic!("first claim must acquire the live latch"),
        };
        let mut completion = match handler.try_claim_live_historian_session("ses") {
            LiveHistorianSessionClaim::Acquired(_) => {
                panic!("a second claim must observe the existing live session")
            }
            LiveHistorianSessionClaim::Busy(completion) => completion,
        };

        drop(guard);
        tokio::time::timeout(Duration::from_millis(50), completion.as_mut())
            .await
            .expect("busy observers must always see a completion notify they can await");
    }

    #[test]
    fn stale_live_historian_cleanup_cannot_delete_the_next_run_entry() {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let old_token = Arc::new(());
        let old_completion = Arc::new(Notify::new());
        sessions.lock().unwrap().insert(
            "ses".to_string(),
            LiveHistorianSession {
                token: Arc::clone(&old_token),
                completion: Arc::clone(&old_completion),
            },
        );
        let old_guard = SessionSetGuard {
            sessions: Arc::clone(&sessions),
            session_id: "ses".to_string(),
            token: Arc::clone(&old_token),
            completion: Arc::clone(&old_completion),
        };

        let new_token = Arc::new(());
        let new_completion = Arc::new(Notify::new());
        sessions.lock().unwrap().insert(
            "ses".to_string(),
            LiveHistorianSession {
                token: Arc::clone(&new_token),
                completion: Arc::clone(&new_completion),
            },
        );

        drop(old_guard);
        let entry = sessions
            .lock()
            .unwrap()
            .get("ses")
            .cloned()
            .expect("the replacement live entry must survive stale cleanup");
        assert!(Arc::ptr_eq(&entry.token, &new_token));

        drop(SessionSetGuard {
            sessions: Arc::clone(&sessions),
            session_id: "ses".to_string(),
            token: new_token,
            completion: new_completion,
        });
        assert!(sessions.lock().unwrap().is_empty());
    }

    fn seed_awaiting(store: &McStore, messages: &[CkIngressMessage]) {
        let live = crate::ck_wire::project_messages(messages).unwrap().blocks;
        let chunk = historian_chunk::build_historian_chunk(
            messages,
            &live,
            1,
            DEFAULT_HISTORIAN_CHUNK_TOKENS,
            4,
        );
        let fingerprint_items: Vec<_> = chunk.snapshot.iter().map(|item| item.as_item()).collect();
        let fingerprint = historian::compute_chunk_fingerprint(&fingerprint_items);
        let loaded = store.load("ses").unwrap();
        let mut meta = loaded.meta;
        meta.historian = HistorianDurableState {
            state: HistorianPhase::AwaitingProducer,
            firing_seq: 1,
            chunk_range: Some(HistorianChunkRange {
                from_ordinal: 1,
                to_ordinal: 3,
            }),
            chunk_fingerprint: fingerprint,
            producer_session_id: Some("producer-session".to_string()),
            producer_run_id: Some("run-reattach".to_string()),
            fired_at_ms: Some(1),
            expected_revert_epoch: 0,
            failure_backoff_at_ms: None,
            last_failure: None,
            last_no_fire: None,
        };
        store
            .commit("ses", loaded.row_version, &loaded.core, &meta)
            .unwrap();
    }

    fn seed_historian_phase(store: &McStore, phase: HistorianPhase) {
        let loaded = store.load("ses").unwrap();
        let mut meta = loaded.meta;
        meta.historian = HistorianDurableState {
            state: phase,
            firing_seq: 1,
            chunk_range: Some(HistorianChunkRange {
                from_ordinal: 1,
                to_ordinal: 3,
            }),
            chunk_fingerprint: "seeded-fingerprint".to_string(),
            producer_session_id: Some("producer-session".to_string()),
            producer_run_id: Some("run-stale".to_string()),
            fired_at_ms: Some(1),
            expected_revert_epoch: 0,
            failure_backoff_at_ms: None,
            last_failure: None,
            last_no_fire: None,
        };
        store
            .commit("ses", loaded.row_version, &loaded.core, &meta)
            .unwrap();
    }

    fn seed_idle(store: &McStore) {
        let loaded = store.load("ses").unwrap();
        let mut meta = loaded.meta;
        meta.historian = HistorianDurableState::default();
        store
            .commit("ses", loaded.row_version, &loaded.core, &meta)
            .unwrap();
    }

    fn seed_abandoned_idle(store: &McStore, backoff_at_ms: i64, detail: &str) {
        let loaded = store.load("ses").unwrap();
        let fired = match historian::fire(
            &HistorianDurableState::default(),
            1,
            3,
            "seeded-fingerprint".to_string(),
            0,
            1,
        )
        .unwrap()
        {
            historian::FireOutcome::Fired(state) => state,
            historian::FireOutcome::Busy(_) => unreachable!(),
        };
        let mut meta = loaded.meta;
        meta.historian =
            historian::abandon_with_detail(&fired, backoff_at_ms, Some(detail.to_string()));
        store
            .commit("ses", loaded.row_version, &loaded.core, &meta)
            .unwrap();
    }

    fn expire_historian_backoff(store: &McStore) {
        let loaded = store.load("ses").unwrap();
        let mut meta = loaded.meta;
        meta.historian.failure_backoff_at_ms = Some(now_ms() - 1);
        store
            .commit("ses", loaded.row_version, &loaded.core, &meta)
            .unwrap();
    }

    async fn assert_seeded_phase_recovers_then_refires_after_backoff(phase: HistorianPhase) {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();
        seed_historian_phase(&store, phase.clone());

        let recovering = call_transform(&handler, messages.clone()).await;
        assert_eq!(recovering["historian"]["state"], phase.as_str());
        assert_eq!(recovering["historian"]["no_fire"], "recovering");
        wait_for_idle(&store).await;
        assert_eq!(producer.connects.load(Ordering::SeqCst), 0);
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);
        assert_eq!(producer.binds.load(Ordering::SeqCst), 0);
        assert_eq!(producer.statuses.load(Ordering::SeqCst), 0);

        let backed_off = call_transform(&handler, messages.clone()).await;
        assert_eq!(backed_off["historian"]["fired"], false);
        assert_eq!(backed_off["historian"]["no_fire"], "backoff");
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);

        expire_historian_backoff(&store);
        let fresh = call_transform(&handler, messages).await;
        assert_eq!(fresh["historian"]["fired"], true);
        wait_for_count(&producer.starts, 1).await;
        wait_for_idle(&store).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_seeded_publishing_recovers_then_refires_after_backoff() {
        assert_seeded_phase_recovers_then_refires_after_backoff(HistorianPhase::Publishing).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_seeded_firing_recovers_then_refires_after_backoff() {
        assert_seeded_phase_recovers_then_refires_after_backoff(HistorianPhase::Firing).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_seeded_validating_recovers_then_refires_after_backoff() {
        assert_seeded_phase_recovers_then_refires_after_backoff(HistorianPhase::Validating).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_stale_reattach_against_idle_is_noop() {
        let producer = Arc::new(ProducerState::default());
        producer.outputs.lock().unwrap().push_back(historian_output(
            1,
            3,
            "stale reattach summary",
        ));
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();
        seed_awaiting(&store, &messages);

        let reattaching = call_transform(&handler, messages).await;
        assert_eq!(reattaching["historian"]["no_fire"], "reattaching");
        seed_idle(&store);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let loaded = store.load("ses").unwrap();
        assert_eq!(loaded.meta.historian, HistorianDurableState::default());
        assert_eq!(producer.connects.load(Ordering::SeqCst), 0);
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);
        assert_eq!(producer.binds.load(Ordering::SeqCst), 0);
        assert_eq!(producer.statuses.load(Ordering::SeqCst), 0);
        assert_eq!(producer.await_outputs.load(Ordering::SeqCst), 0);
    }

    #[derive(Clone, Copy)]
    enum ReattachGenerationCase {
        SupersededBeforeReattachSpawn,
        FinishedReadyWithSameGeneration,
    }

    async fn run_reattach_generation_case(
        case: ReattachGenerationCase,
    ) -> (
        HistorianAdditiveRows,
        HistorianAdditiveRows,
        HistorianDurableState,
        bool,
    ) {
        let producer = Arc::new(ProducerState::default());
        producer.block_output.store(true, Ordering::SeqCst);
        producer
            .outputs
            .lock()
            .unwrap()
            .push_back(historian_output_with_fact(
                1,
                3,
                "Reattached generation fact.",
            ));
        let (handler, store, _dir, project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();
        seed_awaiting(&store, &messages);
        let before = historian_additive_rows(&store, "ses", &project);
        let hook_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hook_ran_for_hook = Arc::clone(&hook_ran);
        let snapshots = Arc::clone(&handler.transform_snapshots);
        let store_for_hook = Arc::clone(&store);
        let messages_for_hook = messages.clone();
        *handler
            .between_transform_and_prepare
            .lock()
            .expect("interleave hook mutex") = Some(Box::new(move || {
            hook_ran_for_hook.store(true, Ordering::SeqCst);
            let mut snapshots = snapshots.lock().expect("transform snapshots mutex");
            match case {
                ReattachGenerationCase::SupersededBeforeReattachSpawn => {
                    snapshots.begin("ses");
                }
                ReattachGenerationCase::FinishedReadyWithSameGeneration => {
                    // Move the cache record from pending to completed for the same numeric
                    // version before publication. This proves publication accepts a completed
                    // record when its version did not change.
                    let parsed = transform_request(messages_for_hook, 1, 200_000);
                    let retained_bytes = serde_json::to_vec(&parsed).unwrap().len();
                    let revert_epoch = store_for_hook.load("ses").unwrap().meta.revert_epoch;
                    snapshots.finish_ready(
                        "ses",
                        1,
                        Arc::new(parsed),
                        revert_epoch,
                        retained_bytes,
                    );
                    assert!(matches!(
                        snapshots.entries.get("ses"),
                        Some(TransformSnapshot::Ready { generation: 1, .. })
                    ));
                }
            }
        }));

        let response = call_transform(&handler, messages).await;
        assert_eq!(response["historian"]["no_fire"], "reattaching");
        wait_for_count(&producer.await_outputs, 1).await;
        producer.block_output.store(false, Ordering::SeqCst);
        producer.notify.notify_waiters();
        wait_for_idle(&store).await;

        let after = historian_additive_rows(&store, "ses", &project);
        let state = store.load("ses").unwrap().meta.historian;
        (before, after, state, hook_ran.load(Ordering::SeqCst))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reattach_snapshot_fence_rejects_pre_observation_supersession_and_accepts_ready() {
        let (stale_before, stale_after, stale_state, stale_hook_ran) =
            run_reattach_generation_case(ReattachGenerationCase::SupersededBeforeReattachSpawn)
                .await;
        assert!(stale_hook_ran, "the pre-reattach interleave hook must run");
        assert_eq!(stale_after, stale_before);
        assert_eq!(stale_state.state, HistorianPhase::Idle);
        assert_eq!(stale_state.failure_backoff_at_ms, None);
        assert_eq!(
            stale_state.last_failure.as_deref(),
            Some("publish rejected: transform snapshot state changed after reattach started")
        );

        let (control_before, control_after, control_state, control_hook_ran) =
            run_reattach_generation_case(ReattachGenerationCase::FinishedReadyWithSameGeneration)
                .await;
        assert!(
            control_hook_ran,
            "the same-generation control hook must run"
        );
        assert_ne!(control_after, control_before);
        assert_eq!(control_state.state, HistorianPhase::Idle);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_reattach_is_single_flight_and_latch_releases() {
        let producer = Arc::new(ProducerState::default());
        producer.block_output.store(true, Ordering::SeqCst);
        producer
            .outputs
            .lock()
            .unwrap()
            .push_back(historian_output(1, 3, "reattached summary"));
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();
        seed_awaiting(&store, &messages);

        let _ = call_transform(&handler, messages.clone()).await;
        let _ = call_transform(&handler, messages.clone()).await;
        wait_for_count(&producer.binds, 1).await;
        assert_eq!(producer.binds.load(Ordering::SeqCst), 1);
        wait_for_count(&producer.statuses, 1).await;
        assert_eq!(producer.statuses.load(Ordering::SeqCst), 1);

        producer.block_output.store(false, Ordering::SeqCst);
        producer.notify.notify_waiters();
        wait_for_idle(&store).await;

        producer.block_output.store(true, Ordering::SeqCst);
        producer
            .outputs
            .lock()
            .unwrap()
            .push_back(historian_output(1, 3, "reattached again"));
        seed_awaiting(&store, &messages);
        let _ = call_transform(&handler, messages).await;
        wait_for_count(&producer.statuses, 2).await;
        assert_eq!(producer.statuses.load(Ordering::SeqCst), 2);
        producer.block_output.store(false, Ordering::SeqCst);
        producer.notify.notify_waiters();
        wait_for_idle(&store).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_backoff_blocks_refire_and_records_durable_skip_reason() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();
        seed_abandoned_idle(
            &store,
            now_ms() + HISTORIAN_FAILURE_BACKOFF_MS,
            "validate rejected: stale summary",
        );

        let response = call_transform(&handler, messages).await;
        assert_eq!(response["historian"]["fired"], false);
        assert_eq!(response["historian"]["no_fire"], "backoff");
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(state.state, HistorianPhase::Idle);
        assert_eq!(state.last_no_fire.as_deref(), Some("backoff"));
        assert!(
            state
                .failure_backoff_at_ms
                .is_some_and(|backoff_at_ms| backoff_at_ms > now_ms()),
            "the cooldown remains active until the backoff boundary"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_expired_backoff_refires_and_success_clears_failure_state() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();
        seed_abandoned_idle(&store, now_ms() - 1, "validate rejected: stale summary");

        let response = call_transform(&handler, messages).await;
        assert_eq!(response["historian"]["fired"], true);
        wait_for_count(&producer.starts, 1).await;
        wait_for_idle(&store).await;
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(state.last_failure, None);
        assert_eq!(state.failure_backoff_at_ms, None);
        assert_eq!(state.last_no_fire, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handler_connect_failure_records_backoff_and_later_fire_clears_it() {
        let producer = Arc::new(ProducerState::default());
        producer
            .connect_errors
            .lock()
            .unwrap()
            .push_back(HistorianProducerError::Connect {
                endpoint: "127.0.0.1:1".to_string(),
                source: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
            });
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let messages = big_messages();

        let first = call_transform(&handler, messages.clone()).await;
        assert_eq!(first["historian"]["fired"], true);
        wait_for_count(&producer.connects, 1).await;
        wait_for_historian_state(&store, |state| {
            state
                .last_failure
                .as_deref()
                .is_some_and(|detail| detail.contains("producer connect"))
        })
        .await;
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(state.state, HistorianPhase::Idle);
        assert!(state
            .failure_backoff_at_ms
            .is_some_and(|until| until > now_ms()));
        assert!(
            state
                .last_failure
                .as_deref()
                .is_some_and(|detail| detail.contains("producer connect")),
            "pre-fire connect failures must land in durable state, not only stderr"
        );
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);

        let backed_off = call_transform(&handler, messages.clone()).await;
        assert_eq!(backed_off["historian"]["fired"], false);
        assert_eq!(backed_off["historian"]["no_fire"], "backoff");
        assert_eq!(producer.connects.load(Ordering::SeqCst), 1);

        expire_historian_backoff(&store);
        let fresh = call_transform(&handler, messages).await;
        assert_eq!(fresh["historian"]["fired"], true);
        wait_for_count(&producer.starts, 1).await;
        wait_for_idle(&store).await;
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(state.last_failure, None);
        assert_eq!(state.failure_backoff_at_ms, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_fire_reason_is_durable_change_gated_and_cleared_by_fire() {
        // Skip branch writes the discriminant durably (supervised rigs read state, not
        // responses), a repeat of the same reason writes nothing, and a real fire clears it.
        let producer = Arc::new(ProducerState::default());
        let mut config = default_test_config();
        config.model_chain.clear();
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), config.clone());
        let messages = big_messages();

        let _ = call_transform(&handler, messages.clone()).await;
        let loaded = store.load("ses").unwrap();
        assert_eq!(
            loaded.meta.historian.last_no_fire.as_deref(),
            Some("no_models")
        );
        let version_after_first = loaded.row_version;

        let _ = call_transform(&handler, messages.clone()).await;
        let loaded = store.load("ses").unwrap();
        assert_eq!(
            loaded.row_version, version_after_first,
            "an unchanged skip reason must not rewrite the row"
        );

        // Same store, models restored: the fire must clear the stale skip reason.
        config.model_chain = vec!["prov/model-a".to_string()];
        let handler2 = McHandler::with_producer_factory_and_config(
            Arc::new(TestProducerFactory {
                state: Arc::clone(&producer),
            }),
            config,
        );
        handler2.store.set(Arc::clone(&store)).ok().unwrap();
        handler2.bind_route(7, binding(_project.to_str().unwrap(), "ses"));
        let fired = call_transform(&handler2, messages).await;
        assert_eq!(fired["historian"]["fired"], true);
        // The clearing write happens in the spawned firing's persist. Durable state is
        // still Idle until that task runs, so gate on the producer actually starting
        // first (otherwise wait_for_idle returns before the firing began).
        wait_for_count(&producer.starts, 1).await;
        wait_for_idle(&store).await;
        let loaded = store.load("ses").unwrap();
        assert_eq!(loaded.meta.historian.last_no_fire, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn own_producer_sessions_pass_through_untransformed() {
        // The historian's own llm-runner run must never be re-transformed: prepending
        // m0/m1 framing ahead of the historian system prompt restructures the calibrated
        // request and makes the model treat the seed examples as session content.
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) =
            handler_with_store(Arc::clone(&producer), default_test_config());
        let session = historian::historian_producer_session_id("proj", "parent-session", 3);
        handler.bind_route(9, binding("/tmp/nonexistent-proj", &session));
        let messages = [ck("m1", 1, "seed block + new_messages payload")];
        let req = serde_json::json!({
            "kind": "transform",
            "v": 2,
            "serializer_profile": "owned-llmrunner",
            "session_id": session,
            "render_config": "cfg0",
            "messages": messages.iter().map(|m| serde_json::to_value(m).unwrap()).collect::<Vec<_>>(),
        });
        let out = handler.handle_transform_value(9, req).await;
        let HandlerOutcome::Response(bytes) = out else {
            panic!("pass-through must be a response");
        };
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["action"], "PASSTHROUGH");
        let out_msgs = v["ck_messages"].as_array().unwrap();
        assert_eq!(out_msgs.len(), 1, "no m0/m1 prepends, no drops");
        // No store row was created and no historian evaluation ran for the child session.
        assert!(store.load(&session).unwrap().row_version.is_none());
        assert!(store.load_pass_trace(&session).unwrap().is_none());
        assert!(v.get("historian").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trigger_false_no_fire_detail_carries_quantized_numbers() {
        // Small content: the trigger honestly declines, and the durable discriminant
        // must carry the measurement (eligible vs bar vs protected N) so a rig can
        // distinguish "bar uncrossed" from "eligible measuring zero" in one query.
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let messages = vec![ck("m1", 1, "small turn")];
        let _ = call_transform(&handler, messages.clone()).await;
        let _ = call_transform(&handler, messages).await;
        let loaded = store.load("ses").unwrap();
        let detail = loaded
            .meta
            .historian
            .last_no_fire
            .expect("trigger_false recorded");
        assert!(
            detail.starts_with("trigger_false{")
                && detail.contains("bar~")
                && detail.contains("ctx_limit="),
            "detail carries the numbers: {detail}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn defer_pass_historian_diagnostics_are_byte_pure_and_non_vacuous() {
        let producer = Arc::new(ProducerState::default());
        let mut config = default_test_config();
        config.model_chain.clear();
        let (handler, store, _dir, _project) = handler_with_store(producer, config);
        let messages = big_messages();

        let _ = call_transform(&handler, messages.clone()).await;
        let with_historian = call_transform(&handler, messages.clone()).await;
        assert_eq!(with_historian["action"], "SOFT+");
        assert_eq!(with_historian["historian"]["no_fire"], "no_models");
        assert!(with_historian["historian"]["reason"].is_string());
        // Progress numbers surface on every boundary-resolving pass so a stalled rig
        // drive can watch eligible content approach the fire bar.
        let progress = &with_historian["historian"]["progress"];
        assert!(progress["tail_size_bar"].as_f64().unwrap() > 0.0);
        assert!(progress["protected_tail_n_tokens"].as_f64().unwrap() > 0.0);
        assert!(progress["eligible_chunk_tokens"].is_number());

        let req: TransformRequest = serde_json::from_value(request(messages)).unwrap();
        let project_path = handler.resolve_binding(7, "ses").unwrap().project_root;
        let project_path_string = project_path.to_string_lossy().to_string();
        let response_without_historian = transform::transform(
            &store,
            &req,
            &transform::ProducerContext {
                project_path: &project_path_string,
                note_project_path: &project_path_string,
                project_directory: &project_path_string,
                history_budget_tokens: memory_render::DEFAULT_HISTORY_BUDGET_TOKENS,
                memory_enabled: true,
                now_ms: now_ms(),
                execute_threshold_percentage: 65.0,
                smart_drops: false,
                cache_ttl: "5m".to_string(),
                model_key: None,
                observed_last_response_at_ms: None,
                guidance_date: Some("Today's date: Thu Jan 01 1970".to_string()),
                injected_reductions: Vec::new(),
            },
        )
        .unwrap();
        let without_value = serde_json::to_value(response_without_historian).unwrap();
        assert_eq!(with_historian["ck_messages"], without_value["ck_messages"]);
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ShadowWireFixture {
        state_sync: StrictShadowStateSync,
        shadow_transform: StrictShadowTransform,
        shadow_reset: StrictShadowReset,
        local_watermarks: StrictShadowWatermarks,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowStateSync {
        method: String,
        shadow_generation: u64,
        expected_shadow_seq: u64,
        seed_id: String,
        seed_generation: u64,
        seed_batch_index: usize,
        seed_batch_total: usize,
        seed_complete: bool,
        seed_boundary_id: Option<String>,
        compartments: Vec<StrictShadowCompartment>,
        memories: Vec<StrictShadowMemory>,
        memory_mutations: Vec<StrictShadowMemoryMutation>,
        user_profile: Vec<String>,
        workspace: Option<StrictShadowWorkspace>,
        last_todo_state: String,
        acked_watermarks: StrictShadowWatermarks,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowCompartment {
        sequence: i64,
        start_message: i64,
        end_message: i64,
        start_message_id: String,
        end_message_id: String,
        start_date: String,
        end_date: String,
        title: String,
        content: String,
        p1: String,
        p2: String,
        p3: String,
        p4: String,
        importance: i32,
        episode_type: String,
        legacy: i32,
        created_at: i64,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowWorkspace {
        fingerprint: String,
        members: Vec<StrictShadowWorkspaceMember>,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowWorkspaceMember {
        project_path: String,
        share_categories: Vec<String>,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowMemory {
        id: i64,
        project_path: String,
        category: String,
        content: String,
        normalized_hash: String,
        importance: i32,
        scope: String,
        shareable: i32,
        source_session_id: String,
        source_type: String,
        seen_count: i64,
        retrieval_count: i64,
        first_seen_at: i64,
        created_at: i64,
        updated_at: i64,
        last_seen_at: i64,
        last_retrieved_at: i64,
        status: String,
        expires_at: i64,
        verification_status: String,
        verified_at: i64,
        superseded_by_memory_id: i64,
        merged_from: String,
        metadata_json: String,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowMemoryMutation {
        id: i64,
        project_path: String,
        mutation_type: String,
        target_memory_id: i64,
        superseded_by_id: i64,
        category: String,
        new_content: String,
        queued_at: i64,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowTransform {
        method: String,
        shadow_generation: u64,
        seed_pass: bool,
        input: Vec<Value>,
        ts_output: Vec<Value>,
        normalizations: Vec<Value>,
        pass_inputs: StrictShadowPassInputs,
        ts_decision: StrictShadowDecision,
        declared_trim: StrictDeclaredTrim,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowPassInputs {
        now_ms: i64,
        model_key: String,
        usage: StrictShadowUsage,
        effective_execute_threshold: f64,
        history_budget_tokens: f64,
        cache_ttl: String,
        provider_error: String,
        mid_turn: bool,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowUsage {
        input_tokens: u64,
        limit: u64,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowDecision {
        class: String,
        marker_state: StrictShadowMarkerState,
        materialize_reason: String,
        emergency: bool,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowMarkerState {
        marker_message_id: String,
        advanced_this_pass: bool,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictDeclaredTrim {
        flat_boundary_id: String,
        boundary_bare_message_id: String,
        boundary_absolute_ordinal: u64,
        next_absolute_ordinal: u64,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowReset {
        method: String,
        shadow_generation: u64,
        reason: String,
    }

    #[allow(dead_code)]
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictShadowWatermarks {
        compartment_sequence: i64,
        memory_id: i64,
        m0_mutation_id: i64,
        memory_mutation_id: i64,
        last_todo_state_hash: String,
    }

    #[test]
    fn shadow_workspace_namespace_never_reuses_real_project_paths() {
        let real_owner = "/real/owner";
        let real_foreign = "/real/foreign";
        let (workspace, paths) = prepare_shadow_workspace(
            "shadow:session",
            Some(ShadowWorkspaceWire {
                fingerprint: "fixture".to_string(),
                members: vec![
                    ShadowWorkspaceMemberWire {
                        project_path: real_owner.to_string(),
                        share_categories: vec!["CONSTRAINTS".to_string()],
                    },
                    ShadowWorkspaceMemberWire {
                        project_path: real_foreign.to_string(),
                        share_categories: vec!["CONSTRAINTS".to_string()],
                    },
                ],
            }),
        )
        .unwrap();
        let workspace = workspace.expect("workspace");
        assert!(workspace
            .members
            .iter()
            .all(|member| member.project_path.starts_with(SHADOW_SESSION_PREFIX)));
        assert_eq!(
            paths.get(real_owner).map(String::as_str),
            Some("shadow:session")
        );
        assert_ne!(
            paths.get(real_foreign).map(String::as_str),
            Some(real_foreign)
        );
        assert!(
            shadow_source_path(Some("/real/not-a-member"), "shadow:session", &paths, true).is_err()
        );
    }

    #[test]
    fn generated_shadow_wire_fixture_matches_strict_and_production_parsers() {
        let fixture_value: Value =
            serde_json::from_str(include_str!("../testdata/shadow-wire-fixture.json"))
                .expect("shadow wire fixture must be valid JSON");
        let fixture: ShadowWireFixture = serde_json::from_value(fixture_value.clone())
            .expect("shadow wire fixture contains an unknown or missing field");

        serde_json::from_value::<ShadowStateSyncWire>(fixture_value["state_sync"].clone())
            .expect("state_sync fixture must parse through the production wire struct");
        serde_json::from_value::<ShadowTransformWire>(fixture_value["shadow_transform"].clone())
            .expect("shadow_transform fixture must parse through the production wire struct");
        serde_json::from_value::<ShadowResetWire>(fixture_value["shadow_reset"].clone())
            .expect("shadow_reset fixture must parse through the production wire struct");

        assert_eq!(fixture.state_sync.method, "state_sync");
        assert_eq!(
            fixture.state_sync.seed_boundary_id.as_deref(),
            Some("message-2#2")
        );
        assert_eq!(fixture.shadow_transform.method, "shadow_transform");
        assert_eq!(fixture.shadow_reset.method, "shadow_reset");
        assert_eq!(fixture.state_sync.compartments.len(), 2);
        assert_eq!(fixture.state_sync.memories.len(), 2);
        assert_eq!(fixture.state_sync.memory_mutations.len(), 1);
        assert!(fixture.state_sync.user_profile.is_empty());
        assert_eq!(
            fixture
                .state_sync
                .workspace
                .as_ref()
                .expect("fixture workspace")
                .members
                .len(),
            2
        );
        assert_eq!(
            fixture.shadow_transform.pass_inputs.history_budget_tokens,
            19_500.0
        );
        assert_eq!(fixture.local_watermarks.m0_mutation_id, 1);
    }

    fn shadow_pass_inputs() -> Value {
        json!({
            "now_ms": 12345,
            "model_key": "ts/model",
            "usage": { "input_tokens": 45_000, "limit": 50_000 },
            "effective_execute_threshold": 65.0,
            "cache_ttl": "5m",
            "mid_turn": false
        })
    }

    fn shadow_transform_body(
        session: &str,
        generation: u64,
        ts_output: Vec<CkWireMessage>,
        seed_pass: bool,
    ) -> Value {
        json!({
            "kind": "shadow_transform",
            "session_id": session,
            "shadow_generation": generation,
            "seed_pass": seed_pass,
            "pass_seq": 0,
            "serializer_profile": "owned-llmrunner",
            "render_config": "cfg0",
            "messages": vec![ck("m0", 0, "zero ordinal is real"), ck("m1", 1, "tail")],
            "ts_ck_messages": ts_output,
            "pass_inputs": shadow_pass_inputs(),
            "ts_decision": { "class": "defer" },
            "normalizations": [],
        })
    }

    fn shadow_compartment(sequence: i64, content: &str) -> Value {
        json!({
            "sequence": sequence,
            "start_message": sequence,
            "end_message": sequence,
            "start_message_id": format!("m{sequence}#0"),
            "end_message_id": format!("m{sequence}#0"),
            "title": format!("c{sequence}"),
            "content": content,
            "p1": format!("{content}-p1"),
        })
    }

    fn paged_seed_batch(
        session: &str,
        seed_id: &str,
        generation: u64,
        expected_seq: u64,
        index: usize,
        total: usize,
        compartments: Vec<Value>,
    ) -> Value {
        let complete = index + 1 == total;
        let mut batch = json!({
            "kind": "state_sync",
            "session_id": session,
            "shadow_generation": generation,
            "expected_shadow_seq": expected_seq,
            "seed_id": seed_id,
            "seed_generation": generation,
            "seed_batch_index": index,
            "seed_batch_total": total,
            "seed_complete": complete,
            "compartments": compartments,
            "memories": [],
            "memory_mutations": [],
            "user_profile": [],
        });
        if complete {
            let object = batch.as_object_mut().unwrap();
            object.insert("seed_boundary_id".to_string(), Value::Null);
            object.insert("workspace".to_string(), Value::Null);
            object.insert("last_todo_state".to_string(), json!("[]"));
            object.insert("acked_watermarks".to_string(), json!({ "complete": true }));
        }
        batch
    }

    #[allow(clippy::too_many_arguments)]
    fn paged_transform_page(
        method: &str,
        session: &str,
        page_id: &str,
        generation: u64,
        index: usize,
        total: usize,
        complete: bool,
        messages: Vec<Value>,
    ) -> Value {
        let mut page = json!({
            "method": method,
            "session_id": session,
            "transform_page_id": page_id,
            "transform_generation": generation,
            "transform_page_index": index,
            "transform_page_total": total,
            "transform_page_complete": complete,
            "messages": messages,
        });
        if method == "shadow_transform" {
            page["shadow_generation"] = json!(generation);
        }
        if complete {
            let object = page.as_object_mut().expect("transform page body");
            if method == "transform" {
                object.extend([
                    ("kind".to_string(), json!("transform")),
                    ("v".to_string(), json!(2)),
                    ("serializer_profile".to_string(), json!("owned-llmrunner")),
                    ("render_config".to_string(), json!("cfg0")),
                    (
                        "usage".to_string(),
                        json!({
                            "current_total_input_tokens": 45_000,
                            "context_limit_tokens": 50_000,
                        }),
                    ),
                ]);
            } else {
                object.extend([
                    ("kind".to_string(), json!("shadow_transform")),
                    ("shadow_generation".to_string(), json!(generation)),
                    ("seed_pass".to_string(), json!(true)),
                    ("pass_inputs".to_string(), shadow_pass_inputs()),
                    ("ts_decision".to_string(), json!({ "class": "defer" })),
                    ("declared_trim".to_string(), Value::Null),
                    ("ts_ck_messages".to_string(), json!([])),
                    ("input".to_string(), json!([])),
                    ("ts_output".to_string(), json!([])),
                    ("normalizations".to_string(), json!([])),
                ]);
            }
        }
        page["transform_page_digest"] = json!(transform_page_content_digest(&page));
        page
    }

    fn seed_accounting(handler: &McHandler) -> (usize, usize) {
        let seeds = handler.shadow_seeds.lock().expect("shadow seed mutex");
        (seeds.total_staged_bytes, seeds.pending_seed_count)
    }

    #[tokio::test]
    async fn shadow_dispatch_enforces_shadow_route_precedence() {
        let state = Arc::new(ProducerState::default());
        let (handler, _store, _dir, project) = handler_with_store(state, default_test_config());
        handler.bind_route(8, binding(project.to_str().unwrap(), "shadow:ses"));

        let plain_on_shadow = handler
            .dispatch_value(
                8,
                json!({
                    "kind": "transform",
                    "session_id": "shadow:ses",
                    "serializer_profile": "owned-llmrunner",
                    "render_config": "cfg0",
                    "messages": [ck("m0", 0, "hi")],
                }),
            )
            .await;
        assert_eq!(
            error_code(plain_on_shadow),
            "plain_transform_on_shadow_binding"
        );

        let shadow_on_plain = handler
            .dispatch_value(7, json!({ "kind": "shadow_reset", "session_id": "ses" }))
            .await;
        assert_eq!(error_code(shadow_on_plain), "shadow_binding_required");
    }

    #[tokio::test]
    async fn mirror_kill_switch_gates_shadow_lanes_but_not_authority_state_sync() {
        let state = Arc::new(ProducerState::default());
        let mut config = default_test_config();
        config.shadow_enabled = false;
        let (handler, store, _dir, project) = handler_with_store(state, config);
        let project_path = project.to_string_lossy().to_string();
        let synced = handler
            .dispatch_value(
                7,
                json!({
                    "kind": "state_sync",
                    "session_id": "ses",
                    "shadow_generation": 0,
                    "expected_shadow_seq": 0,
                    "compartments": [shadow_compartment(0, "authority summary")],
                    "memories": [{
                        "id": 42,
                        "category": "CONSTRAINTS",
                        "content": "authority memory"
                    }],
                    "user_profile": ["authority profile"],
                    "last_todo_state": "[]"
                }),
            )
            .await;
        assert!(matches!(synced, HandlerOutcome::Response(_)));
        assert_eq!(
            store.load_compartments("ses").unwrap()[0].content,
            "authority summary"
        );
        assert_eq!(
            store
                .load_active_memories(&project_path, 0)
                .unwrap()
                .iter()
                .map(|memory| memory.content.as_str())
                .collect::<Vec<_>>(),
            vec!["authority memory"]
        );
        assert_eq!(
            store.load_active_user_memories().unwrap(),
            vec!["authority profile"]
        );
        let stale = handler
            .dispatch_value(
                7,
                json!({
                    "kind": "state_sync",
                    "session_id": "ses",
                    "shadow_generation": 0,
                    "expected_shadow_seq": 0
                }),
            )
            .await;
        let (stale_code, stale_message) = error_frame(stale);
        assert_eq!(stale_code, "authority_seq_mismatch");
        let details: Value = serde_json::from_str(&stale_message).unwrap();
        assert_eq!(details["code"], "authority_seq_mismatch");
        assert_eq!(details["durable_authority_seq"], 1);

        handler.bind_route(8, binding(project.to_str().unwrap(), "shadow:ses"));
        for method in ["state_sync", "shadow_transform", "shadow_reset"] {
            let rejected = handler
                .dispatch_value(8, json!({ "kind": method, "session_id": "shadow:ses" }))
                .await;
            assert_eq!(error_code(rejected), "shadow_disabled", "{method}");
        }
    }

    #[tokio::test]
    async fn interleaved_authority_senders_keep_the_seq_fence() {
        let state = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(state, default_test_config());
        let request = || {
            json!({
                "kind": "state_sync",
                "session_id": "ses",
                "shadow_generation": 0,
                "expected_shadow_seq": 0,
                "compartments": [shadow_compartment(0, "first sender")],
                "user_profile": [],
            })
        };

        let (first, second) = tokio::join!(
            handler.dispatch_value(7, request()),
            handler.dispatch_value(7, request()),
        );
        let outcomes = [first, second];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, HandlerOutcome::Response(_)))
                .count(),
            1
        );
        let errors = outcomes
            .into_iter()
            .filter_map(|outcome| match outcome {
                HandlerOutcome::Error { code, .. } => Some(code),
                HandlerOutcome::Response(_) | HandlerOutcome::Streamed => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(errors, vec!["authority_seq_mismatch"]);
        assert_eq!(store.load("ses").unwrap().meta.shadow_seq, 1);
    }

    #[tokio::test]
    async fn authority_state_sync_enforces_resolved_owner_and_preserves_foreign_members() {
        let (handler, store, _dir, project) =
            handler_with_store(Arc::new(ProducerState::default()), default_test_config());
        let route_project_root = project.to_str().unwrap();
        activate_module_authority(
            &store,
            "context",
            "git:identity",
            route_project_root,
            "memories",
        );

        let mismatched = handler
            .dispatch_value(
                7,
                json!({
                    "kind": "state_sync",
                    "session_id": "ses",
                    "shadow_generation": 0,
                    "expected_shadow_seq": 0,
                    "memories": [{
                        "id": 1,
                        "project_path": "tenant:third-key",
                        "category": "CONSTRAINTS",
                        "content": "must roll back"
                    }]
                }),
            )
            .await;
        assert_eq!(error_code(mismatched), "invalid_params");
        assert!(store
            .load_active_memories("tenant:third-key", 0)
            .unwrap()
            .is_empty());
        assert!(store
            .load_active_memories("git:identity", 0)
            .unwrap()
            .is_empty());
        assert_eq!(store.load("ses").unwrap().meta.shadow_seq, 0);

        let absent = handler
            .dispatch_value(
                7,
                json!({
                    "kind": "state_sync",
                    "session_id": "ses",
                    "shadow_generation": 0,
                    "expected_shadow_seq": 0,
                    "memories": [{
                        "id": 2,
                        "category": "CONSTRAINTS",
                        "content": "resolved owner"
                    }],
                    "memory_mutations": [{
                        "id": 1,
                        "mutation_type": "update",
                        "target_memory_id": 2,
                        "new_content": "resolved owner"
                    }]
                }),
            )
            .await;
        assert!(matches!(absent, HandlerOutcome::Response(_)), "{absent:?}");
        let absent_response = match &absent {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(bytes).unwrap(),
            HandlerOutcome::Error { .. } | HandlerOutcome::Streamed => Value::Null,
        };
        assert_eq!(absent_response["memories_skipped"], json!(true));
        assert!(store
            .load_active_memories("git:identity", 0)
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .max_memory_mutation_id(&["git:identity".to_string()])
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .max_memory_mutation_id(&[route_project_root.to_string()])
                .unwrap(),
            0
        );

        let workspace = handler
            .dispatch_value(
                7,
                json!({
                    "kind": "state_sync",
                    "session_id": "ses",
                    "shadow_generation": 0,
                    "expected_shadow_seq": 1,
                    "workspace": {
                        "fingerprint": "workspace-v1",
                        "members": [
                            {"project_path": route_project_root, "share_categories": ["CONSTRAINTS"]},
                            {"project_path": "git:foreign", "share_categories": ["CONSTRAINTS"]}
                        ]
                    },
                    "memories": [
                        {
                            "id": 3,
                            "project_path": route_project_root,
                            "category": "CONSTRAINTS",
                            "content": "workspace owner"
                        },
                        {
                            "id": 4,
                            "project_path": "git:foreign",
                            "category": "CONSTRAINTS",
                            "content": "workspace foreign"
                        }
                    ]
                }),
            )
            .await;
        assert!(matches!(workspace, HandlerOutcome::Response(_)));
        let workspace_response = match &workspace {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(bytes).unwrap(),
            HandlerOutcome::Error { .. } | HandlerOutcome::Streamed => Value::Null,
        };
        assert_eq!(workspace_response["memories_skipped"], json!(true));
        assert!(store
            .load_active_memories("git:identity", 0)
            .unwrap()
            .is_empty());
        assert!(store
            .load_active_memories("git:foreign", 0)
            .unwrap()
            .is_empty());
        assert!(store
            .load_active_memories(route_project_root, 0)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn authority_state_sync_storage_feeds_real_transform_m0() {
        let state = Arc::new(ProducerState::default());
        let (handler, store, _dir, project) = handler_with_store(state, default_test_config());
        let project_path = project.to_string_lossy().to_string();
        let sync = handler
            .dispatch_value(
                7,
                json!({
                    "kind": "state_sync",
                    "session_id": "ses",
                    "shadow_generation": 0,
                    "expected_shadow_seq": 0,
                    "seed_id": "authority-seed",
                    "seed_generation": 0,
                    "seed_batch_index": 0,
                    "seed_batch_total": 1,
                    "seed_complete": true,
                    "seed_boundary_id": "m0#0",
                    "workspace": null,
                    "acked_watermarks": {},
                    "compartments": [shadow_compartment(0, "authority summary")],
                    "memories": [{
                        "id": 42,
                        "category": "CONSTRAINTS",
                        "content": "authority memory"
                    }],
                    "user_profile": ["authority profile"],
                    "last_todo_state": "[]"
                }),
            )
            .await;
        assert!(matches!(sync, HandlerOutcome::Response(_)));

        let response =
            call_transform_request(&handler, request(vec![ck("m0", 0, "live authority input")]))
                .await;
        let m0 = m0_text(&response);
        assert!(m0.contains("authority summary"), "{m0}");
        assert!(m0.contains("authority memory"), "{m0}");
        assert!(m0.contains("authority profile"), "{m0}");
        assert_eq!(store.load_compartments("ses").unwrap().len(), 1);
        assert_eq!(
            store.load_active_memories(&project_path, 0).unwrap()[0].content,
            "authority memory"
        );
    }

    #[tokio::test]
    async fn paged_authority_transform_reassembles_and_executes_without_shadow_gate() {
        let state = Arc::new(ProducerState::default());
        let mut config = default_test_config();
        config.shadow_enabled = false;
        let (handler, _store, _dir, _project) = handler_with_store(state, config);
        let first = paged_transform_page(
            "transform",
            "ses",
            "authority-page",
            0,
            0,
            2,
            false,
            vec![json!({
                "mid": "m0",
                "ordinal": 0,
                "ck": ck("m0", 0, "authority first").ck,
            })],
        );
        let final_page = paged_transform_page(
            "transform",
            "ses",
            "authority-page",
            0,
            1,
            2,
            true,
            vec![json!({
                "mid": "m1",
                "ordinal": 1,
                "ck": ck("m1", 1, "authority final").ck,
            })],
        );
        let mut first = first;
        first["native_messages"] = json!([{ "text": "a".repeat(280 * 1024) }]);
        first["transform_page_digest"] = json!(transform_page_content_digest(&first));
        let mut final_page = final_page;
        final_page["native_messages"] = json!([{ "text": "b".repeat(280 * 1024) }]);
        final_page["transform_page_digest"] = json!(transform_page_content_digest(&final_page));

        let first_ack = handler.dispatch_value(7, first).await;
        assert!(matches!(first_ack, HandlerOutcome::Response(_)));
        let response = handler.dispatch_value(7, final_page).await;
        let HandlerOutcome::Response(bytes) = response else {
            panic!("authority page assembly should execute: {response:?}");
        };
        let response: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(response["action"].is_string(), "{response}");
    }

    #[tokio::test]
    async fn paged_authority_transform_rejections_discard_partial_assembly() {
        let state = Arc::new(ProducerState::default());
        let (handler, _store, _dir, _project) = handler_with_store(state, default_test_config());

        let first = paged_transform_page(
            "transform",
            "ses",
            "order-page",
            0,
            0,
            3,
            false,
            vec![json!({
                "mid": "m0",
                "ordinal": 0,
                "ck": ck("m0", 0, "first").ck,
            })],
        );
        assert!(matches!(
            handler.dispatch_value(7, first).await,
            HandlerOutcome::Response(_)
        ));
        let newer = paged_transform_page(
            "transform",
            "ses",
            "newer-page",
            0,
            0,
            2,
            false,
            vec![json!({
                "mid": "newer-m0",
                "ordinal": 0,
                "ck": ck("newer-m0", 0, "newer").ck,
            })],
        );
        assert_eq!(
            error_code(handler.dispatch_value(7, newer).await),
            "authority_transform_page_attempt_mismatch"
        );
        let retry_first = paged_transform_page(
            "transform",
            "ses",
            "order-page",
            0,
            0,
            3,
            false,
            vec![json!({
                "mid": "m0",
                "ordinal": 0,
                "ck": ck("m0", 0, "first").ck,
            })],
        );
        assert!(matches!(
            handler.dispatch_value(7, retry_first).await,
            HandlerOutcome::Response(_)
        ));
        let gap = paged_transform_page(
            "transform",
            "ses",
            "order-page",
            0,
            2,
            3,
            true,
            vec![json!({
                "mid": "m2",
                "ordinal": 2,
                "ck": ck("m2", 2, "gap").ck,
            })],
        );
        assert_eq!(
            error_code(handler.dispatch_value(7, gap).await),
            "authority_transform_page_order_mismatch"
        );

        let replacement = paged_transform_page(
            "transform",
            "ses",
            "replacement-page",
            0,
            0,
            1,
            true,
            vec![json!({
                "mid": "m0",
                "ordinal": 0,
                "ck": ck("m0", 0, "replacement").ck,
            })],
        );
        assert!(matches!(
            handler.dispatch_value(7, replacement).await,
            HandlerOutcome::Response(_)
        ));

        let digest_first = paged_transform_page(
            "transform",
            "ses",
            "digest-page",
            0,
            0,
            2,
            false,
            vec![json!({
                "mid": "m0",
                "ordinal": 0,
                "ck": ck("m0", 0, "digest-first").ck,
            })],
        );
        assert!(matches!(
            handler.dispatch_value(7, digest_first).await,
            HandlerOutcome::Response(_)
        ));
        let mut changed_final = paged_transform_page(
            "transform",
            "ses",
            "digest-page",
            0,
            1,
            2,
            true,
            vec![json!({
                "mid": "m1",
                "ordinal": 1,
                "ck": ck("m1", 1, "original-final").ck,
            })],
        );
        changed_final["messages"] = json!([{
            "mid": "m1",
            "ordinal": 1,
            "ck": ck("m1", 1, "changed-final").ck,
        }]);
        assert_eq!(
            error_code(handler.dispatch_value(7, changed_final).await),
            "authority_transform_page_digest_mismatch"
        );

        let generation_first = paged_transform_page(
            "transform",
            "ses",
            "generation-page",
            1,
            0,
            2,
            false,
            vec![json!({
                "mid": "m0",
                "ordinal": 0,
                "ck": ck("m0", 0, "generation-first").ck,
            })],
        );
        assert!(matches!(
            handler.dispatch_value(7, generation_first).await,
            HandlerOutcome::Response(_)
        ));
        let generation_changed = paged_transform_page(
            "transform",
            "ses",
            "generation-page",
            2,
            1,
            2,
            true,
            vec![json!({
                "mid": "m1",
                "ordinal": 1,
                "ck": ck("m1", 1, "generation-final").ck,
            })],
        );
        assert_eq!(
            error_code(handler.dispatch_value(7, generation_changed).await),
            "authority_transform_page_attempt_mismatch"
        );

        let partial_envelope = json!({
            "method": "transform",
            "session_id": "ses",
            "transform_page_id": "partial",
        });
        assert_eq!(
            error_code(handler.dispatch_value(7, partial_envelope).await),
            "invalid_params"
        );
    }

    #[tokio::test]
    async fn paged_transform_sessions_are_isolated_and_shadow_pages_keep_the_gate() {
        let state = Arc::new(ProducerState::default());
        let (handler, _store, _dir, project) = handler_with_store(state, default_test_config());
        handler.bind_route(8, binding(project.to_str().unwrap(), "authority-a"));
        handler.bind_route(9, binding(project.to_str().unwrap(), "authority-b"));

        for (channel, session, mid, text) in [
            (8, "authority-a", "a0", "authority a"),
            (9, "authority-b", "b0", "authority b"),
        ] {
            let first = paged_transform_page(
                "transform",
                session,
                &format!("page-{session}"),
                0,
                0,
                2,
                false,
                vec![json!({
                    "mid": mid,
                    "ordinal": 0,
                    "ck": ck(mid, 0, text).ck,
                })],
            );
            assert!(matches!(
                handler.dispatch_value(channel, first).await,
                HandlerOutcome::Response(_)
            ));
        }
        for (channel, session, mid, text) in [
            (8, "authority-a", "a1", "authority a final"),
            (9, "authority-b", "b1", "authority b final"),
        ] {
            let final_page = paged_transform_page(
                "transform",
                session,
                &format!("page-{session}"),
                0,
                1,
                2,
                true,
                vec![json!({
                    "mid": mid,
                    "ordinal": 1,
                    "ck": ck(mid, 1, text).ck,
                })],
            );
            let response = handler.dispatch_value(channel, final_page).await;
            assert!(matches!(response, HandlerOutcome::Response(_)));
        }

        handler.bind_route(10, binding(project.to_str().unwrap(), "shadow:paged"));
        assert!(matches!(
            handler
                .dispatch_value(
                    10,
                    json!({ "kind": "shadow_reset", "session_id": "shadow:paged" })
                )
                .await,
            HandlerOutcome::Response(_)
        ));
        assert!(matches!(
            handler
                .dispatch_value(
                    10,
                    json!({
                        "kind": "state_sync",
                        "session_id": "shadow:paged",
                        "shadow_generation": 1,
                        "expected_shadow_seq": 0,
                    }),
                )
                .await,
            HandlerOutcome::Response(_)
        ));
        let shadow_first = paged_transform_page(
            "shadow_transform",
            "shadow:paged",
            "shadow-page",
            1,
            0,
            2,
            false,
            vec![json!({
                "mid": "shadow-m0",
                "ordinal": 0,
                "ck": ck("shadow-m0", 0, "shadow input").ck,
            })],
        );
        let shadow_final = paged_transform_page(
            "shadow_transform",
            "shadow:paged",
            "shadow-page",
            1,
            1,
            2,
            true,
            Vec::new(),
        );
        assert!(matches!(
            handler.dispatch_value(10, shadow_first).await,
            HandlerOutcome::Response(_)
        ));
        let shadow_response = handler.dispatch_value(10, shadow_final).await;
        let HandlerOutcome::Response(bytes) = shadow_response else {
            panic!("shadow page assembly should execute: {shadow_response:?}");
        };
        let shadow_response: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(shadow_response["class"], "identical");

        let mut disabled_config = default_test_config();
        disabled_config.shadow_enabled = false;
        let (disabled, _store, _dir, project) =
            handler_with_store(Arc::new(ProducerState::default()), disabled_config);
        disabled.bind_route(8, binding(project.to_str().unwrap(), "shadow:paged"));
        let shadow_page = paged_transform_page(
            "shadow_transform",
            "shadow:paged",
            "shadow-page",
            1,
            0,
            2,
            false,
            Vec::new(),
        );
        assert_eq!(
            error_code(disabled.dispatch_value(8, shadow_page).await),
            "shadow_disabled"
        );
    }

    #[tokio::test]
    async fn shadow_reset_and_state_sync_gate_generation_and_seq() {
        let state = Arc::new(ProducerState::default());
        let (handler, store, _dir, project) = handler_with_store(state, default_test_config());
        handler.bind_route(8, binding(project.to_str().unwrap(), "shadow:ses"));

        let reset = match handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:ses" }),
            )
            .await
        {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected reset outcome: {other:?}"),
        };
        assert_eq!(reset["shadow_generation"], 1);
        let stale = handler
            .dispatch_value(
                8,
                json!({
                    "kind": "state_sync",
                    "session_id": "shadow:ses",
                    "shadow_generation": 0,
                    "expected_shadow_seq": 0,
                }),
            )
            .await;
        assert_eq!(error_code(stale), "shadow_generation_mismatch");

        let synced = match handler
            .dispatch_value(
                8,
                json!({
                    "kind": "state_sync",
                    "session_id": "shadow:ses",
                    "shadow_generation": 1,
                    "expected_shadow_seq": 0,
                    "seed_boundary_id": "m0#0",
                    "compartments": [{
                        "sequence": 0,
                        "start_message": 0,
                        "end_message": 0,
                        "start_message_id": "m0#0",
                        "end_message_id": "m0#0",
                        "start_date": "2026-01-02",
                        "end_date": "2026-01-03",
                        "title": "c0",
                        "content": "summary",
                        "p1": "summary"
                    }],
                    "memories": [{ "id": 0, "category": "CONSTRAINTS", "content": "zero memory" }],
                    "memory_mutations": [{
                        "id": 0,
                        "mutation_type": "update",
                        "target_memory_id": 0,
                        "new_content": "zero memory updated"
                    }],
                    "user_profile": ["prefers root cause", "x < y"],
                    "last_todo_state": "[]"
                }),
            )
            .await
        {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected sync outcome: {other:?}"),
        };
        assert_eq!(synced["shadow_seq"], 1);
        let loaded = store.load("shadow:ses").unwrap();
        assert_eq!(loaded.meta.shadow_seq, 1);
        assert_eq!(loaded.core.boundary_id, "m0#0");
        assert_eq!(loaded.meta.coverage_ordinal, Some(0));
        assert_eq!(loaded.meta.coverage_start_ordinal, Some(0));
        assert_eq!(loaded.meta.coverage_compartment_seq, Some(0));
        assert_eq!(loaded.meta.folded_compartment_seq, 0);
        assert_eq!(loaded.meta.last_todo_state.as_deref(), Some("[]"));
        assert_eq!(
            store.load_shadow_user_profile("shadow:ses").unwrap(),
            vec!["prefers root cause", "x < y"]
        );
        let stored_compartments = store.load_compartments("shadow:ses").unwrap();
        assert_eq!(
            stored_compartments[0].start_date.as_deref(),
            Some("2026-01-02")
        );
        assert_eq!(
            stored_compartments[0].end_date.as_deref(),
            Some("2026-01-03")
        );
        let composed = crate::m0_compose::compose_m0_from_store(
            &store,
            &crate::m0_compose::M0ComposeInputs {
                session_id: "shadow:ses",
                project_path: "shadow:ses",
                project_directory: project.to_str().unwrap(),
                now_ms: 0,
                history_budget_tokens: 60_000.0,
                covered_system_messages: &[],
                memory_enabled: true,
            },
            |_| 0,
        )
        .unwrap();
        assert!(composed.m0_bytes.contains("## 0-0 · 2026-01-02→03 · c0"));
        assert_eq!(
            store.load_active_memories("shadow:ses", 0).unwrap()[0].id,
            0
        );

        let duplicate = handler
            .dispatch_value(
                8,
                json!({
                    "kind": "state_sync",
                    "session_id": "shadow:ses",
                    "shadow_generation": 1,
                    "expected_shadow_seq": 0,
                }),
            )
            .await;
        assert_eq!(error_code(duplicate), "shadow_seq_mismatch");
    }

    #[tokio::test]
    async fn paged_shadow_seed_profiles_reach_store_and_shadow_m0() {
        let state = Arc::new(ProducerState::default());
        let (handler, store, _dir, project) = handler_with_store(state, default_test_config());
        let session = "shadow:paged-profile";
        handler.bind_route(8, binding(project.to_str().unwrap(), session));
        assert!(matches!(
            handler
                .dispatch_value(8, json!({ "kind": "shadow_reset", "session_id": session }))
                .await,
            HandlerOutcome::Response(_)
        ));

        let mut first = paged_seed_batch(session, "profile-seed", 1, 0, 0, 3, vec![]);
        first["user_profile"] = json!(["prefers root cause"]);
        let mut second = paged_seed_batch(
            session,
            "profile-seed",
            1,
            0,
            1,
            3,
            vec![shadow_compartment(0, "first compartment")],
        );
        second["user_profile"] = json!(["x < y & z"]);
        let final_batch = paged_seed_batch(session, "profile-seed", 1, 0, 2, 3, vec![]);

        assert!(matches!(
            handler.dispatch_value(8, first).await,
            HandlerOutcome::Response(_)
        ));
        assert!(store.load_shadow_user_profile(session).unwrap().is_empty());
        assert!(matches!(
            handler.dispatch_value(8, second).await,
            HandlerOutcome::Response(_)
        ));
        assert!(store.load_shadow_user_profile(session).unwrap().is_empty());
        assert!(matches!(
            handler.dispatch_value(8, final_batch).await,
            HandlerOutcome::Response(_)
        ));

        let profile = store.load_shadow_user_profile(session).unwrap();
        assert_eq!(profile, vec!["prefers root cause", "x < y & z"]);
        let composed = crate::m0_compose::compose_m0_from_store(
            &store,
            &crate::m0_compose::M0ComposeInputs {
                session_id: session,
                project_path: session,
                project_directory: project.to_str().unwrap(),
                now_ms: 0,
                history_budget_tokens: 60_000.0,
                covered_system_messages: &[],
                memory_enabled: true,
            },
            |_| 0,
        )
        .unwrap();
        assert_eq!(
            composed.m0_bytes,
            "<user-profile>\n- prefers root cause\n- x &lt; y &amp; z\n</user-profile>\n\n<session-history>\n## 0-0 · c0\nfirst compartment-p1\n</session-history>"
        );
    }

    #[tokio::test]
    async fn paged_shadow_seed_is_atomic_idempotent_and_matches_single_shot() {
        let state = Arc::new(ProducerState::default());
        let (handler, store, _dir, project) = handler_with_store(state, default_test_config());
        handler.bind_route(8, binding(project.to_str().unwrap(), "shadow:paged"));
        handler.bind_route(9, binding(project.to_str().unwrap(), "shadow:paged"));
        handler.bind_route(10, binding(project.to_str().unwrap(), "shadow:single"));
        let _ = handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:paged" }),
            )
            .await;

        let first_batch = paged_seed_batch(
            "shadow:paged",
            "seed-a",
            1,
            0,
            0,
            2,
            vec![shadow_compartment(0, "first")],
        );
        let first_ack = match handler.dispatch_value(8, first_batch.clone()).await {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected first batch outcome: {other:?}"),
        };
        assert_eq!(first_ack["staged"], true);
        assert_eq!(first_ack["next_expected_index"], 1);
        assert!(store.load_compartments("shadow:paged").unwrap().is_empty());
        let accounting_after_first = seed_accounting(&handler);

        let redrive = match handler.dispatch_value(8, first_batch).await {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected redrive outcome: {other:?}"),
        };
        assert_eq!(redrive["next_expected_index"], 1);
        assert_eq!(seed_accounting(&handler), accounting_after_first);

        let stale_zero = handler
            .dispatch_value(
                8,
                paged_seed_batch(
                    "shadow:paged",
                    "stale-seed",
                    0,
                    0,
                    0,
                    2,
                    vec![shadow_compartment(9, "stale")],
                ),
            )
            .await;
        assert_eq!(error_code(stale_zero), "shadow_generation_mismatch");
        assert_eq!(seed_accounting(&handler), accounting_after_first);
        let future_zero = handler
            .dispatch_value(
                8,
                paged_seed_batch(
                    "shadow:paged",
                    "future-seed",
                    2,
                    0,
                    0,
                    2,
                    vec![shadow_compartment(9, "future")],
                ),
            )
            .await;
        assert_eq!(error_code(future_zero), "shadow_generation_mismatch");
        assert_eq!(seed_accounting(&handler), accounting_after_first);

        let competing_transform = handler
            .dispatch_value(
                9,
                shadow_transform_body("shadow:paged", 1, Vec::new(), true),
            )
            .await;
        assert_eq!(error_code(competing_transform), "shadow_seed_in_progress");
        assert!(store.load_compartments("shadow:paged").unwrap().is_empty());
        let production = call_transform(&handler, vec![ck("prod-1", 1, "production")]).await;
        assert!(production["action"].is_string());

        let final_batch = paged_seed_batch(
            "shadow:paged",
            "seed-a",
            1,
            0,
            1,
            2,
            vec![shadow_compartment(1, "second")],
        );
        let final_ack = match handler.dispatch_value(8, final_batch.clone()).await {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected final batch outcome: {other:?}"),
        };
        assert_eq!(final_ack["shadow_seq"], 1);
        assert_eq!(seed_accounting(&handler), (0, 0));

        let final_redrive = match handler.dispatch_value(8, final_batch).await {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected final redrive outcome: {other:?}"),
        };
        assert_eq!(final_redrive, final_ack);
        let mut altered_index_redrive = paged_seed_batch(
            "shadow:paged",
            "seed-a",
            1,
            0,
            1,
            2,
            vec![shadow_compartment(1, "second")],
        );
        altered_index_redrive["seed_batch_index"] = json!(0);
        altered_index_redrive["seed_complete"] = json!(false);
        let index_agnostic_redrive = match handler.dispatch_value(8, altered_index_redrive).await {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected index-agnostic redrive outcome: {other:?}"),
        };
        assert_eq!(index_agnostic_redrive, final_ack);

        let _ = handler
            .dispatch_value(
                10,
                json!({ "kind": "shadow_reset", "session_id": "shadow:single" }),
            )
            .await;
        let single_ack = handler
            .dispatch_value(
                10,
                json!({
                    "kind": "state_sync",
                    "session_id": "shadow:single",
                    "shadow_generation": 1,
                    "expected_shadow_seq": 0,
                    "compartments": [
                        shadow_compartment(0, "first"),
                        shadow_compartment(1, "second"),
                    ],
                    "last_todo_state": "[]",
                    "acked_watermarks": { "complete": true },
                }),
            )
            .await;
        assert!(matches!(single_ack, HandlerOutcome::Response(_)));
        assert_eq!(
            store.load_compartments("shadow:paged").unwrap(),
            store.load_compartments("shadow:single").unwrap()
        );
    }

    #[tokio::test]
    async fn paged_shadow_seed_rejects_protocol_and_digest_changes_without_leaking() {
        let state = Arc::new(ProducerState::default());
        let (handler, _store, _dir, project) = handler_with_store(state, default_test_config());
        handler.bind_route(8, binding(project.to_str().unwrap(), "shadow:ses"));

        let _ = handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:ses" }),
            )
            .await;
        let partial = handler
            .dispatch_value(
                8,
                json!({
                    "kind": "state_sync",
                    "session_id": "shadow:ses",
                    "shadow_generation": 1,
                    "expected_shadow_seq": 0,
                    "seed_id": "partial",
                }),
            )
            .await;
        assert_eq!(error_code(partial), "invalid_params");
        assert_eq!(seed_accounting(&handler), (0, 0));

        let _ = handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:ses" }),
            )
            .await;
        let first = paged_seed_batch(
            "shadow:ses",
            "digest-seed",
            2,
            0,
            0,
            2,
            vec![shadow_compartment(0, "original")],
        );
        assert!(matches!(
            handler.dispatch_value(8, first).await,
            HandlerOutcome::Response(_)
        ));
        let changed = handler
            .dispatch_value(
                8,
                paged_seed_batch(
                    "shadow:ses",
                    "digest-seed",
                    2,
                    0,
                    0,
                    2,
                    vec![shadow_compartment(0, "changed")],
                ),
            )
            .await;
        assert_eq!(error_code(changed), "shadow_seed_digest_mismatch");
        assert_eq!(seed_accounting(&handler), (0, 0));

        let _ = handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:ses" }),
            )
            .await;
        let mut scalar_on_intermediate = paged_seed_batch(
            "shadow:ses",
            "scalar-seed",
            3,
            0,
            0,
            2,
            vec![shadow_compartment(0, "first")],
        );
        scalar_on_intermediate["last_todo_state"] = json!("not-final");
        let scalar_error = handler.dispatch_value(8, scalar_on_intermediate).await;
        assert_eq!(error_code(scalar_error), "shadow_seed_protocol_mismatch");
        assert_eq!(seed_accounting(&handler), (0, 0));

        let _ = handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:ses" }),
            )
            .await;
        let mut completion_mismatch = paged_seed_batch(
            "shadow:ses",
            "completion-seed",
            4,
            0,
            0,
            2,
            vec![shadow_compartment(0, "first")],
        );
        completion_mismatch["seed_complete"] = json!(true);
        let completion_error = handler.dispatch_value(8, completion_mismatch).await;
        assert_eq!(
            error_code(completion_error),
            "shadow_seed_protocol_mismatch"
        );
        assert_eq!(seed_accounting(&handler), (0, 0));

        let _ = handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:ses" }),
            )
            .await;
        let mut missing_final_scalar = paged_seed_batch(
            "shadow:ses",
            "missing-tail-seed",
            5,
            0,
            0,
            1,
            vec![shadow_compartment(0, "only")],
        );
        missing_final_scalar
            .as_object_mut()
            .expect("seed body")
            .remove("workspace");
        let missing_tail = handler.dispatch_value(8, missing_final_scalar).await;
        assert_eq!(error_code(missing_tail), "shadow_seed_protocol_mismatch");
        assert_eq!(seed_accounting(&handler), (0, 0));

        let _ = handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:ses" }),
            )
            .await;
        let first = paged_seed_batch(
            "shadow:ses",
            "gap-seed",
            6,
            0,
            0,
            3,
            vec![shadow_compartment(0, "first")],
        );
        assert!(matches!(
            handler.dispatch_value(8, first).await,
            HandlerOutcome::Response(_)
        ));
        let gap = handler
            .dispatch_value(
                8,
                paged_seed_batch(
                    "shadow:ses",
                    "gap-seed",
                    6,
                    0,
                    2,
                    3,
                    vec![shadow_compartment(2, "gap")],
                ),
            )
            .await;
        assert_eq!(error_code(gap), "shadow_seed_order_mismatch");
        assert_eq!(seed_accounting(&handler), (0, 0));
    }

    #[test]
    fn shadow_seed_pending_count_is_handler_wide_and_bounded() {
        let mut seeds = ShadowSeedCoordinator {
            max_pending_seeds: 1,
            ..ShadowSeedCoordinator::default()
        };
        assert!(seeds.arm_after_reset("shadow:a", 1, 0));
        assert!(!seeds.arm_after_reset("shadow:b", 1, 0));
        assert_eq!(seeds.pending_seed_count, 1);
        assert!(matches!(
            seeds.sessions.get("shadow:a").map(|state| &state.phase),
            Some(ShadowSeedPhase::AwaitingSeed { .. })
        ));
        assert!(matches!(
            seeds.sessions.get("shadow:b").map(|state| &state.phase),
            Some(ShadowSeedPhase::Idle)
        ));
    }

    #[tokio::test]
    async fn shadow_seed_handler_wide_accounting_releases_every_non_apply_exit() {
        let state = Arc::new(ProducerState::default());
        let (handler, _store, _dir, project) = handler_with_store(state, default_test_config());
        handler.bind_route(8, binding(project.to_str().unwrap(), "shadow:a"));
        handler.bind_route(9, binding(project.to_str().unwrap(), "shadow:b"));
        let batch_a = paged_seed_batch(
            "shadow:a",
            "seed-a",
            1,
            0,
            0,
            2,
            vec![shadow_compartment(0, &"a".repeat(256))],
        );
        let batch_b = paged_seed_batch(
            "shadow:b",
            "seed-b",
            1,
            0,
            0,
            2,
            vec![shadow_compartment(0, &"b".repeat(256))],
        );
        let bytes_a = serde_json::to_vec(&batch_a).unwrap().len();
        let bytes_b = serde_json::to_vec(&batch_b).unwrap().len();
        {
            let mut seeds = handler.shadow_seeds.lock().expect("shadow seed mutex");
            seeds.max_staged_bytes = bytes_a + bytes_b - 1;
            seeds.max_pending_seeds = 8;
        }
        for (channel, session) in [(8, "shadow:a"), (9, "shadow:b")] {
            let outcome = handler
                .dispatch_value(
                    channel,
                    json!({ "kind": "shadow_reset", "session_id": session }),
                )
                .await;
            assert!(matches!(outcome, HandlerOutcome::Response(_)));
        }
        assert!(matches!(
            handler.dispatch_value(8, batch_a).await,
            HandlerOutcome::Response(_)
        ));
        let overflow = handler.dispatch_value(9, batch_b).await;
        assert_eq!(error_code(overflow), "shadow_seed_buffer_overflow");
        assert_eq!(seed_accounting(&handler).1, 1);
        assert_eq!(seed_accounting(&handler).0, bytes_a);

        handler.unbind_route(8);
        assert_eq!(seed_accounting(&handler), (0, 0));

        handler.bind_route(8, binding(project.to_str().unwrap(), "shadow:rebind"));
        let _ = handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:rebind" }),
            )
            .await;
        let rebind_batch = paged_seed_batch(
            "shadow:rebind",
            "rebind-seed",
            1,
            0,
            0,
            2,
            vec![shadow_compartment(0, "rebind")],
        );
        assert!(matches!(
            handler.dispatch_value(8, rebind_batch).await,
            HandlerOutcome::Response(_)
        ));
        assert!(seed_accounting(&handler).0 > 0);
        let reset = handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:rebind" }),
            )
            .await;
        assert!(matches!(reset, HandlerOutcome::Response(_)));
        assert_eq!(seed_accounting(&handler), (0, 1));
        let post_reset_batch = paged_seed_batch(
            "shadow:rebind",
            "post-reset-seed",
            2,
            0,
            0,
            2,
            vec![shadow_compartment(0, "post-reset")],
        );
        assert!(matches!(
            handler.dispatch_value(8, post_reset_batch).await,
            HandlerOutcome::Response(_)
        ));
        handler.bind_route(8, binding(project.to_str().unwrap(), "shadow:replacement"));
        assert_eq!(seed_accounting(&handler), (0, 0));
    }

    #[tokio::test]
    async fn paged_shadow_seed_uses_pinned_seq_and_restart_requires_fresh_reset() {
        let state = Arc::new(ProducerState::default());
        let (handler, store, _dir, project) =
            handler_with_store(Arc::clone(&state), default_test_config());
        handler.bind_route(8, binding(project.to_str().unwrap(), "shadow:ses"));
        let _ = handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:ses" }),
            )
            .await;
        assert!(matches!(
            handler
                .dispatch_value(
                    8,
                    paged_seed_batch(
                        "shadow:ses",
                        "pinned-seed",
                        1,
                        0,
                        0,
                        2,
                        vec![shadow_compartment(0, "first")],
                    ),
                )
                .await,
            HandlerOutcome::Response(_)
        ));
        store
            .apply_shadow_state_sync(ShadowStateSyncRequest {
                session_id: "shadow:ses",
                shadow_project_path: "shadow:ses",
                shadow_generation: 1,
                expected_shadow_seq: 0,
                seed_boundary_id: None,
                drop_seeds: &[],
                drop_seed_skipped: 0,
                compartments: &[],
                memories: &[],
                memory_mutations: &[],
                user_profile: &[],
                workspace: None,
                last_todo_state: None,
                acked_watermarks: Value::Null,
            })
            .unwrap();
        let final_after_advance = handler
            .dispatch_value(
                8,
                paged_seed_batch(
                    "shadow:ses",
                    "pinned-seed",
                    1,
                    0,
                    1,
                    2,
                    vec![shadow_compartment(1, "second")],
                ),
            )
            .await;
        assert_eq!(error_code(final_after_advance), "shadow_seq_mismatch");
        assert_eq!(seed_accounting(&handler), (0, 0));
        assert!(store.load_compartments("shadow:ses").unwrap().is_empty());

        let restarted = McHandler::with_producer_factory_and_config(
            Arc::new(TestProducerFactory { state }),
            default_test_config(),
        );
        restarted.store.set(Arc::clone(&store)).ok().unwrap();
        restarted.bind_route(8, binding(project.to_str().unwrap(), "shadow:ses"));
        let stray_final = restarted
            .dispatch_value(
                8,
                paged_seed_batch(
                    "shadow:ses",
                    "old-process-seed",
                    1,
                    1,
                    1,
                    2,
                    vec![shadow_compartment(1, "never-apply")],
                ),
            )
            .await;
        assert_eq!(error_code(stray_final), "shadow_seed_not_armed");
        assert!(store.load_compartments("shadow:ses").unwrap().is_empty());

        let reset = match restarted
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:ses" }),
            )
            .await
        {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected restart reset outcome: {other:?}"),
        };
        let fresh_generation = reset["shadow_generation"].as_u64().unwrap();
        let fresh = restarted
            .dispatch_value(
                8,
                paged_seed_batch(
                    "shadow:ses",
                    "fresh-process-seed",
                    fresh_generation,
                    0,
                    0,
                    1,
                    vec![shadow_compartment(0, "fresh")],
                ),
            )
            .await;
        assert!(matches!(fresh, HandlerOutcome::Response(_)));
        assert_eq!(store.load_compartments("shadow:ses").unwrap().len(), 1);
    }

    #[test]
    fn paged_shadow_transform_reassembles_to_the_unpaged_request() {
        let original = json!({
            "method": "shadow_transform",
            "session_id": "shadow:large",
            "shadow_generation": 4,
            "seed_pass": false,
            "input": [json!({ "id": "m1", "text": "a" }), json!({ "id": "m2", "text": "b" })],
            "ts_output": [json!({ "id": "m1", "text": "a" })],
            "normalizations": [json!({ "kind": "summary_message", "message_id": "s" })],
            "pass_inputs": shadow_pass_inputs(),
            "ts_decision": json!({ "class": "defer" }),
            "declared_trim": Value::Null,
        });
        let page = |index: usize,
                    total: usize,
                    complete: bool,
                    input: Vec<Value>,
                    output: Vec<Value>,
                    normalizations: Vec<Value>| {
            let mut page = json!({
                "method": "shadow_transform",
                "session_id": "shadow:large",
                "shadow_generation": 4,
                "transform_page_id": "page-a",
                "transform_generation": 4,
                "transform_page_index": index,
                "transform_page_total": total,
                "transform_page_complete": complete,
                "input": input,
                "ts_output": output,
                "normalizations": normalizations,
            });
            page["transform_page_digest"] = json!(transform_page_content_digest(&page));
            if complete {
                page["seed_pass"] = original["seed_pass"].clone();
                page["pass_inputs"] = original["pass_inputs"].clone();
                page["ts_decision"] = original["ts_decision"].clone();
                page["declared_trim"] = original["declared_trim"].clone();
            }
            page
        };
        let assembled = assemble_transform_pages(vec![
            page(
                0,
                2,
                false,
                vec![original["input"][0].clone()],
                vec![],
                vec![],
            ),
            page(
                1,
                2,
                true,
                vec![original["input"][1].clone()],
                vec![original["ts_output"][0].clone()],
                vec![original["normalizations"][0].clone()],
            ),
        ])
        .expect("paged transform should assemble");
        assert_eq!(assembled, original);
    }

    #[test]
    fn paged_shadow_transform_reassembles_oversized_item_continuations() {
        let item = Value::String("x".repeat(2 * 1024 * 1024));
        let serialized = serde_json::to_string(&item).unwrap();
        let chunk_size = 64 * 1024;
        let chunk_total = serialized.len().div_ceil(chunk_size);
        let mut pages = Vec::new();
        for chunk_index in 0..chunk_total {
            let start = chunk_index * chunk_size;
            let end = std::cmp::min(start + chunk_size, serialized.len());
            let mut page = json!({
                "method": "shadow_transform",
                "session_id": "shadow:oversized",
                "shadow_generation": 4,
                "transform_page_id": "oversized-page",
                "transform_generation": 4,
                "transform_page_index": chunk_index,
                "transform_page_total": chunk_total,
                "transform_page_complete": chunk_index + 1 == chunk_total,
                "input": [{
                    "__shadow_item_continuation": {
                        "field": "input",
                        "item_index": 0,
                        "chunk_index": chunk_index,
                        "chunk_total": chunk_total,
                    },
                    "chunk": &serialized[start..end],
                }],
                "ts_output": [],
                "normalizations": [],
            });
            page["transform_page_digest"] = json!(transform_page_content_digest(&page));
            if chunk_index + 1 == chunk_total {
                page["pass_inputs"] = shadow_pass_inputs();
                page["ts_decision"] = json!({ "class": "defer" });
                page["declared_trim"] = Value::Null;
            }
            pages.push(page);
        }

        let assembled = assemble_transform_pages(pages).expect("continuation item should assemble");
        assert_eq!(assembled["input"], json!([item]));
    }

    #[test]
    fn paged_shadow_transform_generation_change_discards_partial_attempt() {
        let mut coordinator = TransformPageCoordinator::default();
        let page = |generation: u64, index: usize, complete: bool| {
            let mut page = json!({
                "method": "shadow_transform",
                "session_id": "shadow:generation",
                "shadow_generation": generation,
                "transform_page_id": "generation-page",
                "transform_generation": generation,
                "transform_page_index": index,
                "transform_page_total": 2,
                "transform_page_complete": complete,
                "input": [format!("page-{generation}-{index}")],
            });
            let digest = transform_page_content_digest(&page);
            page["transform_page_digest"] = json!(digest.clone());
            (page, digest)
        };
        let (first, first_digest) = page(1, 0, false);
        let first_bytes = serde_json::to_vec(&first).unwrap().len();
        assert!(matches!(
            coordinator.stage(
                "shadow:generation",
                "generation-page".to_string(),
                1,
                0,
                2,
                first_digest,
                first,
                first_bytes,
                false,
            ),
            Ok(TransformPageStageAction::Ack(1))
        ));
        let (stale, stale_digest) = page(2, 1, true);
        let stale_bytes = serde_json::to_vec(&stale).unwrap().len();
        assert!(matches!(
            coordinator.stage(
                "shadow:generation",
                "generation-page".to_string(),
                2,
                1,
                2,
                stale_digest,
                stale,
                stale_bytes,
                true,
            ),
            Err(TransformPageStageError::AttemptMismatch)
        ));
        assert_eq!(coordinator.pending_transform_count, 0);
        assert_eq!(coordinator.total_staged_bytes, 0);
    }

    #[test]
    fn paged_shadow_transform_aggregate_cap_discards_partial_attempt() {
        let mut coordinator = TransformPageCoordinator::default();
        let first = json!({
            "method": "shadow_transform",
            "session_id": "shadow:cap",
            "shadow_generation": 1,
            "transform_page_id": "cap",
            "transform_generation": 1,
            "transform_page_index": 0,
            "transform_page_total": 2,
            "transform_page_complete": false,
            "input": ["first"],
        });
        let first_digest = transform_page_content_digest(&first);
        let first_bytes = serde_json::to_vec(&first).unwrap().len();
        coordinator.max_staged_bytes = first_bytes * 2 - 1;
        assert!(matches!(
            coordinator.stage(
                "shadow:cap",
                "cap".to_string(),
                1,
                0,
                2,
                first_digest,
                first,
                first_bytes,
                false,
            ),
            Ok(TransformPageStageAction::Ack(1))
        ));
        let second = json!({
            "method": "shadow_transform",
            "session_id": "shadow:cap",
            "shadow_generation": 1,
            "transform_page_id": "cap",
            "transform_generation": 1,
            "transform_page_index": 1,
            "transform_page_total": 2,
            "transform_page_complete": true,
            "input": ["second"],
        });
        let second_digest = transform_page_content_digest(&second);
        let second_bytes = serde_json::to_vec(&second).unwrap().len();
        assert!(matches!(
            coordinator.stage(
                "shadow:cap",
                "cap".to_string(),
                1,
                1,
                2,
                second_digest,
                second,
                second_bytes,
                true,
            ),
            Err(TransformPageStageError::BufferOverflow)
        ));
        assert_eq!(coordinator.pending_transform_count, 0);
        assert_eq!(coordinator.total_staged_bytes, 0);
    }

    #[tokio::test]
    async fn shadow_transform_calibrates_once_then_quarantines_without_duplicate_rows() {
        let state = Arc::new(ProducerState::default());
        let (handler, store, _dir, project) =
            handler_with_store(Arc::clone(&state), default_test_config());
        handler.bind_route(8, binding(project.to_str().unwrap(), "shadow:ses"));
        let _ = handler
            .dispatch_value(
                8,
                json!({ "kind": "shadow_reset", "session_id": "shadow:ses" }),
            )
            .await;
        let _ = handler
            .dispatch_value(
                8,
                json!({
                    "kind": "state_sync",
                    "session_id": "shadow:ses",
                    "shadow_generation": 1,
                    "expected_shadow_seq": 0,
                }),
            )
            .await;

        let seed_report = match handler
            .dispatch_value(8, shadow_transform_body("shadow:ses", 1, Vec::new(), true))
            .await
        {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected seed transform outcome: {other:?}"),
        };
        assert_eq!(seed_report["class"], "identical");
        assert_eq!(seed_report["compared"], false);
        assert_eq!(
            store.load_shadow_divergences("shadow:ses").unwrap().len(),
            0
        );
        assert!(store.load("shadow:ses").unwrap().meta.initialized);

        let report = match handler
            .dispatch_value(8, shadow_transform_body("shadow:ses", 1, Vec::new(), true))
            .await
        {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected warm shadow transform outcome: {other:?}"),
        };
        assert_eq!(report["class"], "byte-mismatch");
        assert_eq!(report["quarantined"], true);
        assert!(report.get("replay").is_some());
        assert!(store.load("shadow:ses").unwrap().meta.shadow_quarantined);
        assert_eq!(
            store.load_shadow_divergences("shadow:ses").unwrap().len(),
            1
        );
        assert_eq!(state.starts.load(Ordering::SeqCst), 0);
        assert_eq!(
            store
                .load("shadow:ses")
                .unwrap()
                .meta
                .historian
                .last_no_fire,
            None
        );

        let before = store.load("shadow:ses").unwrap().row_version;
        let decision_only = match handler
            .dispatch_value(8, shadow_transform_body("shadow:ses", 1, Vec::new(), false))
            .await
        {
            HandlerOutcome::Response(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap(),
            other => panic!("unexpected quarantined outcome: {other:?}"),
        };
        assert_eq!(decision_only["class"], "quarantined");
        assert_eq!(decision_only["compared"], false);
        let quarantined = store.load("shadow:ses").unwrap();
        assert!(quarantined.row_version > before);
        assert_eq!(quarantined.meta.shadow_quarantined_pass_count, 1);
        assert_eq!(
            store.load_shadow_divergences("shadow:ses").unwrap().len(),
            1
        );
    }

    #[test]
    fn shadow_decision_comparator_maps_ts_classes_to_rust_actions() {
        let messages = vec![ck("same", 1, "same").ck];
        let ts_defer = json!({ "class": "defer" });
        let ts_soft = json!({ "class": "soft" });
        let ts_hard = json!({ "class": "hard" });

        assert_eq!(
            compare_shadow_outputs(
                &messages,
                &messages,
                &ts_defer,
                &json!({ "action": "SOFT+", "class": "defer" }),
                &[],
                None,
            )
            .class,
            "identical"
        );
        assert_eq!(
            compare_shadow_outputs(
                &messages,
                &messages,
                &ts_soft,
                &json!({ "action": "SOFT", "class": "soft" }),
                &[],
                None,
            )
            .class,
            "identical"
        );
        assert_eq!(
            compare_shadow_outputs(
                &messages,
                &messages,
                &ts_hard,
                &json!({ "action": "HARD", "class": "hard" }),
                &[],
                None,
            )
            .class,
            "identical"
        );
        assert_eq!(
            compare_shadow_outputs(
                &messages,
                &messages,
                &ts_soft,
                &json!({ "action": "SOFT+", "class": "defer" }),
                &[],
                None,
            )
            .class,
            "decision-mismatch"
        );
    }

    #[test]
    fn byte_mismatch_diagnostics_localize_early_and_mid_array_differences() {
        let shared_prefix = "a".repeat(5_000);
        let ts_messages = vec![
            ck("m1", 1, &shared_prefix).ck,
            ck("m2", 2, "before TS_DIFFERENCE after").ck,
            ck("m3", 3, "unchanged tail").ck,
        ];
        let rs_messages = vec![
            ck("m1", 1, &shared_prefix).ck,
            ck("m2", 2, "before RS_DIFFERENCE after").ck,
            ck("m3", 3, "unchanged tail").ck,
        ];
        let decision = json!({ "class": "defer" });

        let mismatch =
            compare_shadow_outputs(&ts_messages, &rs_messages, &decision, &decision, &[], None);
        let ts_canonical = canonical_messages(&ts_messages);
        let expected_offset = ts_canonical.find("TS_DIFFERENCE").unwrap() as u64;
        assert!(expected_offset > SHADOW_COMPARE_PREFIX_LIMIT as u64);
        assert_eq!(mismatch.class, "byte-mismatch");
        assert_eq!(mismatch.first_diff_offset, Some(expected_offset));
        assert_eq!(mismatch.first_mid.as_deref(), Some("m2"));
        assert_eq!(mismatch.first_block.as_deref(), Some("m2#0"));
        assert!(mismatch.ts_window.contains("TS_DIFFERENCE"));
        assert!(mismatch.rs_window.contains("RS_DIFFERENCE"));

        let early_ts = vec![ck("early", 1, "hello").ck];
        let early_rs = vec![ck("early", 1, "jello").ck];
        let early = compare_shadow_outputs(&early_ts, &early_rs, &decision, &decision, &[], None);
        let early_canonical = canonical_messages(&early_ts);
        assert_eq!(
            early.first_diff_offset,
            Some(early_canonical.find("hello").unwrap() as u64)
        );
        assert!(early.ts_window.contains("hello"));
        assert!(early.rs_window.contains("jello"));

        let identical =
            compare_shadow_outputs(&early_ts, &early_ts, &decision, &decision, &[], None);
        assert_eq!(identical.class, "identical");
        assert_eq!(identical.first_diff_offset, None);
    }
}
