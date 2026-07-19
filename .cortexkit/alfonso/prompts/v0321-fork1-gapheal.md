# Fork 1 — gap-heal strict (Option A) for v0.32.1

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Decision (data-backed): heal ONLY tool-only gaps; drop the `<=15 SAFETY_HEAL_GAP` escape that silently absorbs real narrative. Verify each site at source first. Disjoint from other running masons (touches only the two validators + a runner re-read test).

## Evidence behind the decision
A probe replayed 14 real chunks (92 compartments) through deepseek-v4-flash (the calibration-floor historian model, production conditions) across the most tool-heavy session + a narrative one: ZERO non-tool-only heals, zero narrative loss. flash produces contiguous coverage, so the `<=15` escape is not load-bearing for any real model, and dropping it causes no folding regression while eliminating the silent-loss class.

## Fix (both harnesses — identical logic)
- TS: packages/plugin/src/hooks/magic-context/compartment-runner-validation.ts, `healCompartmentGaps` (~:29-56). Remove the `|| gapSize <= SAFETY_HEAL_GAP` branch. Heal a gap ONLY when `fullyInsideToolOnly`. Delete the now-unused SAFETY_HEAL_GAP const.
- Rust: crates/mc-module/src/historian_validate.rs, `heal_compartment_gaps` (~:792-820). Remove the `|| omitted_present.len() as u64 <= SAFETY_HEAL_GAP` branch (keep `fully_inside_tool_only`). Remove the SAFETY_HEAL_GAP const (~:17).
- Result: a non-tool-only gap now fails coverage validation instead of being absorbed → the chunk is rejected.

## Load-bearing verification (do NOT assume — prove at source + test)
1. REJECT→RE-READ, no loss: when a chunk fails validation for a non-tool gap, confirm the runner does NOT advance the compartment boundary — the skipped messages must be re-read on the next run (existing retry / discard-last path). Trace the runner (compartment-runner-incremental.ts + the Rust historian publish path) to confirm rejection leaves coverage unchanged. Add a NON-VACUOUS test: a chunk whose parsed compartments leave a real-narrative gap → validation rejects → boundary/coverage unchanged → the gap ordinals are still eligible next run. If rejection ADVANCES coverage anywhere, STOP and report — that would mean A loses data and needs a different fix.
2. NO LIVELOCK on filtered noise: confirm filtered-noise ordinals (system notifications, empty messages) do NOT create rejectable gaps. My source trace says readSessionChunk pre-absorbs them into the adjacent block's ordinal span (recordFilteredNoise → pendingNoiseMeta → included in the next block's `meta`/startOrdinal), so they never appear as a gap. VERIFY this holds and add a test: a compartment boundary adjacent to filtered-noise ordinals does NOT produce a non-tool gap (no spurious rejection, no repeated-reject loop).

## Tests
- non-tool gap size 5 (real narrative between compartments): previously healed (absorbed), now REJECTS the chunk. Both harnesses.
- tool-only gap of any size (e.g. 20): still heals (fullyInsideToolOnly). Both harnesses.
- reject→re-read preservation test (#1 above).
- filtered-noise-adjacent boundary → no spurious gap (#2 above).
- TS/Rust parity: same gap input → same heal/reject decision.

## Gates
packages/plugin (bun test, typecheck, lint) + crates (cargo test -p mc-module, clippy, fmt) + check_comments. Comments explain WHY (data: flash contiguous; tool-only is the only safe absorb; non-tool gaps reject→reprocess via re-read). Report per-item status + the source-trace conclusion for verification #1 and #2 + test evidence.
