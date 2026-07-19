# Implement: reduction-producer slice RP-B — the >=95% blocking arm (design doc embedded below)

Branch: subc-migration (fork from HEAD — RP-A is merged: scheduler wiring, selection, agent-drop queue, durable substrate all live). Implement ONLY P5 of the embedded v4 design plus its R3-F1 notify-lifecycle pin. Files: crates/mc-module/src/lib.rs (handler spawn/await paths), possibly historian.rs helpers. Do NOT touch transform.rs's selection/classify region (RP-A owns it, merged) except where the re-run call requires it.

Scope, precisely:
1. Per-session completion notify: installed ATOMICALLY with the live_historian_sessions insert (same mutex critical section, BEFORE the latch is observable — never inside the spawned task). Token-guarded cleanup (Arc identity or firing_seq): remove only if the token matches, notify BEFORE the SessionSetGuard releases the live latch. Both races from the design doc must have tests (busy-observed-before-notify-exists is impossible by construction; stale cleanup cannot delete the next run's entry).
2. At usage >= 95% (scheduler PassDecision::Emergency95) with an eligible fire: drive the firing INLINE on the request task (acquire the latch inline, same run_historian_firing, same producer budget). On publish success re-run apply_once in the same request so the fold lands in THIS response. On failure/timeout: abandon exactly like the spawned path and proceed with the pass's already-computed output (emergency selection ran in RP-A's path).
3. Busy case at >=95%: await the ACTIVE run's completion via the notify (bounded by the producer await budget), then re-run apply_once. TS parity: TS blocks on the active run too, force-starting only if absent.
4. Below 95%: spawn path unchanged.

Tests: inline-drive-at-95 (scripted producer publishes -> response carries the folded m0, action HARD, single request), busy-await-at-95 (a live firing completes mid-request -> re-run folds), inline failure degrades (abandon + backoff + the response still returns the emergency-selection output), below-95 still spawns (no inline blocking), notify-lifecycle races (installed-before-observable; token-guarded cleanup). Reuse the ScriptedProducer + factory seams.

Gates: cargo test -p mc-module -p mc-core -p mc-store --features mc-store/test-support; cargo test -p mc-module --test real_daemon; cargo clippy --workspace --all-targets -- -D warnings; cargo fmt --check; check_comments. Commits explain WHY without referencing rounds/briefs/this file.

----- DESIGN DOC (v4, verbatim — implement P5 + R3-F1 only) -----

# MC module: reduction-producer lane (v4 design — implementation-ready)

Round 3: REVISE with ONE finding (completion-notify lifecycle) + verified all six R2 folds
at source. Folded as R3-F1 below. Round 3 also pinned two implementation cautions: the
last_committed_pass_at_ms anchor is set only AFTER the changed-comparison (never creating
the commit), and SessionBinding.model_key populates None until the identity protocol
carries a model (per-model overrides deferred anyway).

Round 2 (BLOCK, 4+2+2): cache_ttl source/default corrected (5m, SessionMeta not config),
tail_state provider_executed exclusion, timing seeding replaced with a durable
last_response_at anchor written on committing passes only, the drain-tx seam named as a
new commit variant, and the ≥95% busy-case aligned to TS (block on the ACTIVE run too).
Corrections marked R2-Fn. v2 (round 1): redesigned the four blocked areas:
full SchedulerInputs sourcing incl. a config expansion (F1), the usage-number sourcing
corrected to TS parity (F3), timing durability split in-memory vs durable (F5), and the
revert-vs-watermark hazard closed by gating the two-pass selector under reconcile (F6).
Corrections marked R1-Fn.

Wires the three already-built isolated pieces into the live transform path and deletes the
last test-only seam (`_decider`). After this lane, the module produces its own reductions
end-to-end and is feature-complete against the TS transform's reclaim behavior (minus
harness-delivered nudge text, which stays on harness legs by design).

## What exists (all gate-passed, none wired)

- `scheduler.rs` (Unit S): `decide(SchedulerInputs) -> SchedulerOutcome` — execute/defer/
  force/emergency pass class, TTL predicates, mid-turn deferral, drain latch, overflow
  scan. NOT called by the live path; `transform.rs` classifies purely from core signals
  (every producer-less pass is effectively a defer with replay).
- `selection.rs` (Unit R): `select_reductions(items, frozen, SelectionContext,
  SelectionConfig) -> Vec<SelDecision>` — control-plane drops, edit supersession
  (smart-drops-gated), two-pass age drops, ctx_reduce agent drops, tiered emergency
  eviction with recency reserve, skeleton window, arc-atomic emission, drop-wins merge.
  Differential-golden'd against the TS selectors. Consumes `SelItem`s (flat blocks).
