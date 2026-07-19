# Plugin-side: one-time session_projects backfill (both harnesses)

## Why

The dashboard's Projects page currently re-derives project identity by spawning `git rev-list` per distinct session directory on every load. On machines with many directories or dead network paths this takes minutes (user report: >2 min). The fix (landing in a parallel change) makes the dashboard a pure reader of the `session_projects` table — but that table only covers sessions the plugin has observed since migration v36 (live measurement: 1,609 rows vs 3,198 unarchived OpenCode sessions, ~50% coverage). Your job: a one-time, chunked, lease-guarded backfill that closes the gap plugin-side, where the TS identity resolver already lives.

## Repo context

- `packages/plugin/src/features/magic-context/session-project-storage.ts` — `recordSessionProjectIdentity(db, sessionId, projectPath)` upserts into `session_projects (session_id, harness, project_path, updated_at)` PK `(session_id, harness)`. The `harness` value comes from `getHarness()` (`packages/plugin/src/shared/harness.ts`), set once at boot per plugin.
- `packages/plugin/src/features/magic-context/memory/project-identity.ts` — `resolveProjectIdentity(directory)` resolves a directory to `git:<root-sha>` or `dir:<md5-12>`; per-directory cached in-process.
- `packages/plugin/src/features/magic-context/tool-owner-backfill.ts` — the precedent pattern for a boot-time one-shot backfill: own state table, lease semantics, chunked work, skip-when-source-missing. Read it before designing.
- OpenCode session source: `~/.local/share/opencode/opencode.db` (`getOpenCodeDbPath()` in `packages/plugin/src/features/magic-context/compaction-marker.ts`), table `session` with columns `id`, `directory`. Open read-only via the existing `openOpenCodeDb()` helper (`features/magic-context/dreamer/open-opencode-db.ts`) which returns null when absent.
- Pi session source: `packages/pi-plugin/src/pi-session-api.ts` (`resolvePiSessionApi`, `listAll`) — Pi sessions are JSONL files carrying a cwd. See `packages/pi-plugin/src/dreamer/primer-raw-provider-pi.ts` for a consumer.

## Design (locked — do not deviate without flagging)

1. **Shared core** in `packages/plugin/src/features/magic-context/session-project-backfill.ts`:
   - `runSessionProjectBackfill(db, sessions: Array<{ sessionId: string; directory: string }>): BackfillResult` — for each input session that has NO `session_projects` row for the current harness:
     - Skip if `directory` is empty.
     - **Skip if the directory no longer exists on disk** (`existsSync`). Do NOT record a guessed identity for dead directories: `session_projects` is also the chunk-backfill scoping authority, and a `dir:` fallback recorded for a deleted git worktree would durably mis-scope it. Unmapped is honest; the dashboard treats unmapped as ungrouped.
     - Otherwise `resolveProjectIdentity(directory)` (cached per distinct dir, so 3k sessions over ~500 distinct dirs is ~500 resolutions) and `recordSessionProjectIdentity(db, sessionId, identity)`.
   - Wrap the whole run in a per-harness done-flag + lease so it runs to completion exactly once per harness and concurrent instances (OpenCode Desktop loads one plugin instance per project in ONE process, plus TUI processes) don't stampede. Use a small state table (`session_project_backfill_state(harness TEXT PRIMARY KEY, status, started_at, lease_expires_at, completed_at)`) mirroring `tool_owner_backfill_state` semantics: acquire via `BEGIN IMMEDIATE` transaction, stale-lease reclaim after a TTL (10 min), mark `completed` at the end. A failed/crashed run must be re-runnable (lease expiry), and partial progress is naturally idempotent (`INSERT OR IGNORE`-style upsert via recordSessionProjectIdentity).
   - Table creation: `CREATE TABLE IF NOT EXISTS` inside the backfill module itself (like `ensureBackfillStateTable` in tool-owner-backfill.ts). No schema migration needed — this is process-local state, not user data. Do NOT bump LATEST_SUPPORTED_VERSION.
2. **OpenCode wiring**: call it fire-and-forget (async, `setTimeout(0)`-deferred or equivalent — must NOT block boot or the first transform) from the plugin entry after DB open. Look at how `runToolOwnerBackfill` is invoked from `storage-db.ts` — but do NOT put this one in openDatabase (it needs opencode.db + git spawns; too heavy for the synchronous open path). Wire it where the dream-timer starts or in `src/index.ts` post-boot async. Source rows: `SELECT id, COALESCE(directory,'') FROM session` from `openOpenCodeDb()` (null → skip whole backfill quietly, normal on Pi-only installs — but this leg only runs under OpenCode anyway).
3. **Pi wiring**: same core, sessions enumerated via the shared Pi session API (`listAll` metas carry session id + cwd). Wire fire-and-forget from `packages/pi-plugin/src/index.ts` post-boot. Pi session volume is small.
4. **Logging**: one summary line per completed run (`[session-projects] backfilled N of M unmapped sessions (skipped X dead dirs) in Yms`), nothing per-session.

## Non-goals

- No dashboard/Rust changes (parallel mason owns that).
- No changes to the live recording path in transform.ts / context-handler.ts.
- No re-resolution of sessions that already have rows.

## Tests (co-located, temp-DB isolated — bunfig preload handles MAGIC_CONTEXT_TEST_DATA_DIR)

1. Backfills unmapped sessions and leaves mapped ones untouched (assert existing row's project_path unchanged).
2. Dead-directory sessions are skipped and remain unmapped (assert no row, and result counts them).
3. Idempotence: second run is a no-op (done flag).
4. Lease: a held, unexpired lease blocks a concurrent run; an expired lease is reclaimed.
5. Empty-directory sessions skipped.
Use a fake resolver seam (inject `resolveIdentity` fn, default = real one) so tests don't need real git repos; ALSO one test with a real temp git repo exercising the default seam end-to-end (`git init` + commit in a temp dir is fine).

## Gates (all must be green before you commit)

- `cd packages/plugin && bun test` (full suite, not just your files)
- `cd packages/pi-plugin && bun test && bun run typecheck`
- `cd packages/plugin && bun run typecheck`
- `bun run lint` from repo root (biome — note: double quotes, 4-space indent in packages/plugin, tabs in packages/pi-plugin; run the formatter before committing)
- `check_comments` — comments must explain WHY for a cold reader, never reference this plan, mason/audit process, or "parallel change".

Commit with a clear message. Do not touch anything under `crates/` or `packages/dashboard/`.
