# Build: LKG transform replay (OpenCode leg + rust-mode adapter)

Implement `.alfonso/plans/lkg-transform-replay.md` (v6) EXACTLY. It is the build spec; five adversarial review rounds shaped every rule in it, so no rule is optional and no "simplification" is permitted without flagging it as a deviation in your report. Read the whole plan first.

Repo: this worktree (branched from `subc-migration` HEAD). Language: TypeScript (plugin) only; the rust-mode adapter changes are in the plugin's TS files too. No Rust changes.

## Units, in build order

### 1. Typed pass outcome + degradation accumulator
- New `packages/plugin/src/hooks/magic-context/pass-outcome.ts`: a pass-local accumulator object created at transform entry, threaded through the pipeline (transform.ts and transform-postprocess-phase.ts), collecting typed degradations `{ site: string, kind: "degraded" | "fatal" }`.
- Convert every output-affecting swallowed failure site listed in the plan's Capture section to record into the accumulator. The plan lists the mandatory sites; locate each in code (they are the existing try/catch + early-return sites) and record precisely there. Helpers that currently return void and swallow internally (auto-search runner outcomes) must return typed outcomes consumed by the caller.
- `captureEligible = finalized && degradations.length === 0`. "finalized" means the pass reached the end of postprocess including provider-sensitive normalization.

### 2. LKG slot store
- New `packages/plugin/src/hooks/magic-context/lkg-slot.ts`: MODULE-SCOPE (process-global) store, one slot per session id, LRU bounded by total bytes (64MB, accounted as `2 * jsonPrefix.length + 256`), single-slot oversize rejection at 24MB. API: `captureSlot`, `getSlot`, `dropSlot(sessionId, reason)`, `noteEntry(sessionId, messages)` (pristine tail + entry id array), plus a test-only reset.
- Slot shape exactly as the plan: `{ jsonPrefix, inputIdSeq, lastInputMessageId, modelKey, providerKey, capturedAt }`.

### 3. Capture
- At end of a fully-finalized pass with `captureEligible`: determine the anchor (newest REAL user message not inside an active assistant/tool arc; reuse the release-valve discriminator semantics — top-level `info.synthetic`, strict time comparison vs latest assistant, latest assistant finish/unexecuted-tool state; decline capture if unreadable). Classify every output message as prefix or post-anchor per the plan's prefix-cut mapping rule; unclassifiable → decline. Serialize prefix to `jsonPrefix` (JSON.stringify); stringify failure → decline. Record `inputIdSeq` = ordered input ids through the anchor; duplicate ids → decline.
- Capture on every eligible pass including defers. No throttle.

### 4. Replay
- In `packages/plugin/src/plugin/messages-transform.ts`: at entry, if a slot exists, `noteEntry` deep-clones (structuredClone) input messages strictly after `slot.lastInputMessageId` and snapshots the entry-time visible id array. In the catch: follow the plan's replay ladder EXACTLY, in order: (1) rethrow `EmergencyFailClosedError`; (2..) armed-registry/needsEmergencyRecovery fail-closed check, slot presence, model/provider match, exact id-sequence validation against the ENTRY-TIME array (equal length through anchor, index-zero, anchor terminal), seam defensive check, materialize `parse(jsonPrefix) ++ pristineTail`, serializer-projection seam validation, serve. Every decline path: drop slot where the plan says so, loud log with the plan's reason strings, raw passthrough.
- New typed `EmergencyFailClosedError` in the emergency path (transform.ts emergency branch): every failure inside that branch (abort failure, notification failure, missing client) throws it. Outer wrapper rethrows it.
- Emergency armed registry: module-scope set; ARM inside `recordOverflowDetected` BEFORE the durable write; CLEAR inside `clearEmergencyRecovery` only AFTER the durable clear succeeds. Find all callers; do not add new arming sites.

### 5. Rust-mode adapter
- In the rust-mode transform path (`rust-mode-transform.ts`): retain the module's last successful `native_messages` response string as the slot's jsonPrefix analogue (the module output IS the full array — for rust mode the anchor is the newest input message consumed by that pass, and the tail is anything the module hasn't seen; keep it simple: full-array slot + empty tail is correct here because the adapter appends nothing). On module failure (transport/timeout/route churn), replace the current raw-passthrough with the same replay ladder (id-sequence validation over the full consumed set; no seam question when the tail is messages the module never consumed — append them verbatim). Park-after-3 semantics unchanged, but parked passes serve replay instead of raw.

### 6. Invalidation wiring
- Drop the slot at every surface the plan lists: `onSessionCacheInvalidated`, `message.removed`, `session.compacted`, recomp start, revert detection, model-change (including hook-handlers path), overflow arm (via the armed registry — also drop slot), session deletion.

### 7. Tests + benchmark (merge gates)
- Fail-first tests from the plan's Merge gates section, every one: early-anchor id-uniqueness (mutant: full-output snapshot), marker-advance validation set (shifted start, missing anchor, suffix-comparison mutant), emergency-armed replay attempt through the OUTERMOST handler (notification-failure, missing-client, abort-failure variants), seam split of a tool run declines, degraded passes decline capture (exercise each accumulator site with a forced failure), pristine-tail test (transform mutates a post-anchor tool message then throws; replay serves original nested content).
- Corpus test: provider-visible equality before/after stringify/parse round-trip on real-shaped fixtures.
- Benchmark script `packages/plugin/scripts/bench-lkg-capture.ts`: p95 capture latency (prefix stringify + id-seq build) and RSS delta on synthetic 2MB/6MB/500-message arrays. Report numbers in your final report. >25ms p95 on the large corpus = report it loudly as a merge blocker, do not tune it away silently.

## Constraints
- TS transform behavior on HEALTHY passes must be byte-identical (capture is observation-only). Prove with existing cache-invariant suites passing unchanged.
- No config knob. No em-dashes in comments. Comments explain invariants, never review rounds or this plan.
- Do not touch Pi files (Pi parity is a follow-up unit; add the PARITY.md pending entry).

## Gates
`bun test` full plugin suite; `bun run typecheck`; biome. Report per-unit files, test names, benchmark table, and any deviation from the plan explicitly.
