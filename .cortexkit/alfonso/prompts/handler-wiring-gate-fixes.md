# Fix round: handler-wiring gate BLOCK findings (3, all source-verified)

Target: crates/mc-module on branch subc-migration (HEAD 0df2ea03 = the merged handler wiring). Read .alfonso/plans/mc-module-handler-wiring.md (v2) for the design context. The adversarial gate returned BLOCK with three findings; fix all three plus their tests. Commit ON TOP (the base is merged; no amends).

## Finding 1 (gate-breaker): a LIVE firing can be double-driven by reattach
maybe_spawn_reattach() (lib.rs ~270-352) spawns whenever durable state is AwaitingProducer — but a NORMAL in-process firing sits in AwaitingProducer while its spawned task awaits producer output. A second transform in that window spawns a "reattach" for the same run; persist_historian_state (historian.rs:274-285) blindly overwrites, so a stale reattach can resurrect/publish after the original task idles.

FIX: handler-level `live_historian_sessions: Arc<Mutex<HashSet<String>>>` (same pattern as the existing reattaching_sessions latch): insert BEFORE spawn_historian_firing, remove in the spawned task's completion path — BOTH success and error arms (use a scope-guard-style struct or explicit removes on every arm; a panic in the task must not leak the entry — wrap the body so the removal is unconditional). maybe_spawn_reattach() skips any session present in the set.

## Finding 2: crash/restart recovery only wired for AwaitingProducer
handle_restart_load() (historian.rs:360-398) correctly abandons Firing|Validating|Publishing, but the handler only reacts to AwaitingProducer — after a crash in those phases, every future transform reports busy forever and never recovers.

FIX: in the handler's per-pass historian evaluation, when NO live in-process firing exists (per the Finding-1 set) and durable state is non-Idle:
- AwaitingProducer → existing reattach path (unchanged).
- Firing | Validating | Publishing → drive the handle_restart_load()/abandon recovery (spawned, guarded by the same reattach single-flight latch) so the state returns to Idle and the NEXT pass can refire.

## Finding 3: config threshold parity
config.rs has DEFAULT_EXECUTE_THRESHOLD_PERCENTAGE = 80.0 and clamp(1.0, 100.0). TS parity: the schema default is 65 and the CAP is 80 (verify the exact numbers at packages/plugin/src/config/schema/magic-context.ts — the execute threshold field's .default() and .max()). Align: default 65.0, clamp cap 80.0, doc comment naming the TS source file. Project-may-only-raise stays, still capped at 80.

## Tests (handler-level, reuse the existing ProducerState/handler_with_store harness)
a. Live firing + concurrent second transform → NO reattach spawned (producer bind/status counters unchanged beyond the firing's own; second response no_fire="busy").
b. Seeded Publishing durable state, no live firing → recovery runs (state returns to Idle; response reflects recovery not busy-forever), and a SUBSEQUENT pass can fire fresh.
c. Same for seeded Firing and Validating.
d. Config: default 65 when no user value; user 70 + project 90 → 80 (raise capped); doc-comment updated.
e. Stale-reattach-cannot-overwrite-Idle: drive the reattach path against an Idle session → no-op (no producer calls, no state change).

## Gates (all): cargo test -p mc-module -p mc-core -p mc-store --features mc-store/test-support; cargo test -p mc-module --test real_daemon; cargo clippy --workspace --all-targets -- -D warnings; cargo fmt --check; check_comments. Commit message: name the double-drive race and the wedge-forever recovery gap as the load-bearing fixes.
