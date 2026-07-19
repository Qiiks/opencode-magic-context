# Issue #231: trust the provider-reported usage context limit proactively for unknown models

Repo: this worktree (branch from `subc-migration` HEAD). TS plugin, OpenCode + Pi parity where applicable. Read the issue first: https://github.com/cortexkit/magic-context/issues/231 (gh issue view 231 --json title,body). The reporter's trace is accurate; the public reply committing the fix direction is on the issue — implement THAT direction, nothing broader:

> treat the provider's own usage report (last_usage_context_limit) as a trusted proactive source when the SDK lookup misses, guarded by the same sane-bounds check (20k-3M), persisted so later passes and the sidebar use it consistently.

## Current mechanics (verify at source before editing)

- `resolveTrustedContextLimit` (packages/plugin/src/hooks/magic-context/event-resolvers.ts:53-95): models.dev SDK hit else reactive `detected_context_limit` (written only on overflow errors). For custom provider routes (e.g. proxy models), both miss -> undefined -> callers fall back to defaults (execute_threshold_tokens silently ignored; the reporter's exact complaint).
- `last_usage_context_limit` already persists per session in session_meta (loadPersistedUsage, storage-meta-persisted.ts:275+) — find its WRITE site and what populates it (OpenCode usage events carry the model's context window on some providers). Confirm what it contains for a models.dev-known model (should agree) and note it is session-scoped and model-coupled: it must only be trusted for the model that produced it (see the #188 model-switch machinery — last_observed_model_key rides the same row).
- The Zod sane-bound used elsewhere is 20k-3M (config schema / context-limit resolution work from v0.22.2). Reuse the same constants, do not mint new ones.

## Implement

1. Extend `resolveTrustedContextLimit`: when models.dev misses AND no detected overflow limit exists, fall back to the persisted `last_usage_context_limit` IF (a) it passes the 20k-3M sane bound and (b) the persisted `last_observed_model_key` matches the CURRENT model key (a stale limit from a previous model must never leak across a switch). Precedence stays: models.dev > detected(smaller-wins vs models.dev) > usage-reported. Document the precedence in the doc comment.
2. Audit the callers of resolveTrustedContextLimit (event-resolvers callers list) so each degrades identically; do NOT change resolveContextLimit's 128K default for pressure math (its doc comment explains why it must keep a positive denominator).
3. `execute_threshold_tokens` path: verify the reporter's symptom end-to-end — with a custom model absent from models.dev and a persisted usage limit, execute_threshold_tokens must now bind against the usage-reported window. Add a regression test reproducing the issue shape: custom provider/model key, no models.dev entry, no overflow state, last_usage_context_limit=1,048,576 -> threshold math uses it; and the negative: last_observed_model_key mismatch -> not trusted.
4. Pi parity: check packages/pi-plugin resolveHistoryBudgetTokensForPi + Pi's forward-pressure floor — Pi trusts its runtime window already; if the same unknown-model gap exists on Pi, apply the analogous fallback; if it structurally cannot (Pi supplies the window), document in PARITY.md and skip.
5. Rust-mode note: rust-mode-transform.ts already forwards a context limit to the module (MIN_PLAUSIBLE_CONTEXT_LIMIT clamp module-side); make sure the improved resolution feeds the SAME value it sends today (single resolution site, no fork).

## Gates

Focused suites (event-resolvers, threshold/scheduler tests, Pi parity file if touched) + full bun test in packages/plugin + typecheck + biome changed-files. Comments explain trust ordering and the model-key guard; never reference the issue number in code comments. Report: write-site findings for last_usage_context_limit (who populates it, which providers), precedence decision table, test names. No em-dashes.
