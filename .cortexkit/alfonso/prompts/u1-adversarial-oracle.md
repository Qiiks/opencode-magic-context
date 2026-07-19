# U1 tagging+reduce flip — adversarial cache-safety audit (read-only)

You are the adversarial gate for the U1 cutover: enabling §N§ tagging + agent `ctx_reduce` for the Claude Code leg (serializer profile `claude-code-anthropic`) of the MC Rust module. U0 (full-array apply) is live in prod; U1 is DESIGN-FROZEN and NOT implemented. Your verdict gates implementation. Be adversarial: your job is to find the cache-bust, data-loss, or irreversibility hole, not to bless the plan.

## Verdict contract
Return SHIP / REVISE / BLOCK with numbered findings. Every finding must cite file:line from THIS repo (read the code; do not trust this brief's claims — verifying the brief against source is part of the audit). Findings that would change bytes on an existing session without a coordinated HARD transition are automatically BLOCK-class.

## Canonical design note (verbatim copy of Thalamus's frozen `docs/u1-tagging-reduce-design.md`)
Read it at: /Users/ufukaltinok/Work/Projects/CortexKit/ai-proxy/docs/u1-tagging-reduce-design.md (252 lines — read it in full).

## Corrected baseline (verify each at source)
- Prod ck-mc built at subc-migration `93cc48c0`; this working tree HEAD is a descendant with no U1-relevant drift.
- `healing::tail_reclaim(SerializerProfile::ClaudeCodeAnthropic) == false` (healing.rs:136-143). U0 = fold-only reclaim, verbatim tail. The healing.rs doc comment is splice-era stale (U0 full-array apply shipped); it is rewritten AT U1 implementation, not before.
- U1 tagging surface exists but is flip-gated: `tagging_enabled()` returns false (healing.rs, cfg(not(test))). Tags today are never emitted.
- Durable reduce command ledger (`mc_reduce_command_ledger`, migration 16, commit ac80ff94) provides command_id idempotency for `agent_drops.append`.
- Epoch state (lib.rs constants): profile_epoch=1, tagger_epoch=0 (flips to 1 at U1), memory_render_epoch=2, compartment_render_epoch=2. Epoch bumps fold into m0 content epoch module-side (omitted-at-zero), self-coordinating one HARD per session.

## The four source-confirmed defects U1 must fix (verify locations yourself)
A. DOUBLE EFFECTOR: `handle_ctx_reduce_facade` (lib.rs) directly calls `append_pending_agent_drops` (no command_id) while Thalamus's response tee ALSO sends `agent_drops.append` with tool_use_id command_id and exact composite conversation key. One model call = two mutation paths, and the direct facade resolves parent scope via shared CK_INSTANCE_TOKEN (wrong lineage for subagents). Fix: direct facade becomes ALWAYS-INERT for CC (ack-only; SUBC ruled generic policy-driven `ack_only` before route dispatch at the shim); the tee is the sole effector. `ExecutionMode::Pure` on the manifest stays (truthful once inert).
B. NEWEST-20 UNENFORCED: guidance promises last-20-tags protection; `select_reductions` is called with `protected_cutoff_ordinal: 0` (transform.rs ~930-959) and `select_agent_drops` emits any pending live non-frozen target (selection.rs ~546-565). Semantics to enforce: top 20 ACTIVE tags by tag_number descending across ALL model-visible tag kinds, recomputed each apply pass; distinct from RECENT_TOOL_SKELETON_WINDOW.
C. PENDING CONSUMPTION: on any changed bust+producer pass, transform.rs ~1413-1442 deletes EVERY loaded pending row, not only applied targets. Fix: exact classification — consume only applied or already-satisfied/obsolete IDs; protected/dormant IDs stay durably pending until eligible.
D. DORMANCY GATE (distinct from C): while a session's `tool_present=false`, pending rows must be ineligible for SELECTION (a selection gate, not just consumption bookkeeping), so a false-window pass can never apply an old drop.

Eligibility (D) vs consumption (C) are separate mechanisms and each needs its own non-vacuous test — a test that would fail if THAT mechanism were removed while the other stayed.

## Coherence mechanism (wire-evidence tool_present)
- Thalamus derives `tool_present` per-request from the raw request's tools[] (exact canonical ctx_reduce name) and passes it to guidance.get + transform. Restart-proof by recomputation; exact per parent/subagent request; cannot disagree with actual invocability. No durable declaration op, no generation counter.
- false→true: tf (tagger epoch component), g (guidance variant), tfe (tail-reclaim effectiveness) change TOGETHER; first pass is a coordinated HARD, untagged.
- true→false: immediate no_reduce guidance + zero tag bytes + loud log; reversible (no never-re-tag latch). Content-safety argument for latch-free reversibility: tags are an OUTPUT-ONLY overlay applied in build_output — they must never enter ingress projection, mid identity, frozen units, or compartment content. If you find ANY path where tag bytes enter durable state or identity, that is a BLOCK (and the pre-agreed fallback is a re-activation budget latch).
- Per-session tool_present=false must keep ALL render identity inputs at preflip values (Thalamus tf0/no_reduce, MC tfe omitted/off) so an existing session takes NO gratuitous hard fold.
- U1 derives EFFECTIVE tail reclaim per-request from tool_present — the global `tail_reclaim` const must NOT simply flip to true for claude-code-anthropic.

## SUBC's four reversibility assertions (make each a first-class audit check)
1. On every tool_present=false pass: tag bytes absent from build_output AND provably never entered ingress identity, frozen units, or compartments.
2. true→false changes tf/g/tfe together through a HARD-class transition.
3. Every still-live pending agent drop remains unapplied across the entire false window regardless of age (retired targets may become obsolete and be consumed as such).
4. Reductions committed in a prior true phase persist (frozen units are immutable) but contain no tag residue.

## Audit scope (read these in full)
- crates/mc-module/src/healing.rs (profiles, tail_reclaim, tagging_enabled)
- crates/mc-module/src/transform.rs (apply_once, selection call sites, pending-row load/consume, build_output overlay points)
- crates/mc-module/src/selection.rs (select_reductions, select_agent_drops)
- crates/mc-module/src/lib.rs (handle_ctx_reduce_facade, agent_drops.append, ledger, guidance.get, status/epochs)
- crates/mc-store/src/lib.rs (pending_agent_drops, mc_reduce_command_ledger, commit_with_consumed_drops)
- The Thalamus design note (path above)

## Questions the verdict must answer explicitly
1. Is the frozen design sufficient to fix A-D without introducing a new bust class? Any hole in the tf/g/tfe joint-transition rule (e.g. a component that can flip independently)?
2. Is latch-free reversibility safe given the output-only tag overlay claim — verified at source, not asserted?
3. Do the four reversibility assertions, as specified, actually pin the invariants (or is one vacuously satisfiable)?
4. Newest-20: is "top 20 active by tag_number desc across all kinds, recomputed per pass" well-defined at the selection call sites (ordinal vs tag_number mismatch risk)?
5. Consumption classification: can a concurrent pass (busy historian, emergency arm) still over-consume or double-apply under the proposed split?
6. Anything in the U1 surface already merged (flip-gated) that would misbehave the moment tagging_enabled() flips?

Output: write your full findings to the chat (no file writes — you are read-only).
