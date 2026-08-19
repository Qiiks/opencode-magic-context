# Six-axis review: external PR #340

- **PR:** `cortexkit/magic-context#340`, “feat(memory): preserve corroborating episode evidence”
- **Reviewed head:** `1975b8a81574d0881b88d3a0291016e3a7911510`
- **PR base:** `b27a6d7672cb9c8a119998b9aa0ae1b9017cc87f`
- **Related proposal:** issue #335 by `iceteaSA`, “Memory promotion is single-episode”
- **Migration role:** owns v79 and therefore must land before PR #341's v80 migration
- **Review date:** 2026-08-19

## Verdict: needs-rework

The direction is right. Here an **episode** means one session's observation of one content version. A content-bound `(memory, content hash, session)` evidence set makes independent-session corroboration computable without adding a sixth memory category, changing the existing category/hash uniqueness key, or putting classification metadata into rendered memory bytes. The TypeScript/Pi tool paths, merge/rekey paths, authority seed/mirror payloads, and Rust `ctx_memory` facade now preserve substantially more provenance than `seen_count` alone.

The independent issue #335 proposal from `iceteaSA` and this implementation from `coleleavitt` converge on the same missing primitive. Two community actors independently identifying the discarded episode set is strong signal that the gap is real and worth fixing.

The PR is not merge-ready for two blocking reasons:

1. **Rust historian promotion—the main autonomous memory producer in Rust mode—still does not record episodes.** `crates/mc-module/src/historian.rs::to_store_fact()` leaves `source_session_id` as `None`; `publish_historian_chunk()` calls `promote_facts_tx()` without the publishing session; and `promote_facts_tx()` still skips an exact active-content duplicate without calling `record_memory_evidence_tx()`. A newly promoted historian fact also receives no evidence row. The new Rust tests exercise direct `insert_memory`/facade writes, not incremental or wrap-up historian publication.
2. **`memory_evidence` is authority-relevant but remains outside the structural armed-store fence.** It is seeded into and mirrored from the module authority store, yet the TypeScript DB has no `memory_evidence_authority_guard_*` triggers. `recordMemoryEvidence()` is exported without its own `assertTsMemoryIdWriteAllowed()` check. A new episode currently also updates the guarded parent row, so that wrapper should roll back under MODULE authority, and the public idempotent insert path checks authority first. The protection is nevertheless incidental: raw/future evidence-only insert, update, or delete code can mutate a MODULE/DRAINING project's read model without touching `memories`. `migrations-armed-replay.test.ts` still enumerates only the six memory/note parent-table triggers and never probes the new table.

The current deduplicated check view passes all 14 checks; the optional `[code]smith` check is skipped. The rollup also retains one older failed smoke attempt, discussed in the verification notes. The passing rerun does not cover either blocking path above.

## Issue #335: agreement and divergence

### Agreement

The PR implements the primitive issue #335 actually asks for:

- evidence is durable rather than collapsed into one counter;
- a repeated extraction in the same session is idempotent;
- a distinct session can increase corroboration;
- provenance remains attached to the memory through updates and canonical merges;
- `seen_count` can be reconciled from distinct sessions while retaining a legacy baseline for pre-v79 counts.

The chosen primary key, `(memory_id, content_hash, source_session_id)`, is stronger than the issue sketch's `(memory_id, session_id)` for content edits: it retains which wording/version a session observed. `source_message_id`, when available, supplies a useful evidence anchor.

### Deliberate divergence

The primitive does **not** decide that two differently phrased rows express the same fact. Different normalized hashes remain separate memories until a curate/merge path consolidates them; only then is their evidence unioned. That is compatible with #335's statement that the episode table is the ask and semantic policies are optional, but the PR description should not imply that paraphrase corroboration is automatic.

There is also no explicit `session_fact_ordinal`; `source_message_id` is optional and historian promotion currently supplies neither. The episode set still answers “how many independent sessions support this canonical memory,” but not a complete ordered audit trail within each session.

## 1. Design fit with the memory system

### Taxonomy, dedup, and classification

