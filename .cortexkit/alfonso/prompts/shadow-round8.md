# Shadow lane round 8: profile seeding inert in prod + identity-drift send-fail loop

Repo: this worktree (magic-context monorepo). Two production-observed defects in the shadow transform lane (TS sender: `packages/plugin/src/hooks/magic-context/shadow-sender.ts`; Rust module: `crates/mc-module/`, store `crates/mc-store/`). Work on the current branch HEAD. Both fixes need investigation-first: reproduce, then fix, then prove with tests. Do NOT mask either symptom.

## Context

The shadow lane mirrors every OpenCode transform pass to the Rust module (`shadow:<sid>` lineages) and byte-compares outputs (`shadow_divergences` table in the module store). Round 7 (commit on this branch, see `shadow_user_profile` table, migration in mc-store) added user-profile seeding: the sender is supposed to include the project's active user-profile lines in the cold-start seed, the module stores them in `shadow_user_profile` (keyed by `shadow_project_path`, `profile_index`, `content`), and `m0_compose` reads them shadow-scoped so the Rust m0 renders `<user-profile>` between `</project-docs>` and `<session-history>` exactly like the TS renderer.

## Defect 1: profile seeding is inert in production

Evidence from the live prod store (`~/.local/share/cortexkit/magic-context/store.db`, read-only inspection only — do NOT write to it):
- `SELECT count(*) FROM shadow_user_profile` → **0 rows**, despite the round-7 dist running since the last OpenCode restart and multiple fresh lineages seeding at pass_seq=1.
- A byte-mismatch divergence (session `shadow:ses_0ad83017…`, 2026-07-16 23:22) shows the TS window containing `</project-docs>\n\n<user-profile>\n- User strongly pushes back…` while the Rust window jumps straight to `<session-history>` — the Rust m0 composed without profile.

The round-7 unit/differential tests pass, so the gap is between the tested path and the production path. Candidate mechanisms to check (in order):
1. The sender's seed assembly: is the profile item type actually included in the paged seed batches sent over the wire (check `toFlatWireBody` / seed item serialization), or only in the unpaged/test path?
2. The module's seed apply: does the paged `state_sync` apply arm handle the profile item type, or only the atomic/unpaged apply? (Round 7 landed alongside seed paging — a likely seam.)
3. Keying: `shadow_user_profile` is keyed by `shadow_project_path` — verify the sender sends the same project path the module later uses in `m0_compose` for `shadow:` sessions (a key mismatch would write rows the compose never finds — though 0 rows total points at write-side).
4. Gating: is profile inclusion gated on a flag/config that is false in prod (e.g. only when memory enabled, or a wrong default)?

Required proof: a test that drives the REAL seed path shape used in production (paged seed, multiple batches, profile lines included) end-to-end into a module store and asserts (a) `shadow_user_profile` rows exist post-apply and (b) `m0_compose` output for the shadow session contains the `<user-profile>` block byte-identical to the TS renderer for the same lines. If the existing round-7 test passes while this new test fails on unfixed code, you have found the seam — fix it and both must pass.

## Defect 2: identity-drift send-fail loop on in-flight tail messages

Evidence from the prod plugin log (42 occurrences over ~17 minutes, session `ses_331acff9…`):
```
shadow: send failed (ignored): CK message block identity drift for mid msg_f6f2a5989001aSG5t5HAk2NLA4
```
Same mid every time. Mechanism (verify at source): the sender includes the newest assistant message while it is still STREAMING (in-flight, parts still growing). The module pins block identity first-seen per mid (`enforce_block_identity`). The in-flight form gets pinned; the completed form has different/more blocks; every subsequent pass rejects with identity drift, forever. Park-on-repeat (round 7) keys on RESET reasons, not send-failure classes, so this loops indefinitely — the lineage is dead but keeps burning a send + reject every pass.

Fix shape (adjust if investigation contradicts):
1. **Sender: never pin identity from an un-completed tail message.** The transform already knows the in-flight assistant (mid-turn state). Exclude the in-flight tail message's blocks from the identity map (or mark them provisional so the module skips the drift check for them). The next pass after completion pins the stable form. Careful: the module side must agree — if the module pins server-side on first sight regardless, the fix must land there (skip pinning for the newest message when the pass is mid-turn, or pin-with-replace-allowed until the mid appears in a non-tail position).
2. **Parking: extend park-on-repeat to send-failure classes.** Same-class send failure N times consecutively (N=3) for the same session → park the lineage for process lifetime with one log line, exactly like reset-reason parking. Transient transport classes (timeout, backoff, connection) remain exempt — only deterministic rejects (identity drift, validation) park.
3. **Recovery for already-poisoned lineages**: a parked-for-drift lineage must be recoverable by the next shadow_reset/reseed (which re-pins identity fresh). Verify the reset path clears the pinned identity for the session (it should — reset drops lineage state — but prove it with a test: pin in-flight form, drift-reject, reset, re-send completed form, pass).

Required proof: a test reproducing the exact loop (send pass with in-flight tail → pin → complete the message → send again → assert NO drift reject after fix), and a parking test (3 consecutive drift rejects → parked → no further sends → reset unparks).

## Constraints

- Absolute fail-open: no shadow-path error may ever affect the live transform output. Every new code path must be inside the existing try/catch containment.
- Cache safety: zero changes to non-shadow paths. `enforce_block_identity` for NON-shadow (owned/CC) sessions must be byte-for-byte untouched — the in-flight exemption applies to shadow lineages only (CC/owned legs have their own remint machinery upstream; do not touch it).
- The module store schema may gain a migration ONLY if strictly needed (prefer none).
- Run: `cargo test -p mc-module -p mc-store` and the plugin suite `cd packages/plugin && bun test src/hooks/magic-context/shadow-sender.test.ts` plus any test file you add. Full plugin suite before finishing: `bun test`.
- Commit in the worktree with a clear message. Do not push.

## Deliverables

1. Root-cause note for defect 1 (which seam, why tests missed it).
2. Fixes + tests for both defects, all suites green.
3. List any behavior you were tempted to defer — per project rule #9245 (NO-DEFERRAL), nothing gets deferred silently; if something is out of scope, say so explicitly in your final report.
