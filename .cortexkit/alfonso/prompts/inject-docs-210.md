# Fix #210: dreamer.inject_docs=false still injects ARCHITECTURE.md / STRUCTURE.md

Regression class: config semantics lost in a refactor. When `<project-docs>` moved from the system prompt into the m[0] baseline (the m[0]/m[1] cache layout), the `dreamer.inject_docs` gate was left behind on the old path and never threaded into the new one. Result: the flag does nothing on either harness.

## Verified current state
- OpenCode: `packages/plugin/src/hooks/magic-context/hook.ts:817` computes `injectDocs: deps.config.dreamer?.inject_docs !== false` and passes it to `createSystemPromptHashHandler` — whose options declare `injectDocs` (`system-prompt-hash.ts:132`) but NEVER use it. Dead plumbing.
- OpenCode m[0] compose reads docs UNCONDITIONALLY: `packages/plugin/src/hooks/magic-context/inject-compartments.ts` — `readProjectDocsCanonical` at ~line 1523 (materialize path) and ~line 2217 (fallback/marker path), plus `computeProjectDocsHash(projectDirectory)` at ~line 960. Also check every other `readProjectDocsCanonical` / `computeProjectDocsHash` call site in packages/plugin (grep both).
- Pi mirror reads docs UNCONDITIONALLY in `packages/pi-plugin/src/inject-compartments-pi.ts` at ~lines 850, 995, 1172, 1257, 1320, 1371 (grep `readProjectDocsCanonical` for the authoritative list). Pi's `index.ts:1095-1098` gates only the system-prompt block builder (`buildMagicContextBlock`), not the m[0] render.

## Required semantics
- The m[0] `<project-docs>` block is injected iff `dreamer.inject_docs !== false` (default true). FLAG-ONLY on BOTH harnesses — do NOT couple it to dreamerEnabled/dreamerRunnable: docs files exist on disk regardless of whether the dreamer maintains them, and users hand-author them. (Pi's old system-prompt-era coupling to dreamerRunnable is not carried over; the m[0] path is the only path now.)
- Config is resolved per-pass/per-project already at the callers (OpenCode: deps.config in hook.ts / transform options; Pi: effectiveConfig with project switching). Thread the boolean through the injection options to every docs read site — do not read config files inside inject-compartments.

## Cache-stability invariants (this is m[0] byte territory — get these exactly right)
1. Gate the BLOCK and the HASH together, at every site: when disabled, use `{ renderedBlock: "", canonicalHash: "" }` — the exact shape of the existing `projectDirectory`-missing path, which is the byte-stable precedent. Never gate one without the other (a live hash with an empty block, or vice versa, makes CAS comparisons and snapshot markers disagree across passes).
2. Deterministic per pass: with the flag off, EVERY site must produce the same empty pair on every pass (execute, defer, fallback, markers). A site left ungated would cause byte drift between materialize and replay.
3. Flipping the flag mid-session must NOT force a HARD fold. projectDocsHash is deliberately NOT a materialization trigger (docs changes fold in on the next natural bust) — preserve that. With the flag off the hash becomes "" and stays "", which reads as a docs-content change absorbed at the next natural HARD; verify no mustMaterialize trigger fires from it (there is a test suite around this — the docs-defer behavior).
4. Pi parity: identical gating in every Pi mirror site; Pi's per-project config resolution (project switch) decides the flag per pass.

## Cleanup (do not leave the dead path)
- Remove the dead `injectDocs` field from `createSystemPromptHashHandler` options (`system-prompt-hash.ts:132`) and its pass-through at `hook.ts:817` — it gates nothing since docs left the system prompt. Also `packages/plugin/src/index.ts:483` destructures `inject_docs: _injectDocs` — check what that site does and clean it up if it is also residue.
- Pi `index.ts:1095-1098`: `buildMagicContextBlock`'s `injectDocs` — check whether that block builder still renders any docs (if it is dead there too, remove; if it still renders something, gate it consistently with the same flag-only semantic).
- Check `packages/plugin/src/config/schema/magic-context.ts:171` — the `inject_docs` `.describe()` text must match the actual behavior (m[0] `<project-docs>` injection, default true). Regenerate the JSON schema if the description changes (`bun packages/plugin/scripts/build-schema.ts`).

## Tests
- OpenCode: with `inject_docs: false`, a materialized m[0] contains NO `<project-docs>` block and NO docs bytes; a defer pass replays byte-identical. With the flag unset/true, docs render (regression guard).
- Flag-off does not trigger mustMaterialize by itself on a defer pass (cache-neutrality of the gate).
- Pi: mirror test in the Pi injection suite (same assertions).
- The existing docs-defer tests (projectDocsHash not a fold trigger) must stay green unmodified — if one needs edits, explain why in the commit message.
- Follow existing test styles in inject-compartments tests (both packages).

## Gates
- `cd packages/plugin && bun test && bun run lint && bunx tsc --noEmit`
- `cd packages/pi-plugin && bun test && bun run lint && bunx tsc --noEmit`
- `check_comments` clean.
- Commit message: explain the regression (flag stranded on the retired system-prompt path when docs moved to m[0]), the flag-only semantic decision, and the block+hash-together invariant. Reference #210 at the end.
