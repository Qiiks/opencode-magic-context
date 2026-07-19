# Rust MC mode — U4: commands + sidebar + closing module ops (Rust + TS)

Part of the per-project Rust MC cutover (plan: `.alfonso/plans/rust-mc-mode-v1.md` v2, unit U4). Mixed unit: two module ops (Rust), the command/sidebar rewires (TS), and one small U1 residual fix (TS). Current branch HEAD contains U0/U1/U2/U5 + serve_native encode-back.

## Part A — module ops (crates/mc-module, crates/mc-store)

### A1. `session.status` gains `include_compartments_after_seq`

Optional request field `include_compartments_after_seq: N` (integer ≥ -1). When present, the ok-response additionally carries:
```json
"compartments": [ { "sequence", "start_message", "end_message", "start_message_id", "end_message_id", "title", "content", "p1", "p2", "p3", "p4", "importance", "episode_type", "created_at" } ],
"max_sequence": <highest published seq for the session, or N when none newer>
```
Rows strictly `sequence > N`, ordered ascending, capped at 50 per call (caller loops; put the cap in a named const). This feeds the TS compartment mirror-back (the adapter's `ModuleCompartmentReader` already calls exactly this shape — see `packages/plugin/src/hooks/magic-context/hook.ts` getCompartmentsAfter and `module-state-sync.ts` mirrorModuleCompartments for the exact field names TS expects; match THEM, they are the contract).

### A2. `session.recomp` becomes real: re-cut-to-zero + organic refire

Replace the current honest-stub `nothing_to_do` disposition. Module-native recomp is NOT a staged rebuild — it is the existing #423 re-cut machinery pointed at ordinal zero: (a) under the recomp latch + command ledger (both exist), run the re-cut/truncation path that deletes the session's compartments and cache-state rows so the boundary returns to never-minted (find `truncate_compartments_for_revert` / the revert re-cut arm; reuse, do not fork), bumping the revert/generation fences so any in-flight historian publication for the session fail-loud aborts (the publication fence machinery exists); (b) return disposition `started`; (c) the next transform passes re-fold organically from raw history (pressure-driven historian refire) — recomp does NOT synchronously run the historian. Preserve: command-id idempotent replay (`load_recomp_command` path stays), `already_in_progress` via the latch, and `nothing_to_do` ONLY when the session has zero compartments and a never-minted boundary. Tests: started deletes rows + resets boundary + fences in-flight publication (fail-first: without the fence bump a stale publish would land); replay of same command_id returns the recorded disposition without re-cutting; concurrent second command → already_in_progress.

## Part B — TS command + sidebar rewires (packages/plugin)

For rust-mode sessions only (ts-mode untouched, tests prove):
1. `/ctx-wrapup` → `session.wrapup` op (exists; deferred-binding is a CC-shim concern — the OC leg calls it directly with the session id, keep in [5,100], command_id minted like U3's). Progress UX: keep the existing start notification; completion reads the structured dispositions (completed/nothing_to_compact/already_in_progress/failed + rounds) into the existing user-facing wording.
2. `/ctx-status` → `session.status` structured fields mapped into the existing status dialog data (usage/boundary/coverage/counts). Fields the module doesn't serve (embedding drain state etc. — TS-owned) keep their TS sources; this is a merge, not a replacement.
3. `/ctx-flush` → `session.flush` (exists: arms one-shot defer→execute promotion). Existing flush wording unchanged.
4. `/ctx-recomp` → `session.recomp` (Part A2 contract), mapping dispositions to the existing recomp start/busy wording.
5. Sidebar snapshot: when mode is rust, the RPC snapshot builder sources pressure/boundary/compartment numbers from the last `session.status` (cache it per session with a short TTL ~2s to avoid per-render module calls; TS-owned rows like notes/memories counts keep their context.db sources).
6. `/ctx-embed`, `/ctx-dream`, `/ctx-aug`: explicitly unchanged (TS-owned subsystems) — one test asserting `/ctx-embed` still works in rust mode.

## Part C — U1 residual fix (TS, small)

In `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts`: the `need_full_sync` retry sends the UNPAGED `body` while the original send used `buildPagedModuleTransformPayloads(body)`. Fix: the retry must re-page (same helper) and send all pages. Add a test with a mock forcing need_full_sync on a payload large enough to page (or assert the pager is invoked on the retry path regardless of size).

## Gates

- `cargo test -p mc-module -p mc-store`, clippy clean.
- Plugin: command-handler + sidebar + rust-mode-transform suites; full `bun test` (known pre-existing noise: late "Database has closed" runner failure + TS5097 script typecheck — report, don't chase).
- ts-mode byte-neutrality: every rewire is behind the mode check; existing command tests green.

Commit in the worktree; do not push. Report exact wire shapes for A1/A2 as implemented and anything you were tempted to defer.
