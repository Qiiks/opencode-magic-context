# Task: Pi todowrite transcript renders stale/empty args (workaround for a Pi TUI bug)

## Root cause (source-confirmed with the Pi team — do NOT re-derive)

Pi 0.80.3's TUI creates a `ToolExecutionComponent` per tool call from streaming
assistant `message_update` chunks, where tool-call args can be PARTIAL or undefined.
`renderCall` IS re-invoked on every state update, but with `this.args` frozen at
component creation: neither `tool_execution_start` (which carries the complete parsed
`event.args`) nor `message_end` copies final args into an existing component — they
only flip `executionStarted`/`argsComplete`. Result: our todowrite transcript block can
permanently render "Todos — 0 active" / "No todos" while the aboveEditor overlay
(fed by our `tool_execution_start` capture) correctly shows all todos.

Pi's render context (third `renderCall` parameter, `ToolRenderContext`) exposes
`toolCallId: string` — the intended stable key. The Pi-side fix is not landed, so this
workaround must be PERMANENTLY HARMLESS: once Pi fixes arg propagation, our cached
value equals `context.args` and becomes redundant (no version gate, no removal needed).

## The fix

In `packages/pi-plugin/`:

1. Add a small bounded in-process cache (module-level, e.g. in `todo-view-pi.ts`):
   `toolCallId -> TodoItem[]`, capped (e.g. 50 entries, FIFO eviction — tool call ids
   are per-session transient; a Map with insertion-order eviction is fine).
2. Populate it from the EXISTING `tool_execution_start` capture path in `index.ts`
   (where `capturePiTodowriteArgsIfCompatible` already validates the exact shape) —
   only on successful compatible capture, storing the parsed todos. Do not add a new
   event handler; thread it through the existing one.
3. `renderTodowriteCall(args, theme)` gains the optional render context: change the
   tool's `renderCall(args, theme, context)` in `tools/todowrite.ts` to look up
   `context.toolCallId` in the cache FIRST and fall back to `parseTodos(args.todos)`.
   (Check how the ToolDefinition render signature threads params — Pi calls
   `renderCall(this.args, theme, context)`.)
4. `renderResult` in `tools/todowrite.ts`: same cache consult as a FALLBACK when its
   own parse of `result.details.todos` / content-text JSON yields null or empty —
   the result path can also render from a stale component in the same scenario.
5. Clear cache entries on `session_shutdown`/`session_before_switch` if a hook is
   already available in the todo lifecycle registration (registerTodoOverlay /
   registerTodoStateLifecycle in todo-view-pi.ts); otherwise rely on the FIFO cap.

## Tests (co-located, use existing test fakes in todo-view-pi.test.ts / index tests)
- renderCall with empty/undefined args but a cache hit for context.toolCallId renders
  the cached todos (the exact user-reported shape: 10 todos, 1 in_progress).
- renderCall with args present and NO cache entry falls back to args (unchanged).
- renderCall where cache and args BOTH present and equal renders identically
  (the permanently-harmless property once Pi fixes arg propagation).
- renderResult stale-empty result + cache hit renders cached todos; real parsed result
  wins over cache when present.
- Cache eviction: cap respected, oldest evicted.
- Capture path: incompatible (foreign-shape) todos do NOT populate the cache
  (fail-closed property preserved).

## Gates
bun test packages/pi-plugin (full), tsc, biome, check_comments.

## Rules
- Base: subc-migration HEAD. Only packages/pi-plugin/.
- Comments explain the Pi lifecycle WHY (component born from partial streaming args;
  start/end events never update existing component args) without referencing Pi
  version numbers as if permanent — say "current Pi versions".
- Commit trailer: Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>
