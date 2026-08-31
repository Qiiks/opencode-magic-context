# Turn-detection audit: OpenCode, Pi, Claude Code, and dashboard

## Definitions and detection sites

### OpenCode TypeScript

| Site | Definition |
| --- | --- |
| `packages/plugin/src/hooks/magic-context/read-session-db.ts:133-176` | A newer user row is **real** when it has no parts or at least one part that is not `synthetic`, `ignored`, an MC todo part, or a marker part. The all-parts predicate deliberately permits a real prompt that also carries a synthetic `@mention` part. Comparison is `user.time_created > latestAssistant.time_created`. |
| `packages/plugin/src/hooks/magic-context/read-session-db.ts:91-131` | The session is **mid-turn** when the latest assistant has `finish="tool-calls"`, or one of its part rows is a non-provider-executed `type="tool"`. A newer real user row releases either condition; synthetic notices do not. |
| `packages/plugin/src/hooks/magic-context/boundary-execution.ts:13-56` | A normal `execute` decision is changed to `defer` while `midTurn`; force/emergency, explicit refresh, and subagent bypasses are allowed. The deferred-execute flag records intent and is drained only after work succeeds. |
| `packages/plugin/src/hooks/magic-context/transform.ts:1372-1399` | The TypeScript transform takes one DB snapshot of `isMidTurn`, applies the boundary lock, and persists `deferred_execute_state` when the lock changes execute to defer. |
| `packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:2020-2022,2393-2396` | The OpenCode Rust adapter sends the same DB-derived `mid_turn`, request-ingress time, and persisted `lastResponseTime` to `mc-module`. |
| `packages/plugin/src/hooks/magic-context/event-handler.ts:521-564,640-647,688-710` | `lastResponseTime` advances on every terminal assistant **step** update with usage and a finish/completion field. Both `tool-calls` and `stop` are delivery boundaries. It is step EOF, not turn EOF (the failure fixed by `3f6a109e`). |
| `packages/plugin/src/hooks/magic-context/tail-hygiene-walk.ts:223-259` | A **real user turn** is a user message that does not satisfy the same all-parts synthetic predicate as the release valve. Summary, todo-head, Channel-2, ALF/marker, and m0/m1 rows do not increment cadence. |
| `packages/plugin/src/hooks/magic-context/ctx-reduce-nudge.ts:84-188,281-292` and `hook-handlers.test.ts:797-881` | Channel-1 same-band re-fire requires a five-real-user-turn gap; same-band copy stays sticky. The empty last level, not ordinal zero, means never fired, so an all-tool window with zero real turns is dampened correctly. |

### Pi TypeScript

| Site | Definition |
| --- | --- |
| `packages/pi-plugin/src/read-session-pi.ts:147-191` | Find the newest assistant in the live event. It is **mid-turn** when `stopReason="toolUse"`, or that assistant has a `toolCall` whose exact id has no later `toolResult.toolCallId`. A later user releases only if the same object is a durable branch entry of `type="message"`; wire-shaped steer/custom users are not real. |
| `packages/pi-plugin/src/read-session-pi.ts:193-235` | Tool completion is id-paired, not role- or position-paired. An unrelated result cannot close the newest call. |
| `packages/pi-plugin/src/boundary-execution-pi.test.ts` plus shared `boundary-execution.ts` | Pi feeds `isMidTurnPi` to the same deferred-execute transition as OpenCode. |
| `packages/pi-plugin/src/tail-hygiene-walk-pi.ts:182-195` | A real turn is a rendered `role="user"` entry retained by the Pi synthetic classifier. `syntheticLeadingCount` excludes m0/m1; hidden custom Channel-2 entries are excluded. |
| `packages/pi-plugin/src/ctx-reduce-nudge-pi.ts:210-241,296-299` | Channel-2 uses a hidden custom message with `deliverAs:"nextTurn"`; it joins the next real user turn and must neither steer the active turn nor create an autonomous turn. |
| `packages/pi-plugin/src/index.ts:1942-1966` | Channel-2 is queued only at a clean final `stop` agent-end boundary, not error/abort/retry agent-end events. |
| `packages/pi-plugin/src/ctx-reduce-nudge-pi.test.ts:459-563` | The five-turn Channel-1 floor reads `countRealPiUserMessages`; synthetic leading rows leave the count unchanged. |

