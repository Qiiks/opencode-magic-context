# Rust MC mode — U2: module-side host_directives + small ops (Rust, crates/)

Part of the per-project Rust MC cutover (plan: `.alfonso/plans/rust-mc-mode-v1.md` v2 — read "Architecture" items 3-5 and unit U2). This unit is MODULE-SIDE ONLY (crates/mc-module, crates/mc-store). No TS changes. The consumers land in a parallel unit; your contract is the wire shapes below — pin them with tests so the TS side can build against them.

Golden rule for everything here: the transform output bytes for existing consumers (CC leg via thalamus, owned leg via broca, shadow lane) must be BYTE-IDENTICAL to today unless the new fields are explicitly requested. All new response fields are additive and omitted when empty (`#[serde(skip_serializing_if = ...)]`), so existing consumers see identical JSON.

## 1. `host_directives` on the transform response

New OPTIONAL field on the transform ok-response: `host_directives`, omitted entirely when there is nothing to direct (which is every pass for non-opencode-plugin profiles — gate emission on the serializer profile for the OpenCode plugin leg, NOT on cc/owned profiles).

v1 shape:
```json
"host_directives": {
  "channel2_nudge": { "text": "<nudge body>" }
}
```
Semantics (plan F5, pinned): the module emits `channel2_nudge` DETERMINISTICALLY on every pass while its view of the session says the ceiling nudge is due and undelivered — idempotent emission, the module NEVER records delivery. The TS host owns delivery + dedup via its existing channel2 lease in context.db. Module-side "due" logic: reuse the existing channel-2 trigger math that already exists for the CC leg's nudge surface (severity over working window, reclaimable >= usable/3, ctx_reduce present); if the CC leg composes channel-2 as a synthetic message today, factor the trigger+text out and share it — do NOT fork the math. The OpenCode-plugin profile emits the directive instead of splicing a synthetic user message (the host delivers via promptAsync).

## 2. `todo_state.set` op

Tiny management op (same dispatch surface as agent_drops.append):
Request: `{ "method": "todo_state.set", "v": 1, "session_id": "...", "state_json": "<raw todo state JSON string>", "owner_message_id": "<mid>" }`
Response: `{ "ok": true }`.
Semantics: last-write-wins guarded by owning message id (plan F5): if the stored row for the session has the SAME owner_message_id and identical state hash, no-op; otherwise upsert. This feeds the existing synthetic-todo anchor machinery exactly like the CC leg's wire-read todo capture — find where the CC leg extracts todowrite state from the wire and store into the same table/state the anchor machinery reads, so the OpenCode leg (where the host captures via tool.execute.after and forwards) and the CC leg (wire-read) converge on one storage path. Idempotent replay-safe (host may re-send after crash).

## 3. `session.flush` op

Request: `{ "method": "session.flush", "v": 1, "session_id": "..." }` → Response `{ "ok": true, "armed": true }`.
Semantics: force the next transform pass for that session to take a SOFT refresh (m1 re-render + drain pending work) — the /ctx-flush equivalent. Implement as a durable one-shot flag on the session state (cleared when consumed). It must NOT force a HARD fold. If an equivalent arm-flag already exists module-side, reuse it; do not duplicate.

## 4. `session.recomp` op

Request: `{ "method": "session.recomp", "v": 1, "session_id": "...", "command_id": "<nonempty ≤128B>" }`
Response: `{ "ok": true, "disposition": "started" | "already_in_progress" | "nothing_to_do" }`.
Semantics: rebuild the session's compartment structure from raw history via the EXISTING re-cut + import machinery (plan U2: "recomp reuses re-cut + import"). Reuse the wrapup op's shape conventions: command_id idempotency through the existing command ledger, process-local per-session latch, machine-readable dispositions. Recomp emits NO facts/promotions (parity with TS recomp — verify what the re-cut path does and gate promotion off for this path if needed). If full recomp via re-cut is NOT actually reachable with existing machinery, do not fake it: implement honestly or report exactly what is missing — no placeholder implementations.

## 5. Guidance variant check (plan open item, U2 acceptance)

Verify `guidance.get` serves the language-directive block for the OpenCode-plugin variant the same way the CC leg gets it. If the localized block only rides one variant, fix so both carry it. Add a test pinning this.

## Tests

- Response-shape test: transform ok-response WITHOUT host_directives is byte-identical to before this change for cc + owned + shadow profiles (serialize and compare against a pre-change golden — regenerate honestly, do not hand-edit).
- channel2 directive: due+undelivered → emitted every pass (two consecutive passes both carry it); not due → absent; profile-gated (cc profile never gets it).
- todo_state.set: upsert, same-owner+same-hash no-op, replay idempotence.
- session.flush: armed flag consumed exactly once, no HARD.
- session.recomp: dispositions, idempotent command replay, latch.
- `cargo test -p mc-module -p mc-store`, clippy clean.

Commit in the worktree; do not push. Report the exact wire shapes as implemented (they become the U1/U4 contract).
