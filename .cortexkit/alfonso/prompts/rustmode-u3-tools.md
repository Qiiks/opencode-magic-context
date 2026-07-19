# Rust MC mode — U3: tools rewire for rust-mode sessions (TS)

Part of the per-project Rust MC cutover (plan: `.alfonso/plans/rust-mc-mode-v1.md` v2, unit U3 — read it; the "State authority" section defines which store each tool hits). TS only (packages/plugin). Current branch HEAD contains U0 (resolved `config.transform_mode`), U1+U5 (rust-mode transform authority, `module-wire.ts`, `module-state-sync.ts`, `RustModeModuleClient`), and the module ops from U2.

## Contract

For sessions whose resolved transform_mode is "rust", the agent-facing tools change their BACKING calls — the tool names, schemas, and descriptions the model sees stay byte-identical (tool-set stability; changing descriptions would bust caches and confuse guidance). TS-mode sessions are untouched (their tests must stay green).

Per tool:

1. **ctx_reduce** → module `agent_drops.append` (exists, prod-serving; idempotent command ledger). Request: `{method:"agent_drops.append", v:1, session_id, drop:"<raw drop string>", command_id:"<unique per invocation>"}` — the module canonicalizes ranges server-side; send the RAW user string, do not parse TS-side. command_id: mint `"oc-<sessionId>-<monotonic>"` or a UUID (must be non-empty, ≤128B, unique per call, stable across retries of the SAME call). Response `{ok:true, queued:n}` → the tool returns its existing deferred-discard ack wording UNCHANGED (guidance depends on it). Module error → tool returns the existing failure wording; never crash the tool.
2. **ctx_search / ctx_expand** → KEEP TS implementations UNCHANGED in this unit. Rationale (plan State-authority): search reads context.db (memories/FTS/commits/notes) and the compartment mirror-back keeps context.db compartment rows current, so TS search stays correct in rust mode. ctx_expand reads opencode.db raw history — host-resident, unchanged. (The CC-leg facade ops exist but serve module-store scopes; wiring them here would split read authority. If you find a search path that reads a table rust mode makes stale, REPORT it — do not silently fix.)
3. **ctx_memory** → hybrid (plan U3, verbatim): writes go to context.db through the EXISTING TS implementation (single write authority on the OC leg), then trigger a module memory sync via the U5 service (`syncModuleState` with the memory-delta path / bump the watermark inputs so the next authority pass ships the delta). Concretely: after a successful write/update/archive/merge in a rust-mode session, invoke the sync-trigger seam (add a narrow hook in the tool implementation that the rust-mode wiring registers; ts-mode registers nothing). Do NOT call module memory-mutation facade ops on this leg.
4. **ctx_note** → fully TS (context.db), explicitly unchanged; assert with a test that rust-mode note writes hit context.db (the CC-leg mc_notes fork must not activate here).

## Wiring

The tools are constructed in `packages/plugin/src/plugin/tool-registry.ts` + per-tool `src/tools/*/tools.ts`. Thread the resolved mode + module client the same way the transform got them (hook config; the client seam is `RustModeModuleClient`). Keep the seam narrow: a single `rustToolBackends` object created in hook.ts when mode is rust, passed to the registry; tools check `backends?.reduce` etc. rather than reading config themselves.

## Tests

- ctx_reduce rust-mode: mock module client → agent_drops.append called with raw string + unique command_id; ack wording byte-identical to ts-mode ack; module error → existing failure wording, no throw.
- ctx_reduce ts-mode: module client NOT called (existing path untouched).
- ctx_memory rust-mode: context.db write happens exactly as ts-mode, sync trigger fired once per mutation; ts-mode: no trigger.
- ctx_note rust-mode: context.db row, no module call.
- Existing tool suites all green.
- Full plugin suite (known pre-existing noise: late "Database has closed" runner failure — report, don't chase).

Commit in the worktree; do not push. Report anything you were tempted to defer.
