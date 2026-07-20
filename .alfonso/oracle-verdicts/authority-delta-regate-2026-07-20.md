# Authority delta adversarial re-gate — 2026-07-20

## Verdict: **NO-SHIP**

Audited range: `64a5f9fc..2ffd9e02` (`3f7447e0`, `a8c92e51`, `d044bf0b`, `a3271e0e`, `0996eb96`, `c3356f9c`, `2ffd9e02`). Earlier U10/U11-reviewed code was treated only as an interaction surface.

The delta does not change production m0/m1/tail rendering code, but it does not establish the claimed authority vocabulary law. There are several independent NO-SHIP failures: migration 30 can prevent the module store from opening, per-session authority routes can bind and normalize the wrong filesystem root, state sync can write route or arbitrary third-vocabulary keys, transform-owned historian/note paths still use the route vocabulary, a cleanup tombstone can delete the canonical TS mirror row, the OpenCode resolver bypass trusts a client-supplied string, and the new correctness FIFO can strand timed-out requests.

## Findings

### 1. **BLOCKER — migration 30 cannot upgrade a v29 store containing a route-keyed MODULE note**

**Evidence**

- `crates/mc-store/src/lib.rs:1176-1185` installs `mc_notes_ownership_update`, which aborts a project-key change unless `mc_note_caller_project()` equals the old or new project:

  > `WHEN (... OR NEW.project_path IS NOT OLD.project_path ...)`
  > `AND NOT (mc_note_caller_project() IS OLD.project_path`
  > `OR mc_note_caller_project() IS NEW.project_path)`
  > `SELECT RAISE(ABORT, 'note ownership update is outside the old or new project');`

- `crates/mc-store/src/lib.rs:3510-3528` registers that UDF before migrations, but its default outside `with_note_conn_fenced` is the empty string:

  > `.clone().unwrap_or_default()`
  > `inner.migrate(NS, MIGRATIONS)?;`

- Migration 30 runs a direct project-key update at `crates/mc-store/src/lib.rs:1624-1644`:

  > `UPDATE mc_notes`
  > `SET project_path = (SELECT binding.project ...)`
  > `... AND authority.state = 'MODULE'`

`inner.migrate` does not establish the old/new project in `note_caller_project`. Therefore a v29 database with binding `/repo -> git:A`, notes authority `MODULE`, and an `mc_notes.project_path='/repo'` row fires the v26 ownership trigger with caller `''`, and `McStore::open` fails instead of reaching schema 30. The existing upgrade test at `crates/mc-store/src/lib.rs:14158-14233` creates only memory rows, so it cannot catch this.

**Failure sequence**

1. A v29 store has a route-keyed note that migration 30 is intended to repair.
2. The new module opens the store and registers `mc_note_caller_project`, currently unset.
3. Migration 30 attempts `/repo -> git:A`.
4. `mc_notes_ownership_update` aborts the migration.
5. Store open fails; all Rust-mode transforms and facade calls are unavailable.

**Test that would have caught it**

Construct a schema-29 fixture containing a MODULE notes authority, route binding, and route-keyed note; open it through `McStore::open`; assert schema 30, identity rekey, and an update feed row.

---

### 2. **HIGH — the route-root bind fix is not threaded through authority preparation and route caching**

**Evidence**

- Rust transform resolves the live session directory, but `prepareRustMemoryAuthority` receives only the identity. At `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:402-406` it calls:

  > `module.authorityStatus({ context_store_uuid: contextStoreUuid, project: projectPath, domain })`

  There is no `projectRoot`.

- The transport is initialized once with the hook launch directory at `packages/plugin/src/hooks/magic-context/hook.ts:627-628`:

  > `const transport = new SubcShadowTransport(...);`
  > `transport.setAuthorityBindRoot(deps.directory);`

- `packages/plugin/src/hooks/magic-context/shadow-sender.ts:1676-1686` falls back to that fixed root:

  > `args.projectRoot ?? this.bindRootForAuthority()`

