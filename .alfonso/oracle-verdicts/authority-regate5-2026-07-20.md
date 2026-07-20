# Authority round-5 adversarial re-gate — 2026-07-20

## Verdict: **SHIP-WITH-PINS**

Audited range: `ef3ce2d9..e0a0248a` (`e0a0248a`, plus the round-4 verdict-only commit `03418f9b`). This review treated previously cleared surfaces as settled and attacked only the round-4 fixes and mechanisms added by this delta.

H1-H5 are closed in production. The generation-owned resnapshot now rejects stale stage/install work; live lineage observations refresh atomically; a drain call is bounded; resolved dismissal is one transaction; and the exact classification race maps to `authority_draining`. The new real restart-path test also closes the prior G1 test-shape gap.

No sequence found in this delta causes reachable data loss, unsafe ownership publication, a permanent correctness wedge, or changed bytes on a successful defer pass. Four tails should remain explicit pins: module session deletion still does not delete `mc_cache_state`, so the new dead-session prune is unreachable in normal lifecycle and lineage rows grow linearly; drain contention has no actual scheduled retry beyond a future natural transform; H1 claim acquisition has no churn bound/backoff and relies on UUID uniqueness; and H4 lacks the requested transition-race regression even though the production gap is structurally gone.

## Closure matrix

| Prior finding | Round-5 result | Catching-test assessment |
|---|---|---|
| **H1 generation ownership** | **Closed** | File-backed concurrent stale-generation and pull-vs-drain tests exercise the old failure shape. Migration/restart coverage exercises takeover from an ownerless v59 `resnapshotting` row. Repeated start-CAS churn and forced generation collision are not tested; both are pin-grade tails. |
| **H2 lineage refresh/prune** | **Live-session bug closed; cleanup tail pinned** | The store test proves timestamp refresh, idle-live retention, exact-root rejection, and pruning after a synthetic cache-row delete. It does not prove normal dead-session pruning because production has no cache-row delete protocol. |
| **H3 unbounded drain** | **Closed per invocation; retry scheduling pinned** | The direct coordinator test proves six bounded finish attempts (initial attempt plus five recaptures), a retryable result, durable `DRAINING`, and later success. It does not prove a scheduled adapter retry because none exists. |
| **H4 partial dismissal** | **Closed in production; LOW test-shape pin** | Dismissal now performs one fenced SQL update. Normal CRUD proves one version increment and one feed row; the already-DRAINING facade test proves rejection. There is no barriered begin-drain race test. |
| **H5 raced classification mapping** | **Closed** | The exact preliminary-check/apply interleaving is forced and asserts `authority_draining`, unchanged classification, unchanged feed head, and durable `DRAINING`. |
| **G1 real restart path** | **Closed** | `prepareRustMemoryAuthority` is exercised from a schema-57 DRAINING restart through live resnapshot, tombstone replay, finish, and readiness. |

## H1 — generation-CAS resnapshot ownership

### Production closure

Migration 60 adds `mirror_resnapshot_state.generation` and keeps the context schema fence at 60 (`packages/plugin/src/features/magic-context/migrations.ts:2352-2356`; `storage-db.ts:49`). The protocol now has database-visible ownership rather than a process-local generation convention:

- `memoryResnapshotState` reads `(status, generation)`, and `casMemoryResnapshotState` compares both fields using `generation IS ?` (`context-authority.ts:795-821`).
- A caller must CAS its freshly minted generation from the exact state it observed before deleting abandoned staging (`context-authority.ts:1326-1380`). A failed start CAS rereads the entire state and retries from scratch.
- Every page stages only while the status is `resnapshotting` and the generation still matches. A stale caller returns without installing (`context-authority.ts:832-870,1394-1402`).
- Final replacement, state transition to `complete`, and generation-local staging cleanup occur in one immediate transaction and all recheck the owner generation (`context-authority.ts:872-907,1410-1415`). No stale generation can replace a newer complete live set.
- Cursor-zero replay also CASes the exact observed resnapshot state before declaring the barrier complete (`context-authority.ts:1263-1285`).

