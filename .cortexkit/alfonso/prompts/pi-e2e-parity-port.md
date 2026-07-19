# Task: Pi E2E parity — port the genuinely-portable coverage gaps

Repo: this worktree (magic-context monorepo). Work in packages/e2e-tests/ (tests + maybe small src/ helper additions) — NO production code changes in packages/plugin or packages/pi-plugin. If a test cannot pass without a production change, STOP and report the finding instead of changing production code.

Context: an assertion-level audit compared OpenCode e2e files against their Pi twins. Pi's biggest gaps are all in the cache-invariants suite. Reference implementations are the OpenCode files; the Pi harness (`src/pi-harness.ts`, `src/pi-runner/`) and the shared cache-bust oracle (`src/cache-analysis.ts`) are the substrate. Read these first:
- tests/cache-invariants.test.ts (OC reference: A1/A2/A3, B9-B12)
- tests/pi-cache-invariants.test.ts (Pi twin, currently B9-only)
- tests/pi-cache-stability.test.ts + tests/pi-long-running-session.test.ts (existing Pi idioms: send helpers, usage constants, readMeta, m0/m1 extraction)
- tests/memory-injection.test.ts vs tests/pi-memory-injection.test.ts

## Deliverable 1 — extend tests/pi-cache-invariants.test.ts (priority order)

Port these OC scenarios to Pi, using the SHARED oracle from src/cache-analysis.ts for all bust-window assertions (never ad-hoc byte equality for "zero busts" claims):

1. **A3**: aged ctx_reduce never vanishes mid-prefix on defer growth. Drive: emit ctx_reduce on an old tag, materialize it on an execute pass, then grow the session with low-pressure defer turns; assert zero busts across the post-reduce window AND the final wire still contains the `[dropped` placeholder (never silently stripped).
2. **B12**: project epoch bump HARD-refolds — seed baseline m[0], surface a delta in m[1] (compartment or memory), bump project_memory_epoch in the DB (mirror how the OC test does it), drive a busting pass; assert the delta folded INTO m[0], m[1] reset to placeholder, and subsequent defer replay is bust-free.
3. **B11**: non-additive memory mutation renders `<memory-updates>` in m[1] while m[0] stays byte-frozen (capture exact baseline bytes and compare), then stable defer replay.
4. **B10**: additive memory write rides m[1] `<new-memories>`; m[0] equals the exact pre-write baseline bytes; trailing replay stable.
5. **B9 strengthening**: in the existing Pi B9 test, capture the exact pre-publish m[0] bytes and assert byte-equality after surfacing (not substring), and assert m[0]+m[1] stay byte-identical across the surface pass and following defers (the OC `Set.size===1` pattern).
6. **A2**: after an execute pass, the following defer window has zero busts under the shared oracle.
7. **A1**: low-pressure pure-defer growth never busts, under the shared oracle.

Pi-specific hazards you MUST account for (learned from a recent fix in tests/pi-long-running-session.test.ts — read its phase-6 comment):
- Pi's pipeline VETOES drains/heuristics while a historian run is in flight. If a scenario needs an execute/busting pass to land a mutation, wait for the compartment_state_lease row for the session to clear (see the waitFor pattern in pi-long-running-session.test.ts phase 6) and/or retry the turn under pressure.
- Use existing Pi usage-constant idioms (LOW/HIGH/FORCE) and openTestDb with busy_timeout for any writable DB access.
- Keep each scenario's turns minimal — this suite runs on every CI push.

## Deliverable 2 — strengthen tests/pi-memory-injection.test.ts

Add: explicit `countCompartments == 0` assertion on the assertion session (prove memory injection alone, not history), and assert the wire contains the `<session-history>` wrapper (not just `<project-memory>`).

## Deliverable 3 — NEW tests/pi-subagent-behavior.test.ts (Pi-NATIVE, not a port)

Pi subagents are hidden subprocesses (`pi --print --no-session`), not child sessions. Do NOT mimic OpenCode's createChildSession. Two tests, driven at the PiSubagentRunner integration level (packages/pi-plugin/src/subagent-runner.ts — read its existing test file for the seam it exposes; prefer driving through existing seams over adding new ones, but a small argv/env-capturing test hook in the runner's spawn path is acceptable IF one doesn't exist — that is the ONE permitted production touch, test-seam-only, mirroring existing __set*TestHooks conventions):

1. **Recursion/isolation**: spawning a hidden child (historian or dreamer agent) sets MAGIC_CONTEXT_PI_SUBAGENT=1 in the child env, passes --no-session, and passes a strict per-agent --tools allowlist; unknown agent name fails closed to --no-tools. Assert on the captured spawn argv/env.
2. **Failure propagation**: a child returning an overflow-shaped provider error propagates as a runner failure without spawning any further (recursive) children and without creating child session rows.

If the runner's existing unit tests in packages/pi-plugin already cover part of this, extend THERE instead of duplicating in e2e — report which home you chose and why.

## Wiring

New pi-*.test.ts files are picked up automatically by CI's `tests/pi-*.test.ts` glob — verify the new file name matches.

## Gates

cd packages/e2e-tests && NODE_ENV="" bun test --timeout 600000 tests/pi-cache-invariants.test.ts tests/pi-memory-injection.test.ts tests/pi-subagent-behavior.test.ts (plus full pi-*.test.ts sweep once green; run the flakier long-running file twice). Also run packages/pi-plugin bun test if you touched the runner seam. repo lint, typecheck, check_comments. Tests must be non-vacuous: for each ported scenario, verify the assertion FAILS if the mechanism is broken (e.g. temporarily invert a condition locally to prove the test bites — do not commit the inversion).

Commit with trailer: Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>
