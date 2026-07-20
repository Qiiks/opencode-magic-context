# Authority round-3 adversarial re-gate — 2026-07-20

## Verdict: **NO-SHIP**

Audited range: `9b3bdf82..777663a5` (`9c0f96e1`, `976093cc`, `47653851`, `777663a5`). The prior R1-R7 verdict was used as the closure checklist; cleared surfaces outside these four remediation commits were not re-litigated.

The delta closes the original detached timer call, PREPARING key exposure, migration-number gap, facade lookup fail-open, cross-root cache-provenance bypass, and canceled dreamer registration leak. The live resnapshot also prevents the original R2 canonical-row deletion on its normal transform cadence.

Two DRAINING interactions remain release-stopping. A crash-resumed drain still calls the old direct page applier, so an upgraded database with a pending resnapshot and a tombstone at its cursor becomes permanently unable to finish the drain. Separately, DRAINING remains identity-resolved for every facade, not only transforms/reads; a mutation that passes the TypeScript MODULE check just before drain begin can commit after the captured feed bound and then be omitted from the TS handoff. The process-memory lineage fix is security-safe after module restart but causes a real, bounded availability regression for legitimate OpenCode memory/note calls until another transform re-observes the root.

## Findings

### G1. **HIGH — crash-resumed DRAINING bypasses the resnapshot entry point and can wedge authority recovery permanently**

**Evidence**

- The safe entry point performs `ensureLiveMemoryResnapshot` before an ordinary pull at `packages/plugin/src/features/magic-context/context-authority.ts:1228-1247`.
- The DRAINING path still calls `module.mirrorPull` followed directly by `applyMirrorPage` at `context-authority.ts:604-614`.
- `applyMirrorPage` correctly rejects a tombstone while status is `pending_check` or `resnapshotting` at `context-authority.ts:1143-1150`.
- Rust startup sees persisted DRAINING and calls this drain path before reconciliation or a normal mirror cadence at `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:431-450`. The normal resnapshot cadence is only reached after a successful transform at `rust-mode-transform.ts:973-980`.

**Failure sequence**

1. A schema-57 context database has an advanced memory cursor and a legacy `mirror_identity`; the module authority is durably DRAINING after a prior crash.
2. Migration 58 creates `mirror_resnapshot_state='pending_check'`.
3. The next Rust pass enters DRAINING recovery before any normal mirror pull.
4. The next feed page contains the legacy cleanup tombstone that motivated R2.
5. `drainAuthority` sends that page directly to `applyMirrorPage`; the resnapshot barrier rejects it and leaves the cursor unchanged.
6. The transform fails before reaching the post-transform mirror cadence. Every retry repeats the same ordering.
7. The authority stays DRAINING, Rust transforms fall back to raw/LKG and eventually park, and TS memory/note tools remain fenced as “not ready.”

A read-only Bun probe of this exact shape returned:

> `{"error":"memory mirror resnapshot must complete before tombstones","mirrorCalls":1,"cursor":20,"status":"pending_check"}`

**Catching test**

Create a schema-57 context fixture with cursor 20, a legacy memory identity, and a DRAINING module whose row 21 is a tombstone. Migrate, run the real `prepareRustMemoryAuthority` recovery path, and assert that a live-only resnapshot occurs before row 21, the cursor advances, and authority reaches TS. Repeat after interruption with status `resnapshotting`.

**Required fix**

Make every memory-feed consumer, including drain recovery, pass through the resnapshot barrier. Prefer one exported resnapshot primitive followed by the drain’s bounded-to-captured-head replay, rather than relying on the post-transform cadence.

---

### G2. **HIGH — a facade mutation can commit after the DRAINING feed bound and be lost at the TS handoff**

**Evidence**

