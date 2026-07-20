# Authority remediation adversarial re-gate — 2026-07-20

## Verdict: **NO-SHIP**

Audited range: `2ffd9e02..9b3bdf82`, including the U9a merge (`a3b83d17`), the nine-finding vocabulary remediation (`7b91f6e3`), the migration-fixture floor change (`6814505f`), and the authority-client `this` fix (`9b3bdf82`). The prior verdict at `.alfonso/oracle-verdicts/authority-delta-regate-2026-07-20.md` supplied findings F1-F9.

The steady-state vocabulary remediation is materially better: F1, F2, F3, F8, and F9 are closed by production changes and regressions, and the normal F4/F5 paths now use identity keys. The combined branch is nevertheless not shippable. The default U9a classify path deterministically detaches another class method and fails before reaching Broca; F6 still deletes the canonical TypeScript memory on the real v57-to-v58 live-upgrade shape; a retained route binding makes PREPARING visible as the identity before seed completion; and the merge dropped migration version 30 from fresh stores. Two lower-severity authority fail-open/provenance gaps also remain.

## Release-stopping findings

### R1. **BLOCKER — the default U9a classify transport still detaches `SubcShadowTransport.call`**

**Evidence**

- Production creates the Rust client as a class instance at `packages/plugin/src/index.ts:153-157`:

  > `new SubcShadowTransport(...)`

- The dream-timer adapter binds `authorityStatus` correctly with `.call(...)`, but invokes a detached `call` method at `packages/plugin/src/index.ts:257-269`:

  > `rustModeModuleClient.call as unknown as (...) => Promise<unknown>`
  > `)(callArgs)`

- `SubcShadowTransport.call` reads instance state immediately (`this.activeSession`, `this.requestTimeoutMs`, and route methods) at `packages/plugin/src/hooks/magic-context/shadow-sender.ts:1640-1718`.
- The mainline fix only wrapped the authority methods used by `prepareRustMemoryAuthority` at `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:400-414`; it did not fix this U9a adapter.
- `task-executor.test.ts:289-357` uses an object-literal mock whose `call` does not depend on `this`, so the regression passes around the production failure.

**Concrete failure sequence**

1. Rust mode starts and `index.ts` constructs `SubcShadowTransport`.
2. The timer sees MODULE memories authority and selects the module classify path.
3. `dreamer.run_task` is sent through `classifyModuleClient.call`.
4. The wrapper invokes the extracted class method with `this === undefined`.
5. The call throws before route lookup or Broca; the classify task fails and never reaches `memory.set_classification`.

A direct probe of the exact detached method shape produced:

> `TypeError: undefined is not an object (evaluating 'this.activeSession')`

**Test that must be added**

Construct the timer registration with a real `SubcShadowTransport`-shaped class whose `call` reads instance state. Exercise one MODULE classify run and assert both `dreamer.run_task` and `memory.set_classification` reach the client. An object-literal mock is insufficient.

**Required fix**

Call through the object (`rustModeModuleClient.call(callArgs)`) or use an arrow wrapper that retains the instance. Audit this adapter pattern rather than casting methods to free functions.

---

### R2. **BLOCKER — F6 is not closed on the live v57-to-v58 mirror shape; a cleanup tombstone deletes the canonical TS row**

**Evidence**

- Migration 58 only creates an empty provenance table at `packages/plugin/src/features/magic-context/migrations.ts:2310-2323`. It does not backfill rows already consumed by the durable mirror cursor.
- Before v58, a legacy and canonical module insert can both update one context memory, but `rememberIdentity` permits only one module identity for that context row (`context-authority.ts:743-756`). With legacy-first feed order, only the legacy mapping remains.
- On a tombstone, `applyMemoryRow` deletes the mapped identity, then searches `mirror_live_memory_rows` for a surviving content match (`context-authority.ts:805-864`). On a live upgrade that table is empty because the canonical insert was consumed before v58.
- The new test, `canonical mapping survives legacy-first normalization tombstone ordering` at `context-authority.test.ts:357-422`, replays both insert rows after migration 58. It proves a fresh replay, not the deployed cursor/migration order.

**Concrete failure sequence**

1. A v57 context DB has context memory `9395`, with legacy module identity `(/repo, 100)`. The canonical module row `(git:identity, 200)` was already consumed, but could not claim the same context row because of the unique identity slot.
2. Upgrade to v58 creates an empty `mirror_live_memory_rows`; the existing mirror cursor remains past both inserts.
3. Module route normalization emits only the legacy tombstone; canonical module row 200 remains live.
4. Mirror replay removes mapping 100, sees no v58 live-row record for 200, and deletes context memory 9395.
5. The canonical module row survives, but the TS mirror has silently lost it. No later feed event is required to recreate it.

