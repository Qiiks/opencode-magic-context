# GitHub issue #402: install-safe native embedding accelerators

## Topology found

Before this change, both published plugin packages declared
`@huggingface/transformers` in `dependencies`. At the lockfile's resolved
version, `@huggingface/transformers@4.2.0` declares
`onnxruntime-node@1.24.3` as a regular dependency, not an optional dependency.
That package has `postinstall: node ./script/install`. `packages/cli` does not
declare either package; it only probes installed plugin trees for doctor.

That regular transitive edge is why a failed native download aborts the Pi npm
install rooted at `~/.pi/agent/npm`: npm treats the failure as required before
the plugin can be installed.

## Reproduction record

The issue's Node 24 Linux trace is a valid failure mode: its installer rejects
the unfollowed HTTP 302 while downloading the build list. I ran the requested
clean reproduction against the same resolved package (`onnxruntime-node@1.24.3`)
on 2026-08-31 with Node 24.16.0, npm 11.13.0, and Darwin arm64. The postinstall
completed successfully in this environment, so the transient 302 was no longer
reproducible here; this report does not claim to have reproduced a failure that
did not occur.

The structural mechanism was instead verified with a disposable npm fixture:
a direct optional native package whose `postinstall` exits 1 is pruned while
npm exits 0 and retains the required plugin package. Bun 1.4.0 also exits 0;
it blocks the untrusted lifecycle script rather than running it. This proves
the relevant package-manager behavior without depending on a transient CDN
response.

## Fix

`@huggingface/transformers` is now a build-only dependency. Each plugin build
emits two lazy artifacts into `dist`:

- a Node-condition transformers chunk that imports `onnxruntime-node` only when
  local native embeddings are initialized;
- `transformers-web.js`, built with Bun's browser condition for the existing
  WASM fallback.

Published plugin manifests now declare `onnxruntime-node@1.24.3` and
`sharp@^0.34.5` as direct `optionalDependencies`; both are native modules loaded
by transformers' Node entry. `onnxruntime-web` is a regular direct dependency
of both plugins, so the fallback stays available after npm prunes an optional
native package. There is no published regular dependency path to
`@huggingface/transformers` or `onnxruntime-node`.

The runtime keeps the existing native-to-WASM flow. A missing native module
loads the bundled web transformers artifact; if that fails too, local embeddings
are disabled and the existing doctor/remote-provider guidance is logged.
Doctor's Pi probe now explicitly covers a fully absent `onnxruntime-node` with
WASM present. The native happy path remains native.

A lockfile/manifest fence test covers both published plugins: transformers may
not return to runtime dependencies, native accelerators must remain optional,
WASM must remain regular, and each build must emit the browser-condition entry.
The fence was deliberately broken by moving `onnxruntime-node` back to a hard
plugin dependency; it failed, then was restored. Existing version fences
were not moved.

## Verification record

- `npm` direct-optional fixture with a deliberately failing postinstall: passed;
  npm retained the plugin and removed the failed native package.
- `bun` direct-optional fixture: passed; Bun installed the tree and safely
  blocked the untrusted failing lifecycle script.
- Packed Pi plugin installed at a temporary `~/.pi/agent/npm` equivalent with
  npm: passed; no `@huggingface/transformers` package was installed, optional
  native ONNX and Sharp installed, and the bundled native transformers chunk
  loaded.
- The same packed Pi plugin installed with `npm --omit=optional`: passed; no
  native ONNX or Sharp package was present and `dist/transformers-web.js` loaded
  its `pipeline` export.
- Focused runtime, doctor, and install-shape tests: passed, including absent
  native module -> WASM and doctor reporting.

## Reply draft

Thanks for the report. The immediate cause is upstream
`onnxruntime-node@1.24.3`'s `postinstall` build-list download: it can reject an
HTTP 302 instead of following it. That is the behavior tracked by
[microsoft/onnxruntime#32245](https://github.com/microsoft/onnxruntime/pull/32245).

We changed the plugin packaging rather than publishing a separate OpenAI-only
package. Native ONNX (and the Node-only Sharp helper that transformers loads)
are now direct optional accelerators. The transformers code ships in lazy plugin
artifacts, while the WASM runtime remains a regular dependency. If a native
postinstall fails, npm still installs the plugin; local embeddings use the
existing WASM fallback, and remote `openai-compatible` or Synapse embeddings
continue without any local ONNX package.

Until the fixed release is available, the verified workaround is to install the
Pi package through its npm root while omitting optional native packages:

```sh
npm --prefix ~/.pi/agent/npm install --omit=optional @cortexkit/pi-magic-context@<fixed-version>
```

That intentionally disables local native embeddings for that install but keeps
the plugin, remote OpenAI-compatible embeddings, and the rest of Pi working. We
did not find a documented environment switch for the affected onnxruntime-node
postinstall, so this reply does not recommend an unverified one.

The package manifests on this branch are version `0.41.0`; this fix is carried
by the `0.41.0` publish produced from this commit (or the next patch if `0.41.0`
has already published before merge). No separate openai-only package is needed.