- The five-category taxonomy is unchanged. Evidence stores provenance only; it does not add or overload a category.
- The canonical TypeScript uniqueness key remains `(project_path, category, normalized_hash)`. The Rust exact-content promotion policy is also unchanged.
- Classification remains cache-neutral: evidence recording does not set importance, scope, shareability, verification, or `classified_at`. It therefore does not violate the #7034 doctrine that classification is background metadata rather than an immediate render event.
- `renderMemoryLineV2` and the Rust memory renderer are untouched; evidence and `seen_count` do not appear in memory-line bytes.

### `seen_count` semantics

The TypeScript and Rust direct-write implementations use a sound compatibility formula:

- same `(content hash, session)` insertion is ignored;
- a newly observed session adds one distinct-session unit;
- when v79 evidence already exists, `seen_count - distinct_evidence_count` is retained as the unknown legacy baseline;
- when no evidence exists, an observation from a session different from the singular legacy `source_session_id` advances the old count rather than resetting it;
- after a content update, two evidence rows from the same session still count as one corroborating session;
- merge/rekey paths add legacy baselines and union distinct sessions rather than blindly summing evidence counts.

That makes corroboration computable while preserving pre-v79 information that cannot be reconstructed.

### Blocking design parity gap: Rust historian

The direct module `ctx_memory` path is episode-aware, but the historian is not:

- `to_store_fact()` projects validated facts with `source_session_id: None`.
- `publish_historian_chunk()` has `request.session_id`, but does not pass it into `promote_facts_tx()`.
- `promote_facts_tx()` uses a `HashSet<String>` of active content; an exact duplicate takes the `continue` branch, so it neither records the new session nor refreshes the count.
- New historian rows are inserted directly rather than through the episode-aware insert primitive.

This leaves Rust-mode incremental and wrap-up promotion with the old “discard the episode set” behavior—the exact gap the PR claims to close.

**Required change:** thread the publishing session through the Rust historian store transaction. Existing exact-content matches must call the transaction-local evidence recorder; new facts must create their initial historian evidence row atomically. Preserve the existing `promote_facts`/unanchored gating so a skipped wrap-up cannot mint evidence. Add tests through `publish_historian_chunk`, not only `insert_memory`.

## 2. Migration v79 and armed-store doctrine

### STRUCTURE.md rule audit

| Requirement | Result |
|---|---|
| Version allocation | **Pass.** Claims v79, immediately after v78. |
| Fence bump | **Pass.** `LATEST_SUPPORTED_VERSION` is 79. |
| Fresh database parity | **Pass.** `initializeDatabase()` creates `memory_evidence` and its session index. |
| `ensureColumn()` | **Not applicable.** This is a new table, not a column on an existing table. |
| Co-located test | **Present but incomplete.** `migrations-v79.test.ts` exists. |
| `clearSession()` | **Correctly omitted.** Evidence is memory-owned durable provenance; `source_session_id` is an observation identifier, not a session-owned row. Clearing it with raw session history would recreate the information loss #335 identifies. Memory deletion cascades the rows. |
| `LATEST_SUPPORTED_VERSION` lockstep | **Pass.** The migration ceiling and array maximum are both 79. |

### Migration-test non-vacuity gap

`migrations-v79.test.ts` calls `initializeDatabase()` before migration. The latest fresh schema has already created `memory_evidence`, so deleting v79's `CREATE TABLE` statement would not make this test fail. It validates backfill into an existing table, but not a real v78-to-v79 upgrade, table/index creation, or replay idempotence.

**Required change:** construct or restore a genuine v78 fixture without `memory_evidence`, mark migrations through v78, run the v79 step, and assert the table shape, primary key, index, backfill, one v79 ledger row, and idempotent re-run. Keep a separate fresh-DB parity assertion.

### Blocking armed-store gap (#14597)

The new table participates in authority transfer:

- TypeScript authority seed pages include evidence arrays.
- module state sync and authority seed ingest those arrays;
- module changefeed snapshots return evidence to the TypeScript mirror;
- mirror apply deletes/reinstalls evidence as an authoritative snapshot.

Despite that role, only `memories` and `notes` have durable authority guard triggers. A raw evidence insert/update/delete can occur while the parent memory's project is MODULE/DRAINING because SQLite does not require it to update the guarded parent row. The current `recordMemoryEvidence()` wrapper follows a successful insertion with a parent `seen_count` update, so that particular transaction should trip the existing guard and roll back; the database invariant itself is still open. The armed replay test simply advances through v79; it neither claims/populates `memory_evidence` nor proves an unprivileged evidence-only write is refused.