### Rust module (OpenCode and Claude Code profiles)

| Site | Definition |
| --- | --- |
| `crates/mc-module/src/transform.rs:637-795` | `mid_turn` is shadow ingress evidence that protects a streaming assistant identity. `prev_response_completed_at_ms` and `request_observed_at_ms` are request evidence for temporal gaps, not render identity or a turn-end proof. |
| `crates/mc-module/src/transform.rs:3216-3241,5656-5665` | Synthetic blocks are removed before cache logic. `mid_turn=true` makes the newest real assistant provisional/mutation-exempt. |
| `crates/mc-module/src/transform.rs:6146-6173` | The scheduler's independent **mid-tool** detector examines the newest assistant ordinal and requires an exact non-provider-executed tool-call arc with no matching non-provider-executed result arc. It does not consult a newer real user row. |
| `crates/mc-module/src/scheduler.rs:532-564` | Normal execute defers on `TailState.mid_tool_use`; force/emergency and configured bypasses win. Successful scheduled work drains the durable deferred intent. |
| `crates/mc-module/src/tail_hygiene.rs:82-94` and `transform.rs:9386-9441,9852-9961` | Rust Channel-1 counts projected non-synthetic user messages. Five-turn cadence is based on that count. A count regression (`last_fire > current`) expires the old ordinal, which prevents pseudo-compaction/lineage truncation from suppressing reminders forever. |
| `crates/mc-module/src/transform.rs:12138-12181` | For the OpenCode Anthropic continuation shape, a real-content assistant tail stays untouched. A prior response completion timestamp cannot distinguish turn EOF from the preceding tool step's EOF; only a contentless assistant shell can be losslessly re-anchored. This is the `3f6a109e` lesson. |
| `crates/mc-module/src/transform.rs:831-846` | Claude Code pseudo-compaction is explicit transport state (`lineage_switched`, edge/epoch/constituents, `compaction_observed`); the module does not infer a new turn from summary text. |

Claude Code/Broca has no in-repository harness-side turn classifier: Thalamus supplies CK messages plus `mid_turn` and response/request timestamps. The module trusts `mid_turn` only for live-tail mutation protection, infers open tool arcs for the scheduler, and treats `prev_response_completed_at_ms` as step EOF. Therefore a Thalamus classifier change is wire-visible and must be tested in the Thalamus repository as well as against the module contract here.

### Tauri dashboard

| Site | Definition |
| --- | --- |
| `packages/dashboard/src-tauri/src/db.rs:998-1104,2515-2590` | OpenCode assistant requests are read from `message.data`. The shipped fix reads native `parentID`; that user-message id is OpenCode's turn key for every assistant request spawned by the same user prompt. |
| `packages/dashboard/src-tauri/src/pi_sessions.rs:670-832` and `db.rs:1106-1159,2592-2643` | Pi JSONL yields one cache event per assistant message with usage and preserves `stopReason`. Without a native parent key, `toolUse` means the next request is a continuation. |
| `packages/dashboard/src-tauri/src/external_cache_sessions.rs:314-347` | Claude Code emits one assistant line per content block. The scanner deduplicates by `message.id`, retaining the final block/usage, then sorts distinct API messages. This is request deduplication, not turn detection. |
| `packages/dashboard/src-tauri/src/db.rs:1431-1441,1658-1708` | Turn grouping now prefers OpenCode `parentID`. JSONL fallback recognizes OpenCode `tool-calls`, Pi `toolUse`, and Claude `tool_use` as continuation finishes. Every retained assistant message remains one request bar. |
| `packages/dashboard/src-tauri/src/db.rs:2498-2513` | “By turns” loading trims the grouped request timeline at `is_turn_start` boundaries; therefore an incorrect grouping directly changes the visible count/window. |
| `packages/dashboard/src/components/CacheDiagnostics/CacheDiagnostics.tsx:423-462` | The UI groups request bars by backend `turn_id`; `events.length` is the per-turn request count. It does not re-detect boundaries. |

