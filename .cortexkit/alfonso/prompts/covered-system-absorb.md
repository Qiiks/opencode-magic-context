# Build: absorb covered system messages into m0 as text (kills the covered-system 400)

Repo: /Users/ufukaltinok/Work/Projects/CortexKit/magic-context, branch `subc-migration`. Rust module
only (`crates/`). Do NOT touch `packages/`. Do NOT push. This is CACHE-CORE (changes fold-output
bytes) — precision over speed, and every decision below is settled; do not re-litigate.

## Why (live prod 400, ground truth)
Claude Code's mid-conversation-system beta places system-role messages mid-array. When such a
message falls BEHIND the fold boundary ("covered"), build_output currently re-emits it as a
system-role message inside the synthetic prefix: [m0(user), covered-system, m1(user), tail].
Anthropic rejects that with HTTP 400: "role 'system' must precede an 'assistant' message or end
the array" — a user-role message may never directly follow a system message. Live captures prove
CC-native traffic always obeys this rule (1048 conforming placements, 0 violations across 176
captures); ONLY our synthetic placement violates it. There is NO valid slot for a system-role
message inside our prefix by construction (user-role m0/m1 bracket it; the tail's first role is
not guaranteed; end-of-array is the live tail).

## The fix: covered systems leave the messages array entirely
On EVERY profile (this is universal correctness, not CC-specific; in practice only the CC leg has
in-array covered systems today):
1. build_output STOPS emitting covered system messages as messages.
2. m0's rendered text gains a `<covered-system-messages>` block carrying their content:
   - dedup within the covered set by byte-identical content, keeping FIRST-ordinal order
     (the exact opt-MERGE rules, rendered as text instead of a message),
   - each entry rendered verbatim (content bytes unmodified inside the block),
   - block omitted entirely when the covered set is empty (no empty-block bytes).
3. The covered-system set is derived AT COMPOSE TIME (HARD fold / m0 materialization) from the
   live input array: every system-role, non-synthetic message with ordinal < the new
   coverage_ordinal. The consumer is a verbatim-history harness (CC resends full history every
   turn), so the covered systems are always present in the input at fold time; the rendered block
   freezes into m0's frozen_payload like every other m0 byte. On later folds (coverage advance),
   the new m0 re-derives the block from the (larger) covered set — deterministic, pure function of
   (input array, new coverage).
4. Structural invariant + test: on the claude-code-anthropic profile, NO system-role message can
   ever appear between the start of the synthetic prefix and the first tail message in
   build_output's result. Assert it as a debug_assert AND as a unit test over a fold with covered
   systems.

## Where (read these first)
- `crates/mc-module/src/transform.rs` build_output — the covered-system re-emission (the
  "system-role trim exemption": systems with ordinal < coverage are exempted from the trim and
  re-emitted; that exemption's PURPOSE was content preservation, which the m0 block now serves).
  Find it via the trim/`coverage_ordinal` filtering when composing the output array.
- `crates/mc-module/src/m0_compose.rs` (compose_m0_from_store) — m0 text composition; add the
  covered-system block section. Thread the live input array (or the pre-extracted covered-system
  list) into the compose path — compose currently reads the store; the covered systems come from
  the PASS's input, extracted where build_output/apply_once has both the array and the new
  coverage. Keep compose deterministic: the block's input must be an explicit parameter, not a
  global.
- The sparse-ordinal validator/chunk builder ALREADY absorbs system ordinals into coverage
  (historian_validate.rs) — no historian change needed; systems are covered but never summarized,
  which is exactly why the m0 block is the content-preservation mechanism.

## Transition (the part that must not be improvised)
Existing folded sessions have frozen m0 payloads WITHOUT the block, and their covered systems are
currently re-emitted as messages each pass. The new code must NOT change those sessions' bytes on
defer passes (frozen m0 replays frozen; changing re-emission behavior mid-lineage busts cache and
loses content). The designed transition mechanism is the co-owned profile epoch:
- Bump `PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC` from 0 to 1 (crates/mc-module/src/lib.rs, the const
  surfaced in the status op). The epoch folds into render_config identity, so every CC session
  takes ONE coordinated HARD fold on its first pass under the new binary — re-rendering m0 WITH
  the block and dropping the message-form emission atomically for that session. Before that fold,
  the session's old frozen state replays unchanged (the epoch change itself forces the fold, so
  there is no mixed window within a session).
- The re-emission path must therefore key on the CURRENT pass's compose, not on a global flag:
  after the epoch-forced HARD, covered systems are absorbed; there is no session state where old
  frozen m0 (no block) coexists with suppressed re-emission. Make the invariant explicit in a
  comment at the suppression site.
- Do NOT bump TAGGER_FEATURE_EPOCH (that is the U1 flip, not yours).

## Tests (non-vacuous; say how each fails without the change)
1. Fold with 2 covered systems (one duplicated byte-identical at two ordinals) + 1 tail system:
   output has NO system in the prefix, m0 text contains the block with 2 entries (dedup applied,
   first-ordinal order), tail system survives verbatim in the tail. Fails pre-change (systems
   re-emitted as messages, no block).
2. Empty covered set: m0 contains NO covered-system block bytes. Fails if the block renders empty.
3. Defer replay byte-identity: after the fold in (1), two defer passes render byte-identical
   output including the m0 block (frozen replay).
4. Coverage advance: a second fold with one MORE covered system re-renders m0 with 3 entries;
   the prior block's 2 entries keep their order. Fails if the block isn't re-derived.
5. The structural invariant test from item 4 in the fix section.
6. Epoch: assert PROFILE_EPOCH_CLAUDE_CODE_ANTHROPIC == 1 in the status-op test that pins epochs
   (find the existing one) and that a session folded under epoch-0 state takes a HARD on its
   first pass with the new render_config (the existing render_config-change HARD test class —
   extend it if one covers profile-epoch specifically, else add it).
7. Existing golden vectors: regenerate ONLY those whose scenarios contain covered systems; every
   other golden must be byte-identical (prove the absorb is inert without covered systems).
   The real_daemon spine and existing transform tests must pass unmodified except where they
   explicitly asserted the old covered-system re-emission (update those to the new contract).

## Gate
cargo test -p mc-module -p mc-store; cargo clippy -p mc-module -p mc-store --all-targets -- -D
warnings; cargo fmt --check; check_comments. All green. Report exact counts, files:lines, every
golden you regenerated and why, and any place the design above under-specified reality (flag it,
do not improvise silently). Commit on subc-migration with the co-author trailer. Do NOT push.
