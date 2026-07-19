# Pi transform performance: isolated harness + full optimization pass (issue #224)

## The problem

GitHub issue #224 (user FurryWolfX, Pi 0.80.3 on WSL): the Magic Context Pi transform gets progressively slower as messages accumulate, lagging the whole TUI:

```
[2026-07-10T01:36:16.193Z] [magic-context][019f496c-...] transform completed in 3337.1ms (5725 messages, 4194 targets, watermark: 4122)
```

3.3 seconds per LLM round-trip, on the hot path, growing linearly (or worse) with session length. Ufuk sees the same slowness on his own machine.

The line comes from `packages/pi-plugin/src/context-handler.ts` — the Pi context handler that runs tagging + strips + drops + replay + injection on EVERY pass.

Prior art: note-#283-class analysis on the OpenCode side measured the tag loop at O(parts) per pass with extract+owner-derivation ≈ 60% of the cost and identified an incremental-tagging design (skip identity derivation for already-persisted stable parts, re-apply prefixes only). That analysis was parked because the path is cache-critical. Pi's shape is easier to isolate (plain messages array in, mutated array out), which is why we're doing the Pi side first, in isolation, with a mechanical byte-identity gate.

## Your job (two deliverables)

### Deliverable 1: an isolated perf harness (committed, reusable)

Create `packages/pi-plugin/scripts/experiments/perf/` with a harness that:

1. **Feeds the real Pi transform exactly what Pi feeds it.** Drive the real `context-handler.ts` entry (the same function the extension registers) with reconstructed event payloads — NOT a reimplementation. If the handler needs a `ctx` object, build a minimal faithful fake from the real types.
2. **Uses real session data as fixtures.** Load Pi JSONL sessions from a directory given by env var `MC_PI_PERF_FIXTURES` (default: `~/.pi/agent/sessions`). Ufuk's machine has several >5MB sessions (e.g. `--Users-ufukaltinok-Work-Projects-CortexKit-antigravity-auth--/2026-06-02T15-48-02-003Z_019e8905-*.jsonl`). DO NOT commit fixture data — the harness reads from disk at run time. For CI-committable smoke coverage, add a small synthetic generator (N messages with realistic part mixes: text, toolCall arcs, thinking, images) so the harness runs without private data too.
3. **Simulates accumulation.** Replay a session incrementally: pass 1 = first K messages, pass 2 = first K+step, ... so we measure the growth curve, not just one point. The handler must run against a REAL temp context.db (never the production DB — use MAGIC_CONTEXT_TEST_DATA_DIR isolation like the test suite), with tags accumulating across passes exactly as production does.
4. **Measures per-phase timings.** Instrument (via wrapping/hooks, not permanent edits to production code) the major phases: entry parse/branch resolution, tagging loop (identity derivation vs prefix re-apply vs targets rebuild), token counting/backfill, strips/replay (reasoning, images, placeholders, caveman), drop application, boundary/trigger checks, injection (m0/m1), and DB I/O. Output a table per pass: phase → ms, plus totals and the growth trend.
5. **Byte-identity gate (the core primitive).** The harness must be able to run TWO builds of the transform (baseline vs optimized) over the same fixture + same temp-DB seed and assert the output message arrays are DEEP-BYTE-IDENTICAL on every pass (canonical JSON serialization compare), and that the persisted tag rows match. This gate is what makes optimization safe. Make it a single command: `bun scripts/experiments/perf/compare.ts --fixture <path>`.

### Deliverable 2: the optimization work itself

Profile first, then optimize what the data says. Candidate directions (verify against YOUR profile, do not assume):

- **Incremental tagging** (the #283 design): parts already bound in the persisted tag map need no re-derivation — skip extract + tool-owner FIFO for messages below the live tail; re-apply known §N§ prefixes + rebuild target closures only. Tool parts key on composite (callId, owner) via stateful FIFO across messages — you need a reverse lookup (entryId/callId → tagNumber) for stable parts. Text/file parts key cheaply.
- **Allocation churn**: per-pass `parts.filter`/`.some` allocs, prefix string rebuilds, JSON re-parses of entry data that could be memoized on entry identity (Pi entries are stable objects — reference-keyed WeakMap memoization is plausible).
- **Token-count backfills**: should be write-once per tag (they're already persisted); verify no per-pass re-estimation happens on stable parts.
- **DB round-trips**: batch reads/writes; check for per-part statements that should be per-pass.
- **Caveman/strip replay**: persisted-depth replay should be O(changed), not O(all).

Constraints:
- Output must stay byte-identical — the compare gate proves it on every fixture. If an optimization would change bytes, it is WRONG (cache stability: old messages must render identically pass-over-pass).
- No behavior changes: same tags minted, same numbers, same DB rows (modulo timing columns).
- No new config knobs. No feature flags — this is a pure perf fix, it ships on or off the merge.
- Keep production-code changes surgical and well-commented (comments explain the invariant, not the history).
- The full Pi suite (`cd packages/pi-plugin && bun test`) must stay green, plus `bun run typecheck` and lint at repo root standards.
- Do NOT touch packages/plugin (OpenCode side) beyond shared code that Pi imports — if a shared-core change helps both, that's fine, but the OpenCode plugin suite must also stay green then.

## Report (final message)

1. Baseline growth curve: ms per pass at 500/1000/2000/4000/5725-equivalent messages (largest fixture available), per-phase breakdown at the largest size.
2. Optimized curve, same points, same fixtures.
3. Headline: X ms → Y ms at the issue's scale (5725 msgs), and the asymptotic story (O(parts) → O(tail)?).
4. Byte-identity proof: compare.ts output across all fixtures (pass count, zero diffs).
5. Any optimization you evaluated and REJECTED, with the reason (especially anything rejected for byte-identity risk).
6. Anything you found that is inherently O(parts) and cannot be removed (be honest — the OpenCode analysis found prefix re-apply is inherent).

## Verification expectations

- Byte-identity compare green on the synthetic corpus AND (on this machine) the real fixtures.
- Full Pi suite green, typecheck green.
- New tests: at minimum, a regression test pinning the incremental-tagging correctness (a session where a mid-array part's identity WOULD have been re-derived differently if the skip logic were wrong — e.g. duplicate callIds across turns — must produce identical tags to the full derivation).
- Commit in logical units (harness first, then each optimization separately) so the diff-review can attribute wins.
