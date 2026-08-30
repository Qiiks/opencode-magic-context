# GitHub issue #393 — memory ids survive Rust authority round-trips

Date: 2026-08-30

## Contributor register

- Reporter: [@iceteaSA](https://github.com/iceteaSA), their 13th verified finding
- Surface: Rust memory authority activation, steady mirror pull, and drain back to TypeScript
- Evidence: one activate → revert cycle preserved 3,262 rows and every compared payload value, but rekeyed 245 rows and dropped vector recall for 225 of them
- Outcome: source-confirmed and fixed in the TypeScript mirror identity projection

**STORE MIGRATION: NONE.**

**FENCE MOVEMENT: NONE.** This change does not alter an authority state, managed-write trigger, schema fence, migration ceiling, or module route fence.

## Source confirmation

The reported delete/reinsert mechanism is confirmed. It spans the module reset, the host backlink uniqueness rule, and mirror feed ordering:

1. `McStore::authority_begin_prepare` starts a new memories generation by executing `DELETE FROM mc_memories WHERE context_store_uuid = ? AND project_path = ?` (`crates/mc-store/src/lib.rs`). The memory feed's delete trigger emits a tombstone for each deleted module row.
2. `seed_memory_snapshots` then seeds the host snapshot. A row removed by the reset no longer satisfies `memory_by_identity`; if no retained natural-key candidate exists, `memory_upsert` inserts it under a fresh module autoincrement id. `authority.seed` returns those new module ids to TypeScript.
3. `prepareAuthority` already tried to record each returned module id against its `source_row_id`, but `rememberIdentity` used `INSERT OR IGNORE`. The old module id still occupied `mirror_identity`'s `UNIQUE(domain, context_row_id)` slot, so the new backlink was silently ignored.
4. During both ordinary `pullAndApplyMirrorPage` and `drainAuthority`, `applyMemoryRow` consumed the old tombstone first. The stale backlink resolved it to the durable host row, so the tombstone deleted the host row and its FK satellites.
5. The later insert carried the correct `context_store_uuid` and `context_row_id`, but that host id had just been deleted. The stable-id lookup failed, the natural-key lookup found no remaining row, and `contextMemoryId` called `insertMemory`, allocating a fresh `memories.id` above the prior maximum.

That is a true DELETE+INSERT round trip despite byte-identical memory payloads.

### The 245-row discriminator

The source-level discriminator is **the module row's current host source identity**, concretely the rows selected at prepare time by:

```sql
context_store_uuid = :current_context_store_uuid
AND project_path = :authority_project
```

Those rows are deleted by `authority_begin_prepare`. Identity-less state-sync/facade rows are not selected. `module-state-sync.ts` sends host memory ids as ordinary `id` fields, and `replace_authority_memories_tx` updates a known source-identity row when one exists; its ordinary insert branch does not stamp `context_store_uuid`/`context_row_id`. Module-minted rows likewise begin without a host source identity. `mapping_origin` does not participate in the reset predicate.

Source alone cannot prove why the reporter's persisted module database contained exactly 245 matching rows rather than another number. That cardinality requires the retained incident `mc_cache.db`; the source-reproducible count is `SELECT COUNT(*) FROM mc_memories WHERE context_store_uuid = ? AND project_path = ?` immediately before `authority.prepare`. The affected class is not “all 3,262 memories” and not “all mapped memories”; it is the current-store-identity subset. The destructive host leg additionally required its old module ids to retain stale `mirror_identity` backlinks.

## Fix

Stable source identity now replaces stale mirror identity instead of competing with it:

- authority seed responses rebind `(domain, module_project, new_module_row_id)` to the original `source_row_id` after removing the old context-row backlink;
- a steady mirror row carrying this store's `context_store_uuid/context_row_id` uses the same authoritative rebind;
- before a memory page applies tombstones, all later rows in that page carrying valid local source identity are prebound, so a tombstone+replacement pair updates the durable host row in place; and
- natural-key-only rows remain conservative: without stable source identity, they may update a uniquely matching row but cannot steal its canonical backlink.

Both call paths use `applyMirrorPage`: `pullAndApplyMirrorPage` for steady operation and `drainAuthority` for revert/drain. Legitimate tombstones with no replacement still delete their host row; the pre-existing tombstone deletion contract was retained rather than reversed.

## `memories.id` blast-radius audit

| Reference | Enforcement / behavior | Effect of host DELETE+INSERT rekey |
| --- | --- | --- |
| `memory_embeddings.memory_id` | FK to `memories(id) ON DELETE CASCADE`; vector reads join on the id | Old vectors are deleted and the new row is unembedded. This is the confirmed recall loss. |
| `memory_verifications.memory_id` | FK to `memories(id) ON DELETE CASCADE` | File mappings and verification evidence are deleted. A later mirror mapping snapshot may recreate them, but there is an evidence-loss window and a sparse row may not restore them. |
| `memories.superseded_by_memory_id` | Unenforced self-reference | A source row that itself rekeys preserves the numeric value, but any pointer whose target rekeys can become dangling or point at an unrelated future row. |
| `memory_mutation_log.target_memory_id` and `.superseded_by_id` | Unenforced append-only ids | Historical render mutations remain under the gone id and no longer describe the reinserted row. |
| `mirror_identity.context_row_id` | No FK; unique per domain context row | This was the causal stale backlink. A raw rekey leaves it dangling; the mirror path later remaps to the fresh id, losing continuity. The fix prevents the host rekey and replaces stale module backlinks. |
| `mural_manifest.memory_ids_json` | JSON list, no FK or translation | The rendered mural manifest retains gone ids until rebuilt. |
| `embedding_measurement_corpus.primary_result_ids_json` / `.shadow_result_ids_json` | Historical JSON result lists | Measurements retain non-resolving old ids; latency data survives but result attribution no longer joins to the live memory. |
| `synapse_batch_ledger.manifest_json` when `scope='memory'` | JSON batch manifest whose item `id` is the memory id | An in-flight or retained embedding batch remains addressed to old ids; responses cannot populate the replacement rows correctly. |
| `memories.merged_from` | JSON provenance list of memory ids in normal merge writes | Provenance ids that rekey stop resolving to the live source rows. This is audit damage, not a relational FK failure. |
| `identity_merge_log.row_id` / `.target_row_id` when `table_name='memories'` | Generic historical text ids | The audit remains a record of the old operation, but direct correlation to the current row is lost. |
| `v22_backfill_failures.row_id` when `table_name='memories'` | Generic migration-failure ledger | No FK; a recorded failed-row identity is not translated. It is historical and not on the live recall path. |
| `memories_fts.rowid` | FTS external-content rowid maintained by memory insert/delete/update triggers | The old FTS row is deleted and a new row is inserted, so text remains searchable, but under the new memory id. |
| `session_meta.cached_m0_max_memory_id` | High-water mark, not a row reference; normal memory invalidation clears it | Rekeying upward changes delta/watermark behavior but does not leave a direct dangling join. |
| `mirror_live_memory_rows.module_row_id` and `mirror_pending_references` module ids | Module-side identity, not `memories.id` | Not directly broken by a host id change; `mirror_identity` is the translation boundary. |
| `project_state`, `session_projects`, workspaces/members, user memories, notes, compartments, and `m0_mutation_log` | Keyed by project/session/workspace/compartment identities | No `memories.id` dependency found. Workspace references are project-path based. |

Opaque `metadata_json`, note manifests, and generic tool payload text have no schema-defined `memories.id` join and were not classified as id-keyed references. Module-side `mc_memory_mappings.memory_id` keys the module row id, not the host id; it is not directly invalidated by a host rekey.

## Invariant and mutation evidence

The regression fixture includes:

- a host row whose old module backlink is replaced during activation (the incident discriminator);
- a host row with a missing `mirror_identity` backlink but valid local source identity;
- an identity-less module-side/legacy row recovered only by the unique natural key;
- a supersede source/target pair;
- an embedding and a verification mapping attached to the re-minted source.

`memoryIdentityFingerprint` records the row count plus the two fingerprints credited in the report:

- SHA-256 over ordered `id || content`; and
- SHA-256 over ordered `id || '>' || superseded_by_memory_id`.

The fixture performs prepare/activation, a steady mirror pull, and a drain/revert. The full fingerprints, host ids, embedding owner id, and verification owner id must remain identical. A stable count with either moved hash fails.

Both deliberate mutations used the exact `NON-VACUITY BREAK` marker and were restored immediately:

| Deliberate break | Red evidence |
| --- | --- |
| Disable replacement of the stale context-row backlink, restoring `INSERT OR IGNORE` behavior | `context-authority.test.ts:1518` kept count `4` but changed both content and supersede fingerprints after steady mirror pull. |
| Disable replacement prebinding before page tombstones | `context-authority.test.ts:1545` kept count `4` but changed both fingerprints after drain/revert. |

No mutation marker remains in the tree.

## Verification

- `bun test src/features/magic-context/context-authority.test.ts` — passed, 45 tests and 141 assertions.
- `bun run typecheck` in `packages/plugin` — passed (`retina-local-fs` build types, plugin no-emit types, and plugin script types).
- `bun run test` in `packages/plugin` — passed, 4,238 tests and 21,308 assertions.
- `bunx biome check src/features/magic-context/context-authority.ts src/features/magic-context/context-authority.test.ts` — passed after formatting.
- `cargo test -p mc-module` — skipped because no module-side source, identity schema, or Rust code changed; the source audit read the existing module/store behavior only.

`bun install --frozen-lockfile` was run in the plugin workspace before verification and changed no dependency or lockfile.

## Scope boundary

The 71 dangling supersede pointers reported by @iceteaSA are pre-existing and out of scope. This patch neither repairs nor rewrites them. Whether archived supersede targets should remain resolvable is a separate audit question.

## Reply draft

Thanks @iceteaSA — source-confirmed. Your paired count+hash fingerprint caught exactly what count checks and content-only hashes are structurally unable to see: the mirror preserved every compared payload value while replacing durable row identities.

The reset begins module-side: `authority_begin_prepare` deletes rows stamped with the current `context_store_uuid` and authority project, then seed can reinsert them under new module ids. TypeScript received those new ids, but `rememberIdentity` used `INSERT OR IGNORE`; the old module backlink still occupied `UNIQUE(domain, context_row_id)`, so the replacement backlink was silently discarded. Mirror pull then applied the old tombstone first, deleted the host row and its FK satellites, and the later insert could no longer find `context_row_id`, so SQLite allocated a fresh host id. That explains the contiguous ids above the previous maximum and the embedding loss.

The exact source discriminator is the module rows carrying the current store UUID and project, not `mapping_origin`. Source establishes that subset selection but cannot derive the incident count of 245 without the retained module DB; that count is directly recoverable with the matching-UUID/project query above.

The fix makes stable source identity authoritative: activation replaces stale backlinks, and steady/drain page application prebinds local source identities before tombstones. The round-trip fixture covers the stale-backlink class, a missing backlink, an identity-less module row, supersede pointers, embeddings, and verification mappings. It compares count plus SHA-256 over ordered `id||content` and `id||'>'||superseded_by`; restoring stale-backlink behavior or skipping tombstone prebinding makes the invariant red while count remains unchanged.

The blast-radius sweep confirmed more than embeddings: verification mappings cascade away; mutation-log ids, supersede targets, mural manifests, pending Synapse manifests, merge provenance, and measurement/audit ids can become stale. FTS rebuilds correctly under the new rowid, and workspace/session references are project keyed. The 71 pre-existing dangling supersede pointers remain untouched as a separate audit question. No migration or fence movement was needed.
