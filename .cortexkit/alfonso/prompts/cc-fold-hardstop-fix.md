# Implement: CC-leg zero-fold hard-stop fix (Rust module, branch subc-migration)

You are implementing a bounded, Oracle-reviewed fix in the Magic Context Rust module
(`crates/mc-module`, repo /Users/ufukaltinok/Work/Projects/CortexKit/magic-context, branch
`subc-migration`). Effort: short (1-4h). Do NOT touch the TS plugin (`packages/`). Do NOT push.

## Background (why)
On the Claude Code MITM leg (serializer profile `claude-code-anthropic`) all tail reducers are OFF —
the historian FOLD is the only reclaim. A live session hard-stopped with ZERO folds because the
historian substance floor (`chunk.token_estimate < min_chunk_tokens(512) && !in_emergency`) blocked
firing: the chunk was 199 tokens (tool arcs collapse to one-line TC: summaries) and the emergency
bypass (`usage_percentage >= 95`) is unreachable because Claude Code hard-blocks at ~83.5% of window.

## Fix A — generalize the substance-floor bypass to "fold is the only reclaim"
1. `crates/mc-module/src/historian_chunk.rs`: add field to `HistorianAssemblerConfig` (struct at
   ~line 392): `pub fold_is_only_reclaim: bool,`.
2. Change the substance-floor gate (~line 540-548, currently
   `if chunk.token_estimate < config.min_chunk_tokens && !config.in_emergency`) to:
   `if chunk.token_estimate < config.min_chunk_tokens && !config.in_emergency && !config.fold_is_only_reclaim`.
   Update the surrounding comment to explain: on a fold-only (verbatim-tail) profile the fold is the
   sole reclaim, so the substance floor must not block it at any pressure (the floor exists to avoid a
   low-value producer spawn where OTHER reclaim exists).
3. Wire it at the ONLY production call site — `prepare_historian_fire` in `crates/mc-module/src/lib.rs`
   (config literal ~line 1429-1441, currently sets `in_emergency: usage_percentage >= 95.0`). Add:
   `fold_is_only_reclaim: !healing::tail_reclaim(SerializerProfile::parse(&parsed.serializer_profile).expect("serializer_profile validated upstream")),`
   `parsed.serializer_profile` is validated before this path (lib.rs:1858-1859 normal transform,
   2281-2286 shadow); `use healing::SerializerProfile` is already imported (lib.rs:65). If a helper
   already exists to derive the profile enum, reuse it; otherwise the parse+expect above is fine given
   the upstream validation.
4. The two OTHER `HistorianAssemblerConfig` literals are in tests (historian_chunk.rs:1247, 1322) —
   set `fold_is_only_reclaim: false` there unless the test specifically exercises the CC case.

### Fix A invariant (add a cheap debug tripwire)
`serializer_profile` is stable per session_id (the consumer sends it per-request; ai-proxy always
sends claude-code-anthropic for CC traffic — a session never migrates owned-llmrunner->CC). So a CC
session is born CC with zero pre-existing frozen `red:*` units, making `!tail_reclaim == fold is only
reclaim` strictly true. Add a `debug_assert!` on the claude-code-anthropic fold path asserting no
frozen red units are present (documents the invariant; must NOT fire in any real scenario). If a clean
assertion point is awkward, a code comment stating the invariant + the reasoning is acceptable — do
NOT add runtime overhead in release builds.

## Fix B (MC half) — plausibility FLOOR (floor-only, fall-back-to-default, NOT clamp-to-min)
MC currently accepts any positive context_limit. A too-small denominator forces constant Emergency95
and (with Fix A) many tiny CC folds. Raise the acceptance threshold to the existing constant
`scheduler::MIN_PLAUSIBLE_CONTEXT_LIMIT` (crates/mc-module/src/scheduler.rs:35 = 1024):
1. `usage_numbers` (lib.rs:3677-3691): change the limit filter from `.filter(|limit| *limit > 0.0)`
   to `.filter(|limit| *limit >= MIN_PLAUSIBLE_CONTEXT_LIMIT as f64)` so an implausibly small limit
   FALLS BACK to the 200_000 default (do NOT clamp to 1024 — input/1024 still reads as constant
   emergency; reject-and-default is the correct behavior).
2. Mirror on the scheduler `decide` effective_context_limit path (transform.rs:1298-1304 / scheduler.rs
   `decide` ~line 568-572): apply the same MIN_PLAUSIBLE_CONTEXT_LIMIT floor with fall-back-to-default,
   NOT clamp-to-min. Keep it consistent with usage_numbers.
This is floor-only by construction: values >= 1024 pass unchanged (167k and 1M-nominal both pass;
MAX_PLAUSIBLE_CONTEXT_LIMIT=10M already covers 1M), so it can NEVER clamp a deliberate large 1M
denominator down. Do NOT add an upper clamp. Do NOT add a CC-profile-specific magic floor (considered
and rejected — the generic 1024 floor + safe 200k fallback suffices).

## Tests (non-vacuous — write carefully)
- Fix A: profile=claude-code-anthropic, chunk.token_estimate=199 (<512), in_emergency=false -> Fire
  (NOT BelowBudget). This case MUST fail against the pre-fix gate (prove it: the same inputs without
  the new clause return BelowBudget). Same inputs with profile=owned-llmrunner (tail_reclaim=true),
  in_emergency=false -> still BelowBudget (floor holds where reducers exist). EmptyChunk still blocks
  on both profiles.
- TEST-HELPER GOTCHA: `request_with_usage` defaults serializer_profile to "owned-llmrunner"
  (lib.rs:4549-4555). Any CC test MUST explicitly override the profile to "claude-code-anthropic" or
  it silently tests the wrong leg and passes vacuously. Verify your CC test actually drives the CC path.
- Fix B: context_limit below 1024 (e.g. 500) FALLS BACK to 200k default (assert resulting percentage
  uses 200k, not 500); 167000 passes unchanged (133k input -> ~79.6%, below 95, above trigger);
  1_000_000 passes unchanged (<= 10M MAX_PLAUSIBLE), proving no upper clamp.
- Regression: genuinely-empty eligible range still NoFire(EmptyChunk/EmptyEligibleRange).

## Gate before returning
Run the module gate: `cargo test -p mc-module -p mc-store` (or the repo's convenience script — check
root package.json scripts for a cargo test wrapper), `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check` on the workspace (the dashboard src-tauri is excluded from the root workspace).
All green. Report the exact test counts and any file:line you changed. Commit on subc-migration with a
clear message and the co-author trailer if the repo convention uses one (check recent `git log`).
Do NOT push to origin. Do NOT modify anything under `packages/`.