The one-shot live-shape probe returned:

> `{"memories":[],"identities":[],"live":[]}`

The requested two-tombstone variant does not make the mechanism sound: with no v58 live provenance, the first mapped tombstone deletes the context row before a later canonical tombstone can distinguish “canonical also deleted” from “canonical survives.” If both module rows truly disappeared, final deletion is correct; the current consumer cannot tell that case from the data-loss case above.

**Test that must be added**

Build a schema-57 fixture with an advanced memories mirror cursor, one context memory, and only the legacy `mirror_identity`; migrate to 58; deliver the normalization tombstone without replaying prior inserts; assert the context row survives while the canonical module row is live. Also cover two tombstones and both tombstone orders.

**Required fix**

The upgrade must establish authoritative live module identities before destructive tombstones are applied—for example, a bounded replay/resnapshot protocol or a mirror representation that retains both observed identities until deletion state is known. An empty local table cannot reconstruct pre-migration feed history.

---

### R3. **HIGH — retained bindings make PREPARING a half-flipped cache key and permit multiple coordinated transitions**

**Evidence**

- `authority_project_for_route` returns the identity for `PREPARING`, `MODULE`, and `DRAINING` at `crates/mc-store/src/lib.rs:3650-3675`.
- Route bindings are upserted at `mc-store/src/lib.rs:3599-3621` and are never deleted. `authority_finish_drain` changes only `mc_authority.state` to `TS` at `mc-store/src/lib.rs:9343-9420`.
- A later `authority_begin_prepare` deletes existing identity-owned seed rows and commits `PREPARING` before pages are sent (`mc-store/src/lib.rs:8932-8978`). Each `authority.seed` request then writes rows independently (`mc-module/src/lib.rs:5114-5162`).
- Transform resolves memories and notes before store reads at `mc-module/src/lib.rs:5611-5635`. Therefore an old binding plus PREPARING selects the just-cleared or partially seeded identity.
- The only transition regression, `authority_activation_moves_render_reads_once_through_the_m1_revision` at `mc-module/src/lib.rs:13486-13555`, activates a fresh binding directly. It does not cover drain-to-TS followed by re-prepare, nor a pass paused between seed pages.

**Concrete failure sequence**

1. A project was MODULE-owned, so `/repo -> git:A` is durable.
2. Drain finishes to TS; the binding remains and transforms fall back to `/repo` because TS is filtered out.
3. Re-prepare begins. In one transaction, identity seed rows are deleted and state becomes PREPARING.
4. A transform on another process/channel resolves the retained binding to `git:A`, reads an empty or partly seeded pool, and may commit a SOFT transition.
5. Additional seed pages change the identity revision; later transforms can emit more SOFT transitions before and after MODULE acknowledgement.

The cache transaction remains internally coherent, but it is coherently observing an incomplete authority snapshot. This violates the claimed single coordinated route-to-identity transition.

**Test that must be added**

Persist a binding, finish a drain to TS, establish a route-keyed cached pass, begin re-prepare, pause between seed pages, and run transforms from a second route. Before final acknowledgement they must either remain on the TS key or return a typed “authority preparing” result without changing cache state. After MODULE, assert exactly one SOFT transition followed by byte-stable SOFT+.

**Required fix**

Do not expose the identity to transforms during unverified PREPARING. Publish the key flip atomically with verified MODULE ownership/binding, or explicitly fence transforms until seed completion.

---

### R4. **HIGH — the merge dropped migration version 30 from the final migration chain**

**Evidence**

- The current list ends `29, 31, 32`: migration 29 begins at `crates/mc-store/src/lib.rs:1433`, migration 31 at `1550`, and migration 32 at `1567`.
- Commit history shows the incompatible branch shapes:
  - `2ffd9e02`: `... 28, 29, 30`
  - `a3b83d17`: `... 28, 29, 31, 32`
  - `7b91f6e3`: `... 28, 29, 30`
  - final `9b3bdf82`: `... 28, 29, 31, 32`
- The remediation fixture was changed to assert only `MAX(version) >= 30` at `mc-store/src/lib.rs:14550-14563`. A fresh store with versions 31/32 but no version 30 therefore passes.
- The remediation branch used version 30 for route normalization. The U9a branch reserved 31 for the dream-task ledger and moved normalization to 32. The merge retained the U9a numbering rather than preserving an immutable 30 record.

**Concrete failure sequence**

1. A live remediation store already records migration 30.
2. A fresh install of this combined branch records 1-29, 31, and 32, but never 30.
3. Both installations report schema head 32 while carrying different durable migration histories.
4. Any later repair, rollback, or contiguous-history validation cannot infer the same applied sequence from the version table. Reintroducing 30 later becomes an out-of-order migration decision rather than an immutable historical step.

