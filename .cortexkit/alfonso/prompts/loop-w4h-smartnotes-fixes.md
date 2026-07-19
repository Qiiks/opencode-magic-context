# Fix batch: smart-notes sandbox hardening (W4-H verified findings)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration, packages/plugin/src/features/magic-context/smart-notes/ (+ dreamer/evaluate-smart-notes.ts, storage-notes.ts where named). Verified audit findings — verify each at source before fixing. DO NOT change the v1 capability design (readFile+httpGet together is the accepted design, documented in docs/AUDIT-KNOWN-ISSUES.md A50 — do not add allowlists or remove capabilities). DO NOT touch IPv6/NAT64 classification (banked separately).

## Fix 1 (HIGH): denylist must apply to the CANONICAL path — capabilities.ts guardedReadFile (~:102-138)
Today: isSecretDeniedPath(normalized) runs on the LEXICAL repo-relative path (line ~110); parent realpath (~117) is used for containment only. A repo directory symlink `public -> secrets` lets readFile("public/token.json") read secrets/token.json — the denied segment never reaches the denylist. O_NOFOLLOW only guards the final component.
FIX: after resolving parentReal, recompute the canonical repo-relative path (path.relative(rootReal, path.join(parentReal, path.basename(target)))) and re-run BOTH isSecretDeniedPath and containment on it. Lexical pre-check stays (cheap fast-reject). Tests: dir-symlink alias to a denied dir returns null; a benign dir symlink inside the repo still reads; final-component symlink still rejected via O_NOFOLLOW.

## Fix 2 (HIGH): non-regular files must be rejected BEFORE open; filesystem ops must not wedge the global mutex — capabilities.ts + sandbox-runner.ts
Today: open(O_RDONLY|O_NOFOLLOW) precedes the stat.isFile() check; opening a repo FIFO blocks until a writer appears, the sandbox timeout aborts the controller but the open never returns, the QuickJS context is never disposed, and every later smart-note run queues behind the process-wide withSandboxLock forever.
FIX: lstat the canonical target first and reject anything not a regular file (before open). Add O_NONBLOCK to the open flags where the platform supports it (harmless for regular files, prevents FIFO blocking as defense-in-depth). Race the whole guardedReadFile body against the run signal (Promise.race with an abort promise) so a hung filesystem op resolves the capability promise on abort — a late-resolving open must close its handle in a detached .then without touching anything else. Regression test: a FIFO in the project (use node's mkfifoSync alternative: spawn mkfifo, skip test on platforms without it) — assert the aborted run returns within its budget AND a subsequent run completes (lock released).

## Fix 3 (MEDIUM): note-state transitions must be compare-and-set — storage.ts, evaluate-smart-notes.ts, runner.ts, storage-notes.ts (Oracle cites storage.ts:60-97,135-164,173-239; evaluate-smart-notes.ts:125-143,235-280; runner.ts:53-58,91-120; storage-notes.ts:345-388,417-435)
Today: compile/due/liveness results update keyed only by (id, type); a user dismissing or editing the note mid-run gets their state overwritten (dismissed -> ready resurrection, newer ready_reason clobbered).
FIX: every commit compares expected prior state inside the write: compilation requires status='pending' AND the condition/content revision it compiled from; due/liveness commits require the expected compiled-check identity (hash or revision). Zero affected rows = stale result: discard silently (no counters, no status change, log at debug). Tests: dismiss-during-compile stays dismissed; condition-edit-during-run discards the stale check; normal path unaffected.

## Fix 4 (MEDIUM): fresh non-proxying HTTPS agent per guarded request — ssrf-guard.ts (~:228-251)
Today: https.request gets a pinned lookup but no dedicated agent — keep-alive socket reuse or a proxying global agent can bypass the pinned resolution.
FIX: construct a dedicated `new https.Agent({ keepAlive: false, maxSockets: 1 })` per request (proxy-less by construction), pass it, destroy it on settlement (finally). Test: pre-seed https.globalAgent with a keepAlive agent, assert the guarded request creates its own connection (observe via the agent's createConnection not being invoked / socket count).

## Fix 5 (MEDIUM): typed cancelled outcome — sandbox-runner.ts (~:74-78,124-165), runner.ts (~:70-89,121-148), evaluate-smart-notes.ts (~:295-333)
Today: external cancellation (sweep budget/deadline/lease expiry while queued on the mutex) surfaces as {network:false} failure and increments check-health counters, driving healthy checks to failing/fallback under contention.
FIX: add a distinct 'cancelled' terminal outcome (entered-lock-already-aborted, or abort raised by the RUN's external signal rather than the check's own execution timeout). Cancelled results mutate NO note state and count toward NO health policy. Only the check's own execution timeout keeps counting as failure. Tests: pre-aborted signal -> cancelled, zero state deltas; genuine execution timeout still counts.

## Fix 6 (MEDIUM): bound the cron search + validate at compile — compiler.ts (~:313-316), schedule.ts (~:23-27), dreamer/cron.ts (~:54-61,196-215), runner.ts (~:105-119)
Today: impossible-but-valid cron (0 0 31 2 *) walks minute-by-minute up to 4 years (~2.1M Date constructions) inside the state transaction.
FIX: cap the next-occurrence search at the smart-note scheduling ceiling (anything past it clamps there anyway — derive the bound from the existing clamp constant, do not invent a new knob); validate cron at compilation (reject expressions with no occurrence within the ceiling); compute next-due BEFORE opening the write transaction. Length-cap the expression. Tests: impossible cron compiles to a rejection (or clamps) in <50ms; the transaction no longer contains the search.

## Fix 7 (LOW): output bounds — compiler.ts (~:184-227), sandbox-runner.ts (~:160-165), evaluate-smart-notes.ts (~:237)
Caps before parse/inject/log: compiler response + compiled source (pick sane fixed caps, e.g. 64KB source), manifest entry count, cron string length, and truncate sandbox error name/message (e.g. 2KB) before returning/interpolating/logging.

## Gates
packages/plugin: bun test (full suite), typecheck, lint, check_comments. Comments explain invariants (why canonical-path recheck, why CAS, why cancelled is not failure) — never reference this audit or finding numbers.

Report: per-fix status + test evidence + any deviation with reason.