- Slice-3 mechanics in `transform.rs`: freeze-once / replay-forever / fail-loud, GC on
  HARD, monotonicity validation. Consume `DeciderInputs.reductions` — the seam to delete.

## Design

### P0 — config expansion (R1-F1, new pre-slice)
Current module config is only {model_chain, execute_threshold_percentage, memory_enabled}.
The scheduler/selection consume more; expand the tiered config (same trust pins, same
JSONC reader) with exactly what they read and nothing else:
- `smart_drops: bool` (default false) — project-tier allowed (behavioral, not cost).
- `cache_ttl: String` (default "5m" — the TS schema default; R2-F1: NOT 60m, and it is
  consumed via `SessionMeta.cache_ttl`, not `SchedulerConfig`) — user-tier only.
- Per-model threshold overrides: DEFERRED — the scalar `execute_threshold_percentage`
  covers the rig and v1 module scope; `model_key` is still passed (from the session
  binding's model) so the scheduler API stays complete, with an empty override map.

### P1 — scheduler wiring (R1-F1: full input sourcing)
`handle()` calls `scheduler::decide()` BEFORE `apply_once`. Every `SchedulerInputs` field
sourced explicitly:
- `config`: from P0 config (thresholds, cache_ttl) — frozen at bind like the budget.
- `usage`: request `ModuleUsage` (fill-basis, the LLMRUNNER-fixed number).
- `session` (SessionMeta timing): durable fields per P3 + in-memory recency (P3 split).
- `now_ms`: request time.
- `model_key`: `SessionBinding` gains a `model_key: Option<String>` field, populated at
  route bind from the connection identity when the harness supplies one; None otherwise
  (per-model overrides are deferred anyway, so None is fully functional) (R2-F1).
- `context_limit`: request `ModuleUsage.context_limit_tokens` (200k fallback as today).
- `tail_state` (mid-turn tool-use detection): derived from the live FlatProjection — the
  newest assistant message has an open arc: a ToolCall block with `provider_executed ==
  false` (R2-F2: provider/server-executed tools are EXCLUDED, matching the TS mid-turn
  detector and the selection exclusions) whose arc has no ToolResult in live. Pure
  function of the request, no new state.
- `deferred_execute`: DURABLE (`ModuleMeta.deferred_execute_state: Option<...>`,
  serde(default)) — the TS CAS-flag equivalent; written on the deferring pass (which
  commits BECAUSE it writes this flag), cleared on the consuming execute.
- `boundary_bypass`: explicit-bust = false in v1 (no /ctx-flush equivalent on this leg
  yet); subagent = false (self-owned sessions never reach here — identity pass-through).
- `drain_latch`: durable per P3.
- `overflow_error_text`: request field `provider_error: Option<String>` — the harness leg
  forwards provider overflow errors when it has them; None otherwise (scheduler treats as
  no-overflow). Pinned into the wiring contract as an OPTIONAL caller field.
Contract with the core: the scheduler NEVER forces the core's plan — it gates the
PRODUCERS. The core's classify stays the authority on byte semantics; a scheduler
"execute" with zero new reductions and no other delta lands as a byte-identical
defer-shaped pass. An idle-TTL hard rides the existing `hard_fold_requested` advisory.

PRODUCER GATE (R1-F4, the TS BUST-clause restored): selection runs when
`scheduler == Execute/EmergencyForce` OR the pass is ALREADY BUSTING for another reason
(hard_fold_requested advisory true: first-fold, reconcile, epoch/TTL hard). Mirrors TS
shouldRunHeuristics = (execute || materialization || hardFold); a known-bust pass drains
reductions into the bust it's already paying for.

### P2 — selection wiring (deletes `_decider`)
On the producer gate (P1), the transform builds `SelItem`s from `tail_sel_items(live,
coverage_ordinal)` — the COVERAGE-FILTERED tail, NOT the full projection (R1-F2: covered
raw blocks must not enter the candidate pool or the emergency floor math). It assembles
`SelectionContext` from durable meta + usage, runs `select_reductions`, and feeds the
output where `DeciderInputs.reductions` is consumed today. On Defer: selection does not
run (mechanics replay frozen — existing invariant).
`current_total_input_tokens` (R1-F3 corrected): the REQUEST usage fill number
(`ModuleUsage` input+cache-read basis) — the same number the scheduler consumed, measured
by the provider for the PREVIOUS request, exactly the TS parity source (TS reads
contextUsage.inputTokens before cleanup; it never re-measures composed output). The v1
doc's "module-derived over composed m0+m1+tail" was wrong (compose happens AFTER
selection; the number is a pre-pass measurement, not a post-compose one).
TWO-PASS SELECTOR UNDER REVERT (R1-F6): when `loaded.core.reconcile_pending` is true, the
two-pass age selector is DISABLED for the pass (the durable `last_execute_ordinal` may be
stale-high vs a store about to be re-cut; ordinals in live may be below it, and selecting
on it would over-drop re-exposed young content). The re-cut arm additionally clamps
`last_execute_ordinal = min(self, new_coverage_end, treating None coverage as 0)` in the
same commit, restoring the watermark for future passes. Emergency eviction (recency-
reserved, floor-based) stays active under reconcile — it keys on live ordinals only.
`_decider` field: DELETED from the request; the remaining test fixtures migrate to either
(a) driving selection through crafted live arrays (preferred for behavior tests) or (b) a
`#[cfg(test)]` injection hook on the module handler (for monotonicity/adversarial tests
that need hand-crafted reductions).