## Shape matrix

Legend: **M** mid-turn/locked, **B** boundary/unlocked, **+1** one real-user cadence increment, **+0** none. A dashboard value is `turns/requests` for the shape itself. `†` marks an intentional difference in what is being counted; `BUG-n` refers to a scoped brief below.

| Shape | OpenCode lock | Pi lock | Rust scheduler arc | OC / Pi / Rust real-turn counter | Dashboard after fix | Divergence |
| --- | --- | --- | --- | --- | --- | --- |
| Plain user turn after a completed assistant | B | B (with durable branch evidence) | B | +1 / +1 / +1 | 1/1 | None. |
| Multi-step tool loop (call → result → next request → final) | M until final assistant | M while `toolUse` or an exact call is unpaired | M only while the newest call is unpaired; B at the paired step boundary | +0 / +0 / +0 during steps | 1/N, keyed by OpenCode parent | **BUG-2:** Rust can execute at a paired step boundary while OpenCode/Pi locks remain mid-turn. |
| Interrupted tool call followed by a real queued user | B when the user timestamp is strictly newer | B when the user is a genuine branch message | M while the abandoned call remains unpaired | +1 / +1 / +1 | 2/2 in OpenCode, even if timestamps collide | **BUG-2:** Rust ignores the newer-user release. |
| Unrelated/unpaired tool result after newest call | M; the latest assistant tool part remains live | M; only matching `toolCallId` closes it | M; only the matching arc closes it | +0 / +0 / +0 | Same turn by native parent or continuation finish | Aligned; keeper tests fail if “any result” is treated as turn end. |
| Synthetic user injection (Channel-2 or ALF/work notice) during a tool turn | M; all-parts synthetic/ignored/marker rows do not release | M; custom/steer user shapes absent from durable branch set do not release | Prior arc unchanged because synthetic CK is filtered | +0 / +0 / +0 | OpenCode native parent may expose a separate synthetic request turn; Pi `nextTurn` adds none | Intentional†: operational real-user cadence differs from OpenCode's native request grouping. |
| Two queued real users with the same timestamp | **M possible** because release uses strict `time_created >` | B by branch-object identity | M if the abandoned arc remains open | +2 / +2 / +2 | 2/2 via distinct native parent ids | **BUG-1** in the OpenCode release valve; dashboard fixed. |
| Claude Code continuation after pseudo-compaction | N/A | N/A | Determined by Thalamus `mid_turn` for protection and CK arc pairing for scheduling; compaction metadata itself is not turn EOF | Rust count can regress after lineage truncation; regression guard expires old cadence | Heuristic fallback groups after `tool_use`; no native CC turn root | Explicit compaction semantics are intentional†; missing CC turn roots are **BUG-3** for interrupted continuations. |
| Pi Channel-2 `deliverAs:"nextTurn"` | N/A | M before the real user, then B with that branch user | Synthetic row excluded; real user contributes normally | N/A / +0 then +1 / transport-dependent | 0 requests for the hidden custom row; next assistant starts the real turn | Intentional†. |

## Why the dashboard differed from OpenCode

The dashboard previously reconstructed turns from the *previous assistant request's finish string*: only literal `tool-calls` continued a turn. OpenCode already stores the canonical relationship on every assistant message as `parentID`, pointing at the user message that owns the native turn. Finish inference loses information in precisely the owner-visible cases:

1. an interrupted `tool-calls` request followed by a queued user was incorrectly merged with the new turn;
2. two queued users sharing a timestamp could not be separated reliably;
3. imported/finalized rows whose finish changed no longer matched the ownership relationship; and
4. Pi (`toolUse`) and Claude Code (`tool_use`) continuation values were treated as new turns.

The shipped dashboard change reads `parentID` in both OpenCode SQL paths, carries it as a non-serialized grouping hint, and makes it authoritative over finish inference. The request bars are unchanged; only their parent turn and `is_turn_start` boundary change. JSONL harnesses retain a normalized finish fallback because their scanners do not yet expose an equivalent native turn root. There is no cache/materialization state change.

