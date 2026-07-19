# Remove the `ctx_reduce_enabled` config flag; decouple caveman compression from it

## Why

`ctx_reduce_enabled: false` is a vestigial knob. Its original job was "opt out of agent-driven drops because `auto_drop_tool_age` reduces automatically" — that auto-drop was deleted long ago, so `false` today only hides the reduce surface (tool, §N§ prefixes, nudges, guidance) while reduction still happens (historian + emergency paths). There is already a second, structural off-switch: the availability gate (`packages/plugin/src/hooks/magic-context/ctx-reduce-availability.ts`) drives the entire no-reduce rendering off the session's tool allow-list. A user who does not want the tool denies `ctx_reduce` in their agent's tools allow-list and everything adapts automatically. We are removing the config flag entirely and letting availability be the only gate. Session modes collapse from 3 (primary+reduce / primary-no-reduce / subagent) to 2 (primary / subagent).

Caveman text compression is currently gated on `ctx_reduce_enabled === false`, so removing the flag FORCES the decouple: caveman becomes an orthogonal opt-in that runs for any primary session when `caveman_text_compression.enabled === true` (subagents stay excluded — they have no `ctx_expand` recovery path and their output is curated by the parent). This is safe by design: caveman targets old conversation prose (`message` tags), drops target tool outputs / explicitly discarded tags; caveman only touches `status === "active"` tags so a dropped tag is skipped (drop wins); both replay frozen state every pass.

## The change

### A. Config schema (OpenCode `packages/plugin/src/config/schema/magic-context.ts`)
- Delete `ctx_reduce_enabled` from the interface, Zod schema, and defaults. An existing key in user config is simply ignored by parsing (verify: unknown keys must not produce a hard error; if the loader warns on unknown keys, that warning is acceptable).
- Update `caveman_text_compression` descriptions: remove every "Only active when ctx_reduce_enabled=false" claim; new contract is "active for primary sessions when enabled; never for subagents".
- Regenerate `assets/magic-context.schema.json` via `bun packages/plugin/scripts/build-schema.ts`.

### B. OpenCode plugin
All `ctx_reduce_enabled`/`ctxReduceEnabled` sites (19 files — grep is authoritative, the list below is the load-bearing subset):
- `plugin/tool-registry.ts:67`: always register `ctx_reduce`.
- `hooks/magic-context/hook.ts` (307, 547, 610-614, 812, 836-841, 862, 878): delete the flag plumbing; caveman pass-through becomes `deps.config.caveman_text_compression?.enabled === true` (no flag term).
- `hooks/magic-context/transform.ts`: `ctxReduceEnabledEffective` (460) collapses to the availability resolution alone; `skipPrefixInjection` (1340) accordingly; the caveman gate (1505) drops `!deps.ctxReduceEnabled` (READ the surrounding code first — understand what `reducedMode` means there and preserve its role); line 1745's `deps.ctxReduceEnabled === false && !reducedMode` condition: read it, understand what it gates, and convert to the availability-driven equivalent; Channel-1 gating (2003-2006) already keys on the effective value.
- `hooks/magic-context/system-prompt-hash.ts` (122, 180, 305-311): variant selection driven by `ctxReduceCallable` (availability) + subagent only. The caveman "BEWARE: history compression is on" warning is now emitted whenever caveman is enabled (both reduce and no-reduce prompt variants) — the agent must know prose gets rewritten regardless of whether it can call ctx_reduce.
- `agents/magic-context-prompt.ts` (27-30, 91, 147-178): the `ctxReduceEnabled` parameter's role is taken by the availability-driven caller; the no-reduce guidance variant MUST remain (subagents/denied sessions still need it) — only the config input disappears. Emit `CAVEMAN_COMPRESSION_WARNING` whenever caveman is enabled, in both variants.
- `hooks/magic-context/tag-messages.ts`, `shared/tag-transcript.ts`, `transform-postprocess-phase.ts`, `heuristic-cleanup.ts`, `caveman-cleanup.ts`: comment updates + any flag plumbing.
- `plugin/hooks/create-session-hooks.ts:21`: delete.

