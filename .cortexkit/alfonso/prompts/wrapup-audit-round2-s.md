# Fix 4 search/identity/parity findings from release audit (branch subc-migration)

Four verified findings from a blind release audit. Read current code first; paths relative to repo root.

## S1 (HIGH): project identity must not flip mid-session on transient git failures
packages/plugin/src/features/magic-context/memory/project-identity.ts: during a transient-failure cooldown (git timeout / dubious ownership on a dir WITH git metadata), resolveProjectIdentity returns the dir: fallback; when git recovers it flips back to git:. Memories/notes written during the cooldown window scope to dir:<hash> and become invisible under git: afterwards (orphaned rows).
Fix structurally with a last-known-good cache: add an in-process Map<canonical, gitIdentity> populated on every SUCCESSFUL strict git: resolution. On transient failure or active cooldown for a git-metadata-present dir, return the cached git: identity when present (log once); fall back to dir: ONLY when no git: identity was ever resolved this process (true cold-start failure). Keep the existing dir:-fallback caching for genuinely non-git dirs unchanged. Clear/refresh semantics: the cache is per-canonical-path and overwritten on each successful resolution (a repo re-init with a different root commit heals on the next successful resolve).
Tests: transient failure after a successful resolve returns the SAME git: identity (no flip); cold-start transient failure still returns dir: with cooldown; recovery updates the cache.

## S2 (MEDIUM): symlinked checkouts false-negative the .git fast path
Same file, hasGitDir(): walks only lexical path.resolve ancestors, so a symlink into a repo subdirectory reports not-a-repo and resolves dir: without ever spawning git. Fix: when the lexical walk misses, retry the walk from realpathSync.native(canonical) (guard with try/catch; a realpath failure just means keep the lexical verdict). Test with a tmpdir symlink into a fake repo layout (a .git FILE must also count, as today).

## S3 (MEDIUM): cross-session smart-note anchors point ctx_expand at the wrong session
packages/plugin/src/features/magic-context/search.ts loads project smart notes regardless of source session, but note hits expose only anchorOrdinal, and the ctx-search formatter (packages/plugin/src/tools/ctx-search/tools.ts) prints "@msg N" + a ctx_expand hint — which expands in the CURRENT session only (ctx-expand tools). A foreign-session note's anchor would expand the wrong message.
Fix: thread the note's source sessionId into note search results; the formatter prints the "@msg N" anchor (and the ctx_expand footer hint contribution) ONLY when the note's source session matches the searching session. Foreign-session notes render without the anchor. Mirror in Pi if Pi's search formatter shares this path (check @magic-context/core usage — the formatter may already be shared; if shared, one fix covers both, verify with a Pi-side test only if a Pi-specific formatter exists).
Tests: own-session note keeps the anchor; foreign-session smart note renders without "@msg" and the footer hint is suppressed when no hit carries an anchor.

## S4 (MEDIUM): Pi caveman replay ignores the config off switch
packages/pi-plugin/src/context-handler.ts (~3890): replayCavemanCompression runs unconditionally, so tags with a stored caveman depth keep replaying compressed text after the user disables caveman_text_compression. OpenCode gates replay on the enabled flag (see transform.ts's replayCavemanCompression call site). Fix: gate Pi's replay the same way (config off => skip replay so original text returns; the one-time cache bust on flip matches OpenCode behavior). Test: with caveman disabled and a tag carrying caveman depth, replay does not rewrite the text.

## S5 (LOW): doctor onnxruntime load-probe should not crash doctor
packages/cli/src/lib/embedding-runtime.ts probeOnnxRuntimeNodeLoad() require()s onnxruntime-node in-process; a native abort on odd platforms kills the doctor. Fix: run the load probe in a short-lived child process (spawn node -e with a JSON verdict on stdout, ~10s timeout, treat timeout/signal/nonzero-exit as load-failure with the captured stderr snippet). Keep the existing pure file-existence checks in-process. Tests: stub-level (probe function parses child outcomes correctly); do not require onnxruntime in tests.

## Gates
cd packages/plugin && bun test --timeout 30000 && bun run typecheck; cd packages/pi-plugin && bun test --timeout 30000 && bun run typecheck; cd packages/cli && bun test; root bun run lint; check_comments. Commit clearly. Do not touch the wrapup orchestrators, compartment-runner*.ts, or pi-historian-runner.ts (another worker owns those). In context-handler.ts touch ONLY the caveman replay call site.
