## Finding 1: Shadow capture adds uncaught and unbounded synchronous work to the live transform
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/transform.ts:177-179, 458-462, 2180-2228`; `shadow-sender.ts:282-358, 361-409`; `read-session-raw.ts:76-89, 139-143`
- **Confidence**: high
- **Issue**: Enabling an observe-only feature can fail or materially delay the user’s real prompt.
- **Evidence**: Before the transform’s normal work, `cloneForShadow(messages)` and `resolveDeclaredTrimForShadow(...)` execute outside the later shadow `try/catch` (`transform.ts:458-462`). A SQLite or serialization exception therefore rejects the live transform. At enqueue, the sender synchronously clones both input and output and reads hint decisions. Ordinal resolution performs a full raw-session read, whose implementation selects every message and every part; its own documentation states this grows unbounded on long sessions. The completion timer is logged before enqueue (`2180-2184`), hiding this overhead. The by-ID helper does use a cached read-only handle (`read-session-db.ts:40-69`), so per-call handle opening is not the problem.
- **Suggested Fix**: Wrap the initial capture in a fail-open shadow guard; replace the full-session read with one bounded batch query for only input IDs; snapshot only fields that can mutate; cap or skip shadow comparison above a byte/message budget; include shadow preparation in latency telemetry.

## Finding 2: Compartment state-sync rows cannot deserialize on the Rust side
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts:564-615`; `crates/mc-module/src/lib.rs:266-295, 2148-2152`
- **Confidence**: high
- **Issue**: Once a session has a new compartment to synchronize, `state_sync` rejects and no shadow transform is run.
- **Evidence**: TypeScript emits nested objects:
  - `start: { flat_id, bare_message_id, absolute_ordinal }`
  - `end: { flat_id, bare_message_id, absolute_ordinal }`

  Rust requires top-level `start_message`, `end_message`, `start_message_id`, and `end_message_id`; the first two are mandatory. Serde therefore reports missing fields. The sender does not classify `invalid_params` as a peer/CAS rejection, so it repeatedly logs failures rather than correcting the lineage.
- **Suggested Fix**: Emit the exact Rust shape using `compartment.startMessage`, `endMessage`, `startMessageId`, and `endMessageId`, or change both sides to a shared nested schema. Add a cross-language test that serializes a real TS compartment and deserializes it as `ShadowStateSyncWire`.

## Finding 3: M0 mutations and watermarks are silently discarded
- **Severity**: high
- **Location**: `shadow-sender.ts:165-176, 696-738, 993-999`; `crates/mc-module/src/lib.rs:184-200, 2161-2193`; `crates/mc-store/src/lib.rs:2076-2155`; `inject-compartments.ts:1119-1129`
- **Confidence**: high
- **Issue**: Destructive compartment changes can silently leave Rust shadow state stale while TypeScript believes synchronization succeeded.
- **Evidence**: TypeScript sends `m0_mutations` and `watermarks`. `ShadowStateSyncWire` declares neither; it declares `acked_watermarks` instead and has no M0 mutation type. Unknown fields are silently ignored. The store transaction applies only compartments, memories, and memory mutations. Nevertheless, after a successful response the sender advances `lastAckedWatermarks`, including `m0_mutation_id`, so those mutations are never resent. This is byte-affecting: TS explicitly treats `max_mutation_id` changes from delete/merge/recomp as a HARD materialization trigger (`inject-compartments.ts:1121-1129`).
- **Suggested Fix**: Align `watermarks`/`acked_watermarks`; add an M0 mutation wire/store representation that applies delete/merge/recomp semantics, or force a generation reset on any M0 mutation. Temporarily enable `deny_unknown_fields` in integration tests to catch drift.

## Finding 4: Rust uses a different byte-affecting history budget than the TS pass
- **Severity**: high
- **Location**: `transform.ts:877-884, 2235-2281`; `magic-context.ts:21`; `crates/mc-module/src/lib.rs:103-107, 2350-2357, 2937-2940`; `crates/mc-module/src/memory_render.rs:29-30`
- **Confidence**: high
- **Issue**: Long histories will generate false byte divergences even when both implementations are otherwise correct.
- **Evidence**: TS derives the history budget from context limit × execute threshold × `history_budget_percentage`, whose default is `0.15`. Rust freezes every binding to `60_000` tokens; the source comment explicitly says reading it from config is a later refinement. Neither `ShadowPassInputs` wire type carries the TS-derived budget. For a 200K context at the default 65% threshold, TS uses 19,500 tokens while Rust uses 60,000, producing different decay tiers once history crosses the smaller limit.
- **Suggested Fix**: Include the exact resolved `history_budget_tokens` in each shadow pass and use it in Rust. Do the same for every other byte-affecting resolved budget rather than independently recomputing defaults.

## Finding 5: Workspace memory inputs rendered by TS are absent from shadow synchronization
- **Severity**: high
- **Location**: `inject-compartments.ts:1524-1583`; `shadow-sender.ts:659-724`; `crates/mc-store/src/lib.rs:3329-3334`; `crates/mc-module/src/lib.rs:297-346`
- **Confidence**: high
- **Issue**: Workspaced sessions compare different memory baselines and will false-diverge/quarantine.
- **Evidence**: Live TS materialization resolves the workspace and calls `getMemoriesByProjects` over expanded identities with share-category filtering. Shadow sync calls only `getMemoriesByProject` for the owning project and scopes memory mutations to that same single path. Rust explicitly returns no workspace membership for a `shadow:` project. Furthermore, TS emits `project_path` on memory rows, but `ShadowMemoryWire` has no such field, so source identity would be lost even if foreign rows were added.
- **Suggested Fix**: Mirror the exact visible workspace memory set and its source/display attribution, plus workspace-sensitive watermarks and mutations. Add a shadow integration test based on the existing workspace-memory render fixtures.

## Finding 6: Failed authentication leaks the locally created socket
- **Severity**: medium
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts:1158-1188`
- **Confidence**: high
- **Issue**: Repeated handshake failures can leak file descriptors in the live plugin process.
- **Evidence**: `ensureConnected` creates a local `socket` and authenticates it before assigning `this.socket`. If authentication times out or proof validation fails, the catch block destroys only `this.socket`, which is still null; the local socket is not closed. Backoff limits frequency but does not eliminate the leak.
- **Suggested Fix**: Keep the candidate socket outside the `try` scope and destroy it on every pre-install failure. Add real socket tests for proof rejection, handshake timeout, mid-frame close, and request timeout; current sender tests use only `FakeTransport`.

## Summary
**Findings: 0 critical, 5 high, 1 medium, 0 low.**

**Verdict: HOLD.** The most important reason is live-lane safety: enabling shadow mode introduces uncaught and unbounded synchronous capture work into the real transform. Independently, the current compartment wire mismatch means meaningful sessions lose shadow coverage as soon as compartments exist.

False-positive checks performed:
- The ordinal by-ID fallback uses the same valid-row ordering and summary exclusion as the full reader; its DB handle is cached, so I did not report a basis or handle-open bug.
- Shadow handlers enforce `shadow:` bindings, and the plain transform path rejects shadow bindings; no direct live-row isolation breach was found.
- The `tool_provider` route kind matches the Rust manifest.
- The full config spread carries extra runtime keys, but I found no serialization/logging sink for credentials and did not count it as a leak.
- Neither PARITY document declares these shadow differences intentional.
- TS tests validate TS-produced shapes against a fake transport, while Rust tests hand-construct Rust-native shapes; no cross-language serde test covers these failures.