- Even if callers start passing the correct root, `packages/plugin/src/hooks/magic-context/shadow-sender.ts:1767-1781` caches authority routes only by `sessionId` (authority calls use the MC project identity as that id):

  > `const existing = this.routes.get(sessionId);`
  > `if (existing) return { client, route: existing };`

  Two roots for one identity (for example worktrees) therefore reuse the first route and never bind the second root.

- On a MODULE status, `crates/mc-module/src/lib.rs:4935-4945` immediately persists and normalizes the channel's bound root.

**Failure sequence**

1. The plugin starts in `/repo-A`; a live session later resolves `/repo-B` (the code explicitly supports per-session directory changes).
2. Authority preparation for `git:B` omits `/repo-B`, so the authority route opens on `/repo-A`.
3. The module persists `/repo-A -> git:B` and runs inline normalization there.
4. `/repo-B` remains unbound, while path-keyed rows belonging to repo A can be rekeyed to B or deleted as apparent B twins.
5. State sync and transforms on the actual `/repo-B` route use a different vocabulary.

The normalizer's `(category, normalized_hash)` twin test cannot protect against this wrong binding: two genuinely different projects with the same content become indistinguishable after repo B's authority is erroneously attached to repo A's route root.

**Test that would have caught it**

Use one transport with launch root A and a session directory B; assert every status/prepare/seed route binds B. Then open two roots for one `git:<sha>` identity and assert both route bindings exist rather than reusing the first route handle.

---

### 3. **HIGH — authority state sync can write the route key or an arbitrary third vocabulary**

**Evidence**

`authority_source_path` at `crates/mc-module/src/lib.rs:8939-8962` trusts non-workspace wire input whenever an authority binding exists:

> `let Some(source_path) = source_path else { return Ok(root_path.to_string()); };`
> `return Ok(if authority_project.is_some() { source_path.to_string() } else { root_path.to_string() });`

Thus:

- an omitted `project_path` always becomes the filesystem route root, even with a MODULE authority;
- a supplied `project_path` becomes any caller-provided string when the route is bound.

The workspace branch is also path-keyed for the owner. `apply_state_sync_wire` calls `prepare_authority_workspace(&root_path, ...)` at `crates/mc-module/src/lib.rs:6219-6223`, and `prepare_authority_workspace` stores the first member as its `authority_project_path` argument at `crates/mc-module/src/lib.rs:9052-9061`; that argument is the route root, not `authority_project`.

Finally, `replace_authority_memories_tx` writes the mapped value directly at `crates/mc-store/src/lib.rs:9723-9735` and `9763-9821`:

> `SET project_path = ?2 ... &memory.project_path`
> `INSERT INTO mc_memories (id, project_path, ...)`

There is no transaction-level assertion that each owner row equals the bound authority identity.

**Failure sequence**

1. Bind `/repo -> git:A` with memories authority MODULE.
2. Send a non-workspace authority `state_sync` memory with no `project_path`: a `/repo` row is inserted.
3. Or send `project_path:"tenant:B"`: a third-key row is inserted (or an existing seeded row is moved there by source-id adoption).
4. In a workspace payload, the owning member is deliberately mapped to `/repo`.
5. Mirror feed consumers subsequently treat the bad key as authoritative.

**Test that would have caught it**

Activate a real MODULE binding, then state-sync absent, mismatched, and workspace-owner `project_path` forms. Assert all owner memory and mutation rows are `git:A` (or reject the request atomically); assert foreign workspace members remain only their validated member identities.

---

### 4. **HIGH — transforms read the route vocabulary while facades/seed write the identity; historian and wrapup still mint path-keyed rows**

**Evidence**

- The transform sets its project key directly from the route root at `crates/mc-module/src/lib.rs:5508-5517`:

  > `let project_path = binding.project_root.to_string_lossy().to_string();`
  > `ProducerContext { project_path: &project_path, project_directory: &project_path, ... }`

- m0 reads exactly that key at `crates/mc-module/src/m0_compose.rs:130-137` via `resolve_workspace_membership(inputs.project_path)` and `load_memory_render_snapshot(inputs.project_path, ...)`. `authority_project_for_route` is not used by the transform path.

- The organic historian carries the same route value into `HistorianFiringTask` at `crates/mc-module/src/lib.rs:3173-3180`; wrapup explicitly recomputes it from `binding.project_root` at `3244-3253` and stores it in the task at `3281-3285`.

