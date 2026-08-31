# GitHub #400: Pi bundled CLI subagent launch

## Source confirmation

The report's localization is correct. Registry package metadata (`npm view
@earendil-works/pi-coding-agent@<version> bin`) records this recent layout
transition:

| Pi version | `bin.pi` |
|---|---|
| 0.80.2, 0.81.0, 0.82.0, 0.83.0, 0.84.0, 0.84.1, 0.84.2 | `dist/cli.js` |
| 0.84.3, 0.84.4 | `dist/bundle/cli.js` |

That makes the Windows 0.84.4 entry
`@earendil-works/pi-coding-agent/dist/bundle/cli.js`. The previous
`isPiCliScript()` predicate only accepted a literal `dist/cli.js` suffix, so it
rejected the 0.84.3+ running entry and reached the bare `pi` fallback. This
matches the reported `spawn pi ENOENT` before any provider request.

The existing dreamer session resolver already handles the relevant installed
layouts: it canonicalizes a bin-shim symlink with `realpathSync`, walks to a
matching `pi-coding-agent` package manifest, recognizes duplicated
`dist/package.json` build metadata, and rejects manifest targets that leave the
package root. The launch resolver now follows the same package-metadata model.

## Change

- The runner identifies a running Pi entry from its containing
  `pi-coding-agent/package.json` and resolves the manifest's `bin.pi` value.
  It no longer assumes a particular `dist` subdirectory. It first walks from
  the canonicalized running entry, then resolves package metadata from that
  entry's Node module paths before trying the extension module's paths.
- The declared bin is validated as an existing regular file inside the package
  root before and after symlink canonicalization. The duplicate `dist` metadata
  parent-root correction and package-root containment protections remain in
  place.
- A successful detection spawns `process.execPath` with the resolved CLI script
  as its first fixed argument. This is the cross-platform shape required by the
  report: the child keeps the current Node/Bun runtime and Pi version rather
  than executing a Windows shim or looking up a bare command.
- If script detection cannot identify Pi on Windows, the fallback manually walks
  `PATH` with `PATHEXT`, including npm's `.exe`, `.cmd`, and `.ps1` forms. It
  never enables `shell: true`; command-interpreter invocations use fixed argv
  elements rather than a constructed command string. A remaining spawn failure
  includes the script-detection miss and checked paths, for example
  `spawn pi ENOENT after script-detection miss on ...`.

## Regression coverage

`packages/pi-plugin/src/subagent-runner.test.ts` covers the legacy
`dist/cli.js` layout, the 0.84.3+ `dist/bundle/cli.js` layout, a bin-shim
symlink, metadata resolution from the running module paths, an unresolvable
entry, and a bin target that attempts package-root traversal. Its Windows unit
matrix injects `platform`, environment, and filesystem seams so `.exe`, `.cmd`,
and `.ps1` resolution is tested without a Windows host. A runner lifecycle test
also asserts that an ENOENT names the detection miss and checked paths.

Mutation evidence: the package-bin resolver was temporarily replaced with the
old `dist/cli.js` assumption as an intentional mutation and
`bun test src/subagent-runner.test.ts --test-name-pattern "bundled dist layout"`
failed at `subagent-runner.test.ts:288`: the bundled-layout assertion received
the bare `pi` fallback instead of `process.execPath` plus
`dist/bundle/cli.js`. The manifest-based resolver was restored before the final
verification.
