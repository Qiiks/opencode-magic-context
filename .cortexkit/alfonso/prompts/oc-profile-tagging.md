# Rust mode: enable the tagging/reduce surface for the native OpenCode profile

Branch from `subc-migration` HEAD. Crates + TS adapter work. Cache-affecting: every byte change must ride the epoch-fold discipline (memory #8603 pattern).

## Drive finding (verified live)

On the containerized rust-mode drive (OpenCode leg, serve_native), `ctx_reduce` calls reach the module and land in `mc_reduce_command_ledger` (2 rows for session ses_OqknfoW2O3LTOcjLvOMQoREVPtz1) but `pending_agent_drops` stays empty and no `[dropped N]` ever appears. Root cause at source: `crates/mc-module/src/transform.rs:893` gates the whole tag surface with `tagging_active = cc_u1_active && loaded.meta.cc_u1_active`, and `cc_u1_active()` (lib.rs:171) requires `SerializerProfile::ClaudeCodeAnthropic`. The native OpenCode profile therefore has NO tag minting, NO tag-number prefixes on the wire, NO channel-1 appends, and agent-drop canonicalization has no tag rows to resolve against, so drops queue nowhere. In TS mode the TS plugin does all of this itself; in rust mode the module must own it. This is a hard parity gap for the thin-plugin cutover.

## Implement

1. Generalize the gate: introduce a profile-aware `tagging_surface_active(profile, tool_present)` that is true for ClaudeCodeAnthropic (existing U1 semantics, unchanged) AND for the native OpenCode profile when `tool_present` is true. Keep `cc_u1_active` for the CC-specific arms that are genuinely CC-only (ack-only tool mechanics, guidance variant choice) — audit each use of `cc_u1_active` / `loaded.meta.cc_u1_active` and classify: tag overlay, channel-1, temporal marks, pending-drop consumption, newest-20 protection, imitation strip = surface-generic; anything referencing the Thalamus tee/ack contract = CC-only.
2. tool_present signal on the OC leg: verify what the TS rust-mode adapter sends today (packages/plugin/src/hooks/magic-context/rust-mode-transform.ts + module-wire.ts). The plugin already resolves ctx_reduce availability per session (ctx-reduce-availability.ts, {callable, frozen}); thread that VERDICT into the transform request as tool_present. If the field is absent today, add it to the wire request (serde default false so CC/Thalamus traffic is unaffected).
3. Epoch fold: enabling tagging changes rendered bytes for existing rust-mode sessions. Fold the tagger epoch into the m0 content epoch for the OpenCode profile the same omitted-at-zero way (profile_render_epoch / M0ContentEpoch pattern, memory #8603) so the flip takes exactly one self-coordinated HARD per session. Do NOT bump TAGGER_FEATURE_EPOCH itself (no format change, this is an activation).
4. Persisted meta: mirror the `meta.cc_u1_active` latch semantics for the OC surface (the two-arm latch exists so a transient tool-absent request cannot flap bytes mid-session; reuse the same mechanism, do not invent a new one).
5. Numbered placeholders: the OC leg is NOT a verbatim-tail profile, so frozen reductions there follow the owned-leg semantics that already exist for non-CC profiles; confirm `[dropped N]` renders with the tag number via the build_output overlay and that `strip_leading_tag_imitations` applies.
6. Server-side drop canonicalization must now resolve raw range strings against the minted tag rows for OC sessions; add an end-to-end test: seed session, transform with tool_present=true (tags mint), agent_drops.append raw "1-3", next bust pass consumes and serves `[dropped N]` placeholders, ledger marks first_applied.

## Tests (fail-first where marked)

- OC profile + tool_present=true: tag prefixes appear on tool-result blocks in build_output; byte-stable across a defer replay.
- OC profile + tool_present=false: zero overlay bytes, byte-identical to today (fail-first: flipping the default to true must fail this).
- CC profile: existing U1 tests all untouched and green (the gate refactor must be behaviorally invisible to CC).
- Epoch fold: existing rust-mode session takes exactly ONE HARD on first tool_present=true pass, then SOFT+ steady state (assert render_config identity change and stability after).
- The end-to-end reduce test from item 6.
- Adapter side: request carries tool_present from the availability verdict; frozen=false sessions send false (fail-open default never flips bytes before the verdict freezes).

## Gates

cargo test -p mc-module -p mc-store + clippy; plugin bun test focused (rust-mode-transform, module-wire) + full suite; report classification table of every cc_u1_active call site (surface-generic vs CC-only) in your summary. No em-dashes in comments; comments explain the invariant not the history.