- Publication passes that value to `promote_facts_tx` at `crates/mc-store/src/lib.rs:6660-6672`, which inserts it verbatim at `crates/mc-store/src/lib.rs:10208-10223`.

The new facade guard does the opposite: `resolve_facade_scope` returns the authority identity when `memory_project` matches the route binding (`crates/mc-module/src/lib.rs:7091-7104`), and writes use that identity (`7191-7202`). Authority seed also requires exact identity equality (`5077-5086`). The result is a split store:

- seeded/facade/no-workspace-sync identity rows are invisible to m0/m1, because the transform queries `/repo`;
- historian/wrapup facts are inserted under `/repo` after the route has already been bound. Route-binding triggers do not fire on later memory inserts, so those rows remain path-keyed until some later explicit rebind happens.

This also means an identity-scoped facade write can leave the next transform at the old revision for `/repo`; the bytes remain cache-stable, but the durable write is silently absent from context.

**Test that would have caught it**

Under `/repo -> git:A` MODULE authority: seed and facade-write an identity memory, run the next transform, and assert it appears in m0/m1. Then publish both organic and wrapup historian facts and assert every resulting memory/mutation row uses `git:A`, never `/repo`.

---

### 5. **HIGH — identity-keyed smart notes cannot be evaluated, surfaced, or acknowledged on an authority route**

**Evidence**

`resolve_facade_scope` only looks up an authority identity when an arguments object contains `memory_project`. At `crates/mc-module/src/lib.rs:7088-7111`:

> `let requested_project = arguments.and_then(... "memory_project");`
> `let memory_project_path = match (requested_project, self.store.get()) { ... _ => route_project_root.clone() };`

Both internal note paths pass `None`:

- evaluator: `resolve_facade_scope(channel, None, "notes")` at `crates/mc-module/src/lib.rs:7454-7475`, then `write_note_evaluation` with the route path;
- delivery ACK/NACK: the same call at `7533-7559`;
- transform note claim uses `ctx.project_path` (the route root) at `crates/mc-module/src/transform.rs:1522-1529` and `crates/mc-module/src/m1_compose.rs:275-282`.

After seed/normalization places notes under `git:A`, these calls either find no notes or fail `require_note_project`. The existing evaluator test at `crates/mc-module/src/lib.rs:11988-12023` inserts its note under `/repo` and never activates authority, so it proves only the legacy path case.

**Failure sequence**

1. Seed a pending smart note under authority identity `git:A`.
2. The evaluator bridge calls `note.evaluate` on `/repo`'s route.
3. Scope resolves to `/repo`, so the CAS rejects the `git:A` note.
4. Even if a verdict is already ready, transform claims notes only under `/repo`; no m1 delivery is created.
5. ACK/NACK also scopes `/repo`, leaving any identity delivery unresolved.

**Test that would have caught it**

Repeat the evaluator + render + ACK/NACK lifecycle with active notes authority and distinct route/identity values; assert all transitions and `mc_note_deliveries.project_path` use the identity.

---

### 6. **HIGH — normalization tombstones can delete the canonical TS row when the legacy feed row predates the seed row**

Migration/runtime normalization intentionally deletes a path twin, and feed triggers are already present. Memory feed triggers are created in migration 23 at `crates/mc-store/src/lib.rs:918-990`; migration 30 is later and the upgrade test itself expects a tombstone (`14158-14232`). Migrations therefore do **not** run before these triggers.

The TS mirror's identity model makes ordering safety-critical:

- `packages/plugin/src/features/magic-context/migrations.ts:2032-2039` enforces `UNIQUE(domain, context_row_id)`, so one context row can have only one module identity.
- `rememberIdentity` uses `INSERT OR IGNORE` at `packages/plugin/src/features/magic-context/context-authority.ts:741-754`.
- A tombstone removes its mapping, checks for another mapping, then deletes the context row when none exists at `803-828`.

**Failure sequence**

