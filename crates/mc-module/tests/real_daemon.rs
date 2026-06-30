//! End-to-end acceptance test: the cache-stability transform driven THROUGH a live
//! subc daemon (a real subc-core spawns mc-module as a provider, and a SubcConsumer
//! calls the `transform` op over the wire).
//!
//! Covered here (the cases drivable through the real production path): the first-pass
//! Hard fold, growing-tail and nonce-only defers (cached prefix byte-stable), an
//! epoch (render-config) Hard, a revert that removes the boundary (defer + reconcile,
//! then Hard rematerialize), and a process restart replaying byte-identical. The m1
//! delta SOFT and the deferred-drop drain need a content/reducer producer not yet
//! built, so they are exercised in the library tests with stubbed inputs instead.

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

    // ===== PRODUCTION-PATH cases (session "spine", `_decider` ABSENT) =====
    // These drive the real transform path with no test-only surface present.

    // bootstrap: the first pass folds Hard, rendering [m0(covered), m1(placeholder)] ++ tail.
    let r = call(
        &consumer,
        &identity,
        json!({
            "session_id": "spine", "render_config": "cfg0",
            "items": [ck("a", 1, "<h>BASE</h>")]
        }),
    )
    .await;
    assert_eq!(r["action"], "HARD", "bootstrap must fold Hard");
    assert_eq!(r["boundary_id"], "a");
    assert_eq!(m0(&r), "<h>BASE</h>");
    assert_eq!(m1(&r), M1_PLACEHOLDER);
    assert_eq!(r["committed"], true);

    // growing-tail defers. Send the FULL live array each pass (the module locates the
    // boundary over it). Prefix blocks byte-identical; tail verbatim; no write.
    let mut prev_m0: Option<String> = None;
    for n in 2..=5u64 {
        let mut items = vec![ck("a", 1, "<h>BASE</h>")];
        for k in 2..=n {
            items.push(ck(&format!("t{k}"), k, &format!("tail{k}")));
        }
        let d = call(
            &consumer,
            &identity,
            json!({
                "session_id": "spine", "render_config": "cfg0", "items": items
            }),
        )
        .await;
        assert_eq!(d["action"], "SOFT+", "defer must not bust");
        assert_eq!(d["committed"], false, "pure defer must not write");
        if let Some(p) = &prev_m0 {
            assert_eq!(&m0(&d), p, "m0 changed on defer over the wire");
        }
        let tail: Vec<String> = (2..=n).map(|k| format!("t{k}")).collect();
        assert_eq!(tail_ids(&d), tail, "tail must be verbatim live items");
        prev_m0 = Some(m0(&d));
    }

    // epoch-Hard: a render-config change rematerializes.
    let e = call(
        &consumer,
        &identity,
        json!({
            "session_id": "spine", "render_config": "cfg1",
            "items": [ck("a", 1, "<h>BASE2</h>")]
        }),
    )
    .await;
    assert_eq!(e["action"], "HARD", "epoch change must fold Hard");
    assert_eq!(m0(&e), "<h>BASE2</h>");

    // revert removes the boundary "a" (array no longer contains it) → defer+reconcile,
    // then Hard rematerialize against the live array.
    let rev = call(
        &consumer,
        &identity,
        json!({
            "session_id": "spine", "render_config": "cfg1", "items": [ck("z", 9, "<h>OTHER</h>")]
        }),
    )
    .await;
    assert_eq!(rev["action"], "SOFT+", "revert pass must not bust");
    assert_eq!(
        rev["reconcile_pending"], true,
        "boundary loss flags reconcile"
    );
    assert_eq!(m0(&rev), "<h>BASE2</h>", "revert keeps frozen m0");

    let remat = call(&consumer, &identity, json!({
        "session_id": "spine", "render_config": "cfg1", "items": [ck("a2", 10, "<h>REVERTED</h>")]
    })).await;
    assert_eq!(
        remat["action"], "HARD",
        "boundary still absent → Hard remat"
    );
    assert_eq!(remat["boundary_id"], "a2");
    assert_eq!(m0(&remat), "<h>REVERTED</h>");
    assert_eq!(remat["reconcile_pending"], false);

    // ===== PRODUCER-LOGIC cases (session "prod", `_decider` PRESENT) =====
    // The delta/fold LOGIC runs for real through the transform; only the upstream content (which
    // memories/compartments changed, where to cut) is stubbed via `_decider`. A separate session
    // keeps the production-path cases above free of any test-only surface.

    // bootstrap the prod session
    call(
        &consumer,
        &identity,
        json!({
            "session_id": "prod", "render_config": "cfg0", "items": [ck("a", 1, "<h>BASE</h>")]
        }),
    )
    .await;

    // an m1 delta rides as a SOFT — m0 frozen, m1 re-renders.
    let v5 = call(
        &consumer,
        &identity,
        json!({
            "session_id": "prod", "render_config": "cfg0", "items": [ck("a", 1, "<h>BASE</h>")],
            "_decider": { "m1_content": { "revision": 7, "body": "<mem>rule</mem>" } }
        }),
    )
    .await;
    assert_eq!(v5["action"], "SOFT", "m1 delta rides as a SOFT");
    assert_eq!(m0(&v5), "<h>BASE</h>", "m0 stays frozen across a SOFT");
    assert_eq!(m1(&v5), "<mem>rule</mem>");

    // a HARD fold folds the m1 content into m0, resets m1, mints a new boundary.
    let v6 = call(
        &consumer,
        &identity,
        json!({
            "session_id": "prod", "render_config": "cfg0",
            "items": [ck("a", 1, "<h>BASE</h>"), ck("b", 2, "<h>MORE</h>")],
            "_decider": { "hard_fold_requested": true, "fold_through_ordinal": 2,
                          "m1_content": { "revision": 7, "body": "<mem>rule</mem>" } }
        }),
    )
    .await;
    assert_eq!(v6["action"], "HARD");
    assert_eq!(v6["boundary_id"], "b");
    assert_eq!(
        m0(&v6),
        "<h>BASE</h><h>MORE</h><mem>rule</mem>",
        "m1 folded into m0"
    );
    assert_eq!(m1(&v6), M1_PLACEHOLDER, "m1 reset to placeholder");

    // ===== TAIL-REDUCER cases (session "red", `_decider.reductions` PRESENT) =====
    // A tail tool output is reduced in place (its bytes → [dropped N]) and the reduction
    // is frozen/replayed/folded through the real transform over the wire.

    call(
        &consumer,
        &identity,
        json!({ "session_id": "red", "render_config": "cfg0", "items": [ck("a", 1, "BASE")] }),
    )
    .await;

    // freeze a drop on tail item t2 (a SOFT); the surrounding live item stays verbatim
    let red_items = json!([
        ck("a", 1, "BASE"),
        ck("t2", 2, "HUGE-OUTPUT"),
        ck("t3", 3, "after")
    ]);
    let rd =
        json!({ "reductions": [{ "target_id": "t2", "kind": "drop", "payload": "[dropped 1]" }] });
    let froze = call(
        &consumer,
        &identity,
        json!({ "session_id": "red", "render_config": "cfg0", "items": red_items, "_decider": rd }),
    )
    .await;
    assert_eq!(froze["action"], "SOFT", "a new reduction rides a SOFT");
    assert_eq!(
        tail_bytes(&froze, "t2"),
        "[dropped 1]",
        "t2 reduced in place"
    );
    assert_eq!(
        tail_bytes(&froze, "t3"),
        "after",
        "interleaved live item verbatim"
    );

    // defer: the frozen reduction replays byte-identical, no write
    let defer = call(
        &consumer,
        &identity,
        json!({ "session_id": "red", "render_config": "cfg0", "items": red_items, "_decider": rd }),
    )
    .await;
    assert_eq!(
        defer["action"], "SOFT+",
        "unchanged reduction set → pure defer"
    );
    assert_eq!(defer["committed"], false);
    assert_eq!(
        tail_bytes(&defer, "t2"),
        "[dropped 1]",
        "frozen reduction replays"
    );

    // a HARD fold whose coverage crosses t2 → m0 carries the REDUCED bytes, red:t2 GC'd
    let fold_red = json!({
        "hard_fold_requested": true, "fold_through_ordinal": 2,
        "reductions": [{ "target_id": "t2", "kind": "drop", "payload": "[dropped 1]" }]
    });
    let folded = call(
        &consumer,
        &identity,
        json!({ "session_id": "red", "render_config": "cfg0", "items": red_items, "_decider": fold_red }),
    )
    .await;
    assert_eq!(folded["action"], "HARD");
    assert_eq!(folded["boundary_id"], "t2");
    assert_eq!(
        m0(&folded),
        "BASE[dropped 1]",
        "m0 carries the reduced bytes for the covered item"
    );

    // ===== restart the module process and confirm byte-identical replay (spine session) =====
    module.kill_and_wait();
    drop(module);
    tokio::time::sleep(Duration::from_millis(200)).await; // OS releases the single-writer lease
    let _module2 = spawn_module(&module_bin, &daemon.connection_file, &data_home);

    let after = call(&consumer, &identity, json!({
        "session_id": "spine", "render_config": "cfg1", "items": [ck("a2", 10, "<h>REVERTED</h>")]
    })).await;
    assert_eq!(after["action"], "SOFT+", "restart must not bust");
    assert_eq!(after["committed"], false, "restart replay writes nothing");
    assert_eq!(
        m0(&after),
        "<h>REVERTED</h>",
        "lineage m0 reproduces byte-identical across restart"
    );

    drop(consumer);
    drop(daemon);
}

