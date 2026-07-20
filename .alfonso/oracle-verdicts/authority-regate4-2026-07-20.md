# Authority round-4 adversarial re-gate — 2026-07-20

## Verdict: **NO-SHIP**

Audited range: `777663a5..ef3ce2d9` (`0675bb40`, `ef3ce2d9`). The round-3 G1-G5 verdict was used as the closure checklist; previously cleared F1-F9 and R1-R7 surfaces were not re-litigated except where this delta touched them.

G1 and G4 are closed in production. The ordinary G2 status-check race is now fenced in the writer transaction, G3 survives an ordinary near-term module restart, and G5 is memory-bounded in a single resnapshot attempt. The delta is still not shippable: concurrent resnapshot attempts can overwrite a complete generation with an incomplete stale generation and mark it complete, recreating the canonical-memory deletion hazard that the resnapshot exists to prevent. Four lower-severity transition/lifecycle defects also remain: the drain retry has no termination guarantee while non-facade module writers remain active, lineage timestamps are never refreshed and are pruned from live sessions, one multi-transaction facade dismissal can be partially committed before returning `authority_draining`, and a raced classification rejection returns the wrong transition code.

## Release-stopping finding

### H1. **HIGH — concurrent resnapshot generations clobber one another and can publish an incomplete live set as complete**

**Evidence**

- `ensureLiveMemoryResnapshot` gives each attempt a string generation, but starting any attempt deletes **all** staging rows and writes only the unowned status `resnapshotting` at `packages/plugin/src/features/magic-context/context-authority.ts:1254-1263`.
- Each page is committed independently under its generation at `context-authority.ts:1278-1282`.
- Completion does not compare an active generation. It replaces the live table from the caller's generation, deletes **all** generations, and sets the global status to `complete` at `context-authority.ts:1290-1296`; `installStagedLiveMemorySnapshot` performs the global deletes at `context-authority.ts:819-830`.
- Migration 59 keys staging rows by generation but gives `mirror_resnapshot_state` no owner-generation/CAS column (`packages/plugin/src/features/magic-context/migrations.ts:2334-2348`; status was created in migration 58 at `2323-2329`).
- The in-process cadence coalesces by module object only (`context-authority.ts:1322-1342`). Drain recovery calls `ensureLiveMemoryResnapshot` directly, and separate serve/TUI processes have distinct module objects and database handles. The same race is therefore possible both cross-process and between a direct recovery drain and the ordinary cadence.

**Failure sequence**

1. Attempt A starts and stages page A1, then waits for page A2.
2. Attempt B starts. Its startup transaction globally deletes A1, then B stages its complete snapshot and atomically installs it.
3. B sets `mirror_resnapshot_state='complete'` and deletes staging.
4. Stale attempt A resumes. It never rechecks the active generation or status, stages only A2, replaces B's complete live set with A2, deletes staging, and again records `complete`.
5. A normal feed pull now trusts an incomplete provenance set. A tombstone for an omitted still-live canonical identity can delete the TypeScript context row, recreating R2/G5.

A read-only controlled Bun probe against the real function produced:

> `{"afterB":[{"module_project":"B-1"},{"module_project":"B-2"}],"final":[{"module_project":"A-2"}],"status":{"status":"complete"}}`

This also breaks the staging-to-live boundary under a concurrent normal pull: once B records `complete`, another consumer may apply destructive feed rows while stale A remains able to replace the live set afterward.

**Catching test**

Run two real `ensureLiveMemoryResnapshot` calls against one file-backed context database with barriers after A1 and after B completion. Require that only the currently owned generation may install or mark complete, and that a stale generation cannot delete another generation's staging rows. Repeat with one caller entering through `pullAndApplyMirrorPage` and one through `drainAuthority`.

**Required fix**

Persist the active generation in `mirror_resnapshot_state` and CAS every stage/finalize operation against it, or acquire a database-backed resnapshot lease for the whole paged attempt. Cleanup must target abandoned generations without allowing an older attempt to install after a newer one has completed.

---

## Additional findings

### H2. **MEDIUM — the 30-day lineage prune expires live roots because successful observations never refresh their timestamp**

**Evidence**

- `commit_transform` uses `INSERT OR IGNORE` for `(session_id, project_root, observed_at)` at `crates/mc-store/src/lib.rs:5743-5751`. Once the primary key exists, later accepted transforms on the same root do not update `observed_at`.
- Every store open deletes rows older than 30 days without consulting cache state, recent transforms, or session liveness at `mc-store/src/lib.rs:3818-3833`.
- `module_knows_transform_session` requires the durable exact-root row after process restart before it trusts existing cache state (`crates/mc-module/src/lib.rs:2579-2606`).
- The new store test deliberately proves that an old root is pruned while `mc_cache_state` survives (`mc-store/src/lib.rs:11381-11417`). The real restart regression covers only a fresh lineage row (`mc-module/src/lib.rs:12729-12826`).

