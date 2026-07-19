---
task_id: bg_f8f752bc
agent: council-member
session_id: ses_0b4657e07ffeOHiCGfdEIhakWC
parent_session_id: ses_0b4664e56ffeKx8uRSceXLl3v9
status: completed
completed_at: 2026-07-10T11:19:57Z
---

[assistant] 2026-07-10T10:58:54Z
**Inspecting full config schema**

<!-- -->

[assistant] 2026-07-10T10:59:24Z

[assistant] 2026-07-10T10:59:35Z

[assistant] 2026-07-10T11:00:03Z
**Investigating API key leakage in hook config**

<!-- -->

[assistant] 2026-07-10T11:00:11Z
**Assessing line outputs and mapper history**

<!-- -->

[assistant] 2026-07-10T11:00:18Z
**Assessing config spread impact on consumers**

<!-- -->
**Reviewing config field usage in hook functions**

<!-- -->

[assistant] 2026-07-10T11:00:24Z
**Investigating credential exposure boundaries**

<!-- -->

[assistant] 2026-07-10T11:00:29Z

[assistant] 2026-07-10T11:00:34Z
**Inspecting shadow_input_messages conversion**

<!-- -->

[assistant] 2026-07-10T11:00:42Z
**Analyzing state sync rejection causes**

<!-- -->
**Diagnosing queue reset and blocking logic**

<!-- -->
**Identifying stale state from rapid session reuse**

<!-- -->
**Identifying reset race condition risk**

<!-- -->
**Highlighting unhandled reset promise rejection**

<!-- -->

[assistant] 2026-07-10T11:01:07Z

[assistant] 2026-07-10T11:01:09Z
**Assessing session reset and revert behavior**

<!-- -->

[assistant] 2026-07-10T11:01:16Z

[assistant] 2026-07-10T11:01:24Z
**Identifying missing reset on revert**

<!-- -->
**Assessing resetSession wiring and revert handling**

<!-- -->

[assistant] 2026-07-10T11:01:38Z

[assistant] 2026-07-10T11:01:41Z
**Analyzing shadow reset and ordinal mismatch effects**

<!-- -->
**Identifying compartment truncation bug in state sync**

<!-- -->
**Planning inspection of raw summary row reads**

<!-- -->

[assistant] 2026-07-10T11:01:55Z

[assistant] 2026-07-10T11:02:04Z
**Investigating async withRawSessionMessageCache usage**

<!-- -->

[assistant] 2026-07-10T11:02:08Z
**Searching for transformWithRawCache usage**

<!-- -->

[assistant] 2026-07-10T11:02:11Z
**Clarifying cache read and fallback logic**

<!-- -->
**Planning to inspect DB ordinal implementations**

<!-- -->

[assistant] 2026-07-10T11:02:28Z

[assistant] 2026-07-10T11:02:32Z
**Planning complete reader extraction**

<!-- -->

[assistant] 2026-07-10T11:02:34Z
**Identifying malformed row handling issues**

<!-- -->
**Analyzing summary field type mismatch**

<!-- -->

[assistant] 2026-07-10T11:02:44Z

[assistant] 2026-07-10T11:02:48Z
**Assessing SQLite concurrency and numeric field handling**

<!-- -->

[user] 2026-07-10T11:02:48Z
<system-reminder>
Routine context housekeeping is near: a large span of this session will be comparted soon, and ~68k tokens of tool output remain unreduced. Drop spent outputs with ctx_reduce first so the archived span is the part that matters.
oldest reclaimable: §3§ aft_outline · §4§ aft_outline · §6§ read · §7§ read.
</system-reminder>

[assistant] 2026-07-10T11:02:54Z
**Planning selective content pruning**

<!-- -->

[assistant] 2026-07-10T11:03:04Z
**Inspecting Rust shadow false positives**

<!-- -->

[assistant] 2026-07-10T11:03:08Z
**Inspecting mutation ID handling**

<!-- -->

[assistant] 2026-07-10T11:03:12Z

[assistant] 2026-07-10T11:03:17Z
**Identifying silent mutation handling bug**

<!-- -->
**Investigating M0 operation usage in Rust store**

<!-- -->

[assistant] 2026-07-10T11:03:23Z
**Clarifying type mappings between TS and Rust**

<!-- -->
**Analyzing shadow session isolation and ID handling**

<!-- -->

[assistant] 2026-07-10T11:03:33Z
**Identifying passthrough bypass before binding**

<!-- -->
**Assessing dispatch arms for shadow binding**

<!-- -->

[assistant] 2026-07-10T11:03:40Z

[assistant] 2026-07-10T11:03:42Z
**Inspecting facade binding requirements**

<!-- -->

[assistant] 2026-07-10T11:03:45Z

[assistant] 2026-07-10T11:03:49Z

[assistant] 2026-07-10T11:03:53Z
**Identifying potential shadow route risks**

<!-- -->

[assistant] 2026-07-10T11:03:59Z
**Analyzing shadow_seq and pass_seq handling**

<!-- -->

