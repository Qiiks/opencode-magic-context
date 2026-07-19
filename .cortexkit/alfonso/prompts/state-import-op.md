# state.import — bootstrap-seed a real session's durable compartments (mc-module)

Build a new management op `state.import` on the MC Rust module (crates/mc-module + mc-store, branch subc-migration). It seeds durable COMPARTMENTS for a NEW session key from an exported bundle, so an imported session's first transform pass bootstrap-HARD-folds into m0 instead of assembling the full uncompacted history.

## Why this shape (context you must respect, not reinvent)
The transform is a pure function of (durable compartments, project memories, live array). The existing bootstrap arm (`hard_fold_requested = boundary_id.is_empty() && has_compartments`) already turns seeded compartments into a first-pass HARD fold that mints the boundary from the live array. Therefore the import surface carries compartments ONLY — no cache-state, no reductions/tags/pending drops, no memories (project-scoped, carry automatically), no historian runtime state. Do not add any of those to the payload.

## Op contract (wire, ToolProvider management route — follow agent_drops.append's dispatch pattern in lib.rs)
Request (one or more batches):
{
  "kind": "state_import",            // dispatch arm alongside existing management ops
  "v": 1,
  "session_id": "<REAL session key>",
  "import_id": "<opaque bundle digest from the caller>",
  "batch_seq": <0-based>,
  "batch_count": <total>,
  "compartments": [ { per-compartment payload, see below } ]
}
Response per non-final batch: { "ok": true, "staged": <count> }.
Final batch commits atomically and responds { "ok": true, "imported": <total>, "duplicate": false }.

Per-compartment payload:
- seq: i64, strictly increasing across the WHOLE bundle (validate across batches)
- start_message / end_message: i64 ordinals in the IMPORTED mid basis (m<ordinal>)
- end_message_id: flat block id in the imported basis ("m12#0") — becomes the fold's boundary-anchor candidate for the LAST compartment; structural validation only (parseable mid#idx, non-empty); liveness is enforced later by the existing mint-absent guard at first fold
- title: string (will be sanitized by the renderer as usual — do NOT sanitize on ingest)
- p1..p4: tier texts; p1 required, p2-p4 nullable (legacy rows arrive as p1-only)
- importance: i64 (default 50 if absent), episode_type: optional string
- start_date / end_date: optional ISO date strings (renderer's temporal fields)

## Semantics (each is a MUST, with a test)
1. BOOTSTRAP-ONLY: hard-refuse (error code "session_not_empty") if the session key has ANY existing durable state — cache_state row, compartments, tags, pending drops, ledger rows. Import never merges.
2. IDEMPOTENT by import_id: a durable record of a completed import (store it in the same fenced txn); re-submitting the same import_id returns { ok, imported, duplicate: true } without touching state; a DIFFERENT import_id against a now-non-empty session hits rule 1.
3. PAGED: reuse the ShadowSeedCoordinator mechanics (session-scoped staging, batch_seq contiguity check, per-batch staging under the 1MiB frame cap, aggregate byte cap, staleness eviction) — but this is NOT the shadow lane: no shadow_generation, real session key, and the final-batch commit writes through the SAME fenced-transaction mold as publish_historian_chunk (row_version CAS; the commit creates the cache_state row via the normal bootstrap INSERT path or leaves boundary_id empty for the first fold to mint — match what replace_compartments does for a fresh key).
4. VALIDATION (structural, reject the whole bundle on first violation): seq strictly increasing; start<=end per compartment; ranges non-overlapping and ordered; p1 non-empty; end_message_id parseable as mid#idx. Return error codes that name the violated rule.
5. NO side effects on reject or on staging failure: staged batches for an import that never completes are evicted by the coordinator's staleness rules, and a rejected final batch leaves the session empty.

## Tests (non-vacuous — each would fail if its mechanism were removed)
- happy path: 2-batch bundle for a fresh key → commit → compartments readable, boundary_id empty, has_compartments true; then a transform pass over an imported-basis live array bootstrap-HARD-folds and mints the last compartment's end anchor (drive through the existing transform test helpers).
- session_not_empty: seeded key (any prior commit) refuses with zero writes.
- idempotency: same import_id twice → duplicate:true, row_version unchanged; different import_id after success → session_not_empty.
- batch gap: batch_seq skip rejects; staged partial import evicted (staleness or explicit) leaves the key importable.
- structural rejects: overlapping ranges / non-increasing seq / empty p1 / malformed end_message_id each name their rule.
- legacy shape: p1-only compartments import and render (reuse existing legacy-row render tests as the model).

## Rules
- Follow the existing dispatch/error-shape conventions in lib.rs (HandlerOutcome::Error codes, respond(json!)).
- mc-store gets the new staging/commit primitives; migration only if a new table is genuinely needed (prefer reusing the shadow staging table ONLY if it is cleanly separable — if reuse would entangle shadow semantics, add a dedicated mc_import staging table with the next migration number and update LATEST tests accordingly).
- cargo test -p mc-module -p mc-store green, clippy/fmt clean, real_daemon test passes.
- Comments explain invariants for a future reader; no U-numbers, no seat names, no plan references.
- Commit in your worktree with clear messages. Do not touch packages/ (TS).
