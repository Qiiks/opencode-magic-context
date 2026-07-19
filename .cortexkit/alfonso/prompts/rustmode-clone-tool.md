# Rust MC drive prep: session clone script (OpenCode + MC data)

Build `packages/plugin/scripts/clone-session.ts` — a bun script that clones a real OpenCode session INCLUDING its Magic Context state, producing a fully independent twin session for destructive acceptance drives. This is drive tooling for the Rust MC mode plan (`.alfonso/plans/rust-mc-mode-v1.md`, "ACCEPTANCE DRIVE" gate): the clone gets driven through a real PTY opencode session with `transform_mode: "rust"` while we inspect the data underneath, and is re-cut for every iteration round.

## Usage shape

```
bun packages/plugin/scripts/clone-session.ts --session <ses_id> [--suffix <label>] [--dry-run]
```

Prints: new session id, row counts per table copied (opencode + MC), and the directory/project the clone is bound to.

## What it must do

1. **opencode.db copy** (`~/.local/share/opencode/opencode.db`): copy the `session` row plus ALL `message` and `part`-equivalent rows (inspect the actual schema first — the message table stores `data` JSON with id/session refs inside; parts may live in a separate table or inside data. Read the real schema, do not assume). Mint a NEW session id (same `ses_` prefix format + unique suffix so OpenCode accepts it), rewrite session-id references everywhere they occur (row columns AND inside `data` JSON blobs), and PRESERVE message/part ids verbatim (MC tag identity and compartment ranges key on message ids — rewriting them would orphan the MC state; a single opencode.db cannot have message-id collisions across sessions IF the schema scopes messages by session — VERIFY this: if message ids are a global primary key, the clone must remint message ids AND consistently remap them in the MC state copy below. Determine which case holds from the real schema and say so in your report).
2. **context.db copy**: reuse `copySessionStateForClone()` (`packages/plugin/src/features/magic-context/storage-clone.ts` — built for Pi clone-inheritance, runs in one immediate transaction, maps ordinals/tag keys, clears cached m0/m1 bytes to force a fresh first-pass materialization). Extend/wrap rather than fork: if it needs an id-remap hook for the opencode message-id case above, add the hook with a default identity mapping so the Pi path is untouched. Verify what it copies against the plan's needs: tags, compartments, session_meta scalars, pending ops, marker state. It intentionally clears cache bytes — correct for the drive (first rust pass does the cold-start seed).
3. **Safety**: refuse to run while an opencode process holds the DB unless `--force` (check WAL/lock or just document the requirement + busy_timeout); transaction per database; `--dry-run` prints the plan (row counts) without writing; NEVER mutate source-session rows — assert the source row count before/after and fail loud on any diff. The new session must be left in a state OpenCode can open cold (verify by listing: the session row's data JSON version fields, title, directory all valid — title gets a " (rust-drive clone)" suffix so it is unmistakable in the picker).
4. **MC exclusions**: do NOT copy channel2 claims, emergency latches, or historian lease rows (fresh drive must start clean — enumerate what session-scoped state you deliberately skip and why in the report). Message-index/FTS rows: skip (lazily rebuilt). Embeddings: skip (chunk embeddings are keyed by session; the clone can re-embed if needed).

## Verification (self-drive, no PTY needed)

- Unit test with a synthetic mini opencode.db + context.db fixture (schema-from-real, small rows): clone, assert twin independence (mutating clone rows leaves source untouched), id remapping consistency (every session-id reference rewritten; message-id policy per the case you determined), MC state completeness (tag count parity, compartment range parity after remap).
- Run against the REAL DBs in --dry-run mode and include the printed plan for one mid-size session in your report (read-only; pick any session with >1000 messages and >20 compartments from context.db).

Test-data isolation rules apply (MAGIC_CONTEXT_TEST_DATA_DIR; never write the live DBs from tests). SQLite binds: spread positional args, never array-form. busy_timeout 5000 on any writable handle.

Commit in the worktree; do not push.
