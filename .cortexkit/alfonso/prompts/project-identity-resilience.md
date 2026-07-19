# Make project-identity resolution never disable Magic Context (issue #212 bug 2 + Discord dubious-ownership report)

## The failure class (two live user reports)

`packages/plugin/src/features/magic-context/memory/project-identity.ts` resolves a directory to `git:<root-sha>` by spawning `git rev-list --max-parents=0 HEAD` (execFileSync, 5s timeout). `shouldUseDirectoryFallback` only accepts `not_git_repo` and the unknown-access prefix; every other failure propagates. The callers are plugin load (`packages/plugin/src/index.ts:231`, `packages/plugin/src/hooks/magic-context/hook.ts:199`) and the per-pass transform (`transform.ts:1127,1141`), so propagation does not mean "retry later" — it means Magic Context is disabled:

- Report A (issue #212, Windows): non-git directory + slow disk → git spawn hits the 5s timeout → `errorClass: "git_timeout"` → not in the fallback list → transform throws EVERY pass → outer wrapper fails open with unmodified messages → no tagging, no historian, ever. Plus a 5s SYNCHRONOUS event-loop stall per pass from the failing re-spawn.
- Report B (Discord, opencode-server): directory HAS .git but git refuses with `fatal: detected dubious ownership in repository` (repo owned by different uid than server process) → stderr not recognized by `classifyGitError` → `errorClass: "unknown"` without the access prefix → throw at PLUGIN LOAD → OpenCode logs `failed to load plugin ... error="git rev-list failed while resolving project identity"` → no /ctx-* commands in that project at all.

## The fix (four parts, all in the shared core so Pi heals too)

### 1. No-.git fast path (kills report A's spawn entirely)
In `resolveProjectIdentity` (and anywhere else that spawns git for identity), before spawning: walk `canonical` and its ancestors up to the filesystem root looking for a `.git` entry (fs.existsSync per level; a plain FILE named .git counts — worktrees/submodules use a gitdir pointer file). If NO `.git` exists anywhere up the tree, git itself would conclude not-a-repo; return the deterministic `dir:` fallback immediately, CACHED in `directoryFallbackCache` (same as today's not_git_repo caching — and note the existing cache-revalidation in resolveProjectIdentity already re-probes `hasGitDir` on every cached hit, which handles "git init later"; verify that revalidation probe also walks ancestors now, or a subdirectory session would never notice the parent repo appearing — keep the probe cheap: nearest-ancestor .git presence only).
The existing `hasGitDir` probe checks ONLY `<dir>/.git` (non-recursive) — sessions in repo SUBDIRECTORIES currently work because git itself walks up. Your ancestor-walk must preserve that: .git in any ancestor → proceed to git spawn.

### 2. Classify dubious-ownership explicitly
In `classifyGitError`, add a stderr match for `detected dubious ownership` → new errorClass `"dubious_ownership"`. Add it to the ProjectIdentityError class union and to BOTH CHECK constraints' allowed sets IF the class is ever recorded to the failures table (check `storage-v22-backfill-failures.ts` FAILURE class list + the two `error_class` CHECK constraints in `storage-db.ts:780,1373` and `migrations.ts:838` — recording a new class string into a table whose CHECK doesn't allow it would crash; either add it to the CHECKs via a new migration OR map it to an allowed class at the recording boundary. Prefer the recording-boundary mapping — a migration for a log-table CHECK is not worth it; record it as 'unknown' there with the message carrying the detail).

### 3. Uncached fallback + retry cooldown for .git-present failures
Extend `shouldUseDirectoryFallback` to also accept `git_timeout`, `git_missing`, `dubious_ownership`, and remaining `unknown` git failures (i.e. all ProjectIdentityError classes EXCEPT `permission_denied` — keep permission_denied propagating: an unreadable directory is not safely hashable either, and assertDirectoryUsable throws before hashing anyway).
Policy for these (unlike the no-.git case): fall back to `dir:` WITHOUT populating `directoryFallbackCache` permanently, so identity flips to `git:` when git recovers. BUT add a per-directory retry cooldown so recovery probing doesn't stall every pass: a new in-process map `transientFailureCooldown: Map<canonical, untilMs>`; on a git-spawn failure of these classes, set now+5min; while within cooldown, `resolveProjectIdentity` returns the `dir:` fallback WITHOUT spawning git. After expiry the next call re-probes. This bounds report A's 5s sync stall to once per 5 minutes per directory.
For `dubious_ownership` specifically the failure is deterministic until the user fixes git config, so the cooldown just makes re-probing cheap; correct behavior.

### 4. Surface the dubious-ownership remedy to the user
Git's error names the exact fix. On first `dubious_ownership` fallback per directory per process: (a) log a clear line, and (b) deliver a one-shot session warning via the existing warning path used for config warnings in `packages/plugin/src/index.ts` (see `sendIgnoredMessage` usage there; follow the same pattern/pin semantics) with text like:
"Magic Context: git refused to read <dir> (dubious ownership — the repo is owned by a different user). Using a directory-based project identity for now, which keeps memory separate from this repo's normal identity. Fix: git config --global --add safe.directory <dir>"
Keep the wording exactly this shape (concise, names the consequence, gives the one command). For the load-path occurrence there is no session yet — log-only is fine there. Deliver the session warning from the transform/hook path where a sessionId exists, one-shot latch per (process, directory).
Pi side: log-only is acceptable (verify Pi has no equivalent one-shot warning channel already in use for config warnings in packages/pi-plugin/src/index.ts — if it does, reuse it; if not, log-only + note it in PARITY.md).

### 5. Load path hardening (belt)
Even with 1-4, wrap the plugin-load call sites (`index.ts:231`, `hook.ts:199`) so an unexpected ProjectIdentityError (permission_denied or a future class) degrades: catch, log, use the `dir:` fallback via the exported `directoryFallback`-equivalent path (add a small exported helper `resolveProjectIdentityOrFallback(directory)` if needed) instead of letting the plugin fail to load. Identity resolution must NEVER be able to prevent plugin load. Check hook.ts:381,472,479,506 and rpc-handlers/tool-registry call sites for the same exposure — per-call sites that already run inside try/catch fail-open wrappers can stay as-is; verify rather than assume, and state in your final report which call sites you left unwrapped and why they are safe.

## Cost to acknowledge in comments (WHY, no plan references)
A `dir:`-fallback session writes memories/session_projects under `dir:<hash>` until git recovers, then flips to `git:<sha>` — a bounded split that self-heals (backfill + dreamer reconcile). Staying alive with a temporary identity beats being disabled. This is a deliberate policy change from "transient failures propagate": that policy assumed retrying callers, but the real callers are plugin load and the per-pass transform, where propagation = disabled.

## Tests (all co-located)
1. No-.git fast path: temp dir without .git anywhere → resolveProjectIdentity returns dir: WITHOUT spawning git (assert via injected/mocked exec seam if one exists — check how project-identity.test.ts currently forces errors; there may be no seam, in which case assert indirectly: a directory whose PATH lacks git (env manipulation) must resolve dir: instantly, and TIMING is not an assertion).
2. Ancestor .git: session dir = repo subdirectory → still resolves git: (existing behavior preserved).
3. dubious ownership stderr → errorClass dubious_ownership → resolveProjectIdentity returns dir: fallback, uncached (a subsequent call after clearing the cooldown + fixing git resolves git:). Use the existing error-forcing pattern from project-identity.test.ts.
4. git_timeout → dir: fallback + cooldown: two immediate calls spawn git ONCE (second serves fallback within cooldown).
5. permission_denied still propagates.
6. Load-path: hook creation with a resolver that throws → hook still constructs, identity = dir: fallback (mock at the module seam like hook.test.ts does).
7. Recording boundary: dubious_ownership failure recorded to the backfill-failures table does not violate the CHECK constraint.

## Gates
- `cd packages/plugin && bun test && bun run typecheck`
- `cd packages/pi-plugin && bun test && bun run typecheck` (shared core — Pi consumes the same module)
- `bun run lint` from repo root (4-space/double-quote in plugin)
- `check_comments`

Do not touch `crates/`, dashboard, or config schema. Commit with a clear message.