Current open behavior happens to work because migration 32 and the per-open repair are idempotent, but the release history is not coherent. This is exactly the parallel-version hazard the migration gate was meant to prevent.

**Test that must be added**

For both a fresh DB and a DB pre-recorded through remediation migration 30, assert the exact applied version set and the exact schema objects after opening the combined binary. The fresh test must assert that version 30 exists, not merely that `MAX(version) >= 30`.

**Required fix**

Preserve the shipped/reserved migration-30 identity and append U9a migrations without rewriting history. Decide the canonical immutable numbering before release and test both live and fresh shapes.

## Additional authority gaps

### R5. **MEDIUM — facade authority lookup errors still fail open to the route vocabulary**

`resolve_facade_scope` maps both `Ok(None)` and `Err(_)` to `route_project_root` at `crates/mc-module/src/lib.rs:7571-7595`. The transform and authority state-sync paths now return `authority_project_resolution_failed` on the same lookup failure (`mc-module/src/lib.rs:5614-5634` and `6342-6349`). A one-shot store error can therefore make a facade read/evaluate against `/repo` while MODULE ownership is actually `git:A`, yielding a silent empty/wrong result instead of a retryable error.

**Test:** inject a one-shot failure in `authority_project_for_route`, then let downstream store access succeed. Assert a typed error and zero route-keyed reads/writes. Do not combine `Err` with the legitimate `Ok(None)` fallback.

### R6. **MEDIUM — F7 provenance is session-global, not bound to the route/project that established it**

`module_knows_transform_session` trusts any cache row for the supplied session ID before checking route provenance (`crates/mc-module/src/lib.rs:2560-2580`). `bind_facade_route_for_write` can then bind the caller's current route root to any requested MODULE project (`mc-module/src/lib.rs:7492-7521`). The tests at `mc-module/src/lib.rs:12422-12536` reject an unknown wrapper token but do not attempt a known session ID on a different root.

**Failure sequence:** establish cache state for session S on root A; open a facade route claiming S on root B with harness `opencode`; request a write to A's MODULE identity; the cache row satisfies provenance and the facade can rebind B to A. Same-session directory changes are legitimate, so the fix needs authenticated route lineage rather than a blanket same-root rule.

**Test:** use one known session across two roots, with only root A having carried a transform. Assert root B cannot claim/rebind A's authority without an authenticated transition that links the new route to that session/project.

### R7. **LOW pin — dreamer recursion registration has no cancellation guard**

`handle_dreamer_run_task` inserts the child ID at `crates/mc-module/src/lib.rs:7262-7264` and unregisters only on ordinary success/failure exits at `7314-7333`. Cancellation while awaiting a producer leaves a permanent registry entry. Use an RAII registration guard and add a canceled-run test. This is not the current ship blocker, but it prevents an unbounded stale bypass registry.

## F1-F9 closure audit

| Prior finding | Result | Production closure and regression that fails on the old sequence |
|---|---|---|
| **F1 migration-30 note trigger** | **Closed functionally; migration lineage reopened as R4** | `McStore::open` runs `repair_migration_30_authority_routes` after migrations (`mc-store/src/lib.rs:3537-3597`), and each note rekey goes through `with_note_conn_fenced`; `authority_route_binding_schema_29_note_upgrade_rekeys_through_caller_fence` (`14410-14582`) constructs the old failing v29 note. `cargo test -p mc-store authority_route_binding` passed. |
| **F2 wrong authority root/route reuse** | **Closed** | `rust-mode-transform.ts:369-414,791-813` threads the resolved directory; `shadow-sender.ts:1869-1898` caches by `sessionId + projectRoot`. Regressions: `rust-mode-transform.test.ts:173-257` and `shadow-sender.test.ts:2402-2435`. R1 is a separate U9a adapter detachment. |
| **F3 state-sync third vocabulary** | **Closed for the owner boundary** | `mc-module/src/lib.rs:6331-6406` resolves the owner before mapping; `authority_source_path` rejects mismatches. `authority_state_sync_enforces_resolved_owner_and_preserves_foreign_members` (`18083-18210`) covers absent, mismatched, mutation, and workspace-owner forms. |
| **F4 transform/historian route split** | **Closed in steady MODULE state; not closed across PREPARING and facade lookup errors** | Transform resolves memory/note owner keys before reads (`5611-5645`); historian and wrapup retain the resolved memory key (`2980-3336`). `authority_historian_publication_promotes_facts_under_the_identity_key` (`13557-13599`) and `authority_wrapup_publication_promotes_facts_under_the_identity_key` (`15527-15562`) cover publication. R3 and R5 remain. |
| **F5 identity note lifecycle** | **Closed on successful lookup; R5 remains** | `resolve_facade_scope` resolves the notes owner, transform carries `note_project_path`, and m1 claims by it (`m1_compose.rs:202-283`). `authority_note_lifecycle_resolves_identity_for_evaluate_render_and_ack` (`mc-module/src/lib.rs:12575-12646`) covers evaluate, render, delivery, and ACK. |
| **F6 mirror tombstone deletes canonical row** | **Not closed** | The fresh-replay regression at `context-authority.test.ts:357-422` passes, but migration 58 supplies no live-store backfill. R2 reproduces canonical deletion on the deployed upgrade shape. |
| **F7 caller-controlled OpenCode resolver bypass** | **Partially closed** | Unknown wrapper tokens are rejected and Claude resolution is preserved (`mc-module/src/lib.rs:12471-12536`). `has_cache_state(session_id)` remains route/project-agnostic; R6 gives the cross-route sequence. |
| **F8 decoded reduced-argument validation** | **Closed** | `unwrap-imitated-reduced-args.ts:26-98` rejects unknown fields, wrong types, non-finite numbers, and oversized arrays/strings. `unwrap-imitated-reduced-args.test.ts:55-86` exercises all five schemas, and tool/facade tests preserve explicit-field precedence. |
| **F9 correctness FIFO deadlock** | **Closed** | `shadow-sender.ts:1507-1725` removes aborted/timed-out waiters, uses queue-inclusive deadlines, and releases the active lane in `finally`. `keeps correctness FIFO moving when an aborted head waiter never enters the active section` (`shadow-sender.test.ts:2437-2503`) reproduces the old stranded-head sequence. |

