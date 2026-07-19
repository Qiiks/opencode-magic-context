# Audit-D fixes: facade DoS, Pi doctor migration, setup races, dual-stack, Windows .cmd (v0.32.1)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Verified findings from the CLI/smart-notes/storage audit. Verify each at source. Two HIGH release blockers first.

## Fix 1 (HIGH): malformed ctx_reduce range → overflow/DoS on the Rust facade
crates/mc-module/src/lib.rs:3609-3636 (reached from :2674-2698), parse_tag_range_string. Input like drop="0-18446744073709551615" overflows `end - start + 1`: checked builds PANIC (kills the transform plane for a CC agent), wrapping builds bypass the 1000-element guard and attempt ~2^64 iteration (hang). A CC agent can send anything through the facade.
FIX: parse range endpoints as bounded positive integers (1..=i64::MAX; tag ids/ordinals are positive), reject non-positive/zero/out-of-range; compute range size with checked arithmetic (checked_sub/checked_add) and reject before the 1000-element guard rather than after overflow. Malformed input → a clean facade error, never panic/hang.
Test: the exact "0-18446744073709551615" case → clean rejection; "5-3" reversed → rejection; a valid "3-5,8" → parses; oversized-but-valid (> 1000 elems) → the existing guard rejects.

## Fix 2 (HIGH): ordinary Pi doctor migrates the shared DB
packages/cli/src/commands/doctor-pi.ts:18-22,115-124,572-630 calls core openDatabase() (which runs migrations, storage-db.ts:1535-1544). A user with an installed v50 plugin running `npx ...@latest doctor --harness pi` gets migrated to v51 → their older plugin fails its schema fence on next restart (the exact v26/v41 incident class). Same check the CLI Highs fixed for opencode doctor (openExistingContextDatabase) — Pi doctor was missed.
FIX: normal Pi doctor health checks use openExistingContextDatabase(path, {readonly:true}) (the fence helper from lib/database-access.ts — existing-only, readonly, no migration). Reserve migration-capable openDatabase for EXPLICIT maintenance commands only (migrate). Mirror opencode doctor's exact approach.
Test: a v50 DB fixture + Pi doctor run → DB REMAINS v50 (assert version unchanged), doctor reports gracefully (fenced/degraded, not a migrate).

## Fix 3 (MED): setup commit-time re-detect + Pi adapter tri-state
setup-opencode.ts:45-52,100-104,303-317,440-469 snapshots format==="none" BEFORE prompts, then unconditionally writes — if the target appears meanwhile, the atomic rename clobbers it. FIX: re-detect + parse/merge the target immediately BEFORE commit (not from the pre-prompt snapshot); if it now exists, merge into it. ALSO adapters/pi.ts:50-63,173-189 still maps parse-failure→null→`?? {}` before writing (the H1 class, even if no prod caller reaches it today) — replace with the tri-state jsonc helper (lib/jsonc-config.ts) so a parse error aborts rather than overwrites.
Test: target created mid-wizard → setup merges, does not clobber; malformed existing Pi settings → abort, file unchanged.

## Fix 4 (MED): dual-stack hosts rejected by IPv6 deny
smart-notes/ssrf-guard.ts:151-185 throws when ANY DNS answer is IPv6 — so a host with safe public IPv4 + an ordinary global AAAA fails entirely. FIX: FILTER out IPv6 candidates, validate every remaining IPv4 candidate, reject only when NO IPv4 remains. Pinned lookup still guarantees zero IPv6 egress (we connect only to a validated IPv4). This preserves the Fork-3 security property (no IPv6 egress) while restoring dual-stack reachability.
Test: host with public IPv4 + global AAAA → allowed, pinned to the IPv4; IPv6-only host → rejected; IPv4 loopback/RFC1918 → still rejected.

## Fix 5 (MED): Windows .cmd/.bat shims detected but not runnable
find-on-path.ts:41-49 returns .cmd/.bat; pi-helpers.ts:37-57 invokes them directly via spawnSync/execFileSync — Node can't exec Windows command scripts without cmd.exe/shell, so an npm-installed pi.cmd yields no version/models and bypasses the min-version gate (setup-pi.ts:253-268). FIX: execute .cmd/.bat shims through ComSpec (cmd.exe /c) — a small helper that detects a .cmd/.bat target and routes through %ComSpec%. Do NOT use shell:true with interpolated args (injection — the #177 lesson); build argv explicitly. Keep POSIX path unchanged.
Test: a .cmd target → invoked via ComSpec, version parsed; a POSIX binary → unchanged path.

## Fix 6 (MED, not independently gating but cheap): migrate IN(?) bind-limit
commands/migrate.ts:573-603 builds one bind per message → "too many SQL variables" on long sessions. FIX: batch the part lookups (chunked IN lists within the existing read transaction) or join against a session-scoped subquery. Keep it inside the one deferred read transaction (don't reintroduce the snapshot-consistency bug the txn fixed).
Test: a session exceeding the SQLite variable limit (~32k/999 depending on build) migrates successfully.

## Fix 7 (LOW): clear vs in-flight backfill orphan
storage-meta-session.ts:165-195 deletes tool_owner_backfill_state, but an in-flight no-match backfill recreates it via the unconditional UPSERT (tool-owner-backfill.ts:291-300,490-506). FIX: make the terminal transition update-only after lease acquisition (or condition insertion on matching tags still existing) so a cleared session can't be resurrected. Small; include if it doesn't balloon.

## Gates
packages/cli + packages/plugin + crates as touched: bun test, typecheck, lint, build (cli), cargo test/clippy/fmt (mc-module), check_comments. Comments explain invariants (positive-bounded range parse; doctor must not migrate; commit-time re-detect; IPv4-filter-not-host-reject; ComSpec for windows shims), no audit refs. Large batch — if one finding balloons, land the rest and report it deferred with reason. Report per-fix status + test evidence.