- Drain begin captures `MAX(feed_seq)` once while changing state to DRAINING at `crates/mc-store/src/lib.rs:9340-9354`.
- Route resolution deliberately returns the identity in DRAINING at `mc-store/src/lib.rs:3728-3756`.
- All facade scopes consume that selector at `crates/mc-module/src/lib.rs:7624-7639`; memory writes then proceed at `mc-module/src/lib.rs:7704-7749` and notes use the same scope.
- The TypeScript tool checks authority in one request and sends the mutation in a second request at `packages/plugin/src/tools/ctx-memory/tools.ts:403-442` (notes have the same split at `packages/plugin/src/tools/ctx-note/tools.ts:267-315`). This is a gate, not an atomic fence.
- Drain replay stops at the captured bound at `context-authority.ts:604-615`. Finish accepts the caller’s equal expected/actual checksum and does not re-read the feed head at `context-authority.ts:638-652` and `mc-store/src/lib.rs:9477-9505`.

**Failure sequence**

1. `ctx_memory` receives MODULE from `authority.status`.
2. Another process begins drain before the tool’s subsequent `ctx_memory` request; feed bound N is captured and state becomes DRAINING.
3. The module facade resolves DRAINING to the identity and accepts the write, appending feed row N+1. The user receives a successful save result.
4. The drain mirrors only through N and computes both checksum arguments from that TS view.
5. Finish flips to TS without comparing the current module feed head to N.
6. The N+1 memory remains only in the now-inactive module pool; TS tools and transforms cannot see the acknowledged write.

Transforms do not otherwise observe a half-drained pool: drain replay copies out of the module without deleting module rows, so identity-keyed transform reads remain coherent. The unsafe case is a writer admitted during that window.

**Catching test**

Pause a real facade mutation after its MODULE status check. Begin drain and capture N, release the mutation so it appends N+1, then complete the drain. Assert either that the mutation is rejected retryably or that finish cannot flip until N+1 is mirrored and included in independently compared checksums.

**Required fix**

Keep DRAINING identity resolution for transform/read continuity, but reject facade mutations unless the domain state is exactly MODULE. Also fence drain finish against feed-head advancement (or recapture/replay until stable) so non-facade module writers cannot escape the handoff.

---

### G3. **MEDIUM — module restart makes legitimate OpenCode memory/note calls fail until the next transform**

**Evidence**

- `transform_session_roots` is process memory. `module_knows_transform_session` checks it first and returns false before consulting durable cache state at `crates/mc-module/src/lib.rs:2579-2594`.
- With an empty root map, a claimed OpenCode route enters `session.resolve` at `mc-module/src/lib.rs:7588-7600`.
- The resolver sends the bound value as an `instance_token` to thalamus at `crates/mc-module/src/session_resolver.rs:83-105`. A normal OpenCode facade route is bound with the real OpenCode session id, not the wrapper token namespace that this fallback resolves.
- The production adapters first observe MODULE, then surface the facade rejection as a tool error: memory at `packages/plugin/src/tools/ctx-memory/tools.ts:418-442`, notes at `packages/plugin/src/tools/ctx-note/tools.ts:284-315`.
- A subsequent transform restores the `(session, root)` observation before most transform work, ending the window. The existing tests manually populate the process map and do not recreate a handler around persisted `mc_cache_state`.

**Failure sequence**

1. Session S has durable cache state and previously authenticated root A.
2. Only the module process restarts; SQLite survives, but `transform_session_roots` is empty.
3. Before another transform, a legitimate OpenCode `ctx_memory` or `ctx_note` call arrives for S/A.
4. Durable cache state cannot arm the bypass because `root_observed` is false.
5. Thalamus cannot resolve the real session id as a wrapper instance token, so the module returns `session_unresolved`.
6. The TypeScript adapter returns a user-visible `Error: Rust module ctx_memory/ctx_note failed...`; calls recover after the next transform observes S/A.

This is fail-safe for provenance, not fail-broken security: the module rejects rather than binding the wrong root. It is nevertheless a real availability regression. Production `ctx_reduce` is unaffected because its module arm does not resolve facade scope; TS `ctx_search`/`ctx_expand` remain local, although direct module facade clients for those methods would also reject.

**Catching test**

