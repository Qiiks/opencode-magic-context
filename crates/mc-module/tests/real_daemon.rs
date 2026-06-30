//! Slice-1 acceptance gate: the cache-stability spine driven THROUGH a live subc
//! daemon (real subc-core spawns nothing — mc-module self-registers as a provider —
//! and a SubcConsumer calls the `transform` op end-to-end over the wire).
//!
//! Proves the production-driveable spine subset: bootstrap-Hard, V1 (growing-tail
//! defer byte-stable), V7 (nonce-only defer stable), epoch-Hard, V8 (revert →
//! defer+reconcile → Hard rematerialize), and V9 (restart → byte-identical replay).
//! The Soft vectors (V2–V5) and the deferred-drain V6 stay lib-only: they need a
//! reduction producer that slice-1 deliberately omits.

#![forbid(unsafe_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Mutex, OnceLock},
    time::Duration,
};

use serde_json::{json, Value};
use subc_client_rs::{CallOptions, ConsumerOptions, RetryBackoff, SubcConsumer};
use subc_protocol::{BindIdentity, RouteTarget};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

const MODULE_ID: &str = "magic-context";
const START_TIMEOUT: Duration = Duration::from_secs(10);

// ---- process lifecycle ----

struct LiveDaemon {
    child: Child,
    runtime_dir: PathBuf,
    config_dir: PathBuf,
    connection_file: PathBuf,
}

impl Drop for LiveDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.runtime_dir);
        let _ = fs::remove_dir_all(&self.config_dir);
    }
}

struct ModuleProcess {
    child: Child,
}

