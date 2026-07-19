---
task_id: bg_96fef19f
agent: council-member
session_id: ses_0b4657cc2ffewCuPnq6xlLMeFv
parent_session_id: ses_0b4664e56ffeKx8uRSceXLl3v9
status: completed
completed_at: 2026-07-10T11:06:27Z
---

[user] 2026-07-10T10:37:25Z
## Solo Analysis Mode
You MUST do ALL exploration yourself using your available read/search tools.
- Do NOT use task or any delegation tool under any circumstances
- Do NOT delegate to explore, librarian, or any other subagent
- Do NOT spawn background tasks
- Search the codebase directly — you have full read-only access to every file
- This mode produces the most thorough analysis because you see every result firsthand


## Analysis Intent: AUDIT

You are conducting an **audit** — your goal is to find discrete issues, risks, or violations.

**Focus:**
- Search for problems, anti-patterns, security risks, correctness issues, or violations of stated requirements
- Each finding must be a distinct, actionable item with concrete evidence
- Severity determines priority: critical (blocks/breaks), high (significant risk), medium (should fix), low (nice to fix)
- For each finding, provide the specific location (reference, section, or component where it occurs)
- State your confidence: high (clear evidence), medium (likely but needs verification), low (suspicion, investigate further)
- **This is a broad sweep, not a targeted trace.**

**Analytical standards:** Support claims with concrete evidence. State confidence (high/medium/low) for key assertions. Note caveats and limitations.

**Structure your response as:**
```
<COUNCIL_MEMBER_RESPONSE>
## Finding 1: [Title]
- **Severity**: critical/high/medium/low
- **Location**: [specific reference — e.g. component, section, endpoint, rule]
- **Confidence**: high/medium/low
- **Issue**: [what is wrong and why it matters]
- **Evidence**: [concrete reference, snippet, or observation that proves the issue]
- **Suggested Fix**: [actionable recommendation]

## Finding 2: [Title]
...

## Summary
[Total findings by severity. Overall risk assessment with confidence levels.]
</COUNCIL_MEMBER_RESPONSE>
```

## Analysis Question

BLIND ADVERSARIAL COUNCIL AUDIT — Magic Context SHADOW-MODE lane, pre-production soak gate.

Repo: /Users/ufukaltinok/Work/Projects/CortexKit/magic-context
Branch: subc-migration, HEAD 6ea3179f

You are one of several independent auditors. Work ALONE, hunt for the NEXT bug class. Do NOT assume the code is correct because it has tests — the first armed run already exposed two integration bugs the mocked tests missed (wire-shape mismatch, ordinal starvation), both already fixed at HEAD. Your job is to find what the mocks still miss. Read the ACTUAL code with file:line evidence; do not speculate abstractly.

=== WHAT SHADOW MODE IS ===
A dev-flag lane (config shadow_transform.enabled, user-tier only) where the OpenCode TS plugin mirrors every finalized transform pass to the Rust mc-module over the subc daemon socket for byte-comparison — WITHOUT affecting the live session. The TS side fire-and-forgets: per-session FIFO queue, state_sync (compartments/memories/mutations sync), shadow_transform (input + TS output + decision), shadow_reset (generation bump + wipe). The Rust side runs its own transform on the mirrored input against a shadow:<sid> store lineage, byte-compares against the TS output, and records divergences in shadow_divergences. The soak is OBSERVE-ONLY by design.

=== AUDIT SCOPE (areas that changed or are first-run-live today) ===

1. packages/plugin/src/plugin/hooks/create-session-hooks.ts — the config mapper was changed from a hand-maintained field list to a full spread (`...pluginConfig`) with two defaulted overrides (commit e932804c). HUNT: does spreading the ENTIRE plugin config into the hook config leak anything that shouldn't reach per-session hook code, shadow anything the hook type declares differently, or change behavior for any existing consumer that relied on a field being absent (e.g. `undefined` vs present-with-value)?

