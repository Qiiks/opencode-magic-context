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

pub mod compartment_coverage;
pub mod decay_render;
pub mod m0_compose;
pub mod memory_render;
pub mod project_docs;
pub mod transform;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use cortexkit_store_types::{sqlite_store_path, Isolation, StorageBackend, StorageDescriptor};
use mc_store::McStore;
use serde_json::{json, Value};
use subc_client_rs::{async_trait, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest};

use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ModuleManifest,
        ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
    },
    ModuleHelloAckBody, PROTOCOL_VERSION,
};
use transform::{transform, DeciderInputs, TransformRequest};

/// The per-route session binding: the project + session a route channel is bound to.
/// Established once at `on_bind` (the daemon relays the resolved {project_root, session}
/// for the route), read by the transform path to resolve which project's store to read,
/// and removed at `on_route_gone`. The project is NEVER taken from a per-pass request
/// field — a crafted request could spoof it to read another project's memories — so it
/// lives here, keyed by the route channel the daemon controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBinding {
    pub project_root: PathBuf,
    pub session: String,
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

/// The module handler. Holds the single store handle (opened once in `on_hello_ack`)
/// and the per-route session bindings (channel → {project, session}).
pub struct McHandler {
    store: OnceLock<McStore>,
    /// Route channel → its session binding. Populated at `on_bind`, removed at
    /// `on_route_gone`. A `Mutex<HashMap>` (not a lock-free map) because writes are
    /// rare (once per route open/close) and reads are one cheap lookup per transform.
    bindings: Mutex<HashMap<u16, SessionBinding>>,
}

impl McHandler {
    pub fn new() -> Self {
        McHandler {
            store: OnceLock::new(),
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

    /// Resolve the project for a transform request on `channel`, FAIL-LOUD: the channel
    /// must be bound AND its bound session must match the request's `session_id`. Returns
    /// the project_root the caller keys its store reads off, never a default. The returned
    /// root is not yet consumed (the Hard/Soft transform arms will read the store under it);
    /// but the resolve-or-reject is enforced from the start, and it changes no transform
    /// output — a correctly-bound request resolves and proceeds identically.
    fn resolve_project(
        &self,
        channel: u16,
        request_session: &str,
    ) -> Result<PathBuf, BindingError> {
        let map = self.bindings.lock().expect("bindings mutex");
        let binding = map.get(&channel).ok_or(BindingError::Unbound)?;
        if binding.session != request_session {
            return Err(BindingError::SessionMismatch);
        }
        Ok(binding.project_root.clone())
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
                let _ = self.store.set(store);
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
            Some("transform") => {
                let store = match self.store.get() {
                    Some(store) => store,
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
                // Resolve the project from the route channel the request arrived on
                // (ctx.channel(), the identity subc authenticated at route.bind), NEVER
                // the session_id in the request body (caller-claimed, spoofable). Fail
                // loud two ways: Unbound (no on_bind for this channel, or it was torn
                // down) and SessionMismatch (the body's session_id != the session this
                // channel was bound for — a request can't drive a session its route
                // wasn't bound for). Both reject before any transform work. The resolved
                // project_root is not consumed yet (the Hard/Soft arms will read the
                // store under it); resolving + rejecting here produces identical output
                // for a correctly-bound request, so it changes no cached bytes.
                let _project_root = match self.resolve_project(ctx.channel(), &parsed.session_id) {
                    Ok(root) => root,
                    Err(BindingError::Unbound) => {
                        return HandlerOutcome::Error {
                            code: "route_unbound".to_string(),
                            message: "transform on a channel with no session binding".to_string(),
                        }
                    }
                    Err(BindingError::SessionMismatch) => {
                        return HandlerOutcome::Error {
                            code: "session_mismatch".to_string(),
                            message:
                                "request session_id does not match the channel's bound session"
                                    .to_string(),
                        }
                    }
                };
                // The `_decider` wire field is test-only scaffolding (production builds
                // the decision inputs internally, not from the request). Absent →
                // all-default → the pure production path.
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
                match transform(store, &parsed, &deciders) {
                    Ok(response) => respond(serde_json::to_value(response).unwrap_or(Value::Null)),
                    Err(e) => HandlerOutcome::Error {
                        code: "transform_failed".to_string(),
                        message: e.to_string(),
                    },
                }
            }
            // Default: echo (proves the wire round-trips).
            _ => respond(json!({ "ok": true, "echo": request })),
        }
    }
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
    StorageDescriptor {
        module_id: DEFAULT_MODULE_ID.to_string(),
        storage_namespace: STORAGE_NAMESPACE.to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: sqlite_store_path(&data_home, DEFAULT_MODULE_ID),
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

    #[test]
    fn route_binding_bind_resolve_unbind() {
        let h = McHandler::new();
        let binding = SessionBinding {
            project_root: PathBuf::from("/repo/proj"),
            session: "ses_a".to_string(),
        };
        h.bind_route(7, binding);

        // resolve succeeds when the channel is bound AND the session matches
        assert_eq!(
            h.resolve_project(7, "ses_a").unwrap(),
            PathBuf::from("/repo/proj")
        );

        // a teardown removes the binding → a later resolve fails loud (no stale project)
        h.unbind_route(7);
        assert_eq!(h.resolve_project(7, "ses_a"), Err(BindingError::Unbound));
    }

    #[test]
    fn resolve_fails_loud_unbound_and_on_session_mismatch() {
        let h = McHandler::new();
        // never bound → Unbound (NEVER a default project, which would be a cross-project read)
        assert_eq!(h.resolve_project(3, "ses_x"), Err(BindingError::Unbound));

        h.bind_route(
            3,
            SessionBinding {
                project_root: PathBuf::from("/repo/own"),
                session: "ses_own".to_string(),
            },
        );
        // bound, but a request claiming a DIFFERENT session on this channel → SessionMismatch
        assert_eq!(
            h.resolve_project(3, "ses_other"),
            Err(BindingError::SessionMismatch)
        );
        // the matching session still resolves
        assert_eq!(
            h.resolve_project(3, "ses_own").unwrap(),
            PathBuf::from("/repo/own")
        );
    }

    #[test]
    fn rebind_overwrites_stale_channel_entry() {
        let h = McHandler::new();
        h.bind_route(
            5,
            SessionBinding {
                project_root: PathBuf::from("/a"),
                session: "s1".into(),
            },
        );
        // a reused channel re-binds to a new session → last write wins (no stale leak)
        h.bind_route(
            5,
            SessionBinding {
                project_root: PathBuf::from("/b"),
                session: "s2".into(),
            },
        );
        assert_eq!(h.resolve_project(5, "s2").unwrap(), PathBuf::from("/b"));
        assert_eq!(
            h.resolve_project(5, "s1"),
            Err(BindingError::SessionMismatch)
        );
    }
}
