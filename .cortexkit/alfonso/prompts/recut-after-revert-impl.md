# Implement: historian re-cut after revert (design doc embedded below)

Branch: subc-migration (you fork from HEAD). Implement the v6 design EXACTLY as written in the embedded doc — it survived six adversarial Oracle rounds and every clause is load-bearing; where the doc pins a rule (R2-Fn/R3-Fn/R4-Fn/R5-Fn markers), do not simplify it away. Crates: mc-store (truncate_compartments_for_revert + TruncateOutcome + revert_epoch/last_recut ModuleMeta fields + expected_revert_epoch durable firing field + publish predicate), mc-module (transform.rs reconcile-HARD re-cut arm, coverage-shrink gate for todo re-capture/re-anchor, historian_chunk.rs assembly epoch snapshot, historian.rs fire/reattach epoch carry + epoch-reject abandon arm).

Implementation notes beyond the doc:
- The store epoch predicate should reject with the existing publish-conflict error class so the historian task's existing CAS-conflict handling routes it (the doc's D5 pin).
- Follow the existing code style for fenced tx methods and serde(default) fields (see publish_historian_chunk and the existing HistorianDurableState fields as templates).
- The doc's Tests section is the required test list: the full gate arc (revert -> pass 1 truncates+re-mints+publishes clean), nothing-survives bootstrap, ordinary-HARD stays loud, crash re-entry via coverage-shrink gate, post-truncate row_version handoff, epoch fence interleavings (assembly-to-fire revert, fire-to-publish revert, reattach carry), epoch-mismatch publish -> Idle+backoff+detail, serde epoch-0 compat, CAS conflict retry.
- Gates: cargo test -p mc-module -p mc-core -p mc-store --features mc-store/test-support; cargo test -p mc-module --test real_daemon; cargo clippy --workspace --all-targets -- -D warnings; cargo fmt --check; check_comments. Commit messages explain WHY (revert recovery, the epoch fence, the blob handoff) without referencing rounds/notes/this brief.

----- DESIGN DOC (v6, verbatim) -----

# MC module: re-cut after revert (v4 design)

v2 folded round 1; v3 folded round 2 (epoch snapshot point / storage / reattach carry /
atomic truncate+bump / capture-on-shrink / interior-hole honesty). Round 3 (BLOCK, 2+2)
found the row-version handoff unsound and a crash window bypassing the shrink-specific todo
handling, plus two precision issues (serde defaults unstated, wrong compartment field
name). v4 corrections marked R3-Fn. Round 4 (BLOCK, 1+2) caught the epoch blob handoff
(the pass commit writes the WHOLE meta blob and would un-bump the epoch unless patched in),
an overstated fencing claim, and a stale source-facts bullet — folded below as R4-Fn.
Round 5 (BLOCK, 1+2): the epoch-reject publish path's durable state was unpinned (wedge
hazard), last_recut lacked a pinned writer/storage, and stale wordings contradicted the
corrected CAS design — folded as R5-Fn.

