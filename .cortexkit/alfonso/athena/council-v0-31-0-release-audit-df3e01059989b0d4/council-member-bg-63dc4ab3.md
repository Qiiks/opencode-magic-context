## Finding 1: OpenCode m[0]/m[1] cache does not invalidate when project identity changes
- **Severity**: P1 should-fix
- **Location**: `packages/plugin/src/hooks/magic-context/inject-compartments.ts:629-648`, `:1026-1116`, `:1713-1729`; caller at `transform.ts:1768-1781`
- **Confidence**: high
- **Issue**: OpenCode resolves a per-pass `projectIdentity` and passes it into m[0]/m[1] rendering, but the cached m[0] snapshot does not store or compare project identity. If a session initially materializes under a cold `dir:` fallback and later resolves to `git:...`, cached m[0] can continue serving the old project-memory baseline until some unrelated hard bust.
- **Evidence**: `M0M1State` tracks many cached markers but not `cachedM0ProjectIdentity` (`inject-compartments.ts:629-648`). `mustMaterialize` compares model/system/TTL/epoch/mutation/upgrade markers, but no project identity (`:1026-1116`). `persistCachedM0` supports `projectIdentity` (`storage-meta-shared.ts:460-474`), but OpenCode’s materialize call omits it (`inject-compartments.ts:1713-1729`). Pi has the missing guard: `cachedM0ProjectIdentity !== state.projectIdentity` returns `project_change` (`packages/pi-plugin/src/inject-compartments-pi.ts:926-934`).
- **Suggested Fix**: Thread `projectIdentity` through OpenCode `M0SnapshotMarkers`/`M0M1State`, persist it in `persistCachedM0`, and hard-fold when a non-null cached identity differs from the current identity. Treat legacy null as “unknown” for one lazy adoption, matching Pi.

## Finding 2: Last-known-good git identity reuse is keyed by exact cwd, not repo root
- **Severity**: P1 should-fix
- **Location**: `packages/plugin/src/features/magic-context/memory/project-identity.ts:262-308`, `:335-348`, `:370-407`
- **Confidence**: high
- **Issue**: The resilience path can still split a live repo into `dir:` identities when the cwd changes within the same repository during transient git failure. A successful identity for `/repo` is cached under `/repo`; later resolving `/repo/subdir` during git failure looks only for `/repo/subdir` and falls back to `dir:`.
- **Evidence**: `identityCache`/`lastKnownGitIdentityCache` are keyed by `path.resolve(directory)` (`:262-304`). The fallback branch calls `reuseLastKnownGitIdentity(canonical)` only for that exact canonical directory (`:384-407`). There is no repo-root or ancestor lookup despite `hasGitDir` walking ancestors.
- **Suggested Fix**: Cache/reuse LKG identities by resolved git root/worktree gitdir, or when `hasGitDir(cwd)` is true, walk ancestor/realpath cache entries before returning `dir:`.

## Finding 3: `/ctx-wrapup` and trigger-fired historian mutual exclusion has a cross-process TOCTOU race
- **Severity**: P1 should-fix
- **Location**: OpenCode `compartment-runner.ts:112-122`, `wrapup-orchestrator.ts:249-294`, `compartment-lease.ts:13-31`; Pi `context-handler.ts:2841-2853`, `:2989-2997`
- **Confidence**: medium
- **Issue**: Trigger-fired historian checks `isWrapupInProgress` before acquiring the compartment lease, but the lease acquisition itself does not check the wrapup marker. Another process can pass the marker check before wrapup commits its marker, then acquire the lease after the marker exists and still publish during a manual wrapup window.
- **Evidence**: OpenCode checks wrapup at `compartment-runner.ts:112-118`, then separately calls `acquireCompartmentLease` at `:121-122`. The lease SQL only arbitrates `compartment_state_lease`, not `wrapup_in_progress_state` (`compartment-lease.ts:13-31`). Pi has the same split: marker check at `context-handler.ts:2989-2997`, lease at `:2841-2843`.
- **Suggested Fix**: Make lease acquisition atomically fail when an unexpired wrapup marker exists, or immediately re-check the marker after acquiring the lease and release/abort before any historian work.

## Finding 4: Wrapup marker renewal can throw uncaught from timer callbacks
- **Severity**: P1 should-fix
- **Location**: OpenCode `wrapup-orchestrator.ts:277-290`; Pi `commands/ctx-wrapup.ts:235-239`; storage `storage-meta-persisted.ts:517-555`, `:558-565`
- **Confidence**: medium
- **Issue**: The 60s renewal timers call `updateWrapupInProgress` without try/catch. That helper starts `BEGIN IMMEDIATE` and can throw on `SQLITE_BUSY`/I/O/schema errors. In a timer callback, this can crash the plugin process or leave the marker to expire rather than cleanly aborting/retrying.
- **Evidence**: OpenCode timer directly calls `renewWrapupMarker(...)` (`wrapup-orchestrator.ts:285-290`); Pi does the same (`ctx-wrapup.ts:235-239`). `updateWrapupInProgress` performs throwing DB operations with no outer error return (`storage-meta-persisted.ts:523-555`). `releaseWrapupInProgress` also throws on DB errors (`:558-565`).
- **Suggested Fix**: Wrap renewal and release in best-effort try/catch. Distinguish ownership loss (`null`) from transient DB errors; log transient renewal failures and retry before TTL, but abort cleanly if ownership is actually lost.

## Summary
Findings: P0 = 0, P1 = 4, P2 = 0. Overall verdict: **HOLD** for release-readiness until at least the identity-cache invalidation and wrapup mutual-exclusion issues are fixed or explicitly accepted. No deterministic P0 data-loss bug was proven, but the P1s directly affect the advertised v0.31.0 resilience and wrapup invariants.