The old release-stopping sequence is caught by `a stale paged resnapshot cannot replace a newer completed generation` (`context-authority.test.ts:771`) and the requested cross-entrypoint form by `pull and drain resnapshots honor the same file-backed generation owner` (`:876`). Both use separate handles to one file-backed database.

### Repeated conflict and crash-owner attack

A crashed owner does not wedge the state. A new caller observes its `resnapshotting` generation and CASes directly from that prior value to a new generation. The schema-57 restart test begins from migrated `resnapshotting/generation=NULL` plus abandoned staging, takes ownership, removes the abandoned generation, resnapshots, replays the tombstone, and completes through the real preparation path (`rust-mode-transform.test.ts:734`). Migration 60 separately proves that upgrade adds the nullable owner column without destroying staged rows.

The acquisition loop itself is unbounded and has no backoff (`context-authority.ts:1326-1380`). With a finite set of contenders, every successful steal invalidates the prior owner and the final claimant can complete; stale owners fail closed at their next stage/install check. An unbounded stream of cross-process claimants arriving faster than a scan can stage/finalize can nevertheless keep superseding owners and prevent completion. This is an availability tail, not an unsafe publication sequence: tombstones remain blocked while status is incomplete, and a stale caller cannot install.

Generation identity is `${Date.now().toString(36)}:${crypto.randomUUID()}` (`context-authority.ts:1357`). Correctness therefore assumes UUID non-collision rather than enforcing a database sequence. A same-generation collision would create false shared ownership, but random UUID v4 supplies approximately 122 random bits; even one billion attempts has birthday-collision probability on the order of `10^-19`. Treat deterministic collision injection and bounded/backoff acquisition as hardening pins, not a ship gate.

A cursor-zero replay can complete the state while an owned live scan has staged rows. The scan then returns on its next owner check, but generation-local staged rows are not cleaned by the replay path. At most the interrupted generation remains because no further resnapshot starts from `complete`; this is a bounded storage tail, not a provenance failure.

## H2 — lineage refresh and dead-session cleanup

### Live-session closure

`commit_transform` now uses an UPSERT that refreshes `observed_at` for the exact `(session_id, project_root)` inside the same successful cache CAS transaction (`crates/mc-store/src/lib.rs:5741-5761`). Rejected CAS passes still cannot refresh or create lineage. The reopen test commits an ancient observation, commits the same root at current time, retains an ancient idle-but-live session, rejects a different root, and prunes a synthetic deleted session (`:11395-11456`). This closes the aged-live availability regression.

### Pin P1 — the dead-session predicate is not reachable in normal deletion

The revised prune is conservative and safe: it deletes only rows older than 30 days for which no `mc_cache_state.session_id` exists (`mc-store/src/lib.rs:3818-3841`). However, nothing in production deletes `mc_cache_state`:

- `commit_transform`, authority state sync, and shadow reset create or UPSERT cache rows; recomp reset updates them.
- Repository-wide inspection found the only literal `DELETE FROM mc_cache_state` in the synthetic lineage test (`mc-store/src/lib.rs:11433`).
- OpenCode `session.deleted` invokes TypeScript `clearSession` on `context.db` (`packages/plugin/src/hooks/magic-context/event-handler.ts:727-751`). That function cannot reach the separate module store.
- Rust-mode cleanup calls transport close; `SubcShadowTransport.closeSession` removes routes, while module `on_route_gone`/`unbind_route` remove process-local bindings and coordinators. Neither deletes durable store rows.

Consequently, normal deleted sessions retain both their cache row and lineage forever, so `NOT EXISTS (mc_cache_state)` is false and the prune never fires. The test proves the SQL predicate after a hand-written deletion, not the product lifecycle.