Run a transform for S/A, persist cache state, destroy and recreate `McHandler` against the same store, then issue legitimate OpenCode memory and note calls before another transform. Require success without permitting S on root B. The test must exercise the real restart boundary, not manually seed `transform_session_roots`.

**Required fix**

Restore authenticated root lineage across module restart, or add a restart handshake that re-attests the OpenCode `(session, root)` pair. Durable cache state alone must not authenticate a different root.

---

### G4. **LOW pin — one optional module callback is still detached from a potentially class-backed client**

`packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:983-990` assigns `options.moduleClient.getCompartmentsAfter` into a new reader object. If an implementation is a class method that reads `this`, `mirrorModuleCompartments` invokes it with the reader as receiver, not the original client. The current production `SubcShadowTransport` does not implement this optional method, while the hook-created wrapper uses an arrow function, so this is latent rather than the R1 production blocker. The grep sweep found no other detached references among `call`, `authorityStatus`, `authorityPrepare`, `authoritySeed`, `authorityDrain`, `mirrorPull`, or `closeSession`; those invoke through their owning object or an arrow adapter.

**Catching test:** provide a class-backed `RustModeModuleClient` whose `getCompartmentsAfter` reads instance state, complete one Rust transform, and assert compartment mirroring succeeds.

**Required pin:** wrap it as `(sessionId, after) => options.moduleClient.getCompartmentsAfter!(sessionId, after)`.

---

### G5. **LOW pin — live-only transport pagination loops, but the resnapshot is not memory-bounded**

`ensureLiveMemoryResnapshot` advances `page.next_cursor` correctly and rejects a stalled page at `context-authority.ts:1197-1216`. A probe with 1,001 live rows and limit 1,000 made two live-only calls and installed all 1,001 rows. However, every page is appended to one `ChangefeedRow[]` at lines 1197 and 1210, and delete/refill happens only after the final page at lines 1218-1224. Peak plugin memory is therefore O(total live module rows), despite bounded wire frames.

**Catching test:** exercise a configured maximum or incremental staging table with many pages and assert the in-memory retained row count stays bounded while the final swap remains atomic.

**Required pin:** stage pages transactionally in a generation-keyed table, then atomically swap/mark complete; abandon or replace the staging generation on retry.

## R1-R7 closure audit

| Prior finding | Result | Round-3 conclusion |
|---|---|---|
| **R1 detached timer transport** | **Closed for production; G4 latent pin** | `createDreamTimerModuleClient` forwards `authorityStatus` and `call` through the instance at `packages/plugin/src/plugin/dream-timer-module-client.ts:21-35`. The class-backed timer regression passes. |
| **R2 live-upgrade tombstone loss** | **Normal path closed; end-to-end recovery not closed** | Live-only resnapshot establishes canonical provenance before destructive tombstones, and snapshot replacement plus `complete` is one local transaction. G1 wedges DRAINING recovery because it bypasses this entry point; G5 is a boundedness pin. |
| **R3 PREPARING half-flip** | **PREPARING closed; DRAINING write fence open as G2** | `authority_project_for_route` excludes PREPARING and includes MODULE/DRAINING. Seed, prepare, and status operations carry explicit `(context_store_uuid, project, domain)` and do not depend on route owner resolution. |
| **R4 missing migration 30** | **Closed** | The chain is exactly 1..32. Migration 30 runs before independent ledger migration 31; migration 32 is an idempotent normalization replay after 30. Fresh stores have no authority rows during these steps, and live stores already recording 30 apply 31/32 plus the fenced per-open repair. |
| **R5 facade authority lookup fail-open** | **Closed** | `resolve_facade_scope` distinguishes `Ok(None)` from `Err` and returns `authority_project_resolution_failed`; the injected one-shot regression passes. |
| **R6 session-global provenance** | **Security closure holds; availability regression G3** | Root membership is required, so S/A cannot authenticate S/B. Process restart loses the membership and safely rejects even S/A until another transform. |
| **R7 cancellation leak** | **Closed** | `DreamerRunGuard` removes the registered child on `Drop`; the aborted-task regression passes. |