**Failure sequence**

1. Session S first commits on root A more than 30 days ago and remains active; later transforms continue to succeed on A.
2. `INSERT OR IGNORE` leaves the original old timestamp unchanged.
3. The module restarts. Store open deletes S/A while retaining S's cache state.
4. A legitimate OpenCode memory or note facade request arrives before the next transform.
5. Durable root proof is absent, so the request falls through to session resolution and can fail exactly as round-3 G3 did. A later transform restores the row.

This is fail-safe, not a cross-root authorization bypass, but it is the same user-visible availability regression for long-lived sessions.

The lineage write itself is correctly severe: it is inside the cache CAS transaction. A lineage insert failure rolls back the cache commit and fails the transform before a module response is accepted; the TypeScript adapter restores raw/LKG bytes. Silently accepting a cache commit without its required durable proof would be worse.

**Catching test**

Commit S/A at an old timestamp, commit a changed S/A transform at a current timestamp, reopen, and require S/A to remain authorized while S/B remains rejected. Also cover a genuinely deleted session separately from an idle-but-live session.

**Required fix**

Use an upsert that refreshes `observed_at` on every accepted cache commit, and prune only roots whose owning session is demonstrably dead (or whose cache/session activity is also beyond the retention rule).

**Lifecycle note:** context.db migration 59 creates project-global `mirror_live_staging`, not a session-scoped table, so it is correctly absent from TypeScript `clearSession`; the structural clear-session test passes. `mc_transform_session_roots` is instead a module-store migration-33 table. The host's `clearSession` cannot reach it, and `SubcShadowTransport.closeSession` only closes routes (`packages/plugin/src/hooks/magic-context/shadow-sender.ts:1851-1868`). If module-store session rows are expected to obey the same deletion rule, a module session-delete protocol is still missing; this delta does not add one.

---

### H3. **MEDIUM — `AuthorityFeedHeadAdvanced` can livelock because the retry is unbounded and DRAINING still admits internal feed writers**

**Evidence**

- `drainAuthority` wraps the full attempt in `while (true)` and immediately `continue`s on every `authority_feed_head_advanced`, with no attempt bound, deadline, yield policy, or terminal status (`packages/plugin/src/features/magic-context/context-authority.ts:605-687`).
- The finish comparison is correctly in the ownership-flip transaction (`crates/mc-store/src/lib.rs:9819-9854`), so this is an availability issue rather than a lost-row issue.
- The MODULE-only facade trigger does **not** starve every writer. DRAINING deliberately remains identity-resolved for transforms at `mc-store/src/lib.rs:4031-4060`.
- An already prepared TypeScript process does not poll authority again: `prepareRustMemoryAuthority` returns from its process-local `memoryAuthorityReady` cache at `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:381-386`.
- That process can continue authority `state_sync`; `apply_state_sync` replaces regular authority memories without checking authority state at `mc-store/src/lib.rs:5987-5998`. Changed rows append feed entries through the global feed triggers.
- Historian publication can still promote facts into `mc_memories` at `mc-store/src/lib.rs:7258-7355`, and note evaluation plus delivery ACK/NACK still update `mc_notes` without `with_facade_mutation` at `crates/mc-module/src/lib.rs:8045-8118,8121-8182`.

Thus the fenced sources are user-facing memory/note/classification mutations. Still-live state-sync clients, historian publication, smart-note evaluation, note claim/delivery, and ACK/NACK remain possible feed producers during DRAINING. A steady producer can win between every replay and finish, causing endless recapture. The existing TypeScript regression advances the head once and then forces success (`context-authority.test.ts:760-834`); it cannot catch starvation.

**Catching test**

Drive a real drain while an already-ready second client repeatedly commits changed authority state or historian facts. Require either a bounded retryable drain result or a transition fence that stops new domain feed writes and lets the handoff complete.

**Required fix**

Either fence all DRAINING domain writers (while preserving read/transform continuity), or give the coordinator a bounded/deadline-aware result that surfaces contention instead of looping forever. A single late append should continue to recapture safely.

---

### H4. **LOW pin — `with_facade_mutation` is a trigger scope, not one transaction; resolved note dismissal can partially commit before returning `authority_draining`**

