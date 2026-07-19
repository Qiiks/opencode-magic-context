# Pi hot-path perf fixes (blind audit round 2 — post-#224)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. A blind perf audit (source-verified citations, HEAD ~e02f5102-era) found the remaining growth costs on Pi's per-pass hot path. IMPORTANT COORDINATION: another mason is concurrently fixing CORRECTNESS findings in some of the same files (context-handler.ts, tag-transcript.ts, strip-placeholders). You are LAST to merge: rebase/merge onto the branch tip before finalizing and resolve semantically. Do not touch the files' correctness-fix regions except to integrate.

The byte-identity law applies to every change here: your fixes must not alter output bytes on any pass. The committed compare harness (packages/pi-plugin/scripts/experiments/perf/) is your proof tool — run the byte-compare gate over the real fixtures after each change (see its README/script flags; it compares baseline-vs-optimized outputs + tag rows over sessions on disk).

## The findings (verify each at source first; citations approximate)

### F1 (Critical): historian pre-gate defeated — eager boundary resolution before the cheap gate
packages/pi-plugin/src/context-handler.ts:3272-3361: maybeFireHistorian eagerly calls resolveRunnablePiBoundarySnapshot() (full tag Map load + full branch conversion + token index + tool arcs + fingerprint, protected-tail-boundary.ts:655-688/383-400) BEFORE checkCompartmentTrigger's cheap upper-bound gate. And if the gate doesn't skip, getUnsummarizedTailInfo does ANOTHER provider-backed full read because Pi supplies neither inMemoryTail nor a tag floor (compartment-trigger.ts:299-355). Θ(messages+tags), possibly twice, every historian-enabled pass.
FIX: call checkCompartmentTrigger FIRST (cheap gate), use its returned boundarySnapshot when it fires; resolve a snapshot lazily only where actually required (first-pass failure recovery / refresh callback). Mirror the OpenCode ordering (transform.ts does gate-first). Also supply the Pi tail/floor inputs to getUnsummarizedTailInfo the way OpenCode does so the gate itself stays cheap.

### F2 (High): full-history work before the boundary trim
context-handler.ts:1663 (whole-wire clone), 3544-3551 (full conversion), tag-transcript.ts:413-609 (full walk), with the compartment-boundary trim only near the end (4432-4441). Compacted history pays nearly the whole transform cost before being discarded. Plus tag-transcript.ts:406-410 initializes the Pi tagger at floor 0 (loads ALL persisted tags; the shared Tagger already has tail-scoping).
FIX: (a) apply the known cached-boundary trim BEFORE conversion/tagging (keep the existing late trim for boundaries published mid-pass); (b) derive a tag floor from the first retained stable message and initialize the Pi tagger scoped (parity with OpenCode's live-derived floor, memory of that design: floor from the first wire ids, revert-safe, never persisted). The trim must be provably byte-safe: only trim what injectM0M1Pi would trim anyway on this pass; when in doubt (no cached marker), don't.

### F3 (High): incremental message-index reschedules + full branch rebuild on the event loop
context-handler.ts:1743-1748 schedules the latest user message every context event; message-index-async.ts:175-210 dedup only covers the 100ms window (no completed watermark); the deferred callback rebuilds ALL Pi raw messages then .find()s one (context-handler.ts:417-424) — synchronous Θ(session) on the single event loop.
FIX: completed (sessionId,messageId) watermark to skip rescheduling; pass the already-located latestUserEntry converted directly instead of re-reading the branch.

### F4 (High on execute passes): pending-op replay loads + sorts the whole tag table
context-handler.ts:3811-3816 getTagsBySession when no preload; apply-operations.ts:307-343 filters/sorts ALL tags; active tags fetched AGAIN at 4293-4295.
FIX: scope the read (pending ids + protected-tail actives + tool candidates via SQL ORDER/LIMIT), or fetch once and reuse across both sites. Check EXPLAIN QUERY PLAN for the status IN ('active','dropped') trigger query (storage-tags.ts:293-320) — if it scans the generic session index, add one combined partial index (new migration + LATEST_SUPPORTED_VERSION bump + fresh-DB schema + migrations-vN test, per repo convention).

### F5 (Medium/High): auto-search bootstrap before decision check
context-handler.ts:4422-4424 runs ensureProjectRegisteredFromPiDirectory (sync config load + JSONC parse + SQLite write) BEFORE auto-search-pi.ts:267-290 checks the durable per-message decision.
FIX: decision check first; cache registration per (project, config fingerprint) so repeat passes skip the sync load.

### F6 (Medium): repeated fat meta reads + redundant BPE on defer passes
storage-meta-session.ts:74-80 selects the full session row INCLUDING cached m0/m1 blobs; ~8 reads per pass across scheduling/injection/commit/accounting (context-handler.ts:1809,3697,3738,3885,4753; inject-compartments-pi.ts:895-912). mustMaterializePi re-fingerprints project docs synchronously even on cache hits (project-docs-hash.ts:83-92). Defer passes BPE-tokenize decoded m0+m1 unconditionally (context-handler.ts:2261-2273).
FIX: one per-pass skinny meta snapshot threaded through (blobs fetched only at injection); persist cached m0/m1 token counts alongside the blobs and recompute only when bytes change; docs fingerprint gated to materialization-check passes that actually need it (keep semantics: docs changes still fold in on natural HARDs).

### F7 (Medium, memory): tagger cleanup + auto-search decision growth
context-handler.ts:5167-5195 clearContextHandlerSession never calls tagger.cleanup (per-session maps retained forever; floor-0 init makes them total-history). storage-meta-persisted.ts:1333-1405 auto-search decisions: whole-array JSON copy + linear dedup per append, parsed every pass, pruning keeps every visible message.
FIX: wire tagger.cleanup into session cleanup; cap/structure the decisions (keyed retention: all hint entries + only latest no-hint per message, or a keyed table if smaller-effort — your call, justify).

### F8 (harness): close the blind spots
The perf harness never exercises historian/execute/auto-search lanes (run.ts:110-121 registers none; usage pinned 0.25% at 192-217) and measures the handler but not eventual serialization. Add lanes: historian-enabled at low/high pressure, execute pass with compacted historical tags, auto-search sticky-replay, and a post-pass 150ms drain that records deferred event-loop work. Publish before/after phase tables for F1-F6 from the harness in your report.

## Order of work
F1 → F2 → F4 (the growth trio), then F3/F5/F6/F7, F8 alongside. Commit per finding or per coherent pair. If any fix risks byte divergence and can't be proven safe by the compare gate, SAY SO and skip it rather than shipping it.

## Gates
Byte-compare gate green over all real fixtures for every commit; cd packages/pi-plugin && bun test; cd packages/plugin && bun test (shared files); typecheck+lint; migration test if F4 adds an index; check_comments clean (invariants, not audit refs). Report: per-finding before/after numbers from the harness + growth curve at 1000/3000/5725 synthetic messages.
