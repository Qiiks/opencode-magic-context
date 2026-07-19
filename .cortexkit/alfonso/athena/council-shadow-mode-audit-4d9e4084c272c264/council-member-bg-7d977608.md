## Finding 1: CRITICAL — Compartment wire-shape mismatch: `serializeCompartment` emits nested `start`/`end` objects but Rust `ShadowCompartmentWire` requires flat `start_message`/`end_message` fields, causing EVERY state_sync with compartments to reject

- **Severity**: critical
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts:584-616` (TS emitter) vs `crates/mc-module/src/lib.rs:267-295` (Rust parser)
- **Confidence**: high
- **Issue**: The TS `serializeCompartment` emits compartments with nested `start: { flat_id, bare_message_id, absolute_ordinal }` and `end: { flat_id, bare_message_id, absolute_ordinal }` objects. The Rust `ShadowCompartmentWire` expects flat `start_message: i64` and `end_message: i64` fields (REQUIRED, no `#[serde(default)]`), plus optional `start_message_id: String` and `end_message_id: String`. Since serde ignores unknown fields (no `deny_unknown_fields`), the nested `start`/`end` objects are silently dropped, and the required `start_message`/`end_message` fields are missing → `serde_json::from_value` fails with `missing field start_message`. The handler at `lib.rs:2149-2151` returns `invalid_params_error`. This triggers a peer-reject → reset loop on the TS side (`shadow-sender.ts:891-896`), permanently wedging the shadow lane for any session with compartments.
- **Evidence**: 
  - TS emits: `{ id, sequence, start: { flat_id, bare_message_id, absolute_ordinal }, end: { flat_id, bare_message_id, absolute_ordinal }, title, content, p1, p2, p3, p4, importance, episode_type, legacy, created_at }` (shadow-sender.ts:584-615)
  - Rust expects: `{ sequence: i64, start_message: i64, end_message: i64, start_message_id: String, end_message_id: String, title, content, p1-p4, importance, episode_type, legacy, created_at }` (lib.rs:267-295)
  - `start_message` and `end_message` have NO `#[serde(default)]` → required → deserialization fails when absent
  - The test at `shadow-sender.test.ts:350-447` validates the TS wire shape against a `FakeTransport` that accepts ANY shape — it never sends to the real Rust parser. The Rust store tests (`lib.rs:6080-6166`) use `StoredCompartment` directly, never testing serde deserialization from the TS wire shape.
  - The `real_daemon.rs` integration test does NOT exercise shadow ops at all (no `shadow` or `state_sync` references in `crates/mc-module/tests/`).
- **Suggested Fix**: Align the TS `serializeCompartment` to emit flat fields matching the Rust struct: `start_message: startOrdinal, end_message: endOrdinal, start_message_id: args.compartment.startMessageId, end_message_id: args.compartment.endMessageId`. Remove the nested `start`/`end` objects. Add an integration test that round-trips a compartment through the real Rust serde parser.

## Finding 2: HIGH — `watermarks` vs `acked_watermarks` field name mismatch: TS sends `watermarks` but Rust reads `acked_watermarks`, silently losing the TS-computed watermarks

- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts:732` (TS emits `watermarks`) vs `crates/mc-module/src/lib.rs:199` (Rust expects `acked_watermarks`)
- **Confidence**: high
- **Issue**: The TS `buildStateSyncPayload` emits `watermarks: currentWatermarks` in the state_sync params (shadow-sender.ts:732). The Rust `ShadowStateSyncWire` has `#[serde(default)] acked_watermarks: Option<Value>` (lib.rs:198-199) — the field is named `acked_watermarks`, not `watermarks`. Since serde ignores unknown fields, the TS `watermarks` field is silently dropped, and `acked_watermarks` defaults to `None`. The Rust handler then computes a fallback (lib.rs:2176-2183) from the compartment/memory max IDs. The carefully computed TS watermarks (which track `compartment_sequence`, `memory_id`, `m0_mutation_id`, `memory_mutation_id`, `last_todo_state_hash`) are silently discarded. The fallback uses different key names (`compartment_seq` vs `compartment_sequence`, `memory_mutation_id` matches, `last_todo_state` as boolean vs hash string).
- **Evidence**: 
  - TS: `watermarks: currentWatermarks` (shadow-sender.ts:732) where `currentWatermarks` has keys `compartment_sequence, memory_id, m0_mutation_id, memory_mutation_id, last_todo_state_hash`
  - Rust: `#[serde(default)] acked_watermarks: Option<Value>` (lib.rs:199) — field name mismatch
  - Rust fallback: `json!({ "compartment_seq": ..., "memory_id": ..., "memory_mutation_id": ..., "last_todo_state": bool })` (lib.rs:2176-2183) — different shape than TS watermarks
  - The stored `shadow_acked_watermarks` is only written, never read for logic (grep confirms only 3 references: field decl, write in state_sync, clear in reset), so the impact is limited to stored metadata being wrong, not affecting the transform comparison. But it defeats the purpose of tracking watermarks for potential future use.
