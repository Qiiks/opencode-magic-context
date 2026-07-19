# Mason: implement shadow seed paging (spec v2)

Branch from `subc-migration` HEAD. Implement EXACTLY the design in `.alfonso/plans/shadow-seed-paging.md`
(v2, 108 lines — read it fully first; it was Oracle-BLOCK-hardened then SUBC source-passed, every line
is load-bearing). This is a concurrency-critical fix on the fail-open, default-off shadow-mirror lane
that already caused a 26GB RAM leak once — leak paths and cross-generation torn-snapshot paths are the
highest-risk areas. Do NOT deviate from the spec; if you believe a spec step is wrong, STOP and report
rather than improvise.

## What & why
Cold-start shadow `state_sync` seed packs every compartment (content + p1..p4) into one request body,
busting our own `MAX_FACADE_FRAME_BYTES = 1 MiB` guard (crates/mc-module/src/lib.rs:3404, enforced in
handle() ~3234). Fix = page the seed into <=512 KiB batches, module reassembles in-memory, atomic apply
ONLY on the final `seed_complete` batch. The store's `apply_shadow_state_sync` (mc-store) stays
BYTE-FOR-BYTE UNCHANGED — paging is pure handler-side reassembly + a per-session state machine.

## Two source-verified invariants you must honor (do not re-litigate)
1. `handle_shadow_transform_value` gates only on durable `shadow_generation` (lib.rs:2456) — NO
   pending-seed awareness. So the state machine MUST reject `shadow_transform` + competing shadow
   mutators with `shadow_seed_in_progress` while a seed is in flight. Gate ONLY shadow-lane ops, NEVER
   the production compaction transform (separate handler) — wire it narrow.
2. `apply_shadow_state_sync` UPSERTS compartments (no delete of omitted rows, mc-store:2438-2443);
   only memories/workspace/mutations are `replace_*`. So compartment completeness depends on the
   preceding `reset_shadow_session` wiping the lineage — **reset-before-seed is load-bearing**. Add a
   test proving a paged seed WITHOUT a preceding reset leaves stale compartments.

## Implementation checklist (from spec — all required)
RUST (crates/mc-module/src/lib.rs, handle_shadow_state_sync_value):
- Route on seed-envelope presence: absent → existing single-shot path UNCHANGED; present → staging.
- Per-session state machine: Idle | AwaitingSeed{seed_id,gen,seq,total} | Collecting{next_index,
  digests,bytes} | Applying. Armed only by a COMMITTED shadow_reset.
- Index 0 starts ONLY when seed_generation AND expected_shadow_seq equal durable ModuleMeta (not a
  relative buffer compare). Pin {seed_id,gen,seq,total} for the attempt; every batch must exactly match.
- Ordering: index==next_expected required; forward gap → reject loud + discard (never hold).
- Idempotency: per-accepted-batch content digest; redriven index<next_expected = no-op ONLY if digest
  matches (same index diff bytes → discard + fail loud). ONE bounded CompletedSeed{seed_id,final_digest,
  gen,seq,total,result} per session; a batch with seed_id==CompletedSeed.seed_id AND matching digest
  returns the ORIGINAL ack INDEX-AGNOSTIC (the final-batch redrive after buffer-clear is the real
  lost-ack race — must no-op, not reject as a stray seed_complete).
- HANDLER-WIDE byte accounting: process-wide total_staged_bytes + max pending-seed count; per-seed cap
  <= handler cap (32 MiB). Exceed → discard + error-loud shadow_seed_buffer_overflow. RELEASE accounting
  on EVERY exit: final success, ANY failure, protocol/order/overflow reject, last-route teardown
  (lib.rs:3208-3231 must drop PendingSeed + release bytes), defensive channel rebind (bind_route ~819).
- seed_complete: drain buffer+final → build SAME ShadowStateSyncRequest with the PINNED expected_seq
  (never the final batch's unverified value) → EXISTING store.apply_shadow_state_sync → record
  CompletedSeed → clear buffer+accounting → Idle → return existing {ok,gen,seq,row_version}. Workspace
  prep + path conversion happen HERE (after full assembly with the final workspace map), NOT per-batch.
- Intermediate success: ack {ok:true,staged:true,next_expected_index}. NO store touch, NO seq advance.
- All-or-none envelope validation: total>=1, index<total, seed_complete==(index+1==total) exactly;
  scalar tail (seed_boundary_id, workspace, last_todo_state, acked_watermarks) on FINAL batch only;
  scalar-tail on intermediate → reject; partial envelope → invalid_params.

TS (packages/plugin/src/hooks/magic-context/shadow-sender.ts):
- Add seed batch fields to the state_sync wire (seed_id, seed_generation, expected_shadow_seq,
  seed_batch_index, seed_batch_total, seed_complete). acked_watermarks must go in the FINAL wire params
  (currently TS-local only).
- buildStateSyncPayload on a force seed: accumulate items into batches, flush when the next item would
  breach 512 KiB measured on the EXACT flat body: Buffer.byteLength(JSON.stringify(toFlatWireBody(batch)))
  incl envelope + final scalars. Single item / scalar tail that can't meet 512 KiB → FAIL LOUD (throw
  to the fail-open catch). Oversized legacy incremental delta that would hit 1 MiB → reset + rebuild as
  a paged seed.
- ENTIRE batch loop runs inside ONE dequeued processPass (no enqueue-per-batch, no Promise.all).
  Sequential awaited client.request. Capture generation+expected_seq ONCE, hold across all batches.
  Intermediate ACKs must NOT update lastAckedSeq/watermarks — ONLY the final ACK may.
- Seed clock active THROUGH final ACK; check remaining budget before/after every await; cap each request
  timeout to remaining budget. On budget/EBUSY/abort/ANY mid-loop failure: closeSession + mark
  reset-required + never leave the route silently open. Fresh seed_id per attempt (retry with different
  rebuilt contents = new seed_id).

## Tests (all from spec — non-vacuous, must FAIL if the mechanism is wrong)
Rust + TS as applicable: paged N-batch seed == single-shot seed (golden compartment byte-compare AFTER
reset); reset-before-seed leaves stale compartments without a reset; concurrent shadow_transform 2nd
route mid-seed REJECTED (shadow_seed_in_progress) + mirror uncorrupted; reader mid-seed sees prior/empty
never half-seed; aggregate handler-wide cap across MULTIPLE sessions discards+loud no OOM; every release
path frees total_staged_bytes back to 0 (final, mid-loop fail, overflow, route close, rebind); redriven
intermediate same digest = no-op, same index diff bytes = discard+loud; final-batch redrive after
buffer-clear = original ack via CompletedSeed; stale/future generation at index 0 does NOT allocate or
disturb an existing buffer; seq advanced between batches → final apply uses PINNED seq; all-or-none
envelope rejections; body-size invariant on exact flat bytes; restart mid-seed → buffer+receipt gone,
mirror unflipped (integration-level or documented if not unit-testable).

## Gate before reporting done
- cargo test -p mc-module -p mc-store -p mc-core (+ real_daemon), cargo clippy --workspace -D warnings, cargo fmt --check
- cd packages/plugin && bun test src/hooks/magic-context/ && bunx tsc --noEmit && bunx biome check
Report the exact test names you added and which spec negative each proves. Commit to subc-migration with
co-author trailer `Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>`. Do NOT
rebuild the ck-mc binary or deploy — I handle deploy after a SUBC post-implementation source-pass.