# Council Synthesis — Magic Context SHADOW-MODE Blind Adversarial Audit

**Question:** Blind adversarial council audit of the Magic Context SHADOW-MODE lane before arming a soak on production OpenCode sessions. Hunt the next bug class. Verdict SHIP/HOLD.

**Repo/Branch:** `/Users/ufukaltinok/Work/Projects/CortexKit/magic-context`, `subc-migration`, HEAD `6ea3179f`
**Intent:** AUDIT · **Mode:** solo · **Members consulted:** 8/8 (all completed with valid responses)

---

## VERDICT: **HOLD** — Unanimous (8/8)

Every one of the eight council members independently reached HOLD. No member voted SHIP. The single blocking reason is unanimous: **the compartment `state_sync` wire shape does not deserialize on the Rust side** — the exact "wire-shape mismatch the mocks miss" bug class the audit brief warned had already burned twice. Arming the soak today would produce **zero valid parity comparisons for the entire target population** (any session that has ever formed a compaction boundary), while dashboards show only generic "send failed" noise — the soak would certify parity it never measured.

**Athena verified the critical finding directly against source** (shadow-sender.ts:584-604 nested `start`/`end` vs lib.rs:269-270 required flat `start_message`/`end_message` with no `#[serde(default)]`). It is real, not a false positive.

**Agreement level: STRONG.** The top 3 findings are unanimous or near-unanimous with independent file:line evidence. Divergence exists only on severity labels (High vs Medium) and on a few solo findings.

---

## Blast-radius weighting (per the brief)
- **Class (1) — affects the LIVE user session** = WORST. → Findings #3 (clone latency), #7 (mutex poison), plus the uncaught pre-transform capture (#3 sub-point).
- **Class (2) — corrupts shadow state silently** = defeats the soak's purpose. → Findings #1, #2, #4, #5, #6.
- **Class (3) — merely loses shadow coverage, logged** = ACCEPTABLE. → Findings #8, #9, and several Low items.

---

## FINDINGS

### UNANIMOUS (all / near-all members)

#### #1: Compartment `state_sync` wire-shape mismatch — every sync carrying a compartment rejects at Rust serde
- **Severity**: Critical
- **Confidence**: Unanimous (8 members) — Athena independently verified against source
- **Members Reported**: Opus 4.8, GPT 5.6 PRO, GPT 5.6 Terra PRO, GPT 5.5 xhigh, XAI Composer 2.5, Ollama Minimax M3, Ollama GLM 5.2, Gemini Flash 3.5
- **Issue**: TS `serializeCompartment` emits each compartment as **nested** objects `start:{ flat_id, bare_message_id, absolute_ordinal }` / `end:{...}`. Rust `ShadowCompartmentWire` declares **required, non-defaulted flat** fields `start_message: i64` and `end_message: i64`. With no `deny_unknown_fields`, the nested objects are silently ignored and serde fails with *"missing field `start_message`"*. `handle_shadow_state_sync_value` returns `invalid_params`.
- **Evidence**: `shadow-sender.ts:584-604` (nested emit) vs `crates/mc-module/src/lib.rs:269-270` (required flat, NO `#[serde(default)]`) — **VERIFIED by Athena**. Handler reject at `lib.rs:2148-2151`. The Rust integration test at `lib.rs:7071-7080` uses the *correct flat* shape (`"start_message": 0, "end_message": 0`); the TS test at `shadow-sender.test.ts:~383` only asserts `compartments: expect.any(Array)` against a `FakeTransport` that accepts any shape. No test sends TS-built compartments through the real Rust serde parser. `real_daemon.rs` exercises no shadow ops.
- **Impact**: Class (2), silent soak defeat. `processPass` calls `state_sync` **before** the transform, so a rejected sync means `shadow_transform` is never reached. `invalid_params` is neither a peer-reject nor a connection failure, so the sender just does `send_failures += 1` + "send failed (ignored)" and retries forever. **Every compartmentalized session — i.e. every real long-lived soak target — produces zero valid comparisons** while looking healthy. (Several members note it can also drive a reset loop / wedge.)
- **Fix Direction**: Emit flat `start_message`/`end_message`/`start_message_id`/`end_message_id` from TS (matches the existing Rust test — simplest), OR add a nested custom deserializer / `ShadowCompartmentEdgeWire` on Rust. **Then add a shared cross-language fixture that serializes a real populated compartment through `toFlatWireBody` and deserializes it as `ShadowStateSyncWire`** — this is the test gap that let the bug class recur.

