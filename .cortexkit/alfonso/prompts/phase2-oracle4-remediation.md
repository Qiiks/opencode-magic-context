# Phase-2 remediation round 4 (final cross-seat Oracle BLOCK, 3 findings)

Repo: subc-migration branch, HEAD bc2c73ff. Work in crates/mc-module + crates/mc-store. Gates: `cargo test -p mc-module --lib`, `cargo test -p mc-store`, `cargo clippy -p mc-module --all-targets -- -D warnings`, `cargo fmt --check`. Every fix carries a FAIL-FIRST test. Comments explain WHY; never reference this brief or Oracle findings. No em-dashes.

Context: the CC-parity phase-2 delta passed my diff-Oracle after round 3, but the peer seat's independent Oracle found 3 remaining source-confirmed defects. All in the wrapup/snapshot machinery.

## S1-A: stale snapshot can PUBLISH additive rows before the generation retryable fires

Confirmed hazard: wrapup validates the transform-snapshot generation at round entry (lib.rs ~3672-3685) and again before TERMINAL LEDGER recording, but the historian publication itself (lib.rs ~1323-1338 driving the store publish at mc-store ~4692-4752) is fenced only by {row_version, revert_epoch, historian phase}. A transform that has BEGUN (generation bumped via begin()) but not yet committed leaves row_version unchanged, so a wrapup round publishes compartments, chunk transcripts, and facts derived from the retired snapshot; the later generation check makes the RESPONSE retryable but the additive rows already landed against a lineage that may be mid-re-cut.

Fix: enforce a publication-time generation fence. Validate the process-local snapshot generation immediately before the store publication and hold the transform-snapshots lock across that validation AND the publication call (same bounded-local-write rationale as the terminal-ledger fence; document it). On mismatch, abort BEFORE any additive write and surface the retryable snapshot_stale path. Wire the fence through the wrapup drive only; the organic historian path (pressure-fired) has no snapshot dependency and must NOT acquire this fence (state why in a comment: organic publishes derive from raw store reads, not the cached wrapup snapshot).
Test: use the producer await-output hook to call transform_snapshots.begin(session) mid-round (generation bump, no commit), then assert: response retryable, AND mc_compartments / mc_chunk_transcripts / fact tables / publication floor ALL unchanged (count + max seq before/after). Fail-first: without the fence the compartments land.

## S1-B: legacy failed ledger row survives a successful terminal record => response-loss reruns forever

Confirmed: record_wrapup_command_if_current (mc-store ~3147-3164) returns LegacyFailurePreserved(row) when a legacy "failed" row occupies the (session_id, command_id) key: the fresh terminal result is RESPONDED but never recorded. If that response is lost, every same-id retry skips the failed row on replay and re-executes forever.

Fix: on a successful terminal outcome with a legacy failed row present, atomically REPLACE the row with the new terminal disposition inside the same fenced transaction (UPDATE disposition/rounds/summary/created_at). Delete the LegacyFailurePreserved variant if it becomes unreachable. Diagnostics: keep the replaced failure detail by appending a short suffix to the stored summary (e.g. "; replaced failed record from <created_at>") within the 500-char cap, not a separate table.
Test (response-loss replay): seed a legacy failed row, run wrapup with the same command_id to terminal, drop the response, retry same id => replay returns the recorded terminal verbatim with replayed=true and does NOT drive (producer start count unchanged). Fail-first against current behavior (retry re-executes).

## S2: snapshot cache byte budget ignores active Arc leases

Confirmed: TransformSnapshotCache::get clones the Arc<TransformRequest> out; eviction charges only map-resident entries. Concurrent wrapups across sessions can each retain a multi-MiB request for the whole blocking budget (up to 3800s) with no global bound.

Fix: add a global active-lease budget to the cache: a shared counter (bytes + count) incremented when a Ready snapshot is leased for wrapup and decremented on RAII guard drop (new SnapshotLease type holding the Arc + the shared counter handle). Wrapup acquires the lease at entry; if the budget (suggest bytes: same 64 MiB constant family, count: 8) would be exceeded, return the retryable snapshot_unavailable response ("too many concurrent wrapups"). Eviction from the map stays as-is; leased bytes are bounded separately.
Test: hold N leases via blocked wrapups (producer hook parks), assert lease N+1 (or bytes overflow) returns retryable and that guard drop releases budget (a subsequent wrapup succeeds). Cache churn while leases are held stays within both bounds. Fail-first: without the budget the (N+1)th lease is admitted.

## Deliverables

Single commit on the task branch off bc2c73ff. Run all four gates plus mc-store. Report per-finding: fix summary, test names, fail-first evidence, gate outputs.
