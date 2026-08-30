# Synapse phase-2 gates 1 and 6 closure

**Verdict: CLOSED IN CODE; LIVE REPAIR DEFERRED TO THE ORGANIC DRAIN**

**Read-only candidate validation:** 2026-08-30T19:32:18Z

**Reference snapshot:** `synapse-phase2-cutover-validation-2026-08-30.md` at 2026-08-30T18:49:00Z

## Repair

Shadow chunk coverage is now key- and hash-complete. For every primary `(compartment_id, window_index, chunk_hash)`, the current shadow identity must contain the same key and hash. A same-key row with a different `chunk_hash` remains outstanding and the existing replace-on-write worker re-embeds the compartment.

The passive project selector, `/ctx-embed-history` selector, session outstanding count, and `/ctx-embed` coverage report now reconstruct the current canonical transcript windows and compare their complete window-key/hash set with the current model rows. A model row's mere existence no longer prevents the transcript hash check from running.

Missing expected window keys are ordered before stale-hash replacements. Provider budgets are unchanged:

- shadow worker: 64 items, 512 KiB, or 2 seconds per tick;
- passive commit sweep: 16 per batch, at most 500 per sweep;
- passive chunk sweep: 8 per batch, at most 200 per sweep;
- memory and session-command batches retain their existing small bounds.

No schema or migration fence changed. `chunk_hash` already stores the required chunk-content identity. Memory content changes invalidate every embedding row and saves use the current `normalized_hash` compare-and-save guard; commit SHA is the Git content identity. Their shadow backlog predicate therefore remains an existence check over rows whose source lifecycle already invalidates changed content.

## Gate-6 invariant

The fixture contains three current-identity compartments:

1. one with an expected window key missing while another model row exists;
2. one with the expected key but a stale `chunk_hash`;
3. one with the complete current key/hash set.

Coverage is `1 / 3`, not drained. Repairing only the missing window leaves the stale row outstanding; repairing only the stale hash leaves the missing window outstanding. Coverage reaches `3 / 3` only after both defects are repaired. The shadow fixture additionally re-arms the ordinary rotation-aware worker and proves it replaces both defects before recording `drained`.

Mutation checks temporarily reverted both candidate paths to existence-only (marked `NON-VACUITY BREAK` during the check). Both named vacuity tests failed, then passed again after restoration. The deliberate breaks were not retained.

## Live candidate-set validation

The context database was opened only as:

```text
file:/Users/ufukaltinok/.local/share/cortexkit/magic-context/context.db?mode=ro
readonly: true
```

No registration, embedding provider, drain, or write path ran. The query selected the newest shadow descriptor per project/scope and computed:

- memory/commit items with a primary row and no current-shadow row;
- primary chunk rows with no exact current-shadow `(compartment_id, window_index, chunk_hash)` row;
- defect class `missing` when the key was absent and `stale` when the key existed with another hash;
- the production candidate query's compartment IDs and its missing-before-stale order.

| Project | Missing memories | Missing commits at report snapshot | Missing chunk rows / items | Stale-hash chunk rows | Candidate set equals defect union | Missing ordered first |
|---|---:|---:|---:|---:|---|---|
| Magic Context | 0 | 0 | 6 / 3 | 633 | yes | yes |
| AFT | 0 | 0 | 26 / 13 | 627 | yes | yes |
| Alfonso | 4 | 29 | 18 / 13 | 785 | yes | yes |
| **Total** | **4** | **29** | **50 / 29** | **2,045** | **yes** | **yes** |

The report-snapshot candidate set therefore contains exactly the reported **62 missing items** (`4 + 29 + 29`) and **83 missing vector rows** (`4 + 29 + 50`), plus all **2,045** stale-hash rows.

The database remained live after the reference snapshot. At validation time Alfonso had one additional missing commit, `df5da6943b01…`, committed at 2026-08-30T18:53:03Z—four minutes after the report cutoff. The unrestricted live count was therefore 30; applying the report's 18:49 UTC snapshot boundary reproduced 29 exactly. This post-snapshot row is also correctly selected for organic repair.

No actual live drain was run. After merge, ordinary bounded registration/sweep processing will consume these candidates.

## Verification

- focused compartment selector and shadow backfill tests;
- project embedding registry tests;
- plugin TypeScript typecheck;
- full plugin test suite.