2. packages/plugin/src/hooks/magic-context/shadow-sender.ts — the full sender. HUNT ESPECIALLY:
   (a) HOT-PATH SAFETY: enqueue() runs inside the transform hot path. Verify EVERY code path in enqueue (including resolveOrdinalsForShadow with its new by-id DB fallback, denormalizeShadowOutput, cloneJson of potentially huge message arrays) is exception-safe and bounded — a throw or a multi-second stall here delays the user's real prompt. Is cloneJson(messages) on a 400-message array acceptable per-pass cost? Is the by-id DB read (readRawSessionMessageById) opening a DB handle per call, and what happens under SQLITE_BUSY?
   (b) WIRE SHAPE (commit 6ea3179f): toFlatWireBody flattens {method, params} to flat. Cross-check EVERY field the TS builders emit against the Rust serde parsers in crates/mc-module/src/lib.rs (ShadowStateSyncWire, ShadowTransformWire, ShadowResetWire, ShadowPassInputs, ShadowUsageWire, ShadowCompartmentWire, ShadowMemoryWire, ShadowMemoryMutationWire): field names, types, required-vs-optional, snake_case exactness. Name any field that will reject or silently default.
   (c) ORDINAL FALLBACK: the new below-floor by-id fallback in resolveOrdinalsForShadow. Can the fallback return an ordinal computed on a DIFFERENT basis than the cache (e.g. cache primed tail-only carries absolute ordinals from baseOrdinal while the by-id COUNT includes/excludes summary rows differently)? A silent basis mismatch would poison the shadow store with wrong ordinals rather than failing loud.
   (d) SubcShadowTransport: hand-rolled socket protocol (auth handshake, frame header, backoff). Route kind now tool_provider (commit 1555b231). HUNT: connection lifecycle bugs (socket close mid-request, reader waiters, backoff doubling), auth proof correctness, frame length handling, the 5s request timeout leaving the FIFO wedged.
   (e) QUEUE SEMANTICS: MAX_QUEUE_PER_SESSION=4 drop-oldest, blockedUntilReset, requireResetReason transitions. Any path where a session gets permanently wedged (blocked but no reset queued) or where a reset races a queued pass?

