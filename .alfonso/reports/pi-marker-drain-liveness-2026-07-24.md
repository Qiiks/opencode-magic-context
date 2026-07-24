# Pi deferred compaction-marker drain: liveness gap (m0-only coverage read)

Date: 2026-07-24
Status: diagnosis complete — product fix deferred to a separate cache-safety-reviewed task.
Failing test: `packages/e2e-tests/tests/pi-long-running-session.test.ts`, phase 5
("Pi native compaction entry written", the `waitFor` at lines 385-388). The test is
left RED on purpose: it points at the real gap below.

## Summary

After a normal Pi historian publication, the staged deferred compaction marker
never drains into a native JSONL `compaction` entry unless an unrelated HARD bust
later re-materializes m[0]. A plain user drive turn (execute / force-materialization
pass) reaches the drain code but is skipped every time, because the Jul-6 seam-fix
coverage gate reads the "rendered boundary" from the **m[0] snapshot markers only**,
while every fresh publication renders its compartment into **m[1]** (m[0] folds new
compartments only on a HARD bust). Coverage therefore can never be satisfied by the
publication that staged the marker — it needs a later HARD fold.

This is stricter than OpenCode (which drains on the consuming pass) and stricter than
the deferred-marker doctrine ("deferred work rides the NEXT BUST CYCLE" — an execute
pass is a bust cycle, but it does not advance the m[0] boundary).

## What changed, and when

- Gate introduced: commit `174320e3` "mason: fix pi deferred marker seams"
  (2026-07-06). It added `pendingPiMarkerCoveredByRenderedBoundary` and made the
  deferred-marker drain conditional on it.
  - Gate definition: `packages/pi-plugin/src/context-handler.ts:4057-4066`.
  - Gate use in the drain: `packages/pi-plugin/src/context-handler.ts:5181-5227`
    (inside `runPipeline`, under `deferredHistoryDrainEligible`).
  - Pre-seam-fix behavior (visible in the `174320e3` diff): the drain applied
    `applyDeferredPiCompactionMarker` whenever a pending marker existed and
    `appendCompaction`/`readBranchEntries` were available — **no rendered-boundary
    coverage requirement**. So pre-Jul-6 an execute pass after publish drained the
    marker; post-Jul-6 it cannot until the rendered boundary covers the ordinal.
- Test ported before the gate: `b8f9a2d4` (2026-06-11, partial Pi ballast port). The
  test's phase 5 assumes the pre-seam-fix "drive turn drains" behavior.
- Exposure: `dcf5d14c` (2026-07-24) fixed the e2e historian fixtures to emit valid v2
  tiered compartments, so publication now succeeds and the test progresses far enough
  to hit the downstream drain gap (previously it failed earlier, at publication).

## The mechanism (m0-only coverage read)

The drain gate:

```
// packages/pi-plugin/src/context-handler.ts:4057
function pendingPiMarkerCoveredByRenderedBoundary(pending, injection): boolean {
    if (!injection || injection.contentionExhausted) return false;
    const boundary = injection.renderedBoundary;
    if (pending.endMessageId === boundary.endMessageId) return true;
    return boundary.ordinal !== null && pending.ordinal <= boundary.ordinal;
}
```

`injection.renderedBoundary` is produced by `injectM0M1Pi`:

```
// packages/pi-plugin/src/inject-compartments-pi.ts:2543-2547
const boundaryId = findCompartmentBoundaryForSnapshot(markers);
const renderedBoundary = resolveRenderedCompartmentBoundary(currentCompartments, boundaryId);
```

`markers` are the **m[0] snapshot markers** (from a fresh m[0] materialization or the
cached m[0] replay). `resolveRenderedCompartmentBoundary`
(`inject-compartments-pi.ts:2298-2311`) returns `{ endMessageId: null, ordinal: null }`
when `boundaryId` is null — i.e. when the m[0] snapshot has no compartment boundary.

The key asymmetry: **new compartments are an m[1] delta, not an m[0] trigger.**

```
// packages/pi-plugin/src/inject-compartments-pi.ts:1159-1162 (mustMaterializePi)
// new_compartment is NOT a trigger (parity with OpenCode — Bug 1 fix): new
// compartments are an m[1] delta (renderM1Pi readNewCompartments WHERE
// sequence > cachedM0Seq ...), folded into m[0] only on a HARD bust.
```

So after a publication:
1. The historian stages the pending marker (`pi-historian-runner.ts:1223-1232`,
   `setPendingPiCompactionMarkerState`) and `onPublished` signals a deferred
   history-refresh + materialization (`context-handler.ts:3495-3496`).
2. The new compartment renders into **m[1]** (the delta). **m[0] is not
   re-materialized** (new_compartment is not a HARD trigger), so the cached m[0]
   stays the pre-publication placeholder and its snapshot markers carry **no**
   compartment boundary.
3. On the next execute / force-materialization pass the drain block IS entered
   (`deferredHistoryDrainEligible` is true), but
   `pendingPiMarkerCoveredByRenderedBoundary` sees `renderedBoundary = <none>`
   (ordinal null) and skips, preserving the deferred signals.
