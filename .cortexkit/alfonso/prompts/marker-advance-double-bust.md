# TS lane: marker-ADVANCE double bust (present -> absent flip on the pass after drain)

Branch from `subc-migration` HEAD. Cache-core (rule #4975): investigation with replay evidence BEFORE any fix. The first-apply direction of this class was fixed in commit f1027e17 (same-pass injection of the joined summary representation); this is the mirror residual on the ADVANCE path.

## Live incident (ALF session ses_227ce5788ffeRPA9THoPLOQreO, serve running the FIXED dist — verified, so this is a residual, not a stale binary)

Wire dumps (tmpdir opencode-anthropic-auth-dumps, copy them before they rotate):
- Pass A 2026-07-18T20-07-23-122Z-03872-...body.json: message[1] assistant content BEGINS with {"type":"text","text":"\u00a756879\u00a7 [Compacted by magic-context — session history is managed by the plugin]"} followed by the real text part.
- Pass B 2026-07-18T20-07-38-501Z-03875-...body.json: SAME assistant message with NO summary text part at all. First divergence at message[1]; bust from there (166K new tokens on a ~390K prompt).

Log window (magic-context.log, 2026-07-18T20:07:10): the pass before A ran a deferred_materialization execute with pendingOps=412, caveman x36, tool reclaim x21, "compaction-marker drain: removed old boundary at ordinal 39607, advancing to 40058" — i.e. this was a marker ADVANCE (old summary row removed, new one inserted at a later ordinal), not a first apply.

## Hypotheses to convict/eliminate with replay (in ranked order)

1. Old-marker removal asymmetry: the f1027e17 fix injects the NEW summary's joined representation into the drain pass's own output, but the drain also REMOVES the old marker row (ordinal 39607). If the old summary's text part was already part of the stable prefix (from its own apply months/hours ago) and the removal changes message[1] on the NEXT pass instead of the drain pass, the flip direction is inverted: pass A still renders the old/new summary (injected), pass B rebuilds input without the old row and message[1] loses the part. Check which message msg id owns the summary at wire index 1 in both dumps and which ordinal (39607 vs 40058) it corresponds to.
2. The injected representation joined into the WRONG anchor: the fix injects at the position where OpenCode's projection joins the summary; on advance, the join target may differ between the drain pass (old row still present in its input) and the next pass (old row gone), shifting which assistant the text merges into.
3. A strip path eating the part on B: summary-row exclusion in readRawSessionMessages, dangling-tag strip (the injected text carries tag 56879), or the system-injection heuristic (the drain pass dropped 9 system injections) removing the persisted summary part on the next pass's replay.

## Method (non-negotiable)

- Reconstruct pass B's input from opencode.db (read-only) around the boundary: which rows exist at ordinals 39600-40060 after the advance, which carry summary flags, and what the projection joins into message[1].
- Diff against pass A's dump to convict the exact mechanism with file:line in transform-postprocess-phase.ts / compaction-marker-manager.ts / tag-messages.ts.
- Only then fix. The invariant: across a marker ADVANCE, the wire representation of every message at or before the new boundary is byte-identical between the drain pass and every subsequent pass. Whatever representation the drain pass serves for the removed-old/inserted-new summary rows, subsequent passes must reproduce it exactly (or the drain pass must serve the post-advance representation up front, extending the f1027e17 approach to the removal side).
- Two-pass regression fixture for the ADVANCE case (old marker mid-history + new marker later; assert full wire byte-equality across the pair). The existing f1027e17 fixture covers first-apply only; keep it green.
- Cache-replay tests (defer replay byte-identity) and the full plugin suite.

## Gates

bun test packages/plugin full + focused postprocess/marker suites; report must include the conviction (mechanism + file:line), the reconstructed-row table around the boundary, and the regression's fail-first proof (test fails on pre-fix code). No em-dashes; comments explain invariants, not incident history.