const M1_PLACEHOLDER: &str = "(no new content since last materialization)";

fn ck(id: &str, ordinal: u64, bytes: &str) -> Value {
    json!({ "id": id, "ordinal": ordinal, "bytes": bytes })
}

/// The m0 synthetic block bytes from a response's ck_messages.
fn m0(r: &Value) -> String {
    block_bytes(r, "mc_m0")
}
fn m1(r: &Value) -> String {
    block_bytes(r, "mc_m1")
}
fn block_bytes(r: &Value, id: &str) -> String {
    r["ck_messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id)
        .unwrap_or_else(|| panic!("no {id} block in ck_messages: {r}"))["bytes"]
        .as_str()
        .unwrap()
        .to_string()
}
/// The non-synthetic tail item ids, in order.
fn tail_ids(r: &Value) -> Vec<String> {
    r["ck_messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["synthetic"] != json!(true))
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect()
}
/// The bytes of a non-synthetic tail item by id.
fn tail_bytes(r: &Value, id: &str) -> String {
    r["ck_messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id && i["synthetic"] != json!(true))
        .unwrap_or_else(|| panic!("no tail item {id} in ck_messages: {r}"))["bytes"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---- helpers (adapted from subc-client-rs/tests/real_daemon.rs) ----

async fn call(consumer: &SubcConsumer, identity: &BindIdentity, mut body: Value) -> Value {
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
