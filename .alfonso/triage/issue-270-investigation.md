# Issue #270 Investigation — "Sub-agent cut off by context-fill limit while composing final response"

- **Status:** investigation only (no fixes)
- **Reporter scenario:** an `explore` subagent (session `ses_031ead306ffe9wcCEXTyhmIlnp`) on
  `opencode/deepseek-v4-flash-free` (200k limit) ended its final assistant message as
  `[step-start|reasoning|step-finish]` with **zero text parts**; the parent's Task tool received an
  empty result with no error. Plugin v0.33.1, OpenCode 1.18.13, linux.
- **Source baseline:** all file/line citations are against HEAD (`50ab7e14`). Verified via
  `git diff v0.33.1 HEAD` that every cited file is **byte-identical** between the reporter's v0.33.1
  and HEAD, so citations apply directly to the shipped plugin the reporter ran.
- **Evidence files:** issue body (`gh issue view 270`), attached diagnostics
  `magic-context-issue-20260805-020352.md` (59,873 bytes; downloaded copy analyzed locally).

---

## Q1 — Reconstruct the subagent's final 2–3 transform passes

**Finding: impossible from this report — the attached log contains ZERO lines from the subagent
session.** Every session-tagged line in the log tail belongs to the PARENT session
`ses_03ffc99e8ffeOLC598H64peTXS` (379 `ses_*` lines, all parent; `grep -c
ses_031ead306ffe9wcCEXTyhmIlnp` over the attachment = 0).

Which session each part of the evidence belongs to:

- Issue body prose + `session_meta` numbers (tool_call_tokens=142648, conversation_tokens=27883,
  last_input_tokens=100890, observed_safe_input_tokens=175408, detected_context_limit=0): the
  **subagent** session `ses_031ead306ffe9wcCEXTyhmIlnp` (quoted by the reporter from the
  `session_meta` table).
- Every line in the attachment's `## Log (last 400 lines)` section: the **parent** session
  `ses_03ffc99e8ffeOLC598H64peTXS`. All `injected generic guidance ... subagent=false` lines
  corroborate this (e.g. attachment line 593).

Parent-session timeline reconstructed from the log (all times 2026-08-04 UTC, attachment lines):

| Time | Event | Evidence |
|---|---|---|
| 20:20:55→20:23:11 | 5 parent transforms, 126→130 messages, usage 60.6%→62.3%, all `decision=defer` | L220, L255, L304, L352, L400, L448 |
| 20:23:11→20:32:38 | **9.5-minute parent gap** — consistent with the parent blocked on the Task tool while the subagent ran (only non-session line in the window is `[20:23:33.648Z] [dreamer] timer tick`) | L448→L460 |
| 20:32:38→20:32:41 | parent transform, 131 messages (+1 = assistant msg carrying the Task call+result) | L460, L495 |
| 20:33:11→20:33:28 | parent continues normally: 132→133 messages, usage 62.5%→62.8%, all defer | L500, L544, L548, L592 |

What MC did on the **parent's** visible passes: pure `defer` passes at 60.6–62.8% (below the 63%
proactive floor — "compartment trigger: not firing at 60.6%/62.8%", L223/L505-area), with
`clearedParts=0 mergedReasoningParts=0`, `strippedParts=0`, `rematerialized=false,
reason=cache_hit`, no heuristic-drop or emergency-drop log lines. I.e. additive-only work
(tagging, §N§ prefixes, temporal gap markers, m[0]/m[1] cache-hit injection); **no MC mutation
removed any content on the parent side**.

Why the subagent's own lines are missing — two candidate explanations, neither resolvable from the
report:

1. **Session filtering at bundle time.** The issue bundler drops every log line that mentions a
   different session when the user picks one session in the `doctor --issue` picker
   (`packages/cli/src/lib/logs-opencode.ts:83-106` `filterLogLinesBySession`; picker at
   `packages/cli/src/commands/doctor-opencode.ts:274-301`). Caveat: the picker is only shown when
   `recentSessions.length > 1`, and the report's own "Recent sessions" section renders the same
   list as empty ("_No recent OpenCode sessions found (or OpenCode DB unavailable on this
   runtime)_", `packages/cli/src/lib/diagnostics-opencode.ts:884-887`; the collector needs
   `opencode.db` + Bun, `diagnostics-opencode.ts:442-452`). So per the report's own contents the
   filter should NOT have been applied — yet the shape of the log (parent-only, dreamer ticks
   surviving, which is exactly what a parent-session filter would produce) makes filtering the
   most likely mechanical explanation. Note the collector also excludes child sessions from the
   picker list by design (`parent_id IS NULL`, `diagnostics-opencode.ts:480-485`) — so even with
   the picker, the subagent session itself was never offered as a choice.
