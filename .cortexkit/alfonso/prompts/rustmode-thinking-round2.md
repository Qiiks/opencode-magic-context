# Round 2: latest-assistant thinking 400 SURVIVES the ingress-preservation fix — find the real mutation via replay

Repo: this worktree (branch from `subc-migration` HEAD, which already contains commit 43ffc79f "preserve latest OpenCode reasoning ingress"). Investigation-FIRST with hard evidence; do not guess.

## New live evidence

With 43ffc79f BUILT AND RUNNING (deployed 20:51 local, module restart verified, pass trace clean: receive=2 complete=2 reject=0), beat 5 on `ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO` drew the SAME Anthropic 400: `messages.65.content.4: thinking or redacted_thinking blocks in the latest assistant message cannot be modified.`

Shape facts: input array 225 messages; served array (post-fold) is ~66-67 messages (fold minted boundary msg_2P5mSoAYNofR0BcS7rJTPinkvKy3#4, so [m0, m1, tail]); error index 65 with content index 4 — a thinking block sitting at BLOCK POSITION 4 inside the latest assistant message. The newest assistants in the session end with finish=tool-calls (mid tool-loop), history authored on sonnet-5, live turns on opus-4-8.

## Prime hypothesis to prove or kill (then fix what the evidence says)

The mutation may not be module-side at all: OpenCode serializes the transform's output through its AI-SDK path, which MERGES CONSECUTIVE ASSISTANT MESSAGES and emits thinking in source order (project memory: ProviderTransform.message merges; TS-mode MC runs stripReasoningFromMergedAssistants BEFORE serialization to compensate). If the module's served tail contains consecutive assistant messages (e.g. an aborted assistant adjacent to the next, or a message MC's TS pipeline would have merged/stripped), the AI-SDK merge glues them and the latest assistant's signed thinking lands at content.4 (after the prior assistant's blocks) — position modified from Anthropic's view. A thinking block at position 4 is the classic merged-run signature. The ingress-verbatim fix (43ffc79f) cannot help because the mangling happens DOWNSTREAM of the module.

## Steps

1. REPLAY: reconstruct the exact served array. Use the MC_REPLAY_STORE harness (crates/mc-module, used for the CC-leg cap replays) against a COPY of ~/.local/share/cortexkit/magic-context/store.db plus the live opencode.db raw messages for the session (read-only; work on copies). Identify served indices 60-66: roles, block sequences, which message is the latest assistant, whether consecutive-assistant runs exist in the served tail.
2. Determine what OpenCode's serializer does to that shape: check the rust module's healing profile for OpencodeAiSdk (crates/mc-module/src/healing.rs, strip_reasoning_from_merged_assistants) — does the module ALREADY model the merge? Was the latest assistant exempted from that strip by 43ffc79f, so its reasoning now survives INTO a merged run (the fix creating the very exposure)? File:line the interaction.
3. Establish what TS MODE serves for this exact tail (the TS pipeline is the reference: it works on Anthropic daily). Which messages does stripReasoningFromMergedAssistants strip for this shape, and what does the final wire look like?
4. Fix to TS-parity: the module's OC-profile output, AFTER OpenCode's downstream serialization, must match what TS mode produces. Depending on evidence this is one of: (a) apply the merged-run reasoning strip with correct topology modeling (including the latest-assistant interaction: if the latest assistant is part of a merged run, reproduce exactly what TS mode does for that case — do NOT invent a new policy); (b) if TS mode avoids the merge entirely by never serving consecutive assistants, reproduce that guarantee. The 43ffc79f exemption stays for the non-merged case.
5. Regression tests: a replay-derived fixture reproducing the beat-5 served shape, asserting the post-serializer projection puts the latest assistant's thinking blocks in valid position (or strips them exactly as TS mode does); plus the merged-run + latest-assistant interaction case both ways.

## Gates

cargo test -p mc-module (lib+integration), clippy -D warnings. Report: served-array reconstruction evidence (indices 60-66 roles/blocks), the proven mutation mechanism with file:line, what TS mode does, the fix, test names. No em-dashes in comments.
