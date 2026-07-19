# Phase-2 remediation round 3 (post diff-Oracle BLOCK)

Repo: subc-migration branch, HEAD 6630e25e. Work in crates/mc-module + crates/mc-store. Gates: `cargo test -p mc-module --lib`, `cargo test -p mc-store`, `cargo clippy -p mc-module --all-targets -- -D warnings`, `cargo fmt --check`. Every fix carries a FAIL-FIRST test (prove failure against pre-fix behavior via targeted mutant or temporary revert, then pass). Comments explain WHY; never reference this brief, Oracle findings, or plan versions. No em-dashes.

An adversarial review of the previous remediation found the write side sound (commit_transform CAS fold is empty-txn on conflict) but flagged the following. Fix all.

## P1-B: single-snapshot overlay reads

Problem: the transform read path loads cache state, tags, temporal marks, hints, and channel-1 appends through SEPARATE store calls (crates/mc-module/src/transform.rs:747-815). A concurrent commit_transform can land between reads, so a pure defer or pending_rewrite passthrough (which never commits, hence never CAS-revalidates) can render state at row version N with a subset of overlays from N+1: byte divergence across processes.

Fix: add `McStore::load_transform_snapshot(session_id)` returning cache state (LoadedState) PLUS all overlay tables (tags, temporal marks, hints, channel-1 appends, frontier) read inside ONE SQLite read transaction. Replace the separate loads in the transform entry with it. Writer passes keep their existing CAS as the final authority; the single read snapshot is the linearization point for no-write passes.
Test: deterministic interleave via a store test that commits overlays between two of the old-style separate reads vs the new snapshot read; assert the snapshot's row_version and overlay set are mutually consistent (the old path's inconsistency must be demonstrable in the fail-first phase, e.g. by temporarily reintroducing split reads behind a test hook).

## P1-C: honest backoff_active + ledger hygiene

Problem 1: record_historian_connect_failure discards the result of store.commit (crates/mc-module/src/lib.rs:2083-2127) while run_wrapup_firing maps producer connect errors to reason backoff_active (:3610-3631). Under a CAS conflict or store failure no durable backoff was armed, so the caller is told backoff_active but an immediate retry executes: the reason lies.
Fix: propagate the persistence outcome; return backoff_active ONLY after a successful durable transition, otherwise snapshot_unavailable. Retry the CAS-conflict arm once with a fresh load before downgrading (a conflict usually means another writer just committed; re-attempt the backoff write on the new version).

Problem 2: record_wrapup_command accepts arbitrary disposition strings and the schema permits "failed"; legacy failed rows replay poisoned results forever.
Fix: reject nonterminal dispositions at the record API (accept only completed | nothing_to_compact; debug_assert plus runtime error). On the REPLAY path, treat a stored "failed" row as absent (execute normally); do not delete rows (audit trail), just skip them for replay.
Tests: connect-failure with a store CAS conflict returns snapshot_unavailable not backoff_active (fail-first against current mapping); record API rejects "failed"; a pre-seeded legacy failed row does not replay and a retry executes.

## P1-D: CAS-fenced terminal ledger recording

Problem: wrapup_snapshot_is_current checks the process-local generation, releases the lock, then reads the store epoch separately (lib.rs:3230-3244); terminal_wrapup_response then records the ledger row in yet another transaction with no state predicate (:3293-3320, store :2817-2880). A transform starting after the generation check or committing between the epoch read and the insert records a terminal command against retired state.

Fix: add `McStore::record_wrapup_command_if_current { session_id, command_id, disposition, rounds, summary, expected_row_version, expected_revert_epoch }` that inside ONE transaction reads mc_cache_state row_version + meta revert_epoch, compares, and inserts only on match (returns a typed Stale outcome otherwise). In the handler, hold the transform-snapshots mutex across the generation validation AND the store call (the store call is a fast local SQLite write; document why holding the lock is acceptable), returning retryable snapshot_stale on any mismatch. The no-command-id path needs only the response (no recording), so it can skip the fence.
Tests: transform commit between validation and recording => retryable + no ledger row (drive via the producer await-output hook committing a revert_epoch bump, plus a variant that calls begin() on the snapshot cache); successful path unchanged.

## P1-F: authored-user tail requires genuine text

Problem: eligible_authored_user_tail (transform.rs:2795-2811) checks only role == user after skipping synthetic/system. Two defects: (a) a trailing role=tool result suppresses the preceding authored user (wrong: results are transport, not authored turns); (b) a CC-wire tool result carried as role=USER is itself selected, the hint path finds no text target, and the frontier still advances: that user's hint is frozen-empty forever, and the temporal path can attach a gap marker to a tool-result block.

Fix: eligibility = role user AND at least one genuine Text block that is not a tool-result payload (use the projection's block kinds: a user message whose blocks are all ToolResult/Opaque-result carriers is not authored). Skip trailing tool-role AND tool-result-only user messages when walking back for the tail, exactly as synthetic/system are skipped. Apply the SAME predicate to frontier advancement and temporal-mark targeting so a tool-result-only user neither mints nor consumes eligibility.
Tests: [user, toolresult(role=tool)] tail => the user is eligible (hint mints); [user, toolresult-as-user] tail => the authored user is eligible, the result block gets no temporal marker and no frontier burn; the previously-wrong behavior demonstrated fail-first.

## P2 batch (fix, lighter tests)

1. Non-CAS overlay mutators (mint_or_get_tags, append_channel1_once, apply_active_overlay_decisions, append_user_hint_once in mc-store): restrict to crate-private (pub(crate)) or fold into commit_transform-only usage; if tests need seeding, gate a seeding helper under #[cfg(test)] or a clearly named test-support method. Add the CAS-conflict test asserting every overlay table stays empty on conflict.
2. Lexical scorer: require at least one discriminative token (df below half the pool) among the matches; drop a truncated final token at the 500-char cap; leave Unicode normalization as a documented non-goal (comment).
3. Status latch coherence: sample the wrapup latch before AND after the DB snapshot read; if it changed, re-read once. Cheaper than holding the mutex across I/O; document the choice.
4. Test adequacy repairs flagged by the review: make the temporal replay test use time values whose old-basis and new-basis renders DIFFER (e.g. mint delta formats +12m while wire delta formats +2h) so the source-of-time fix is mutation-sensitive; extend the rejection test to stage channel-1 appends too; port overlay_decisions_share_an_atomic_ordinal_watermark to drive commit_transform instead of the obsolete eager API.

## Deliverables

Single commit on the task branch. Run all four gates plus mc-store. Report per-finding: fix summary, test names, fail-first evidence, gate outputs.
