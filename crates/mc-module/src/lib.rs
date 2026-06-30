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

pub mod decay_render;
pub mod project_docs;
pub mod transform;

use std::sync::OnceLock;

use cortexkit_store_types::{sqlite_store_path, Isolation, StorageBackend, StorageDescriptor};
use mc_store::McStore;
use serde_json::{json, Value};
use subc_client_rs::{async_trait, HandlerOutcome, ModuleHandler, RequestCtx};

use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ModuleManifest,
        ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
    },
    ModuleHelloAckBody, PROTOCOL_VERSION,
};
use transform::{transform, DeciderInputs, TransformRequest};

/// Canonical module id (overridable via `SUBC_MODULE_ID_ENV` at boot).
pub const DEFAULT_MODULE_ID: &str = "magic-context";

/// Storage namespace for the cache-state domain.
const STORAGE_NAMESPACE: &str = "mc_cache";

/// The module handler. Holds the single store handle, opened once in `on_hello_ack`.
pub struct McHandler {
    store: OnceLock<McStore>,
}

impl McHandler {
    pub fn new() -> Self {
        McHandler {
            store: OnceLock::new(),
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
                let _ = self.store.set(store);
            }
            Err(e) => {
                eprintln!("mc-module: store open failed: {e}");
            }
        }
    }

    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
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
}
