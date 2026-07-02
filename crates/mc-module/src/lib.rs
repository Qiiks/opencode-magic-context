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
pub mod compartment_coverage;
pub mod config;
pub mod decay_render;
pub mod historian;
pub mod historian_chunk;
pub mod historian_producer;
pub mod historian_prompt;
pub mod historian_validate;
pub mod injection;
pub mod m0_compose;
pub mod m1_compose;
pub mod memory_render;
pub mod project_docs;
pub mod scheduler;
pub mod selection;
pub mod transform;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use cortexkit_store_types::{sqlite_store_path, Isolation, StorageBackend, StorageDescriptor};
use mc_store::{HistorianPhase, McStore};
use serde_json::{json, Value};
use subc_client_rs::{async_trait, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest};

use boundary::{BoundaryBlock, BoundaryContext, BoundaryMsg, Role, TriggerContext};
use config::{ConfigCache, McModuleConfig};
use historian::{reattach_historian_producer, run_historian_firing, HistorianProducerDriver};
use historian_chunk::{
    assemble_historian_firing, AssembleHistorianFiringOutcome, AssembledHistorianFiring,
    HistorianAssemblerConfig,
};
use historian_producer::{HistorianProducer, HistorianProducerConfig, HistorianProducerError};
use selection::SelKind;
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ModuleManifest,
        ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
    },
    ModuleHelloAckBody, PROTOCOL_VERSION,
};
use transform::{transform_with_projection, DeciderInputs, HistorianDiagnostics, TransformRequest};

/// The per-route session binding: the project + session a route channel is bound to, plus
/// the render budget frozen at bind. Established once at `on_bind` (the daemon relays the
/// resolved {project_root, session} for the route), read by the transform path to resolve
/// which project's store to read, and removed at `on_route_gone`. The project is NEVER
/// taken from a per-pass request field — a crafted request could spoof it to read another
/// project's memories — so it lives here, keyed by the route channel the daemon controls.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionBinding {
    pub project_root: PathBuf,
    pub session: String,
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

/// Storage namespace for the cache-state domain.
const STORAGE_NAMESPACE: &str = "mc_cache";
/// Mirrors packages/plugin/src/config/schema/magic-context.ts commit_cluster_trigger.enabled default.
const DEFAULT_COMMIT_CLUSTER_TRIGGER_ENABLED: bool = true;
/// Mirrors packages/plugin/src/config/schema/magic-context.ts commit_cluster_trigger.min_clusters default.
const DEFAULT_MIN_COMMIT_CLUSTERS: usize = 3;
/// Mirrors packages/plugin/src/hooks/magic-context/derive-budgets.ts with the default
/// 128K historian context fallback: clamp(128_000 × 0.25, 8_000, 50_000) = 32_000.
const DEFAULT_HISTORIAN_CHUNK_TOKENS: usize = 32_000;
/// Secondary assembler guard; TS trigger sizing is authoritative, this only rejects tiny chunks.
const DEFAULT_HISTORIAN_MIN_CHUNK_TOKENS: usize = 512;