- **Suggested Fix**: Rename the TS field from `watermarks` to `acked_watermarks` to match the Rust struct, OR rename the Rust field to `watermarks`. Ensure the watermark value shape matches what the Rust side expects.

## Finding 3: MEDIUM — `m0_mutations` field silently dropped: TS sends m0_mutations in state_sync but Rust `ShadowStateSyncWire` has no such field

- **Severity**: medium
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts:174,735` (TS emits `m0_mutations`) vs `crates/mc-module/src/lib.rs:185-200` (Rust `ShadowStateSyncWire` has no `m0_mutations` field)
- **Confidence**: high
- **Issue**: The TS `ShadowStateSyncPayload` includes `m0_mutations: unknown[]` (shadow-sender.ts:174) and `buildStateSyncPayload` populates it (shadow-sender.ts:696-706, 735). The Rust `ShadowStateSyncWire` has no `m0_mutations` field — only `compartments`, `memories`, `memory_mutations`, `last_todo_state`, `acked_watermarks`. The `m0_mutations` array is silently dropped by serde. The Rust shadow store never receives m0 mutation data. This is likely a design choice (the Rust shadow transform computes its own m0 state), but the TS side tracks `m0_mutation_id` in watermarks (shadow-sender.ts:160,547) and sends m0_mutations for incremental sync — all of which is wasted work. The TS test at `shadow-sender.test.ts:385` asserts `m0_mutations: expect.any(Array)` exists in the payload, validating a field that the Rust side silently discards.
- **Evidence**: 
  - TS: `m0_mutations: m0Mutations` (shadow-sender.ts:735) where `m0Mutations` comes from `getM0MutationsAfterId` (line 696-706)
  - Rust: `ShadowStateSyncWire` fields are `session_id, shadow_generation, expected_shadow_seq, compartments, memories, memory_mutations, last_todo_state, acked_watermarks` (lib.rs:185-200) — no `m0_mutations`
  - The `apply_shadow_state_sync` store method (lib.rs:2079-2172) has no m0_mutations parameter
- **Suggested Fix**: If m0_mutations are not needed on the Rust side, remove the TS-side computation to avoid wasted DB reads and serialization. If they ARE needed, add an `m0_mutations` field to `ShadowStateSyncWire` and wire it through to the store.

## Finding 4: MEDIUM — Socket reuse after request timeout corrupts frame alignment: `SubcShadowTransport` does not destroy the socket on read timeout, leaving stale partial data in the `SocketReader` buffer

- **Severity**: medium
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts:1192-1213` (`unaryJson`) and `1275-1305` (`SocketReader.readExact`)
- **Confidence**: high
- **Issue**: When `readTerminalFor` times out (REQUEST_TIMEOUT_MS = 5s, line 1204), the error propagates out of `unaryJson` but the socket is NOT destroyed. The `SocketReader.readExact` throws on timeout WITHOUT consuming partial data from `this.chunks` (the copy block at lines 1293-1304 is only reached after the while loop at line 1277 completes successfully). The next `ensureConnected` check (line 1159: `if (this.socket && !this.socket.destroyed && this.reader) return`) sees the socket as alive and reuses it. The next `readFrame` call reads a header from stale partial data, causing frame misalignment. All subsequent shadow passes fail with frame parse errors until the server closes the socket (triggering the `close` handler at line 1176-1180 which clears the socket).
- **Evidence**: 
  - `readExact` timeout: line 1280-1284 throws `"read timeout"` without consuming buffer data
  - `unaryJson` does NOT destroy socket on error: lines 1192-1213 — no cleanup in catch/throw path
  - `ensureConnected` reuses stale socket: line 1159 checks `!this.socket.destroyed` — a timed-out socket is not destroyed
  - The `socket.once("close", ...)` handler (line 1176) only fires on server-side close, not on client-side timeout
  - Blast radius: shadow lane only (live session unaffected), but corrupts all shadow coverage until server closes connection