**Required change:** add insert/update/delete authority guards for `memory_evidence`, joining through the parent memory's project and using the same durable privilege-state predicate as the existing guards. Install them after v79 backfill so migration can reconstruct old evidence, add them to `GUARD_TRIGGERS`, prove raw and public-API writes fail while armed, and prove privileged mirror replay still succeeds. `recordMemoryEvidence()` should also assert the parent memory's TypeScript authority before opening its transaction.

### Sequencing

This PR is the v79 anchor already assumed by the PR #341 review. It must land before #341's v80 migration; shipping v80 first causes the append-only `candidate.version > currentVersion` selector to skip a later v79 forever. Treat #340 as a schema-fence mover and coordinate its rollout/restart window before rebasing #341 on top.

## 3. Cache safety

### What is safe

- Evidence-only writes do not append `memory_mutation_log`, bump `project_memory_epoch`, or change the canonical memory ID.
- `renderMemoryLineV2` and Rust memory rendering are unchanged, so `seen_count` and evidence do not enter m0/m1 text.
- TypeScript duplicate promotion therefore leaves existing m0/m1 bytes and watermarks untouched.
- Rust evidence insertion emits a richer mirror/changefeed snapshot, but the m1 revision signal remains based on memory IDs, memory mutations, compartments, notes, and profile state—not evidence or `seen_count`.
- Host-to-module evidence-only synchronization rewinds the memory watermarks only to ship the corrected row; it does not reseed compartments.
- A later natural bust may observe the refreshed `updated_at` or changed ranking, which is acceptable: the doctrine forbids a background corroboration write from causing its own render event, not from being visible at the next legitimate materialization.

### Missing proof

No focused test records a duplicate episode between two otherwise identical transform passes and byte-compares frozen m0/m1 output or proves the classifier remains defer/no-bust. The generic render functions being untouched is strong static evidence, but this is exactly the sort of silent invariant that deserves a non-vacuous regression.

**Required change:** add TypeScript and Rust-mode regressions that materialize a memory, capture m0/m1 bytes and revision signals, record same-session and new-session evidence, then assert no background-only HARD/SOFT arm and byte-identical output until an independent natural bust. At the natural bust, assert the rendered memory line is still unchanged.

## 4. Blast radius

| Surface | Result |
|---|---|
| TypeScript historian promotion | **Covered.** `promoteSessionFactsDurable()` uses `insertMemoryIdempotent()` with the source session. Existing incremental/wrap-up gating remains intact. |
| Rust historian promotion | **Blocking gap.** Both incremental and wrap-up publication enter `promote_facts_tx()` without episode attribution. |
| OpenCode `ctx_memory` | **Covered.** Write/update/merge paths supply session evidence; exact duplicates request memory sync. Source message attribution uses the tool-owning message when available. |
| Pi `ctx_memory` | **Covered.** The shared idempotent storage path and tests mirror OpenCode behavior. |
| Dreamer verify/update | **Covered.** Content-bound evidence survives content updates; a later observation can attach to the new hash. |
| Dreamer curate/merge | **Covered.** Canonical merges union episode sets and preserve a legacy baseline. |
| Dreamer retrospective | **Covered.** Direct retrospective insertions now use the idempotent evidence-aware primitive. |
| Identity rekey/v22 backfill | **Covered.** Collision merges union evidence in TypeScript and Rust route-binding repair paths. |
| Workspace sharing | **Compatible.** Workspace union remains read-only; evidence follows the owning memory and is merged when identities collapse. |
| Authority seed/state sync/mirror | **Functionally covered.** Sparse-vs-empty evidence semantics are tested, and evidence-only sync can resend an existing memory ID. Structural TypeScript guard coverage is still missing. |
| Memory delete | **Covered.** Foreign keys cascade evidence on canonical deletion. |

## 5. Test adequacy

### Strengths

