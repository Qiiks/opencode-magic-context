# Phase-2 second-Oracle remediation (3 P0 + 5 P1)

Repo: subc-migration branch, HEAD da68a4e9. All work in crates/mc-module (+ mc-store if a schema/API is needed). Rust edition per workspace. Gates: `cargo test -p mc-module --lib`, `cargo test -p mc-store`, `cargo clippy -p mc-module --all-targets -- -D warnings`, `cargo fmt --check`. Every fix needs a FAIL-FIRST test (prove the test fails against the pre-fix behavior via a temporary revert or targeted mutant, then passes). Comments explain WHY, never reference this brief/Oracle/finding numbers. No em-dashes anywhere.

Context: the phase-2 CC-parity delta (temporal gap comments G2, lexical auto-search hints G1, session.wrapup/session.status ops G5) was audited by an independent Oracle. Verdict BLOCK. Fix all findings below.

## P0-1: G2 gap basis is wrong on the real call path (JOINT CONTRACT — implement module side)

Problem: gap markers derive from tag mint times, but on Claude Code the previous assistant and the current user are first observed in the SAME request (one now_ms), so the computed gap between the current user and its immediate predecessor is always ~0. The interesting gap (user idle time after the previous response) is unobservable from mint times.

Fix (contract agreed with the proxy seat): the transform request gains an OPTIONAL top-level field `prev_response_completed_at_ms: Option<u64>` (wall-clock ms when the previous provider response for THIS session completed, proxy-observed). Module-side semantics:
- On a pass where the newest USER message is new (not yet decided in mc_temporal_marks), gap = observed_now_ms − prev_response_completed_at_ms, only when the field is present AND prev_response_completed_at_ms < now. Absent field or nonsensical value (future, zero) → freeze the durable NO-MARKER decision (empty marker row) exactly like the existing below-threshold path. Never guess from mint times anymore for the user-after-assistant case.
- Threshold/format/placement unchanged (300s, floor 2-unit render, `<!-- {gap} -->\n` prepend, user messages only, decision frozen at first sight in mc_temporal_marks, replay from rows forever).
- Keep mint-time basis ONLY as the between-users fallback when a session has multiple new user messages in one request (rare); document why.
Tests: field present and > threshold renders and freezes; absent field freezes no-marker; frozen decision survives later passes that carry a different value (first-sight wins); zero/future values freeze no-marker.

## P0-2: command ledger must record ONLY genuine terminals

Problem: `terminal_wrapup_response(disposition="failed", ...)` is called for RECOVERABLE conditions and persists by command_id: historian failure backoff active (lib.rs ~3346), snapshot Missing/InFlight (~3371), revert-epoch mismatch (~3397), generation race (~3440), and the absolute-budget timeout arm. A same-id retry then replays the failure verbatim forever, permanently poisoning the command.

Fix: recoverable conditions return a NONTERMINAL typed response and MUST NOT insert into mc_wrapup_commands. Contract: response `{ok: false, disposition: "retryable", reason: "<machine_code>", summary}` where reason ∈ {"backoff_active", "snapshot_unavailable", "snapshot_stale", "budget_exhausted"}. Projection-parse failure (malformed cached request) is also retryable (a newer transform refreshes the snapshot). ONLY completed and nothing_to_compact are recorded in the ledger. The "failed" disposition remains in the wire enum but is never produced by any current arm; leave the replay path able to serve historic failed rows.
Tests: each recoverable arm returns disposition=retryable with the right reason and the ledger stays empty for that command_id; a subsequent successful retry with the SAME command_id executes (not replayed) and records completed.

## P0-3: publication-time linearization for wrapup rounds

Problem claim: ready generation/revert epoch are validated at round ENTRY, not transactionally at publication. A newer transform can commit (bump revert_epoch / re-cut) while the producer runs; the old wrapup round may still publish against retired state, and terminal command recording can happen against a superseded snapshot.

FIRST verify at source what the wrapup drive threads into `HistorianPublishRequest` per round: the publish path already carries `expected_row_version` + `expected_revert_epoch` CAS fencing. If each wrapup round re-reads the assembly snapshot and threads that round's observed row_version/revert_epoch into the publish, the mid-round re-cut is already fenced at commit; state that in a comment and close the gap that remains:
- Re-validate {ready generation still current, revert_epoch unchanged} immediately before TERMINAL ledger recording; if stale, return the retryable snapshot_stale response from P0-2 (no ledger write).
- If the round publish does NOT thread the per-round observed values (uses entry-time values), fix it to thread per-round values.
Tests: simulate a transform commit between round entry and publish (bump revert_epoch in store mid-drive via the producer test seam) → publish is rejected by CAS/fence, wrapup returns retryable, ledger empty. Terminal recording with a stale generation → retryable, no ledger row.

