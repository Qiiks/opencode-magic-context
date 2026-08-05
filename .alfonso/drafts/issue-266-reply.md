Shipped in S7: set `compaction.enabled` to `false` in the user-level `magic-context.jsonc` to keep memory, docs, search, notes, raw-message indexing, and additive injection while disabling Magic Context context management; native OpenCode/Pi compaction is allowed to own the window, `fail_closed_blocking` is inert, and the setting takes effect after a restart. MC's setting is separate from OpenCode's `compaction.auto` / `compaction.prune`; the first turn after disabling may trigger one native compaction cycle on a long session, and `/ctx-wrapup` is the suggested catch-up when switching back on. Child sessions are covered by native compaction (verified against OpenCode v1.18.4), and `transform_mode: "rust"` safely falls back to TypeScript with a single boot warning in compaction-off mode.

```jsonc
{
  "compaction": {
    "enabled": false
  }
}
```