1. Before authority activation, a CC/foreign facade creates path-keyed module row P for content already present in context.db under `git:A`; its feed insert is first.
2. Authority seed later creates canonical module row C with the same category/hash.
3. Normalization deletes P and emits a tombstone.
4. Mirror replay processes P first and adopts the existing TS row by the unambiguous content match.
5. Replay processes C; it finds the same TS row, but C's mapping is ignored by the unique constraint because P already owns that context row.
6. P's tombstone removes P's mapping, sees no surviving mapping for C, and deletes the canonical TS row.
7. No later feed row is required, so context.db remains missing the memory.

The existing TS test at `packages/plugin/src/features/magic-context/context-authority.test.ts:243-354` tests only the safe reverse order (canonical identity insert first, legacy path insert second, tombstone third).

**Test that would have caught it**

Replay `[legacy path insert, canonical seeded insert, legacy tombstone]` against a pre-existing identity-scoped context row; assert the row and canonical C mapping survive. Run the same ordering through an actual schema-29-to-30 module upgrade and mirror pull.

---

### 7. **HIGH — session resolver bypass trusts a client-supplied harness string**

**Evidence**

At `crates/mc-module/src/lib.rs:7059-7086`, the only discriminator is:

> `if binding.harness == OPENCODE_HARNESS && !is_shadow_session(bound_session) {`
> `bound_session.to_string()`
> `} else { session_resolver.resolve_session(...) }`

But `on_bind` explicitly accepts every route and copies wire identity fields without validation at `crates/mc-module/src/lib.rs:7844-7862`:

> `Accept every route`
> `harness: req.identity.harness.clone()`
> `session: req.identity.session.clone()`
> `BindDecision::accept()`

The TS `HarnessId = "opencode" | "pi"` type is only a producer-side convention; `BindIdentity.harness` on the wire remains client-controlled. The daemon/module provides no authenticated facade mode or allowed target distinction.

**Failure sequence**

1. A Claude Code leg, stale wrapper, or foreign local consumer opens the module route with `harness:"opencode"` and `session:<instance-token>`.
2. The module skips `session.resolve`.
3. Session notes are written with the instance token; search/expand/read scope that token; note anchors load token-keyed cache state.
4. Calls succeed rather than failing typed resolution, silently splitting one conversation into token-namespace state.

Pi currently has no Rust facade transport in `packages/pi-plugin` (its scoped delta is the imitated-argument helper), so `harness:"pi"` is dormant on this surface. If Pi facade calls are added, they will enter the resolver arm and require a Pi-capable `thalamus` mapping.

**Test that would have caught it**

Bind a facade route whose wire harness claims OpenCode but whose session is an instance token; require authenticated route provenance (or explicit identity mode), and assert the resolver cannot be bypassed by changing only the harness string.

---

### 8. **MEDIUM — imitated-argument decoding bypasses tool-schema type validation and can throw inside tools**

**Evidence**

`packages/plugin/src/tools/unwrap-imitated-reduced-args.ts:11-28` parses a model-controlled summary after the framework has validated only the outer `{reduced:boolean, summary:string}` shape, then returns it via an unchecked cast:

> `const parsed: unknown = JSON.parse(record.summary);`
> `return parsed as T;`

Tool executions unwrap before their ordinary checks. For example, `ctx_search` does this at `packages/plugin/src/tools/ctx-search/tools.ts:184-188`:

> `args = unwrapImitatedReducedArgs(args, ["query"]);`
> `const query = args.query?.trim();`

`{"reduced":true,"summary":"{\"query\":{}}"}` passes the outer schema and then throws because an object has no `trim`. The same class exists at memory `content/category?.trim()` (`packages/plugin/src/tools/ctx-memory/tools.ts:507-514`), note action inference/condition checks (`packages/plugin/src/tools/ctx-note/tools.ts:243-248`, `278-281`), and Pi's shared helper call sites. Rust's facade re-parses before its manual validators and generally fails typed, but that does not protect the TS/Pi tools.

The intended real-arguments-win rule does hold for valid primary fields on all five arms: memory `action`, note `action|content`, reduce `drop`, search `query`, expand `message|start`; Rust uses key presence and TS uses non-`undefined` presence. The defect is lack of revalidation after unwrapping, not precedence.

**Test that would have caught it**