2. **The subagent's lines genuinely left the 400-line window.** The logger appends without
   rotation (`packages/plugin/src/shared/logger.ts:31-46`), and the window covers 20:20:55→20:33:28
   including the 9.5-min gap when the subagent ran, so this would be anomalous: a subagent doing
   ~142k tokens of tool work should emit `transform stage:` lines per LLM request under its own
   session id. If explanation (2) is the true one, that is itself a diagnostics bug worth chasing
   — but we cannot adjudicate it without the unfiltered log.

**What MC *would have* done on the subagent's passes** (per the session-modes table,
`ARCHITECTURE.md:153-165`, confirmed in code): tagging + §N§ prefixes when `ctx_reduce` is
available in the child's tool map (`ctx-reduce-availability.ts:60-96`), Channel-1/2 nudges when
available (`hook-handlers.ts:450-454`), heuristic tool drops on **every execute pass**
(`transform-postprocess-phase.ts:696-736`), and the ≥85% tiered emergency drop (the subagent's
"overflow path", `transform-postprocess-phase.ts:612-620, 730-734`). Subagents do **not** get
85% force-materialize of m[0] or the 95% fail-closed block (`transform-postprocess-phase.ts:610-611`,
`transform.ts:1300-1305`; see Q2b). **Did any MC mutation touch the subagent's final turn?
Unverifiable from this report** — but see Q2 for why no MC mutation *can* truncate a response
being generated.

---

## Q2 — The "context-fill limit" claim

The subagent's own `session_meta` refutes the premise: `last_input_tokens=100890` against a 200k
limit is **50.4%**, and `detected_context_limit=0` means no provider overflow error was ever
parsed for this session (`overflow-detection.ts:27-48` patterns; a detected limit is persisted by
`recordDetectedContextLimit` on the subagent path, `event-handler.ts:315-324, 401-411`, and would
show >0).

### (a) Can the subagent heuristic-drop path drop content mid-turn and starve the final response?

**No.** Three independent reasons, all source-verified:

1. **Transforms run pre-request, never mid-generation.** The transform mutates the outgoing
   request's message array before the LLM call; the model's response-in-progress is not part of
   any request and is never touched by MC. There is no MC code path that runs during streaming.
2. **Drops only touch historic tool output, never fresh content.** `applyHeuristicCleanup`
   (`heuristic-cleanup.ts:37-296`) drops/dedups `tool`-type tags and strips system-injection
   `message` tags only BELOW the protected cutoff `maxTag - protectedTags`
   (`heuristic-cleanup.ts:81-82`), with `DEFAULT_PROTECTED_TAGS = 20`
   (`features/magic-context/defaults.ts:1`). It never drops assistant/user conversation text, and
   open/incomplete tool arcs are excluded via `partHasCompletedResult` (`ARCHITECTURE.md:134`).
   The routine (non-emergency) pass does dedup + injection-strip only — Phase 2 removed
   need-blind routine tool drops (`heuristic-cleanup.ts:44-48, 90-94`).
3. **Worst case is context quality, not response truncation.** Even an aggressive drop could only
   remove *older* context the model might have wanted; it cannot turn a composed response into a
   zero-text message. The reporter saw the subagent "visibly start composing its conclusion" —
   that reasoning was being generated at that moment and was never an MC-mutable artifact.

### (b) Is there any MC mechanism that blocks/aborts subagent responses?

**No.** Exhaustive check of the abort/block surfaces:

- **95% fail-closed block** (`transform.ts:2039-2105`): `evaluateEmergencyFailClosed`
  (`transform-postprocess-phase.ts:519-558`) returns `shouldAbort` only when
  `emergencyRecoveryArmed && emergencyRecoveryOrigin === "provider_overflow" &&
  !foldMaterializedThisPass`. Recovery is **never armed for subagents**: the `session.error`
  overflow path skips them (`event-handler.ts:300-330`), the `message.updated` overflow path skips
  arming them (`event-handler.ts:388-415`), and the proactive model-shrink arm is gated
  `!sessionMeta?.isSubagent` (`transform.ts:951-963`). The 95% synthetic bump also requires armed
  recovery (`transform.ts:997-1010`). So the abort predicate is unreachable in a subagent session.
- **Historian emergency recovery** is gated `fullFeatureMode` (= `!isSubagent`,
  `transform.ts:681-682, 1300-1305`).
- **Fail-closed storage blocking** (schema fence / storage failure) exempts internal child
  sessions (`fail-closed-block.ts:103-115`, `messages-transform.ts:95-112`) and requires
  deterministic storage inoperability — the log shows healthy transforms throughout.
- **Hidden-agent step caps / `session.abort`** (`ARCHITECTURE.md:169`) apply only to
  MC-registered hidden agents (Q2c).

