# P0 fix: shadow sender full-session hydration storm (26-44GB RSS)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. File: packages/plugin/src/hooks/magic-context/shadow-sender.ts (+ read-session helpers). This is memory-urgent: the user's OpenCode server hit 26-44GB RSS from this path.

## Verified mechanism (OC + source confirmed — do not re-derive, build on it)

The shadow sender runs on an ASYNC enqueue path where no transform-primed raw-message cache exists, so `withRawSessionMessageCache(() => readRawSessionMessages(sessionId))` performs a FULL session hydration: all message rows + ALL part rows + JSON.parse of every part (1.53GB on the largest session). Three call sites do this:
- `resolveOrdinalsForShadow` (~line 406) — needs ONLY Map<messageId, ordinal>.
- `resolveDeclaredTrimForShadow` (~line 562) — needs ONE message by id (boundary flat-block id), and is markerKey-cached anyway.
- the seed/state_sync compartment serialization (~line 782) — needs only the compartment boundary messages by id.

AND a deterministic infinite-waste loop on compacted sessions: MC's own compaction-summary messages (assistant, summary=true, finish="stop") are IN the wire array but deliberately FILTERED from the raw view before ordinal numbering (readRawSessionMessagesFromDb, read-session-raw.ts:~100). So the summary id can never resolve → `resolveOrdinalsForShadow` returns unresolved → pass skipped — AFTER paying the full hydration. Every pass, every compacted session, forever. (The provisional-tail logic added in a23d4772 only covers unpersisted SUFFIX holes; a summary row is a mid-array hole → fail-skip.)

## The fix (three parts)

### 1. Id-only ordinal reader (kills the hydration)
Add to packages/plugin/src/hooks/magic-context/read-session-raw.ts (and the -db module as appropriate, following the existing FromDb/cache split):

`readRawSessionMessageIdOrdinalsFromDb(db, sessionId): Map<string, number>` — SELECT id, data FROM message WHERE session_id=? ORDER BY time_created ASC, id ASC, filter OUT summary rows using THE SAME predicate as readRawSessionMessagesFromDb (info.summary === true && info.finish === "stop"; keep the JSON-parse-of-message-data — message rows are small, parts are the gigabytes; do NOT try to json_extract in SQL unless you verify byte-identical filter semantics with the JS predicate on edge shapes), then Map(id -> index+1). MUST produce ordinals identical to readRawSessionMessagesFromDb over the same rows — add a differential test: build a session fixture with summary rows, tool arcs, weird roles; assert the map equals {id: ordinal} derived from readRawSessionMessagesFromDb.

`resolveOrdinalsForShadow` switches to it (no withRawSessionMessageCache wrapper needed). Keep the by-id point-read fallback (readRawSessionMessageById) for below-floor/holes only if still needed for the ordinal — actually the id-only map is now complete for persisted messages, so the fallback collapses; keep the provisional-suffix logic for unpersisted live-tail messages exactly as-is.

### 2. Summary rows: excluded, not unresolved
In resolveOrdinalsForShadow: detect summary rows in the wire array (info.summary === true && info.finish === "stop" on the message info — same predicate) and EXCLUDE them from the annotated input array entirely instead of failing resolution. ALSO exclude them from ts_output before the compare payload is built (they are MC-internal boundary markers, pass-through in both lanes; the raw-view ordinal space already excludes them, so symmetric exclusion keeps both lanes consistent). Record the exclusion as a normalization entry (follow the existing normalizations mechanism — tag_prefix / ctx_search_hint pattern) so the module-side compare knows bytes were removed deterministically. Check the module side (crates/mc-module/src/lib.rs shadow arms) tolerates the new normalization kind string — if the Rust parser enumerates kinds, add the new kind + test; if it passes them through opaquely, just regenerate the wire fixture.

### 3. Point reads for the other two sites
- resolveDeclaredTrimForShadow: replace the full read with readRawSessionMessageById(sessionId, targetEndMessageId).
- Seed/state_sync compartment serialization (serializeCompartment rawById consumer): replace the full-map with per-id point reads of exactly the boundary message ids each compartment needs (look at serializeCompartment to see which ids it reads — likely start/end message ids for flat block ids and date resolution). A tiny per-call Map cache over the point reads is fine; NEVER the full session.

Also: the per-session pass queue — when enqueueing a new shadow pass and the queue already holds older UNSENT passes for the same session, drop the older ones (newest-wins). Only the newest pass is comparable anyway; queued stale passes each cost a full resolve cycle. Verify against the existing FIFO cap logic and keep the drop-oldest accounting consistent.

## Tests (non-vacuous)
1. Differential ordinal test (described above) — must fail if filter/order semantics ever drift from readRawSessionMessagesFromDb.
2. Compacted-session pass: wire array containing a summary row resolves OK, summary excluded from annotated input AND from ts_output, normalization recorded, pass SENT (not skipped).
3. No-full-read guard: instrument/spy that readRawSessionMessages (the full reader) is NOT called by resolveOrdinalsForShadow / declaredTrim / seed serialization paths (e.g. mock the module and assert zero calls on those paths). This is the regression pin for the whole incident.
4. Newest-wins queue: enqueue 3 passes for one session, assert only the newest survives.
5. Existing shadow-sender suite + wire fixture regenerated if the wire shape changed (bun packages/plugin/scripts/generate-shadow-wire-fixture.ts), Rust fixture test green (cargo test -p mc-module).

## Gates
cd packages/plugin && bun test, typecheck, lint. cargo test -p mc-module + clippy if the Rust side changed. check_comments clean — comments explain invariants (summary rows have no ordinal by design; ordinal reader must mirror the raw reader's filter; async path has no primed cache) with zero references to this incident or memory numbers.
