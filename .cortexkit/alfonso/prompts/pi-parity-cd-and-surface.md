# Task: Pi parity — /cd config bleed + user-surface fixes

Repo: this worktree (magic-context master). Changes in packages/pi-plugin (+ PARITY.md, + tests). Reference: packages/plugin. Read packages/pi-plugin/PARITY.md first.

## Finding 1 (the big one): tools/commands freeze launch-project config across /cd

Pi is a long-lived process where /cd switches projects. The context pipeline already re-resolves per-project config per cwd (index.ts:649-720 and the before_agent_start re-resolution at index.ts:961-999), but tools and slash commands are registered ONCE with boot-time config: tools at index.ts:606-630, commands (/ctx-status, /ctx-recomp, /ctx-wrapup, /ctx-session-upgrade, /ctx-embed, /ctx-dream, /ctx-aug) at index.ts:760-873. Concrete bug example: /ctx-dream gates on static deps.dreamerEnabled (commands/ctx-dream.ts:27-31, 56-69). After /cd, commands use the OLD project's model, language, memory gates, schedules, and enablement.

Fix approach: introduce a `resolveCurrentProjectDeps(ctx)` accessor that resolves effective config from ctx.cwd at INVOCATION time (reuse the exact resolution logic the context pipeline / before_agent_start path uses — extract it into a shared helper rather than duplicating; there is already a memoized per-cwd resolver pattern in the codebase from the earlier project-config-bleed fix, find and reuse it). Convert each command handler and tool that captured boot config to call the accessor per invocation. Keep registration static (Pi registers tools/commands once by design — only the CONFIG the handlers read becomes dynamic). Where a handler needs the dreamer/historian runner objects, resolve those through the same accessor so model/language/schedule follow the current project.

Scope check: every deps field captured at registration in index.ts:760-873 and tools at :606-630 — audit each for per-project sensitivity (model, language, enablement, thresholds, memory gates, smart-note gating, protected tag config). Fields that are genuinely process-global (db handle, runner infrastructure) stay static.

## Finding 2: Pi smart-note writes omit the creating sessionId

OpenCode stores sessionId on smart-note writes (tools/ctx-note/tools.ts:216-239); Pi omits it (pi-plugin/src/tools/ctx-note.ts:148-155). Consequence: note-search anchors (@msg N + ctx_expand footer) never render for Pi-created smart notes because the own-session restriction can't match. Fix: store the sessionId.

## Finding 3: /ctx-recomp --upgrade parsing missing on Pi

OpenCode accepts --upgrade and returns a deprecation/upgrade hint (command-handler.ts:65-75, 188-200, 628-633). Pi's parser rejects it as invalid usage (commands/ctx-recomp.ts:242-267). Fix: accept the flag and return the same deprecation hint text (pointing at /ctx-session-upgrade).

## Finding 4: /ctx-aug empty-sentinel divergence — document as intentional, do NOT change Pi

When sidekick finds nothing, OpenCode injects a <sidekick-augmentation> block containing "No relevant memories found"; Pi detects the sentinel and sends the raw prompt with no block (ctx-aug.ts:176-192). Pi's behavior is the better one (a no-op augmentation block wastes tokens and adds noise). Resolution: add a PARITY.md entry documenting Pi's skip-empty-augmentation as intentional, with a note that OpenCode should eventually adopt it. Do not change either harness's behavior in this task.

## Finding 5: Pi status dialog omits work metrics OpenCode exposes

Pi computes work metrics (context-handler.ts:2551-2557) but StatusDialogDetail has no work-metric fields (dialogs/status-dialog.ts:60-105; render :245-330), while OpenCode's sidebar shows newWorkTokens/totalInputTokens (rpc-handlers.ts:439-440, sidebar-content.tsx:879-885). Fix: add the work-metric line(s) to Pi's /ctx-status dialog output from the already-computed stored values.

## Finding 6: minor tool-surface drift

- ctx_memory list output: add the table header + VERIFY column parity (plugin tools.ts:84-139 vs pi ctx-memory.ts:154-183).
- ctx_expand schema descriptions: bring Pi's start/end/message parameter descriptions up to the OpenCode text (plugin ctx-expand/tools.ts:17-40 vs pi ctx-expand.ts:38-59) — these are LLM-facing and the richer text exists for a reason.

## Tests (non-vacuous)

- /cd: register with project A config, invoke a command with ctx.cwd in project B (different dreamer enablement + model) → handler uses B's config; same for a tool (smart-note gating or protected-tag config).
- Smart-note write stores sessionId; note search from the same session renders the @msg anchor.
- /ctx-recomp --upgrade returns the deprecation hint, not a usage error.
- Status dialog includes the work-metric fields.

## Gates

cd packages/pi-plugin && bun test --timeout 60000, bun run typecheck, repo-root bun run lint, check_comments. PARITY.md entry for Finding 4. Do not modify packages/plugin behavior. Commit with trailer: `Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>`