## R2 crash/concurrency matrix

- **Crash after status becomes `resnapshotting`, before or during pulls:** safe. The next attempt treats `resnapshotting` as incomplete and repeats the live scan.
- **Crash after the final module page, before local replacement:** safe. No local live set was changed; the next attempt repeats.
- **Crash during local delete/refill/status update:** safe. `replaceLiveMemorySnapshot` and `status='complete'` share one immediate SQLite transaction at `context-authority.ts:1218-1224`, so rollback restores the old table and incomplete status.
- **Crash after that transaction commits:** safe. Destructive feed application is enabled only with the committed live set.
- **Tombstone arrives during the multi-page scan:** safe on the normal entry point. The live scan may include or omit the row, but the tombstone has a feed sequence after the old durable cursor and is applied after the snapshot. Feed history is not pruned in this repository.
- **First mirror after upgrade with no memory identities:** `pending_check` becomes `complete` without a pointless live-only call at `context-authority.ts:1182-1188`. A later first insert is learned through the ordinary feed.
- **More rows than `limit`:** cursor progression works (1,001/1,000 probe: two calls, 1,001 installed); G5 records the remaining memory-boundedness pin.
- **DRAINING recovery:** unsafe due to G1 because it uses the old direct applier.

## Migration ordering assessment

Migration 30 is restored at `crates/mc-store/src/lib.rs:1550-1623`, before migration 31's independent dream-task ledger and migration 32's idempotent route-normalization replay. Nothing in 31 requires a 30-created object. Migration 32 depends on the binding/authority/source tables established before 30, and executing 30 first only means 32 normally finds no additional rows. The exact-set fresh assertion is at `mc-store/src/lib.rs:12086-12105`; the schema-30 live-shape fixture asserts versions 1..32 and fenced note repair. Both targeted tests passed.

## Served-byte assessment

No production m0, m1, tail serializer, or cache-commit code changed in these four commits. The PREPARING selector intentionally keeps transforms on the complete route-keyed TS snapshot until MODULE; it prevents partial-seed bytes rather than introducing a defer-only mutation. Mirror resnapshot runs after the current Rust response has been applied, and timer/migration/RAII changes do not touch response bytes. The dedicated defer byte-purity regression passed. G2 can make a later TS-owned pass omit an acknowledged memory, but that is handoff durability loss, not same-pass uncoordinated defer-byte mutation.

## Verification performed

Passed:

- `bun install --frozen-lockfile`
- `bun test src/features/magic-context/context-authority.test.ts src/features/magic-context/dreamer/task-executor.test.ts` — 25 passed, 0 failed
- `bunx tsc --noEmit` in `packages/plugin`
- `cargo test -p mc-store fresh_and_migrated_stores_have_latest_schema -- --nocapture` — 1 passed
- `cargo test -p mc-store authority_route_binding_schema_30_live_upgrade_rekeys_through_caller_fence -- --nocapture` — 1 passed
- `cargo test -p mc-store authority_state_machine_persists_generations_and_drain_journal -- --nocapture` — 1 passed
- `cargo test -p mc-module authority_ -- --nocapture` — 18 passed
- `cargo test -p mc-module opencode_ -- --nocapture` — 15 passed
- `cargo test -p mc-module cancelled_dreamer_run_unregisters_its_child_session -- --nocapture` — 1 passed
- `cargo test -p mc-module defer_pass_historian_diagnostics_are_byte_pure_and_non_vacuous -- --nocapture` — 1 passed
- Read-only 1,001-row live-only pagination probe — 2 pages, 1,001 rows installed, status complete
- Read-only schema-57/DRAINING/tombstone probe — reproduced G1 with cursor and status unchanged

## Ship gate

Do not ship until G1 and G2 are fixed with the stated interleaving/restart tests. G3 should be fixed before claiming the OpenCode provenance remediation complete. G4 and G5 may remain explicit follow-up pins only if the release owner accepts their latent/bounded blast radius.
