# Round 3: capture the ACTUAL Anthropic wire request for the failing beat, diff rust-mode vs ts-mode, fix at the proven layer

Repo: this worktree (branch from `subc-migration` HEAD, contains rounds 1-2: 43ffc79f ingress exemption, 524c96d6 merged-run strip). The 400 SURVIVES both fixes. No more topology reasoning from the module's view — this round captures the real wire bytes.

## Evidence that invalidates the previous premise

Beats 4, 5, 6 all failed with EXACTLY `messages.65.content.4` while the OpenCode input array GREW (222 → 225 → 228). Frozen coordinates across growing input mean: (a) the "latest assistant" on the wire is the FROZEN HISTORICAL newest assistant of the cloned session (the beats append user messages AFTER it; Anthropic's rule targets the last assistant-role message, not the last message), and (b) served-array index arithmetic from the module's view was the wrong instrument — OpenCode's AI-SDK serializer EXPANDS assistant messages (tool results split into separate wire messages), so wire index 65 does not equal served index 65. Rounds 1-2 may both have been aimed at the wrong message entirely.

## Method (wire capture, both modes)

1. Build a throwaway capture proxy (node, ~40 lines): listens on 127.0.0.1:PORT, forwards verbatim (headers + body) to https://api.anthropic.com, dumps each request body to /tmp/mc-wirecap/<timestamp>-<mode>.json, streams the response back. No mutation.
2. The drive project is ~/Work/Projects/CortexKit/benchmarks with the clone session `ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO`. Point anthropic at the proxy via project-scoped opencode config (benchmarks/opencode.json provider.anthropic.options.baseURL — verify the exact key against the installed opencode's config schema; the OAuth auth plugin must keep working since provider id is unchanged). REVERT this config when done.
3. RUST beat: `cd benchmarks && opencode run -s ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO -m anthropic/claude-opus-4-8 "Wire beat R: reply OK"` — expect the 400; capture the request. If `opencode run` cannot target the session, use the PTY pattern (bash pty:true + bash_write).
4. TS CONTROL beat: edit benchmarks/.cortexkit/magic-context.jsonc transform_mode "rust"->"ts", SAME command, capture. TS mode is the daily-prod reference; expect success (if TS ALSO 400s, that is a huge finding: the raw clone tail itself is poisoned — report and stop, do not fix module code for a clone-tool artifact. Check whether clone-session.ts remint could have altered part ordering or dropped fields on the mid-tool-turn assistant). Restore transform_mode to "rust" after.
5. Diff the two request bodies: locate messages[65] in each, identify its source OpenCode message id (match text content against opencode.db), and produce a block-level diff of the latest wire-assistant message (and its neighbors 60-66): block order, thinking signature bytes, text prefixes (tag §N§ overlays), missing/extra blocks, cache_control placement.
6. Fix at the layer the diff convicts. Candidates in likelihood order given rounds 1-2: tag overlay prefixing a text block inside the historical newest assistant (TS mode may exclude the newest assistant from wire prefixes); encode-back emitting typed re-encode instead of retained bytes for a message the tag pass touched; the merged-run strip now stripping a block Anthropic expected verbatim (over-strip: if TS control shows thinking PRESENT on the same message, round-2's strip is itself the modification and must be scoped to what TS does exactly). Match TS-mode wire byte behavior for this message class exactly; no invented policy.
7. Regression: extend the replay/unit fixtures with the convicted shape asserting the wire-projection for that message matches TS-mode reference bytes.

## Hygiene

Proxy + captures stay in /tmp (never committed). Revert both config edits (opencode.json baseURL, transform_mode). The clone session is disposable — beats through it are free, but keep it on the rust lane at exit. cargo test -p mc-module + clippy green. Report: the two wire dumps' paths, messages[65] identity + block diff table, convicted layer with file:line, fix, tests. No em-dashes in comments.