### C. Pi plugin (`packages/pi-plugin/src`)
- `index.ts` (623-636, 669-674, 1127-1131): delete flag reads. Tools: `ctx_reduce` always registered for primary sessions (keep the existing `sessionScopedToolsDisabled` and per-call gating unrelated to this flag). Caveman pass-through gates on `caveman_text_compression?.enabled` only.
- `tools/index.ts` + `tools/ctx-reduce.ts`: registration unconditional (subject to existing non-flag gates); update doc comments.
- `context-handler.ts` (713, 2197, 2419-2421, 3287, 3601, 3645): `ctxReduceEnabled` plumbing collapses to Pi's availability equivalent (read 2419's comment — it mirrors OpenCode's `ctxReduceEnabledEffective`; keep the missing-tool guard).
- `system-prompt.ts` (42, 70): variant by availability/subagent only; caveman warning now independent of the flag.

### D. Dashboard config editor
- `packages/dashboard/src/components/ConfigEditor.tsx` (or wherever the field lives — grep `ctx_reduce_enabled`): remove the field; update the config-field-coverage test.

### E. Docs
- Root `CONFIGURATION.md`: remove the key; adjust the caveman section (no longer conditioned on the flag; document the allow-list path as the way to opt out of the reduce surface).
- `packages/docs` site: regenerate the config reference (`build-config-docs` script) and update `session-modes` page: 3 modes → 2 (primary / subagent); caveman row becomes "opt-in (primary only)".
- Root `ARCHITECTURE.md` "Session modes" section: collapse the table's two primary columns into one; caveman row = "opt-in". DO NOT touch anything between the `mc:protected START/END` markers (this section is outside them, verify before editing).

### F. Tests
- Update every test that sets/asserts the flag (grep list: `system-prompt-hash.test.ts`, `transform.test.ts` Unit B (1474), `magic-context-prompt.test.ts`, config tests, Pi `system-prompt.test.ts`, `context-handler.test.ts`, `tools/index.test.ts`, `config/index.test.ts`, e2e-tests if any use the flag — grep `packages/e2e-tests`).
- The no-reduce rendering path keeps coverage — rework those tests to drive the availability gate (tool absent from the first user message's tools map) instead of the config flag.
- NEW tests: (1) caveman compression runs for a primary session with ctx_reduce available (previously impossible) — assert compression applies AND a ctx_reduce-dropped tag is not caveman-rewritten (drop wins); (2) prompt guidance includes the caveman warning in the reduce-enabled variant when caveman is on.
- `config/migrate-config-location.test.ts` uses the key as fixture content only (108-129) — swap the fixture key for a live one.

## Cache-safety constraints (this touches prompt bytes — treat as load-bearing)
- For a former `ctx_reduce_enabled: false` user, the system prompt variant and §N§ prefixes change ONCE after upgrade (acceptable, like any upgrade). What must NOT happen: oscillation. The availability verdict is already frozen per-session from the first user message (`ctx-reduce-availability.ts`) — do not weaken that freeze.
- Caveman's replay must stay deterministic per pass regardless of drop interleaving (existing invariant — keep the persisted-depth replay untouched).

## Gates
- `cd packages/plugin && bun test && bun run typecheck`
- `cd packages/pi-plugin && bun test && bun run typecheck`
- `cd packages/cli && bun test` (docs-gen may reference the key)
- `bun run lint` from repo root (formatter: 4-space/double-quote in plugin, tabs in pi-plugin)
- `cd packages/dashboard && bun run build` + its config coverage test
- `check_comments` — comments explain WHY for a cold reader; no references to this plan or the removal process. Where old comments say "when ctx_reduce_enabled is false", rewrite to the availability framing, don't just delete.

Do not touch `crates/`. Commit with a clear message.
