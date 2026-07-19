# Fix: module-side profile-epoch fold + absorb Oracle follow-ups

Repo: /Users/ufukaltinok/Work/Projects/CortexKit/magic-context, branch `subc-migration` (HEAD has the
covered-system absorb, c6c05935). Rust module only. Do NOT touch `packages/`. Do NOT push.
CACHE-CORE precision work; every decision is settled below.

## Problem 1 (the deploy blocker — build this first)
The absorb bumped `PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC` 0->1 expecting a coordinated HARD fold per
session, but the epoch is only SURFACED in the status op — nothing folds it into the render_config
identity the classifier compares. The consumer (ai-proxy) sends a STATIC v1 render_config string, so
with the current code the absorb's new m0 render behavior + covered-system suppression would apply
MID-LINEAGE on existing sessions: old frozen m0 (no covered-system block) + suppressed re-emission =
covered systems vanish from the wire = content loss + cache bust. Verified: transform.rs:795
`fold_m0_content_epoch(&req.render_config, &M0ContentEpoch { workspace_fingerprint, upgrade_state:
"", memory_content_epoch: "" })` — no epoch component.

### Fix: fold the module's OWN epochs into effective_render_config
In `crates/mc-module/src/compartment_coverage.rs`, `M0ContentEpoch` + `fold_m0_content_epoch`:
- Add a field `profile_render_epoch: String` to M0ContentEpoch. Its doc: the module's own rendered-
  prefix FORMAT epoch for this session's serializer profile — the "m0 in an incompatible format"
  composition class (same rationale as upgrade_state, see the struct doc's content-vs-composition
  rule). Folded module-side so a format flip self-coordinates ONE transition HARD per session,
  independent of what render_config string the consumer sends.
- At the call site (transform.rs ~795), set it from the request's validated serializer profile:
  empty string when the profile's epoch is 0, else e.g. "mpe1". RULE: the component MUST be the
  empty string at epoch 0 so every existing non-CC session's effective_render_config stays
  BYTE-IDENTICAL (zero spurious folds on owned-llmrunner/pi/opencode at this deploy). Implement a
  small helper in lib.rs or healing.rs: `profile_render_epoch(profile: SerializerProfile) -> u32`
  returning PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC for ClaudeCodeAnthropic and 0 for the others today,
  with a doc note that future profile epochs slot in here and that TAGGER_FEATURE_EPOCH gets the
  same treatment at the U1 flip (do NOT add tagger folding now).
- fold_m0_content_epoch already turns any field change into a render_config string change ->
  the classifier's existing render_config-changed -> HARD rule fires. Do not add a new trigger.

### Tests for problem 1 (non-vacuous)
1. Transition: a session with initialized meta whose last_render_config was recorded under epoch-0
   (no mpe component) takes a HARD on its first pass under the new code (render_config_changed),
   and the composed m0 carries the covered-system block. Fails without the fold (pass classifies
   SOFT+/defer and old bytes replay).
2. One-shot: the pass AFTER the transition HARD is defer/SOFT+ again (last_render_config updated,
   predicate false). Fails if the fold loops.
3. Non-CC unchanged: an owned-llmrunner (and one pi/opencode-profile) session's
   effective_render_config is BYTE-IDENTICAL before/after this change (epoch 0 -> empty component)
   and takes NO fold from this deploy. Fails if the component renders at epoch 0.

## Problem 2 — Oracle follow-ups on the absorb (bg_73f877c7, all SHORT)
(a) HOT-PATH GATE: `system_absorb_hard_due` calls store.load_compartments() on EVERY initialized
    pass (transform.rs ~866-876). Gate it cheaply: only run the check when compartment coverage
    could have advanced since the meta snapshot — cheapest correct gate: compare
    `store.max_compartment_seq(&session)` (or an existing cheap watermark already loaded in the
    pass — check what loaded.meta carries; there is a folded/max seq notion in meta) against the
    meta's recorded value, and only on mismatch load the full rows. If no cheap watermark exists,
    add a `max_compartment_sequence(&session)` single-scalar query to mc-store (SELECT MAX). The
    steady-state defer pass must do at most one scalar query here.
(b) BYTE-LEVEL GOLDEN: add a transform output golden (crates/mc-module/testdata/, following the
    existing golden pattern) for a fold with covered systems: 2 covered systems (one byte-identical
    duplicate at two ordinals) + 1 tail system. Pins: dedup, first-ordinal order, exact
    <covered-system-messages> block placement inside m0, tail system verbatim in the tail, no
    system-role message in the prefix. Byte-exact.
(c) CONTENT-DRIFT test: a covered system whose mid reappears with DIFFERENT content on a later pass
    must fail loud via the existing block-identity guard (IdentityDrift), not silently re-render.
    Assert the error path.
(d) LEADING-SYSTEM tests per profile: owned-llmrunner/pi/opencode-profile sessions with a leading
    (ordinal-0, uncovered) system message: it passes through the output verbatim (never absorbed,
    never suppressed — it is tail/uncovered). One test parameterized over the three profiles.

## Gate
cargo test -p mc-module -p mc-store; clippy -D warnings; fmt --check; check_comments. All green.
Report counts + files:lines + the golden you added. Commit on subc-migration, co-author trailer.
Do NOT push.
