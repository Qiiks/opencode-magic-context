## Finding 1: `state_sync` compartment wire shape mismatch (TS nested `start`/`end` vs Rust flat `start_message`/`end_message`)
- **Severity**: critical
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts` (`serializeCompartment` ~584–615) vs `crates/mc-module/src/lib.rs` (`ShadowCompartmentWire` ~267–295, `handle_shadow_state_sync_value` ~2148–2151)
- **Confidence**: high
- **Issue**: TS emits compartments as `{ id, sequence, start: { flat_id, bare_message_id, absolute_ordinal }, end: { ... }, title, ... }`. Rust expects flat `start_message`, `end_message`, `start_message_id`, `end_message_id` (required `i64` for start/end). Serde has no aliases for nested `start`/`end`. Any `state_sync` that includes non-empty `compartments` fails whole-request deserialization (`invalid_params`) or, if tests only used empty arrays, the soak never mirrors compartment history into the shadow store.
- **Evidence**: TS builder at 584–615; Rust struct at 267–295; module integration test uses flat JSON at `lib.rs` 7071–7080. `shadow-sender.test.ts` (~350–447) asserts flat top-level sync fields and `compartments: expect.any(Array)` but does not assert per-compartment Rust shape.
- **Suggested Fix**: Align TS serialization to `ShadowCompartmentWire` (map `start.absolute_ordinal` → `start_message`, `end.absolute_ordinal` → `end_message`, ids to `*_message_id`, drop or map `id`/`flat_id`), or add `#[serde(alias)]`/custom deserializer on Rust. Add a round-trip test with one real compartment.

## Finding 2: `m0_mutations` mirrored on TS wire but absent from Rust `ShadowStateSyncWire`
- **Severity**: high
- **Location**: `shadow-sender.ts` (`buildStateSyncPayload` ~696–706, ~735) vs `crates/mc-module/src/lib.rs` (`ShadowStateSyncWire` ~185–200)
- **Confidence**: high
- **Issue**: TS includes `m0_mutations` in every state sync payload and tests document it (`shadow-sender.test.ts` ~385). Rust wire struct has no `m0_mutations` field; serde silently ignores it. Shadow Rust transform never sees M0 mutation log state TS uses, causing systematic byte/decision divergence once M0 paths matter—not a logged “coverage loss,” a silent wrong mirror.
- **Evidence**: TS `m0_mutations: m0Mutations` at 735; grep shows zero `m0_mutation` references under `crates/mc-module`. `apply_shadow_state_sync` (~2122–2130) only upserts compartments, memories, memory_mutations.
- **Suggested Fix**: Extend `ShadowStateSyncWire` + `apply_shadow_state_sync` to apply M0 mutations (parity with TS watermarks `m0_mutation_id`), or stop claiming parity and gate shadow compare until implemented.

