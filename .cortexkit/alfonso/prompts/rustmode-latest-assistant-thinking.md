# Investigate + fix: served OC-leg array trips Anthropic "thinking blocks in the latest assistant message cannot be modified"

Repo: this worktree (branch from `subc-migration` HEAD). Investigation-FIRST: you must identify the exact block-level mutation before writing any fix. Rust (mc-module) is the likely home; TS adapter touch allowed if evidence demands.

## Live evidence

Rust-mode drive session `ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO` (benchmarks project, transform_mode rust). Beat 4 (2026-07-17 ~17:35 local): the authority pass SUCCEEDED end-to-end module-side for the first time — seed applied (shadow_acked_watermarks compartment_sequence=377, memory_id=7951), transform completed, mc_cache_state row_version=4 with boundary minted at `msg_2P5mSoAYNofR0BcS7rJTPinkvKy3#4`, frozen m0 composed with project docs. The SERVED array then drew an Anthropic 400: `messages.65.content.4: thinking or redacted_thinking blocks in the latest assistant message cannot be modified. These blocks must remain as they were in the original response.`

Context that matters:
- The session's history was authored on anthropic/claude-sonnet-5; the drive turns run anthropic/claude-opus-4-8 (model switch mid-session). Thinking blocks in tail assistants carry sonnet signatures.
- Earlier beats (raw fail-open passthrough of the same session) drew the SAME 400 class, so the trigger may predate the served path — but beat 4's error is against the served array and must be fixed for the OC leg regardless.
- Anthropic's rule: the LATEST assistant message's thinking/redacted_thinking blocks must be byte-identical to what the API returned (signature-verified), and block order within that message must be preserved.

## Investigation steps (do these, report findings with evidence)

1. Reconstruct the served array for the failing pass: the module store at ~/.local/share/cortexkit/magic-context/store.db has the session's cache state (block fingerprint map, frozen units); the live opencode.db has the raw messages. Identify which message lands at served index 65 and what its content[4] block is. The MC_REPLAY_STORE replay harness (crates/mc-module tests / replay tooling from the CC-leg debugging) may help; if unusable for the OC profile, build the served array via a unit test that seeds the same store rows and drives handle_transform_value with serve_native.
2. Determine which mutation touched the latest assistant message. Candidates to check IN ORDER: (a) tag overlay appending §N§ prefixes to a text block in the newest assistant (tags must never target the newest assistant's message if it carries reasoning blocks — check tag placement rules for the OC profile); (b) healing passes (strip_reasoning_from_merged_assistants or empty-content healing) merging/reordering blocks in the latest assistant; (c) serve_native encode-back re-encoding a modified message and dropping/reordering the reasoning block or its signature (mark_modified clears retained bytes; verify typed Reasoning re-encode preserves signature and position for the OpencodeAiSdk profile); (d) channel-1 append targeting a tool result inside the latest assistant turn's arc.
3. Check whether the LATEST-ASSISTANT exemption that exists for other paths (reasoning blocks ineligible for reduction; imitation strip only on completed messages) covers the mutation you found, and why it didn't here.

## Fix shape (adjust to evidence)

The invariant to implement module-side for the OpencodeAiSdk (and any anthropic-family) profile: the NEWEST assistant message on the wire is MUTATION-EXEMPT in its entirety when it contains reasoning/redacted blocks — no tag overlay, no healing reorder, no channel-1 append, no re-encode (serve it from retained ingress bytes verbatim). Older assistants keep existing behavior. If the evidence shows the mutation is legitimate but the re-encode loses signature/order, fix the encode-back instead. Either way: a regression test that builds a latest-assistant-with-thinking + mutation-trigger fixture and asserts the served bytes for that message are ingress-identical; plus a fail-first form proving today's behavior differs.

## Gates

cargo test -p mc-module (lib+integration), clippy -D warnings; focused TS suites if touched. Report: the identified mutation site with file:line, the served-index-65 identification evidence, fix, test names. No em-dashes in comments; comments explain the Anthropic latest-assistant invariant, not this incident.
