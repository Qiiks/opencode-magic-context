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
pub mod transform;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;

use cortexkit_store_types::{sqlite_store_path, Isolation, StorageBackend, StorageDescriptor};
use mc_store::{HistorianPhase, McStore};
use serde_json::{json, Value};
use subc_client_rs::{async_trait, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest};

use boundary::{BoundaryBlock, BoundaryContext, BoundaryMsg, Role, TriggerContext};
use config::{ConfigCache, McModuleConfig};
use healing::SerializerProfile;
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
#[cfg(test)]
use transform::ReductionDecision;
use transform::{transform_with_projection, HistorianDiagnostics, TransformRequest};

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
/// After a historian abandon, suppress refires for this long so a persistently
/// failing model does not burn a full summarization pass on every transform.
const HISTORIAN_FAILURE_BACKOFF_MS: i64 = 60_000;

/// The module handler. Holds the single store handle (opened once in `on_hello_ack`)
/// and the per-route session bindings (channel → {project, session}).
pub struct McHandler {
    store: OnceLock<Arc<McStore>>,
    producer_factory: Arc<dyn HistorianProducerFactory>,
    config: Mutex<ConfigCache>,
    #[cfg(test)]
    fixed_config: Option<McModuleConfig>,
    reattaching_sessions: Arc<Mutex<HashSet<String>>>,
    live_historian_sessions: Arc<Mutex<HashMap<String, LiveHistorianSession>>>,
    scheduler_observations: Mutex<HashMap<String, SchedulerObservation>>,
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
        McHandler {
            store: OnceLock::new(),
            producer_factory,
            config: Mutex::new(ConfigCache::default()),
            #[cfg(test)]
            fixed_config: None,
            reattaching_sessions: Arc::new(Mutex::new(HashSet::new())),
            live_historian_sessions: Arc::new(Mutex::new(HashMap::new())),
            scheduler_observations: Mutex::new(HashMap::new()),
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
        McHandler {
            store: OnceLock::new(),
            producer_factory: factory,
            config: Mutex::new(ConfigCache::default()),
            fixed_config: Some(config),
            reattaching_sessions: Arc::new(Mutex::new(HashSet::new())),
            live_historian_sessions: Arc::new(Mutex::new(HashMap::new())),
            scheduler_observations: Mutex::new(HashMap::new()),
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
        if let Err(e) = self.resolve_binding(channel, session_id) {
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
        let project_path = binding.project_root.to_string_lossy().to_string();
        let pass_now = now_ms();
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
        let mut result = match run_transform() {
            Ok(result) => result,
            Err(e) => {
                return HandlerOutcome::Error {
                    code: "transform_failed".to_string(),
                    message: e.to_string(),
                }
            }
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
                            Err(e) => {
                                return HandlerOutcome::Error {
                                    code: "transform_failed".to_string(),
                                    message: e.to_string(),
                                }
                            }
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
                                            Err(e) => {
                                                return HandlerOutcome::Error {
                                                    code: "transform_failed".to_string(),
                                                    message: e.to_string(),
                                                }
                                            }
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
                                Err(e) => {
                                    return HandlerOutcome::Error {
                                        code: "transform_failed".to_string(),
                                        message: e.to_string(),
                                    }
                                }
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
                    Err(e) => {
                        return HandlerOutcome::Error {
                            code: "transform_failed".to_string(),
                            message: e.to_string(),
                        }
                    }
                };
            }
        }
        let mut response = result.response;
        response.historian = Some(diagnostics);
        self.record_response_observation(&parsed.session_id, now_ms());
        respond(serde_json::to_value(response).unwrap_or(Value::Null))
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
        let config = self.effective_config(&req.identity.project_root);
        self.bind_route(
            req.route_channel,
            SessionBinding {
                project_root: req.identity.project_root.clone(),
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
        match method {
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
            Some("transform") => self.handle_transform_value(channel, request).await,
            Some("ctx_reduce" | "append_agent_drops" | "agent_drops.append") => {
                self.handle_agent_drops_value(channel, request)
            }
            // Explicit wire-debugging echo. Opt-in only: echoing every
            // unrecognized body would silently swallow misrouted requests
            // (a caller can "succeed" against an echo while testing nothing),
            // so unknown shapes fail loud below instead.
            Some("echo") => respond(json!({ "ok": true, "echo": request })),
            _ => unrecognized_request_error(&request),
        }
    }
}

/// Classify a request that matched no known `method`/`kind`. Two distinct
/// errors so a misroute is diagnosable from the error code alone:
/// - `{name, arguments}` without `method`/`kind` is the shape of an MCP
///   tools/call envelope. This module does not accept MCP-routed tool calls
///   yet (they need a mapping from the MCP shim's ephemeral session to the
///   durable wire session id before commands like ctx_reduce can target the
///   right queue), but the shape is recognized so a misconfigured MCP router
///   fails with an exact cause instead of a generic shape error.
/// - Anything else names the discriminator fields we looked for and the
///   top-level keys we actually got.
fn unrecognized_request_error(request: &Value) -> HandlerOutcome {
    let has_mcp_shape = request.get("name").is_some() && request.get("arguments").is_some();
    if has_mcp_shape {
        return HandlerOutcome::Error {
            code: "facade_envelope_not_supported".to_string(),
            message: "MCP tools/call envelope ({name, arguments}) is not routable to this \
                      module yet; MC commands use flat bodies with a top-level `kind` field"
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
            ],
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

    use crate::ck_wire::{
        CkIngressMessage, CkKind, CkWireBlock, CkWireMessage, HarnessMeta, ProviderExtras,
    };
    use historian_producer::{ProducerOutput, RunHandle, RunState};
    use mc_store::{HistorianChunkRange, HistorianDurableState, ModuleUsage, StoredCompartment};
    use tokio::sync::Notify;

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
            model_key: None,
            config: default_test_config(),
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

    fn error_code(outcome: HandlerOutcome) -> String {
        match outcome {
            HandlerOutcome::Error { code, .. } => code,
            other => panic!("expected error outcome, got {other:?}"),
        }
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
                json!({ "name": "ctx_reduce", "arguments": { "drop": "1-3" } }),
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
        let session = historian::historian_producer_session_id("proj", 3);
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
                injected_reductions: Vec::new(),
            },
        )
        .unwrap();
        let without_value = serde_json::to_value(response_without_historian).unwrap();
        assert_eq!(with_historian["ck_messages"], without_value["ck_messages"]);
    }
}