The ordinary TOCTOU is closed. `with_facade_mutation` keeps the UDF scope active while each store primitive starts a `BEGIN IMMEDIATE` fenced transaction (`crates/mc-store/src/lib.rs:3857-3885`; `../commons/crates/cortexkit-store/src/lib.rs:185-232`). The memory/note BEFORE triggers query authority state inside that same writer transaction (`mc-store/src/lib.rs:1731-1815`). If the mutation transaction wins, drain begin waits and captures its feed row; if drain begin wins, the trigger aborts before the row and AFTER feed trigger. Reads never acquire this scope.

However, `with_facade_mutation` does not wrap its closure in one transaction. `ctx_note dismiss` routes through it at `crates/mc-module/src/lib.rs:8434-8445`, while `McStore::dismiss_note` first commits the dismissed status and then, when a resolution is supplied, starts a second transaction to append the resolution to content (`crates/mc-store/src/lib.rs:8574-8614`).

**Failure sequence**

1. The first transaction dismisses the note and appends a note feed update.
2. Drain begin commits DRAINING in the gap and captures that feed row.
3. The second content-update transaction hits the DRAINING trigger and returns `authority_draining`.
4. The facade reports rejection, but the note is already dismissed and one feed row is durable.

**Catching test:** add a barrier between the two dismissal writes, begin drain, and require either the complete dismissal to precede the captured head or a rejection with no row/feed change. Prefer making dismissal plus resolution one store transaction.

---

### H5. **LOW pin — a raced classification rejection is atomic but loses the new typed `authority_draining` code**

`memory.set_classification` is wrapped and its store transaction explicitly checks exact MODULE state before updating rows (`crates/mc-module/src/lib.rs:7517-7525`; `crates/mc-store/src/lib.rs:5245-5266`). It therefore leaves no partial classification/feed rows when drain wins.

In the exact race, however, the transaction returns `AuthorityStateMismatch { found: "DRAINING" }` before any row trigger fires. The handler maps that variant to `authority_state_mismatch` before its generic `authority_draining` mapping (`mc-module/src/lib.rs:7530-7544`). The host classifier recognizes only `authority_draining` for the transition-specific retry path (`packages/plugin/src/features/magic-context/dreamer/classify.ts:381-406`). The existing test starts from an already-DRAINING state, so it exits through the preliminary typed branch and misses this interleaving.

**Catching test:** pause after the preliminary MODULE lookup, begin drain, release classification, and assert `authority_draining` plus unchanged rows/feed.

---

## Facade mutation inventory

| Public/module arm | Mutation behavior in this delta | Result |
|---|---|---|
| `ctx_memory write` | `with_facade_mutation` -> one fenced insert transaction | Fenced; no partial feed on rejection |
| `ctx_memory update` | Read prechecks, then `with_facade_mutation` -> one fenced update+mutation-log transaction | Fenced; reads are outside the scope, final write rechecks in-trigger |
| `ctx_memory archive` | Read prechecks, then one fenced batch transaction | Fenced |
| `ctx_memory merge` | Read prechecks, then one fenced merge transaction | Fenced |
| `memory.set_classification` | Wrapped; exact MODULE+generation read in the update transaction | Atomic; H5 typed-code pin |
| `ctx_note write` (session/smart) | Wrapped; one fenced note insert transaction | Fenced |
| `ctx_note update` | Read precheck, then one fenced CAS transaction | Fenced |
| `ctx_note dismiss` | Wrapped, but resolution form can use two write transactions | H4 partial-rejection pin |
| `ctx_memory get`, `ctx_note read`, `ctx_search`, `ctx_expand` | Read-only | Correctly do **not** acquire the mutation scope |
| `ctx_reduce` facade acknowledgement | Returns before identity/storage work | Correctly not an authority mutation |
| `agent_drops.append` and transform drop consumption | Mutate session command/cache tables, not `mc_memories`/`mc_notes` or authority feed | Outside the domain mutation fence by design |
| `note.evaluate`, note claim/delivery, `transform.ack/nack` | Internal module lifecycle mutations, not wrapped | Safe from silent loss via finish-head fence, but remain H3 writer sources |
| authority `state_sync`, historian/wrapup publication | Internal module writers, not wrapped | Safe from silent loss via finish-head fence, but remain H3 writer sources |

## G1-G5 closure matrix

