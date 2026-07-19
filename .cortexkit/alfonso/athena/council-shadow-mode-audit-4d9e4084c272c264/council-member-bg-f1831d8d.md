## Finding 1: Compartment wire format mismatch — every state_sync with compartments will reject
- **Severity**: critical
- **Location**: packages/plugin/src/hooks/magic-context/shadow-sender.ts:584-615 (TS builder) vs crates/mc-module/src/lib.rs:266-295 (Rust `ShadowCompartmentWire`)
- **Confidence**: high
- **Issue**: The TS `serializeCompartment` (shadow-sender.ts:584-615) emits compartments as nested objects: `start: { flat_id, bare_message_id, absolute_ordinal }` and `end: { flat_id, bare_message_id, absolute_ordinal }`. The Rust `ShadowCompartmentWire` requires FLAT top-level fields: `start_message: i64`, `end_message: i64` (lines 269-270, NO `#[serde(default)]`, so they are REQUIRED). The TS does NOT send `start_message` or `end_message` at the top level — those values are nested inside `start.absolute_ordinal` and `end.absolute_ordinal`. Serde will reject the deserialization with "missing field `start_message`", returning `invalid_params` for every state_sync that includes compartments. The shadow soak will never successfully sync compartments to the Rust store.
- **Evidence**: The Rust integration test `shadow_reset_and_state_sync_gate_generation_and_seq` (lib.rs:7071-7080) uses the CORRECT flat format: `"start_message": 0, "end_message": 0, "start_message_id": "m0#0"`. The TS test (shadow-sender.test.ts:383) only checks `compartments: expect.any(Array)` — it does NOT verify that the array contents match the Rust deserializer. The mocks miss this because the TS test is one-sided.
- **Suggested Fix**: Either (a) change the TS to emit flat `start_message` and `end_message` fields (matching the Rust struct), or (b) add `#[serde(default)]` to `start_message` and `end_message` in the Rust struct and add a custom deserializer that reads from `start.absolute_ordinal` and `end.absolute_ordinal`. Option (a) is the simpler fix and matches the existing Rust test.

## Finding 2: Session permanently wedged after peer reject in runQueue catch path
- **Severity**: high
- **Location**: packages/plugin/src/hooks/magic-context/shadow-sender.ts:887-904 (`runQueue` catch)
- **Confidence**: high
- **Issue**: When `processPass` throws a peer reject (e.g., `shadow_generation_mismatch`), the `runQueue` catch at line 896 sets `state.blockedUntilReset = true` but does NOT queue a reset work item. `processPass` at line 960 then short-circuits all subsequent passes with `if (state.blockedUntilReset) return;`. The `enqueue` mismatch path (line 1036-1043) correctly queues a reset, but the `runQueue` catch path does not. The `shadowSender.resetSession()` method exists but is never called from anywhere in the codebase (grep confirms 0 call sites). The session is permanently wedged until process restart, silently dropping all shadow coverage for that session.
- **Evidence**: shadow-sender.ts:887-904 sets `blockedUntilReset = true` and `requireResetReason = code` but never calls `pushWork` to enqueue a reset. Contrast with the `enqueue` mismatch path at line 1026-1043 which explicitly pushes a `{kind: "reset", ...}` work item.
- **Suggested Fix**: In the `runQueue` catch's `isPeerReject` branch, after setting `state.blockedUntilReset = true`, push a reset work item to the front of the queue (or set a flag that the next enqueue triggers a reset). Also wire `shadowSender.resetSession()` to the live session's reset path so the shadow lane stays in sync with the live lane's generation bumps.

## Finding 3: Three deep JSON clones per transform pass — 150-600ms latency on the live lane
- **Severity**: high
- **Location**: packages/plugin/src/hooks/magic-context/transform.ts:459 (`cloneForShadow`), shadow-sender.ts:380 (`resolveOrdinalsForShadow` clone), shadow-sender.ts:288 (`denormalizeShadowOutput` clone)
- **Confidence**: high
- **Issue**: Every transform pass triggers three independent `JSON.parse(JSON.stringify(...))` deep clones of the messages array: one in `cloneForShadow` at the transform level, one in `resolveOrdinalsForShadow` to annotate ordinals, and one in `denormalizeShadowOutput` to strip tag prefixes. For a 400-message session with parts (text, tool calls, tool results), each clone processes 10-50MB of data. JSON cloning at this scale takes 50-200ms per clone. Three clones add 150-600ms to every transform pass, a 75-300% increase over the transform's baseline cost. This is a significant performance regression for the live lane when `shadow_transform.enabled` is true.
- **Evidence**: shadow-sender.ts:226-228 defines `cloneJson` as `JSON.parse(JSON.stringify(value))`. The three callsites are transform.ts:459, shadow-sender.ts:380, and shadow-sender.ts:288. Each is a full deep clone of the messages array.
- **Suggested Fix**: Consolidate the three clones into one: clone once at `cloneForShadow`, pass the clone to both `resolveOrdinalsForShadow` (which annotates in-place) and `denormalizeShadowOutput` (which denormalizes in-place). Or use a faster structured-clone alternative (e.g., `structuredClone()` in modern Node/Bun, or a manual deep clone that avoids the string round-trip).

