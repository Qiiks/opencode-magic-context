# Reply draft for GitHub issue #400

Thanks to @luelueluez7Z for localizing this. We confirmed the suspected cause
verbatim: the current Pi installation is launched from
`@earendil-works/pi-coding-agent/dist/bundle/cli.js`, while the plugin's
`isPiCliScript()` detection only matched `dist/cli.js`. That predicate therefore
missed Pi 0.84.3+ and the runner fell back to bare `pi`; on the reported
PowerShell installation that resolves only to a `pi.ps1` shim, which Node cannot
launch as `spawn("pi")`, producing `ENOENT` before any model request.

The fix stops hard-coding the CLI layout. It identifies the running
`@earendil-works/pi-coding-agent` package, reads its `package.json` `bin.pi`
entry, validates that target stays inside the package, and re-spawns the current
runtime as `process.execPath <resolved-cli-script> ...`. This covers both the
older `dist/cli.js` and current `dist/bundle/cli.js` layouts, including
symlinked bin installs, while keeping the current manifest traversal and
containment guards.

We also hardened the last-resort Windows path: after an unresolved script it
walks `PATH` with `PATHEXT` for `.exe`, `.cmd`, and `.ps1` Pi shims without using
`shell: true`. If that still cannot start, the failure now identifies the
script-detection miss and checked paths (for example,
`spawn pi ENOENT after script-detection miss on ...`) rather than hiding the
actual branch that led to the bare command.

Regression tests cover the legacy, bundled, symlink, module-path, unresolvable,
and Windows shim cases. We also temporarily restored the old `dist/cli.js`
assumption and verified that the bundled-layout test fails, then restored the
manifest-based resolution.