#### #2: `m0_mutations` (and `watermarks`) silently dropped at the Rust parser
- **Severity**: High (impact debated Medium–High)
- **Confidence**: Unanimous (8 members)
- **Members Reported**: All 8
- **Issue**: TS sends `m0_mutations` (from `getM0MutationsAfterId`) and `watermarks` in `state_sync`. `ShadowStateSyncWire` has **no `m0_mutations` and no `watermarks`** field — it declares `acked_watermarks` (name mismatch) and no m0 mutation type. With no `deny_unknown_fields`, both are silently discarded (`grep m0_mutation` in `crates/mc-module` = 0 hits). Rust recomputes `acked_watermarks` itself, so the `watermarks` drop is benign; the **`m0_mutations` drop is functional** — the Rust shadow transform materializes m[0] without the mutation inputs the live TS lane applied. Notably TS treats `max_mutation_id` changes (delete/merge/recomp) as a **HARD** materialization trigger (`inject-compartments.ts:1121-1129`), so this is byte-affecting.
- **Evidence**: `shadow-sender.ts:696-706,732,735` (TS emit) vs `crates/mc-module/src/lib.rs:184-200` (wire struct). `reset_shadow_session` (lib.rs:~2207) has no shadow m0-mutation table to clean up. TS still advances `lastAckedWatermarks.m0_mutation_id`, so dropped mutations are never resent.
- **Impact**: Class (2). On sessions *without* compartments (where #1 doesn't fire), every m0-mutation-bearing pass produces a **systematic m[0] byte divergence** rooted in missing input, not a real transform bug — poisoning the divergence signal with false positives (or masking real ones).
- **Fix Direction**: Add `m0_mutations` to the wire + a shadow m0 table + reset cleanup with delete/merge/recomp semantics; OR (if m[0] is intentionally recomputed) stop sending `m0_mutations`, stop advancing its watermark, and document the exclusion. Rename `watermarks`→`acked_watermarks` if the value is meant to be consumed. Temporarily enable `deny_unknown_fields` in integration tests to catch this drift class.

#### #3: Shadow capture adds synchronous, unbounded (and partly uncaught) work to the LIVE transform path
- **Severity**: High
- **Confidence**: Unanimous (8 members)
- **Members Reported**: All 8
- **Issue**: With `shadow_transform.enabled`, each pass runs synchronously on the user's critical path: (1) `cloneForShadow(messages)` at transform start, then in `enqueue` (2) `cloneJson(inputMessages)` in `resolveOrdinalsForShadow` and (3) `cloneJson(outputMessages)` in `denormalizeShadowOutput`. `cloneJson` is `JSON.parse(JSON.stringify(...))` over the **entire** array. On a 400-message tool-heavy session this is estimated at tens-to-hundreds of ms per pass — a real latency regression on the live prompt, contradicting the "no live impact" contract. **Critically, `cloneForShadow` + `resolveDeclaredTrimForShadow` at `transform.ts:459-462` execute OUTSIDE the shadow try/catch** (which starts ~2186/2226), so a SQLite or serialization exception there can **reject the live transform**, not merely lose shadow coverage.
- **Evidence**: `transform.ts:459` (uncaught `cloneForShadow`), `459-462` (uncaught trim capture), `shadow-sender.ts:226-228` (`cloneJson`), `:380`, `:288` (the two enqueue clones). The enqueue clones ARE inside try/catch (`transform.ts:2226`) so throw-safe but not cost-bounded; the line-459 capture is neither.
- **Impact**: Class (1) — worst weighted, touches the live session (latency always; live-transform rejection if the pre-guard capture throws).
- **Fix Direction**: Wrap the initial capture in a fail-open guard. Consolidate three clones into one (clone once, annotate/denormalize in-place) or use `structuredClone`. Defer heavy prep (ordinal resolution, denormalize) to the async `runQueue` worker so `enqueue` stays O(1). Add a byte/message-size cap that skips (with a counter) oversized passes. Add shadow prep to latency telemetry and measure on a large session before arming.

### MAJORITY (>half)

#### #4: Recomp / below-floor drift leaves STALE compartments (and/or mixed-basis ordinals) in the shadow store
- **Severity**: High (one member Medium-High)
- **Confidence**: Majority (5 members)
- **Members Reported**: Opus 4.8, GPT 5.6 Terra PRO, GPT 5.5 xhigh, GPT 5.6 PRO (ordinal-basis variant), Gemini Flash (ordinal variant)
- **Issue**: Two related silent-corruption mechanisms:
  - **(a) Recomp staleness (Terra PRO, GPT 5.5):** Recomp `DELETE`s and recreates all live compartments, often **reusing old sequence numbers**. The sender only transmits `sequence > acked` high-water mark, so it sends none of the recreated rows; the Rust store only upserts supplied compartments with **no delete/replace op**. It emits an `m0_mutations` record to signal this — which Rust ignores (see #2). Shadow store retains obsolete compartments silently.
  - **(b) Ordinal-basis mismatch (Opus, GPT 5.6 PRO):** The tail-primed cache carries absolute ordinals anchored to a stored `baseOrdinal`/compaction marker, while the by-id `COUNT(*)` fallback computes ordinals fresh from current DB state. If a non-summary row **below the boundary is deleted** (message revert/undo) after the marker was written, above-floor (cache) and below-floor (by-id) messages carry **different bases within one pass**; the drift check only compares an id against its own prior value, so it can't catch intra-pass inconsistency. Terra PRO adds: compartment serialization uses `ordinalForMessageId` with **no** by-id fallback, so below-floor compartment endpoints return `null`→`"unresolved"` and the pass silently skips sync.
- **Evidence**: `compartment-runner-recomp.ts:110-123` (delete+reinsert+`recomp_boundary_change`); `shadow-sender.ts:646-647` (skip `sequence <= acked`), `:696-706`; `crates/mc-store/src/lib.rs:~3991-4036` (upsert only, no delete); `shadow-sender.ts:385-396` (by-id fallback) vs `:639-644` (compartment path, no fallback); `read-session-raw.ts:106-117` vs `:384-393`.
- **Impact**: Class (2). Silent shadow lineage corruption → false divergences on long/recompacted sessions, the highest-value soak targets. Reachability is conditional (recomp event, or below-boundary deletion + active tail-prime), hence not Critical.
- **Fix Direction**: On any recomp/m0 revision, **force a full shadow reset + snapshot** (don't ack the watermark until Rust applies it) OR add an authoritative-replace sync mode that deletes absent shadow compartments. For ordinals: anchor the by-id fallback to the same `baseOrdinal` basis as the primed cache (or fail-loud → reset when they can't be reconciled); give the compartment path the same by-id fallback the input path has.

#### #5: Session permanently wedged after peer-reject / reset-failure in `runQueue`
- **Severity**: High (one member Medium)
- **Confidence**: Majority (4 members)
- **Members Reported**: Ollama Minimax M3, Gemini Flash 3.5, GPT 5.6 Terra PRO (variant), Opus 4.8 (verified-good on the self-heal path — see Dismissed note)
- **Issue**: On a peer-reject, the `runQueue` catch sets `state.blockedUntilReset = true` and `requireResetReason` but (per Minimax) does **not** enqueue a reset work item; `processPass` then short-circuits every subsequent pass with `if (state.blockedUntilReset) return`. `shadowSender.resetSession()` exists but has **0 call sites**. Gemini variant: if a queued `reset` item itself throws (transient connection failure), the reset arm is not wrapped in try/catch, the item is already shifted off the queue, and the session is wedged with no path to re-queue. Net: shadow coverage silently lost for that session until process restart. **Note:** Opus reported the transitions self-heal via `processPass`→`performReset` on the next enqueue — a genuine intra-council disagreement on whether the mismatch path (which DOES push a reset) vs the connection-failure path (which per Minimax/GPT-5.6-Pro does not) is the wedge. Worth resolving empirically.
- **Evidence**: `shadow-sender.ts:887-904` (catch sets flags, no `pushWork`), `:960` (`blockedUntilReset` short-circuit), `:1026-1043` (the mismatch path that DOES push a reset — contrast), `:898-902` (connection-failure path sets `initialized=false`/`requireResetReason` only), reset arm at `:878-885` (no try/catch per Gemini).
- **Impact**: Class (3) if only coverage is lost (acceptable-but-silent); becomes a soak-integrity problem because it's silent and per-session sticky.
- **Fix Direction**: In the catch's peer-reject/connection-failure branches, push a reset work item to the front (or set a flag the next enqueue honors). Wrap the reset arm in try/catch that preserves `blockedUntilReset` and retries. Wire `resetSession()` to the live lane's generation-bump/reset path.

#### #6: SubcShadowTransport — request timeout does not destroy the socket, corrupting frame alignment
- **Severity**: Medium
- **Confidence**: Majority (4 members)
- **Members Reported**: GPT 5.5 xhigh, GPT 5.6 Terra PRO, Ollama Minimax M3, Ollama GLM 5.2
- **Issue**: On a 5s `readTerminalFor` timeout, `unaryJson` does not `socket.destroy()` and does not classify `"subc request timeout"` as a connection failure, so no route reopen/backoff. If a header was read but the body timed out, stale partial bytes remain in the `SocketReader` buffer; `ensureConnected` sees the socket as alive (`!destroyed`) and reuses it, so the next `readFrame` starts mid-body → all subsequent shadow passes fail with frame-parse errors until the server closes. (Opus reached the opposite conclusion — see Dissent.)
- **Evidence**: `shadow-sender.ts:1192-1213` (`unaryJson`, no cleanup on error), `:1159` (`ensureConnected` reuses non-destroyed socket), `:1275-1305` (`readExact` throws without consuming buffer), `:1176-1180` (close handler only fires on server close), `isConnectionFailure` only matches backoff/ECONN strings.
- **Impact**: Class (2)/(3) — shadow-only, but silently corrupts all shadow coverage for that connection until the daemon closes it.
- **Fix Direction**: Destroy socket + clear routes on any read/write/protocol/timeout error; classify timeouts as connection failures; add a max frame-length guard; add real-socket tests for partial-frame timeout, mid-frame close, proof rejection, handshake timeout.

### MINORITY (2–3 members)

#### #7: Cross-lane coupling via shared store handle + shared `std::Mutex` (poison / contention)
- **Severity**: Medium
- **Confidence**: Solo→Minority (1 primary: Opus 4.8; touches Finding-5 store-sharing concerns others raised indirectly)
- **Members Reported**: Opus 4.8
- **Issue**: The `shadow_transform` handler runs `transform_with_projection` + divergence recording against the **same** `Arc<McStore>` connection (serialized by `with_conn_fenced`) and takes the **same** `self.bindings`/store `std::Mutex`es as the live lane. Two channels: (a) DB-lock **contention** adds hold-time that can delay real-lane store ops (latency); (b) **mutex poisoning** — the handler uses `.expect("… mutex")`; a panic in shadow code while holding a shared lock poisons it, and the next real-lane `.lock().expect(...)` panics too. Low-probability (compare runs outside the txn, rusqlite is Result-based) but a genuine cross-lane blast channel the "observe-only" framing assumes cannot exist.
- **Evidence**: `handle_shadow_transform_value` (lib.rs:~2250-2438), `self.bindings.lock().expect(...)` (~lib.rs:790), `with_conn_fenced` in shadow store methods.
- **Impact**: Class (1) if poison ever triggers; else Class (3) latency.
- **Fix Direction**: Ensure no shadow path can panic while holding a lock shared with the live lane (poison-tolerant recovery or `parking_lot` non-poisoning mutexes for shared bindings); consider a separate store connection/handle for the shadow lane.

#### #8: By-id DB fallback is O(session_size) per below-floor id (COUNT full-scan)
- **Severity**: High (Minimax) / performance-only
- **Confidence**: Minority (2 members)
- **Members Reported**: Ollama Minimax M3, GPT 5.5 xhigh (as part of #2 there)
- **Issue**: The below-floor `readRawSessionMessageById` fallback runs `SELECT COUNT(*) ... WHERE (time_created < ? OR (=? AND id <= ?))` — O(session_size) per call. The code comment claims below-floor ids are "a handful" per pass, bounding the *number* of calls, but each call still scans from session start; on a 100K-message session with marker lag this can be ~1M rows scanned per pass. Since this runs in the enqueue path, it feeds the live-latency concern (#3).
- **Evidence**: `shadow-sender.ts:395` → `read-session-raw.ts:384-393`; `read-session-db.ts:77-89`.
- **Impact**: Class (1) latency contribution on very large sessions.
- **Fix Direction**: Anchor the by-id ordinal to the tail-prime `baseOrdinal` + tail position instead of a fresh `COUNT(*)`; or maintain an in-memory ordinal index.

#### #9: Read-only OpenCode session DB opened without `PRAGMA busy_timeout`
- **Severity**: Low–Medium
- **Confidence**: Minority (2 members)
- **Members Reported**: Ollama GLM 5.2, Gemini Flash 3.5
- **Issue**: `getReadOnlySessionDb` opens `new Database(dbPath, { readonly: true })` with no `busy_timeout`, unlike the MC DB (`storage-db.ts:354`) and `open-opencode-db.ts:18` (both set 5000ms). Under OpenCode writer load / WAL checkpoint, the shadow hot-path reads can hit `SQLITE_BUSY` immediately rather than waiting. The throw is caught by `transform.ts:2226` so the pass is skipped safely — but coverage is silently lost under write contention.
- **Evidence**: `read-session-db.ts:56-66` vs `open-opencode-db.ts:18`.
- **Impact**: Class (3), silent coverage loss under load.
- **Fix Direction**: `db.exec("PRAGMA busy_timeout = 2000..5000")` after opening, or short retry/backoff.

### SOLO (single member — lower confidence, verify before acting)

#### #10: Rust `handle_guidance_value` does not reject shadow bindings (isolation gap)
- **Severity**: Low
- **Confidence**: Solo (1 member: Ollama GLM 5.2)
- **Issue**: `handle_agent_drops_value`, `handle_transform_value`, and `ctx_reduce` all guard with `is_shadow_session(...)`, but `handle_guidance_value` (lib.rs:~1730-1779) calls `resolve_binding` with no such check. A shadow route could call `guidance.get` and receive guidance text. Read-only — cannot corrupt state — but an isolation-enforcement gap contradicting the brief's "every arm" requirement.
- **Impact**: Class (3), read-only. Note this partially qualifies the otherwise-unanimous "isolation verified good" conclusion.
- **Fix Direction**: Add the `is_shadow_session` guard to `handle_guidance_value`.

#### #11: Rust byte-affecting history budget differs from TS (60_000 hardcoded vs config-derived)
- **Severity**: High (per reporter)
- **Confidence**: Solo (1 member: GPT 5.6 PRO)
- **Issue**: TS derives history budget = context limit × execute threshold × `history_budget_percentage` (default 0.15); Rust freezes every binding to `60_000` tokens with a comment saying config-reading is a "later refinement." `ShadowPassInputs` carries no TS-derived budget. For 200K context at default threshold, TS≈19,500 vs Rust 60,000 → different decay tiers once history crosses the smaller limit → systematic false byte divergences on long histories.
- **Impact**: Class (2) if real — would poison the divergence signal for exactly the long sessions the soak targets. **SOLO and not cross-verified by Athena — verify the Rust constant and whether `ShadowPassInputs` already carries an effective budget before treating as blocking.**
- **Fix Direction**: Include the resolved `history_budget_tokens` in each shadow pass and consume it in Rust; audit all byte-affecting resolved budgets for the same recompute-default divergence.

#### #12: Workspace-memory inputs rendered by TS are absent from shadow sync
- **Severity**: High (per reporter)
- **Confidence**: Solo (1 member: GPT 5.6 PRO)
- **Issue**: Live TS materialization resolves the workspace and calls `getMemoriesByProjects` over expanded identities with share-category filtering; shadow sync calls only `getMemoriesByProject` for the owning project. Rust returns no workspace membership for a `shadow:` project, and `ShadowMemoryWire` has no `project_path` field, so source identity is lost. Workspaced sessions would false-diverge.
- **Impact**: Class (2) for workspaced sessions. **SOLO — verify against actual workspace-memory render path before treating as blocking.**
- **Fix Direction**: Mirror the exact visible workspace memory set + source attribution + workspace-sensitive watermarks/mutations; add a shadow test from existing workspace-memory fixtures.

### Low-severity / observability (solo, non-blocking)
- **Failed-auth socket leak** (GPT 5.6 PRO): `ensureConnected` creates a local socket, authenticates before assigning `this.socket`; on auth failure the catch destroys `this.socket` (still null), leaking the candidate FD. Fix: destroy the candidate on every pre-install failure. Class (1)-adjacent (live process FD leak) but rate-limited by backoff — **Medium, worth fixing pre-soak.**
- **Config spread leaks `embedding.api_key` into hook config** (GPT 5.5): inert today (0 hook call sites; `project-security.ts` strips `shadow_transform` from project config so repos can't self-arm), but fragile if hook code ever logs/serializes config. Fix: forward typed fields or add a warning. Low.
- **Shadow transform skips `trace_pass_*`** (GPT 5.5): observability gap, by-design, document it. Low.
- **`shadow_reset` `reason` / `last_todo_state_hash` fields dropped by Rust** (GPT 5.5): informational-only, no impact. Low.
- **Duplicate `resolveDeclaredTrimForShadow` / `getAutoSearchHintDecisions` DB reads per pass** (GPT 5.5): minor extra live DB load, feeds #3. Low.
- **Malformed-JSON ordinal discrepancy** (Gemini): `readRawSessionMessagesFromDb` filters malformed rows (shifting ordinals) but the by-id `COUNT(*)` counts them → false ordinal mismatch if malformed rows exist (rare). Low.

---

## Summary Table

| # | Finding | Severity | Agreement | Members |
|---|---------|----------|-----------|---------|
| 1 | Compartment wire-shape mismatch (nested vs flat) rejects every compartment sync | **Critical** | Unanimous | 8/8 |
| 2 | `m0_mutations`/`watermarks` silently dropped at Rust parser | High | Unanimous | 8/8 |
| 3 | Synchronous + partly uncaught shadow capture on LIVE transform path | High | Unanimous | 8/8 |
| 4 | Recomp / below-floor drift → stale compartments / mixed-basis ordinals | High | Majority | 5/8 |
| 5 | Session permanently wedged after peer-reject / reset-failure | High | Majority | 4/8 |
| 6 | Request timeout doesn't destroy socket → frame misalignment | Medium | Majority | 4/8 |
| 7 | Shared store handle + std::Mutex poison/contention cross-lane channel | Medium | Solo | 1/8 |
| 8 | By-id COUNT fallback O(session_size) per below-floor id | High/perf | Minority | 2/8 |
| 9 | Read-only session DB missing `busy_timeout` | Low–Med | Minority | 2/8 |
| 10 | `handle_guidance_value` missing shadow-binding guard | Low | Solo | 1/8 |
| 11 | Rust history budget 60k hardcoded vs TS config-derived | High* | Solo | 1/8 |
| 12 | Workspace-memory inputs absent from shadow sync | High* | Solo | 1/8 |

\* Solo High findings not cross-verified — verify before treating as blocking.

---

## Priority Recommendations

### MUST FIX before arming the soak (blocks the soak's purpose or hits the live lane)
1. **#1 Compartment wire shape** — flatten the TS emit (or add Rust nested deserializer), then add a **shared TS↔Rust serde fixture** with a populated compartment. This is the recurring bug class; the fixture is the durable fix.
2. **#3 Live-path safety** — move `cloneForShadow` + `resolveDeclaredTrimForShadow` inside a fail-open guard; consolidate/defer clones; add a size cap; add latency telemetry. Measure per-pass overhead on a 400-message session before arming.
3. **#2 `m0_mutations` drop** — decide: implement the shadow m0 path, or stop sending + document the exclusion. Either way, add `deny_unknown_fields` to integration tests to catch the whole silent-drop class (this alone would have caught #1, #2).

### SHOULD FIX before or early in soak (silent shadow corruption)
4. **#4 Recomp/ordinal drift** — force full reset+snapshot on recomp; reconcile ordinal bases or fail loud.
5. **#5 Session wedge** — resolve the Opus-vs-Minimax disagreement empirically, then guarantee a reset is always re-queued and wire `resetSession()`.
6. **#6 Socket cleanup on timeout** — destroy+reconnect on any transport error; classify timeouts as connection failures.

### VERIFY (solo/unconfirmed — cheap to check, potentially blocking)
7. **#11 history budget** and **#12 workspace memory** — both Class (2) if real; confirm against source before the soak so divergence noise doesn't mask real bugs.

### NICE TO HAVE (coverage/robustness)
8. #7 mutex poison isolation, #8 by-id COUNT cost, #9 busy_timeout, #10 guidance guard, and the Low items.

---

## Dismissed / Verified-Good (false-positive filtering)

Members explicitly checked and cleared these — recorded so they are not re-litigated:
- **Shadow isolation (3a) — largely GOOD.** `shadow_binding` rejects non-shadow bindings; plain `transform` and `ctx_reduce` reject shadow bindings, enforced on those arms (tests at lib.rs:~7004-7030). Shadow writes namespaced by `shadow:` session_id. **Caveat: Finding #10 (guidance arm) is the one gap — read-only, low.**
- **CAS (3c) — GOOD.** `apply_shadow_state_sync` gates on `shadow_generation` + `expected_shadow_seq`; those fields ARE sent correctly by `toFlatWireBody`. Compartment upserts keyed `(session_id, sequence)` → overlapping sequences are idempotent-upsert, not duplicating. Duplicate sync correctly rejects with `shadow_seq_mismatch` (by design).
- **enqueue throw-safety (2a) — GOOD.** The `enqueue` call is wrapped in try/catch at `transform.ts:2226`, so throws inside resolveOrdinals/denormalize/cloneJson/by-id read (incl. SQLITE_BUSY) are caught and the live pass proceeds. **The remaining live exposure is the un-guarded `cloneForShadow` at line 459 (#3) and cost, not correctness.**
- **Ordinal basis in steady state — GOOD.** Full/tail reader (`index+1` after summary filter) and by-id `COUNT(*)` (same filter + sort) agree in steady state; the drift risk (#4) is conditional on below-boundary deletion / tail-prime.
- **`tool_provider` route kind — GOOD.** Matches the Rust manifest.
- **Config full spread (#1 create-session-hooks) — GOOD (no active bug).** Extra keys inert; no serialization/logging sink for credentials found; `project-security.ts` strips `shadow_transform` from project config. Only the latent `embedding.api_key` smell (Low) remains.
- **PARITY docs** — `packages/pi-plugin/PARITY.md` and smart-notes `PARITY.md` cover Pi↔OpenCode host differences and SSRF guards respectively; **neither documents any of these shadow divergences as intentional**, so none of the findings are pre-sanctioned.

### Genuine intra-council dissent (preserved, not flattened)
- **#5 wedge:** Opus 4.8 concluded the block/reset transitions self-heal via `processPass`→`performReset`; Minimax M3 / GPT 5.6 Terra / Gemini found paths (connection-failure catch; throwing reset arm; unwired `resetSession()`) that do NOT re-queue a reset. Resolve empirically before relying on self-heal.
- **#6 socket timeout:** GPT 5.5 / Terra / Minimax / GLM see frame-misalignment persistence; Opus 4.8 concluded corr+channel matching in `readTerminalFor` consumes and discards the late response so the stream re-aligns and there's no permanent wedge. The difference hinges on whether stale bytes remain buffered after a body-read timeout — verify with a partial-frame-timeout test.

---

## Confidence
**HIGH** on the verdict (HOLD) and on findings #1–#3 (unanimous, independent file:line evidence, #1 Athena-verified against source). **MEDIUM** on #4–#6 (majority, some conditional reachability, two live dissents). **LOWER** on solo findings #7, #10–#12 (single-member, uncross-verified) — flagged for verification rather than immediate action.
