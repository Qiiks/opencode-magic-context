# Round 4: the OC-leg serve is missing TS's reasoning-clearing entirely — port it (this is the real parity gap)

Repo: this worktree (branch from `subc-migration` HEAD; rounds 1-3 present: 43ffc79f, 524c96d6, 0778d67e). Beat 7 with round-3 deployed STILL fails `messages.65.content.4` unchanged. Stop iterating on the exemption — the round-3 wire captures (/tmp/mc-wirecap/1784318986603-rust.json vs 1784319013333-ts.json, still on disk; read them FIRST) already showed the real difference and we kept treating it as a side note:

- RUST wire: thinking blocks retained across the SERVED TAIL (m65 and neighbors carry signed thinking).
- TS wire: NO thinking blocks anywhere in the tail (m65 = tool_use + tagged text; neighbors likewise).

TS mode does not win by a cleverer latest-assistant exemption. It wins because the TS pipeline CLEARS REASONING from historical assistant messages before serving (packages/plugin/src/hooks/magic-context/reasoning-replay.ts, clearOldReasoning — the D2 feature: watermark-persisted, replayed deterministically every pass, provider-aware). With no thinking present in any served assistant, Anthropic has nothing to verify and the request passes. The rust OC-leg serve path never got this feature — every historical thinking block survives into the wire, where OpenCode's downstream serialization (AI-SDK merge, tool_use repositioning at thinking boundaries) and our overlays inevitably perturb SOMETHING about a signature-verified block, and the latest assistant 400s. Rounds 1-3 were fighting symptoms of D2's absence.

## Task

Port TS reasoning-clearing semantics to the rust module's OpencodeAiSdk serve path:

1. READ the TS reference precisely first: packages/plugin/src/hooks/magic-context/reasoning-replay.ts (+ its tests, + strip-content.ts interplay). Extract the exact rules: which messages are eligible (historical/completed assistants), what the latest-assistant / mid-turn handling is (TS capture shows even the newest historical assistant msg_TbBacBb96... served WITHOUT thinking and Anthropic accepted — pin down under which rule), what replaces the block (removal vs empty-sentinel, provider-gated via modelAcceptsEmptyContent), and how determinism across passes is guaranteed (watermark columns). Document the extracted contract in the module code comment.
2. Implement in the module's OC-profile serve (crates/mc-module/src/transform.rs healing/residual layer or a dedicated pass; you decide placement, but it must run for OpencodeAiSdk profile ONLY — the claude-code-anthropic leg keeps verbatim-tail semantics, and owned-broca keeps its existing behavior; add explicit non-applicability tests for both).
3. Cache safety: clearing must be DETERMINISTIC across passes (same input set cleared identically on defer replays — the module's frozen/fingerprint machinery must see stable bytes; check interaction with block fingerprints and the retained-bytes serving so cleared messages don't oscillate between passes). If TS uses a persisted watermark for first-clear timing, decide the module-side equivalent (durable in mc_store meta, not in-memory) and justify in the report.
4. Update rounds 1-3 machinery where it becomes redundant or contradictory: the latest-assistant ingress exemption must not resurrect thinking that clearing removes (define precedence explicitly: clearing wins for historical messages; the exemption remains only for a genuinely live in-flight assistant if TS has such a case).
5. VERIFY ON THE LIVE RIG, not just unit tests: rebuild release ck-mc in the worktree, deploy to ~/.local/share/cortexkit/bin/ck-mc (codesign --force -s -), ck module restart magic-context, then drive one beat through the clone session exactly like round 3 did (cd ~/Work/Projects/CortexKit/benchmarks && opencode run -s ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO -m anthropic/claude-opus-4-8 "Wire beat R4: reply OK" or the PTY pattern). Success = assistant reply lands with no 400 (check opencode.db newest messages). If it still 400s, capture the wire again with the round-3 proxy pattern and report the block diff — do NOT iterate blindly.

## Gates

cargo test -p mc-module (lib+integration) + clippy -D warnings; the live beat proof from step 5 (paste the DB query result showing the assistant reply). Comments explain the Anthropic signature-verification invariant and the D2 clearing contract, never rounds/incidents. No em-dashes.
