# Task: mc-module durable reject/receive trace (observability, non-CAS)

## Why (incident-driven)

A production session wedged for 9 minutes of turns and the module's durable state could
not answer "did the module receive transform requests, and if so what did it return?" —
because a rejected pass commits NOTHING by design (TransformError leaves the CAS
unadvanced; correct for cache-state) and mc-module writes no stderr. Every
TransformError currently vanishes. This task adds a durable trace WITHOUT touching pass
semantics.

## The fix (crates/ only: mc-store + mc-module)

1. **mc-store**: new table `mc_pass_trace` (migration, follow the existing migration
   chain pattern): columns `session_id TEXT PRIMARY KEY`, `last_received_at_ms INTEGER`,
   `last_completed_at_ms INTEGER`, `last_reject_error TEXT NULL`,
   `last_reject_at_ms INTEGER NULL`, `reject_count INTEGER NOT NULL DEFAULT 0`,
   `receive_count INTEGER NOT NULL DEFAULT 0`. One row per session, UPSERT semantics.
   Writes are PLAIN single-statement upserts — deliberately OUTSIDE the fenced
   CAS pass-commit transaction and never part of it (a trace write must succeed even
   when the pass errors, and must never contend with or extend the fenced commit).
   Store methods: `trace_pass_received(session_id, now_ms)`,
   `trace_pass_completed(session_id, now_ms)`,
   `trace_pass_rejected(session_id, error: &str, now_ms)` (error string capped at 2000
   chars), `load_pass_trace(session_id) -> Option<PassTrace>`.

2. **mc-module handler** (lib.rs, the transform dispatch arm — NOT the shadow arms):
   - on entry (after binding resolution, before transform): `trace_pass_received`
   - on Ok: `trace_pass_completed`
   - on Err (ANY TransformError, unconditional): `trace_pass_rejected` with the
     Display string of the error
   Trace failures are swallowed (log-and-continue is not available; just ignore —
   observability must never fail a pass). Shadow arms and facade arms are OUT of
   scope: shadow has its own divergence store, facade ops are not passes.
   The historian self-exemption pass-through (mc-historian: namespace) must NOT write
   traces (it is not a real session pass).

3. **Read surface**: extend the existing diagnostics/status op (find how
   HistorianDurableState/last_no_fire is exposed — there is a status/diagnostics arm)
   to include the pass trace for the session, so the trace is readable via the daemon
   without sqlite access.

## Tests
- Reject writes trace: drive a transform that hits a TransformError (e.g. ordinal
  violation fixture) → mc_pass_trace row has the error string, reject_count=1,
  received_count=1, completed unchanged; CAS row_version UNADVANCED (prove the
  trace is outside the CAS).
- Success writes received+completed, reject fields untouched.
- Repeated rejects increment the counter and overwrite last_reject_*.
- Self-perpetuating scenario (the incident shape): N sequential failing passes → N
  rejects traced while cache state stays frozen at the pre-failure row_version.
- Historian-namespace pass-through writes NO trace.
- Migration: fresh store has the table; existing store gains it.

## Gates
cargo test -p mc-store, cargo test -p mc-module --lib, cargo test -p mc-module --test
real_daemon, clippy -D warnings, fmt, check_comments.

## Rules
- Base: subc-migration HEAD.
- Comments explain WHY the trace is outside the CAS and why failures are swallowed.
- No changes to TransformError semantics, no recovery arms (separate design work).
- Commit trailer: Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>
