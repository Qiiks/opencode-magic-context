# Claude Code module manifest and content-epoch fixture

Source revision: `5a031ea39c947a6d299cff42bd6f59176fb5750c`.

This fixture records the module behavior that the session-scoped prompt-surface implementation extends. Line references are pinned to the source revision above.

## How the tool manifest is served

- `crates/mc-module/src/main.rs:1-5,28-33` shows that the module calls `serve_with` once at process startup and supplies `manifest(&module_id)` to the `HELLO` handshake.
- `crates/mc-module/src/lib.rs:11201-11210` constructs that `ModuleManifest` from only the module ID and begins its `ToolProvider` list.
- `crates/mc-module/src/lib.rs:11243-11266` materializes the four session-facing `ctx_*` descriptions and their schemas in the startup manifest. The descriptions come from the full built-in functions at `crates/mc-module/src/lib.rs:11081-11095`; no session, model key, preset, or content epoch participates in this pre-change materialization.
- `crates/mc-module/src/lib.rs:8998-9016` records project/session identity only later, when a route is bound. Therefore the startup `HELLO` manifest cannot itself select different text per bound session. Session-selected manifest text must be served through a route request while the startup manifest remains the stable tool-ID/schema declaration.

## Existing content-epoch fold mechanism

- `crates/mc-module/src/transform.rs:499-503` accepts the caller's rendered system-prompt hash on each transform request.
- `crates/mc-module/src/transform.rs:4084-4098` places that hash in the base render identity together with provider and model identity.
- `crates/mc-module/src/compartment_coverage.rs:17-23,87-118` defines the named `M0ContentEpoch` fold: all module-owned content/format epoch inputs are length-prefixed into one effective render-config string.
- `crates/mc-module/src/transform.rs:2009-2043` computes all module-owned epoch inputs and invokes `fold_m0_content_epoch` once before classification.
- `crates/mc-module/src/transform.rs:2546-2570` compares that one effective identity with the durable identity, producing one `render_config_changed` predicate. `crates/mc-module/src/transform.rs:2609-2614` treats the predicate as one bust opportunity.

The prompt-surface implementation must follow this existing pattern: freeze the session-selected guidance and manifest text for a model-key epoch, derive one prompt-surface content epoch from both surfaces, and add that single epoch to `M0ContentEpoch`. A coincident guidance and manifest change must not create independent activation latches or classifier triggers. The startup manifest's tool IDs and parameter schemas remain unchanged.
