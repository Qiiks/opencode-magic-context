
## Solo Analysis Mode
You MUST do ALL exploration yourself using your available read/search tools.
- Do NOT use task or any delegation tool under any circumstances
- Do NOT delegate to explore, librarian, or any other subagent
- Do NOT spawn background tasks
- Search the codebase directly — you have full read-only access to every file
- This mode produces the most thorough analysis because you see every result firsthand


## Analysis Intent: AUDIT

You are conducting an **audit** — your goal is to find discrete issues, risks, or violations.

**Focus:**
- Search for problems, anti-patterns, security risks, correctness issues, or violations of stated requirements
- Each finding must be a distinct, actionable item with concrete evidence
- Severity determines priority: critical (blocks/breaks), high (significant risk), medium (should fix), low (nice to fix)
- For each finding, provide the specific location (reference, section, or component where it occurs)
- State your confidence: high (clear evidence), medium (likely but needs verification), low (suspicion, investigate further)
- **This is a broad sweep, not a targeted trace.**

**Analytical standards:** Support claims with concrete evidence. State confidence (high/medium/low) for key assertions. Note caveats and limitations.

**Structure your response as:**
```
<COUNCIL_MEMBER_RESPONSE>
## Finding 1: [Title]
- **Severity**: critical/high/medium/low
- **Location**: [specific reference — e.g. component, section, endpoint, rule]
- **Confidence**: high/medium/low
- **Issue**: [what is wrong and why it matters]
- **Evidence**: [concrete reference, snippet, or observation that proves the issue]
- **Suggested Fix**: [actionable recommendation]

## Finding 2: [Title]
...

## Summary
[Total findings by severity. Overall risk assessment with confidence levels.]
</COUNCIL_MEMBER_RESPONSE>
```

## Analysis Question

BLIND ADVERSARIAL COUNCIL AUDIT — Magic Context SHADOW-MODE lane, pre-production soak gate.

Repo: /Users/ufukaltinok/Work/Projects/CortexKit/magic-context
Branch: subc-migration, HEAD 6ea3179f

You are one of several independent auditors. Work ALONE, hunt for the NEXT bug class. Do NOT assume the code is correct because it has tests — the first armed run already exposed two integration bugs the mocked tests missed (wire-shape mismatch, ordinal starvation), both already fixed at HEAD. Your job is to find what the mocks still miss. Read the ACTUAL code with file:line evidence; do not speculate abstractly.

=== WHAT SHADOW MODE IS ===
A dev-flag lane (config shadow_transform.enabled, user-tier only) where the OpenCode TS plugin mirrors every finalized transform pass to the Rust mc-module over the subc daemon socket for byte-comparison — WITHOUT affecting the live session. The TS side fire-and-forgets: per-session FIFO queue, state_sync (compartments/memories/mutations sync), shadow_transform (input + TS output + decision), shadow_reset (generation bump + wipe). The Rust side runs its own transform on the mirrored input against a shadow:<sid> store lineage, byte-compares against the TS output, and records divergences in shadow_divergences. The soak is OBSERVE-ONLY by design.

=== AUDIT SCOPE (areas that changed or are first-run-live today) ===

1. packages/plugin/src/plugin/hooks/create-session-hooks.ts — the config mapper was changed from a hand-maintained field list to a full spread (`...pluginConfig`) with two defaulted overrides (commit e932804c). HUNT: does spreading the ENTIRE plugin config into the hook config leak anything that shouldn't reach per-session hook code, shadow anything the hook type declares differently, or change behavior for any existing consumer that relied on a field being absent (e.g. `undefined` vs present-with-value)?

