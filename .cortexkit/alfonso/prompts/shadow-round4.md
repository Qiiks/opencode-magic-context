# Shadow soak round 4: subagent gating + diff-window diagnostics + reseed cooldown

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Both sides again: TS sender (packages/plugin/src/hooks/magic-context/shadow-sender.ts + arming site in hook.ts/transform wiring) and Rust module (crates/mc-module comparator + mc-store divergence schema). Live post-bounce soak data surfaced three issues; fix all three. Cache law + fail-open discipline as always; the shadow lane must never affect the real lane.

## Fix 1 (TS sender): do not arm subagent sessions
Evidence: ~32/37 post-bounce byte-mismatches are subagent (mason/child) sessions. TS deliberately skips m[0]/m[1] injection for subagents (see session-modes: isSubagent gates injection in transform.ts), but the module has no subagent mode and composes the full m0 baseline — every subagent pass diverges at byte 0 (<worktree_context> vs <project-docs>). This is a DESIGNED mode difference, not a transform bug; comparing it is pure noise.
Change: the sender's arming decision must skip sessions the TS lane treats as subagent (same signal the transform uses — sessionMeta.isSubagent / the subagent detection the injection gate reads). Skip means: no reset, no seed, no shadow_transform, and clean up any existing armed state for that session (if a lineage exists in the module store from before this fix, leave it — it goes stale harmlessly; do NOT add a delete op). Log once per session at arm-decision time ("shadow: skipped (subagent session)"). Add a test: subagent session never produces sender traffic.
Note for the report: subagent-profile parity is intentionally out of scope (future work — the module would need a no-injection profile; banked separately).

## Fix 2 (module + store): divergence rows must localize the first differing byte
Evidence: a real primary-session byte-mismatch (the class we built the soak FOR) is undiagnosable from the table — ts_prefix/rs_prefix store the first 4096 chars from offset 0, both sides identical that deep, actual diff is beyond. 
Change in the comparator (crates/mc-module, where byte-mismatch rows are built): compute the first differing byte offset between the two serialized forms; store (a) first_diff_offset INTEGER, (b) ts_window/rs_window = a window CENTERED on the diff (e.g. 300 bytes before + 900 after, clamped), replacing the from-0 prefixes for the mismatch case. Keep the existing columns (schema: add columns via mc-store migration — follow the existing migration pattern, bump the store schema version) — write both old prefix fields (backcompat for existing readers, can stay from-0) AND the new offset+windows. Also store which message index/mid the diff falls in if cheaply derivable from the flat projection (first_mid/first_block already exist — verify they point at the DIFF location, not just the first message; if they're currently first-message-of-array, fix them to the diff-bearing block).
Tests: a deliberate mid-array byte flip produces a row whose first_diff_offset is correct and whose windows contain the differing bytes; an early-byte flip still works; identical arrays produce no row.

## Fix 3 (TS sender): reseed allowance survives module bounces
Evidence: lineages that burned their single once-per-process reseed retry against the OLD module (which ignored seed_boundary_id) are now stuck in trim-mismatch/quarantine until the next OpenCode restart, even though the NEW module would accept their seed.
Change: replace the once-per-process-per-session reseed latch with a bounded cooldown: allow a reseed attempt per session at most once per N minutes (suggest 30m) with a small lifetime cap per process (suggest 5), tracked in the existing in-memory sender state. A successful seed (accepted, no typed reject) resets the counter. This keeps the hot-loop protection (the reason the latch exists) while letting module-side fixes deploy without waiting for a harness restart.
Test: simulate reject → cooldown blocks immediate retry → after cooldown window a retry happens → success resets.

## Coordination
The mc side and TS side land together in one merge (wire shape unchanged — offset/windows are store-internal, no fixture regen expected; verify). Note which half requires which deploy: Fix 2 = module bounce (SUBC), Fix 1+3 = dist rebuild riding Ufuk's NEXT natural OpenCode restart (do NOT require one).

## Gates
cargo test -p mc-module -p mc-store + clippy -D warnings + fmt; packages/plugin bun test + typecheck + lint; check_comments clean (invariants, no incident refs). Report: the three fixes' verification evidence + confirmation of which deploy each half needs.
