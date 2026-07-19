# Two small cleanups: remove vestigial sidebar Facts row; make doctor catch a missing onnxruntime native binding (issue #212 bugs 1 + 4)

## Part A: remove the "Facts" sidebar row

Facts were retired as a render source in v2 (historian promotes facts directly to project memories; `session_facts` is vestigial). The TUI sidebar still shows a "Facts" row that is hardcoded to 0 (`packages/plugin/src/plugin/rpc-handlers.ts:225-229` explains why counting the vestigial table would mislead). A user read the bundled code and filed it as a bug ("factCount hardcoded to 0"). The right fix is removing the row, not counting the table.

- Remove the Facts `StatRow` from `packages/plugin/src/tui/slots/sidebar-content.tsx` (~line 792).
- Remove `factCount` from the RPC snapshot payload end-to-end: `packages/plugin/src/plugin/rpc-handlers.ts` (both sites: ~157, ~229/427), `packages/plugin/src/shared/rpc-types.ts:13`, `packages/plugin/src/tui/data/context-db.ts:60`, and any TUI reads (`packages/plugin/src/tui/index.tsx:184` shows `(${d.factCount})` — read the surrounding code to see what that detail label is for and remove/replace it coherently).
- Update `packages/plugin/src/plugin/sidebar-snapshot-cache.test.ts` fixtures.
- The TUI ships as raw TS via the `./tui` export and talks to the server over RPC; removing a field from the payload is safe only if the TUI code no longer reads it — make both sides consistent in this change. Note the RPC server and TUI can be version-skewed across processes (old TUI + new server): leave the wire OBJECT shape tolerant (TUI must not crash on a missing factCount — optional-chaining or ?? 0 during read is already the pattern; deleting the read entirely is cleanest).
- Do NOT touch `inject-compartments.ts` factCount — that one is real (counts rendered facts in m[0], used for logging).
- Check packages/pi-plugin for any mirrored sidebar/status surface showing facts (Pi /ctx-status output) and clean it the same way if present.

## Part B: doctor must catch a missing onnxruntime-node native binding

Report: on a Windows box where onnxruntime-node's postinstall could not download prebuilt binaries (restricted registry), embeddings failed at runtime with "the provider returned no result", while `doctor` said `Embedding provider: local (Xenova/all-MiniLM-L6-v2 bundled)` and PASS. The doctor check verifies the package resolves but not that the native binding actually loads.

- Find the doctor's local-embedding check in `packages/cli/src/` (grep for the "Embedding provider" / local embedding check; there is an existing runtime-resolvability check added for issue #128 — read it first).
- Extend it: when the provider is local, attempt to actually load the onnxruntime binding the same way the runtime does (the plugin latches `ERR_DLOPEN_FAILED` as missing-runtime — see `isLocalEmbeddingRuntimeMissing`-adjacent code or its successor in `packages/plugin/src/features/magic-context/memory/embedding-local.ts` for how the runtime detects this). A cheap require/dlopen probe of `onnxruntime-node` (or verifying the platform-arch binding file exists under `onnxruntime-node/bin/napi-v6/<platform>/<arch>/`) in the doctor process is enough. On failure: WARN with a message naming the cause ("onnxruntime-node native binding missing — its postinstall likely failed; embeddings will not work. Reinstall with network access to registry + GitHub releases, or switch embedding.provider to an HTTP endpoint").
- Runtime error message: where the local provider currently fails silently to "provider returned no result", make the missing-binding case say what is actually wrong (one clear log line naming onnxruntime-node + the doctor command). Check `embedding-local.ts` for the existing graceful-degradation path from #128 and improve its message if it is generic.
- DECLINED direction (do not do): bundling prebuilt binaries into our npm tarball.

## Tests
- Part A: sidebar-snapshot-cache.test.ts updated; a test asserting the RPC snapshot payload no longer carries factCount; TUI compiles (tsc covers the raw-TS tui/ tree — verify `bun run typecheck` includes it).
- Part B: doctor check unit test with a mocked/absent binding path → WARN with the expected message; present binding → PASS. Follow the existing doctor-check test patterns in packages/cli.

## Gates
- `cd packages/plugin && bun test && bun run typecheck`
- `cd packages/cli && bun test`
- `cd packages/pi-plugin && bun test` (only if you touched pi-plugin)
- `bun run lint` from repo root
- `check_comments`

Do not touch `crates/` or the dashboard. Commit with a clear message.
