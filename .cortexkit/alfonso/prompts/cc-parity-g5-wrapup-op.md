# CC-leg parity G5: session.wrapup + session.status management ops

Repo: this worktree (magic-context), branch base = subc-migration HEAD. Rust work in
`crates/mc-module` (`lib.rs` op routing + `historian.rs` + `boundary.rs`). These ops
back MCP prompts (`/mcp__ck__wrapup`, `/mcp__ck__status`) that the Thalamus shim
will expose; the shim calls these management ops on `prompts/get` and returns the
op's summary string as the prompt text. Ops must therefore be IDEMPOTENT under
retry and return a compact single-paragraph human-readable summary.

## session.status (small, build first)

New ToolProvider management method `session.status` {v, session_id}: resolve the
bound session (same binding/facade discipline as agent_drops.append; reject
unbound). Return {ok, summary} where summary is a compact line: compartment count,
coverage ordinal, boundary present/absent, pending drops count, tag count, last
historian outcome (from HistorianDurableState: last fire/publish/no-fire reason),
surface state. Read-only; no CAS, no side effects. This is the /ctx-status
equivalent for CC users.

## session.wrapup (the real work)

Semantics ported from the TS /ctx-wrapup (read
`packages/plugin/src/hooks/magic-context/wrapup-orchestrator.ts` and
`resolveWrapupProtectedTailBoundary` in protected-tail-boundary.ts for the
CONTRACT, not the code shape): force historian compaction of the raw tail down to
a keep watermark of the newest N raw messages (default 20, optional `keep` arg),
in a bounded sequential loop, regardless of pressure triggers.

Module design:
- Wrapup boundary: count raw messages of ANY role from the tail backward (module
  equivalent: live blocks grouped by ordinal above coverage_ordinal); keep
  watermark = the Nth-newest message's ordinal; snap the cut to a safe boundary
  using the EXISTING boundary machinery in boundary.rs (tool-arc fencing, the
  terminal-arc guard from the fold-safety work, user-boundary snapping). Never cut
  a live tool arc; never let the newest message or its arc become the boundary
  anchor (the existing verbatim-tail guard already enforces this — reuse it).
- Loop: fire the historian producer on the chunk below the watermark using the
  EXISTING firing assembly (prepare/assemble + producer driver + publish path —
  historian.rs owns all of it; the Emergency95 inline-drive arm shows how to drive
  a fire to completion inline). Repeat until coverage reaches the watermark or a
  bounded iteration/time cap (max 5 rounds / 600s per round, mirroring TS bounds).
  Substance floor: bypass min_chunk_tokens exactly as the emergency path does
  (fold_is_only_reclaim profiles already bypass; wrapup on CC is such a profile).
- Concurrency: take the existing historian busy/latch discipline — if a fire is
  already active, JOIN it (await, then continue the loop) rather than erroring.
  A second concurrent wrapup call must not double-drive: use a wrapup-in-progress
  latch in ModuleMeta (in-memory per-session entry is acceptable if process-local
  suffices — the module is the single writer; justify the choice in a comment).
- Idempotency for prompts/get retries: if a wrapup is already running for the
  session, return {ok, summary:"wrapup already in progress, N rounds done"} instead
  of starting a second. A completed wrapup re-invoked = runs again over whatever
  tail remains (harmless; typically no-fires on empty eligible range and reports
  "nothing to compact").
- The op does NOT touch the transform hot path: it drives fires exactly like the
  existing coordinator paths. The NEXT transform pass after publishes observes the
  boundary and takes the natural HARD fold (same as any historian publish); the
  summary must say so ("compacted K messages into M compartments; takes effect on
  your next message").
- Failure: producer failure mid-loop → stop, return {ok:false-shaped error or
  ok+summary with the failure phase} consistent with existing error envelopes;
  durable state must be exactly what the normal failure paths leave (no new
  failure states).

## Tests

- status: bound/unbound rejection; summary fields against a seeded store.
- wrapup: seeded multi-chunk session drives to the watermark in ≤N rounds
  (use the existing producer test doubles / spine harness in transform tests and
  the real_daemon patterns); keep-watermark honored (newest N raw messages remain
  raw); tool-arc never cut (fixture with an arc straddling the watermark snaps
  wider); busy-join (concurrent second call returns in-progress summary, no double
  fire — fail-first: remove the latch, test must fail); idempotent re-invoke after
  completion reports nothing-to-compact; bounded rounds cap enforced.
- Full gates: cargo test -p mc-module -p mc-store, clippy --all-targets, fmt.

House rules: comments explain WHY, never reference this task; NO em dashes
anywhere; commit with
Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>.
