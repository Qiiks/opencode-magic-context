# U1 dormant repair implementation (MC-1..7) — Rust module, subc-migration

Implement the locked U1 repair scope in the MC Rust module (crates/). This is DORMANT work: nothing may change behavior for any pass where the new request-local activation input is absent or false. Production is untouched (no deploy; you only commit in your worktree).

## Authoritative specs — read these FIRST, in this order
1. `.alfonso/oracle-verdicts/u1-mc-seat-oracle-2026-07-15.md` — the joint Oracle verdict + joint addendum + LKG amendment. The MC-1..7 list at the end of the joint addendum is your scope, verbatim.
2. `/Users/ufukaltinok/Work/Projects/CortexKit/ai-proxy/docs/u1-tagging-reduce-design.md` — Thalamus's frozen design note (their consumer expectations).
3. The current source: crates/mc-module/src/{transform.rs, selection.rs, lib.rs, healing.rs}, crates/mc-store/src/lib.rs.

## The seven work items (details in the verdict file — do not reinterpret, verify each citation at source)
MC-1 (the BLOCK, do first): frozen drop payloads stay canonical "[dropped]" — delete `numbered_drop_placeholders_for_new_freezes` (transform.rs ~1754-1776) as a freeze-time mutator. The "§N§" numbering becomes a VOLATILE OUTPUT OVERLAY applied in build_output only when the pass is active (see MC-2): when rendering a frozen `red:` unit whose target has a live tag row, the overlay may display "[dropped §N§]" in the egress clone, but the durable frozen payload and all identity/digest inputs must remain "[dropped]". Existing tests that assert frozen "[dropped §N§]" payloads change to assert canonical frozen bytes + overlay-only numbering. NOTE: pre-existing frozen units that already carry "[dropped §N§]" bytes in a store must replay byte-identical (they are Lineage-durable); only NEW freezes go canonical. Add a regression test for that mixed state.

MC-2: one normalized `cc_u1_active = (serializer_profile == ClaudeCodeAnthropic && tool_present)` where `tool_present` is a NEW OPTIONAL request field (`#[serde(default)]` → absent = false). This single boolean gates ALL of: tag row minting, the §N§ output overlay, tfe (effective tail reclaim for the CC profile — the global `tail_reclaim()` const stays false for ClaudeCodeAnthropic; effective reclaim for CC is per-request via this boolean), pending-agent-drop selection eligibility, synthetic-todo injection for CC, channel-1-style output appends for CC, and the guidance variant served (`guidance.get` gains the same field; no_reduce when false). No independently selectable guidance variant may contradict the boolean. For non-CC profiles nothing changes (their tail_reclaim stays const-true and tagging stays off until their own cutover).

MC-3: newest-20 protection as a dynamic BLOCK-ID SET: active tag rows = (block currently in the live tail) ∧ (taggable/model-visible kind) ∧ (no frozen reduction on the block); order by tag_number DESC, take 20, exclude those block ids from BOTH `select_agent_drops` results and automatic selection for the pass. Do NOT use `protected_cutoff_ordinal` for this. Tag rows must be LOADED whenever the profile is ClaudeCodeAnthropic and the request carries tool_present (even on the transition pass), so a reactivation HARD can classify pending drops against prior tags.

MC-4: exact pending-row consumption classification: on a producing pass, classify each loaded pending_agent_drops row as APPLIED (a reduction for its target froze this pass), OBSOLETE (target no longer live / already frozen / retired), or RETAINED (protected by newest-20, dormant because cc_u1_active=false, or target not yet eligible). Delete ONLY applied+obsolete IDs via `commit_with_consumed_drops`. CRITICAL: a pass whose only effect is consumption (core/meta byte-identical) MUST still run the fenced commit — compute consumed IDs after planning and commit when state changed OR consumed non-empty.

MC-5: direct facade `handle_ctx_reduce_facade` becomes ALWAYS-INERT for the CC leg: return the fixed acknowledgement ("queued for the next compaction pass" shape — match the existing ack text) BEFORE token resolution, scope resolution, argument parsing beyond basic shape, or any store access. `ExecutionMode::Pure` stays. The manifest schema for ctx_reduce becomes {type:"object", properties:{drop:{type:"string"}}, required:["drop"], additionalProperties:false}.

MC-6: management op cleanup: `agent_drops.append` remains the ONLY mutation route — drop the `ctx_reduce` and `append_agent_drops` dispatch aliases. REQUIRE nonempty `command_id` and nonempty raw `drop` string (reject otherwise). Ledger retention: remove the newest-512 prune; command ids are retained for the lineage lifetime (bounded by real reduce volume).

MC-7: transform ok response gains two ADDITIVE fields, always present: `surface_state`: "inactive" (cc_u1_active false this pass) | "transition" (the coordinated HARD pass where the surface flips either direction) | "active" (steady-state true), and `row_version` (the committed/loaded store row version for the pass). Non-CC profiles report "inactive".

Transition semantics (MC-2/MC-7 interlock): false→true and true→false each ride ONE coordinated HARD (the tf/g/tfe components change together through the existing m0-content-epoch fold machinery — use the profile_render_epoch/M0ContentEpoch pattern, per-request-conditional). A session at tool_present=false keeps ALL render identity inputs byte-identical to today's U0 output (zero gratuitous folds for existing sessions — this is the dormancy invariant). TAGGER_FEATURE_EPOCH flips 0→1 as part of this work (it participates in the epoch fold only when cc_u1_active).

## Required tests (each must be non-vacuous — would fail if its mechanism were removed)
- SUBC reversibility assertions 1-4 as first-class tests (see verdict): (1) false pass ⇒ zero tag bytes in build_output AND no tag bytes in ingress identity/frozen units/compartments; (2) true→false flips tf/g/tfe together in ONE hard; (3) still-live pending drops remain durable-unapplied across an entire false window regardless of age; (4) a reduction committed during true persists with canonical frozen bytes and NO overlay residue on false passes — the fixture MUST keep the reduced target in the live tail (HARD's unit GC can otherwise fake a pass).
- C/D isolation: (C) under true, one protected pending target + one applied target ⇒ only the applied ID is consumed; (D) under false with a forced execute/HARD, an old eligible pending target produces NO red unit and survives; then applies after true.
- Consumption-only commit: a pass with byte-identical core/meta but an obsolete pending row consumes it (row_version bumps).
- Mixed legacy state: an existing store with a frozen "[dropped §7§]" unit replays byte-identical while a new freeze in the same pass goes canonical.
- U0 byte-identity: a full pass matrix (bootstrap HARD, defer, SOFT, coverage-fold) with tool_present absent produces byte-identical output to the pre-change code for the CC profile AND for owned-broca/pi/opencode profiles. If you need a golden, capture it from HEAD~ before your changes.
- surface_state/row_version presence on every ok response; transition reported exactly once per flip.
- Facade inertness: ctx_reduce facade call with an unresolvable token still returns the fixed ack, zero store access (assert via store call-count or similar seam).
- agent_drops.append: alias routes rejected, missing command_id rejected, ledger rows survive past 512 appends.

## Rules
- Gates: cargo test -p mc-module -p mc-store -p mc-core, clippy clean, fmt clean. Run the full workspace suite including tests/real_daemon.rs.
- The healing.rs splice-era doc comment on tail_reclaim gets rewritten to describe the U0/U1 reality (full-array apply; per-request effective reclaim).
- Comments explain invariants for a reader with no context; never reference Oracle findings, U-numbers, or seat names.
- Commit in your worktree with clear per-item messages. Do not touch packages/ (TS) at all.
