# Fix round: Oracle findings on the Pi tag-reuse perf optimization

Branch: `subc-migration` of ~/Work/Projects/CortexKit/magic-context (merge 2caf9279 landed the optimization). An adversarial cache-safety Oracle returned REVISE with three findings. Fix all three with regression tests, keeping the perf win intact (identity reuse + DB-op reduction stay; only the unsafe skips go).

## Finding 1 (High): stale-negative fallback-adoption gate

`packages/pi-plugin/src/context-handler.ts:3779-3808` probes `hasPiFallbackMessageTags` / `hasPiFallbackToolOwnerTags` ONCE before fingerprint construction and passes the booleans into `adoptPiFallbackTags` (`:1447-1467`), where an explicit `false` suppresses the callee's own authoritative query and returns before the `BEGIN IMMEDIATE`.

Race: a sibling connection (multi-process Pi on the same session, which the tagger supports via `data_version` refresh — `packages/plugin/src/features/magic-context/tagger.ts:669-690`) inserts a `pi-msg-*` fallback row between the probe and adoption. The optimized pass allocates a real-ID tag instead of adopting; next pass, collision adoption keeps the fallback row and deletes the real duplicate (`packages/plugin/src/features/magic-context/storage-tags.ts:1246-1283`) — the §N§ prefix flips between passes. Cache-stability violation.

FIX: the passed booleans must never be authoritative when negative. Either (a) drop the boolean plumbing entirely and restore the callee's unconditional probes (they run per pass but are cheap indexed lookups — measure before assuming they were the cost), or (b) keep the fast-path ONLY when the boolean is true, and on false re-probe inside `adoptPiFallbackTags` before deciding to skip. Whichever you pick, the decision-to-skip must be made against a read that happens-after the fingerprint map was built.

TEST: simulate the interleave — build the fingerprint map, insert a fallback row via a second DB connection, then run adoption; assert the fallback tag is adopted (same tag id persists) rather than a real-ID duplicate being allocated.

## Finding 2 (Medium): reuse path skips grown-result accounting

Project rule (durable memory): tool tagging MUST bump the tag's byte size and token counts when a later occurrence carries a larger payload than the initial occurrence.

The reuse path violates this two ways (`packages/plugin/src/shared/tag-transcript.ts`):
- `canReuseIdentity` skips `readAggregateToolAccounting` and all size/token updates (`:293-350`).
- Direct reused-result aggregates install `maxByteSize = Infinity` (`:376-390`), so even a later non-reused occurrence can never bump.

Subtlety that makes this reachable: reuse status is decided from the TRANSCRIPT message id (`:204-206`), but Pi folds preceding tool results into the FOLLOWING user message (`packages/pi-plugin/src/transcript-pi.ts:255-280`, id = the user entry id `:339-376`) — so a tool result inherits the fold-target user's reuse status, not its own entry's.

FIX: keep identity reuse (tag id from `tagger.getToolTag`), but restore accounting on reused results with a cheap staged guard: always read the result text + byte size (cheap); ONLY when byteSize > persisted maxByteSize, run BPE token counting and the DB writes. Seed the aggregate's `maxByteSize` from the PERSISTED tag row value (read it alongside `getToolTag` — add a joined read if needed rather than a second query), never `Infinity`. The steady-state cost is one getText+byteLength per reused result; BPE only on genuine growth (rare).

TEST: reuse-vs-derive equivalence for a grown folded result — pass N tags a result at size S, pass N+1 presents the same stable ids but the result grew to S' > S; assert the optimized path persists the same byte/token bump full derivation would.

## Finding 3 (Low, hardening): token-cache false-hit guard + reuse-set scoping

- `safeJsonStringify` is plain JSON.stringify (`packages/pi-plugin/src/tokenize-pi-messages.ts:205-210`). Key-order differences are safe (miss). But a custom `toJSON` or prototype-backed/non-enumerable `role`/`content` can produce EQUAL JSON while the tokenizer reads different values via property access. FIX: bypass the cache (treat as no-stable-id) when the message is not a plain object (`Object.getPrototypeOf(m) !== Object.prototype && !== null`) or has a `toJSON` anywhere the tokenizer reads. Core Pi messages are plain-object JSONL rebuilds, so the fast path keeps its hit rate.
- `taggedStableMessageIdsBySession` only grows until session end (`packages/pi-plugin/src/context-handler.ts:2314-2315`, cleanup `:5153`). FIX: replace the grow-only union with "the previous successful pass's live real ids" (rebuild the set from `strictEntryIds` each success instead of unioning). This also means a branch-switch entry that disappears and REAPPEARS takes the full derivation path once — safer and bounded.

TESTS: toJSON-bearing message bypasses the cache (recount happens); reuse set does not contain ids absent from the latest successful pass.

## Gates (all must pass)

- `cd packages/pi-plugin && bun test` and `cd packages/plugin && bun test` — green.
- `bun run typecheck` both packages; repo lint standards.
- Re-run the byte-identity comparator over the real corpus: `cd packages/pi-plugin && bun scripts/experiments/perf/compare.ts --fixture ~/.pi/agent/sessions --step 5000` — zero diffs.
- Re-run the headline benchmark (`bun scripts/experiments/perf/benchmark.ts --synthetic --messages 5725 --step 500`) and report the new numbers — quantify how much of the 577→389ms win survives the accounting restore (expect most of it; the win was identity+DB-ops, not accounting reads).
- check_comments clean. Comments must explain invariants (why negative probes can't be trusted, why maxByteSize seeds from the persisted row), never reference this review or finding numbers.