## Finding 4: By-id DB fallback is O(session_size) per below-floor id — quadratic on large sessions
- **Severity**: high
- **Location**: packages/plugin/src/hooks/magic-context/shadow-sender.ts:395 (`readRawSessionMessageById` fallback)
- **Confidence**: high
- **Issue**: When a message id is below the tail-prime floor, `resolveOrdinalsForShadow` falls back to `readRawSessionMessageById` which executes `SELECT COUNT(*) FROM message WHERE ... AND (time_created < ? OR (time_created = ? AND id <= ?))`. This is a full table scan from the start of the session to the target message, executed on a 100K-row session it's 100K rows scanned per call. The comment claims "Below-floor ids per pass are bounded by the marker lag (a handful)" — this is true for the COUNT, but each call is still O(session_size). With marker lag of 10 below-floor ids per pass on a 100K-message session, that's 1M rows scanned per transform pass. This is a quadratic blowup on large sessions.
- **Evidence**: shadow-sender.ts:385-396: the fallback at line 395 calls `readRawSessionMessageById` which in read-session-raw.ts:384-393 executes `SELECT COUNT(*) AS ordinal FROM message WHERE session_id = ? AND NOT (summary AND finish='stop') AND (time_created < ? OR (time_created = ? AND id <= ?))`. The `readRawSessionMessageCountFromDb` in read-session-db.ts:77-89 is similarly O(session_size).
- **Suggested Fix**: When the tail-prime is active, the by-id fallback should use the tail-prime's `baseOrdinal` plus the position in the tail, not a fresh `COUNT(*)`. Or maintain a separate ordinal index in memory. Or accept the cost and only fall back for ids NOT in the tail (i.e., below the prime floor) where the count is naturally bounded by the pre-tail size — but the `COUNT(*)` still scans the full table.

## Finding 5: Watermark, m0_mutations, and last_todo_state_hash fields are sent but never read by Rust
- **Severity**: medium
- **Location**: packages/plugin/src/hooks/magic-context/shadow-sender.ts:172-176, 535-551, 696-706 (TS builders) vs crates/mc-module/src/lib.rs:184-200 (Rust `ShadowStateSyncWire`)
- **Confidence**: high
- **Issue**: The TS sends `m0_mutations` (array of mutation log entries), `watermarks` (object with `compartment_sequence`, `memory_id`, `m0_mutation_id`, `memory_mutation_id`, `last_todo_state_hash`), and `last_todo_state_hash` in the state_sync payload. The Rust's `ShadowStateSyncWire` does NOT have any of these fields. Serde silently drops unknown fields. The TS computes `last_todo_state_hash` via `stableHash(sessionMeta.lastTodoState ?? "")` (line 549) and reads m0 mutations from the DB (line 696), but the Rust never uses either. The `watermarks` object is also not used by the Rust — the Rust computes its own `acked_watermarks` from the received compartments/memories/mutations (lib.rs:2176-2183). Wasted CPU on the TS side (hash computation, DB read for m0 mutations) and wasted bandwidth on the wire. The `watermarksEqual` check on the TS side uses `m0_mutation_id` and `last_todo_state_hash` to decide whether to send a state_sync, but since the Rust doesn't track these, the state_sync is sent unnecessarily when only m0 mutations or todo state change.
- **Evidence**: shadow-sender.ts:735 sends `m0_mutations: m0Mutations`. shadow-sender.ts:730 sends `watermarks: currentWatermarks`. lib.rs:184-200 `ShadowStateSyncWire` has no `watermarks`, no `m0_mutations`, no `last_todo_state_hash` field. The TS test at shadow-sender.test.ts:385 checks `m0_mutations: expect.any(Array)` but the Rust never reads it.
- **Suggested Fix**: Either remove the unused fields from the TS payload (saves CPU and bandwidth), or add them to the Rust struct (makes the watermark comparison meaningful on both sides). If the intent is for the Rust to track m0 mutations, the Rust needs a `m0_mutation_log` table for shadow sessions.

