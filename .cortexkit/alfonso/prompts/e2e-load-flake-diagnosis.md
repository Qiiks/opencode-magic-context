# Task: Diagnose the OpenCode e2e release-gate flake (REPORT FIRST, do not fix yet)

You are investigating why the `packages/e2e-tests` OpenCode suite fails during release
gate runs. This is a **diagnosis-first** task: your first deliverable is a written
report of your findings. **Do NOT edit any files or commit anything on this pass.**
After I verify your finding I will reprompt you to implement the fix.

## The symptom (evidence gathered across 6 release-gate runs today)

`scripts/release.sh 0.32.1` runs the full gate. The plugin unit suite, Pi suite, and
CLI suite pass green EVERY run. The OpenCode e2e suite (`packages/e2e-tests`, run via
`bun test` over ~43 tests in ~18-22 files) fails on most runs, and the failures are
ALWAYS timeouts or `ECONNRESET`, NEVER assertion mismatches. Representative failing
tests and their timeouts:
- `deferred compaction marker (plan v6) > writes pending blob ...` timed out after 90000ms
- `emergency >=95% > historian is invoked when usage crosses 95%` timed out after 120000ms
- `long-running OpenCode Magic Context session > exercises execute, notes, reduce ...` ~129712ms
- `historian success path > publishes a compartment ...` timed out after 120000ms
- `e2e smoke > sends a prompt ...` timed out after 60000ms
- `session.create failed after 5 attempts`
- `The socket connection was closed unexpectedly ... ECONNRESET` (unhandled, between tests)
- `sendPrompt did not complete within 180000ms`

CRITICAL DATUM: when the machine was briefly idle (a peer paused its CPU-heavy
background job for ~25 min), the SAME suite ran **42/43 pass** — the one failure that
time was a genuine assertion bug in `overflow-recovery.test.ts` which is already fixed
and committed (`0087ee81`). Every other run happened while the box was at load average
20-50 (the user's many OpenCode editor instances + peer masons compiling + Time Machine).

So the working hypothesis is: **the e2e gate is not load-tolerant** — it spawns real
OpenCode server processes and drives real-prompt turns through the transform/historian
pipeline against a mock provider, and under CPU contention the warmup / `session.create`
/ `sendPrompt` steps exceed their fixed timeouts. But I want you to CONFIRM OR REFUTE
that with source evidence, and find the SPECIFIC structural weakness, not just restate it.

## What to investigate

Work in your worktree checkout. The e2e harness lives in `packages/e2e-tests/`:
- `src/harness.ts` — `sendPrompt` (retry logic, timeout), warmup, `session.create` retry
- `src/opencode-runner/spawn.ts` — how the OpenCode server process is spawned/configured
- `src/mock-provider/` — the mock anthropic provider (are calls actually local/instant?)
- The failing test files under `tests/`
- `scripts/release.sh` and any e2e runner config — **does the suite run test FILES in
  parallel?** `bun test` parallelizes across files by default. If N test files each spawn
  their own OpenCode server simultaneously, they compete for CPU and all slow down
  together — that would explain why timeouts cluster and why an already-loaded box tips
  them over the edge. Check whether there's concurrency control (serial execution,
  `--concurrency`, a lock, or per-file server reuse) and whether the servers are
  heavyweight (real opencode binary + plugin + historian child sessions).

## Questions your report MUST answer

1. Are the timeouts structural-and-fixable, or genuinely "cannot run under load"? Cite
   the exact timeout constants and where warmup/spawn/prompt time actually goes.
2. Does the suite run servers in parallel? How many concurrent OpenCode processes at peak?
   Is that the multiplier that makes a moderately-loaded box fail?
3. Is there a robustness fix that would make the gate reliable WITHOUT masking real
   failures? Candidates to evaluate (do not implement, just assess): serializing e2e
   files, bounding concurrency, more patient/adaptive warmup with health-poll instead of
   fixed sleep, `session.create`/`sendPrompt` retry-with-backoff already present vs
   missing, ECONNRESET being an unhandled between-tests reject that should be caught.
   For EACH candidate say whether it fixes the root cause or just widens a timeout
   (widening timeouts to mask load is NOT acceptable — the user rejects that class of
   fix; a real fix reduces contention or makes the wait event-driven rather than racing
   a fixed deadline).
4. The single most surgical change that makes the release e2e gate pass reliably on a
   moderately-loaded dev box while still failing loudly on a REAL regression.

## Output

A concise report: root cause with file:line citations, the concurrency finding, and a
ranked fix recommendation. **No code changes this pass.** Stop after the report.