[assistant] 2026-07-10T11:04:05Z

[assistant] 2026-07-10T11:04:08Z

[assistant] 2026-07-10T11:04:11Z
**Analyzing pass_seq usage and implications**

<!-- -->
**Identifying quarantine behavior and missing fields**

<!-- -->

[assistant] 2026-07-10T11:04:23Z

[assistant] 2026-07-10T11:04:26Z
**Verifying shadow memory loading and path handling**

<!-- -->
**Comparing Rust and TypeScript message ID schemes**

<!-- -->

[assistant] 2026-07-10T11:05:35Z
**Inspecting ck_wire with grep "#"**

<!-- -->

[assistant] 2026-07-10T11:05:39Z
**Confirming safe shadow mutation handling**

<!-- -->
**Identifying full DB scan performance risk**

<!-- -->
**Highlighting unbounded full session scans**

<!-- -->
**Assessing cache reuse constraints**

<!-- -->
**Analyzing inefficient full scans and caching issues**

<!-- -->
**Identifying missing reset support and transport risks**

<!-- -->
**Identifying potential memory DoS risk**

<!-- -->
**Verifying authentication message handling**

<!-- -->

[assistant] 2026-07-10T11:06:12Z
**Expanding search to TypeScript files**

<!-- -->

[assistant] 2026-07-10T11:06:16Z
**Analyzing socket timeout handling and queue behavior**

<!-- -->
**Identifying permanent route wedging on timeouts**

<!-- -->
**Tracing timeout impact on write backpressure and socket closure**

<!-- -->
**Verifying route caching and IDs**

<!-- -->
**Confirming state sync and watermark handling**

<!-- -->

[assistant] 2026-07-10T11:06:41Z

[assistant] 2026-07-10T11:06:55Z

[assistant] 2026-07-10T11:07:01Z
**Clarifying flatBlockId usage and parity docs**

<!-- -->

[assistant] 2026-07-10T11:07:07Z
**Assessing Pi parity and config tests**

<!-- -->

[assistant] 2026-07-10T11:07:23Z

[assistant] 2026-07-10T11:07:25Z
**Verifying Rust shadow state idempotency**

<!-- -->
**Analyzing CAS reset behavior and concurrency**

<!-- -->

[assistant] 2026-07-10T11:07:37Z
**Analyzing async handler concurrency limits**

<!-- -->
**Verifying shadow and live project isolation**

<!-- -->

[assistant] 2026-07-10T11:07:55Z
**Confirming default serializer profile and config handling**

<!-- -->
**Tracing shadow state sync and normalization usage**

<!-- -->

[assistant] 2026-07-10T11:08:03Z
**Assessing denormalization and ordinal mapping**

<!-- -->
**Reviewing ordinal expectations in tests**

<!-- -->

[assistant] 2026-07-10T11:08:10Z
**Verifying shadow_transform config loader behavior**

<!-- -->

[assistant] 2026-07-10T11:08:15Z
**Investigating backoff reset in shadow transport auth**

<!-- -->
**Identifying shadow store session data leak**

<!-- -->
**Assessing unbounded divergence DB growth**

<!-- -->
**Identifying unbounded database growth issue**

<!-- -->
**Assessing shadow memory duplication impact**

<!-- -->
**Identifying memory update watermark bug**

<!-- -->
**Inspecting shadow memory load path**

<!-- -->

[assistant] 2026-07-10T11:08:49Z

[assistant] 2026-07-10T11:08:53Z
**Correcting search region**

<!-- -->

[assistant] 2026-07-10T11:09:10Z

[assistant] 2026-07-10T11:09:16Z

[assistant] 2026-07-10T11:09:19Z
**Analyzing workspace memory handling discrepancies**

<!-- -->
**Confirming limited project-based memory retrieval**

<!-- -->

[assistant] 2026-07-10T11:10:29Z

[assistant] 2026-07-10T11:10:33Z
**Searching live transform memory injection**

<!-- -->

[assistant] 2026-07-10T11:10:37Z
**Examining workspace injection methods**

<!-- -->

[assistant] 2026-07-10T11:10:43Z
**Identifying project reference context**

<!-- -->

[assistant] 2026-07-10T11:10:48Z
**Continuing code reading from line 550**

<!-- -->

[assistant] 2026-07-10T11:10:54Z
**Searching for getMemories usage**

<!-- -->

[assistant] 2026-07-10T11:10:57Z

[assistant] 2026-07-10T11:11:01Z
**Identifying shadow project memory limitations**

<!-- -->
**Analyzing memory scope mismatch between TS and Rust**

<!-- -->

[assistant] 2026-07-10T11:11:12Z
**Mapping live memory sources**

<!-- -->

[assistant] 2026-07-10T11:11:22Z
**Analyzing user memory discrepancies in shadow sync**

<!-- -->
**Identifying history budget mismatch between Rust and TS**

<!-- -->

[assistant] 2026-07-10T11:11:33Z
**Comparing history budget tokens between Rust and TypeScript**

<!-- -->