The last known hole in the module's historian story (roadmap #423). Since the mint-absent
guard landed, a session reverted BELOW coverage whose store is never re-cut fails loud on
every pass (`BoundaryNotPresent`, "this error repeats until the historian re-cuts the
compartments"). Correct and wedge-proof, but there is no re-cut machinery. This design adds it.

## Current mechanics (source facts)

- Core: `boundary_present=false` → `reconcile_pending` set; reconcile+absent classifies HARD
  (rematerialize). Core-side is done; the module-side mint is what fails.
- Module HARD arm (`transform.rs` ~560-640): `resolve_coverage` mints the anchor from the
  store's last folded compartment; the mint-absent guard errors loud when the anchor block id
  is not in the live array (deliberately including the reconcile-rematerialize path).
- Store: `replace_compartments(session, comps)` exists (full rewrite, fenced but
  version-blind); `append_compartments` is the publish path. Compartment rows carry
  NUMERIC ordinals in `start_message`/`end_message` and FLAT BLOCK IDS (`m<id>#<idx>`) in
  the separate `start_message_id`/`end_message_id` columns (R4-F3).
- Historian firing state machine is orthogonal (Idle/Firing/...); publish validates a
  content fingerprint of the pinned range and abandons on mismatch, so a mid-firing revert
  cannot publish stale rows.

## Design

### D1 — re-cut is deterministic truncation, no LLM
A revert invalidates every compartment that covers now-absent content. The recovery is NOT a
re-summarization: it is deleting the invalidated suffix of the compartment list. The
surviving prefix (compartments fully covered by the live array) stays; raw content between
the surviving coverage end and the live tail re-enters the eligible head and gets
re-summarized by NORMAL future firings (trigger-driven, no special path). This mirrors the
system's own publish/fold division: deterministic host mechanics vs LLM production.

### D1b — revert-epoch publish fence (R2-F1/F2/F3/F4 corrected)
The fresh-fire path sets `observed_chunk_fingerprint = chunk_fingerprint` at assembly, so
publish validates against what the FIRE saw — a revert between fire and publish can land
stale compartments. Fence design, corrected on all four round-2 axes:

- STORAGE (R2-F2): `revert_epoch: u64` lives at the SESSION level in the store row,
  OUTSIDE `HistorianDurableState` — the historian task's `persist_historian_state`
  overwrites `meta.historian` wholesale from stale in-memory state, so anything inside
  historian state can be clobbered mid-run. Session-level meta is written only by
  serialized transform passes.
- SNAPSHOT POINT (R2-F1): the epoch is read at ASSEMBLY, in the same store load that cuts
  the chunk (`assemble_historian_firing`), and carried on `AssembledHistorianFiring` →
  `HistorianFireRequest` as `expected_revert_epoch`. Fire-time or publish-time reads are
  too late: a re-cut committing between assembly and fire must already invalidate the
  firing.
- REATTACH CARRY (R2-F3): `fire()` persists `expected_revert_epoch` into the durable
  firing fields (alongside `chunk_fingerprint`/`producer_run_id`); reattach reads it back
  from there — never from the current session epoch.
- PREDICATE + ATOMICITY (R2-F4): `publish_historian_chunk`'s predicate compares the
  firing's `expected_revert_epoch` to the CURRENT session epoch INSIDE the publish
  transaction. The re-cut's epoch increment happens in the SAME fenced transaction as the
  compartment truncation (one store method: truncate + bump epoch + row-version predicate).
  Crash after that tx but before the pass commit: store truncated AND epoch bumped → any
  stale publish is already rejected; re-entry recomputes the fold from the truncated store
  (idempotent, truncation of an already-truncated prefix is a no-op). This closes the
  two-transaction window: the fence and the truncation are indivisible.

### D2 — where it runs: inside the reconcile-HARD arm, truncate-then-mint, one bust
When `apply_once` runs a HARD with `reconcile_pending` and the minted anchor fails the
presence guard, instead of erroring immediately:
1. Compute the surviving prefix: walk stored compartments in sequence order; a compartment
   SURVIVES iff its `end_message_id` — the FLAT BLOCK ID column (`m<id>#<idx>`), NOT the
   numeric `end_message` ordinal column (R3-F4) — is present in the live array. First
   non-surviving compartment truncates the list there (everything after is invalidated even
   if a later end id happens to match — contiguity is by prefix).
2. Truncate via `truncate_compartments_for_revert` (R5-F3: never bare
   `replace_compartments`, which is fenced but version-blind — full contract under CAS
   discipline below).
3. Recompute the fold from the surviving prefix: mint anchor = surviving last end (guard
   re-checked — it now passes by construction), or if NOTHING survives, fall back to the
   bootstrap arm (empty boundary, no compartments → the pass completes as a plain HARD with
   no session-history; `has_compartments` now false, so no first-fold loop).
4. Reconcile clears on the same pass (the core's step_hard applies new_boundary_id).
One pass, one bust, no intermediate loud-error rounds. The loud error REMAINS for any path
where truncation itself cannot restore consistency (store CAS conflict → next pass retries).

Durable-meta consistency on the same commit (F2, F3 corrected): `coverage_ordinal`
recomputed from the surviving prefix (or cleared when nothing survives);
`folded_compartment_seq` set to the surviving last seq (or bootstrap default). Memory
WATERMARKS (`rendered_memory_ids`, `memory_mutation_cursor`, `max_memory_id`) follow the
NORMAL HARD compose path — the recompose reads active memories and returns fresh manifests
exactly as any HARD does (m0_compose.rs:121-169); what stays untouched is memory CONTENT
(no rows deleted — the D4 policy). The `m1_revision` digest MUST be recomputed AFTER
the truncate tx (it folds `max_compartment_seq`, which truncation changes); the
pre-HARD digest computed at transform.rs:476 is stale by construction on this arm.

Synthetic-todo anchor on shrink (F4): the existing Keep-reanchor handles coverage ADVANCE
only; a re-cut SHRINKS coverage and the pair's anchor may sit in reverted-away tail. On the
re-cut HARD, anchor-absent-from-live is handled like the coverage-fold case: re-anchor the
pair (bytes + call_id unchanged) to the new tail end — cache-safe because this pass is a
bust by definition; if the live tail is empty, emit as the None-anchor case (before-tail
position, existing machinery). Fail-loud stays for anchor-absent on NON-re-cut passes
(existing tests pin that).

CAS discipline (F5, R2-F4, R3-F1 corrected): the re-cut does NOT use bare
`replace_compartments`. One new store method:
`truncate_compartments_for_revert(session, keep_through_seq, expected_row_version)
-> TruncateOutcome { revert_epoch, last_recut, row_version }` — fenced, row-version-predicated,
deletes the invalidated suffix, increments session-level `revert_epoch`, AND bumps the
row_version. The bump fences exactly the STALE writers — any commit still holding a
pre-truncate load (including a historian persist that loaded before the truncate); a
historian persist that loads fresh AFTER the truncate legitimately proceeds and preserves
the new epoch (R4-F2: the bump is stale-load exclusion, not general historian-write
exclusion). The pass commit then uses the RETURNED post-truncate row_version, never the
pass-entry `loaded.row_version` — AND (R4-F1, the blob handoff) the arm PATCHES
`meta.revert_epoch = outcome.revert_epoch` and `meta.last_recut = outcome.last_recut` into
the meta it commits: `commit()` writes the WHOLE ModuleMeta blob with no read-back, so
committing the pass-entry meta clone unpatched would silently un-bump what the truncate tx
just wrote. The returned outcome exists precisely so the arm can both version-target and
content-patch without re-reading.

Shrink handling gates on COVERAGE SHRINK, not on the re-cut arm (R2-F6 + R3-F2): the
round-3 crash matrix showed that re-entry after a durable truncate-tx no longer trips the
anchor-absent guard (the anchor now mints present), so anything special-cased inside the
re-cut arm alone gets BYPASSED on crash re-entry. The shrink-specific handling — todo
re-capture against the post-truncation tail AND the synthetic-todo re-anchor-on-shrink —
therefore keys on the durable observation `new_coverage < meta.coverage_ordinal` (this
pass's recomposed coverage vs what the last pass commit recorded), evaluated on EVERY hard
arm. Crash re-entry: meta still carries the old larger coverage_ordinal, the fold from the
truncated store yields the smaller one → shrink detected → identical handling runs. The
re-cut pass and its crash re-entry are behaviorally identical by construction.

### D3 — gating: only under reconcile; masking risk accepted narrowly (F6)
Truncation must NOT run on ordinary HARDs: a fresh-mint absence there is a publisher-
vocabulary bug and must stay loud (existing behavior, existing hint). The re-cut path is
strictly `reconcile_pending && anchor-absent`. Residual risk (Oracle F6): a publisher-
vocabulary bug coinciding with a legitimate reconcile would be silently truncated — id
presence alone cannot distinguish "reverted away" from "never matched". Accepted narrowly
because the alternative (loud-wedge on every real revert) fails the common case to defend a
double-fault; mitigation: a durable diagnostic `last_recut: Option<String>` — a `#[serde(default)]`
ModuleMeta field WRITTEN BY THE TRUNCATE TX in the same fenced transaction that bumps the
epoch (R5-F2: writer + storage pinned; blob-resident, so it shares the epoch's un-bump
hazard — `TruncateOutcome` carries it back and the arm patches BOTH `meta.revert_epoch`
AND `meta.last_recut` into the committed blob; crash-between-txs keeps it because re-entry
loads the post-truncate blob). Content: dropped seq range, surviving seq, live head/tail
ids, epoch — last re-cut only (overwrite, no growth). So a vocab bug eaten by a re-cut is
reconstructable from the state dump, and the publish-side vocabulary assertions (mint-absent
guard on ordinary HARDs, ChunkLine flat-id contract) remain the primary defense at the
source.

### D4 — facts/events from invalidated compartments
Stay. Promoted facts went to project memory at publish; TS recomp deliberately preserves
curated memories (emits no facts) and archives nothing on structural rebuilds. Same policy.

### D5 — historian firing interactions (R5-F1: epoch-reject outcome pinned)
- Firing in-flight during revert: the fingerprint only protects fires whose ASSEMBLY saw
  the revert; the epoch fence (D1b) covers the assembly→publish gap. When the publish
  transaction rejects on epoch mismatch, the store returns the same conflict class as a
  row-version CAS failure and the historian task handles it exactly like a publish CAS
  conflict: abandon-with-detail (last_failure = "publish rejected: revert epoch mismatch
  (session was re-cut mid-firing)"), durable state → Idle, backoff set — NEVER left in
  Publishing (the busy-forever wedge the recovery arm exists to prevent). Test pins:
  epoch-mismatch publish → Idle + backoff + detail; next pass may re-fire on the truncated
  store after backoff.
- Post-re-cut, the next trigger evaluation sees the boundary at the surviving coverage end;
  the un-covered raw is eligible again; normal firing re-summarizes it. `last_no_fire` /
  progress diagnostics need no changes.

## Contract clause: mid revert-stability (RULED by SUBC, pm_bf05a41f)

SAME MID ⇒ SAME CONTENT, FOREVER. An id is never reused for different content;
revert-then-continue must mint fresh ids for the new continuation. The obligation sits on
whatever feeds the module, per harness (SUBC mirrors this into mc-wiring-contract.md as a
producer obligation):
- llm-runner (owned harness): hazard theoretical today — no revert/rewind/truncate op
  exists on the session surface, lineage is append-only by construction, and
  stamp_ordinals (llmr-protocol content.rs:101) debug_asserts an already-stamped ordinal
  never changes. If llm-runner ever grows revert, the contract forces fresh ids (or a
  session-identity roll), never position re-stamping.
- OpenCode/Pi plugin leg: naturally revert-stable (host ids are ULIDs; revert deletes ids,
  continuation mints fresh ones). Contract is free.
- CC MITM leg: REAL engineering obligation — positional arrays on the wire, so the codec's
  id-synthesis must be content-anchored or sequence-monotonic, never bare-positional.
  Flagged as a MITM-leg design input now (do not rediscover in the codec pass).

Boundary presence stays id-based and cheap (no content hash on the hot path — wrong layer);
#406's `full_array_fingerprint` is the intended content-divergence belt-and-suspenders.

## Open questions (for Oracle)

Non-prefix reverts (F7, R2-F7 corrected): survival-by-end-id + prefix truncation assumes
prefix-only deletion, stable ids, stable ordinals. Non-prefix splices are FORBIDDEN by
contract (llm-runner's stamp_ordinals debug_asserts it; OpenCode/Pi reverts are prefix-
truncations; the MITM codec must synthesize monotonic ids). `enforce_block_identity` fails
loud on same-mid-different-bytes for LIVE mids. HONEST LIMIT (round-2 corrected): an
interior-id ABSENCE with a present later end id is NOT loud-detected today —
`resolve_coverage` validates stored compartment-to-compartment tiling only, not that every
covered live id still exists. It is out of contract and stays undetected until #406's
full_array_fingerprint lands (noted there as the detection layer); the re-cut neither heals
nor masks it (it only ever deletes suffixes).

Q2 (Oracle): partial-compartment invalidation. A compartment covering m2..m5 where the live
array now ends at m3 is dropped whole (D2 rule: end-id presence). The raw for m2..m3 is
still live and returns to the tail — no history loss, only re-summarization cost. Confirm no
edge where dropping-whole loses content that is NOT still live (can start-id be absent while
end-id present? Only with id reuse — Q1 — or non-prefix reverts, which OpenCode/llm-runner
don't do; the prefix-truncation rule in D2 step 1 handles interleaved staleness
conservatively).

Q3 (Oracle): meta/anchor atomicity. Truncation (store write) and the fold commit (core
state + meta) are TWO fenced transactions in this design (the truncate tx, then the
pass commit). Crash between them: store truncated, boundary still old-absent → next pass
re-enters the same reconcile-HARD arm, truncation is a no-op (already-surviving prefix),
mint proceeds. Idempotent, but confirm no window where a CONCURRENT publish (feature-flagged
future multi-writer) could interleave. Single-writer today per store lease.

### Serde compatibility (R3-F3)
All three new fields carry `#[serde(default)]`: session-level `revert_epoch: u64` and
`last_recut: Option<String>` on ModuleMeta, and `expected_revert_epoch: u64` on the durable
firing fields. Default 0 composes for
pre-upgrade state: an old firing (0) publishing into an old session row (0) passes; any
post-upgrade re-cut bumps to ≥1 and fences that stale firing out.

## Tests (gate arc, per SUBC's pinned ask)

Round-3 additions: crash-after-truncate re-entry runs the shrink handling via the
coverage-shrink gate (todo re-capture + re-anchor despite the anchor minting present);
pass commit uses the post-truncate row_version (a stale-version writer between the txs is
rejected); pre-upgrade epoch-0 state loads and publishes, and is fenced after any re-cut.
The full arc as a handler-level test: seed store + live to a folded state → simulate revert
(live array truncated below coverage) → pass 1: core flags reconcile, HARD arm truncates and
re-mints from surviving prefix, boundary present, reconcile cleared, NO error → tail content
re-eligible → scripted producer re-fires and publishes → subsequent fold covers the re-cut
range. Plus: nothing-survives → bootstrap arm; ordinary-HARD absent-anchor still errors loud
(D3 regression); truncation idempotence (re-entry after simulated crash-between-txs); CAS
conflict on truncate_compartments_for_revert → loud error, next pass retries clean.
