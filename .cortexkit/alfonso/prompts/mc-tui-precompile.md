# Task: ship precompiled TUI so the published npm package renders reactively

Repo: this worktree (magic-context). All changes under packages/plugin. This fixes a live production bug: on OpenCode 1.17.14 (OpenTUI 0.4.3), the npm-published TUI sidebar renders but never updates (frozen zeros).

## Root cause (proven live — do not re-derive)

OpenTUI's Solid transform plugin excludes all paths under node_modules (sourceFilter negative-lookahead in @opentui/solid scripts/solid-plugin.js). A published plugin's TUI lives in OpenCode's plugin cache under node_modules, so its TSX is compiled by Bun's plain TSX path -> runtime jsx via @opentui/solid/jsx-runtime. Under runtime jsx, JSX children/props like `{s()?.count ?? 0}` are evaluated EAGERLY at element creation: signals/effects still fire, but on-screen text freezes at first paint (null snapshot -> zeros forever). Dev checkouts (file:// outside node_modules) get the compile-time transform -> reactive bindings -> work, so dev-path testing can never catch this.

## The fix (mechanism validated live on the plugin cache — reuse it exactly)

Precompile src/tui TSX at build time using @opentui/solid's own transform, with imports rewritten to the HOST's virtual runtime module ids. These ids (`opentui:runtime-module:<encodeURIComponent(specifier)>`) are registered process-globally by the host's ensureRuntimePluginSupport via Bun build.module, so they resolve from ANY path (including the plugin cache) and bind the host's SINGLE runtime instance. The validated invocation:

```ts
const { transformSolidSource } = await import(/* resolved from our own dep */ "@opentui/solid/scripts/solid-transform.js");
const mid = (s: string) => "opentui:runtime-module:" + encodeURIComponent(s);
const RUNTIME = new Set([
  "@opentui/core", "@opentui/core/testing",
  "@opentui/solid", "@opentui/solid/components",
  "@opentui/solid/jsx-runtime", "@opentui/solid/jsx-dev-runtime",
  "solid-js", "solid-js/store",
]);
const out = await transformSolidSource(code, {
  filename,
  moduleName: mid("@opentui/solid"),
  resolvePath: (spec: string) => (RUNTIME.has(spec) ? mid(spec) : null),
});
```

CRITICAL pins from the live validation:
- Virtual ids are load-bearing. Compiling with bare specifiers (resolving to the package's own deps) was tested and FAILS: two solid/opentui runtime instances, the sidebar section vanishes entirely.
- resolvePath returning null for non-runtime specifiers leaves relative imports untouched (correct).
- The import of scripts/solid-transform.js may need a filesystem path (Bun couldn't resolve the subpath bare in one context): resolve via `require.resolve("@opentui/solid/package.json")` dirname + "scripts/solid-transform.js", or import.meta.resolve. Make the build script robust to both.

## Deliverables

### 1. Build script: packages/plugin/scripts/build-tui.ts
- Walks packages/plugin/src/tui recursively.
- For each `.tsx`: run the transform above, write the OUTPUT to `src/tui-compiled/<same relative path>` KEEPING the `.tsx` filename (compiled JS is valid TSX; keeping names preserves all relative extensionless imports — validated live).
- For each `.ts`: copy verbatim to the mirror path (they contain no JSX; Bun loads TS natively).
- SKIP test files (`*.test.ts`, `*.test.tsx`) — do not ship them in the compiled tree.
- `src/tui-compiled/` sits at the same depth as `src/tui/`, so `../../shared/...` and `../../../package.json` relative imports resolve identically. Do NOT rewrite relative specifiers.
- Deterministic output (stable file ordering; no timestamps in output) so repeated builds are byte-identical.
- Add `src/tui-compiled/` to .gitignore (build artifact, never committed).

### 2. Loader entry: packages/plugin/src/tui/entry.mjs (plain JS, NO TSX, NO JSX)
The published `./tui` export becomes this loader. Logic:
```js
// Try the host's virtual runtime-module registry first: if the host registered
// it (OpenTUI 0.4.x line), the compiled tree binds the host's single runtime
// instance. Absent registry (0.3.x host or bare bun) -> raw TSX fallback,
// which works wherever the Solid transform still applies to this path.
let mod;
try {
  await import("opentui:runtime-module:" + encodeURIComponent("@opentui/solid"));
  mod = await import("../tui-compiled/index.tsx");
} catch {
  mod = await import("./index.tsx");
}
export default mod.default;
```
Nuances:
- The compiled-tree import must ALSO be inside the try (missing tui-compiled in a dev checkout falls through to raw TSX).
- Keep `"types"` pointing at `./src/tui/index.tsx`.
- Verify the raw-TSX fallback still works in bare bun with deps installed (that is today's smoke).

### 3. package.json wiring
- exports."./tui" = { types: "./src/tui/index.tsx", import: "./src/tui/entry.mjs" }
- files: add "src/tui-compiled"
- scripts: add "build:tui": "bun scripts/build-tui.ts"; chain it in prepublishOnly after build.
- @opentui/solid stays a regular dependency (fallback path + transform source at build time). Do not change dep pins.

### 4. Smoke upgrades (both must be non-vacuous)
- Extend packages/plugin/scripts/smoke-tui-pack-install.ts: after the pack+prod-install, (a) assert `src/tui-compiled/index.tsx` EXISTS in the installed package and contains the string `opentui:runtime-module:` (proves the compiled tree shipped); (b) import the entry via a bun -e probe that FIRST registers stub virtual modules via Bun.plugin build.module for all 8 runtime ids (exporting the installed package's own @opentui/solid, @opentui/core, solid-js copies), then imports the packaged entry.mjs and asserts the default export object — this exercises the compiled path exactly as a 0.4.x host would. Also keep a second probe WITHOUT the stubs asserting the raw-TSX fallback still loads.
- REACTIVITY smoke (the class-killer — the frozen-UI bug passed every import-level check): in the same stubbed-virtual-modules probe, mount a minimal reactive proof: import createSignal/createEffect through the stubbed solid-js virtual id, evaluate a tiny compiled JSX snippet... if mounting real renderables is impractical in the probe, the minimum acceptable assertion is that the compiled sidebar-content output contains `_$effect(` or `_$insert(` wrapper calls around dynamic expressions (the compile-time reactive bindings the runtime path lacks) AND that the RAW tsx source does not. Grep-level but honest: it distinguishes compiled-reactive from runtime-frozen output.

### 5. Build + gates
- bun run build:tui produces the tree; run the extended smoke locally.
- cd packages/plugin && bun test --timeout 60000, bun run typecheck, repo lint, check_comments.
- Comments explain the eager-evaluation failure mode and why virtual ids are load-bearing; no references to this task/plan.
- Commit with trailer: `Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>`

## Do NOT
- Do not remove or downgrade the runtime deps (@opentui/core, @opentui/solid, solid-js) — they are the fallback path.
- Do not touch src/tui sources' behavior (this is packaging only).
- Do not compile src/shared (imported by both runners; no JSX there).
