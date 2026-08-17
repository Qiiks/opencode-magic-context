# Dashboard v0.13.1

## Changed

- **Harness-scoped model catalogs.** The dashboard now invokes one Tauri command, `get_model_catalogs`, which returns `{ opencode, pi }` from one backend read. The configuration editor passes the matching catalog only to each OpenCode or Pi model picker; it does not merge catalog entries.
- **Per-harness agent editors.** Historian and Dreamer have OpenCode and Pi tabs. OpenCode entries expose `variant`; Pi entries expose `thinking_level`. Free-text `provider/model` values remain available when a model is absent from a catalog.

## Implementation note

The single-command `{ opencode, pi }` response was selected over one command per tab so both catalogs are refreshed together and every dashboard consumer reads the same generation without adding a plugin localhost-RPC dependency.
