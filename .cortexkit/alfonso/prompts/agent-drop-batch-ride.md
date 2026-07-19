# Agent-drop batch+ride semantics: kill the partial-range bust cascade

Repo: ~/Work/Projects/CortexKit/magic-context, branch subc-migration, crates/mc-module + crates/mc-store. Gates: `cargo test -p mc-module --lib`, `cargo test -p mc-store --lib`, `cargo clippy -p mc-module`. Comments explain WHY (the invariant), never reference incidents/plans; no em-dashes.

PROD INCIDENT CONTEXT (design input, not comment material): a ctx_reduce range drop ("16-46", 31 tags) was partially inside the newest-20 protection window at first application. Each subsequent turn minted new tags, sliding 2-3 more range members out of protection; each producer pass then applied just those newly-eligible members, mutating bytes at 7-9% array depth and re-creating the entire ~20k-token suffix EVERY TURN for 10+ turns. The cache-economics invariant this violates (ported from the TS plugin doctrine): deferred work rides the next bust cycle; it never forces its own.

THE RULED SEMANTICS (final, do not re-litigate):
1. FIRST APPLICATION: a command's currently-eligible members apply as ONE batch on the first execute-class pass. This is the command's single self-caused bust. (Today's behavior for the eligible subset — keep.)
2. HELD REMAINDER: members still protected by the newest-20 block-id set at first application are HELD. They apply only as one batch on a later pass that is ALREADY byte-changing for a non-drop reason: HARD fold (any trigger: coverage, TTL, config/profile transition, epoch), emergency arm, reconcile-rematerialize, or another command's first application in the same pass. No fully-eligible exception, no pressure exception: a held member NEVER applies on an otherwise-stable pass.
3. TRICKLE PROHIBITION: newly-eligible members never apply one-at-a-time as the protection window slides.

IMPLEMENTATION POINTS (verify each at source before coding):
- Pending rows: crates/mc-store pending_agent_drops rows are appended via append_pending_agent_drops_with_command (command ledger already exists). Add a durable per-row or per-command first_applied marker (prefer a nullable first_applied_at_ms on the pending row batch or a command-scoped column — pick the smallest schema change; mc-store migration + tests per repo convention if a column is needed).
- Selection: select_agent_drops (crates/mc-module/src/selection.rs) currently filters on frozen/live only; protection filtering happens later via ctx.protected_block_ids retain. Restructure so the selection layer knows, per agent-drop id, whether its command has already first-applied AND whether this pass is already-busting. SelectionContext gains a `pass_already_busting: bool` (computed in transform.rs BEFORE selection from the pre-selection HARD signals: effective_render_config change, TTL expiry vs cache_ttl, reconcile_pending, emergency arm engaged, coverage-advance/fold prepared this pass — mirror the mustMaterialize-style triggers that are knowable pre-selection; document which HARD triggers are NOT knowable pre-selection and why missing them is safe: a missed ride opportunity only delays the batch to the next one, never causes an extra bust).
- Rule: for a drop id whose command has first_applied set: include it in selection ONLY if pass_already_busting AND it is not protected. For a command not yet first-applied: include its unprotected members (today's rule); after the pass commits (freeze succeeded), mark the command first_applied in the same commit transaction (commit_transform already carries consumed_drop_ids; extend the commit payload minimally).
- Consumption semantics UNCHANGED: held rows stay durable pending (not consumed); consumed_pending_drop_ids continues to consume only applied/retired/covered/reasoning rows.
- Interaction with the reasoning-ineligibility hotfix (shipped earlier today): reasoning-targeted rows retire as structurally unappliable regardless of hold state — keep that path intact.

MUTATION-SENSITIVE TESTS (the incident shape, minimum set):
1. Queue a 30-member range with ~half inside the newest-20 protection set. First execute-class pass: exactly the unprotected members freeze in ONE pass; wire bytes for the protected members' blocks unchanged.
2. Across subsequent passes that mint new tags (sliding 2-3 members out of protection per pass) with NO other bust cause: assert ZERO new freezes and byte-identical replay of the affected region on every pass (the trickle prohibition — this test must FAIL on today's code).
3. A later pass with a natural HARD (e.g. config change or TTL): the ENTIRE now-eligible remainder freezes as one batch on that pass; still-protected members remain held.
4. Another command's first application counts as a ride opportunity for command A's held remainder (both batch on the same pass).
5. Restart persistence: first_applied markers survive module restart (held rows do not re-trickle after reattach).
6. Ledger/idempotency: command replay (same command_id) after partial application acks without disturbing hold state.

Report per point: what changed, where, test names, and the pre-selection bust-signal list you settled on with the not-knowable exclusions documented.