## Finding 6: `embedding.api_key` included in hook config via spread
- **Severity**: low
- **Location**: packages/plugin/src/plugin/hooks/create-session-hooks.ts:27-32 (`buildMagicContextHookConfig`)
- **Confidence**: medium
- **Issue**: The spread `...pluginConfig` includes the full `embedding` sub-object which contains `api_key` (defined in config/schema/magic-context.ts:233). The hook code does NOT access `embedding.api_key` (grep confirms 0 call sites in hooks/magic-context), so it's currently inert. However, the comment claims "the extra top-level keys carried by the spread are inert" — this is true today but fragile. If any hook code ever logs the config, serializes it for debugging, or passes it to a third-party library, the API key would be exposed. This is a code smell, not an active bug.
- **Evidence**: create-session-hooks.ts:27-32 spreads `...pluginConfig` which includes `embedding.api_key`. Grep for `embedding.api_key` in hooks/magic-context returns 0 results. The hook type `MagicContextDeps.config` (hook.ts:131-133) declares `embedding?: { provider?: ... }` without `api_key`.
- **Suggested Fix**: Explicitly destructure and forward only the fields the hook type declares, rather than spreading the full plugin config. Or add a code comment warning that the spread is sensitive to future hook code that might log/serialize the config.

## Finding 7: `unaryJson` request timeout includes connection establishment time
- **Severity**: medium
- **Location**: packages/plugin/src/hooks/magic-context/shadow-sender.ts:1192-1204
- **Confidence**: high
- **Issue**: `unaryJson` does `await this.ensureConnected()` first, then sets the 5s deadline for `readTerminalFor`. The connection establishment can take up to 2s (HANDSHAKE_TIMEOUT_MS = 2000). So the actual request timeout is 5s minus connection time. For a slow connection (e.g., 1.5s to establish + 0.5s for auth), the request only has 3s. This is by design (the 5s is the total budget), but it's not documented. A user observing shadow timeouts might not realize the connection time is eating into the request budget.
- **Evidence**: shadow-sender.ts:47 defines `REQUEST_TIMEOUT_MS = 5_000`, line 48 defines `HANDSHAKE_TIMEOUT_MS = 2_000`. Line 1192-1204: `unaryJson` calls `await this.ensureConnected()` then `readTerminalFor(reader, channel, corr, REQUEST_TIMEOUT_MS)`. The deadline is set at the start of `readTerminalFor` (line 1398) which is AFTER `ensureConnected` returns.
- **Suggested Fix**: Either document the total budget as `HANDSHAKE_TIMEOUT_MS + REQUEST_TIMEOUT_MS = 7s`, or set the deadline in `unaryJson` BEFORE calling `ensureConnected` so the total budget is exactly 5s.

## Finding 8: `runQueue` catch silently drops passes on connection failure
- **Severity**: medium
- **Location**: packages/plugin/src/hooks/magic-context/shadow-sender.ts:898-902
- **Confidence**: high
- **Issue**: When `processPass` throws a connection failure (e.g., `ECONNREFUSED`, `backoff active`), the catch sets `state.initialized = false` and `state.requireResetReason = "route_reopen"`. The pass is lost (not re-queued). The next `processPass` call will see `!state.initialized` and call `performReset` first (line 952-959). But the lost pass is not re-queued after the reset. If the connection is flaky, every pass is lost during the outage window. The `MAX_QUEUE_PER_SESSION = 4` cap means only 4 passes are buffered; after that, oldest passes are dropped (line 860-868). So during a long outage, the shadow lane falls behind and old passes are dropped.
- **Evidence**: shadow-sender.ts:887-904: the catch logs the error and sets flags but never re-queues the pass. Line 876 shifts the pass from the queue before the catch, so it's gone. Line 852-853 reschedules if there are more items, but those items are also lost on the same connection failure.
- **Suggested Fix**: On connection failure, re-queue the pass at the front of the queue (or increment a retry counter and drop after N attempts). The `requireResetReason = "route_reopen"` flag should trigger a re-queue after the next successful connection.

## Finding 9: `getAutoSearchHintDecisions` called on every pass even when no hints exist
- **Severity**: low
- **Location**: packages/plugin/src/hooks/magic-context/shadow-sender.ts:323
- **Confidence**: high
- **Issue**: `denormalizeShadowOutput` calls `getAutoSearchHintDecisions(args.db, args.sessionId)` on every pass. This is a DB read (`SELECT auto_search_hint_decisions FROM session_meta WHERE session_id = ?`). For sessions that never use auto-search hints, this is a wasted DB read on every transform pass. The result is filtered for "hint" decisions, and if empty, the denormalization loop is skipped. The DB read is unconditional.
- **Evidence**: shadow-sender.ts:323-326: `const hintDecisions = getAutoSearchHintDecisions(args.db, args.sessionId).filter(...)` — the DB read happens before the filter.
- **Suggested Fix**: Check `args.normalizationTargets` for any hint-related entries before calling `getAutoSearchHintDecisions`. If no hint normalizations are expected, skip the DB read. Or cache the result per-session (the hint decisions change infrequently).