For memory, note, reduce, search, and expand in both OpenCode and Pi, feed summaries with wrong types, unknown fields, oversized arrays/strings, and mixed real+summary fields. Assert no throw, normal validation errors, caps applied to the decoded object, and explicit valid primary fields always win.

---

### 9. **HIGH — the new correctness FIFO ignores abort while queued and can strand the rest of the lane**

**Evidence**

The transport is global across sessions (`activeSession`, one callback queue) and allows 16 waiting calls (`packages/plugin/src/hooks/magic-context/shadow-sender.ts:1506-1516`, constant at line 62). At `1593-1612`, a correctness call waits only for `activeSession`:

> `while (this.activeSession !== null) {`
> `await new Promise<void>((resolve) => this.laneReleaseCallbacks.push(resolve));`
> `}`
> `if (args.signal?.aborted) throw args.signal.reason;`

No abort listener is installed while waiting. More importantly, an already-aborted waiter throws at line 1612 **before** entering the try/finally that releases the next waiter (`1628-1632`). The wake chain stops with `activeSession === null` and callbacks still queued.

Rust transform starts its 15-second abort timer before calling the transport at `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:545-559`. Each active transport request has its own 15-second request timeout (`shadow-sender.ts:65`, `1531`, `1620-1624`), but queue time is outside that request timeout.

**Failure sequence**

1. A slow facade/authority/mirror call holds the lane.
2. Transform T1 and T2 queue behind it; T1's Rust timer expires while queued.
3. The active call finishes and wakes T1.
4. T1 observes `signal.aborted` and throws before shifting T2's callback.
5. T2 remains unresolved until unrelated later traffic happens to acquire/release the otherwise-idle lane; its own caller deadline has no effect while queued.
6. Up to 16 queued calls can also make a transform wait multiples of the nominal 15-second deadline, causing raw/LKG fallback late or hanging the host pass.

There is no test for `SubcShadowTransport` correctness-lane FIFO/abort behavior in the delta; the existing `shadow-fifo` test covers the separate best-effort shadow sender coalescing queue.

**Test that would have caught it**

Queue three correctness calls, hold the first beyond the second call's abort deadline, then release it. Assert every promise settles within its own deadline, canceled waiters remove themselves and wake successors, strict FIFO remains, and a facade call cannot delay a transform beyond the transform budget.

## Module-store write-site inventory

| Write path | Project-key result with MODULE authority + binding | Audit result |
|---|---|---|
| Authority seed (`handle_authority_seed_value` -> `seed_*_snapshot`) | Handler requires `snapshot.project_path == authority.project` before insert/update. | Safe at the public handler; exact identity. |
| Facade memory write | `bind_facade_route_for_write`, identity lookup, scope equality check, and `InsertMemoryInput.route_project_root` enforcement. | Identity when the Rust adapter supplies `memory_project`; covered by existing tests. |
| Facade memory update/archive/merge | IDs are re-read for ownership against the resolved facade identity; mutation log copies row identity. | Identity on the guarded facade path. Public store primitives themselves do not know a route. |
| Facade note write/update/dismiss | Same binding/scope enforcement; note inserts additionally carry route enforcement. | Identity on the guarded facade path. |
| Authority state sync memories/mutations/workspace | Wire project path is mapped by `authority_source_path`; owner workspace is mapped from route root. | **Unsafe — Finding 3.** |
| Organic historian publication | Transform passes route root into `promote_facts_tx`. | **Unsafe — Finding 4.** |
| Explicit wrapup historian publication | Explicitly derives `project_path` from `binding.project_root`. | **Unsafe — Finding 4.** |
| Smart-note evaluator CAS | Resolves facade scope with no `memory_project`, producing route root. | **Broken/inert for identity rows — Finding 5.** |
| Smart-note render claim + delivery ACK/NACK | Transform project and no-argument facade scope are route-root keyed. | **Broken/inert; can create route-keyed delivery rows only for legacy route notes — Finding 5.** |
| Inline route normalizer / migration 30 | Deletes path twins, rekeys remaining memories, mutation rows, and notes. | MODULE predicate is present, but upgrade and mirror interactions are unsafe — Findings 1, 2, and 6. |
| TS mirror apply | Writes privileged context.db rows under the feed snapshot's `project_path`. | Propagates upstream bad vocabulary; cleanup ordering can delete canonical rows — Finding 6. |
| User-profile state sync | Global `mc_user_memories`, no project key. | Outside the project-vocabulary split. |
| Test-support seed helpers | Test-only. | Not a production write site. |

