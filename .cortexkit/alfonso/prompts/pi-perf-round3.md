# Pi perf round 3: close the unaccounted gap (~165ms/pass at 1092 messages)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Live production numbers from Ufuk's 27k-entry session (019de471), steady-state pass: total 367-382ms, instrumented stages sum ~202ms → ~165ms uninstrumented. Prior rounds fixed the named stages (tagMessages 191→77ms, historian pre-gate, boundary checks). This round: instrument the gap FIRST, then optimize the top items. The byte-identity law applies to every change: run the compare harness (packages/pi-plugin/scripts/experiments/perf/) byte-gate over the real fixtures for every optimization commit.

## Phase 1 — instrumentation (commit separately, first)
In packages/pi-plugin/src/context-handler.ts runPipeline + the outer handler, add logTransformTiming wrappers so the stage sum ≈ total (target: >90% of wall time attributed). Known untimed segments (verify each at source):
- transcript.commit() (~line 4622)
- post-commit stable-id map builds (~4654-4672, iterates all messages twice)
- stripPiDroppedPlaceholderMessages (~4743)
- stripPiProcessedImages (4c block)
- caveman replay including its getTagsByNumbers fetch (3c block)
- note replay / sticky reminder index maps + auto-search decision parse (postTransform is timed as one 9ms block — split if it hides anything; the audit flagged two full index maps at ~4823-4835)
- transform-decision log write, message-index scheduling, todo capture — the outer handler segments before/after runPipeline (the 367ms total is measured where? verify the total timer's span vs the handler entry — if the total starts at findSessionId, anything before it is invisible; account for the full handler)
- tokenize cache validation cost inside tokenAccounting: the JSON-equality guard stringifies message content on every hit. Time the guard separately from actual BPE.
Keep the timers permanent (they're cheap and gated on the observer/log path like existing ones), not temporary probes.

Run one pass against the biggest real fixture (~/.pi/agent/sessions — the 019de471 JSONL, >5MB) and put the closed-gap phase table in your report BEFORE optimizing.

## Phase 2 — optimize what the table shows, in measured-impact order
Candidates we already suspect (verify against your Phase-1 numbers; skip any that measure small):
1. entryParseAndBranchResolution (53ms @ 27k entries): branch entries are append-only with stable ids. Memoize parse/conversion per (sessionId, entryId) with the same guarded invalidation the token cache uses (leaf change / branch switch → drop memo; the clone-inheritance and branch-switch paths must invalidate). Only the new tail parses per pass. Target O(new entries).
2. Token-cache JSON-equality guard: if the guard's stringify dominates tokenAccounting, replace full-content stringify comparison with a cheaper stable check where provably safe (e.g. length + lastModified-style structural fingerprint is NOT safe alone — keep correctness; consider caching the canonical JSON string alongside the count so validation is one string compare against a stored string, computed once per message when first cached, and only recomputed when identity-relevant fields could change — think it through and justify, cache-safety wins over speed).
3. Post-commit maps: build in one loop instead of two; skip when no consumer needs them this pass (check consumers' gates).
4. commit(): if it deep-walks all messages, make it dirty-tracking (only write back messages the pipeline actually touched — the transcript already knows which working[i] were reassigned).
5. Anything else the table surfaces >10ms.

## Non-goals
- Do NOT touch the correctness-fix regions from today's audit batch (reasoning rollback, placeholder CAS-before-splice, scheme stamp staging, adoption serialization).
- Do NOT restructure the pipeline order.
- If an optimization can't be proven byte-safe via the compare gate, report-and-skip.

## Gates
Byte-compare gate green over all real fixtures per optimization commit; packages/pi-plugin + packages/plugin: bun test, typecheck, lint; check_comments clean (invariant comments only). Report: before/after closed-gap phase tables on the 019de471-scale fixture + synthetic 5725, per-fix deltas, and the final total.
