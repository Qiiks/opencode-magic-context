# compress-cues chunk-slice starvation fix

## Mechanism (the bug)

`runCompressCues` divides the remaining task deadline evenly across the chunks
still to run:

```ts
sliceMs = Math.max(1, Math.floor(remainingMs / (chunks.length - i)));
```

On a large backfill this hands each chunk far too little time. Live failure: a
470-memory pool selects 12 chunks of 40; with a ~20-minute deadline the even
split is ~100s per chunk. The compressor model (a slow thinking model) needs
longer than that, so **every** chunk times out at ~100s
(`prompt timed out after 99997ms`), contributes 0 cues, and the loop marches
through all 12 doomed chunks — burning ~20 minutes and the model quota, every
day. A model that is consistently slower than its slice starves forever across
daily retries, because nothing ever changes the slice it is given.

A chunk timeout was also indistinguishable from any other chunk failure: the
loop swallowed it and unconditionally continued to the next chunk.

## Fix

All changes are in `packages/plugin/src/features/magic-context/mural/compress-cues.ts`
plus its co-located test. No change to the manifest format, validator,
`setMuralCue`, tool schemas, or migrations.

### 1. Chunk time floor

- New exported constant `CHUNK_TIMEOUT_FLOOR_MS = 240_000`.
- New exported pure helper `computeChunkSliceMs(remainingMs, chunksRemaining)`:

  ```ts
  sliceMs = min(remainingMs, max(CHUNK_TIMEOUT_FLOOR_MS, floor(remainingMs / chunksRemaining)))
  ```

  The even split is used when it already exceeds the floor; otherwise the floor
  wins; the result is never more than the budget actually remaining.
- If `remainingMs < CHUNK_TIMEOUT_FLOOR_MS` **before a chunk starts**, the run
  stops there: progress already written is banked (cues are durable per memory),
  `result.complete` stays `false`, and the function returns. This is distinct
  from the existing `remainingMs <= 0` stop.

### 2. Consecutive-timeout circuit breaker

- `compressOneChunk` now returns a `ChunkOutcome` that classifies a failure as
  `"timeout"` vs `"other"`. Timeout-class is detected by the exact
  `prompt timed out after Nms` error thrown by `promptWithTimeout`
  (`shared/model-suggestion-retry.ts`); validation failures (bad/missing
  manifest, length-capped output) and provider errors are `"other"`.
- The run loop counts consecutive timeout-class failures. When
  `CONSECUTIVE_TIMEOUT_LIMIT` (2) is reached it stops immediately with a distinct
  log line naming the mechanism (model too slow for its time slice) and returns
  incomplete. A success or any non-timeout failure resets the streak, so
  validation failures keep the existing per-chunk retry-next-run behavior and do
  **not** trip the breaker.

### 3. Operator sizing info

When the breaker trips, the log includes the measured per-chunk elapsed for the
whole timeout streak plus the slice budget
(`per-chunk elapsed [..ms, ..ms] vs <slice>ms slice`), so an operator can size
chunk vs model. `compressOneChunk` measures elapsed with a `startedAt` timestamp
and reports it on the failure.

### Disposition path (verified, unchanged)

`task-executor.ts` (compress-cues arm, ~line 332) already records
`failed` + `transient: true` when `!result.complete`:

```ts
if (!result.complete) {
    const error = `compress-cues incomplete: ${result.remaining} selected memories remain`;
    recordRun("failed", error);
    return { status: "failed", transient: true, error };
}
```

So both new early-stop paths (floor stop and breaker) flow through the existing
incomplete → transient-retaining-cron machinery. **No change to task-executor was
needed.**

## Test evidence

Co-located `compress-cues.test.ts` extended (existing suite preserved):

- **(a) floor applied when even division would be below it** —
  `computeChunkSliceMs (chunk time floor)` describe: `computeChunkSliceMs(1_200_000, 12)`
  (the live 12-chunk shape, even split 100s) returns `CHUNK_TIMEOUT_FLOOR_MS`;
  also covers the even-split-wins and never-exceeds-budget cases.
- **(b) run stops banking progress when remaining < floor, complete=false** —
  `stops banking progress when the remaining budget falls below the chunk floor`:
  41 memories (2 chunks); first chunk banks 40 cues, then the deadline is dropped
  to >0 but <floor; asserts `compressed=40, remaining=1, chunks=1, complete=false`.
- **(c) two consecutive timeouts stop early; validation failures don't** —
  `two consecutive chunk timeouts trip the breaker; the third chunk is never attempted`:
  120 memories (3 chunks), timeout-throwing client; asserts `promptCalls === 2`
  (third chunk never attempted), `chunks=2, complete=false`. And
  `validation failures do not trip the timeout breaker`: bad-manifest client;
  asserts `promptCalls === 3` (all chunks attempted).
- **(d) existing green tests stay green** — the 3 existing disposition tests and
  the storage/applyCues suites still pass. (The shared `cueArgs` deadline fixture
  was raised from 60s to 600s so it sits above the new 240s floor; assertions are
  unchanged. At 60s the fixture is now below the floor, so no chunk would run.)

### Commands run

- `bun test src/features/magic-context/mural/compress-cues.test.ts` → **18 pass, 0 fail**.
- `bun test` (full `packages/plugin` suite) → **3231 pass, 0 fail**.
- `bunx tsc --noEmit` (src typecheck) → **exit 0, clean**.
- `bun run typecheck` → src `tsc --noEmit` clean; the second `tsc -p tsconfig.scripts.json`
  step reports pre-existing errors in `scripts/*` files (bench-synapse-vs-local.ts,
  generate-mural-font.ts, test-mural-render.ts, test-synapse-embed.ts) that this
  change does not touch — baseline failure, unrelated to compress-cues.

## Notes

- `bun install --force` was needed to hydrate the worktree's `node_modules`
  (isolated-linker symlinks were not materialized; `zod` was unresolvable before
  install). The incidental `bun.lock` rewrite from install was reverted; only the
  two source files are committed.
- A sidekick comment-clarity review flagged 2 comments, both in files this change
  does **not** touch (`crates/mc-module/src/codec/opencode.rs`,
  `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts`) — a stale
  diff baseline. No comment in the changed files was flagged.
