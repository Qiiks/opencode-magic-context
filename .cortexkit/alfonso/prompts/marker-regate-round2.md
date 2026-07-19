# Marker reconciler round 2: re-gate findings (2 HIGH + 1 LOW hardening)

Branch from `subc-migration` HEAD (271e3d9b). Cache-core (rule #4975). An independent re-gate on the merged reconciler returned NO SHIP with the findings below; everything else (both prior BLOCKERs, the HIGH crash window, steady-state dedup, LKG, rust-mode isolation, emergency/nudge index safety, deterministic-ID collisions, legacy-delete predicate) was explicitly cleared — do not touch those mechanics beyond what the fixes require.

## 1. HIGH — synthetic-head misclassification

transform-postprocess-phase.ts:194-201 with isSyntheticHeadMessage:102-110: the canonical-position walk accepts ANY synthetic:true message as head, so a synthetic TAIL message contiguous with m0/m1 (e.g. a Channel-2 promptAsync synthetic user nudge when the marker compacted everything before it) absorbs into the head and the summary inserts AFTER it: [m0, m1, channel2-user, summary, ...] — wrong canonical position, diverges from the next pass's projection, and the summary reads as an answer to the nudge.

Fix: identify the m0/m1 slots EXPLICITLY instead of by the generic synthetic flag. The injection path knows exactly which messages it prepended (inject-compartments.ts m0/m1 construction); mark them distinctly (dedicated info field or thread their ids/count through the postprocess args into the reconciler) and make the head walk consume exactly those. Regression: Channel-2 synthetic user as the first tail message; assert the summary lands BEFORE it and the wire is two-pass byte-stable through the merge serializer.

## 2. HIGH — todo synthesis anchors to the discarded raw summary

transform-postprocess-phase.ts:1251-1331 (todo synthesis, runs BEFORE the reconciler at ~1442) + transform-message-helpers.ts:80-109 (anchor selection): when the raw persisted summary is the latest replayable assistant (post-compaction shape: only the new user turn is retained), todo synthesis anchors the synthetic pair to summaryMessageId. The reconciler then discards that raw message and rebuilds a text-only copy, silently dropping the injected todo pair EVERY pass; the persisted (callId, messageId) keeps matching so it never re-anchors until todo state changes.

Fix both belts: (a) exclude info.summary === true assistants from todo-anchor eligibility in the selection path; (b) evaluate MOVING the reconciler invocation BEFORE todo synthesis as the primary fix — the re-gate cleared the other downstream mutators (emergency drops, note nudges, auto-search, Channel-1) as id-keyed or later-running, so verify that safety argument at source and make the move if it holds, keeping (a) as defense in depth. Regression: post-compaction shape where the raw summary is the only assistant; assert the todo pair survives reconciliation on a legitimate anchor (or the None-anchor head position) and stays byte-stable across multiple defer passes with unchanged todo state.

## 3. LOW hardening — provisional availability verdict can flip the marker tag prefix once

ctx-reduce-availability.ts:37-61 + transform-postprocess-phase.ts:180-182: a no-user pass returns provisional callable=true; the reconciler renders a TAGGED summary; when the first real user freezes a deny verdict the marker flips untagged — one bounded provider-visible flip. Thread the {callable, frozen} verdict into the reconciler and suppress the tag prefix while frozen=false. Test: provisional pass renders untagged, frozen-callable renders tagged, no oscillation after freeze.

## Gates

Keep the whole existing suite green (2979 baseline) including every marker fixture from rounds 1-2. Full plugin suite + focused marker/postprocess suites, typecheck, biome changed files, fail-first proof per finding. Report which fix shape you chose for finding 2 (move vs exclusion vs both) with the source-verified safety argument. Comments explain invariants, never audit rounds. No em-dashes.