## Normalization safety conclusions

- Every migration-30 and runtime normalization statement repeats `authority.state = 'MODULE'`. Rows whose authority is `TS`, `PREPARING`, or `DRAINING` are not rekeyed or deleted. This part of the state predicate is correct.
- The delete matches exact `route_project_root`, category, and normalized hash. With a correct one-root/one-identity binding, same-hash rows are duplicate content by the store's own dedup identity. Worktrees normally have distinct literal roots; symlink aliases are distinct strings; foreign workspace members do not equal the owner root. However, Finding 2 proves the binding itself can be wrong/reused, at which point the SQL has no ownership evidence and can delete a same-content row from another project.
- Feed triggers exist from migration 23 (notes are recreated in migration 26), before migration 30. Migration deletes emit tombstones and updates emit feed rows. They are not hidden migration-context operations.
- Runtime `bind_authority_route` executes normalization through `with_note_conn_fenced(route_project_root)`, so its note update satisfies the ownership trigger. Migration 30 does not, causing Finding 1.

## Cache-safety assessment

No independent steady-state byte-safety violation was found in the delta:

- The only changes in `crates/mc-module/src/m0_compose.rs`, `m1_compose.rs`, and `transform.rs` are test-fixture initialization of the new `route_project_root` field. Production render functions and tail serialization are unchanged.
- Runtime normalization mutates source tables, not frozen response bytes. `m1_revision_signal_parts` hashes max memory id, max mutation id, and max compartment sequence (`crates/mc-module/src/m1_compose.rs:116-135`), and hard composition carries a memory revision that `commit_transform` rechecks inside the fenced transaction (`crates/mc-store/src/lib.rs:5047-5080`). A source change racing an in-flight hard pass therefore conflicts rather than committing mixed bytes.
- Notes deliberately ride a later natural bust and do not alter a defer response.

The authority defects instead cause the opposite failure: identity writes are often invisible to the route-keyed revision/read path, so a pass can correctly remain byte-identical while failing to surface durable state. That is correctness failure, not an uncoordinated cache bust.

## Concurrency assessment

No SQLite lock-order deadlock was found in the inline normalizer: `bind_authority_route` performs the binding upsert and normalization in one `with_note_conn_fenced` transaction, and transform writes use the same single fenced connection. There is no nested store transaction on this path. The material concurrency defect is the transport FIFO in Finding 9: it globally serializes facade, authority, mirror, and transform work without deadline-aware queueing and can strand successors after an aborted head waiter.

## Verification performed

- `cargo test -p mc-store authority_route_binding -- --nocapture` — passed (4 tests).
- `cargo test -p mc-module authority_ -- --nocapture` — passed (12 tests; the tests do not cover distinct route/identity transform composition, route-keyed note migration, or adversarial state-sync source paths).
- `cargo test -p mc-module facade_ -- --nocapture` — passed (16 tests; the authority-note evaluator fixture remains route-keyed, and the authority facade-write test does not run a subsequent transform).
- Python in-memory SQLite probe reproducing migration 30's project-key update under the installed ownership trigger with the registered UDF's default empty caller — reproduced `note ownership update is outside the old or new project`.
- Isolated Bun probe of `unwrapImitatedReducedArgs({reduced:true, summary:'{"query":{}}'}, ['query'])` followed by the real `query.trim()` shape — reproduced the unchecked-type throw.
- `bun test packages/plugin/src/features/magic-context/context-authority.test.ts` — could not start because worktree dependencies are absent (`ai-tokenizer`).
- `bun test packages/plugin/src/tools/ctx-memory/tools.test.ts packages/plugin/src/tools/ctx-note/tools.test.ts` — could not start because worktree dependencies are absent (`@opencode-ai/plugin`, `@cortexkit/subc-client`). No package install was performed for this read-only audit.
