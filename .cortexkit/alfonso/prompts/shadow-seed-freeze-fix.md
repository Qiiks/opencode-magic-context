# P0: shadow lane freezes the OpenCode event loop on cold-start seed (second recurrence)

Branch: work from `subc-migration` HEAD. All paths relative to repo root.

## Incident (evidence-backed, do not re-litigate)

With `shadow_transform.enabled=true`, restarting OpenCode Desktop froze the entire process
(~20 project instances share one event loop; session lists unloadable) for 105+ seconds.
MC log went fully silent 21:32:32→21:34:17Z immediately after the first primary transform of a
large session (1,304 compartments / 75,995 raw messages) enqueued its first shadow pass.

Root cause A (the freeze): cold-start seed path.
`processPass` → `buildStateSyncPayload` (shadow-sender.ts:776) serializes EVERY compartment;
`serializeCompartment` point-reads boundary messages via `readRawSessionMessageById` →
`readRawSessionMessageByIdFromDb` (read-session-raw.ts:494), which computes the ordinal with a
`COUNT(*) … json_extract(data,…)` scan over ALL session message rows. Measured on the live DB:
ONE such query = 35.9s cold (75,799 rows). Seed runs O(compartments) of them, synchronously
(bun:sqlite is sync), on the event loop. Everything freezes.

Root cause B (per-pass tax, same class): `resolveOrdinalsForShadow` (shadow-sender.ts:415) calls
`readRawSessionMessageIdOrdinals` on EVERY shadow pass → `readRawSessionMessageIdOrdinalsFromDb`
(read-session-raw.ts:228) does `SELECT id, data` over the whole session and JSON-parses every
row's `data` twice (filter + forEach). 76k rows parsed per pass.

Also note (fix if cheap, do not restructure): `cloneForShadow` in transform.ts:461 does
JSON.parse(JSON.stringify(messages)) of the full wire array synchronously on the transform hot
path every pass. Leave the clone (correctness: snapshot before mutation) but do NOT add new work
to that site.

## Required fixes

### 1. Seed must be O(N + C), not O(N × C), and must not block the loop
- Add a parts-only point read that does NOT compute the ordinal COUNT: either a new
  `readRawSessionMessagePartsById(sessionId, messageId)` in read-session-chunk.ts (provider-aware
  like its siblings) backed by a `readRawSessionMessagePartsByIdFromDb` (message row + part rows,
  both indexed lookups, NO COUNT query), or an options flag on the existing reader. The seed does
  not need the recomputed ordinal at all: `serializeCompartment` already has the compartment's
  stored `startMessage`/`endMessage` ordinals; it needs the raw message only for
  `flatBlockCountForRawMessage` (part shapes) and id validation.
- Thread it through `serializeCompartment` and `resolveDeclaredTrimForShadow` (both currently go
  through the COUNT-bearing reader).
- Make the seed compartment loop yield: `buildStateSyncPayload` becomes async (or the compartment
  serialization is chunked by the caller) with `await new Promise(setImmediate)`-class yields every
  ~25 compartments so a 1,300-compartment seed cannot starve the loop. `processPass` is already
  async; propagate.

### 2. Per-pass ordinal map must be incremental
- Keep the per-session `idOrdinalMemo` as the source of truth once primed. Replace the every-pass
  full `readRawSessionMessageIdOrdinals` with:
  (a) prime once per generation (the existing full read, acceptable one-time cost, but run it
      inside the async queue with the same yield discipline — chunked page reads via the existing
      paged reader if straightforward);
  (b) on subsequent passes, fetch only rows NEWER than the highest memoized (time_created, id)
      anchor (indexed range read), parse only those for the summary-row filter, and append to the
      memo;
  (c) drift detection: an index-only `SELECT COUNT(*) FROM message WHERE session_id=?` (no
      json_extract) compared against memoized+new counts. On mismatch → clear memo, return
      {ok:false, reason:"mismatch"} so the existing shadow_reset self-heal path fires. This keeps
      the revert/delete detection the memo previously got from the full re-read.
- The memo/generation semantics (clear on generation change, mismatch → reset) must be preserved
  exactly; do not weaken the drift check to "never mismatch".

### 3. Belt: seed work budget
- Wrap the whole seed (performReset + first state_sync build) in a wall-clock budget (~30s). On
  exceed: log one loud line (`shadow: seed budget exceeded, lane disabled for session`), set
  `state.skipped = true` (the existing permanent-skip latch), increment a counter. A shadow lane
  that cannot seed cheaply must fail ITSELF, never the host. This is the structural guarantee the
  incident demands: no future seed-path regression may freeze the host again.

## Non-goals
- No live-lane (non-shadow) behavior changes. `readRawSessionMessageByIdFromDb` keeps its current
  semantics for existing callers (the COUNT is fine for single lookups on the live paths that use
  it today); you are adding a cheaper variant, not changing them.
- No transport changes (timeouts/queue caps from bg_a5754690 stay as-is).
- No module-side (Rust) changes.

## Tests (non-vacuous; each must fail on the pre-fix code)
1. Seed cost test: fixture session with ~200 compartments; instrument the DB layer (test seam or
   prepared-statement counter) asserting the seed performs ZERO ordinal-COUNT queries and at most
   O(C) part point-reads + ONE full ordinal read.
2. Event-loop test: during a large seed, an interval timer must keep firing (max observed gap
   under some generous bound, e.g. <250ms) — proves the yields.
3. Incremental ordinal test: pass 1 primes the memo; pass 2 with 3 appended messages performs only
   the range read (assert via seam) and resolves correct ordinals; a mid-history deletion flips the
   COUNT drift check → {ok:false, mismatch} → reset path.
4. Seed budget test: a seam-injected slow serialize trips the budget → state.skipped=true, one log
   line, no throw to the caller.
5. Existing shadow-sender tests stay green (byte-identity of built payloads for small sessions:
   same state_sync/shadow_transform bodies as before for a fixture that fits in one chunk).

Run: `cd packages/plugin && bun test src/hooks/magic-context/shadow-sender.test.ts` plus the full
hooks dir, `bunx tsc --noEmit`, `bunx biome check`. Commit with a clear message + co-author
trailer `Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>`.
