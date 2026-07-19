# Task: Pi parity — cache/scheduler gate fixes (4 findings)

Repo: this worktree (magic-context master). All changes in packages/pi-plugin (plus tests). Reference implementation: packages/plugin. Read packages/pi-plugin/PARITY.md first — it documents intentional divergences; your job is to fix ACCIDENTAL ones.

## Finding 1: Pi lacks the active-historian VETO on pending-op drain + heuristics

OpenCode (transform-postprocess-phase.ts:258-264, 327-332, 357-373) blocks pending-op drain and heuristic cleanup while a historian run is in flight (`compartmentRunning`), bypassed only by force-materialization (≥85%) or an m0 hard fold this pass (`bypassCompartmentGate`). Pi (context-handler.ts:3636-3645 heuristics gate, :3757-3774 pending ops) has NO in-flight-historian veto — it can mutate bytes mid-historian-run on a plain execute pass, causing an extra cache bust OpenCode would defer.

Fix: thread the in-flight historian signal (`inFlightHistorian.has(sessionId)` or equivalent) into both gates with the same shape as OpenCode: `(execute-class || hard-fold) && (!historianRunning || forceMaterialization || m0HardFoldThisPass)`. Mirror the emergency bypass semantics exactly (a hard fold or ≥85% force drains even while the historian runs — that is safe by the disjoint-DB model and is what OpenCode does).

## Finding 2: session-activation rehydration arms PENDING instead of gated DEFERRED materialization

Pi startup rehydration correctly signals deferred history + deferred materialization (index.ts:533-538), but the session switch-back path (index.ts:1036-1062) signals deferred history + PENDING materialization, forcing a mutation pass on a schedule OpenCode would keep deferred (gated by the deferred-consumption rules). Symptom: switching back to a session with a durable pending Pi marker produces a surprise cache bust.

Fix: make switch-back rehydration use the same gated deferred-materialization signal as startup rehydration. Verify the marker still drains on the next genuinely cache-busting pass (the coverage-gated drain must still see it).

## Finding 3: stale ctx_reduce cleanup missing the provider gate

OpenCode strips stale ctx_reduce calls ONLY when the provider accepts empty sentinels (`canUseEmptySentinels`, transform-postprocess-phase.ts:747-765). Pi (heuristic-cleanup-pi.ts:359-391) strips them unconditionally. On providers that reject empty content this changes visible history vs OpenCode.

Fix: gate Pi's stale-reduce strip on the same provider condition (Pi has modelAcceptsEmptyContent available — see how strip-content consumers use it on the Pi side). Frozen-id replay semantics must be preserved (the strip stays deterministic across passes for already-stripped ids regardless of the gate — only NEW strips are gated).

## Finding 4: emergency-recovery disarm missing the real-pressure guard

Pi disarms a stuck emergency-recovery flag inside the ≥95% emergency block when no historian is in flight and no eligible history exists (context-handler.ts:2094-2112). The comment cites "mirroring OpenCode transform.ts:745", but OpenCode's current behavior (transform.ts:1094-1111) KEEPS recovery armed at the no-head condition and records it. PARITY.md itself documents the intended Pi disarm as gated on real pressure below the force threshold.

Fix: add the real-pressure guard to the disarm: only clear when REAL pressure (actual current usage, not the emergency-bumped value) is below FORCE_MATERIALIZATION_PERCENTAGE, matching PARITY.md's documented intent. Keep the no-eligible-history + no-in-flight conditions. Update the stale comment (remove the dead transform.ts:745 reference; explain the invariant: the flag must survive while the session is genuinely oversized, because disarming early re-exposes the session to overflow sends; it may clear once the user has freed context below the force threshold).

## Tests (parity regression tests, non-vacuous)

- Veto: with an in-flight historian marker set, an execute pass does NOT drain pending ops / run heuristics; with force-materialization it DOES; without the historian it does.
- Rehydration: switch-back rehydration leaves materialization deferred (no mutation on the next defer pass); the pending marker drains on the next execute/hard pass.
- Provider gate: stale-reduce strip skipped for a non-empty-content provider, applied for anthropic; already-stripped ids replay on both.
- Disarm guard: armed recovery + no eligible history + real pressure ≥ force threshold → stays armed; real pressure below → clears.

## Gates

cd packages/pi-plugin && bun test --timeout 60000 (all green), bun run typecheck, repo-root bun run lint, check_comments. Do not touch packages/plugin behavior (OpenCode side is the reference, not the target). Commit with trailer: `Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>`