2. packages/plugin/src/hooks/magic-context/shadow-sender.ts — the full sender. HUNT ESPECIALLY:
   (a) HOT-PATH SAFETY: enqueue() runs inside the transform hot path. Verify EVERY code path in enqueue (including resolveOrdinalsForShadow with its new by-id DB fallback, denormalizeShadowOutput, cloneJson of potentially huge message arrays) is exception-safe and bounded — a throw or a multi-second stall here delays the user's real prompt. Is cloneJson(messages) on a 400-message array acceptable per-pass cost? Is the by-id DB read (readRawSessionMessageById) opening a DB handle per call, and what happens under SQLITE_BUSY?
   (b) WIRE SHAPE (commit 6ea3179f): toFlatWireBody flattens {method, params} to flat. Cross-check EVERY field the TS builders emit against the Rust serde parsers in crates/mc-module/src/lib.rs (ShadowStateSyncWire, ShadowTransformWire, ShadowResetWire, ShadowPassInputs, ShadowUsageWire, ShadowCompartmentWire, ShadowMemoryWire, ShadowMemoryMutationWire): field names, types, required-vs-optional, snake_case exactness. Name any field that will reject or silently default.
   (c) ORDINAL FALLBACK: the new below-floor by-id fallback in resolveOrdinalsForShadow. Can the fallback return an ordinal computed on a DIFFERENT basis than the cache (e.g. cache primed tail-only carries absolute ordinals from baseOrdinal while the by-id COUNT includes/excludes summary rows differently)? A silent basis mismatch would poison the shadow store with wrong ordinals rather than failing loud.
   (d) SubcShadowTransport: hand-rolled socket protocol (auth handshake, frame header, backoff). Route kind now tool_provider (commit 1555b231). HUNT: connection lifecycle bugs (socket close mid-request, reader waiters, backoff doubling), auth proof correctness, frame length handling, the 5s request timeout leaving the FIFO wedged.
   (e) QUEUE SEMANTICS: MAX_QUEUE_PER_SESSION=4 drop-oldest, blockedUntilReset, requireResetReason transitions. Any path where a session gets permanently wedged (blocked but no reset queued) or where a reset races a queued pass?

3. crates/mc-module/src/lib.rs shadow handlers (handle_shadow_state_sync_value, handle_shadow_transform_value, handle_shadow_reset_value, shadow_binding) + crates/mc-store shadow methods (apply_shadow_state_sync, reset_shadow_session). HUNT:
   (a) ISOLATION: can ANY shadow-lane write touch a non-shadow session row? The shadow session id is derived from binding (shadow: prefix). Verify shadow_binding rejects non-shadow bindings on shadow ops AND that plain transform ops reject shadow bindings (codes non_shadow_op_on_shadow_binding, plain_transform_on_shadow_binding — verify they're actually enforced on EVERY arm).
   (b) The shadow_transform handler runs the REAL transform + a byte-compare. Does a shadow divergence or panic in the compare path affect the real lane in any way (shared store handle, lease contention, pass-trace pollution)?
   (c) apply_shadow_state_sync CAS semantics: generation/seq mismatch arms, and what happens when state_sync carries compartments whose sequences overlap already-synced ones (idempotent or duplicating?).

4. CACHE SAFETY OF THE LIVE LANE: the sender captures cloneForShadow(messages) BEFORE the transform mutates and reads declared-trim state before/after. Verify the shadow capture path cannot mutate the live messages array (shared references through cloneJson boundaries — is the clone deep and taken at the right point?), and that shadow-sender DB reads (getCompartments, getMemoriesByProject, etc.) hold no transactions that could contend with the transform's own writes.

=== DELIVERABLE (per member) ===
- Independent findings with file:line evidence. Read the real code.
- Rank each finding Critical / High / Medium / Low.
- Weight by blast radius: (1) can affect the LIVE user session = WORST class; (2) corrupts shadow state silently = defeats the soak's purpose; (3) merely loses shadow coverage but logged = ACCEPTABLE.
- Explicit false-positive filtering: check PARITY.md files and existing test coverage before claiming a bug. State what you checked.
- Your own verdict: SHIP (arm the soak on production sessions) or HOLD, with the single most important reason.

Relevant reference files exist at packages/pi-plugin/PARITY.md and packages/plugin/src/features/magic-context/smart-notes/PARITY.md. Test files live alongside sources (*.test.ts) and in crates/*/tests or #[cfg(test)] modules.