## U9a interaction and new-mechanism audit

- **Classification vocabulary is correct inside the Rust handlers.** `dreamer.run_task` resolves the route through `authority_project_for_route` before selecting the authority (`mc-module/src/lib.rs:7180-7210`), and `memory.set_classification` resolves and compares the same owner (`7381-7407`). The default TypeScript adapter fails earlier at R1.
- **The registered classify child does not pollute transform provenance in the ordinary path.** Registered dreamer bypass occurs at `mc-module/src/lib.rs:5527-5538`, before `transform_route_channels.insert(channel)` at `5593-5596`; it performs no cache-state write. R7 covers cancellation lifetime, not provenance pollution.
- **Transform channel provenance is cleared on bind and unbind.** `mc-module/src/lib.rs:8277-8292` removes the channel from `transform_route_channels` for both operations. A reused channel does not retain ordinary transform provenance.
- **Separate memory/note keys are threaded correctly.** `ProducerContext` receives `project_path`, `note_project_path`, and filesystem-only `project_directory` separately (`mc-module/src/lib.rs:5642-5645`); hard and soft note claims use the note key (`transform.rs:1526-1539,1584-1592`; `m1_compose.rs:276-283`).
- **`authority_project_resolution_failed` does not crash the user's prompt loop.** The Rust-mode adapter catches module request failures, restores raw bytes or LKG, counts failures, and returns (`rust-mode-transform.ts:996-1015`). The TypeScript `messages-transform.ts` SQLITE_BUSY classifier is not reached because the Rust adapter consumes this error first. Hard-failing the module pass is appropriate; the missing piece is preserving a retryable code/diagnostic rather than conflating it with permanent failures and parking after three occurrences.

## Verification and probes

Passed:

- `bun install --frozen-lockfile`
- `bun test src/features/magic-context/context-authority.test.ts src/hooks/magic-context/rust-mode-transform.test.ts src/hooks/magic-context/shadow-sender.test.ts src/tools/unwrap-imitated-reduced-args.test.ts src/features/magic-context/dreamer/task-executor.test.ts` — **88 passed, 0 failed**
- `cargo test -p mc-store authority_route_binding` — **5 passed**
- `cargo test -p mc-module authority_` — **17 passed**
- `cargo test -p mc-module claimed_opencode_harness` — **1 passed**
- `cargo test -p mc-store fresh_and_migrated_stores_have_latest_schema` — **1 passed**
- `bunx tsc --noEmit` in `packages/plugin` — passed
- Detached transport probe — reproduced R1 with `TypeError: undefined is not an object (evaluating 'this.activeSession')`
- Live-upgrade mirror probe — reproduced R2 with the canonical context row deleted

Baseline-only failure:

- `bun run typecheck` passes the main source `tsc --noEmit` stage, then fails the scripts config on pre-existing TS5097 `.ts` import-extension errors in `scripts/bench-synapse-vs-local.ts` and `scripts/test-synapse-embed.ts`. The source-only typecheck above passed.

## Ship pins

This branch should not ship until at least R1-R4 are fixed with the stated regressions. R5 and R6 should be resolved before claiming the authority vocabulary/provenance law closed; R7 may be a follow-up only if the registry is changed to an RAII lifetime guard before release.