Growth is bounded per key but unbounded over product lifetime: one stale row per distinct `(session_id, project_root)`, usually one per deleted transformed session, not one per pass. Thus 10 deleted sessions/day is about 3,650 rows/year; multiple roots per session multiply that rate. A disposable SQLite probe using the real table plus primary-key and `observed_at` indexes, 26-byte session IDs, and representative 63-byte roots measured about 213 bytes per row after `VACUUM` (20.3 MiB per 100,000 rows). Actual size varies with identifiers and page fill; at 10/day that representative incremental lineage cost is under 1 MiB/year, while 1,000/day is roughly 74 MiB/year. The already-persistent `mc_cache_state` payload is additional and likely dominates.

This is a real lifecycle/storage leak, but not a delta-local data-loss, cross-root authorization, or transition wedge. The exact-root primary key and cache-provenance check remain intact. Pin a module-side session-delete request that atomically deletes cache-owned session state and lineage, wire it from `session.deleted`, and replace the synthetic delete in the test with that production path.

## H3 — bounded drain contention and retry propagation

`drainAuthority` now returns the discriminated `AuthorityDrainResult` union (`context-authority.ts:34-42`). On repeated `authority_feed_head_advanced`, it allows five recaptures after the initial attempt and then returns:

```text
{ code: "authority_drain_contended", retryable: true, state: "DRAINING", ... }
```

The bound is therefore six begin/finish cycles, as the test explicitly asserts (`context-authority.test.ts:1094`). The durable state remains DRAINING, the marker remains installed, and a later invocation can converge. H3's per-call livelock is closed.

### Pin P2 — “next scheduled transform” is not scheduled

There is exactly one production caller of the TypeScript coordinator: `prepareRustMemoryAuthority` resumes domains already observed in DRAINING (`rust-mode-transform.ts:431-456`). No flip-back command, command-handler path, timer, queue, or separate recovery worker calls `drainAuthority` elsewhere in `packages/plugin`.

The sole caller does typed structural handling with `if ("code" in drained)`, but discards `retryable`, `attempts`, and the returned authority state, then throws `MemoryAuthorityUnavailableError` whose text says “the next scheduled transform will resume the drain” (`rust-mode-transform.ts:451-454`). The outer transform catches that as a generic failure, serves validated LKG or raw input, and increments the ordinary Rust failure counter. After three failures it parks and probes only on every fifth *live* transform pass (`rust-mode-transform.ts:707-722,1006-1025`). It does not schedule a pass.

Thus recovery is guaranteed only if a later natural transform occurs (and, after parking, enough live passes occur). If the project becomes idle, DRAINING persists safely but indefinitely. This is an observability/retry pin rather than a ship blocker: each active call is bounded, writes remain fenced, and no unsafe bytes or partial ownership result is accepted. Either install a real delayed retry, or change the message/API to say “a later transform can resume” and surface `authority_drain_contended` in typed diagnostics. Add an adapter-level contention regression; the current test stops at the direct helper.

## H4/H5 adequacy

### H4 — production atomicity closed, exact race test absent

`dismiss_note` still performs a preliminary read, but the write path reloads the note and executes status, content, `dismissal_resolution`, timestamp, and one `status_version` increment in a single `with_note_conn_fenced` immediate transaction (`crates/mc-store/src/lib.rs:8584-8636`). The authority trigger evaluates inside that writer transaction while the facade scope is active. Therefore:

- if dismissal obtains the writer first, its complete row and single feed update precede drain begin and are captured;
- if drain begin obtains the writer first, the one dismissal update aborts before any row/feed change.

There is no longer a between-write gap. `notes_crud_pagination_dismiss_resolution_and_search` now checks one version increment and exactly one feed row (`mc-store/src/lib.rs:13988`), and the facade inventory test checks already-DRAINING mutations are rejected (`mc-module/src/lib.rs:13908`). Neither forces drain begin after the facade preliminary check and before the fenced write, so the prior verdict's exact race specification remains a LOW test-shape pin. A one-shot test hook before `with_note_conn_fenced` would cover both possible lock winners.