[assistant] 2026-07-10T11:11:38Z

[assistant] 2026-07-10T11:11:41Z
**Inspecting McModuleConfig for history budget**

<!-- -->

[assistant] 2026-07-10T11:11:50Z

[assistant] 2026-07-10T11:11:53Z
**Assessing history budget discrepancies**

<!-- -->

[assistant] 2026-07-10T11:11:59Z
**Searching code comments in Rust**

<!-- -->

[assistant] 2026-07-10T11:12:05Z
**Auditing ShadowPassInputs field handling**

<!-- -->
**Identifying config divergences in memory budgets**

<!-- -->
**Tracing missing render_config in TS inputs**

<!-- -->
**Inspecting TypeScript builder outputs comprehensively**

<!-- -->
**Analyzing SQLite busy timeout and shadow mode latency**

<!-- -->
**Evaluating high latency from unbounded JSON cloning**

<!-- -->
**Identifying state sync error causes**

<!-- -->
**Analyzing state sync compartment behavior**

<!-- -->

[assistant] 2026-07-10T11:12:40Z
**Identifying production function alias**

<!-- -->

[assistant] 2026-07-10T11:12:45Z
**Investigating mutation logging limitations**

<!-- -->

[assistant] 2026-07-10T11:12:50Z
**Assessing state sync and workspace handling**

<!-- -->
**Reviewing test coverage for history**

<!-- -->

[assistant] 2026-07-10T11:13:34Z
**Analyzing history budget discrepancies**

<!-- -->
**Inspecting TS projectPath workspace handling**

<!-- -->

[assistant] 2026-07-10T11:13:45Z
**Locating Materialize call at line 1656**

<!-- -->

[assistant] 2026-07-10T11:13:50Z
**Identifying cache boundary limitations**

<!-- -->
**Analyzing ordinal memo and reset handling**

<!-- -->
**Verifying state sync sequence handling**

<!-- -->
**Identifying shadow memory bug class**

<!-- -->
**Analyzing history budget mismatch impact**

<!-- -->
**Tracing async transform error flow**

<!-- -->
**Identifying unbounded session read issue**

<!-- -->
**Aligning parser and mutation fields**

<!-- -->
**Prioritizing distinct high findings for report**

<!-- -->
**Identifying transport socket leak and unbounded frame length**

<!-- -->
**Confirming no config or isolation leaks**

<!-- -->
**Verifying state sync safety and test coverage**

<!-- -->
**Documenting uncovered test paths and config lines**

<!-- -->

[assistant] 2026-07-10T11:14:53Z
**Verifying TS schema lines 20-21**

<!-- -->

[assistant] 2026-07-10T11:14:59Z
**Reviewing event handler and reset leaks**

<!-- -->
**Validating memory source omissions and sync issues**

<!-- -->
**Assessing session memory duplication and cleanup needs**

<!-- -->
**Planning memory reset and transport cleanup**

<!-- -->
**Tracing socket timeout and transform error handling**

<!-- -->
**Identifying compartment wire mismatch as primary bug**

<!-- -->
**Assessing naming mismatches between Rust and TS fields**

<!-- -->
**Searching for shadow_acked_watermarks usage**

<!-- -->

[assistant] 2026-07-10T11:15:29Z
**Analyzing Rust shadow_state_hash details**

<!-- -->
**Identifying state sync and compaction gaps**

<!-- -->

[assistant] 2026-07-10T11:16:03Z
**Defining compaction handler**

<!-- -->

[assistant] 2026-07-10T11:16:19Z

[assistant] 2026-07-10T11:16:24Z
**Reviewing cleanup and reindexing process**

<!-- -->

[assistant] 2026-07-10T11:16:29Z

[assistant] 2026-07-10T11:16:38Z
**Analyzing recompaction and memo mismatch issues**

<!-- -->
**Investigating m0 mutation consumption in TS**

<!-- -->

[assistant] 2026-07-10T11:16:54Z

[assistant] 2026-07-10T11:16:58Z
**Assessing state sync and mutation markers**

<!-- -->

[assistant] 2026-07-10T11:17:06Z

[assistant] 2026-07-10T11:17:11Z
**Identifying Rust state sync limitations**

<!-- -->
**Evaluating history budget and sync risks**

<!-- -->
**Inspecting DB access concurrency**

<!-- -->
**Verifying exception handling coverage**

<!-- -->
**Planning output trimming and context management**

<!-- -->

[assistant] 2026-07-10T11:17:39Z
**Identifying storage cleanup gaps**

<!-- -->
**Highlighting state-sync wire mismatches**

<!-- -->
**Analyzing workspace source metadata handling**

<!-- -->
**Identifying shadow data retention concerns**

<!-- -->
**Evaluating shadow transform memory overlap**

<!-- -->
**Prioritizing high-severity findings by impact**

<!-- -->
**Validating false positives and test coverage**

<!-- -->
**Confirming live transform capture risks**

<!-- -->
<COUNCIL_MEMBER_RESPONSE>
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
</COUNCIL_MEMBER_RESPONSE>