### (c) Step caps — does the reporter's `explore` agent carry one?

**Not from MC.** MC's `config` hook only adds its OWN hidden agents to `config.agent`
(`index.ts:697-714`), each with a hard `maxSteps` cap (`hidden-agent-registrations.ts:57-65`:
dreamer 150, dreamer-docs 60, dreamer-reviewer 4, dreamer-retrospective 40, etc.). It never
modifies built-in agents like `explore`. Two residual unknowns, both outside MC:

- The reporter's own `~/.config/opencode/opencode.jsonc` is **not included** in the diagnostics
  bundle (only `magic-context.jsonc` is) — a user-configured `steps`/`maxSteps` on `explore`
  cannot be ruled out from this report. (Their MC config sets nothing relevant — attachment
  lines 12-68.)
- Whether OpenCode 1.18.13 itself caps built-in subagent steps is an upstream OpenCode question.

### (d) deepseek-v4-flash-free output behavior

The observed shape — a terminal assistant message containing reasoning parts and zero text parts —
is a **known weak-model failure shape already documented in this repo** from the Palace trials:
`packages/plugin/scripts/experiments/visual-memory/run-palace-trial.ts:32-33` ("the budget on
reasoning before emitting content; 16k starved the big categories into empty-content responses")
and `:548` ("output budget before any content, returning empty assistant text"). Two concrete
mechanisms fit, both provider/model-side:

1. **Reasoning starvation:** the model spends its entire output budget in thinking and terminates
   the step (finish=stop-ish) without ever emitting text. The subagent was mid-conclusion in
   reasoning, on a free-tier flash model, after a 142k-token tool-heavy run
   (`session_meta.tool_call_tokens`, persisted by `transform.ts:2155-2230`).
2. **Provider output-cap truncation:** free-tier gateways commonly enforce a `max_output_tokens`
   far below the context window; a cut landing mid-reasoning yields a reasoning-only terminal
   message (would carry `finish_reason=length`; MC already recognizes this finish family in
   `assistant-message-extractor.ts:76-94`).

Distinguishing them requires the provider response metadata for the subagent's final request,
which the report does not contain. **Neither mechanism involves MC.**

---

## Q3 — `observed_safe_input_tokens=175408` vs `last_input_tokens=100890`

**Semantics** (`event-handler.ts:491-495, 568-577`): on every `message.updated` event carrying
usage, `totalInputTokens = input + cache.read + cache.write`; when the message did NOT carry an
overflow error, `observedSafeInputTokens = max(previous, totalInputTokens)`. It is the session's
high-water mark of successfully-served input. `recordOverflowDetected` resets it to 0
(`storage-meta-persisted.ts:1704, 1708, 1730`) — but that function is **never called for
subagents** (Q2b), so for a subagent session it is a pure monotonic max.

**Interpretation for this session:**

- The subagent successfully sent a request of **175,408 tokens at some point (87.7% of 200k)**,
  then its last observed request was **100,890 (50.4%)** — the context SHRANK by ~74.5k between
  observations.
- That trajectory is MC's subagent reduction machinery working as designed: heuristic tool drops
  every execute pass plus the tiered emergency drop, which fires at ≥85%
  (`FORCE_MATERIALIZE_PERCENTAGE = 85`, `boundary-execution.ts:10`) for **both** primaries and
  subagents (`transform-postprocess-phase.ts:612-620, 730-734, 897-910`) — this is the
  mode-table's subagent "overflow path only". 175,408 > 170,000 (85% of 200k), so the emergency
  tiered drop demonstrably fired at least once in this session's life.
- **Does it indicate an earlier overflow event? No.** `detected_context_limit=0` means no
  provider overflow error was ever parsed for the session; the subagent overflow paths only ever
  *record* a reported limit (`event-handler.ts:315-324, 401-411`), and nothing reset the
  high-water mark. The discrepancy indicates **successful reclamation, not overflow**.

---

## Q4 — Propagation: empty subagent final message → parent Task result

**What OpenCode returns:** the Task tool collects the subagent's final assistant text and returns
it as the tool output; with a reasoning-only terminal message there is no text, so the parent
receives an **empty string with no error** — the silent loss the reporter saw. (This is OpenCode
tool internals, not in this repo; MC's own child-result reading uses the identical convention:
`extractLatestAssistantText` returns `null` when the latest assistant message has no text part,
`assistant-message-extractor.ts:57-74` — MC's hidden agents are exposed to the same failure shape.)

**Can MC add a signal? Feasibility (not built):**

- **Detection is feasible.** MC already classifies subagent sessions (`isSubagent` from
  `parentID`, `event-handler.ts:280-283`) and receives `message.updated` events; it can detect a
  terminal subagent assistant message whose parts are reasoning/step-only with zero text parts
  (part data readable via the same opencode.db read paths MC already uses, e.g.
  `read-session-db.ts` consumers).
- **The proven injection channel is `tool.execute.after` output mutation.** Channel 1 already
  appends `<system-reminder>` text to a tool's `output.output` in `tool.execute.after` for every
  tool except `ctx_reduce` (`hook-handlers.ts:518-548, 441-505`), and OpenCode persists + replays
  the mutated output (`ctx-reduce-nudge.ts:1-5`). A `tool === "task"` branch could append a
  warning ("subagent terminated with reasoning only; no text was returned — re-dispatch or
  investigate") when the child's terminal message had no text part, making the parent MODEL aware
  of the silent loss. Open questions for design: mapping the call to the child session id inside
  the hook (likely via a newest-child-of-parent lookup or `subagent_invocations`), and
  distinguishing "legitimately empty result" from reasoning-only termination.
- **Not a viable channel:** `sendIgnoredMessage` — its payload is `ignored: true`, hidden from the
  LLM (`send-session-notification.ts`, title-gate comment), so it only reaches the human.
- **Not possible from a plugin:** altering what OpenCode's built-in task tool itself returns, or
  retroactively adding a text part to the child's finalized message.
- The cleaner long-term fix (Task tool surfacing "subagent ended without text" as an error/signal)
  belongs to **OpenCode upstream**.

---

## Verdict

**Not an MC bug.** Primary cause is **provider/model behavior** (deepseek-v4-flash-free free
tier): the subagent's final step terminated with reasoning only and zero text — the
reasoning-starvation failure shape documented in this repo's Palace trials, or provider-side
output-cap truncation mid-reasoning. Secondary (propagation) gap is **OpenCode's**: the Task tool
returns an empty result with no error when a subagent's final message has no text part.

Evidence summary:

1. The child's final request ran at 100,890/200,000 = 50.4% — nowhere near a fill limit;
   `detected_context_limit=0` proves no provider overflow was ever detected for the session.
2. No MC mechanism exists that can block, abort, or truncate a subagent response (Q2a/Q2b); MC
   transforms run pre-request and only mutate historic tool output outside a 20-tag protected
   window.
3. The 175,408→100,890 token trajectory shows MC's subagent drops RECLAIMING context successfully
   (Q3), the opposite of a cutoff.
4. The failure shape matches a known weak-model pattern already recorded in this repo
   (`run-palace-trial.ts:32-33, 548`).

**Confidence:** HIGH that MC did not cause the empty response (source-verified absence of any
mechanism + the session's own numbers). MEDIUM on the exact provider-side mechanism
(reasoning-starvation vs output-cap truncation) — the report lacks the subagent's log lines and
the provider response metadata for its final request.

**Fix sketch:** N/A for MC root cause (not ours). The Q4 detection+`tool.execute.after` warning
is a viable *defensive enhancement* (feasibility described above), and the diagnostics bundle
should arguably warn when a session filter excluded child-session lines — both optional follow-ups,
not fixes for this issue.

---

## Log gap + additional diagnostics needed from the reporter

The attached log contains **no subagent-session lines at all**, so the subagent's final transform
passes cannot be reconstructed. Exact request wording for the reporter:

> To finish diagnosing #270 we need the Magic Context log lines for the sub-agent session itself —
> the attached report only contains lines for the parent session `ses_03ffc99e8ffeOLC598H64peTXS`
> (likely because the issue bundler was run with a session filter). Please provide:
>
> 1. The unfiltered log slice for the sub-agent:
>    `grep 'ses_031ead306ffe9wcCEXTyhmIlnp' /tmp/opencode/magic-context/magic-context.log > subagent-log.txt`
>    (or re-run `npx @cortexkit/magic-context@latest doctor --issue` and choose
>    **"All sessions (no filtering)"** in the session picker).
> 2. The final assistant message of that session from OpenCode's DB, including its parts and
>    finish/time fields:
>    `sqlite3 ~/.local/share/opencode/opencode.db "SELECT id, data FROM message WHERE session_id='ses_031ead306ffe9wcCEXTyhmIlnp' ORDER BY time_created DESC LIMIT 3"`
>    (redact anything sensitive).
> 3. The provider response metadata for the sub-agent's LAST request — specifically the
>    finish/stop reason and output token count (OpenCode server log or gateway side). This tells us
>    whether the model stopped itself after reasoning (`stop`) or was cut at an output cap
>    (`length`).
> 4. Your `~/.config/opencode/opencode.jsonc` (to rule out a user-configured `steps`/`maxSteps`
>    on the `explore` agent — the diagnostics bundle only included `magic-context.jsonc`).
> 5. Optionally, the parent's Task tool part for this invocation (its `output` bytes), to confirm
>    the empty tool result shape.
