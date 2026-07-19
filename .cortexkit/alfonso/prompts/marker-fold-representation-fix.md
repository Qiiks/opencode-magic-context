# Fix: compaction-marker summary representation must be stable across the fold boundary (TS lane double-bust)

Repo: this worktree (branch from `subc-migration` HEAD). TS plugin. Full investigation report at .alfonso/aft-bust-report-2026-07-17.md IN THE PARENT REPO — it is gitignored, so READ IT from /Users/ufukaltinok/Work/Projects/CortexKit/magic-context/.alfonso/aft-bust-report-2026-07-17.md (absolute path) before coding. Wire dumps referenced there are still on disk.

## Proven mechanism (from the report; verify at the cited lines, do not re-derive)

On a marker-consuming m0 fold pass (AFT session 20:38:16Z), the transform serves its in-memory array WITHOUT any representation of the compaction summary row that the deferred marker apply writes into opencode.db during that same pass's postprocess (transform-postprocess-phase.ts:1259-1317 applies the marker AFTER the input array was assembled). On the NEXT pass, OpenCode's rebuilt input contains the completed summary assistant row; its provider projection joins the summary text with the first tail assistant, so wire message[1] gains a prepended text part ("§N§ [Compacted by magic-context ...]") and the prefix busts a second time (106,436 cache-write tokens on the observed incident).

Cost: every marker-consuming fold pays a second full-prefix bust one pass later. This is a long-standing latent hole (deferred-marker design, May), newly visible because folds on 500K+ sessions are expensive.

## Fix direction (pick after verifying, justify in report)

The invariant: the pass that applies the deferred marker must serve a wire whose next-pass representation is byte-identical — the next pass discovering a NEW adjacent assistant text part is forbidden.

Preferred shape (a): when postprocess applies the deferred marker in a pass, inject the same summary representation into the CURRENT in-memory output at the position it will occupy in the next pass's input (an assistant message with the static marker text, matching byte-for-byte what OpenCode's projection will produce from the DB row — including the §N§ tag prefix the tagger will assign; the tag id must be deterministic at apply time, check how the summary row gets tagged on the next pass and reproduce exactly). This makes the fold pass and the following defer serve identical bytes.
Alternative (b): move the marker DB apply BEFORE input assembly on the fold pass (so the current pass's input already contains the summary row). This touches pass ordering — only choose it if (a)'s byte-prediction is infeasible; justify.
Reject (c) un-deferring back to publish-time apply — that reintroduces the historian-publish bust the deferral exists to avoid.

CAUTION on (a): the projection JOINS the summary text into the FIRST TAIL ASSISTANT (it does not appear as its own wire message — see the report's wire proof: message[1].content gained a text part before the existing tool_use). Your injected representation must reproduce that joined shape, not add a standalone message. Study how OpenCode's provider projection merges the summary row with the adjacent assistant before writing any code (consult filterCompacted/serialization behavior; if genuinely ambiguous, ASK the OC peer question via your report rather than guessing).

## Tests

- Two-pass wire fixture: marker-consuming fold pass, then a defer pass over the rebuilt input (including the summary row as OpenCode would present it); assert message-level byte identity for the first tail assistant across the two passes (fail-first: current code produces the divergence).
- Non-marker fold passes unaffected (byte-identity suite must stay green).
- The existing deferred-marker CAS/restart-safety tests stay green (the deferral machinery itself is not being removed).

## Gates
Focused suites (compaction-marker, transform-postprocess, inject-compartments, cache-invariant suites) + full bun test in packages/plugin + typecheck + biome. This is cache-core: byte-level assertions are the acceptance bar, and any deviation from proposal (a) needs explicit justification in the report. Comments explain the two-store timing invariant. No em-dashes.