- **Suggested Fix**: On any error from `readTerminalFor` (timeout, connection closed, unexpected frame type), destroy the socket and null out `this.socket`/`this.reader` so `ensureConnected` reconnects cleanly. Add `socket.destroy()` in a `finally` block or in the error path of `unaryJson`.

## Finding 5: MEDIUM — `cloneJson` on potentially huge message arrays in the transform hot path: two `JSON.parse(JSON.stringify(...))` round-trips per pass with no bounding

- **Severity**: medium
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts:226-228` (`cloneJson`), `380` (`resolveOrdinalsForShadow`), `288` (`denormalizeShadowOutput`); called from `enqueue` at lines 1018 and 1054
- **Confidence**: high
- **Issue**: `enqueue` runs synchronously inside the transform hot path (transform.ts:2205). It calls `resolveOrdinalsForShadow` which does `cloneJson(args.messages)` (line 380) and `denormalizeShadowOutput` which does `cloneJson(args.outputMessages)` (line 288). Both use `JSON.parse(JSON.stringify(value))` — two full JSON serialization round-trips on the ENTIRE message array per pass. For a 400-message session with large tool outputs, each clone can take 50-200ms, adding 100-400ms of synchronous stall to the user's real prompt. The `enqueue` call is wrapped in try/catch (transform.ts:2226), so a throw is safe, but a multi-second stall delays the real prompt without throwing. There is no size bound or skip-when-large guard.
- **Evidence**: 
  - `cloneJson` at line 226-228: `JSON.parse(JSON.stringify(value))` — no size check
  - `resolveOrdinalsForShadow` line 380: `cloneJson(args.messages)` — clones ALL input messages
  - `denormalizeShadowOutput` line 288: `cloneJson(args.outputMessages)` — clones ALL output messages
  - `enqueue` is called from transform.ts:2205, synchronously in the hot path
  - The test at shadow-sender.test.ts:494-514 verifies enqueue doesn't THROW, but doesn't measure latency
- **Suggested Fix**: Add a message count or estimated byte size guard: skip shadow enqueue when the array exceeds a threshold (e.g., >200 messages or >5MB serialized). Alternatively, use structuredClone (faster) or defer the clone to the async queue worker (runQueue) rather than doing it synchronously in `enqueue`.

## Finding 6: LOW — `handle_guidance_value` does not reject shadow bindings, allowing guidance reads on shadow routes

- **Severity**: low
- **Location**: `crates/mc-module/src/lib.rs:1730-1779` (`handle_guidance_value`)
- **Confidence**: high
- **Issue**: `handle_guidance_value` uses `resolve_binding` (line 1741) but does NOT check `is_shadow_session` to reject shadow bindings, unlike `handle_agent_drops_value` (line 1711) and `handle_transform_value` (line 1944). A shadow route could call `guidance.get` and receive guidance text. This is read-only (returns text bytes), so it cannot corrupt shadow state, but it's an isolation gap — shadow routes should only accept shadow ops.
- **Evidence**: 
  - `handle_agent_drops_value` has guard: line 1711 `if is_shadow_session(&binding.session) { return non_shadow_op_on_shadow_binding }`
  - `handle_transform_value` has guard: line 1944 `if is_shadow_session(&binding.session) { return plain_transform_on_shadow_binding }`
  - `handle_guidance_value` has NO guard: line 1741 only calls `resolve_binding` with no `is_shadow_session` check
- **Suggested Fix**: Add `if is_shadow_session(&binding.session) { return HandlerOutcome::Error { code: "non_shadow_op_on_shadow_binding", ... } }` after the `resolve_binding` call in `handle_guidance_value`.

## Finding 7: LOW — Read-only OpenCode session DB has no busy_timeout configured, risking SQLITE_BUSY on the shadow hot path

- **Severity**: low
- **Location**: `packages/plugin/src/hooks/magic-context/read-session-db.ts:63` (`new Database(dbPath, { readonly: true })`)
- **Confidence**: medium
- **Issue**: The cached read-only OpenCode session DB is opened without a `busy_timeout` pragma. The `readRawSessionMessageById` fallback (shadow-sender.ts:395) and `readRawSessionMessages` (via `withRawSessionMessageCache`) both read from this DB synchronously in the `enqueue` hot path. If OpenCode's writer is holding a lock (e.g., during a WAL checkpoint), the read-only connection can get SQLITE_BUSY. The throw propagates out of `enqueue` and is caught by transform.ts:2226, so the pass is skipped safely. But under heavy write load, the shadow lane could lose coverage silently. The Magic Context DB has a busy_timeout (storage-db.ts:354), but the read-only session DB does not.
- **Evidence**: 
  - `getReadOnlySessionDb` at line 56-66: `new Database(dbPath, { readonly: true })` — no `PRAGMA busy_timeout`
  - Magic Context DB for comparison: storage-db.ts:354 installs busy_timeout BEFORE WAL
  - The `readRawSessionMessageById` fallback is called per below-floor message (shadow-sender.ts:395), which is bounded by marker lag but still on the hot path
- **Suggested Fix**: Add `db.exec("PRAGMA busy_timeout = 2000")` after opening the read-only connection, or wrap the read in a retry with a short backoff.

## Summary

**Total findings: 7** — Critical: 1, High: 1, Medium: 3, Low: 2

**False-positive filtering performed:**
- Checked `packages/pi-plugin/PARITY.md` (730 lines) — no shadow-related entries; all divergences are Pi↔OpenCode mechanism differences, not shadow wire shape
- Checked `packages/plugin/src/features/magic-context/smart-notes/PARITY.md` — SSRF guard parity only, irrelevant to shadow
- Checked `docs/AUDIT-KNOWN-ISSUES.md` — no shadow or wire-shape entries
- Checked `shadow-sender.test.ts` — tests use `FakeTransport` that accepts ANY wire shape; never validates against real Rust serde parser
- Checked `crates/mc-store/src/lib.rs` store tests — use `StoredCompartment` directly, never test serde deserialization from TS wire
- Checked `crates/mc-module/tests/real_daemon.rs` — no shadow ops exercised
- Verified ordinal basis consistency: both `readRawSessionMessagesFromDb` (index+1 after summary filter) and `readRawSessionMessageByIdFromDb` (COUNT with same summary filter and sort order) use the same basis — no basis mismatch
- Verified shadow isolation: `shadow_binding` rejects non-shadow bindings (lib.rs:807), plain transform rejects shadow bindings (lib.rs:1944), ctx_reduce rejects shadow bindings (lib.rs:1711) — except guidance (Finding 6)
- Verified shadow capture safety: `cloneForShadow(messages)` is taken BEFORE transform (transform.ts:459), `denormalizeShadowOutput` deep-clones output before mutating (shadow-sender.ts:288) — live array not mutated
- Verified no double-clone: `enqueue` pre-computes `shadowTsOutput`/`shadowNormalizations` (lines 1054-1066), `buildShadowTransformBody` reuses them (lines 807-812) — no second clone

**Overall risk assessment**: The compartment wire-shape mismatch (Finding 1) is the same class of bug that the first armed run exposed — a wire-shape mismatch that mocks miss because the `FakeTransport` accepts any shape. It will cause EVERY state_sync with compartments to reject, permanently wedging the shadow lane in a reset loop. This completely defeats the soak's purpose for any session that has compartments (which is the primary use case). The `watermarks` mismatch (Finding 2) and `m0_mutations` drop (Finding 3) are silent data losses that don't crash but corrupt the shadow state's metadata.

**Verdict: HOLD** — The single most important reason is Finding 1: the compartment wire-shape mismatch will cause every shadow state_sync with compartments to fail with `invalid_params: missing field start_message`, wedging the shadow lane in a reset loop. This is the exact "wire-shape mismatch the mocked tests missed" class that the audit brief warns about, and it's untested because no integration test sends TS-built compartments through the real Rust serde parser. The soak cannot observe anything useful until this is fixed and verified with an end-to-end test.