## Finding 3: Post-transform `enqueue()` still does synchronous JSON clone + ordinal/DB work on the transform completion path
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/transform.ts` (~2186–2228); `shadow-sender.ts` `enqueue` (~1016–1068), `resolveOrdinalsForShadow` (~361–408), `denormalizeShadowOutput` (~282–358)
- **Confidence**: high
- **Issue**: Shadow is “fire-and-forget” for subc I/O, but `enqueue()` runs synchronously after transform completes (still before the hook returns). Per pass it can: `cloneJson` input (~380), `readRawSessionMessages` / tail cache (~375–378), per-missing-id `readRawSessionMessageById` → `withReadOnlySessionDb` (~395–314), and another full `cloneJson` on output in `denormalizeShadowOutput` (~288, ~1054–1058). On large sessions (hundreds of messages, huge tool bodies) this is multi‑millisecond CPU + SQLite opens on the same thread that just finished transform—blast radius to live latency when `shadow_transform.enabled` is on.
- **Evidence**: `transform.ts` 2205 `shadowSender.enqueue`; `cloneForShadow` only covers input at start (459), not enqueue work; test “keeps sender exceptions off the transform hot path” (~494–514) only covers transport throw after enqueue returns, not cost/stall.
- **Suggested Fix**: Defer heavy prep to `setImmediate`/microtask queue item (keep enqueue O(1)), or precompute annotated input during transform behind the existing clone; cap/lazy denormalize; batch by-id ordinal reads.

## Finding 4: `resolveDeclaredTrimForShadow` + marker read at transform **start** when shadow is enabled
- **Severity**: medium
- **Location**: `transform.ts` (~457–462, ~2188); `shadow-sender.ts` `resolveDeclaredTrimForShadow` (~483–511)
- **Confidence**: high
- **Issue**: Besides post-pass enqueue, enabling shadow adds `resolveDeclaredTrimForShadow` at the beginning of every transform (before main work). That hits session meta, compartment-by-end-message, and `readRawSessionMessages` (cache or full DB). Live sessions pay extra I/O on every pass, not only when a shadow pass is actually queued.
- **Evidence**: `transform.ts` 460–461; `resolveDeclaredTrimForShadow` 495–499.
- **Suggested Fix**: Move “before” trim capture to post-pass only (compare using persisted marker delta) or cache trim with invalidation on compaction marker updates only.

## Finding 5: Ordinal fallback vs tail-primed cache — basis mismatch risk for compartment `state_sync` (not input ordinals)
- **Severity**: medium
- **Location**: `resolveOrdinalsForShadow` (~385–396) vs `serializeCompartment` / `ordinalForMessageId` (~639–644, ~570–581) in `shadow-sender.ts`; `read-session-chunk.ts` tail prime (~242–264)
- **Confidence**: medium
- **Issue**: Input ordinals use by-id DB fallback when cache is tail-only; comment claims same COUNT semantics as full read (~389–393). **Compartment** serialization uses `ordinalForMessageId` with `rawById` from `readRawSessionMessages` only—no by-id fallback. If compartment boundary message IDs sit below the tail-prime floor, `serializeCompartment` returns `null` → `buildStateSyncPayload` returns `"unresolved"` (~655–656) and pass skips sync (~978–984) without reset. Shadow store drifts from TS silently (logged skip), defeating soak on long sessions with new compartments.
- **Evidence**: `ordinalForMessageId` 421–422 returns null if not in map; `buildStateSyncPayload` 639–644 builds map from full cache read only; tail cache docs 219–234.
- **Suggested Fix**: Reuse `readRawSessionMessageById` in `ordinalForMessageId` / compartment path; or include compartment endpoints in tail prime.

## Finding 6: `blockedUntilReset` after peer reject can drop in-flight passes without re-queue
- **Severity**: medium
- **Location**: `shadow-sender.ts` `processPass` (~960), `runQueue` catch (~887–904), `enqueue` mismatch path (~1028–1043)
- **Confidence**: medium
- **Issue**: On `shadow_transform` peer reject, `blockedUntilReset = true` (~896). `processPass` returns immediately at 960 if blocked **after** a reset in the same item—OK. But the **current** pass’s transform may already have been sent before reject on a prior item; subsequent queued passes are shifted and may hit blocked at 960 and return without sending transform, while queue drains—coverage loss. `enqueue` mismatch clears queue and pushes reset (~1028–1043)—good. Connection failure sets `requireResetReason` but not always `blockedUntilReset` (~898–902)—passes may retry without full resync.
- **Evidence**: 960 `if (state.blockedUntilReset) return`; 898–902 only sets `initialized = false` and `requireResetReason`.
- **Suggested Fix**: On connection failure, mirror peer-reject gating (`blockedUntilReset` until reset ack); after reset in `processPass`, optionally re-queue skipped pass.

## Finding 7: `SubcShadowTransport` global `pending` chain + 5s timeout + socket close
- **Severity**: medium
- **Location**: `shadow-sender.ts` `SubcShadowTransport` (~1089–1213, ~1392–1404)
- **Confidence**: medium
- **Issue**: All sessions share one serialized `pending` promise chain (~1115–1120)—slow session blocks others. `readTerminalFor` spins discarding non-matching frames until timeout (~1398–1404); on timeout, in-flight request fails, `send_failures` increment, but socket may still be half-open until daemon closes—next call may hit stale reader state. Socket `close` clears routes (~1176–1179) but does not reject in-flight `readTerminalFor` waiters explicitly beyond `closed` flag.
- **Evidence**: Single `this.pending` for all `call`; `REQUEST_TIMEOUT_MS = 5_000` (~47); no session-level isolation.
- **Suggested Fix**: Per-route or per-session transport queues; on timeout destroy socket and force reconnect; correlate in-flight abort.

## Finding 8: `buildMagicContextHookConfig` full spread — low regression risk if hook stays disciplined
- **Severity**: low
- **Location**: `create-session-hooks.ts` (~18–32)
- **Confidence**: high (for “no current bug”)
- **Issue**: Spread passes entire `MagicContextPluginConfig` into hook config. Extra keys are inert only if `createMagicContextHook` never iterates unknown keys. `project-security.ts` strips `shadow_transform` from **project** config (~234–238), so repo cannot arm shadow—good. No evidence spread breaks consumers; tests pin pass-through (~39–54 `create-session-hooks.test.ts`).
- **Evidence**: Comment 19–26; security strip 234–238.
- **Suggested Fix**: Optional: satisfy hook config type via `satisfies` / pick typed fields for documentation; not a soak blocker.

## Finding 9: Rust shadow isolation — binding checks present; live lane not written by shadow handlers
- **Severity**: low (verified controls)
- **Location**: `crates/mc-module/src/lib.rs` `shadow_binding` (~783–813), `handle_transform_value` (~1944–1949), `handle_agent_drops` (~1711–1715), shadow handlers (~2148+, ~2250+)
- **Confidence**: high
- **Issue**: Shadow ops require `shadow:<sid>` binding; plain transform rejected on shadow binding (`shadow_dispatch_enforces_shadow_route_precedence` ~7004–7030). Shadow transform uses `binding.session` (shadow id) and `shadow_project_path` (~2348–2351)—writes scoped to shadow session rows. Divergence recording does not touch live session id.
- **Evidence**: Tests at 7004–7030; `plain_transform_on_shadow_binding` / `shadow_binding_required`.
- **Suggested Fix**: Audit remaining dispatch arms for `is_shadow_session` if new ops added (only `ctx_reduce` found with `non_shadow_op_on_shadow_binding`).

## Finding 10: `apply_shadow_state_sync` seq gate — duplicate sync correctly rejects; not idempotent replay
- **Severity**: low
- **Location**: `crates/mc-store/src/lib.rs` (~2116–2119, ~2122–2124); test ~7105–7116
- **Confidence**: high
- **Issue**: Re-sending same `expected_shadow_seq` after success yields `shadow_seq_mismatch`—by design. Compartment upsert is `ON CONFLICT DO UPDATE` (~4002)—re-applying same seq generation is impossible without seq bump. Acceptable for observe-only soak.
- **Evidence**: `shadow_reset_and_state_sync_gate_generation_and_seq` duplicate sync test.

---

### False-positive filtering performed
- Read `packages/plugin/src/plugin/hooks/create-session-hooks.test.ts`, `shadow-sender.test.ts` (wire flattening, queue, exception on enqueue).
- Read `crates/mc-module/src/lib.rs` shadow tests (~7004–7146); no `PARITY.md` for shadow lane (only `packages/pi-plugin/PARITY.md` and smart-notes PARITY—out of scope).
- Confirmed prior fixes: `toFlatWireBody` (~799–801) matches Rust top-level serde; ordinal by-id fallback (~385–396) addresses input starvation; `tool_provider` route (~1133–1137).

---

## Summary
| Severity | Count |
|----------|-------|
| Critical | 1 |
| High | 3 |
| Medium | 4 |
| Low | 3 |

**Overall risk**: Shadow soak will run without affecting live **correctness** of transform output (enqueue errors caught in `transform.ts` ~2226–2228; shadow is separate route/store). However, **live latency** can regress with shadow enabled (Findings 3–4), and **shadow mirror correctness** is broken for real compartment sync and M0 mutations (Findings 1–2)—so divergence telemetry will be misleading noise until fixed.

**Verdict: HOLD** (do not treat production soak as authoritative parity evidence yet).

**Single most important reason**: **`state_sync` compartment JSON from TS does not deserialize into `ShadowCompartmentWire`**—once sessions sync compartments to Rust, shadow state is wrong or sync fails entirely, which defeats the soak’s purpose (Finding 1).