# Rust MC mode — U1+U5: adapter transform authority + mode-neutral module state sync (TS)

Part of the per-project Rust MC cutover (plan: `.alfonso/plans/rust-mc-mode-v1.md` v2 — read it fully; sections "Architecture", "State authority", "Failure semantics", units U1/U5, and the v2 amendments F2/F3/F5 are your contract). TS side only (packages/plugin). Work on current branch HEAD, which contains: U0 (`config.transform_mode` resolved, "rust" only when user-tier subc present), U2 module ops (`host_directives.channel2` on the transform response, `todo_state.set`, `session.flush`), shadow round 8 (mid_turn identity handling, parking).

IMPORTANT wire caveat: the module-side `serve_native` encode-back (native OpenCode message output on the response) is being built IN PARALLEL by another mason. Build the adapter against the contract in `.cortexkit/alfonso/prompts/rustmode-encode-back.md` (request field `serve_native: true`, response field `native_messages`). Put the apply seam behind one narrow function so a field-name drift is a one-line fix. Your tests mock the module client; do not block on the Rust side.

## What this unit builds

### 1. Mode gate in the transform (`packages/plugin/src/hooks/magic-context/transform.ts`)

At the top of the transform (after the existing internal-child/subagent early-exits — internal children keep their identity pass-through in BOTH modes), when `config.transform_mode === "rust"`:
- The ENTIRE TS pipeline is bypassed: no tagging, no strips/replays, no caveman, no m0/m1 injection, no postprocess/nudge composition, no TS historian trigger, no TS emergency machinery. Structure this as an early branch into a new `runRustModeTransform(...)` (new file `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts`) so the diff to the TS path is minimal and auditable.
- Subagent sessions in a rust-mode project also route to the module (plan open item: the module has session-mode machinery; pass an `is_subagent` flag on the pass inputs — check what the shadow pass inputs already carry and extend if absent).

### 2. The authority send/apply path (promote shadow-sender machinery)

`runRustModeTransform` synchronously (this is the live prompt path, not a mirror):
1. Resolves ordinals + encodes the live array to the wire shape EXACTLY like the shadow sender does (reuse its functions — refactor shared pieces out of `shadow-sender.ts` into a new `module-wire.ts` rather than duplicating; the shadow sender keeps working, its tests must stay green).
2. Cold start: if the module has no state for the session (first rust pass, or module says so via the existing generation/reset machinery), run the SEED (state_sync paging with compartments + marker state + memories + mutations + user-profile + workspace + todo state — the full shadow seed) and then send the transform. Seeds on the authority path are ALLOWED to take time on the first pass (log timing); reuse the 512KiB paging + oversized-item continuation as-is.
3. Sends `transform` with `serializer_profile: "opencode-aisdk"`, `serve_native: true`, and the same pass inputs the shadow lane sends (usage, thresholds, cache_ttl, mid_turn, provider_error...). Transform-class frame budget applies (32MiB module-side); page the request the same way the shadow lane does.
4. Applies `native_messages` VERBATIM as the transform output (the adapter is a byte-transparent pipe: no re-heal, no re-order, no field surgery). Healing authority is module-side (plan F4).
5. Delivers `host_directives.channel2` if present via the EXISTING channel-2 lease machinery (claim in context.db, promptAsync delivery) — the module emits idempotently; the TS lease remains the sole dedup/delivery authority (plan F5). Reuse the existing channel-2 delivery function; only the trigger source changes.
6. Compartment mirror-back (plan F3): after each successful pass, compare the module's published compartment watermark (the response carries coverage/boundary info; if the max published seq is not on the response, add a cheap read via session.status fields or extend the response contract — prefer what exists) against context.db's mirrored watermark for the session; copy missing compartment rows into context.db in ONE transaction with ON CONFLICT(session, sequence) DO NOTHING. Rows come from the module store... the module store is not directly readable from TS (different DB file, module-owned). So mirror-back needs module data: check what the transform response / session.status exposes; if compartment CONTENT is not exposed by any existing op, add a TODO-FREE honest solution: extend `session.status` request with `include_compartments_after_seq: N` module-side is NOT in your scope (TS-only) — in that case implement the mirror-back against a new thin client function with a clearly-typed interface and a mock, mark the module op as the one dependency in your report, and DO NOT fake data. (The parent will land the module op; your interface + tests must be real.)
7. Forwards todo capture: the existing tool.execute.after capture path additionally calls `todo_state.set` (fire-and-forget with error log) when mode is rust.

### 3. Failure semantics (plan section, verbatim)

- Daemon unreachable / route error / timeout / module reject on the authority path: RAW PASS-THROUGH of the input array (unmodified), loud sessionLog + counter. NEVER fall back to the TS transform.
- 3 consecutive failures → park rust mode for the session until process restart: raw passthrough + ONE warning toast via the existing notification path (find the wrapup/notification toast helper). Parked sessions probe the daemon every 5th pass and unpark on success (plan F2 retrying-park).
- Do NOT implement the >=90% emergency TS re-entry (plan F2's second half) — per the dogfood scoping ruling, parked+pressure = loud ceiling like a stock session for now. Put one comment at the park site stating this is dogfood-grade and the public shape requires re-entry (no plan/finding references in the comment, just the invariant).

### 4. U5: mode-neutral module state sync service

Factor the shadow sender's seed/sync assembly (memories, mutation log, user-profile, workspace, compartments, marker state, watermark computation) into `packages/plugin/src/hooks/magic-context/module-state-sync.ts`, consumed by BOTH the shadow sender (mirror lane) and the rust-mode path (authority lane). Sync triggers on the authority lane (plan "State authority"/MEMORIES): bootstrap seed + on ctx_memory mutations + dreamer-write detection via the existing `project_memory_epoch` / `maxMemoryId` / mutation-log watermarks — the same triggers the TS renderer uses to decide memory deltas today. The shadow sender's behavior must not change (its tests prove it).

### 5. Mutual exclusion

When mode is rust for a session, the shadow sender must not arm for it (U0's resolver already forces shadow off at config level; add a belt at the sender arm site checking the resolved mode).

## Tests (mock module client; no live daemon)

- Mode gate: ts-mode sessions byte-identical through the untouched pipeline (existing tests prove this — run them); rust-mode session bypasses TS mutations entirely (a message array with tag-eligible content passes through with ZERO TS-added bytes when the mock returns it unchanged).
- Apply-verbatim: mock returns modified array → output IS the mock's array (deep-equal, no re-serialization drift).
- Seed-then-transform on first pass; no seed on second pass.
- Failure: mock throws → raw passthrough (input === output), log; 3x → parked; 5th-pass probe unparks on mock success; park toast fired once.
- channel2 directive → lease claim called; no directive → not called; directive on two passes + lease already claimed → single delivery.
- Mirror-back: mock exposes compartments after seq N → rows land in context.db idempotently (second run no-ops).
- todo forward: capture path calls todo_state.set in rust mode only.
- Shadow sender suite green (unchanged behavior).
- Full plugin suite `bun test` (known pre-existing noise: a late "Database has closed" runner failure and TS5097 script typecheck errors — report, don't chase).

Commit in the worktree; do not push. Report: the exact client interface you defined for mirror-back (the module-op dependency), any contract drift you had to guess, and anything you were tempted to defer (nothing gets deferred silently).