4. The m[0] boundary only advances when a **HARD bust** re-materializes m[0]
   (`mustMaterializePi`: model_change / system_hash / ttl_idle / project_change /
   project_memory_change / pending_mutations / renderer_upgrade /
   compartment_render_epoch). Only then does `markers` carry the compartment boundary,
   `renderedBoundary` cover the pending ordinal, and the drain finally apply.

A normal publication bumps **none** of the HARD triggers. In particular
`pending_mutations` reads `m0_mutation_log`
(`inject-compartments-pi.ts:1022` → `getMaxM0MutationId`,
`storage-m0-mutation-log.ts:117`), and that log is written **only** by
recomp/merge/upgrade/delete (`queueM0Mutation`, `storage-m0-mutation-log.ts:48`;
callers in `compartment-runner-recomp.ts`), never by normal publication.

Net effect: in a stable session (no model change, no memory/project mutation, no
ttl-idle, no recomp) the native compaction trim never happens after a normal
publication; the pending marker stays staged indefinitely. (Context-window compaction
is unaffected — the m[1] delta carries the summary immediately; only the native
`getBranch()` trim / JSONL `compaction` entry is delayed.)

## OpenCode-vs-Pi asymmetry

OpenCode's deferred-marker drain does **not** gate on a rendered m[0] boundary:

```
// packages/plugin/src/hooks/magic-context/compaction-marker-manager.ts:208
export function applyDeferredCompactionMarker(db, sessionId, pending, directory?) {
    ...
    const boundary = findBoundaryUserMessage(sessionId, pending.endMessageId); // raw messages
    ...
}
```

OpenCode resolves the trim boundary directly from the raw message list
(`findBoundaryUserMessage`), independent of whether m[0] has folded the compartment.
So OpenCode drains on the consuming pass. This is why the OpenCode long-running twin
(`packages/e2e-tests/tests/long-running-session.test.ts`, phase 6, turns 19-21)
drives its marker drain with ordinary send turns and needs no HARD injection.

Pi's seam-fix gate added the rendered-m[0]-boundary requirement that OpenCode does not
have. The gate's *intent* was sound ("don't drain a marker for a compartment that
isn't rendered anywhere"), but the *implementation* reads "rendered" from the m[0]
snapshot markers only, so an m[1]-rendered compartment — where every fresh publication
lands — never satisfies coverage until a HARD fold moves it into m[0]. That is the
accidental asymmetry.

## Empirical proof (this worktree)

Built `packages/pi-plugin` (`bun run build`) and ran the e2e against the mock provider.

1. Drive execute turns only (pressure turn + execute turns after the publish wait, no
   HARD bust): the test fails with `compactions.length === 0`. The MC log
   (`$TMPDIR/pi/magic-context/magic-context.log`) shows the drain entered and skipped
   on every execute pass:
   - `pending ops WILL APPLY — reason=deferred_publication ...`
   - `heuristics WILL RUN — reason=force_materialization ...`
   - `injected m[0]/m[1] into Pi messages (35 + 518 bytes, materialized=false)`
   - `Pi compaction-marker drain skipped: pending ordinal 7 is newer than rendered boundary <none> endMessageId=<none>; preserving deferred signals`
   DB state at failure: 3 compartments published (seq 0:1-2, 1:3-4, 2:5-7),
   `pending_pi_compaction_marker_state` still staged at ordinal 7, 0 JSONL
   `compaction` entries, `compartment_state_lease` empty (historian finished).

2. Same drive turns PLUS one HARD bust (a single `m0_mutation_log` row,
   `mutation_type='recomp_boundary_change'`, inserted via the test's existing `writeDb`
   helper): the whole test passes — all 8 phases, 27 assertions, including
   `fromHook === true` and `pending_compaction_marker_state === null`. The HARD bust
   re-materializes m[0] with the compartment boundary, coverage is satisfied, and the
   drain writes the native entry.

This proves both halves: (a) a drive turn alone is insufficient (the m0-only coverage
read blocks it), and (b) a HARD bust is sufficient (it advances the m[0] boundary).

## Suggested product fix (for the separate cache-safety-reviewed task)

Make coverage satisfy when the compartment is rendered in **m[0] OR the current m[1]**,
rather than m[0] only — i.e. the drain may apply once the pending ordinal is covered by
what the model actually sees this pass (m[0] fold boundary OR the m[1] new-compartment
delta watermark). Concretely, `pendingPiMarkerCoveredByRenderedBoundary`
(`context-handler.ts:4057`) should accept coverage from the m[1]-rendered compartment
set (e.g. the latest compartment sequence rendered into m[1] this pass), not solely
`injection.renderedBoundary` from the m[0] snapshot markers. This restores parity with
OpenCode's consuming-pass drain and the "next bust cycle" doctrine.

Cache-safety note: the original gate exists to avoid trimming `getBranch()` to a
boundary the model hasn't been shown. Any relaxation must confirm the m[1] delta has
actually rendered the covering compartment this pass (so the trimmed messages are
already summarized in what the model sees) before allowing the native trim — hence a
dedicated, reviewed task rather than an inline test-side workaround.

## Why the test is left red

Injecting an artificial HARD bust into the test would make it pass while encoding the
accidental m0-only behavior as intended — masking the liveness gap. The test stays red
until the product fix above lands; a red test pointing at a real liveness gap is doing
its job.