3. crates/mc-module/src/lib.rs shadow handlers (handle_shadow_state_sync_value, handle_shadow_transform_value, handle_shadow_reset_value, shadow_binding) + crates/mc-store shadow methods (apply_shadow_state_sync, reset_shadow_session). HUNT:
   (a) ISOLATION: can ANY shadow-lane write touch a non-shadow session row? The shadow session id is derived from binding (shadow: prefix). Verify shadow_binding rejects non-shadow bindings on shadow ops AND that plain transform ops reject shadow bindings (codes non_shadow_op_on_shadow_binding, plain_transform_on_shadow_binding — verify they're actually enforced on EVERY arm).
   (b) The shadow_transform handler runs the REAL transform + a byte-compare. Does a shadow divergence or panic in the compare path affect the real lane in any way (shared store handle, lease contention, pass-trace pollution)?
   (c) apply_shadow_state_sync CAS semantics: generation/seq mismatch arms, and what happens when state_sync carries compartments whose sequences overlap already-synced ones (idempotent or duplicating?).

4. CACHE SAFETY OF THE LIVE LANE: the sender captures cloneForShadow(messages) BEFORE the transform mutates and reads declared-trim state before/after. Verify the shadow capture path cannot mutate the live messages array (shared references through cloneJson boundaries — is the clone deep and taken at the right point?), and that shadow-sender DB reads (getCompartments, getMemoriesByProject, etc.) hold no transactions that could contend with the transform's own writes.

=== DELIVERABLE (per member) ===
- Independent findings with file:line evidence. Read the real code.
- Rank each finding Critical / High / Medium / Low.
- Weight by blast radius: (1) can affect the LIVE user session = WORST class; (2) corrupts shadow state silently = defeats the soak's purpose; (3) merely loses shadow coverage but logged = ACCEPTABLE.
- Explicit false-positive filtering: check PARITY.md files and existing test coverage before claiming a bug. State what you checked.
- Your own verdict: SHIP (arm the soak on production sessions) or HOLD, with the single most important reason.

Relevant reference files exist at packages/pi-plugin/PARITY.md and packages/plugin/src/features/magic-context/smart-notes/PARITY.md. Test files live alongside sources (*.test.ts) and in crates/*/tests or #[cfg(test)] modules.

[assistant] 2026-07-10T10:37:25Z
**Planning direct code exploration**

<!-- -->

[assistant] 2026-07-10T10:46:49Z
**Planning strategic code reading batches**

<!-- -->

[assistant] 2026-07-10T10:47:31Z
**Identifying read tool for functionality testing**

<!-- -->

[assistant] 2026-07-10T10:48:07Z
**Investigating tool timeout causes**

<!-- -->

[assistant] 2026-07-10T10:48:37Z
**Testing individual chunk reading**

<!-- -->

[assistant] 2026-07-10T10:49:18Z
**Trimming output length**

<!-- -->

[assistant] 2026-07-10T10:49:36Z

[assistant] 2026-07-10T10:50:08Z
**Requesting additional content from line 429 onwards**

<!-- -->

[assistant] 2026-07-10T10:50:15Z

[assistant] 2026-07-10T10:50:18Z
**Continuing transport process**

<!-- -->

[assistant] 2026-07-10T10:50:38Z
**Tracing enqueue error source**

<!-- -->

[assistant] 2026-07-10T10:51:43Z
**Planning grep fallback for server bug**

<!-- -->

[assistant] 2026-07-10T10:52:23Z
**Listing functions in index.ts hook**

<!-- -->

[assistant] 2026-07-10T10:52:53Z
**Reading index hook transform**

<!-- -->

[assistant] 2026-07-10T10:53:23Z

[assistant] 2026-07-10T10:53:46Z
**Planning shadow creation approach**

<!-- -->

[assistant] 2026-07-10T10:54:19Z
**Planning shadow invocation transformation**

<!-- -->

[assistant] 2026-07-10T10:54:40Z
**Planning line transformation**

<!-- -->

[assistant] 2026-07-10T10:54:45Z
**Identifying cloneForShadow usage**

<!-- -->

[assistant] 2026-07-10T10:54:48Z
**Planning exception handling tests**

<!-- -->

[assistant] 2026-07-10T10:54:50Z
**Analyzing test database and handlers**

<!-- -->

[assistant] 2026-07-10T10:54:58Z
**Reading relevant modules**

<!-- -->

[assistant] 2026-07-10T10:55:06Z
**Identifying compartment wire format mismatch**

<!-- -->
**Analyzing missing m0_mutations in Rust sync**

<!-- -->
**Implementing remaining transform handler and conversions**

<!-- -->

[assistant] 2026-07-10T10:55:25Z
**Planning to read specific code sections**

<!-- -->

[assistant] 2026-07-10T10:56:05Z
**Clarifying m0 watermark acknowledgment behavior**

<!-- -->
**Analyzing Rust and TS wire protocol discrepancies**

<!-- -->
**Investigating ShadowMemoryWire shareable type mismatch**

<!-- -->
**Assessing data type for shareable**

<!-- -->

[assistant] 2026-07-10T10:56:58Z
**Confirming direct memory access**

<!-- -->

[assistant] 2026-07-10T10:57:31Z
**Verifying file path accuracy**

<!-- -->

[assistant] 2026-07-10T10:57:45Z

[assistant] 2026-07-10T10:58:15Z
**Checking parity files via direct fetch**

<!-- -->

[assistant] 2026-07-10T10:58:48Z
**Verifying shadow binding isolation**

<!-- -->
**Auditing transform handlers and reset semantics**

<!-- -->
**Confirming safe handling of delayed responses**

<!-- -->
**Tracing socket leak on auth failure**

<!-- -->
**Analyzing socket error handling and buffer risks**

<!-- -->
**Identifying single pending blocking multi-session**

<!-- -->
**Examining sequence semantics in transform flow**

<!-- -->
**Verifying shadow_seq updates in shadow report**

<!-- -->

[assistant] 2026-07-10T10:59:24Z
**Investigating transform commit behavior**

<!-- -->

[assistant] 2026-07-10T10:59:35Z
**Confirming state sync consistency after transform**

<!-- -->
**Assessing state sync CAS and upsert criteria**

<!-- -->

[assistant] 2026-07-10T11:00:03Z

[assistant] 2026-07-10T11:00:07Z
**Identifying deletion handling flaw in state sync**

<!-- -->
**Highlighting state sync schema revision gaps**

<!-- -->

[assistant] 2026-07-10T11:00:19Z

[assistant] 2026-07-10T11:00:22Z
**Verifying input handling and cloning strategy**

<!-- -->
**Assessing cloning performance impact on transform**

<!-- -->

[user] 2026-07-10T11:00:22Z
<system-reminder>
Routine context housekeeping is near: a large span of this session will be comparted soon, and ~69k tokens of tool output remain unreduced. Drop spent outputs with ctx_reduce first so the archived span is the part that matters.
oldest reclaimable: §2§ aft_outline · §3§ read · §4§ read · §5§ read.
</system-reminder>

[assistant] 2026-07-10T11:00:32Z
**Refining source selection and storage strategy**

<!-- -->

[assistant] 2026-07-10T11:00:42Z
**Tracing database access during enqueue**

<!-- -->
**Verifying exception safety in enqueue**

<!-- -->
**Flagging uncaught DB read exceptions**

<!-- -->
**Reviewing test coverage gaps**

<!-- -->

[assistant] 2026-07-10T11:00:57Z
**Identifying input/output comparator inconsistencies**

<!-- -->
**Examining build config and shadow_transform**

<!-- -->

[assistant] 2026-07-10T11:01:06Z
**Confirming no sensitive config leaks**

<!-- -->

[assistant] 2026-07-10T11:01:16Z
**Investigating potential queue bug and config usage**

<!-- -->
**Analyzing queue race conditions during session reset**

<!-- -->
**Verifying reset and peer reject handling flow**

<!-- -->
**Diagnosing sync failure loop and divergence handling**

<!-- -->
**Searching for resetSession usage**

<!-- -->

[assistant] 2026-07-10T11:01:33Z
**Verifying session reset handling**

<!-- -->
**Identifying memory growth and queue bottlenecks**

<!-- -->
**Assessing authentication proof details**

<!-- -->
**Identifying shadow session prefix conflict**

<!-- -->
**Analyzing store contention between shadow and live lanes**

<!-- -->
**Assessing async shadow transport impact**

<!-- -->
**Confirming readRawSessionMessages behavior**

<!-- -->
**Ranking exception and sync risks**

<!-- -->
**Evaluating m0 log relevance for syncing**

<!-- -->
**Planning compartment deletion validation**

<!-- -->

[assistant] 2026-07-10T11:02:08Z
**Searching for compartment_delete usage**

<!-- -->

[assistant] 2026-07-10T11:02:13Z
**Investigating compartment_merge usage**

<!-- -->

[assistant] 2026-07-10T11:02:16Z
**Searching for queueM0Mutation aliases**

<!-- -->

[assistant] 2026-07-10T11:02:19Z
**Identifying m0 mutation log as test artifact**

<!-- -->

[assistant] 2026-07-10T11:02:23Z
**Reading computation runner details**

<!-- -->

[assistant] 2026-07-10T11:02:28Z
**Identifying stale compartment handling issue**

<!-- -->
**Diagnosing sequence update omission in state sync**

<!-- -->
**Verifying memory mutation handling and rendering**

<!-- -->
**Identifying inadequate test coverage**

<!-- -->

[assistant] 2026-07-10T11:02:43Z
**Planning serialization for line mismatch**

<!-- -->

[assistant] 2026-07-10T11:02:45Z
**Planning Rust struct review**

<!-- -->

[assistant] 2026-07-10T11:02:49Z
**Planning schema cross-check**

<!-- -->
**Assessing TypeScript to Rust schema discrepancies**

<!-- -->
**Verifying TypeScript field defaults and shareable type**

<!-- -->

[assistant] 2026-07-10T11:02:59Z
**Examining transport socket protocol behavior**

<!-- -->
**Tracing state sync timeout and reset conditions**

<!-- -->
**Verifying reset behavior after peer rejection**

<!-- -->
**Identifying unbounded queue reset issue**

<!-- -->
**Analyzing costly session message reads**

<!-- -->
**Examining cache scope affecting fallback**

<!-- -->
**Investigating synthetic message IDs**

<!-- -->
**Analyzing transform-compartment phase**

<!-- -->

[assistant] 2026-07-10T11:03:24Z
**Identifying full DB scan cause**

<!-- -->
**Analyzing DB read delays and cloning impact**

<!-- -->
**Proposing deferred cloning and scan limits**

<!-- -->
**Clarifying synchronous enqueue behavior**

<!-- -->
**Tracing sync execution in pushWork schedule**

<!-- -->
**Planning final report with findings**

<!-- -->
**Identifying silent corruption issues**

<!-- -->
**Documenting detailed code discrepancies**

<!-- -->
**Verifying test coverage and false-positive checks**

<!-- -->
**Separating watermark and compartment schemas**

<!-- -->

[assistant] 2026-07-10T11:03:57Z

[assistant] 2026-07-10T11:04:00Z
**Confirming no ordinal bug found**

<!-- -->
**Verifying read-only DB access and config isolation**

<!-- -->
**Identifying ordinal mismatch in ShadowCompartmentWire**

<!-- -->
**Analyzing message ID usage inconsistencies**

<!-- -->

[assistant] 2026-07-10T11:04:17Z
**Tracing message ID handling**

<!-- -->
**Analyzing state sync failure impact**

<!-- -->
**Weighing severity of coverage loss**

<!-- -->
**Identifying silent corruption risks in live sync**

<!-- -->
**Confirming non-findings and planning serialization fix**

<!-- -->
**Designing nonblocking shadow prep and state snapshot**

<!-- -->
**Scheduling shadow reset for sync**

<!-- -->
<COUNCIL_MEMBER_RESPONSE>
## Finding 1: Non-empty compartment sync is rejected by Rust
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts:564-615`; `crates/mc-module/src/lib.rs:267-295`
- **Confidence**: high
- **Issue**: TS serializes each compartment as nested `start`/`end` objects, while Rust requires top-level `start_message` and `end_message` integers. Any state sync containing a compartment fails serde before a transform can be compared.
- **Evidence**: TS emits `start: { flat_id, bare_message_id, absolute_ordinal }` and the equivalent `end` (`shadow-sender.ts:584-604`). Rust requires non-default `start_message: i64` and `end_message: i64` (`lib.rs:267-270`). `invalid_params` is neither a peer reject nor connection failure, so the sender logs the error and repeatedly attempts the unacknowledged sync rather than resetting/recovering (`shadow-sender.ts:887-904`).
- **Suggested Fix**: Emit the Rust contract exactly—top-level ordinal and ID fields—or change Rust to explicitly deserialize the nested contract. Add an end-to-end TS-built, flat-wire serde test with at least one compartment.

## Finding 2: Shadow capture is synchronous, unbounded, and partly outside fail-open protection
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/transform.ts:459-462`; `shadow-sender.ts:226-228, 361-408, 483-512`
- **Confidence**: high
- **Issue**: The observe-only lane adds synchronous full-message cloning and DB work to the live transform path. Worse, the initial clone and declared-trim lookup occur before the later `try/catch`, so an exception can reject the live transform rather than merely lose shadow coverage.
- **Evidence**: `cloneForShadow(messages)` and `resolveDeclaredTrimForShadow()` execute before the shadow enqueue guard (`transform.ts:459-462`; the guard begins at `2186`). The JSON clone is an unbounded serialize/parse (`transform.ts:177-179`; `shadow-sender.ts:226-228`). Enqueue then clones input again (`shadow-sender.ts:380`) and clones output again (`:288`), while resolving ordinals via a full raw-session read (`:375-378`). No queue cap limits this pre-queue work.
- **Suggested Fix**: Enclose all shadow preparation in fail-open handling; impose byte/message/time caps and skip oversized passes; eliminate duplicate clones; and move raw-session/ordinal preparation off the transform return path or reuse already-computed transform metadata.

## Finding 3: Recompaction silently leaves stale compartments in the shadow store
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/compartment-runner-recomp.ts:110-123`; `shadow-sender.ts:646-647, 696-706`; `crates/mc-module/src/lib.rs:184-200`; `crates/mc-store/src/lib.rs:3991-4036`
- **Confidence**: high
- **Issue**: Recomp deletes and recreates all live compartments, often reusing old sequence numbers. The sender only transmits sequences above its prior high-water mark, so it sends none of the recreated rows. It emits an `m0_mutations` record intended to signal this change, but Rust’s state-sync wire does not define or consume that field. The shadow store therefore retains obsolete compartments.
- **Evidence**: Recomp explicitly `DELETE`s all compartments, reinserts staging rows, and records `recomp_boundary_change` (`compartment-runner-recomp.ts:110-123`). Sender skips every compartment with `sequence <= acked.compartment_sequence` (`shadow-sender.ts:646-647`) and sends `m0_mutations` (`:696-706`). Rust accepts only compartments, memories, and memory mutations (`lib.rs:184-200`) and only upserts supplied compartments—there is no deletion/replacement operation (`mc-store/src/lib.rs:3991-4036`).
- **Suggested Fix**: On any m0/recomp revision, reset the shadow lineage and force a full snapshot, or add an atomic authoritative-replace state-sync mode that deletes absent shadow compartments. Do not acknowledge the m0 watermark until Rust has applied the revision.

## Summary
**3 high findings. Verdict: HOLD.** The most important reason is Finding 2: a production shadow soak is not observe-only when synchronous clone/DB work—and potential exceptions—runs before the live transform completes.

False-positive filtering performed: `packages/pi-plugin/PARITY.md` documents Pi/OpenCode host differences only; the smart-notes PARITY file concerns compiled SSRF checks, neither covers this lane. Existing tests do not close these gaps: `shadow-sender.test.ts` checks a state-sync payload with no compartment, while Rust’s shadow test hand-constructs Rust-shaped `start_message`/`end_message` fields (`crates/mc-module/src/lib.rs:7067-7089`). I also found the shadow-binding and plain-transform isolation guards present, and the ordinal full-read/by-ID paths use the same ordering and summary filtering, so I did not report those as defects.
</COUNCIL_MEMBER_RESPONSE>

