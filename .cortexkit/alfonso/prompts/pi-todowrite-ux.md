# Task: Pi todowrite — first-class UX + disable knob

Repo: this worktree (magic-context). Reference UX implementation: ~/Work/OSS/rpiv-mono/packages/rpiv-todo (READ it first: todo.ts, todo-overlay.ts, view/format.ts, and its tests). We are NOT copying its tool contract — only reaching its UX bar.

## Problem

Magic Context's Pi plugin registers a minimal `todowrite` tool unconditionally (packages/pi-plugin/src/tools/todowrite.ts, registered in tools/index.ts). Two issues:
1. Users running their own todo extension (e.g. rpiv-todo) get TWO task-list tools + duplicated prompt guidance — no way to turn ours off.
2. Ours has no UX at all: raw JSON tool output, no widget, no command. Community extensions set a much higher bar.

## Contract pins (load-bearing — do not deviate)

- The tool NAME stays `todowrite` and the parameter shape stays OpenCode-parity `{ todos: [{content, status, priority?, id?}] }` (replace-list semantics). The synthetic-todowrite injector, the `tool_execution_start`/`message_end` capture into `session_meta.last_todo_state`, and cross-harness consistency all depend on this. UX layer only.
- The tool result's TEXT content stays the pretty-printed JSON of the todos array (OpenCode todo.ts parity — the model reads this back). Rich rendering happens in renderCall/renderResult and the widget, NOT by changing result text bytes.
- Cache: nothing here may touch the transform pipeline. The widget reads state; it never writes message content.

## Deliverable 1: config knob

Add to the shared Zod schema (packages/plugin/src/config/schema/magic-context.ts, follow memory of schema rules: .describe() docs, then regenerate assets/magic-context.schema.json via `bun packages/plugin/scripts/build-schema.ts`):

```
todowrite: z.object({
  enabled: z.boolean().default(true).describe("Pi only: register Magic Context's todowrite task-list tool. Disable if you use your own todo extension. OpenCode ships its own built-in todowrite; this setting has no effect there."),
  overlay: z.boolean().default(true).describe("Pi only: show the persistent todo overlay above the editor while tasks are active."),
}).default({...})
```

Wire into Pi's tools/index.ts registration: when `todowrite.enabled === false`, do NOT register the tool or the /todos command or the overlay. The `last_todo_state` capture path can stay (it no-ops without todowrite calls). Resolve the config the same way other per-project Pi config is resolved at registration (registration is boot-time; per-cwd re-resolution is NOT needed for tool registration since Pi registers tools once — document that a /cd into a project with a different todowrite.enabled requires restart, matching Pi's tool-registration lifecycle).

Also update: docs CONFIGURATION.md (root) generated docs if applicable, dashboard ConfigEditor field coverage (packages/dashboard: add the field so the config-field-coverage test stays green — check how smart_drops/language were added).

## Deliverable 2: UX layer (Pi only, all inside packages/pi-plugin)

New file packages/pi-plugin/src/tools/todo-view-pi.ts (or similar) + rework of todowrite.ts registration:

a) **renderCall / renderResult** on the tool definition (Pi ToolDefinition supports them — see rpiv todo.ts:91-97): compact themed lines instead of raw JSON. renderCall: "Todos — N active" style. renderResult: glyph lines (○ pending, ◐ in_progress, ✓ completed, ✗ cancelled) one per todo, truncated to width. Mirror rpiv's view/format.ts style (glyphs + theme colors) without importing from it.

b) **Overlay widget**: persistent list above the editor while any non-completed todo exists. Mirror rpiv's todo-overlay.ts LIFECYCLE exactly (it encodes hard-won Pi contract knowledge): factory-form setWidget(key, factory, {placement:"aboveEditor"}), register-once + tui.requestRender() refresh, invalidate() clears registration state (handles /reload), auto-hide (setWidget(key, undefined)) when list empties, cap at ~12 content rows with "+N more" tail. State source: in-memory snapshot of the last captured todos (the same {todos:[...]} the capture path sees) — hold it in a module-level per-session map updated from the existing tool_execution_start capture site in index.ts; seed at session_start from session_meta.last_todo_state so the widget survives restarts. Key by session; clear on session switch. Gate the whole widget on config todowrite.overlay && todowrite.enabled.
   - Header line with counts ("2/5 completed · 1 in progress"), matching /ctx-status visual conventions (check dialogs/ for theme usage patterns).
   - Hide completed tasks from PREVIOUS turns like rpiv does (completedTaskIdsPendingHide/hidden sets, reset when list restarts) — this keeps the widget focused on live work.

c) **/todos command**: grouped read-only listing (Pending / In Progress / Completed sections with glyphs + counts header) via ctx.ui.notify, reading the same state. Skip registration when disabled. If a session has no todos: "No todos yet." info notice.

d) **promptSnippet/promptGuidelines**: IF Pi's ToolDefinition type in our installed @earendil-works/pi-coding-agent version supports promptSnippet/promptGuidelines fields (rpiv uses them — verify against node_modules types), move the long TOOL_DESCRIPTION guidance into promptGuidelines (keep description one line). If the fields don't exist in our pinned version, keep the current description unchanged and note it.

## Tests (non-vacuous)

- Config: enabled=false → registerTool/registerCommand never called for todowrite//todos; enabled=true default registers (existing tests keep passing).
- Overlay controller: unit-test the lifecycle with a fake ui ctx + tui (mirror rpiv's todo-overlay tests' approach): register-once, requestRender on second update, auto-hide on empty, re-register after invalidate, completed-from-previous-turn hiding, restart seeding from session_meta.
- renderResult: glyph lines for a mixed list; result TEXT bytes still exact pretty-printed JSON (assert unchanged — contract pin).
- Schema: build-schema regenerated; dashboard field-coverage test green.

## Gates

cd packages/pi-plugin && bun test --timeout 60000; cd packages/plugin && bun test --timeout 60000 (schema tests); typecheck both; repo lint; check_comments; regenerate schema JSON. Comments explain Pi widget lifecycle contracts (why factory-form/register-once/invalidate) without referencing rpiv line numbers or this task.
Commit with trailer: Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>
