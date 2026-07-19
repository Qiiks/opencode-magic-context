# mc-module/mc-store: durable command-id idempotency for agent_drops.append

Branch from `subc-migration` HEAD. Consumer-driven contract extension (THALAMUS response-tee):
their Athena audit found agent_drops.append is not idempotent ACROSS CONSUMPTION — pending_agent_drops
INSERT OR IGNORE dedupes only while rows are pending; commit_with_consumed_drops deletes them, so a
lost-response retry after a drain re-inserts and re-applies. The consumer holds a stable tool_use_id.

## Contract (frozen with THALAMUS — implement exactly)
Request to the agent-drops management op (all method aliases) gains an OPTIONAL field:
  "command_id": string (trimmed, non-empty, cap 128 bytes; longer or empty-after-trim = bad_request)
Behavior when present, ATOMICALLY in the same transaction as the append:
  - Look up (session_id, command_id) in a durable ledger table.
  - HIT: return {"ok":true,"queued":0,"duplicate":true} — no inserts, no ledger touch, regardless of
    whether the original targets are still pending or long consumed.
  - MISS: record the ledger row and perform the normal append in the same transaction (both-or-neither).
Absent command_id: exactly current behavior (no ledger involvement, no response change).
Response stays {"ok":true,"queued":n} on the non-duplicate path (no new fields).

## Implementation
- mc-store: new table (e.g. mc_reduce_command_ledger: session_id, command_id, queued_at_ms,
  PRIMARY KEY(session_id, command_id)) via the store's normal migration mechanism (next version).
  Retention BOUNDED: on insert, prune rows for that session beyond the newest 512 (deterministic,
  same transaction). Ledger rows are removed with the session's other rows on session teardown —
  find the store's existing session-scoped cleanup and add the table to it.
- New store method: append_pending_agent_drops_with_command(session_id, command_id: Option<&str>,
  ids, now_ms) -> Result<AppendOutcome> where AppendOutcome { queued: u64, duplicate: bool } — the
  existing method stays and delegates with command_id=None (do NOT churn existing call sites).
- mc-module handle_agent_drops_value: parse/validate command_id, thread through, add "duplicate":true
  to the response only on the duplicate path.

## Tests (non-vacuous — each must fail if the mechanism is wrong)
1. append with command_id → retry same command_id while rows still pending → queued=0, duplicate=true,
   pending count unchanged.
2. THE AUDIT CASE: append with command_id → drain/consume the rows (commit_with_consumed_drops or the
   transform-drain test helper already used by ctx_reduce_command_appends_and_transform_drains_queue)
   → retry same command_id → queued=0, duplicate=true, pending stays EMPTY (no reinsertion).
3. Different command_id, same targets, after consumption → normal append (this is a genuine re-request).
4. command_id absent → behavior identical to today (both tests around raw "drop" string and drop_ids).
5. Ledger atomicity: a failed append (force store error if there's a seam; otherwise skip with a note)
   must not leave a ledger row (both-or-neither).
6. Retention: 513 distinct command_ids → oldest pruned, newest 512 present.
7. bad_request arms: command_id="" and command_id > 128 bytes.
8. Works combined with the raw "drop" string form (command_id + drop:"1-3").

## Gates
cargo test -p mc-store -p mc-module, clippy --workspace -D warnings, cargo fmt --check.
Commit with trailer Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>.