### H5 — exact race and mapping are covered

The handler now maps `AuthorityStateMismatch { found: "DRAINING" }` to the transition-specific error before the generic mismatch branch (`crates/mc-module/src/lib.rs:7550`). `raced_classification_drain_returns_the_transition_specific_code` installs a hook after the preliminary MODULE lookup, begins drain, resumes the classification transaction, and asserts:

- response code `authority_draining`;
- unchanged importance;
- unchanged memory feed head; and
- durable state `DRAINING`.

This is the exact catching test requested in round 4 (`mc-module/src/lib.rs:13827`). H5 is fully closed.

## Serve-path byte safety on defer passes

No successful defer/no-change byte path is changed by this delta:

- The lineage UPSERT is still reached only inside a successful `commit_transform`; refreshing `observed_at` does not itself force a cache commit or alter module output.
- H4 and H5 affect facade mutation/error paths, not transform rendering.
- Migration 60 and resnapshot ownership affect mirror/recovery state only.
- The H3 contended path fails before accepting module transform output. The adapter restores raw input or a validated LKG through its existing generic failure path; it does not partially apply module bytes.
- `defer_pass_historian_diagnostics_are_byte_pure_and_non_vacuous` continues to pass (`mc-module/src/lib.rs:18072`).

## Migration assessment

- Module-store migration history remains contiguous through v33; no schema change was added there.
- Context migration 60 adds only the nullable owner-generation column. `LATEST_SUPPORTED_VERSION` is aligned at 60.
- Fresh v60 and v59-upgrade tests pass. Upgrade intentionally preserves abandoned staging until the next claimant wins ownership and removes non-owned generations.
- Older migration fixtures were updated to remove version 60 when reconstructing historical schemas; no new session-scoped context table was introduced.

## Verification performed

Passed:

- `bun install --frozen-lockfile`
- `bun test src/features/magic-context/context-authority.test.ts src/features/magic-context/migrations-v60.test.ts src/hooks/magic-context/rust-mode-transform.test.ts` in `packages/plugin` — 40 passed, 0 failed
- `bunx tsc --noEmit` in `packages/plugin`
- `cargo test -p mc-store transform_session_root_lineage_is_cache_committed_and_pruned_on_reopen -- --nocapture` — 1 passed
- `cargo test -p mc-store notes_crud_pagination_dismiss_resolution_and_search -- --nocapture` — 1 passed
- `cargo test -p mc-module raced_classification_drain_returns_the_transition_specific_code -- --nocapture` — 1 passed
- `cargo test -p mc-module draining_authority_rejects_every_facade_mutation_but_keeps_reads_resolved -- --nocapture` — 1 passed
- `cargo test -p mc-module opencode_transform_root_lineage_survives_a_real_handler_restart -- --nocapture` — 1 passed
- `cargo test -p mc-module defer_pass_historian_diagnostics_are_byte_pure_and_non_vacuous -- --nocapture` — 1 passed
- Disposable SQLite lineage-size probe — 100,000 representative rows occupied 20.3 MiB after `VACUUM`; probe database removed
- Scoped diagnostics inspection — 0 errors and 0 warnings (TypeScript compiler is the authoritative TS gate above)

## Ship gate

**Ship this delta.** H1's data-loss sequence is database-fenced and covered across the two real entrypoints; H2 no longer expires live roots; H3 cannot monopolize one coordinator call forever; and H4/H5 are atomically and correctly typed in production.

Release-own these pins:

1. Add a real module-store session deletion protocol and lifecycle test so aged dead lineage can actually prune.
2. Either schedule drain contention retries or accurately expose that retry depends on later natural transform traffic.
3. Add bounded/backoff coverage for repeated resnapshot claim churn; retain the UUID collision assumption explicitly.
4. Add the exact H4 begin-drain interleaving regression even though the single-transaction rewrite removes the production gap.
