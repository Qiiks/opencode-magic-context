# Fork 2 — emergency fail-closed (v0.32.1)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Ruling: at ≥95% usage, when the historian fold does NOT land and we'd otherwise ship an overflowing prompt, FAIL CLOSED with an actionable message instead of shipping a doomed provider request. Verify the emergency path at source first.

## The bug (W1-C, verified)
The ≥95% barrier is publication-blind: if the fold doesn't happen (wrapup lease held elsewhere / no candidates / throw), the pass returns emergency output anyway and can send a prompt that overflows the provider → cryptic 400.

## OpenCode fix (primary — mechanism CONFIRMED by the OC peer)
Sites: packages/plugin/src/hooks/magic-context/transform.ts + transform-postprocess-phase.ts (the ≥95% / FORCE_MATERIALIZATION emergency arm), and the emergency-drop path.
Sequence at ≥95% when fold failed:
1. Run emergency tool-drops FIRST (existing tiered emergency drop) — claw back headroom.
2. Re-estimate input tokens after drops. If now under the model limit → proceed normally (salvaged, no abort).
3. If STILL over the model limit BY MORE THAN the estimator's error margin (conservative — do not abort on a marginal/under estimate; only when clearly over) → FAIL CLOSED:
   a. FIRST send an RPC→TUI notification with actionable text: "Context full — /ctx-flush or /clear to continue." (use the existing sendIgnoredMessage / notification path; must land before the abort because the interrupt detaches the hook Effect).
   b. THEN `await client.session.abort({ sessionID })` — awaited, never fire-and-forget (a fire-and-forget abort races into provider setup). The awaited self-abort interrupts the run fiber via onInterrupt BEFORE the provider request is created (OC-verified against Runner.make).
Do NOT throw from the transform (crashes the prompt loop). Do NOT return an empty/1-element array (OC confirmed both send a real request → provider 400).
This fires ONLY in the narrow ≥95% + fold-failed + still-over-by-margin case. Everywhere the turn is salvageable, never abort.

## Pi parity
Investigate packages/pi-plugin/src/context-handler.ts emergency path. Pi already has a forward-pressure floor + emergency recovery. If Pi has a clean turn-abort primitive reachable from the context handler, mirror the notify-then-abort. If it does NOT (likely — Pi's model differs), keep Pi's current best-effort emergency behavior but ADD the loud actionable notification, and document the divergence in packages/pi-plugin/PARITY.md (OpenCode aborts fail-closed; Pi notifies + best-effort). Do NOT invent a Pi abort primitive — ask via a note in the report if uncertain.

## Cache safety
This is additive to the emergency arm (only runs ≥95% fold-failed). It must NOT change steady-state or defer-pass bytes. Do not touch the replay/representation paths.

## Tests
- ≥95%, fold-failed, post-drop still-over → notification sent THEN session.abort awaited (mock client.session.abort; assert order: notify before abort).
- ≥95%, fold-failed, post-drop UNDER limit → no abort, proceeds.
- marginal over-estimate (within margin) → no abort (no false-positive turn break).
- Pi: parity test or documented-divergence test as applicable.

## Gates
packages/plugin (+ packages/pi-plugin if touched): bun test, typecheck, lint, check_comments. Comments explain the invariant (why fail-closed beats a doomed 400; why notify-before-abort). Report per-item status + test evidence + any Pi uncertainty.
