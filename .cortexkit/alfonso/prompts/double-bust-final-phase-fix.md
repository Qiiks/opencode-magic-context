# Fix: fold→defer representation flip (double bust) — final representation phase

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration, packages/plugin (OpenCode lane ONLY — an Oracle proved Pi and the Rust module are structurally unaffected). Root cause is PROVEN with high confidence (wire dumps + runtime logs + source); implement the Oracle's plan below. This is the top cache-stability defect in the release — a pressure fold pays a second ~50%-prompt bust one pass later, twice observed in production today.

## Proven root cause (context — do not re-litigate, but verify each citation as you touch it)

One phase-ordering defect, two manifestations. The invariant: execute(H, D→D′) must emit the same historical prefix as defer(H, D′). Today:
- execute pass: stripClearedReasoning + stripReasoningFromMergedAssistants run EARLY (transform.ts:1599-1631), THEN heuristics/auto-reclaim/pending-ops mutate (drops write [cleared] via clearThinkingParts, tool-drop-target.ts:214-219/311-321, invoked from transform-postprocess-phase.ts:635-702), THEN ToolMutationBatch.finalize() prunes tool parts and now-empty messages (tool-drop-target.ts:256-266) — CHANGING role adjacency after the merged-strip decision was already destructively applied.
- defer pass: applyFlushedStatuses replays the same drops FIRST (apply-operations.ts:180-215, transform.ts:1497-1507), prunes, THEN strips.
These don't commute → Incident A: a tool auto-reclaimed AFTER the early strip leaves a late [cleared]+signature on the fold pass that the next defer strips (FULL→BARE). Incident B: the early strip evaluated PRE-prune adjacency and destroyed real thinking the defer pass (post-prune topology) would have retained (BARE→FULL).

## The fix (Oracle's plan, adopted verbatim)

1. MOVE (not duplicate) stripReasoningFromMergedAssistants: delete the early block at transform.ts:1599-1631 (+ now-unused import). It cannot stay early even as hygiene — its early destructive application is what makes Incident B unrecoverable.
2. Add ONE final representation phase immediately before runPostTransformPhase() returns (near transform-postprocess-phase.ts:1349). CONTRACT: nothing that mutates messages, tool targets, or role topology may run after this phase — enforce by placement and state it in a comment as the invariant.
3. In that final phase, order: (a) stripClearedReasoning(messages) when modelAcceptsEmptyContent(resolvedProviderID) (catches late auto-reclaim clears), then (b) stripReasoningFromMergedAssistants(messages, resolvedProviderID) (evaluates FINALIZED topology).
4. Existing EARLIER stripClearedReasoning calls stay as intermediate hygiene; the final call is authoritative. The final phase runs on EVERY pass — execute, hard fold, explicit flush, defer. Never gated on isCacheBustingPass.
5. NO frozen-id table for this: tool-drop statuses and reasoning watermarks are already the durable decisions; merged stripping is a deterministic wire projection recomputed from post-replay topology. (This is the design insight — representation = pure function of durable state + final topology.)
6. Telemetry: the final phase logs clearedParts + mergedReasoningParts counts (sessionLog, same style as existing strip logs).

## Non-vacuous tests (all three, as specified — the fixtures are the bulk of the work)

1. INCIDENT A: assistant with [empty sentinels, signed thinking, auto-reclaimable sibling tool, surviving tool]; force a genuine execute-class fold (real mutation gate — see Watch-out below); assert sibling dropped, survivor intact, and fold + fresh-source defer serialize the target IDENTICALLY BARE (no [cleared], no stale signature).
2. INCIDENT B: user → drop-only assistant → target assistant[empty, REAL signed thinking, tool_use, empty]; reclaim+prune predecessor on the fold; assert fold and defer byte-identical FULL, exact real thinking + signature preserved (guards against a vacuous both-BARE pass).
3. GATE/IDEMPOTENCE: inverse adjacency fixture (pruning makes two assistants consecutive → both passes identically BARE); run the finalizer twice → idempotent; non-anthropic provider → unchanged.
Use real tag statuses + ToolMutationBatch; assert the drop/prune actually occurred; compare the historical WIRE PREFIX (serialized bytes), not strip counts.

## Watch out (from the investigation)
- transform_decisions' "pressure_refold" label is normalizeMaterializeReason() defaulting (transform-decision-log.ts:109-112) — both incidents were actually cache_hit m0 replays with execute-class mutation gates. Drive tests through the REAL mutation gate (execute decision + heuristics/auto-reclaim), not the label and not forced m0 folds.
- Do not "fix" by re-running the merged strip at the end while keeping the early one — the early destructive application must GO.

## Gates
cd packages/plugin && bun test (full), typecheck, lint. Run the OpenCode cache-invariants E2E suite if runnable locally (packages/e2e-tests — check its README; if not runnable, say so). check_comments clean — comments state the invariant ("representation strips run once, after all topology mutations; execute and defer must serialize identical prefixes"), never the incident/Oracle. This ships in v0.32.0 — keep the diff tight.