### P3 — state substrate (R1-F5: durable/in-memory split)
DURABLE (ModuleMeta, serde(default), written only by passes that commit for their own
reasons — the flag/watermark writes ARE the committing delta on their passes):
- `last_execute_ordinal: u64` — two-pass age-drop watermark; written on execute-pass
  commit (an execute pass that froze new reductions commits by definition; an execute with
  ZERO deltas skips the watermark write — nothing was dropped, nothing to watermark).
- `last_emergency_input_sample: f64` + `has_prior_emergency_drop: bool` — emergency
  idempotence latch; written by the emergency pass (which commits — it froze evictions).
- `deferred_execute_state` — see P1; the write IS the deferral record.
- `emergency_drain_active: bool` + entered-at — the drain latch (enter ≥95%, exit
  < executeThreshold−10, 30-min self-expiry); latch transitions are rare and ride their
  own commit.
IN-MEMORY + ONE DURABLE ANCHOR (R2-F3 corrected — the v2 seeding anchors were wrong:
expiry_cutoff_ms is the HARD-materialization clock and fired_at_ms is historian
fire-start; neither is a response-recency signal, and mc_cache_state has no updated-at):
- Per-session in-memory map holds last-pass/last-response times for TTL/cadence within a
  process lifetime. A pure defer pass never writes durable state (invariant 5).
- ONE new durable anchor `last_committed_pass_at_ms: i64` (serde(default)) written ONLY by
  passes that already commit for their own reasons (HARD/SOFT/flag writes) — piggybacked,
  never a commit cause. IMPLEMENTATION PIN (round 3): the anchor is set AFTER the
  `core != loaded.core || meta != loaded.meta` changed-comparison decides the pass
  commits — setting it before the comparison would make every pass "changed" and create
  the write-per-pass invariant 5 forbids. It is a genuine (if sparse) recency lower bound.
- Restart seeding: the in-memory entry seeds from `last_committed_pass_at_ms`; if absent
  (0), TTL-hard is DISABLED for the session until the first committing pass refreshes it.
  Conservative in both directions: a stale lower bound can only make the session look
  IDLE-LONGER... which would OVER-fire TTL — so the predicate additionally requires an
  in-memory last-response reading from THIS process lifetime (the durable anchor alone
  never fires TTL-hard; it only supports cadence/diagnostics). Net: TTL-hard fires only on
  process-lifetime-observed idleness, exactly the slice-1 R3-F2 deferral finally
  implemented with a real observation basis.

### P4 — agent drops (ctx_reduce) arrival: DURABLE QUEUE (RULED by SUBC, pm_038c8920)
NOT a request field (caller-retransmission-as-correctness footgun: a drop takes effect on
the next BUSTING pass, possibly many defers later — a request field silently loses it if
any caller stops resending). Instead:
- New mc-store table `pending_agent_drops` (session-keyed): ctx_reduce arrives as an MC
  COMMAND on the ordinary magic-context session route (same BindIdentity as the transform,
  demuxed by method) — the handler appends drop ids to the queue. Fire-and-forget for the
  caller; the module owns durability and drain timing. TS pending_ops is the precedent.
- Identical on both legs: llm-runner's ToolPlane routes the agent's ctx_reduce tool call
  to the MC command (ctx_reduce IS an MC-provided tool); ai-proxy translates the injected
  tool call to the same command when its Mode-4 lands (FUTURE — do not block RP on it;
  design the command shape so ai-proxy reuses it verbatim).