/// The module handler. Holds the single store handle (opened once in `on_hello_ack`)
/// and the per-route session bindings (channel → {project, session}).
pub struct McHandler {
    store: OnceLock<Arc<McStore>>,
    producer_factory: Arc<dyn HistorianProducerFactory>,
    config: Mutex<ConfigCache>,
    #[cfg(test)]
    fixed_config: Option<McModuleConfig>,
    reattaching_sessions: Arc<Mutex<HashSet<String>>>,
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
        McHandler {
            store: OnceLock::new(),
            producer_factory,
            config: Mutex::new(ConfigCache::default()),
            #[cfg(test)]
            fixed_config: None,
            reattaching_sessions: Arc::new(Mutex::new(HashSet::new())),
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
            },
        )
    }

    #[cfg(test)]
    fn with_producer_factory_and_config(
        factory: Arc<dyn HistorianProducerFactory>,
        config: McModuleConfig,
    ) -> Self {
        McHandler {
            store: OnceLock::new(),
            producer_factory: factory,
            config: Mutex::new(ConfigCache::default()),
            fixed_config: Some(config),
            reattaching_sessions: Arc::new(Mutex::new(HashSet::new())),
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

    fn maybe_spawn_reattach(
        &self,
        store: Arc<McStore>,
        parsed: &TransformRequest,
        project_path: String,
        projection: &transform::ck_wire::FlatProjection,
        now: i64,
    ) {
        let Ok(loaded) = store.load(&parsed.session_id) else {
            return;
        };
        if loaded.meta.historian.state != HistorianPhase::AwaitingProducer {
            return;
        }
        let mut latch = self.reattaching_sessions.lock().expect("reattach mutex");
        if !latch.insert(parsed.session_id.clone()) {
            return;
        }
        drop(latch);

        let session_id = parsed.session_id.clone();
        let factory = Arc::clone(&self.producer_factory);
        let latch = Arc::clone(&self.reattaching_sessions);
        let project_root = PathBuf::from(&project_path);
        let live: Vec<_> = projection
            .blocks
            .iter()
            .filter(|b| !b.synthetic)
            .cloned()
            .collect();
        let Some(range) = loaded.meta.historian.chunk_range.clone() else {
            self.reattaching_sessions
                .lock()
                .expect("reattach mutex")
                .remove(&session_id);
            return;
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
        let fingerprint_items: Vec<_> = chunk.snapshot.iter().map(|item| item.as_item()).collect();
        let observed = historian::compute_chunk_fingerprint(&fingerprint_items);
        tokio::spawn(async move {
            let result = async {
                let mut producer = factory.connect(&project_root).await?;
                reattach_historian_producer(
                    &mut *producer,
                    historian::HistorianReattachRequest {
                        store: &store,
                        session_id: &session_id,
                        project_path: &project_path,
                        observed_chunk_fingerprint: &observed,
                        validation_chunk: &chunk.chunk,
                        prior_compartments: &prior_compartments,
                        validate_options: historian_validate::ValidateOptions {
                            sequence_offset: prior_compartments.len() as u64 + 1,
                            in_emergency: false,
                        },
                        publication_floor_ordinal: range.to_ordinal,
                        now_ms: now,
                        failure_backoff_at_ms: now + 60_000,
                    },
                )
                .await
            }
            .await;
            if let Err(e) = result {
                eprintln!("mc-module: historian reattach failed for {session_id}: {e}");
            }
            latch.lock().expect("reattach mutex").remove(&session_id);
        });
    }

    fn maybe_spawn_historian_fire(
        &self,
        store: Arc<McStore>,
        parsed: &TransformRequest,
        binding: &SessionBinding,
        project_path: String,
        projection: &transform::ck_wire::FlatProjection,
        now: i64,
    ) -> HistorianDiagnostics {
        let loaded = match store.load(&parsed.session_id) {
            Ok(loaded) => loaded,
            Err(e) => {
                return HistorianDiagnostics {
                    fired: false,
                    reason: None,
                    no_fire: Some(format!("state_load_failed:{e}")),
                    state: "unknown".to_string(),
                }
            }
        };
        let state = loaded.meta.historian.state.as_str().to_string();
        let cfg = self.effective_config(&binding.project_root);
        let boundary_messages = boundary_messages(parsed, projection);
        let last_compartment_end_ordinal = store
            .load_compartments(&parsed.session_id)
            .map(|cs| cs.iter().map(|c| c.end_message as u64).max().unwrap_or(0))
            .unwrap_or(0);
        let (context_limit, input_tokens, usage_percentage) = usage_numbers(parsed.usage.as_ref());
        let trigger = boundary::check_compartment_trigger(
            &boundary_messages,
            &TriggerContext {
                boundary: BoundaryContext {
                    context_limit,
                    execute_threshold_percentage: cfg.execute_threshold_percentage,
                    usage_percentage,
                    usage_input_tokens: input_tokens,
                    last_compartment_end_ordinal,
                    prior_boundary_ordinal: last_compartment_end_ordinal,
                    migration_floor_active: last_compartment_end_ordinal > 0,
                    emergency_tail_scale: None,
                    trigger_budget: None,
                },
                projected_post_drop_percentage: None,
                compartment_in_progress: loaded.meta.historian.state != HistorianPhase::Idle,
                commit_cluster_trigger_enabled: DEFAULT_COMMIT_CLUSTER_TRIGGER_ENABLED,
                min_commit_clusters: DEFAULT_MIN_COMMIT_CLUSTERS,
            },
        );
        if !trigger.fire {
            return HistorianDiagnostics {
                fired: false,
                reason: None,
                no_fire: Some(if loaded.meta.historian.state == HistorianPhase::Idle {
                    "trigger_false".to_string()
                } else {
                    "busy".to_string()
                }),
                state,
            };
        }
        if cfg.model_chain.is_empty() {
            return HistorianDiagnostics {
                fired: false,
                reason: trigger.reason.map(|r| r.as_str().to_string()),
                no_fire: Some("no_models".to_string()),
                state,
            };
        }
        let Some(boundary) = trigger.boundary.clone() else {
            return HistorianDiagnostics {
                fired: false,
                reason: None,
                no_fire: Some("missing_boundary".to_string()),
                state,
            };
        };
        let live: Vec<_> = projection
            .blocks
            .iter()
            .filter(|block| !block.synthetic)
            .cloned()
            .collect();
        let project_slug = project_slug(&binding.project_root);
        let assemble = assemble_historian_firing(
            &store,
            &parsed.messages,
            &live,
            HistorianAssemblerConfig {
                session_id: parsed.session_id.clone(),
                project_path: project_path.clone(),
                project_slug: project_slug.clone(),
                model_chain: cfg.model_chain.clone(),
                token_budget: DEFAULT_HISTORIAN_CHUNK_TOKENS,
                boundary,
                memory_enabled: cfg.memory_enabled,
                extraction_free: false,
                in_emergency: usage_percentage >= 95.0,
                failure_backoff_at_ms: now + 60_000,
                min_chunk_tokens: DEFAULT_HISTORIAN_MIN_CHUNK_TOKENS,
            },
            now,
        );
        let firing = match assemble {
            Ok(AssembleHistorianFiringOutcome::Fire(firing)) => *firing,
            Ok(AssembleHistorianFiringOutcome::NoFire(reason)) => {
                return HistorianDiagnostics {
                    fired: false,
                    reason: trigger.reason.map(|r| r.as_str().to_string()),
                    no_fire: Some(format!("assemble:{reason:?}")),
                    state,
                }
            }
            Err(e) => {
                return HistorianDiagnostics {
                    fired: false,
                    reason: trigger.reason.map(|r| r.as_str().to_string()),
                    no_fire: Some(format!("assemble_failed:{e}")),
                    state,
                }
            }
        };
        self.spawn_historian_firing(
            store,
            parsed.session_id.clone(),
            project_path,
            binding.project_root.clone(),
            project_slug,
            firing,
        );
        HistorianDiagnostics {
            fired: true,
            reason: trigger.reason.map(|r| r.as_str().to_string()),
            no_fire: None,
            state,
        }
    }

    fn spawn_historian_firing(
        &self,
        store: Arc<McStore>,
        session_id: String,
        project_path: String,
        project_root: PathBuf,
        project_slug: String,
        firing: AssembledHistorianFiring,
    ) {
        let factory = Arc::clone(&self.producer_factory);
        tokio::spawn(async move {
            let result = async {
                let mut producer = factory.connect(&project_root).await?;
                let request =
                    firing.as_fire_request(&store, &session_id, &project_path, &project_slug);
                run_historian_firing(&mut *producer, request).await
            }
            .await;
            match result {
                Ok(outcome) => {
                    eprintln!("mc-module: historian firing finished for {session_id}: {outcome:?}")
                }
                Err(e) => eprintln!("mc-module: historian firing failed for {session_id}: {e}"),
            }
        });
    }

    async fn handle_transform_value(&self, channel: u16, request: Value) -> HandlerOutcome {
        let store = match self.store.get() {
            Some(store) => Arc::clone(store),
            None => {
                return HandlerOutcome::Error {
                    code: "store_unavailable".to_string(),
                    message: "store not opened (no HELLO_ACK storage seam)".to_string(),
                }
            }
        };
        let parsed: TransformRequest = match serde_json::from_value(request.clone()) {
            Ok(req) => req,
            Err(e) => {
                return HandlerOutcome::Error {
                    code: "bad_request".to_string(),
                    message: e.to_string(),
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
        let deciders: DeciderInputs = match request.get("_decider") {
            Some(d) => match serde_json::from_value(d.clone()) {
                Ok(d) => d,
                Err(e) => {
                    return HandlerOutcome::Error {
                        code: "bad_request".to_string(),
                        message: format!("_decider: {e}"),
                    }
                }
            },
            None => DeciderInputs::default(),
        };
        let project_path = binding.project_root.to_string_lossy();
        let producer_ctx = transform::ProducerContext {
            project_path: &project_path,
            project_directory: &project_path,
            history_budget_tokens: binding.history_budget_tokens,
            now_ms: now_ms(),
        };
        let pass_now = producer_ctx.now_ms;
        match transform_with_projection(&store, &parsed, &producer_ctx, &deciders) {
            Ok(mut result) => {
                self.maybe_spawn_reattach(
                    Arc::clone(&store),
                    &parsed,
                    project_path.to_string(),
                    &result.projection,
                    pass_now,
                );
                let diagnostics = self.maybe_spawn_historian_fire(
                    Arc::clone(&store),
                    &parsed,
                    &binding,
                    project_path.to_string(),
                    &result.projection,
                    pass_now,
                );
                result.response.historian = Some(diagnostics);
                respond(serde_json::to_value(result.response).unwrap_or(Value::Null))
            }
            Err(e) => HandlerOutcome::Error {
                code: "transform_failed".to_string(),
                message: e.to_string(),
            },
        }
    }

    #[cfg(test)]
    async fn handle_transform_for_test(&self, channel: u16, request: Value) -> HandlerOutcome {
        self.handle_transform_value(channel, request).await
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
        self.bind_route(
            req.route_channel,
            SessionBinding {
                project_root: req.identity.project_root.clone(),
                session: req.identity.session.clone(),
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
        self.unbind_route(channel);
    }

    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let request = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
        match request.get("kind").and_then(Value::as_str) {
            // Proves the store opened end-to-end: load a sentinel session through the
            // real opened handle and report its presence/state.
            Some("health") => match self.store.get() {
                Some(store) => match store.load("__health__") {
                    Ok(state) => respond(json!({
                        "ok": true,
                        "store_open": true,
                        "initialized": state.meta.initialized,
                        "row_version": state.row_version,
                    })),
                    Err(e) => HandlerOutcome::Error {
                        code: "store_load_failed".to_string(),
                        message: e.to_string(),
                    },
                },
                None => HandlerOutcome::Error {
                    code: "store_unavailable".to_string(),
                    message: "store not opened (no HELLO_ACK storage seam yet)".to_string(),
                },
            },
            // The CK-in/CK-out cache-stability spine.
            Some("transform") => self.handle_transform_value(ctx.channel(), request).await,
            // Default: echo (proves the wire round-trips).
            _ => respond(json!({ "ok": true, "echo": request })),
        }
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

fn respond(value: Value) -> HandlerOutcome {
    match serde_json::to_vec(&value) {
        Ok(bytes) => HandlerOutcome::Response(bytes),
        Err(e) => HandlerOutcome::Error {
            code: "encode_failed".to_string(),
            message: e.to_string(),
        },
    }
}

fn boundary_messages(
    parsed: &TransformRequest,
    projection: &transform::ck_wire::FlatProjection,
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

fn sel_kind_for_flat(block: &transform::ck_wire::FlatBlock) -> SelKind {
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
        .filter(|limit| *limit > 0.0)
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

/// The module manifest registered at HELLO. Slice-1 declares the transform provider
/// role + a project-scoped sqlite storage binding; the surface widens with the spine.
pub fn manifest(module_id: &str) -> ModuleManifest {
    ModuleManifest {
        module_id: module_id.to_string(),
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
            tools: vec![Tool {
                name: "transform".to_string(),
                execution_mode: ExecutionMode::Pure,
                schema: json!({ "type": "object" }),
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency: Concurrency::ModuleManaged,
            emits_push: false,
            sub_supervises: false,
        }],
        consumes: Vec::new(),
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
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use historian_producer::{ProducerOutput, RunHandle, RunState};
    use mc_store::{HistorianChunkRange, HistorianDurableState, ModuleUsage};
    use tokio::sync::Notify;
    use transform::ck_wire::{
        CkIngressMessage, CkKind, CkWireBlock, CkWireMessage, HarnessMeta, ProviderExtras,
    };

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
    }

    fn binding(root: &str, session: &str) -> SessionBinding {
        SessionBinding {
            project_root: PathBuf::from(root),
            session: session.to_string(),
            history_budget_tokens: memory_render::DEFAULT_HISTORY_BUDGET_TOKENS,
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
        starts: AtomicUsize,
        binds: AtomicUsize,
        statuses: AtomicUsize,
        await_outputs: AtomicUsize,
        block_output: std::sync::atomic::AtomicBool,
        notify: Notify,
        outputs: Mutex<VecDeque<String>>,
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
            let text = self
                .state
                .outputs
                .lock()
                .expect("outputs mutex")
                .pop_front()
                .unwrap_or_else(|| historian_output(1, 3, "reattached summary"));
            Ok(ProducerOutput { text })
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
        let dir = tempfile::tempdir().unwrap();
        let data_home = dir.path().join("data");
        std::fs::create_dir_all(&data_home).unwrap();
        let store =
            Arc::new(McStore::open(&dev_descriptor_at(data_home.to_str().unwrap())).unwrap());
        let handler = McHandler::with_producer_factory_and_config(
            Arc::new(TestProducerFactory { state }),
            config,
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
        }
    }

    fn ck(mid: &str, ordinal: u64, text: &str) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "user",
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

    fn big_messages() -> Vec<CkIngressMessage> {
        (1..=80)
            .map(|n| {
                ck(
                    &format!("m{n}"),
                    n,
                    &format!("message {n} {}", "word ".repeat(800)),
                )
            })
            .collect()
    }

    fn request(messages: Vec<CkIngressMessage>) -> Value {
        json!({
            "kind": "transform",
            "session_id": "ses",
            "render_config": "cfg0",
            "usage": ModuleUsage { current_total_input_tokens: 45_000, context_limit_tokens: 50_000 },
            "messages": messages,
        })
    }

    async fn call_transform(handler: &McHandler, messages: Vec<CkIngressMessage>) -> Value {
        match handler
            .handle_transform_for_test(7, request(messages))
            .await
        {
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

    async fn wait_for_idle(store: &McStore) {
        for _ in 0..200 {
            if store.load("ses").unwrap().meta.historian.state == HistorianPhase::Idle {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("historian did not return to idle");
    }

    async fn wait_for_count(value: &AtomicUsize, expected: usize) {
        for _ in 0..200 {
            if value.load(Ordering::SeqCst) >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("counter did not reach {expected}");
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
        wait_for_idle(&store).await;
        let compartments = store.load_compartments("ses").unwrap();
        assert_eq!(compartments.len(), 1);
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
        assert_eq!(producer.starts.load(Ordering::SeqCst), 1);

        producer.block_output.store(false, Ordering::SeqCst);
        producer.notify.notify_waiters();
        wait_for_idle(&store).await;
    }

    fn seed_awaiting(store: &McStore, messages: &[CkIngressMessage]) {
        let live = transform::ck_wire::project_messages(messages)
            .unwrap()
            .blocks;
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
            failure_backoff_at_ms: None,
        };
        store
            .commit("ses", loaded.row_version, &loaded.core, &meta)
            .unwrap();
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
            },
            &DeciderInputs::default(),
        )
        .unwrap();
        let without_value = serde_json::to_value(response_without_historian).unwrap();
        assert_eq!(with_historian["ck_messages"], without_value["ck_messages"]);
    }
}
