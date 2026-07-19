# Implement: Fix A' — protect the newest message (+its arc) on fold-only-reclaim profiles

Repo: /Users/ufukaltinok/Work/Projects/CortexKit/magic-context, branch `subc-migration` (Fix A is
already merged at HEAD b0981e10). Do NOT touch `packages/`. Do NOT push. Rust module only.

## Why (source-verified corruption path the guard closes)
On the Claude Code leg (profile `claude-code-anthropic`, `tail_reclaim==false`), Fix A now lets a
low-substance chunk fold at the NORMAL trigger. An Oracle found a real (narrow) corruption path this
opens: a COMPLETED tool arc whose RESULT is the newest message makes the protected tail empty, so the
newest message gets folded into a compartment and its mid becomes a durable `end_message_id` boundary
anchor. Chain (all in `crates/mc-module/src/boundary.rs`):
- `fence_boundary_for_tool_arcs` pushes `boundary = res_ordinal + 1` for a completed arc (line ~1035).
- If that arc's result is the last message, `res_ordinal + 1 == terminal_ordinal` (terminal = last+1,
  built at ~804-807).
- `protected_tail_start = boundary.max(runtime_floor)` (~459) then equals terminal → protected tail
  EMPTY.
- `apply_head_cap` lets `eligible_end` reach `protected_tail_start` (~1174-1176) → the historian chunk
  includes the newest message (`historian_chunk.rs:324-327`) → published compartment `end_message_id`
  anchors on the newest mid.
- The live-prompt floor that would otherwise protect the newest USER message is gated
  `usage_percentage < FORCE_COMPARTMENT_PERCENTAGE` (=80.0, boundary.rs:462), which is OFF at the ~97%
  spinner-pressure passes this leg sees post-Fix-B — and it only protects a meaningful USER message, not
  a tool_result-as-newest anyway.

Folding the newest message is ALSO semantically wrong on a verbatim-tail profile (the tail is forwarded
verbatim; summarizing the live turn reclaims nothing) — so the correct invariant is: on a
fold-only-reclaim profile, the newest message and its whole tool arc are ALWAYS protected tail,
ungated by pressure. This makes "spinner mid is always in the verbatim tail" TRUE BY CONSTRUCTION,
closing the path regardless of any specific spinner case.

## The change

### 1. Thread the profile signal into BoundaryContext
`crates/mc-module/src/boundary.rs`: add field to `BoundaryContext` (struct ~137):
`pub fold_is_only_reclaim: bool,` — add to the `Default` impl (~159) as `false`.

Wire it at the construction site(s). `BoundaryContext` is built for the trigger/boundary resolution in
the transform/historian path — find where `TriggerContext.boundary` / the `BoundaryContext` passed to
`check_compartment_trigger` and `resolve_protected_tail_boundary` is constructed in `lib.rs` /
`transform.rs` and set `fold_is_only_reclaim = !healing::tail_reclaim(profile)` from the request's
validated `serializer_profile` (the same derivation Fix A added at the prepare_historian_fire config
literal — reuse it; if both call sites can share one computed bool, do that). CRITICAL: both the
trigger check (boundary.rs:575) and the actual firing must see the SAME value, so the fingerprint the
trigger and runner share stays consistent — set it once per pass and thread it through.

### 2. The guard in resolve_protected_tail_boundary
In `resolve_protected_tail_boundary` (~396), AFTER `protected_tail_start` is finalized+clamped
(currently ~481, after the hysteresis collapse and `clamp_ordinal`) and BEFORE `apply_head_cap` (~489):

```rust
if ctx.fold_is_only_reclaim && raw_message_count > 0 {
    // On a verbatim-tail profile the fold is the only reclaim, but folding the NEWEST message is
    // both useless (the tail is forwarded verbatim) and unsafe (its mid would become a durable
    // boundary anchor; on the byte-splice leg that mid can later be retired). Force the newest
    // message AND its whole tool arc into the protected tail, ungated by pressure, so eligible_end
    // can never reach terminal_ordinal.
    let newest_floor = newest_message_protected_floor(&arcs, &index);
    protected_tail_start = protected_tail_start.min(newest_floor).max(offset);
    protected_tail_start = index.clamp_ordinal(protected_tail_start);
}
```

Add the helper (arc-integrity-aware — MUST NOT split a tool pair):
```rust
/// The lowest ordinal that must stay in the protected tail so the NEWEST message and its whole
/// tool arc are never folded. If the last message is part of a tool arc (as invocation or result),
/// returns that arc's invocation ordinal; otherwise the last message's own ordinal.
fn newest_message_protected_floor(arcs: &[ToolArc], index: &TokenIndex) -> u64 {
    let last = index.last_ordinal;
    arcs.iter()
        .filter(|arc| arc.inv_ordinal == last || arc.res_ordinal == Some(last))
        .map(|arc| arc.inv_ordinal)
        .min()
        .unwrap_or(last)
}
```
(`ToolArc`/`build_tool_arcs` are already in this file ~966-1015; `arcs` is already built at ~425;
`index.last_ordinal` exists ~803/821.)

## Invariants the guard must preserve (write tests for each)
1. **Closes the path:** with `fold_is_only_reclaim=true` and a COMPLETED tool arc whose result IS the
   newest message, `eligible_head.end <= protected_start_ordinal <= last_ordinal < terminal_ordinal`
   — assert the resolved boundary's `eligible_head.end` never reaches `terminal_ordinal`, and the
   newest ordinal is in `[protected_start_ordinal, terminal)`. This test MUST FAIL without the guard
   (drive the pre-guard path and show eligible_end reaches terminal).
2. **Arc integrity:** newest message is a tool_result with its invocation an earlier ordinal — the
   protected floor is the INVOCATION ordinal (whole arc protected, never split). Assert the head never
   ends between inv and res of the newest arc.
3. **No starvation / no #132 regression:** a normal fold-only session (newest message is a plain
   1-message user/assistant turn, large head before it) STILL folds its head — assert `eligible_head`
   is non-empty and covers the head up to just before the newest message. Protecting ONE trailing
   message must not block folding a 100k+ head.
4. **Off by default:** with `fold_is_only_reclaim=false` (owned-llmrunner / opencode / pi legs), the
   boundary is byte-identical to today — the guard is a no-op. Assert an existing non-CC boundary case
   is unchanged.
5. **Applies in emergency too:** the guard is NOT gated on pressure — assert it still protects the
   newest arc when `emergency_tail_scale` is set (the scaled `resolve_protected_tail_boundary` call at
   boundary.rs:628 must also honor it, since it inherits `fold_is_only_reclaim` from the same ctx).

## Gate before returning
`cargo test -p mc-module -p mc-store`, `cargo clippy -p mc-module -p mc-store --all-targets -- -D
warnings`, `cargo fmt --check`, `check_comments`. All green. Report exact test counts + every file:line
changed. Commit on subc-migration with a clear message + co-author trailer (check recent `git log`).
Do NOT push. Do NOT modify `packages/`.
