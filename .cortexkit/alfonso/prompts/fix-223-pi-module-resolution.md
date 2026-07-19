# Fix issue #223: pi-coding-agent module resolution fails on symlinked Pi installs

Repo: /Users/ufukaltinok/Work/Projects/CortexKit/magic-context. Work in `packages/pi-plugin/` only.
Do NOT push. Do NOT touch `crates/` or `packages/plugin/`.

## The bug (source-verified)
`packages/pi-plugin/src/dreamer/pi-session-api.ts` line 23 dynamically imports
`@earendil-works/pi-coding-agent` as a BARE specifier:

```ts
const mod = (await import(/* @vite-ignore */ PI_CODING_AGENT_MODULE)) as {...}
```

Node resolves bare specifiers by walking `node_modules` up from the importing file's REAL path
(symlinks resolved). A user (issue #223) manages their Pi agent dir in a symlinked dotfiles repo:
the plugin's realpath is `~/Projects/.../dotfiles/pi/agent/npm/node_modules/@cortexkit/pi-magic-context/dist/index.js`,
so the walk-up happens inside their dotfiles tree and never finds `@earendil-works/pi-coding-agent`
(which lives next to the Pi CLI's own install) → `ERR_MODULE_NOT_FOUND`, breaking the retrospective
scanner, the primer raw-provider, and the index.ts orphan-sweep path (the 3 callers of
`loadDefaultPiSessionApi`).

This is the same layout-fragility class as the onnxruntime resolution fixed for issue #128
(layout-agnostic resolution rather than hardcoded walk-up assumptions).

## The fix: a resolution ladder in loadDefaultPiSessionApi
Replace the single bare import with a ladder that tries, in order, stopping at the first success:

1. **Bare import** (today's behavior — works for normal npm-managed installs where the package
   tree is physically nested).
2. **Resolve from the running Pi binary's entry**: the plugin always runs INSIDE the Pi process, so
   `process.argv[1]` is Pi's entry script, and `@earendil-works/pi-coding-agent` is always
   resolvable from there. Use `createRequire` from `node:module`:
   ```ts
   const require = createRequire(process.argv[1]);
   const resolved = require.resolve("@earendil-works/pi-coding-agent");
   const mod = await import(pathToFileURL(resolved).href);
   ```
   Guard: `process.argv[1]` may be undefined in exotic embeddings — skip the rung if so.
   NOTE: pi-coding-agent may be ESM-only, so `require.resolve` is used ONLY for path resolution;
   the actual load stays dynamic `import()` of the resolved file URL.
3. If every rung fails, throw ONE clear error that names all attempted strategies and states the
   likely cause (symlinked/nonstandard install layout) — so the next diagnostic report is
   self-explanatory. Keep the existing "Pi session APIs unavailable" error for the
   listAll-missing case, unchanged.

Keep the module-level behavior lazy (resolution happens inside loadDefaultPiSessionApi on first
call, as today). Memoize the successful strategy for the process lifetime (the ladder should not
re-probe rung 1's failure on every call — cache the loaded module promise, which the current code
does not do; add it, it also removes repeated import cost from the 3 call sites).

IMPORTANT subtlety: rung 2's `createRequire(process.argv[1])` — verify with a test that this
resolves through Pi's own node_modules. In the test you cannot run a real Pi binary; instead unit
test the LADDER MECHANICS with injected loader functions (see tests below), and keep the
production rungs thin wrappers.

## Structure
Refactor `loadDefaultPiSessionApi` so the ladder is testable:
- `resolvePiCodingAgentModule(loaders?: ModuleLoader[])` — walks the ladder, returns the loaded
  module, memoizes. Default loaders = [bareImportLoader, argv1RequireLoader].
- Keep the exported `loadDefaultPiSessionApi` signature and its returned `PiSessionApi` shape
  IDENTICAL (3 production callers: retrospective-raw-provider-pi.ts, primer-raw-provider-pi.ts,
  index.ts — do not touch them).

## Tests (co-located, packages/pi-plugin/src/dreamer/pi-session-api.test.ts exists — extend it)
1. Ladder order: first loader succeeds → second never called.
2. First loader throws ERR_MODULE_NOT_FOUND → second loader's module used.
3. All loaders fail → single aggregated error naming both strategies (assert message mentions the
   symlink/layout hint).
4. Memoization: two calls → loaders invoked once.
5. Existing tests keep passing unchanged (SessionManager.listAll shape checks etc.).

## Gate
cd packages/pi-plugin && bun test (full pi-plugin suite green), bunx tsc --noEmit, bun run lint
(biome). Also run the plugin suite root gate if fast. Report exact counts + files changed. Commit
on the current branch with a clear message referencing issue #223 + co-author trailer per repo
convention (check git log). Do NOT push.