| Round-3 finding | Round-4 result | Evidence / catching-test assessment |
|---|---|---|
| **G1 drain recovery bypasses resnapshot** | **Closed in production; LOW test-shape pin** | Every memory-feed consumer now crosses the barrier: reconciliation `context-authority.ts:288-295`, drain recovery `624-638`, and normal pull `1300-1319`. The schema-57 DRAINING test passes for both incomplete statuses (`context-authority.test.ts:547-683`), but it calls `drainAuthority` directly rather than the requested real `prepareRustMemoryAuthority` restart path. |
| **G2 facade write after captured bound** | **Core race closed; H3-H5 remain** | Immediate writer transactions recheck through triggers; finish compares feed head in the flip transaction and the late-append regression passes. All public mutation arms are inventoried above. Internal writers are deliberately handled by recapture, not the facade fence. |
| **G3 restart loses root lineage** | **Normal restart closed; H2 reopens aged/live sessions** | The real handler restart regression accepts A and rejects B. Durable lineage is committed with cache CAS, but retention is based on a never-refreshed first timestamp. |
| **G4 detached optional compartment callback** | **Closed** | The adapter is an arrow call-through at `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:983-995`; the class-backed receiver regression passes. |
| **G5 unbounded live resnapshot memory** | **Sequential boundedness closed; concurrent correctness open as H1** | Pages are staged transactionally and the single-attempt three-page regression passes. Generation ownership is not enforced, so the staging table does not make concurrent attempts safe. |

## Byte-safety assessment

- The transform lineage row is written only inside `commit_transform` when a commit is required (`crates/mc-module/src/transform.rs:1759-1787`). A defer/no-change pass does not add a lineage-only write or alter rendered bytes.
- If the lineage insert fails, SQLite rolls back the entire cache transaction. The module rejects before returning accepted output; the TypeScript Rust adapter restores raw input or a validated LKG. Failing closed at this point is appropriate.
- For single-transaction facade memory/note mutations, the authority trigger is BEFORE the domain row change and the feed trigger is AFTER it in the same immediate transaction. `authority_draining` therefore leaves neither a partial domain row nor a feed row. The store regression checks the rejected insert leaves the feed head unchanged (`mc-store/src/lib.rs:15649-15673`).
- H4 is the exception: the facade closure spans two independently committed note transactions.
- H1 changes future mirror provenance, not the bytes of the in-flight transform, but can later delete canonical TypeScript state and therefore remains a release-stopping durability defect.

## Migration and cleanup assessment

- Module-store migration history is contiguous through version 33; migration 33 creates `mc_transform_session_roots` plus the facade authority triggers. The exact fresh/migrated schema test passes.
- Context.db migration 59 adds `mirror_live_staging` and keeps `LATEST_SUPPORTED_VERSION` aligned. Fresh and v58-upgrade regressions pass.
- `mirror_live_staging` has no `session_id` and is project/global protocol state, so TypeScript `clearSession` should not delete it. The structural test that seeds and clears every context.db table with a `session_id` passes.
- The module lineage table does have `session_id`, but there is no module-side durable session deletion in this delta; route close is not store cleanup. Treat that as an explicit lifecycle pin if module-store session retention is intentional.

## Verification performed

Passed:

- `bun install --frozen-lockfile`
- `bun test src/features/magic-context/context-authority.test.ts src/features/magic-context/migrations-v59.test.ts src/hooks/magic-context/rust-mode-transform.test.ts src/tools/ctx-memory/tools.test.ts src/tools/ctx-note/tools.test.ts` — 113 passed, 0 failed
- `bun test src/features/magic-context/storage-db.test.ts -t "every session-scoped table"` — 1 passed, 0 failed
- `bunx tsc --noEmit` in `packages/plugin`
- `cargo test -p mc-store authority_ -- --nocapture` — 15 passed
- `cargo test -p mc-store transform_session_root_lineage_is_cache_committed_and_pruned_on_reopen -- --nocapture` — 1 passed
- `cargo test -p mc-store fresh_and_migrated_stores_have_latest_schema -- --nocapture` — 1 passed
- `cargo test -p mc-module authority_ -- --nocapture` — 19 passed
- `cargo test -p mc-module opencode_transform_root_lineage_survives_a_real_handler_restart -- --nocapture` — 1 passed
- `cargo test -p mc-module defer_pass_historian_diagnostics_are_byte_pure_and_non_vacuous -- --nocapture` — 1 passed
- Controlled concurrent-generation Bun probe — reproduced H1: B's complete two-row live set was replaced by stale A's one-row suffix while status remained `complete`

## Ship gate

Do not ship until H1 has database-enforced generation ownership and the two-attempt interleaving regression. H2 and H3 are real availability defects rather than mere test-shape pins and should be resolved or explicitly release-owned with bounded behavior. H4 and H5 are narrow transition pins, but their catching tests should accompany the straightforward atomicity/error-code fixes.
