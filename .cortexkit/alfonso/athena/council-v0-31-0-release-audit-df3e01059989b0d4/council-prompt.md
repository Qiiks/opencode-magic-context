
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

Blind multi-model adversarial audit of unreleased changes in the git repo at /Users/ufukaltinok/Work/Projects/CortexKit/mc-master-triage (a git worktree on branch master). This is a RELEASE-READINESS audit: the delta ships as v0.31.0 of the Magic Context OpenCode/Pi plugins.

## How to run this audit

1. Start from the diff. In that worktree run: `git -C /Users/ufukaltinok/Work/Projects/CortexKit/mc-master-triage diff v0.30.7..HEAD` (~119 files). Read it to understand the shape of the change.
2. Then VERIFY every suspicion against the full source tree before reporting. Findings must carry file:line and be SOURCE-VERIFIED (read the actual current code), not diff-inferred. A finding you could not confirm in the source is a "suspicion", label it as such and rank it lower.
3. Do NOT report style findings. No formatting, naming, or preference nits.

## What the delta contains (discover the rest yourself from the diff)

1. Removal of the `ctx_reduce_enabled` config flag; caveman text compression decoupled and gated on `caveman_text_compression.enabled && !subagent`; session modes collapsed 3->2; a provisional ctx_reduce-verdict gate that withholds the system-prompt hash baseline until the verdict freezes.
2. Notes (session + smart notes) as a fifth ctx_search source with session-aware @msg anchors.
3. Project-identity resilience: `.git` ancestor stat-walk fast path with realpath retry, dubious-ownership classification + safe.directory warning, `dir:` fallback with 5-min transient cooldown, last-known-good `git:` identity reuse to prevent mid-session identity flips, load-path degradation instead of plugin-disable.
4. Sidebar Facts row removal; CLI doctor onnxruntime-node load probe in a subprocess.
5. NEW `/ctx-wrapup` command (both harnesses): manual blocking historian drain down to a keep-newest-N-raw-messages watermark. Durable `wrapup_in_progress` marker (session_meta, migration v50, 5-min TTL renewed every 60s, ownership-loss abort), mutual exclusion with recomp/upgrade/trigger-fired historian in BOTH directions, `forceDrainQuota` (bypasses pressure-window quota), `forceKeepLastCompartment` downgraded runner-side on `chunk.hasMore` (weak-final keep + unanchored-promotion skip ONLY on the actual final chunk; discard-last promotion skip preserved), boundary plan with raw-message keep watermark + tool-arc fence + user snap + re-fence, publishes are DEFERRED (ride the next natural bust; OpenCode postprocess drain gated on `pendingMarkerCoveredByConsumedBoundary`; Pi marker drains via context-handler's gated drain only).

## Known invariants (violations are SHIP-BLOCKERS)

- Defer passes must replay BYTE-IDENTICAL.
- First-application of any mutation only on cache-busting passes.
- One bust must cover BOTH a history rebuild AND its compaction-marker advance.
- Background/manual publishes never force a materialization.
- Historian discard-last runs never promote unanchored facts.
- A crashed wrapup must not wedge the session (TTL must release it).
- Subagents never get caveman compression.

## Where to dig (this delta already survived several single-Oracle review rounds)

Shallow findings are likely already fixed. Find what an INDIVIDUAL reviewer would miss:
- Cross-feature interactions: wrapup x emergency-drop x nudges x smart-drops; identity-reuse x workspace-fingerprints x embedding-registry; notes-search x auto-search-hints.
- Multi-process interleavings: two OpenCode instances; OpenCode + Pi sharing the same context.db.
- Restart / crash recovery paths.
- Migration + config edges for existing users upgrading from v0.30.7 (e.g. removed config flag, migration v50, durable marker left behind by an old binary).

## Output required

- Per-finding: file:line + severity (P0 blocker / P1 should-fix / P2 nit) + concrete evidence + why it matters + suggested fix direction. Mark unverified items as "suspicion".
- Focus on P0/P1. Cross-feature and multi-process/crash-recovery findings are the target.
- A per-member overall verdict: SHIP or HOLD, with rationale. If HOLD, list the specific blocker(s).