## Divergence disposition and scoped briefs

### BUG-1 — OpenCode equal-time newer-user release

**Scope:** `read-session-db.ts` and its tests only; cache-adjacent, do not fold into the dashboard patch.

Use an ordering key that matches OpenCode's native message order (monotonic message id or an explicit stable `(time_created,id)` comparison), not strict timestamp alone, for both selecting the latest assistant and finding a later real user. Add equal-time fixtures for two real queued users and for a synthetic row plus a real row. Verify that synthetic rows still cannot release the lock.

### BUG-2 — Rust boundary lock does not share host mid-turn semantics

**Scope:** OpenCode/Pi/Thalamus adapters, CK request contract, `tail_state_from_live`, scheduler, and deferred-state tests; cache-adjacent.

Define one scheduler authority that combines exact arc pairing with harness turn evidence. It must remain locked across a completed tool-result step when the harness says the same turn continues, release on a newer real user even if an old call is abandoned, preserve force/emergency bypasses, and keep provider-executed arcs exempt. Do not repurpose response-completion timestamps: `3f6a109e` proves they are step EOF.

### BUG-3 — JSONL dashboard interruption/pseudo-compaction ambiguity

**Scope:** dashboard scanners only (display-only).

Have `pi_sessions.rs` emit a turn root from durable user branch entries while excluding tool results/custom messages, and have the Claude scanner derive a root from UUID/parentUuid chains while preserving `message.id` request dedup. Then replace finish inference for those harnesses with the emitted root. Include interrupted tool, pseudo-compaction, and `deliverAs:"nextTurn"` fixtures.

### Intentional divergences

- **Step EOF versus turn EOF:** `lastResponseTime` / `prev_response_completed_at_ms` is a timing anchor for the previous request step. It is never proof that the agent turn ended.
- **Native display turns versus real-user cadence:** the dashboard follows OpenCode's native parent grouping, including a synthetic request if OpenCode itself creates one. Channel-1 counters deliberately count only operator-authored users.
- **Pseudo-compaction count regression:** lineage may remove old user rows. Treating a persisted cadence ordinal greater than the current projected count as expired prevents permanent nudge suppression; it is not evidence that those removed rows are newly authored turns.
- **Provider-executed tools:** they do not create a host-side unfinished arc and therefore do not hold the boundary lock.

## Keeper tests

| Contract | Keeper |
| --- | --- |
| OpenCode finish/part-row mid-turn and real-vs-synthetic release | `packages/plugin/src/hooks/magic-context/read-session-db.test.ts` |
| Shared deferral transition | `packages/plugin/src/hooks/magic-context/boundary-execution.test.ts` and `boundary-execution-integration.test.ts` |
| Pi exact call/result pairing, custom-user exclusion, and genuine-user release | `packages/pi-plugin/src/boundary-execution-pi.test.ts` |
| OpenCode/Pi five-real-user-turn floor and zero-turn guard | `hook-handlers.test.ts`, `ctx-reduce-nudge.test.ts`, and `ctx-reduce-nudge-pi.test.ts` |
| Rust synthetic real-turn count and exact newest-call arc pairing | `crates/mc-module/src/tail_hygiene.rs` tests and `transform.rs::tail_state_requires_a_result_paired_to_the_newest_assistant_call` |
| Dashboard native OpenCode parent grouping, timestamp collision, JSONL finish normalization | `packages/dashboard/src-tauri/src/db.rs::cache_turn_tests` |
| Claude Code message-id request dedup mutation control | `packages/dashboard/src-tauri/src/external_cache_sessions.rs::tests::parses_claude_code_usage_and_skips_sidechains` |

The mutation controls are observable: classifying synthetic rows as real advances the coupled Channel-1 cadence specimens; treating any tool result as completion fails the Pi and Rust unpaired-result tests; removing Claude `message.id` dedup changes the expected request count from two to three; ignoring OpenCode `parentID` merges the two equal-time dashboard turns.