impl ModuleProcess {
    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ModuleProcess {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mc_transform_spine_through_real_daemon() {
    let workspace = workspace_root();
    let subconscious = subconscious_root(&workspace);

    // Build the daemon (from the sibling subconscious workspace) and our module.
    let daemon_bin = ensure_binary(
        &subconscious,
        subconscious.join("target/debug/subc-core"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        workspace.join("target/debug/mc-module"),
        &["build", "-p", "mc-module"],
    );

    let temp = unique_temp_dir("mc-module-real-daemon");
    let runtime_dir = temp.join("runtime");
    let config_dir = temp.join("config");
    let data_home = temp.join("data"); // store lands here (dev_descriptor → XDG_DATA_HOME)
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(&data_home).unwrap();
    write_empty_config(&config_dir);

    let daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;

    let mut module = spawn_module(&module_bin, &daemon.connection_file, &data_home);

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let identity = identity("spine");

    // --- bootstrap-Hard: first pass folds Hard, renders the baseline, mints boundary ---
    let r = call_transform(
        &consumer,
        &identity,
        json!({
            "session_id": "ses",
            "boundary_present": "ignored",
            "render_config": "cfg0",
            "items": [{ "id": "a", "ordinal": 1, "bytes": "<h>BASE</h>" }]
        }),
    )
    .await;
    assert_eq!(r["action"], "HARD", "bootstrap must fold Hard");
    assert_eq!(r["boundary_id"], "a");
    assert_eq!(r["cached_prefix_bytes"], "<h>BASE</h>");
    assert_eq!(r["committed"], true);

    // --- V1 + V7: growing-tail / nonce-only defers, byte-stable, no write ---
    let mut prev: Option<String> = None;
    for _ in 0..4 {
        let d = call_transform(
            &consumer,
            &identity,
            json!({ "session_id": "ses", "boundary_present": "a", "render_config": "cfg0", "items": [] }),
        )
        .await;
        assert_eq!(d["action"], "SOFT+", "defer must not bust");
        assert_eq!(d["committed"], false, "pure defer must not write");
        let bytes = d["cached_prefix_bytes"].as_str().unwrap().to_string();
        if let Some(p) = &prev {
            assert_eq!(&bytes, p, "defer changed bytes over the wire");
        }
        prev = Some(bytes);
    }

    // --- epoch-Hard: a render-config change rematerializes ---
    let e = call_transform(
        &consumer,
        &identity,
        json!({
            "session_id": "ses",
            "boundary_present": "a",
            "render_config": "cfg1",
            "items": [{ "id": "a", "ordinal": 1, "bytes": "<h>BASE2</h>" }]
        }),
    )
    .await;
    assert_eq!(e["action"], "HARD", "epoch change must fold Hard");
    assert_eq!(e["cached_prefix_bytes"], "<h>BASE2</h>");

    // settle back to defer on the new config
    let s = call_transform(
        &consumer,
        &identity,
        json!({ "session_id": "ses", "boundary_present": "a", "render_config": "cfg1", "items": [] }),
    )
    .await;
    assert_eq!(s["action"], "SOFT+");

    // --- V8: revert removes the boundary → defer+reconcile, then Hard rematerialize ---
    let rev = call_transform(
        &consumer,
        &identity,
        json!({ "session_id": "ses", "boundary_present": "-", "render_config": "cfg1", "items": [] }),
    )
    .await;
    assert_eq!(rev["action"], "SOFT+", "revert pass must not bust");
    assert_eq!(
        rev["reconcile_pending"], true,
        "boundary loss flags reconcile"
    );
    assert_eq!(
        rev["cached_prefix_bytes"], "<h>BASE2</h>",
        "revert keeps frozen bytes"
    );

    let remat = call_transform(
        &consumer,
        &identity,
        json!({
            "session_id": "ses",
            "boundary_present": "-",
            "render_config": "cfg1",
            "items": [{ "id": "a2", "ordinal": 2, "bytes": "<h>REVERTED</h>" }]
        }),
    )
    .await;
    assert_eq!(
        remat["action"], "HARD",
        "boundary still absent → Hard remat"
    );
    assert_eq!(remat["boundary_id"], "a2");
    assert_eq!(remat["cached_prefix_bytes"], "<h>REVERTED</h>");
    assert_eq!(remat["reconcile_pending"], false);

    let stable_bytes = call_transform(
        &consumer,
        &identity,
        json!({ "session_id": "ses", "boundary_present": "a2", "render_config": "cfg1", "items": [] }),
    )
    .await["cached_prefix_bytes"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(stable_bytes, "<h>REVERTED</h>");

    // --- V9: restart the module process; durable lineage state replays byte-identical ---
    module.kill_and_wait();
    drop(module);
    // brief settle so the OS releases the single-writer lease before re-acquire
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _module2 = spawn_module(&module_bin, &daemon.connection_file, &data_home);

    let after_restart = call_transform(
        &consumer,
        &identity,
        json!({ "session_id": "ses", "boundary_present": "a2", "render_config": "cfg1", "items": [] }),
    )
    .await;
    assert_eq!(after_restart["action"], "SOFT+", "restart must not bust");
    assert_eq!(
        after_restart["committed"], false,
        "restart replay writes nothing"
    );
    assert_eq!(
        after_restart["cached_prefix_bytes"], "<h>REVERTED</h>",
        "lineage state must reproduce byte-identical across a real process restart"
    );

    drop(consumer);
    drop(daemon);
}

// ---- helpers (adapted from subc-client-rs/tests/real_daemon.rs) ----

async fn call_transform(
    consumer: &SubcConsumer,
    identity: &BindIdentity,
    mut body: Value,
) -> Value {
    // The handler dispatches on `kind`; tag the envelope as a transform op. The
    // TransformRequest struct ignores this extra field.
    if let Value::Object(map) = &mut body {
        map.insert("kind".to_string(), Value::String("transform".to_string()));
    }
    let bytes = consumer
        .call(
            RouteTarget::ToolProvider {
                module_id: MODULE_ID.to_string(),
            },
            identity.clone(),
            serde_json::to_vec(&body).unwrap(),
            fast_call_options(),
        )
        .await
        .unwrap_or_else(|e| panic!("transform call failed: {e:?}"));
    serde_json::from_slice(&bytes).unwrap()
}

fn spawn_daemon(daemon_bin: &Path, runtime_dir: &Path, config_dir: &Path) -> LiveDaemon {
    let child = Command::new(daemon_bin)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_CONFIG_HOME", config_dir)
        .env("SUBC_PORT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn daemon {}: {e}", daemon_bin.display()));
    LiveDaemon {
        child,
        runtime_dir: runtime_dir.to_path_buf(),
        config_dir: config_dir.to_path_buf(),
        connection_file: runtime_dir.join("subc-connection.json"),
    }
}

fn spawn_module(module_bin: &Path, connection_file: &Path, data_home: &Path) -> ModuleProcess {
    let child = Command::new(module_bin)
        .arg("--subc")
        .arg(connection_file)
        .env(subc_protocol::SUBC_MODULE_ID_ENV, MODULE_ID)
        .env("XDG_DATA_HOME", data_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn module {}: {e}", module_bin.display()));
    ModuleProcess { child }
}

fn write_empty_config(config_dir: &Path) {
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    fs::write(
        config_dir.join("cortexkit").join("subc.jsonc"),
        serde_json::to_string_pretty(&json!({ "version": 1, "modules": {} })).unwrap(),
    )
    .unwrap();
}

fn fast_consumer_options() -> ConsumerOptions {
    ConsumerOptions {
        handshake_timeout: Duration::from_secs(2),
        reconnect_backoff: RetryBackoff {
            base: Duration::from_millis(50),
            cap: Duration::from_millis(250),
            max_attempts: 40,
        },
        restored_debounce: Duration::from_millis(10),
    }
}

fn fast_call_options() -> CallOptions {
    CallOptions {
        timeout: Duration::from_secs(8),
        route_retry: RetryBackoff {
            base: Duration::from_millis(50),
            cap: Duration::from_millis(250),
            max_attempts: 60,
        },
        route_retry_deadline: Duration::from_secs(10),
        ..CallOptions::default()
    }
}

fn identity(session: &str) -> BindIdentity {
    let project_root = unique_temp_dir("mc-module-project");
    fs::create_dir_all(&project_root).unwrap();
    BindIdentity {
        project_root,
        harness: "mc-module-test".to_string(),
        session: session.to_string(),
    }
}

async fn wait_for_connection_file(path: &Path, wait: Duration) {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if path.exists() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("daemon did not write {} within {wait:?}", path.display());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn ensure_binary(manifest_dir: &Path, path: PathBuf, cargo_args: &[&str]) -> PathBuf {
    static BUILD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = BUILD_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let output = Command::new("cargo")
        .args(cargo_args)
        .current_dir(manifest_dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run cargo {cargo_args:?}: {e}"));
    assert!(
        output.status.success(),
        "cargo {cargo_args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(path.exists(), "expected binary at {}", path.display());
    path
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

fn subconscious_root(workspace: &Path) -> PathBuf {
    workspace.parent().unwrap().join("subconscious")
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{name}-{}-{nonce}", std::process::id()))
}