- Same-session repeat versus distinct-session corroboration is behavior-tested in both TypeScript and Rust direct-write paths.
- Legacy `seen_count` baseline advancement and content-version distinct-session counting are covered.
- Merge, identity collision, v22 rekey, authority seed/state sync, sparse-vs-explicit-empty snapshots, and delete lifecycle receive targeted tests.
- OpenCode and Pi tool tests cover evidence attribution and cache-neutral mutation-log behavior.
- Evidence arrays are tested across TypeScript/Rust authority boundaries.

### Gaps

- No auditable red-first commit exists: no published commit shows the new regression failing before the implementation is added. The first commit adds schema, implementation, and tests together; later commits fix review findings and update tests in the same commits.
- No Rust historian publish test catches the principal parity failure.
- The “independent phrasing” test manually merges two rows. It proves evidence union after consolidation, not automatic semantic corroboration. That is acceptable scope for #335's primitive, but the distinction must be explicit.
- The v79 test is not a genuine v78 upgrade and is vacuous with respect to table creation.
- The armed replay has no evidence-table claim, authority trigger, or refusal probe.
- No direct m0/m1 byte-stability test covers an evidence-only update.

## 6. Code fit

### Good fit

- The evidence table is small, normalized, foreign-keyed to the memory, and indexed by session.
- The primary key keeps repeated extraction in one episode idempotent while retaining content-version provenance.
- Shared TypeScript helpers remove ad-hoc `seen_count` bumps from historian, retrospective, OpenCode, and Pi paths.
- Merge/rekey logic centralizes legacy-baseline reconciliation rather than duplicating arithmetic at every caller.
- Sparse evidence snapshots preserve old data while an explicit empty array authoritatively clears it, which is the correct mirror/seed compatibility rule.

### Changes needed for house fit

- Do not make side-table authority depend on the current wrapper's incidental parent update. Add the database guards, and either keep the transaction-local primitive private or make the exported wrapper perform an explicit parent authority check.
- Thread historian session provenance through the existing store boundary rather than adding a second Rust promotion policy.
- Add durable comments that explain why `memory_evidence` is memory-owned and intentionally survives `clearSession()`; that omission otherwise looks like a STRUCTURE.md miss.
- Keep claims precise: the PR makes independent-session support computable **after canonicalization**; it does not perform semantic paraphrase clustering.

## Exact change requests

1. **Fix Rust historian parity.** Pass the publishing session into `promote_facts_tx()`, record evidence for new and exact-duplicate historian facts atomically, preserve unanchored/`promote_facts=false` skips, and test both incremental and wrap-up publication paths.
2. **Fence `memory_evidence` under module authority.** Add durable insert/update/delete triggers, parent-project checks, armed replay claims/refusal probes, and a public-writer authority assertion while preserving privileged mirror/backfill writes.
3. **Make the v79 migration test non-vacuous.** Exercise a true v78 schema without the table, assert DDL/index/backfill/ledger/idempotence, and separately retain fresh-schema parity.
4. **Add cache-neutrality regressions.** Prove same-session and new-session episode writes do not alter m0/m1 bytes or trigger a background render in TypeScript or Rust mode.
5. **Add the missing semantic-scope assertion.** Keep a test that shows different wording remains separate until curate/merge, then becomes two-session evidence after consolidation; document that semantic clustering is outside the primitive.
6. **Land migrations in order.** Merge/deploy #340 as v79 before rebasing/landing #341 v80, with the schema-fence restart coordination already identified in the #341 review.

## Verification notes

- Reviewed the complete seven-commit diff at `1975b8a81574d0881b88d3a0291016e3a7911510`, including the post-review invariant and formatting fixes.
- Compared the implementation against issue #335, the PR #341 sequencing review, STRUCTURE.md migration rules, TypeScript promotion/storage/render paths, authority transfer, and the Rust store/module twin.
- `git diff --check b27a6d7672cb9c8a119998b9aa0ae1b9017cc87f 1975b8a81574d0881b88d3a0291016e3a7911510` passed.
- `gh pr checks 340` reports 14 passing checks at the reviewed head; the optional `[code]smith` check is skipped. The requested `statusCheckRollup` view contains 23 successful records because it retains reruns, plus one failed `opencode HTTP probe (0.0.0.0)` attempt from 2026-08-19 11:38:51Z; a later current-head run of that check passes.
- No merge, post, push, or PR mutation was performed.
