# Fork 2 round 3 — NARROW fail-closed to provider-proven overflow (v0.32.1)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Ruling (Ufuk + adversarial audit): the numeric final-wire gate cannot be made trustworthy inside `messages.transform` (stale system tokens, incomplete part walk, tool-set identity gaps, cold-start pre-auth limits) without reimplementing provider-accurate accounting in TS — which is deferred to the Rust module lane. So Fork 2 NARROWS: abort ONLY on provider-proven overflow; drop the proactive numeric gate entirely. Verify everything at source.

## Changes

### 1. Origin-split the recovery arm (fixes the conflation)
`needs_emergency_recovery` currently conflates two origins: (a) PROVIDER-PROVEN overflow — a real provider context-overflow error was parsed and `detected_context_limit` persisted (overflow-detection.ts path), and (b) PROACTIVE model-shrink arming (#188 — last measured input exceeds a smaller model's trusted limit, no overflow ever happened). Add an origin discriminant (e.g. persisted alongside the flag: `emergency_recovery_origin: "provider_overflow" | "proactive_model_shrink"` — pick the storage shape that matches how the flag is persisted today; keep it replay-safe). Arms from the #188 path set proactive; arms from parsed provider overflow set provider_overflow.

### 2. Fail-closed abort ONLY for provider-proven overflow
In transform.ts, replace the `evaluateEmergencyFailClosed` numeric+untrusted gate with a narrow predicate:
- abort-eligible iff: usage >= 95 AND recovery is armed with origin === provider_overflow AND the historian fold did NOT land a materialized reclaim this pass (keep whatever fold-landed signal exists purely as "did we just reclaim" — if the fold materialized this pass, let the turn proceed; the next pass re-evaluates).
- When abort-eligible: keep the existing notify-then-`abortSessionFailClosed` sequence EXACTLY (awaited notify first, throwOnError abort requiring data===true, throw on failed abort). That machinery survived audit; don't touch it.
- The comparison basis is gone: no finalWireInputTokens, no trustedInputLimitTokens, no margin. `detected_context_limit` being persisted IS the proof the turn-shape is broken (the provider already rejected it once); after drops+fold attempts, if we're still armed and no fold landed, another send is a guaranteed second 400 — abort with the actionable message instead.
- Remove the proactive/never-overflowed path from the abort decision entirely: a proactive-armed session NEVER aborts; it proceeds (worst case: one provider 400, which then arms provider_overflow recovery with real numbers — exactly the pre-Fork-2 behavior).
- Delete or simplify `evaluateEmergencyFailClosed` accordingly (keep a small pure decision function for testability; reasons become e.g. "below-emergency-band" | "provider-overflow-abort" | "proceed").

### 3. KEEP final-wire-token-estimate.ts as TELEMETRY ONLY
Do not delete the module. Remove it from the abort decision; keep computing it in the emergency band (>=95) and LOG it (sessionLog + the fail-closed log line) so we accumulate calibration data for the Rust-lane gate. Mark its doc-comment: telemetry/diagnostic signal, NOT a gate — provider-accurate gating is deferred to the module-side (Rust) estimate.

### 4. Sample-latch re-arm after abort (the wedge fix — keep in any variant)
heuristic-cleanup.ts persists the PRE-drop usage sample; a fail-closed abort prevents the assistant response that would refresh it, so the next pass hits `same-input-sample`, performs no further emergency drop, and aborts again = abort loop with no reclaim progress. FIX: on the abort path (after a confirmed abort), clear or adjust the persisted emergency input sample (e.g. subtract the tokens actually dropped this pass, or clear the latch) so the NEXT pass's emergency drop can select further candidates. Verify against the actual latch read in emergency-drop.ts (~:176-185) and pick the minimal correct adjustment. Add the regression test: abort pass with insufficient first drop → next pass performs FURTHER drops (not same-sample no-op).

### 5. Pi notification nit
context-handler.ts ~:3247-3262: `void uiNotify.call(...)` catches only synchronous throws; a rejecting thenable becomes an unhandled rejection (and the cooldown is consumed on failed delivery). Wrap the result: if thenable, attach a .catch that logs via sessionLog. One-liner class; don't restructure.

## Tests
- provider-overflow-armed + >=95 + no fold landed → notify then abort (order asserted).
- provider-overflow-armed + fold materialized this pass → proceeds, no abort.
- PROACTIVE-armed (model-shrink, never overflowed) + >=95 → NEVER aborts, proceeds.
- restart with stale proactive arm + zeroed input → no abort (the audit's false-positive scenario).
- abort pass → sample latch adjusted → next pass drops more (the wedge regression).
- existing notify/abort machinery tests keep passing (abort validation unchanged).
- Update/remove the now-obsolete numeric-gate tests (final-wire-token-estimate.test.ts keeps its estimator tests but drops the decide() gate coupling; transform gate tests re-point at the narrow predicate).
- E2E overflow-recovery assertion (no main-model request on a fail-closed pass) must still hold — it's a provider-overflow scenario, which remains abort-eligible.

## Gates
packages/plugin + packages/pi-plugin: bun test, typecheck, lint, check_comments. Comments explain the invariant (abort only on provider-proven overflow because only the provider's own rejection is a trustworthy overflow signal from inside messages.transform; numeric gating deferred to module-side accounting). Update PARITY.md 9c if its wording references the numeric gate. Report per-change status + test evidence.