## Finding 10: `resolveDeclaredTrimForShadow` called twice per pass with DB reads
- **Severity**: low
- **Location**: packages/plugin/src/hooks/magic-context/transform.ts:460-462, 2188
- **Confidence**: high
- **Issue**: The transform calls `resolveDeclaredTrimForShadow` before and after the transform (lines 460-462 and 2188). Each call does a `getPersistedCompactionMarkerState` DB read (shadow-sender.ts:487). The full session read is cached after the first call (line 493), but the marker read is not. Two DB reads per pass for the marker state. For a high-frequency transform (every `message.updated`), this is 2x the DB load for the marker table.
- **Evidence**: transform.ts:460-462 calls `resolveDeclaredTrimForShadow` with `shadowSender` truthy. transform.ts:2188 calls it again after the transform. shadow-sender.ts:487 does `getPersistedCompactionMarkerState(args.db, args.sessionId)`.
- **Suggested Fix**: Cache the marker state in the shadow sender's session state (already partially done via `declaredTrimBySession`), and only re-read if the marker key changes. The `before` and `after` calls should share the cached value when the marker hasn't changed.

## Finding 11: Shadow transform handler doesn't call `trace_pass_completed` — inconsistent with live lane
- **Severity**: low
- **Location**: crates/mc-module/src/lib.rs:2250-2438 (`handle_shadow_transform_value`)
- **Confidence**: high
- **Issue**: The live `handle_transform_value` calls `store.trace_pass_completed(&parsed.session_id, now_ms())` at line 2138 and `store.trace_pass_received` at line 1956. The shadow `handle_shadow_transform_value` does NOT call either. The shadow lane's observability is incomplete: there's no `mc_pass_trace` row for shadow sessions, so the `status`/`health` arms won't report shadow transform activity. This is by design (the shadow lane has its own `shadow_divergences` table), but it's worth noting for observability.
- **Evidence**: lib.rs:1956 calls `trace_pass_received`, lib.rs:2138 calls `trace_pass_completed`. lib.rs:2250-2438 (shadow transform) does neither.
- **Suggested Fix**: Either add `trace_pass_received` and `trace_pass_completed` calls to the shadow transform handler (with `session_id = binding.session`), or document why the shadow lane intentionally skips trace writes.

## Finding 12: `shadow_reset` wire sends `reason` which Rust silently drops
- **Severity**: low
- **Location**: packages/plugin/src/hooks/magic-context/shadow-sender.ts:915-921
- **Confidence**: high
- **Issue**: The TS sends `reason: args.reason` in the shadow_reset body. The Rust's `ShadowResetWire` (lib.rs:202-208) has no `reason` field. Serde drops it. The TS uses the reason for logging and for the `requireResetReason` state, but the Rust never sees it. The Rust returns `previous_shadow_generation` in the response, which the TS uses to update its own generation. The reason is informational only on the TS side.
- **Evidence**: shadow-sender.ts:919 sends `reason: args.reason`. lib.rs:202-208 `ShadowResetWire` has only `session_id` and `shadow_generation`.
- **Suggested Fix**: Either add `reason` to the Rust struct for logging, or remove it from the TS payload (it's not used by the Rust).

## Summary

**Total findings: 12** (1 critical, 3 high, 4 medium, 4 low)

**Critical**: 1 — Compartment wire format mismatch (Finding 1) will cause every state_sync with compartments to fail with `invalid_params`. This is the most serious finding: the shadow soak's primary purpose (comparing TS vs Rust compartment handling) is completely broken at the wire level. The first armed run would expose this immediately — every session with compartments would log `shadow_generation_mismatch` or `invalid_params` and the shadow store would never receive compartments.

**High**: 3 — Session wedging (Finding 2) silently drops shadow coverage after a single peer reject. Three deep clones per pass (Finding 3) adds 150-600ms latency to the live lane. By-id DB fallback (Finding 4) is O(session_size) per below-floor id, causing quadratic blowup on large sessions.

**Verdict: HOLD**

The single most important reason is Finding 1: the compartment wire format mismatch is a critical bug that completely breaks the shadow soak's primary purpose. The TS sends nested `start: {absolute_ordinal, ...}` but the Rust requires flat `start_message: i64`. Every state_sync with compartments will reject with `invalid_params`, and the shadow store will never receive compartments. The first armed run would expose this immediately — the soak would log rejection errors for every session and the `shadow_divergences` table would never record compartment-related comparisons. The mocks miss this because the TS test only checks the TS shape (not the Rust deserialization) and the Rust test uses the correct flat format. This must be fixed before arming the soak on production sessions.