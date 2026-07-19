# Historian hardening batch (post-ARC): backoff gate, pre-fire trace, timeout re-drain, dead code

Branch: subc-migration (session cwd repo). All work in crates/mc-module (+ crates/mc-core for one deletion). These are four small, independent fixes sharing the historian surface; land as ONE commit series (one commit per item is fine). All were found during the live rig gate that just completed.

## Context you need first
Read crates/mc-module/src/historian.rs (run_historian_firing, the abandon arms), crates/mc-module/src/historian_producer.rs (HistorianProducerDriver, await_output, drain_subscribe, DEFAULT_AWAIT_TIMEOUT=600s), crates/mc-module/src/lib.rs (maybe_spawn_historian_fire ~line 460-560: the fire gate, the spawned firing task, record_no_fire, the diagnostics block), crates/mc-store/src/lib.rs (HistorianDurableState: state/firing_seq/fired_at_ms/failure_backoff_at_ms/last_failure/last_no_fire, abandon_* and *_persist methods).

Durable-state invariants that MUST hold: last_failure is written by abandon arms and cleared when a firing establishes its producer run; last_no_fire is change-gated (same reason twice = no write) and cleared by fire(); the "busy" skip is never recorded (would race the live firing's writes); all diagnostics writes are best-effort (a CAS conflict must never fail a pass).

## Item 1 — failure backoff must gate re-fires
failure_backoff_at_ms is set on every abandon (currently now+60_000) but READ BY NOTHING: a firing that fails validate re-fires on the very next pass, hammering the model. Fix: in the fire path (maybe_spawn_historian_fire, where state==Idle is checked), if failure_backoff_at_ms is Some(t) and now < t, do NOT fire; record no_fire reason "backoff" — EXCEPTION to the change-gate style: use the plain string "backoff" (no timestamp inside, so the change-gate naturally dedups). When now >= t, fire normally (and the successful establish clears last_failure as today; also clear failure_backoff_at_ms at the same point). The 60s constant should become a named const with a doc comment (why: reject → model retry loop needs a cool-down so a persistently-failing model doesn't burn tokens every pass).
Tests: (a) abandon sets backoff → next pass with now<t does not fire, last_no_fire=="backoff", state stays Idle; (b) now>=t fires normally; (c) establish clears both last_failure and failure_backoff_at_ms.
IMPORTANT: the emergency-drain path, if any bypasses exist, is NOT in scope — do not invent one.

## Item 2 — pre-fire connect failures must write last_failure
In the spawned firing task (lib.rs, the tokio::spawn in maybe_spawn_historian_fire), a producer CONNECT failure before fire()/run establishment currently only eprintln!s — a supervised daemon captures no stderr, so it's invisible. Fix: on the connect-error arm, write last_failure = "producer connect: <error>" to HistorianDurableState. CARE: at that point durable state is mid-Firing (fire() already transitioned) OR still pre-transition depending on the exact arm — read the actual code and make the write consistent with the state machine: if fire() already transitioned to Firing, route through the existing abandon path (which sets backoff + detail — item 1 then gates the refire); if genuinely pre-transition (Idle), do a detail-only best-effort write that does NOT touch state/backoff. Do not invent new state transitions.
Tests: simulate connect failure (the test seam for the producer factory exists — ScriptedProducer / the factory injection used by existing tests), assert last_failure contains "connect" and the state machine lands where the arm dictates (Idle either way after abandon), and that a later successful firing clears it.

## Item 3 — await-timeout recovers the durable run instead of abandoning
Today await_output timing out abandons the firing and the run's completed output is lost (the rig hit this: a run finished moments after the 600s waiter gave up; re-fire re-paid a 50k-input pass). llm-runner runs are durable and re-drainable: subscribing from start re-reads all units (the restart-reattach path already relies on exactly this). Fix in run_historian_firing's await-timeout arm: before abandoning, attempt ONE recovery: reconnect (or reuse the connection if still alive), re-subscribe from start for the same run (the reattach code path shows how — reuse/extract that machinery rather than duplicating it), drain with a SHORT bounded budget (60s const, doc comment), and if a terminal for the run_id is found, proceed exactly as a normal completion (validate → publish). If recovery also times out or errors, abandon as today (backoff + last_failure noting "timed out; recovery re-drain also failed").
Constraints: no unbounded second wait; the recovery drain must attribute terminals by run_id exactly like drain_subscribe does (RunStarted tracking); reasoning-block exclusion applies identically (reuse the same unit_text path). Do NOT restructure the state machine — this all happens within the firing's existing Firing/AwaitingProducer phase before the abandon decision.
Tests: (a) scripted producer: first await times out, recovery re-drain returns the terminal → publish succeeds, NO abandon, no backoff set; (b) both time out → abandon with the combined last_failure detail; (c) recovery terminal for a DIFFERENT run_id → not accepted, abandon.
If the producer trait's shape makes "re-subscribe same run" impossible without a trait change, extend the trait minimally (e.g. a resubscribe(run_id) method with a default Err impl for scripted tests that don't care) — keep it small.

## Item 4 — delete three orphaned functions (confirmed dead by callgraph + grep)
- crates/mc-core/src/decay.rs::compute_budget_pressure_two_pass (superseded solver kept from the port; the shipped one is the other solver)
- crates/mc-module/src/boundary.rs::flatten_messages
- crates/mc-module/src/boundary.rs::chunked_token_estimate (stranded by the TokenIndex rebuild)
Delete each plus any now-unused helpers/imports/tests that exist ONLY to serve them. If any deletion cascades into something still used, stop and keep that one (report it) rather than forcing it.

## Gates (all must pass)
cargo test -p mc-module -p mc-core -p mc-store --features mc-store/test-support
cargo test -p mc-module --test real_daemon
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
check_comments (fix flagged comments)
Commit messages: explain the WHY (rig-gate findings: silent re-fire hammering, invisible connect failures on supervised rigs, lost completed runs), never reference note numbers or this brief.
