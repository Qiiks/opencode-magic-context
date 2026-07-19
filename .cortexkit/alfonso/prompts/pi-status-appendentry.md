# Fix: Pi user-facing status lines leak into LLM context (use appendEntry, not sendMessage)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration.

## The bug (user-visible, confirmed this morning)

Every MC user-facing status surface on Pi goes through `pi.sendMessage({customType:"ctx-status", content, display:true}, {triggerTurn:false})`. Pi source facts (confirmed with the Pi team, 0.80.x):

- `sendMessage` creates a `custom_message` entry; `sessionEntryToContextMessages()` projects it as `role:"custom"`, and `convertToLlm()` turns that into a model-visible `role:"user"` message REGARDLESS of `display`.
- `{triggerTurn:false}` only means "if idle, don't start a turn". If the agent IS STREAMING, the message is queued as a steer into the live turn — which is why the model replies "Noted." to our embedding progress notices.

So the model sees (and answers) every `/ctx-embed` progress line, wrapup progress line, `/ctx-status` output, and config warning. Tokens + transcript noise on every background operation.

## The sanctioned primitive (Pi-team-provided, source-backed)

```ts
pi.registerEntryRenderer<T>("ctx-status", (entry, options, theme) => Component | undefined);
pi.appendEntry<T>("ctx-status", data);
```

- `appendEntry` creates a `type:"custom"` CustomEntry; `sessionEntryToContextMessages()` explicitly skips plain custom entries (`return []`) — never LLM-visible, never triggers/steers a turn.
- Interactive TUI renders them via `entry_appended` + the registered EntryRenderer.
- Renderer shape: `(entry: CustomEntry<T>, options: { expanded: boolean }, theme: Theme) => Component | undefined`.
- Caveat: in JSON/print mode there's no TUI renderer, entries just persist as non-LLM state — acceptable for status lines (they're for the interactive user).

## The change

1. `packages/pi-plugin/src/commands/pi-command-utils.ts` — rewrite `sendCtxStatusMessage` to use `appendEntry` with the same content shape `{title, text, level, details}`. Type the entry data. The `PiMessageSender` pick-type becomes a pick of `appendEntry` (and wherever it's constructed/mocked, update).
2. Register ONE entry renderer for `"ctx-status"` at extension setup in `packages/pi-plugin/src/index.ts` (near the other registrations): render title line (accent/level color) + text body, roughly matching how the current custom message renders (the purple boxed [title] + body Ufuk's screenshot shows). Use the theme argument for colors; degrade gracefully (return undefined) on weird data. Keep it minimal — this is a status line, not a dialog.
3. The auto-embed notify callback in `packages/pi-plugin/src/index.ts` (~line 803: `pi.sendMessage({customType:"ctx-status"...})`) routes through the same helper.
4. Sweep ALL other `sendMessage` call sites in packages/pi-plugin for user-facing status usage and convert them: /ctx-status, /ctx-embed, /ctx-recomp, /ctx-wrapup progress + completion, /ctx-dream, /ctx-flush, /ctx-session-upgrade signals, config/identity warnings — anything whose text addresses the USER. DO NOT touch: the Channel-2 ceiling nudge (`ctx-reduce-nudge-pi.ts`, customType "magic-context:ceiling-nudge") and any send that intentionally reaches the model (its doc comment says so) — that one MUST stay sendMessage (steer semantics are the feature there). If you find a call site that's ambiguous (user-facing text but currently relied on to reach the model), list it in your report instead of guessing.
5. Our own context-handler ingestion: verify `readPiSessionMessages`/`convertEntriesToRawMessages` treats `type:"custom"` entries as non-message noise (it already skips non-message entries — confirm no regression, and confirm the historian/tagger never saw these as turns before via role:"custom"→"unknown" passthrough; if old sessions have historical custom_message ctx-status entries, they're already-persisted history and stay as-is, no migration).
6. Tests: (a) unit test on the helper proving appendEntry is called and sendMessage is NOT; (b) a renderer registration test (mock pi API, assert registerEntryRenderer called with "ctx-status"); (c) update every existing test that asserts `sent[0]?.message.customType === "ctx-status"` (ctx-commands.test.ts has several) to the new shape; (d) a regression test pinning that the Channel-2 nudge still uses sendMessage (so a future sweep can't "fix" it).
7. Version-floor check: `appendEntry`/`registerEntryRenderer` must exist on our supported Pi floor (check the installed pi-coding-agent's type surface — ~/.pi installation or node_modules). If the API is missing on the floor, feature-detect: use appendEntry when available, fall back to current sendMessage behavior (better noisy than invisible), and note the floor in PARITY.md.

## Gates

cd packages/pi-plugin && bun test green, typecheck green, lint clean, check_comments clean. Comments explain the invariant (custom entries don't reach the LLM; sendMessage steers mid-stream) without referencing this incident. Report: list of every call site converted + the ambiguous ones left.