## P1-4: G1 hint minting must target the ACTUAL array tail; pending branches are frontier-only

Problem: hint minting uses rfind over user messages, so a buried user (followed by assistant/tool messages) can mint. Canonical TS behavior requires the newest eligible message to be the actual meaningful tail. AND the pending_rewrite passthrough branch currently computes NEW hints; the frozen design for pending/alarm passes is frontier-only, no new bytes.

Fix: mint only when the newest eligible ordinal in the array is an authored USER message (trailing synthetic/system noise may follow; define eligibility explicitly in a comment: skip synthetic and system-role blocks when finding the tail). pending_rewrite and any reconcile/alarm branch: replay existing decided rows only, never compute or persist new decisions.
Tests: buried user (user then assistant tail) mints nothing; user-at-tail mints; pending_rewrite pass with an undecided user replays nothing and persists nothing, and the same user mints on the next accepted active pass.

## P1-5: G1 lexical scoring must actually match (tokenized scoring + real threshold)

Problem: current implementation does whole-query substring LIKE over up to 500 chars of user text against memory/compartment text; a whole user message as a substring almost never matches, and the reciprocal-rank threshold 1/(rank+1) > 0.05 is vacuously true for top-3. Feature would emit nothing (or garbage).

Fix: tokenize the user text (lowercase, split on non-alphanumeric, drop tokens < 3 chars and a small stopword set, cap at ~24 distinct tokens), score candidates by matched-token count weighted by token rarity across the candidate pool (idf-lite: log(N/df)), require a minimum ABSOLUTE score (at least 2 distinct matched tokens AND normalized score ≥ a threshold you calibrate with a fixture pool of ~30 memories where obviously-related queries hit and unrelated queries emit nothing). Keep top-3 max, keep the existing render shape. This mirrors the TS unified-search lexical lane in spirit; do not build FTS5 tables for this, in-memory scoring over the already-loaded candidate rows is fine at these sizes.
Tests: related query surfaces the planted memory; unrelated query emits nothing (empty decision frozen); threshold is mutation-sensitive (dropping the min-matched-tokens check fails a test with a one-common-token near-miss fixture).

## P1-6: overlay decisions must not persist on locally rejected passes

Problem: tags/hints/temporal/frontier rows are written during the pass BEFORE later local rejection points (boundary validation, IdentityDrift, coverage errors). A rejected pass then leaves durable frontier/decision advances that suppress future hints or freeze wrong decisions.

Fix: compute overlay decisions speculatively during the pass but persist them in the SAME fenced commit transaction that commits the accepted pass state (the store commit that bumps row_version). If the pass rejects, nothing persists. If a fenced-transaction fold is structurally impossible for some write, prove why and gate that write behind the last rejection point instead.
Tests: a pass that fails boundary validation after hint/temporal computation leaves mc_user_hints/mc_temporal_marks/mc_overlay_frontiers untouched; the same messages mint normally on the next accepted pass.

## P1-7: frontier sentinel treats ordinal 0 as ineligible

Problem: mc_overlay_frontiers uses 0 as the absent sentinel, so a legitimate zero-based first message (ordinal 0) is never eligible. This codebase has hit the 1-based-assumption bug class before.

Fix: represent absence as NULL in the row (Option<u64> / distinct absent state), eligibility = ordinal strictly greater than frontier ONLY when a frontier row exists, everything eligible when absent. Test with a real ordinal-0 user message minting a hint on a fresh session.

## P1-8: session.status consistency + output contract

Problem: status assembles from separate store reads (meta, compartments, historian state), which can mix generations under concurrency; and compact_status_detail does not sanitize/enforce the ≤500-char one-paragraph contract.

Fix: read everything status needs in ONE read transaction (or from one loaded snapshot struct); sanitize the summary (collapse control chars/newlines to spaces, the same class of sanitization the compartment-title renderer uses) and hard-truncate to 500 chars on a char boundary.
Tests: control characters in stored state cannot reach the summary; summary length ≤ 500 under an oversized fixture.

## Out of scope (do NOT touch)

Shadow-lane overlay cleanup: shadow lineages run the opencode profile with tagging inactive, so U1 overlay rows cannot exist for shadow sessions. Leave a short comment where shadow_reset clears session state noting overlay tables are cc_u1-only and shadow-exempt by construction, if and only if there is a natural place; otherwise skip.

## Deliverables

Single commit on a worktree branch off subc-migration HEAD (da68a4e9). Commit message explains each fix as an invariant (no finding numbers). Run the four gates listed at top plus `cargo test -p mc-store`. Report: per-finding fix summary, test names, gate outputs.
