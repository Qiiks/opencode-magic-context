# Fix: shadow kill switch must not gate the rust-mode authority sync

Repo: this worktree (branch from `subc-migration` HEAD). Rust (crates/mc-module) + possibly a small TS touch. Investigation-first: verify each claim below at source before changing code.

## Live evidence

Rust-mode beat on session `ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO` (real session id, NOT shadow-namespaced) failed with `shadow lane is disabled by configuration`. Dispatch arm at crates/mc-module/src/lib.rs:6926 gates `state_sync | shadow_transform | shadow_reset` on `shadow_lane_enabled()` (user-tier `shadow_transform.enabled`, currently false as a mirror-lane kill switch). The rust-mode ADAPTER (authority, not mirror) sends `state_sync` via the shared module-state-sync service, so the mirror kill switch blocks the authority engine. Wrong scoping: the kill switch exists to stop runaway MIRROR traffic; a rust-mode session's sync is the engine's own state feed.

## Fix

1. Gate by lane, not by method: `shadow_transform` and `shadow_reset` stay globally gated (inherently mirror ops). `state_sync` is gated ONLY when the target session is shadow-namespaced (`shadow:` prefix on the session id in the request body / bound identity — verify how the binding carries it; the shadow sender binds `shadow:<sid>`, the rust-mode adapter binds the real sid). Authority `state_sync` for a non-shadow session must dispatch regardless of the kill switch.
2. VERIFY STORAGE COHERENCE for authority sessions (this is the part that needs honest investigation, not assumption): `handle_shadow_state_sync_value` was built for the mirror lane. Trace where it writes (shadow-scoped tables vs session-keyed shared tables) and where the rust-mode transform's m0/m1 compose READS for a non-shadow session (compose_m0_from_store et al: the `shadow:` prefix selects shadow-scoped reads; owned/real sessions read the global tables). If an authority state_sync for a real session id writes into a store location the authority compose does not read, the sync is a silent no-op and the transform will fail differently after the gate fix (boundary absent / empty memories). If you find that split, fix it: authority-session state_sync must land compartments/memories/profile/marker state where the real-session compose reads them. Report exactly what you found with file:line either way.
3. Naming/log hygiene: the rejection error and any authority-path logs must not say "shadow" for non-shadow sessions.

## Tests (fail-first where applicable)

- Kill switch OFF + real-session `state_sync` -> dispatches and applies (fail-first against today's gate).
- Kill switch OFF + `shadow:`-namespaced `state_sync` / `shadow_transform` / `shadow_reset` -> rejected with `shadow_disabled`.
- Kill switch ON + both lanes work as before.
- End-to-end authority test: seed a real session's compartments + memories via authority state_sync (kill switch OFF), run a transform for that session, assert m0 composes the seeded compartments and memories (this is the coherence proof for finding 2; if storage was already coherent, this test pins it).

## Gates

cargo test -p mc-module (all suites), clippy clean. If TS changes were needed, focused plugin suites + typecheck. Comments explain the lane-vs-method invariant without referencing this incident. No em-dashes. Report: findings from step 2 with file:line, files touched, test names.
