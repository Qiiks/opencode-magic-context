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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;

use chrono::{Local, TimeZone};
use cortexkit_store_types::{sqlite_store_path, Isolation, StorageBackend, StorageDescriptor};
use mc_store::{
    HistorianPhase, InsertMemoryInput, McStore, NoteInput, ShadowDivergenceRecord,
    ShadowMemoryMutationRow, ShadowMemoryRow, ShadowStateSyncError, ShadowStateSyncRequest,
    ShadowWorkspaceMemberRow, ShadowWorkspaceRow, StoredChunkTranscript, StoredCompartment,
    StoredMemoryMutation, StoredNote,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use subc_client_rs::{async_trait, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest};

use boundary::{BoundaryBlock, BoundaryContext, BoundaryMsg, Role, TriggerContext};
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

/// The per-route binding: the project, harness, session-slot value, and render budget
/// frozen at bind. Transform routes carry the durable session in `session`; MCP facade
/// routes carry an instance token there and must resolve it before touching the store.
/// The project is NEVER taken from a per-pass request field — a crafted request could
/// spoof it to read another project's memories — so it lives here, keyed by the route
/// channel the daemon controls.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionBinding {
    pub project_root: PathBuf,
    pub harness: String,
    pub session: String,
    pub model_key: Option<String>,
    pub config: McModuleConfig,
    /// The history budget (tokens) FROZEN at bind. Byte-affecting (a different budget → a
    /// different m0 trim → different bytes), so it's read once and never per-pass. A
    /// default for now (reading it from config is a later refinement); the freeze-once is
    /// the load-bearing part — it can't change mid-session.
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

/// Render-config epoch members, co-owned with byte-splice-consumer codecs (ai-proxy).
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
pub const MEMORY_RENDER_FORMAT_EPOCH: u32 = 1;
/// Bumps when the rendered m0 prefix format changes for the claude-code-anthropic
/// profile; epoch 1 includes covered system messages in m0 instead of sending them as
/// separate system-role messages.
pub const PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC: u32 = 1;
/// Bumps when the visible tagging surface changes (tag prefixes shipping is 0 -> 1).
pub const TAGGER_FEATURE_EPOCH: u32 = 0;

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

/// The tagger epoch folded for this profile. The gate and epoch must both be enabled:
/// this makes a partial deploy inert rather than allowing tag bytes without a new render
/// identity.
#[cfg(not(test))]
pub const fn tagger_feature_epoch(profile: SerializerProfile) -> u32 {
    if healing::tagging_enabled(profile) {
        TAGGER_FEATURE_EPOCH
    } else {
        0
    }
}

/// Tests exercise the future non-zero fold while the production epoch remains zero.
#[cfg(test)]
pub fn tagger_feature_epoch(profile: SerializerProfile) -> u32 {
    if healing::tagging_enabled(profile) {
        1
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
/// Mirrors packages/plugin/src/config/schema/magic-context.ts commit_cluster_trigger.enabled default.
const DEFAULT_COMMIT_CLUSTER_TRIGGER_ENABLED: bool = true;
/// Mirrors packages/plugin/src/config/schema/magic-context.ts commit_cluster_trigger.min_clusters default.
const DEFAULT_MIN_COMMIT_CLUSTERS: usize = 3;
/// Mirrors packages/plugin/src/hooks/magic-context/derive-budgets.ts with the default
/// 128K historian context fallback: clamp(128_000 × 0.25, 8_000, 50_000) = 32_000.
const DEFAULT_HISTORIAN_CHUNK_TOKENS: usize = 32_000;
/// Secondary assembler guard; TS trigger sizing is authoritative, this only rejects tiny chunks.
const DEFAULT_HISTORIAN_MIN_CHUNK_TOKENS: usize = 512;
/// After a historian abandon, suppress refires for this long so a persistently
/// failing model does not burn a full summarization pass on every transform.
const HISTORIAN_FAILURE_BACKOFF_MS: i64 = historian::HISTORIAN_FAILURE_BACKOFF_MS;
const SESSION_UNRESOLVED_MESSAGE: &str =
    "session unresolved; launch Claude Code through the CortexKit wrapper so ctx_* can bind to this conversation";
const SHADOW_SESSION_PREFIX: &str = mc_store::SHADOW_SESSION_PREFIX;
const SHADOW_COMPARE_PREFIX_LIMIT: usize = 4096;

#[derive(Debug, Deserialize)]
struct ShadowStateSyncWire {
    #[serde(default)]
    session_id: Option<String>,
    shadow_generation: u64,
    expected_shadow_seq: u64,
    #[serde(default)]
    seed_boundary_id: Option<String>,
    #[serde(default)]
    compartments: Vec<ShadowCompartmentWire>,
    #[serde(default)]
    memories: Vec<ShadowMemoryWire>,
    #[serde(default)]
    memory_mutations: Vec<ShadowMemoryMutationWire>,
    #[serde(default)]
    workspace: Option<ShadowWorkspaceWire>,
    #[serde(default)]
    last_todo_state: Option<String>,
    #[serde(default)]
    acked_watermarks: Option<Value>,
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
    usage: Option<ShadowUsageWire>,
    #[serde(
        alias = "effective_execute_threshold",
        alias = "execute_threshold_percentage"
    )]
    effective_execute_threshold: f64,
    #[serde(default)]
    history_budget_tokens: Option<f64>,
    #[serde(default = "default_cache_ttl")]
    cache_ttl: String,
    #[serde(default)]
    provider_error: Option<String>,
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
    memory_project_path: String,
    conversation_key: String,
}

fn default_cache_ttl() -> String {
    "5m".to_string()
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

/// The module handler. Holds the single store handle (opened once in `on_hello_ack`)
/// and the per-route session bindings (channel → {project, session}).
pub struct McHandler {
    store: OnceLock<Arc<McStore>>,
    producer_factory: Arc<dyn HistorianProducerFactory>,
    session_resolver: Arc<dyn SessionResolver>,
    config: Mutex<ConfigCache>,
    #[cfg(test)]
    fixed_config: Option<McModuleConfig>,
    reattaching_sessions: Arc<Mutex<HashSet<String>>>,
    live_historian_sessions: Arc<Mutex<HashMap<String, LiveHistorianSession>>>,
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
    /// Route channel → its session binding. Populated at `on_bind`, removed at
    /// `on_route_gone`. A `Mutex<HashMap>` (not a lock-free map) because writes are
    /// rare (once per route open/close) and reads are one cheap lookup per transform.
    bindings: Mutex<HashMap<u16, SessionBinding>>,
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

struct HistorianFiringTask {
    store: Arc<McStore>,
    session_id: String,
    project_path: String,
    project_root: PathBuf,
    project_slug: String,
    firing: AssembledHistorianFiring,
    live_guard: SessionSetGuard,
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
            scheduler_observations: Mutex::new(HashMap::new()),
            guidance_dates: Mutex::new(HashMap::new()),
            #[cfg(test)]
            guidance_now_ms: Mutex::new(None),
            #[cfg(test)]
            reduction_injection: Mutex::new(HashMap::new()),
            #[cfg(test)]
            between_transform_and_prepare: Mutex::new(None),
            bindings: Mutex::new(HashMap::new()),
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
            scheduler_observations: Mutex::new(HashMap::new()),
            guidance_dates: Mutex::new(HashMap::new()),
            guidance_now_ms: Mutex::new(None),
            reduction_injection: Mutex::new(HashMap::new()),
            between_transform_and_prepare: Mutex::new(None),
            bindings: Mutex::new(HashMap::new()),
        }
    }

    /// Record the route's session binding (called from `on_bind`). Last write wins for a
    /// reused channel — the daemon won't reuse a channel without a `route.gone` first, so
    /// this only overwrites a stale entry that somehow survived (defensive).
    fn bind_route(&self, channel: u16, binding: SessionBinding) {
        self.bindings
            .lock()
            .expect("bindings mutex")
            .insert(channel, binding);
    }

    /// Remove a route's binding (called from `on_route_gone`) so a reused channel can't
    /// resolve a stale/wrong project, and the map doesn't leak entries.
    fn unbind_route(&self, channel: u16) {
        self.bindings
            .lock()
            .expect("bindings mutex")
            .remove(&channel);
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

    /// Return the channel binding without comparing a request session. MCP facade routes
    /// bind the `session` slot to an instance token, not to the durable conversation key;
    /// facade handlers must resolve that token through ai-proxy before touching the store.
    fn facade_binding(&self, channel: u16) -> Result<SessionBinding, BindingError> {
        self.bindings
            .lock()
            .expect("bindings mutex")
            .get(&channel)
            .cloned()
            .ok_or(BindingError::Unbound)
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

    fn maybe_spawn_reattach(
        &self,
        store: Arc<McStore>,
        parsed: &TransformRequest,
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
        now: i64,
    ) -> PreparedHistorianAction {
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
            },
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
        } = task;
        let _guard = live_guard;
        let failure_backoff_at_ms = firing.failure_backoff_at_ms;
        match factory.connect(&project_root).await {
            Ok(mut producer) => {
                let request =
                    firing.as_fire_request(&store, &session_id, &project_path, &project_slug);
                run_historian_firing(&mut *producer, request).await
            }
            Err(err) => {
                record_historian_connect_failure(
                    &store,
                    &session_id,
                    failure_backoff_at_ms,
                    &format!("producer connect: {err}"),
                );
                Err(historian::HistorianDriveError::Producer(err))
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

    fn handle_agent_drops_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => {
                return HandlerOutcome::Error {
                    code: "store_unavailable".to_string(),
                    message: "store not opened (no HELLO_ACK storage seam)".to_string(),
                }
            }
        };
        let Some(session_id) = request.get("session_id").and_then(Value::as_str) else {
            return HandlerOutcome::Error {
                code: "bad_request".to_string(),
                message: "ctx_reduce command requires session_id".to_string(),
            };
        };
        let binding = match self.resolve_binding(channel, session_id) {
            Ok(binding) => binding,
            Err(e) => {
                return match e {
                    BindingError::Unbound => HandlerOutcome::Error {
                        code: "route_unbound".to_string(),
                        message: "ctx_reduce on a channel with no session binding".to_string(),
                    },
                    BindingError::SessionMismatch => HandlerOutcome::Error {
                        code: "session_mismatch".to_string(),
                        message: "request session_id does not match the channel's bound session"
                            .to_string(),
                    },
                };
            }
        };
        if is_shadow_session(&binding.session) {
            return HandlerOutcome::Error {
                code: "non_shadow_op_on_shadow_binding".to_string(),
                message: "ctx_reduce is not accepted on shadow:<real_session> routes".to_string(),
            };
        }
        let drop_ids = drop_ids_from_command(&request);
        if drop_ids.is_empty() {
            return respond(json!({ "ok": true, "queued": 0 }));
        }
        match store.append_pending_agent_drops(session_id, &drop_ids, now_ms()) {
            Ok(queued) => respond(json!({ "ok": true, "queued": queued })),
            Err(e) => HandlerOutcome::Error {
                code: "store_write_failed".to_string(),
                message: e.to_string(),
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

        let date_line = match self.guidance_date_for_session(&store, session_id) {
            Ok(date) => date,
            Err(error) => {
                return HandlerOutcome::Error {
                    code: "store_write_failed".to_string(),
                    message: error.to_string(),
                }
            }
        };
        // Default is the full five-tool block; variant="no_reduce" serves the surface
        // without ctx_reduce/tag discipline for consumers whose tool policy hides it.
        let text = match request.get("variant").and_then(Value::as_str) {
            None | Some("full") => GUIDANCE_TEXT,
            Some("no_reduce") => GUIDANCE_TEXT_NO_REDUCE,
            Some(other) => {
                return HandlerOutcome::Error {
                    code: "bad_request".to_string(),
                    message: format!("unknown guidance variant: {other}"),
                }
            }
        };
        let bytes = guidance_bytes_for(text, &date_line);
        respond(json!({
            "ok": true,
            "bytes": bytes,
            "hash": sha256_hex(bytes.as_bytes()),
            // The guidance text is the only part reflected in render_config. The session
            // date line changes every day, so content_hash excludes it; otherwise a
            // date-only change would trigger cache refreshes even when guidance is
            // unchanged.
            "content_hash": sha256_hex(text.as_bytes()),
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
                "profile_epoch": PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC,
                "tagger_epoch": TAGGER_FEATURE_EPOCH,
            },
        }))
    }

    async fn handle_transform_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let parsed: TransformRequest = match serde_json::from_value(request.clone()) {
            Ok(req) => req,
            Err(e) => {
                return HandlerOutcome::Error {
                    code: "bad_request".to_string(),
                    message: e.to_string(),
                }
            }
        };
        if SerializerProfile::parse(&parsed.serializer_profile).is_none() {
            return unknown_serializer_profile_error();
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
            return respond(
                serde_json::to_value(transform::TransformResponse::passthrough(
                    parsed.messages.into_iter().map(|m| m.ck).collect(),
                    parsed.full_array_fingerprint,
                ))
                .unwrap_or(Value::Null),
            );
        }
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
        let project_path = binding.project_root.to_string_lossy().to_string();
        let pass_now = now_ms();
        // This trace is intentionally outside the fenced cache-state commit: a rejected
        // pass must still leave a durable breadcrumb, and a trace failure must never
        // change the transform result.
        let _ = store.trace_pass_received(&parsed.session_id, pass_now);
        let run_transform = || {
            let producer_ctx = transform::ProducerContext {
                project_path: &project_path,
                project_directory: &project_path,
                history_budget_tokens: binding.history_budget_tokens,
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
                pass_now,
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
                            pass_now,
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
                pass_now,
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
        let _ = store.trace_pass_completed(&parsed.session_id, now_ms());
        self.record_response_observation(&parsed.session_id, now_ms());
        respond(serde_json::to_value(response).unwrap_or(Value::Null))
    }

    #[cfg(test)]
    async fn handle_transform_for_test(&self, channel: u16, request: Value) -> HandlerOutcome {
        self.handle_transform_value(channel, request).await
    }

    fn handle_shadow_state_sync_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let parsed: ShadowStateSyncWire = match serde_json::from_value(request) {
            Ok(req) => req,
            Err(e) => return invalid_params_error(e.to_string()),
        };
        let binding = match self.shadow_binding(channel, parsed.session_id.as_deref()) {
            Ok(binding) => binding,
            Err(outcome) => return outcome,
        };
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => return store_unavailable_error(),
        };
        let compartments: Vec<StoredCompartment> = parsed
            .compartments
            .into_iter()
            .map(StoredCompartment::from)
            .collect();
        let has_workspace = parsed.workspace.is_some();
        let (workspace, member_paths) =
            match prepare_shadow_workspace(&binding.session, parsed.workspace) {
                Ok(prepared) => prepared,
                Err(error) => return invalid_params_error(error),
            };
        let root_path = shadow_project_path(&binding.session);
        let memories: Vec<ShadowMemoryRow> = match parsed
            .memories
            .into_iter()
            .map(|memory| {
                let project_path = shadow_source_path(
                    memory.project_path.as_deref(),
                    &root_path,
                    &member_paths,
                    has_workspace,
                )?;
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
                let project_path = shadow_source_path(
                    mutation.project_path.as_deref(),
                    &root_path,
                    &member_paths,
                    has_workspace,
                )?;
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
        match store.apply_shadow_state_sync(ShadowStateSyncRequest {
            session_id: &binding.session,
            shadow_project_path: &root_path,
            shadow_generation: parsed.shadow_generation,
            expected_shadow_seq: parsed.expected_shadow_seq,
            seed_boundary_id: parsed.seed_boundary_id.as_deref(),
            compartments: &compartments,
            memories: &memories,
            memory_mutations: &memory_mutations,
            workspace: workspace.as_ref(),
            last_todo_state: parsed.last_todo_state,
            acked_watermarks,
        }) {
            Ok(result) => respond(json!({
                "ok": true,
                "shadow_generation": result.shadow_generation,
                "shadow_seq": result.shadow_seq,
                "row_version": result.row_version,
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
            Err(ShadowStateSyncError::InvalidSeedBoundary { declared, detail }) => {
                HandlerOutcome::Error {
                    code: "shadow_seed_boundary_mismatch".to_string(),
                    message: format!("seed boundary {declared:?} rejected: {detail}"),
                }
            }
            Err(e) => HandlerOutcome::Error {
                code: "shadow_state_sync_failed".to_string(),
                message: e.to_string(),
            },
        }
    }

    fn handle_shadow_reset_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let parsed: ShadowResetWire = match serde_json::from_value(request) {
            Ok(req) => req,
            Err(e) => return invalid_params_error(e.to_string()),
        };
        let binding = match self.shadow_binding(channel, parsed.session_id.as_deref()) {
            Ok(binding) => binding,
            Err(outcome) => return outcome,
        };
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => return store_unavailable_error(),
        };
        match store.reset_shadow_session(&binding.session, &shadow_project_path(&binding.session)) {
            Ok(result) => respond(json!({
                "ok": true,
                "shadow_generation": result.shadow_generation,
                "shadow_seq": result.shadow_seq,
                "row_version": result.row_version,
                "previous_shadow_generation": parsed.shadow_generation,
                "reason": parsed.reason,
            })),
            Err(e) => HandlerOutcome::Error {
                code: "shadow_reset_failed".to_string(),
                message: e.to_string(),
            },
        }
    }

    async fn handle_shadow_transform_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let parsed: ShadowTransformWire = match serde_json::from_value(request.clone()) {
            Ok(req) => req,
            Err(e) => return invalid_params_error(e.to_string()),
        };
        let binding = match self.shadow_binding(channel, parsed.session_id.as_deref()) {
            Ok(binding) => binding,
            Err(outcome) => return outcome,
        };
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
            full_array_fingerprint: parsed.full_array_fingerprint.clone(),
            messages: shadow_input,
            tail_delta: None,
            usage,
            provider_error: parsed.pass_inputs.provider_error.clone(),
            declared_trim: parsed.declared_trim.clone(),
        };
        let shadow_project = shadow_project_path(&binding.session);
        let project_path = binding.project_root.to_string_lossy().to_string();
        let producer_ctx = transform::ProducerContext {
            project_path: &shadow_project,
            project_directory: &project_path,
            history_budget_tokens: parsed
                .pass_inputs
                .history_budget_tokens
                .unwrap_or(binding.history_budget_tokens),
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

    async fn handle_facade_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let Some(name) = request.get("name").and_then(Value::as_str) else {
            return unrecognized_request_error(&request);
        };
        match name {
            "ctx_memory" => self.handle_ctx_memory_facade(channel, &request).await,
            "ctx_search" => self.handle_ctx_search_facade(channel, &request).await,
            "ctx_expand" => self.handle_ctx_expand_facade(channel, &request).await,
            "ctx_reduce" => self.handle_ctx_reduce_facade(channel, &request).await,
            "ctx_note" => self.handle_ctx_note_facade(channel, &request).await,
            _ => unrecognized_request_error(&request),
        }
    }

    async fn resolve_facade_scope(&self, channel: u16) -> Result<FacadeScope, HandlerOutcome> {
        let binding = self
            .facade_binding(channel)
            .map_err(|_| session_unresolved_error())?;
        let instance_token = binding.session.trim();
        if instance_token.is_empty() {
            return Err(session_unresolved_error());
        }
        match self
            .session_resolver
            .resolve_session(&binding.project_root, &binding.harness, instance_token)
            .await
        {
            Ok(Some(resolved)) => Ok(FacadeScope {
                memory_project_path: binding.project_root.to_string_lossy().to_string(),
                conversation_key: resolved.session_id,
            }),
            Ok(None) => Err(session_unresolved_error()),
            Err(SessionResolveError::Timeout) => Err(HandlerOutcome::Error {
                code: "session_resolve_timeout".to_string(),
                message: "session.resolve timed out after 2s".to_string(),
            }),
            Err(error) => Err(HandlerOutcome::Error {
                code: "session_resolve_failed".to_string(),
                message: error.to_string(),
            }),
        }
    }

    async fn handle_ctx_reduce_facade(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(args) = facade_arguments(request) else {
            return invalid_params_error("ctx_reduce arguments must be an object");
        };
        let Some(drop_arg) = non_empty_string_arg(args, "drop") else {
            return tool_error_result("Error: 'drop' must be provided.");
        };
        let facade_scope = match self.resolve_facade_scope(channel).await {
            Ok(scope) => scope,
            Err(outcome) => return outcome,
        };
        let store = match self.store.get() {
            Some(store) => store,
            None => return store_unavailable_error(),
        };
        let session_id = facade_scope.conversation_key.as_str();
        let loaded = match store.load(session_id) {
            Ok(loaded) => loaded,
            Err(error) => return tool_error_result(format!("Error: {error}")),
        };
        let Some(profile) = SerializerProfile::parse(&loaded.meta.last_serializer_profile) else {
            return tagging_inactive_error();
        };
        if !healing::tagging_enabled(profile) {
            return tagging_inactive_error();
        }
        if !healing::tail_reclaim(profile) {
            return reduce_unavailable_for_profile_error();
        }

        let requested = match parse_tag_range_string(drop_arg) {
            Ok(ids) => ids,
            Err(error) => {
                return tool_error_result(format!("Error: Invalid range syntax. {error}"))
            }
        };
        let tags = match store.load_tags_for_session(session_id) {
            Ok(tags) => tags,
            Err(error) => return tool_error_result(format!("Error: {error}")),
        };
        let by_number = tags
            .iter()
            .map(|row| (row.tag_number, row))
            .collect::<HashMap<_, _>>();
        let mut block_ids = Vec::new();
        let mut queued_numbers = Vec::new();
        let mut unknown_numbers = Vec::new();
        for tag_number in requested {
            match by_number.get(&(tag_number as i64)) {
                Some(row) => {
                    block_ids.push(row.block_id.clone());
                    queued_numbers.push(tag_number as i64);
                }
                None => unknown_numbers.push(tag_number as i64),
            }
        }
        let inserted = if block_ids.is_empty() {
            0
        } else {
            match store.append_pending_agent_drops(session_id, &block_ids, now_ms()) {
                Ok(count) => count,
                Err(error) => return tool_error_result(format!("Error: {error}")),
            }
        };
        if inserted > 0 {
            suppress_channel1_after_ctx_reduce(store, session_id);
        }
        mcp_text_result(
            render_ctx_reduce_response(
                inserted,
                queued_numbers.len(),
                &queued_numbers,
                &unknown_numbers,
            ),
            false,
        )
    }

    async fn handle_ctx_memory_facade(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(args) = facade_arguments(request) else {
            return invalid_params_error("ctx_memory arguments must be an object");
        };
        let Some(action) = string_arg(args, "action") else {
            return invalid_params_error("ctx_memory requires an action");
        };
        let facade_scope = match self.resolve_facade_scope(channel).await {
            Ok(scope) => scope,
            Err(outcome) => return outcome,
        };
        let store = match self.store.get() {
            Some(store) => store,
            None => return store_unavailable_error(),
        };
        let memory_project = facade_scope.memory_project_path.as_str();
        let conversation_key = facade_scope.conversation_key.as_str();
        match action {
            "write" => {
                let Some(category) = non_empty_string_arg(args, "category") else {
                    return tool_error_result(
                        "Error: 'category' is required when action is 'write'.",
                    );
                };
                let Some(content) = non_empty_string_arg(args, "content") else {
                    return tool_error_result(
                        "Error: 'content' is required when action is 'write'.",
                    );
                };
                match store.insert_memory(InsertMemoryInput {
                    project_path: memory_project,
                    category,
                    content,
                    source_session_id: Some(conversation_key),
                    source_type: Some("agent"),
                    importance: Some(50),
                    expires_at: None,
                    metadata_json: None,
                    now_ms: now_ms(),
                }) {
                    Ok(id) => {
                        mcp_text_result(format!("Saved memory [ID: {id}] in {category}."), false)
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
                match memory_tool::update_memory(store, memory_project, id, content, now_ms()) {
                    Ok(memory) => mcp_text_result(
                        format!("Updated memory [ID: {}] in {}.", memory.id, memory.category),
                        false,
                    ),
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
                let mut archived = Vec::new();
                for id in ids {
                    match memory_tool::archive_memory(store, memory_project, id, reason, now_ms()) {
                        Ok(true) => archived.push(id),
                        Ok(false) => {}
                        Err(error) => return tool_error_result(format!("Error: {error}")),
                    }
                }
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
                match memory_tool::merge_memories(
                    store,
                    memory_project,
                    target_id,
                    &source_ids,
                    content,
                    now_ms(),
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
                    Err(error) => tool_error_result(format!("Error: {error}")),
                }
            }
            _ => tool_error_result("Error: Unknown ctx_memory action.".to_string()),
        }
    }

    async fn handle_ctx_search_facade(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(args) = facade_arguments(request) else {
            return invalid_params_error("ctx_search arguments must be an object");
        };
        let Some(query) = non_empty_string_arg(args, "query") else {
            return tool_error_result("Error: 'query' is required for ctx_search.");
        };
        let limit = usize_arg(args, "limit").unwrap_or(8).clamp(1, 25);
        let facade_scope = match self.resolve_facade_scope(channel).await {
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
        let Some(args) = facade_arguments(request) else {
            return invalid_params_error("ctx_expand arguments must be an object");
        };
        let facade_scope = match self.resolve_facade_scope(channel).await {
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

    async fn handle_ctx_note_facade(&self, channel: u16, request: &Value) -> HandlerOutcome {
        let Some(args) = facade_arguments(request) else {
            return invalid_params_error("ctx_note arguments must be an object");
        };
        let facade_scope = match self.resolve_facade_scope(channel).await {
            Ok(scope) => scope,
            Err(outcome) => return outcome,
        };
        let store = match self.store.get() {
            Some(store) => store,
            None => return store_unavailable_error(),
        };
        let project = facade_scope.memory_project_path.as_str();
        let session = facade_scope.conversation_key.as_str();
        let action = string_arg(args, "action")
            .or_else(|| non_empty_string_arg(args, "content").map(|_| "write"))
            .unwrap_or("read");
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
                match store.insert_note(NoteInput {
                    project_path: project,
                    session_id: session,
                    content,
                    surface_condition: string_arg(args, "surface_condition"),
                    anchor_block_id: anchor.as_deref(),
                    now_ms: now_ms(),
                }) {
                    Ok(note) => {
                        let mut message = format!("Saved session note #{}.", note.id);
                        if note.surface_condition.is_some() {
                            message.push_str(
                                " Surface condition recorded; condition evaluation arrives later.",
                            );
                        }
                        mcp_text_result(message, false)
                    }
                    Err(error) => tool_error_result(format!("Error: {error}")),
                }
            }
            "read" => {
                let limit = usize_arg(args, "limit").unwrap_or(25).clamp(1, 100);
                let offset = usize_arg(args, "offset").unwrap_or(0);
                match store.read_notes(project, session, limit, offset) {
                    Ok(notes) => mcp_text_result(render_notes(notes, offset), false),
                    Err(error) => tool_error_result(format!("Error: {error}")),
                }
            }
            "update" => {
                let Some(note_id) = i64_arg(args, "note_id").filter(|id| *id > 0) else {
                    return tool_error_result(
                        "Error: 'note_id' is required when action is 'update'.",
                    );
                };
                let Some(content) = non_empty_string_arg(args, "content") else {
                    return tool_error_result(
                        "Error: 'content' is required when action is 'update'.",
                    );
                };
                match store.update_note_content(project, session, note_id, content, now_ms()) {
                    Ok(Some(_)) => mcp_text_result(format!("Updated note #{note_id}."), false),
                    Ok(None) => tool_error_result(format!(
                        "Error: Note #{note_id} not found in your session/project or already dismissed."
                    )),
                    Err(error) => tool_error_result(format!("Error: {error}")),
                }
            }
            "dismiss" => {
                let Some(note_id) = i64_arg(args, "note_id").filter(|id| *id > 0) else {
                    return tool_error_result(
                        "Error: 'note_id' is required when action is 'dismiss'.",
                    );
                };
                match store.dismiss_note(
                    project,
                    session,
                    note_id,
                    string_arg(args, "content"),
                    now_ms(),
                ) {
                    Ok(Some(_)) => mcp_text_result(format!("Note #{note_id} dismissed."), false),
                    Ok(None) => tool_error_result(format!(
                        "Error: Note #{note_id} not found in your session/project or already dismissed."
                    )),
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
            req.route_channel,
            SessionBinding {
                project_root: req.identity.project_root.clone(),
                harness: req.identity.harness.clone(),
                session: req.identity.session.clone(),
                model_key: None,
                config,
                // Frozen at bind. Currently a default constant (reading it from config is a
                // later refinement); the load-bearing part is the freeze-once — a different
                // budget would change the rendered m0 bytes, so it can't move mid-session.
                history_budget_tokens: memory_render::DEFAULT_HISTORY_BUDGET_TOKENS,
            },
        );
        subc_client_rs::BindDecision::accept()
    }

    /// Drop the route's binding on teardown so a reused channel can't resolve a stale
    /// project and the map doesn't leak.
    async fn on_route_gone(&self, channel: u16) {
        let gone_session = {
            let bindings = self.bindings.lock().expect("bindings mutex");
            bindings.get(&channel).map(|b| b.session.clone())
        };
        self.unbind_route(channel);
        // Evict the scheduler observation when the session's LAST route closes —
        // the map is otherwise unbounded across a long-lived daemon's session churn.
        if let Some(session) = gone_session {
            let still_bound = {
                let bindings = self.bindings.lock().expect("bindings mutex");
                bindings.values().any(|b| b.session == session)
            };
            if !still_bound {
                self.scheduler_observations
                    .lock()
                    .expect("scheduler observations mutex")
                    .remove(&session);
            }
        }
    }

    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let request = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
        self.dispatch_value(ctx.channel(), request).await
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
                "guidance.get" => self.handle_guidance_value(channel, &request),
                // Handle transform requests: decode the incoming context array, update
                // cache state, and return the rewritten array for the caller.
                "transform" => self.handle_transform_value(channel, request).await,
                "state_sync" => self.handle_shadow_state_sync_value(channel, request),
                "shadow_transform" => self.handle_shadow_transform_value(channel, request).await,
                "shadow_reset" => self.handle_shadow_reset_value(channel, request),
                "ctx_reduce" | "append_agent_drops" | "agent_drops.append" => {
                    self.handle_agent_drops_value(channel, request)
                }
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

fn need_full_sync_response(full_array_fingerprint: Option<String>) -> HandlerOutcome {
    respond(
        serde_json::to_value(transform::TransformResponse::need_full_sync(
            full_array_fingerprint,
        ))
        .unwrap_or(Value::Null),
    )
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

fn facade_arguments(request: &Value) -> Option<&Map<String, Value>> {
    request.get("arguments")?.as_object()
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
    lines.join("\n")
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
    lines.join("\n")
}

fn render_notes(notes: Vec<StoredNote>, offset: usize) -> String {
    if notes.is_empty() {
        return "## Notes\n\nNo active session notes.".to_string();
    }
    let mut lines = vec!["## Session Notes".to_string(), String::new()];
    for note in &notes {
        let condition = note
            .surface_condition
            .as_ref()
            .map(|condition| {
                format!("\n  Condition (recorded, evaluation arrives later): {condition}")
            })
            .unwrap_or_default();
        let anchor = note
            .anchor_block_id
            .as_ref()
            .map(|anchor| format!(" ↳ @block {anchor}"))
            .unwrap_or_default();
        lines.push(format!(
            "- **#{}**: {}{}{}",
            note.id, note.content, anchor, condition
        ));
    }
    if notes.len() == 25 {
        lines.push(String::new());
        lines.push(format!(
            "Showing {} notes (newest first). For older notes: ctx_note(action=\"read\", offset={}).",
            notes.len(),
            offset + notes.len()
        ));
    }
    lines.push(String::new());
    lines.push("To dismiss a stale note: ctx_note(action=\"dismiss\", note_id=N)".to_string());
    lines.join("\n")
}

fn single_memory_id(args: &Map<String, Value>, action: &str) -> Option<i64> {
    if let Some(id) = i64_arg(args, "id") {
        return Some(id);
    }
    let ids = memory_ids(args, action);
    (ids.len() == 1).then_some(ids[0])
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
    let mut deduped = Vec::new();
    for id in ids {
        if !deduped.contains(&id) {
            deduped.push(id);
        }
    }
    deduped
}

fn join_i64s(ids: &[i64]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn tagging_inactive_error() -> HandlerOutcome {
    HandlerOutcome::Error {
        code: "tagging_inactive".to_string(),
        message: "tagging not active for this session's profile".to_string(),
    }
}

fn reduce_unavailable_for_profile_error() -> HandlerOutcome {
    HandlerOutcome::Error {
        code: "reduce_unavailable_for_profile".to_string(),
        message:
            "ctx_reduce is unavailable because this session's profile cannot apply tail mutations"
                .to_string(),
    }
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
            let range_size = end - start + 1;
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
    raw.parse::<u64>()
        .map_err(|_| format!("Invalid integer: \"{raw}\""))
}

fn format_tag_numbers(ids: &[i64]) -> String {
    ids.iter()
        .map(|id| format!("§{id}§"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_ctx_reduce_response(
    inserted: usize,
    valid_requested: usize,
    queued_numbers: &[i64],
    unknown_numbers: &[i64],
) -> String {
    let already_queued = valid_requested.saturating_sub(inserted);
    let mut parts = Vec::new();
    if inserted > 0 {
        parts.push(format!(
            "Queued: drop {}.",
            format_tag_numbers(&queued_numbers[..inserted.min(queued_numbers.len())])
        ));
    } else if valid_requested > 0 {
        parts.push(
            "All requested tags were already queued or processed. No new action is needed."
                .to_string(),
        );
    } else {
        parts.push("No tags queued.".to_string());
    }
    if already_queued > 0 {
        parts.push(format!(
            "{already_queued} requested tag{} already queued and need no action.",
            if already_queued == 1 {
                " was"
            } else {
                "s were"
            }
        ));
    }
    if !unknown_numbers.is_empty() {
        parts.push(format!(
            "Unknown tag(s) {} skipped.",
            format_tag_numbers(unknown_numbers)
        ));
    }
    parts.join(" ")
}

fn suppress_channel1_after_ctx_reduce(store: &McStore, session_id: &str) {
    let Ok(loaded) = store.load(session_id) else {
        return;
    };
    let mut meta = loaded.meta.clone();
    meta.channel1_reduce_suppressed = true;
    meta.channel1_last_nudge_level = "urgent".to_string();
    let tag_tokens = store
        .load_tags_for_session(session_id)
        .map(|tags| tags.iter().map(|tag| tag.token_count.max(0)).sum::<i64>())
        .unwrap_or(0);
    meta.channel1_last_nudge_undropped = meta.channel1_last_nudge_undropped.max(tag_tokens);
    let _ = store.commit(session_id, loaded.row_version, &loaded.core, &meta);
}

fn drop_ids_from_command(request: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for key in ["drop_ids", "agent_drop_ids", "ids"] {
        if let Some(values) = request.get(key).and_then(Value::as_array) {
            ids.extend(
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(ToOwned::to_owned),
            );
        }
    }
    ids.sort();
    ids.dedup();
    ids
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
    let decision_mismatch = ts_decision
        .get("class")
        .and_then(Value::as_str)
        .is_some_and(|class| {
            class
                != rs_decision
                    .get("class")
                    .and_then(Value::as_str)
                    .unwrap_or("")
        });

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
) {
    let Ok(loaded) = store.load(session_id) else {
        return;
    };
    let mut meta = loaded.meta.clone();
    if meta.historian.state == HistorianPhase::Idle {
        if meta.historian.last_failure.as_deref() == Some(detail) {
            return;
        }
        meta.historian.last_failure = Some(detail.to_string());
    } else {
        meta.historian = historian::abandon_with_detail(
            &meta.historian,
            failure_backoff_at_ms,
            Some(detail.to_string()),
        );
    }
    let _ = store.commit(session_id, loaded.row_version, &loaded.core, &meta);
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
                "enum": ["write", "update", "archive", "merge"],
                "description": "Operation to perform."
            },
            "category": {
                "type": "string",
                "description": "Memory category for a new memory, such as ARCHITECTURE, CONSTRAINTS, DECISIONS, PREFERENCES, or WORKFLOW. Required for write."
            },
            "content": {
                "type": "string",
                "description": "Standalone memory text. Required for write, update, and merge."
            },
            "id": {
                "type": "integer",
                "minimum": 1,
                "description": "Single memory id for update or archive."
            },
            "ids": {
                "type": "array",
                "items": { "type": "integer", "minimum": 1 },
                "description": "Memory ids. For update provide exactly one. For archive provide one or more. For merge, the first id is kept and updated, and the remaining ids are superseded."
            },
            "target_id": {
                "type": "integer",
                "minimum": 1,
                "description": "Merge form: memory id to keep and update."
            },
            "source_ids": {
                "type": "array",
                "items": { "type": "integer", "minimum": 1 },
                "description": "Merge form: memory ids to supersede into target_id."
            },
            "reason": {
                "type": "string",
                "description": "Optional short reason for archive."
            }
        },
        "required": ["action"]
    })
}

fn ctx_search_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {
                "type": "string",
                "description": "Literal keyword or phrase to find in memories and summarized history."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 25,
                "default": 8,
                "description": "Maximum number of matches to return."
            }
        },
        "required": ["query"]
    })
}

fn ctx_expand_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "start": { "type": "integer", "minimum": 1, "description": "First message ordinal to expand." },
            "end": { "type": "integer", "minimum": 1, "description": "Last message ordinal to expand, inclusive." },
            "message": { "type": "integer", "minimum": 1, "description": "Recover the single persisted chunk transcript covering this message ordinal." }
        }
    })
}

fn ctx_note_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": { "type": "string", "enum": ["write", "read", "update", "dismiss"], "description": "Operation to perform. Defaults to write when content is provided, otherwise read." },
            "content": { "type": "string", "description": "Note text for write/update, or optional dismissal resolution when action is dismiss." },
            "note_id": { "type": "integer", "minimum": 1, "description": "Note id for update or dismiss." },
            "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 25, "description": "Maximum active notes to return." },
            "offset": { "type": "integer", "minimum": 0, "default": 0, "description": "Skip this many newest notes." },
            "surface_condition": { "type": "string", "description": "Optional externally checkable condition to record with the note. Evaluation arrives later." }
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
                        "Queue tagged context items for deferred discard on the next cache-busting pass".to_string(),
                    ),
                    execution_mode: ExecutionMode::Pure,
                    schema: json!({ "type": "object" }),
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
            of: vec!["ai-proxy".to_string()],
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
        CkIngressMessage, CkKind, CkWireBlock, CkWireMessage, HarnessMeta, ProviderExtras,
    };
    use historian_producer::{ProducerOutput, RunHandle, RunState};
    use mc_core::CoreState;
    use mc_store::{
        HistorianChunkRange, HistorianDurableState, ModuleMeta, ModuleUsage, StoredCompartment,
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
        assert_eq!(MEMORY_RENDER_FORMAT_EPOCH, 1);
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
                of: vec!["ai-proxy".to_string()]
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
        SessionBinding {
            project_root: PathBuf::from(root),
            harness: "mc-module-test".to_string(),
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
                        // consumer (e.g. ai-proxy's plan_outcome harness) can consume the
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

    #[derive(Default)]
    struct ProducerState {
        connects: AtomicUsize,
        starts: AtomicUsize,
        binds: AtomicUsize,
        statuses: AtomicUsize,
        await_outputs: AtomicUsize,
        block_output: std::sync::atomic::AtomicBool,
        notify: Notify,
        connect_errors: Mutex<VecDeque<HistorianProducerError>>,
        await_results: Mutex<VecDeque<Result<ProducerOutput, HistorianProducerError>>>,
        outputs: Mutex<VecDeque<String>>,
        prompts: Mutex<Vec<String>>,
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
            self.state
                .outputs
                .lock()
                .expect("outputs mutex")
                .push_back(historian_output_for_prompt(prompt));
            Ok(RunHandle {
                run_id: format!("run-{n}"),
            })
        }

        async fn await_output(
            &mut self,
            _run_id: &str,
        ) -> Result<ProducerOutput, HistorianProducerError> {
            self.state.await_outputs.fetch_add(1, Ordering::SeqCst);
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
        assert_eq!(status["epochs"]["memory_render_epoch"], json!(1));
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
            json!({ "kind": "guidance.get", "session_id": "ses" }),
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
            json!({ "kind": "guidance.get", "session_id": "ses" }),
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
            json!({ "kind": "guidance.get", "session_id": "ses" }),
        )
        .await;
        assert_eq!(repeated["bytes"], first["bytes"]);
        assert_eq!(repeated["hash"], first["hash"]);

        let still_frozen = call_dispatch_request(
            &handler,
            json!({ "kind": "guidance.get", "session_id": "ses" }),
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
            json!({ "kind": "guidance.get", "session_id": "ses" }),
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
            .dispatch_value(8, json!({ "kind": "guidance.get", "session_id": "other" }))
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
        assert!(write.contains("Surface condition recorded"));
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

        handler.bind_route(7, binding("/repo", ""));
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

        handler.bind_route(7, binding("/repo", "missing-map"));
        let none = call_facade(
            &handler,
            "ctx_search",
            json!({ "query": "anything", "limit": 1 }),
        )
        .await;
        assert_eq!(error_code(none), "session_unresolved");

        handler.bind_route(7, binding("/repo", "slow-map"));
        let timeout = call_facade(
            &handler,
            "ctx_search",
            json!({ "query": "anything", "limit": 1 }),
        )
        .await;
        assert_eq!(error_code(timeout), "session_resolve_timeout");
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

        let reduce = call_facade(&handler, "ctx_reduce", json!({ "drop": "1-3" })).await;
        assert_eq!(error_code(reduce), "tagging_inactive");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_ctx_reduce_parses_ranges_resolves_tags_and_dedups_queue() {
        crate::healing::set_tagging_enabled_for_tests(Some(true));
        let producer = Arc::new(ProducerState::default());
        let resolver = FakeSessionResolver::with(&[("token", FakeResolve::Hit("ses".to_string()))]);
        let (handler, store, _dir, project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(8, binding(project.to_str().unwrap(), "token"));

        let messages = vec![ck("m1", 1, "one"), ck("m2", 2, "two")];
        let transition = call_transform(&handler, messages.clone()).await;
        assert_eq!(transition["status"], "ok");
        assert!(store.load_tags_for_session("ses").unwrap().is_empty());
        let transformed = call_transform(&handler, messages).await;
        assert_eq!(transformed["status"], "ok");
        assert_eq!(store.load_tags_for_session("ses").unwrap().len(), 2);

        let mixed = tool_text(
            call_facade_on_channel(&handler, 8, "ctx_reduce", json!({ "drop": "§1§,99" })).await,
        );
        assert!(mixed.contains("Queued: drop §1§."), "{mixed}");
        assert!(mixed.contains("Unknown tag(s) §99§ skipped."), "{mixed}");
        let queued = store.load_pending_agent_drops("ses").unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].target_id, "m1#0");

        let duplicate = tool_text(
            call_facade_on_channel(&handler, 8, "ctx_reduce", json!({ "drop": "1" })).await,
        );
        assert!(duplicate.contains("already queued"), "{duplicate}");
        assert_eq!(store.load_pending_agent_drops("ses").unwrap().len(), 1);

        let malformed = tool_body(
            call_facade_on_channel(&handler, 8, "ctx_reduce", json!({ "drop": "3-1" })).await,
        );
        assert_eq!(malformed["isError"], json!(true));
        assert!(malformed["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Invalid range syntax"));
        crate::healing::set_tagging_enabled_for_tests(None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn facade_ctx_reduce_rejects_profiles_that_cannot_drain_tail_mutations() {
        crate::healing::set_tagging_enabled_for_tests(Some(true));
        let producer = Arc::new(ProducerState::default());
        let resolver = FakeSessionResolver::with(&[("token", FakeResolve::Hit("ses".to_string()))]);
        let (handler, store, _dir, project) =
            handler_with_store_and_resolver(producer, default_test_config(), resolver);
        handler.bind_route(8, binding(project.to_str().unwrap(), "token"));

        let messages = vec![ck("m1", 1, "one")];
        let mut cc_request = request(messages.clone());
        cc_request["serializer_profile"] = json!("claude-code-anthropic");
        let first = call_transform_request(&handler, cc_request.clone()).await;
        assert_eq!(first["status"], "ok");
        let second = call_transform_request(&handler, cc_request).await;
        assert_eq!(second["status"], "ok");
        assert_eq!(store.load_tags_for_session("ses").unwrap().len(), 1);

        let rejected =
            call_facade_on_channel(&handler, 8, "ctx_reduce", json!({ "drop": "1" })).await;
        assert_eq!(error_code(rejected), "reduce_unavailable_for_profile");
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());
        crate::healing::set_tagging_enabled_for_tests(None);
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

        assert!(!tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "action": "update", "id": shared_update, "content": "shared edited" }),
            )
            .await
        ));
        assert!(!tool_is_error(
            call_facade(
                &handler,
                "ctx_memory",
                json!({ "action": "archive", "ids": [shared_archive] }),
            )
            .await
        ));
        assert!(!tool_is_error(
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
            Some(shared_target)
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

    fn queue_drop_command(handler: &McHandler, target_id: &str) -> Value {
        match handler.handle_agent_drops_value(
            7,
            json!({
                "kind": "ctx_reduce",
                "session_id": "ses",
                "drop_ids": [target_id],
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
    async fn ctx_reduce_command_appends_and_transform_drains_queue() {
        let producer = Arc::new(ProducerState::default());
        let (handler, store, _dir, _project) = handler_with_store(producer, default_test_config());
        let queued = queue_drop_command(&handler, "a#0");
        assert_eq!(queued["queued"].as_u64(), Some(1));
        assert_eq!(store.load_pending_agent_drops("ses").unwrap().len(), 1);

        let response = call_transform(&handler, vec![ck("a", 1, "drop me")]).await;
        assert!(serde_json::to_string(&response)
            .unwrap()
            .contains("[dropped]"));
        assert!(store.load_pending_agent_drops("ses").unwrap().is_empty());
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
                project_directory: &baseline_project_path,
                history_budget_tokens: memory_render::DEFAULT_HISTORY_BUDGET_TOKENS,
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
    async fn handler_connect_failure_records_durable_detail_without_backoff_and_later_fire_clears_it(
    ) {
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
        assert_eq!(state.failure_backoff_at_ms, None);
        assert!(
            state
                .last_failure
                .as_deref()
                .is_some_and(|detail| detail.contains("producer connect")),
            "pre-fire connect failures must land in durable state, not only stderr"
        );
        assert_eq!(producer.starts.load(Ordering::SeqCst), 0);

        let second = call_transform(&handler, messages).await;
        assert_eq!(second["historian"]["fired"], true);
        wait_for_count(&producer.starts, 1).await;
        wait_for_idle(&store).await;
        let state = store.load("ses").unwrap().meta.historian;
        assert_eq!(state.last_failure, None);
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
                project_directory: &project_path_string,
                history_budget_tokens: memory_render::DEFAULT_HISTORY_BUDGET_TOKENS,
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
        seed_boundary_id: Option<String>,
        compartments: Vec<StrictShadowCompartment>,
        memories: Vec<StrictShadowMemory>,
        memory_mutations: Vec<StrictShadowMemoryMutation>,
        workspace: Option<StrictShadowWorkspace>,
        last_todo_state: String,
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
            "cache_ttl": "5m"
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
            },
            |_| 0,
        )
        .unwrap();
        assert!(composed.m0_bytes.contains(
            "start=\"0\" end=\"0\" start-date=\"2026-01-02\" end-date=\"2026-01-03\" title=\"c0\""
        ));
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
