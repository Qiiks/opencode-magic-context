# Oracle brief: fold-pass representation flip causes a second bust (cache-core, evidence-first)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration (TS plugin: packages/plugin/src/hooks/magic-context/). Read-only investigation + fix-plan design. This is the OpenCode TS lane (not the Rust module). Verify everything at source; cite file:line. The fix will be mason-built from YOUR validated plan — so the plan must name exact mechanisms, not hypotheses.

## Incident (twice today, same session, same signature, opposite directions)

Session ses_227ce5788ffeRPA9THoPLOQreO (an Alfonso primary session, Anthropic via the anthropic-auth plugin). Wire dumps: /var/folders/18/257zzylx4h1gbkcvs4cnpqqc0000gn/T/opencode-anthropic-auth-dumps/ (files match ses_227ce5788*, .body.json = full request body). Analyzer: bun packages/plugin/scripts/analyze-cache-busts.ts ses_227ce5788ffeRPA9THoPLOQreO --show-diff.

INCIDENT A (evening): transform_decisions rows (context.db, session above):
- 17:30:18 defer input=651,423
- 17:31:12 execute materialized=1 reason=pressure_refold dropped_count=174 input=429,945  ← designed fold bust #1
- 17:31:44 defer input=430,232 ← this pass's request re-busted ~50% of the prompt (bust #2, unnecessary)
Wire evidence: request 17-31-38 (fold output) message[11] (assistant) = [text " ", thinking "[cleared]" (signature present), tool_use mcp_Peer_inbox toolu_016kwUyjZvWsQiQVzcfu2RYg, text " "]. Next request 17-32-02 message[11] = [tool_use] BARE — the two empty-text sentinels AND the cleared-thinking block are GONE. Everything before msg11 byte-identical (cachedPrefix ends at message[0]).

INCIDENT B (morning, 10:32): same session, diverge at message[83] (assistant, mcp_Edit tool_use): fold request 10-32-28 msg83 = [tool_use] BARE; next request 10-32-46 msg83 = [text " ", thinking "The architecture is hold…", tool_use, text " "] FULL — OPPOSITE DIRECTION (fold pass stripped, defer restored; note the thinking here is REAL text, not [cleared]).

So: on a pressure_refold execute pass, at least one assistant message near the compaction boundary renders in a different representation than adjacent defer passes render it, in EITHER direction. The next pass flips it back → second bust. The flipped message contains: empty-text sentinels (" "), a thinking block, and a tool_use.

Note: morning dumps are endpoint "direct-main", evening "direct-fallback" (anthropic-auth plugin endpoints) — constant WITHIN each incident, so endpoint isn't the per-pass flip cause, but the direction inversion between incidents may correlate with it (or with the [cleared] vs real-thinking difference) — determine which.

## Candidate mechanisms to trace at source (non-exhaustive — follow the evidence)

1. The strip family responsible for the parts: stripStructuralNoise / empty-content sentinels (provider-gated via modelAcceptsEmptyContent), reasoning-clearing + replay (clearOldReasoning writes [cleared] gated to canonical Anthropic; replay is watermark-driven), stripReasoningFromMergedAssistants, and any frozen-id-set replay (strip-content.ts, transform.ts call sites, transform-postprocess-phase.ts ordering). Which of these produces FULL vs BARE for this exact block shape, and which of its gates is pass-class-sensitive (execute/fold vs defer) or ordering-sensitive relative to the fold?
2. The fold pass itself: prepareCompartmentInjection/injectM0M1 + the pending-op drain (174 drops on the fold pass) + heuristic cleanup run in a specific order on the fold pass; on defer passes only replays run. A strip whose input depends on state mutated EARLIER in the same pass (watermarks advanced, tags dropped, messages trimmed) could see different eligibility on the fold pass than on the next defer.
3. Message near the boundary: both incidents' flipped message sits just after the compaction boundary (msg 11 / msg 83 of ~505 wire messages, right after m0/m1 + head). Check boundary-adjacent index/ordinal sensitivity (e.g. a strip keyed on message INDEX or a window that shifts when the marker advances on the fold).
4. The morning direction (fold=BARE, defer=FULL with REAL thinking) vs evening (fold=FULL with [cleared], defer=BARE): the same mechanism seen from both sides of a watermark/frozen-set advance, or two different mechanisms? Resolve explicitly.

## Deliverable

(1) ROOT CAUSE: the exact mechanism, file:line, and why it's pass-class-dependent — proven against BOTH incidents' directions. (2) FIX PLAN for a mason: the invariant to restore (a message's representation must be a pure function of durable state, identical on fold-execute and defer passes), the minimal change, the replay/first-application discipline it must follow (detect-on-bust/replay-everywhere, frozen-id pattern where applicable), and non-vacuous tests: a fold-execute pass followed by a defer pass over a message with [empty-text sentinels + thinking + tool_use] must produce byte-identical representations, in both prior states (pre-strip and post-strip), plus a regression against whichever gate you identify. (3) Blast radius: does the same defect exist in the Pi lane (context-handler replay ordering) and the Rust module (strip parity)? Verdict format: root cause + plan + explicit confidence, with anything you could NOT prove flagged honestly.