- DRAIN PINS (the #423 two-tx lesson applies):
  1. Atomicity: drain-into-frozen-set + clear-consumed-queue-rows commit in the SAME tx as
     the consuming busting pass. Pass aborts → rows stay, retried next busting pass. Never
     delete-then-compose across a tx gap. SEAM (R2-F4): `McStore::commit` owns a private
     fenced tx writing only mc_cache_state — a new variant
     `commit_with_consumed_drops(session, expected, core, meta, consumed_drop_ids)` (or a
     builder over the same fenced tx) deletes the consumed queue rows inside that tx. The
     transform passes the consumed ids alongside the commit; the plain commit stays
     unchanged for every other pass.
  2. Idempotency: draining an id already in the frozen set is a no-op (re-drive/replay and
     resent-command safe). Unresolvable ids (not in live, not frozen) are dropped with the
     consumed rows — stale-reduce semantics subsumed by frozen-set replay.
`SelectionContext.agent_drop_ids` is then populated from the queue read (transform-side),
not from any wire field.

### P5 — ≥95% blocking arm (R1-F7: matched to the real spawn structure)
On `usage ≥ 95%` with an eligible fire, the handler does NOT spawn: it drives the firing
INLINE on the request task — same `live_historian_sessions` guard acquired inline (the
latch admits it: one owner either way), same run_historian_firing, same producer await
budget (600s + re-drain; a ≥95% session is already stalled — TS blocks here too, and a
long wait that succeeds beats an overflow). On publish success the handler re-runs
apply_once in the same request (the fold lands in THIS response). On failure/timeout the
inline drive abandons exactly like the spawned path (backoff + detail) and the pass
proceeds with the emergency selection output already frozen this pass (RP-2 dependency).
Below 95% the spawn path is unchanged. The busy case (R2-F5, TS-parity restored): TS at
≥95% awaits the ACTIVE run too (force-starting only if absent) — so the module does the
same: if a firing is already live, the handler awaits ITS completion (bounded by the same
producer budget; the in-flight run's completion future is exposed via a per-session
oneshot/notify registered when the task spawns) and then re-runs the pass. The v2 "don't
block on busy" was a deviation; a half-done producer still folds SOMETHING, and TS chose
waiting — match it.

## Slice split (R1-F8: coupling acknowledged, sequenced not parallel)
- Slice RP-A (P0+P1+P2+P3+P4): config expansion, scheduler wiring, selection wiring,
  durable substrate, agent-drop queue, `_decider` deletion. ONE slice — round 1 showed
  the pre-classify region couples all of them (scheduler gate → selection → reductions_
  pending → classify all sit in transform.rs:447-510); splitting invites merge damage in
  the most cache-critical block. Big but mechanical against this doc.
- Slice RP-B (P5): blocking arm. AFTER RP-A (needs the latch fields AND the emergency
  selection output for its degradation path).

## Cache invariants (unchanged, restated for the gate)
1. Defer passes replay byte-identical (selection NEVER runs on defer).
2. New reductions ride an already-busting pass only (execute/force gate = the TS
   BUST-clause; the scheduler IS the bust decision on this leg).
3. Frozen reductions are immutable + monotonic (existing validation, untouched).
4. The emergency latch prevents re-drops on the same input sample (idempotence).
5. Scheduler timing fields are meta-blob writes on ALREADY-committing passes only (a
   pure-defer pass with no other delta writes NOTHING — no new write-per-pass).

## Open questions (Oracle)
Q1: RESOLVED — durable queue, see P4.
Q2 (Oracle): the execute-with-no-delta pass — scheduler says execute, selection returns
zero decisions, no historian delta, no memory delta: the pass must land byte-identical
(defer-shaped). Verify no path in P1 forces a SOFT/HARD on an empty execute (e.g. writing
last_execute_ordinal must not bump m1_revision or any digest leg).
Q3 (Oracle): blocking-arm re-entrancy — the awaited firing publishes, the handler re-runs
apply_once in the SAME request: interaction with the single-flight latch, the busy
dedup, and the reattach path if the await times out mid-request.
Q4 (Oracle): last_execute_ordinal vs revert — a revert below the watermark makes ordinals
non-monotonic vs the stored watermark; the two-pass selector would then see stale-high
last_execute_ordinal and over-drop young re-exposed content. Does the re-cut arm need to
clamp these fields (last_execute_ordinal = min(last_execute_ordinal, new_coverage_end))?
