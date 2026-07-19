# Dashboard: eliminate every git spawn — session_projects becomes the identity authority

## Why

A user reports the dashboard's Projects page taking >2 minutes to load. Root cause: the Rust backend re-derives project identity by spawning `git rev-list --max-parents=0 HEAD` per distinct directory on every load (`resolve_project_identity` in `packages/dashboard/src-tauri/src/project_identity.rs`), stacked across three surfaces per Projects render. Transient resolution failures are deliberately uncached, so directories on dead network shares re-pay `fs::metadata` stalls (SMB: 10-30s each) or the 5s `GIT_TIMEOUT` on EVERY load. Windows process-spawn overhead multiplies it.

The plugin already records the ground-truth mapping in the shared context.db: table `session_projects (session_id, harness, project_path, updated_at)` PK `(session_id, harness)`, where `project_path` is the resolved identity (`git:<root-sha>` / `dir:<md5-12>`), written by both harnesses when a session is observed. A parallel plugin-side change backfills historical sessions. Your job: make the dashboard a PURE READER of recorded identities and delete the git-spawn machinery entirely. After this change there must be NO `Command::new("git")` anywhere under `packages/dashboard/src-tauri/`.

## Ground truth (measured on a live install — rely on it)

- `session_projects.project_path` is always identity-form (`git:`/`dir:`), never a raw path.
- `memories.project_path` is 100% identity-form in practice (0 raw-path rows of 6,266 live). Raw paths are a theoretical legacy case only.
- `session_projects` may be missing rows for sessions the plugin never observed (pre-backfill). Unmapped is a NORMAL state to degrade through, not an error.

## The changes

### 1. New identity map loader (db.rs)

`fn load_session_identity_map() -> HashMap<(Harness, String), String>` — one indexed read of `session_projects` from the context DB (`resolve_db_path()` + `open_readonly`). Guard with the existing `table_exists` helper (pre-v36 DBs: return empty map). Parse the `harness` column via the existing `FromStr for Harness`; skip unknown harness values.

### 2. Replace every session-identity resolution with map lookups

All in `packages/dashboard/src-tauri/src/db.rs` unless noted:
- `list_opencode_sessions` (line ~3686): `identity = map lookup by (Opencode, session_id)`; unmapped → empty string. Load the map ONCE per call, not per row.
- `list_pi_sessions` (~3723): same with `(Pi, session_id)`.
- `resolve_session_info` (~4226): same; unmapped → empty (caller already skips empty).
- Session-detail sites (~3971, ~4123): same lookup; these have a session id in hand.
- `session_directories_by_identity` (~1920): INVERT the derivation. Instead of resolving every distinct `session.directory`, join: read `session_projects` rows (identity per session), read opencode.db `session.id → directory` (and Pi session metas → cwd), group directories under their recorded identity, pick the representative with the existing `dir_is_better_representative` heuristic (pure fs stats — keep). Directories of unmapped sessions contribute nothing.
- `enumerate_projects_filtered` (~2005) and sites ~2027, ~2033, ~6660: same principle. For ~2005 (project worktree → identity): derive the identity for a worktree path by looking for a mapped session whose directory equals that worktree (via the inverted map above); unmapped worktrees → skip from enumeration. Inspect each site's semantics before converting; if a site turns out to be dead code after the cutover, delete it.

### 3. `normalize_stored_project_path` (project_identity.rs)

Keep the identity-form passthrough (`git:`/`dir:` prefixes return unchanged). Replace the raw-path branch's `resolve_project_identity` call with the pure md5 `directory_fallback` (after `logical_absolute` normalization) — no spawn. This preserves grouping for genuinely non-git raw paths and is a documented, acceptable degrade for the theoretical raw-path-of-a-git-repo case (zero live rows).

### 4. Delete the resolver machinery (project_identity.rs)

Remove `resolve_project_identity`, `resolve_project_identity_strict`, `IdentityErrorClass` (if unreferenced after this), the identity cache (`IDENTITY_CACHE`, `cache()`, `clear_cache_for_tests`), `GIT_TIMEOUT`, `has_git_dir`, and the `wait_timeout`/`Command` imports THAT THIS FILE owns. Keep: `logical_absolute`, `directory_fallback`, `normalize_stored_project_path`, `basename`. Check `wait-timeout` crate usage elsewhere in src-tauri before touching Cargo.toml — remove the dependency only if this was the last consumer. Update/delete the `#[cfg(test)]` tests in this file accordingly.

### 5. Unmapped-session semantics

- `get_project_cards`: rows with empty identity are NOT grouped into any card (skip them in the accumulation loop). They have no Magic Context data to view.
- `session_matches_filter` by project_identity: empty never matches a filter — already true by string inequality; verify.
- Global session lists still show unmapped sessions (empty identity is fine in `SessionRow`); check the frontend does not crash on empty `project_identity` in the sessions table (it renders `project_display` which is unchanged).

### 6. Callers outside db.rs

`workspaces.rs` uses `normalize_stored_project_path` only (fine after #3). `db_mutations.rs`, `commands.rs`, `serve/dispatch.rs`: grep for `resolve_project_identity` and convert/remove any remaining references. serve mode dispatches to the same db:: functions, so it inherits the fix.

## Tests (extend the existing `#[cfg(test)]` modules in db.rs / project_identity.rs; fixture-DB patterns already exist)

1. `load_session_identity_map`: reads rows for both harnesses; missing table → empty map; unknown harness rows skipped.
2. A seeded temp context.db + temp opencode.db: `list_opencode_sessions` assigns identities from the map without any resolver (no git binary needed in the test env — that's the point), unmapped session gets empty identity.
3. `get_project_cards` groups mapped sessions and excludes unmapped ones; representative-directory inversion picks the main checkout over a worktree (reuse the existing `representative_dir_tests` fixture approach).
4. `normalize_stored_project_path`: identity passthrough unchanged; raw path → md5 fallback deterministically, no git.
5. Grep-guard test (structural): a test that reads the src-tauri source tree and asserts no occurrence of `Command::new("git")` — prevents regression of the whole class.

## Gates

- `cd packages/dashboard/src-tauri && cargo test` (full suite)
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` (scoped to src-tauri; the repo root also has a Rust workspace under crates/ — DO NOT touch it, and note src-tauri is excluded from the root workspace so run cargo from within src-tauri)
- `cd packages/dashboard && bun run build` (frontend typecheck; types should be unchanged)
- `check_comments` — comments explain WHY for a cold reader; never reference this plan, the parallel plugin change as "parallel mason", or process artifacts.

Do not touch `packages/plugin/`, `packages/pi-plugin/`, or `crates/`. Commit with a clear message.
