# Fix: rust-mode module output is silently NOT reaching the wire (apply seam no-op)

Repo: this worktree (branch from `subc-migration` HEAD). TS plugin primarily. This is the real bug behind the last three "same 400" beats — verify the mechanism, then fix with a fail-loud invariant so this class can never be silent again.

## Evidence (tonight, all on disk)

Beat 8 on `ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO` (transform_mode rust, fresh TUI process):
- MC log 21:43:18.3Z: transform stages start, messages=304. Then NOTHING rust-related — no failure, no LKG line — until guidance injection at 21:43:22.3.
- Module store mc_pass_trace: pass received 21:43:21.8, completed 21:43:21.9, reject_count=0 (receive_count=12 total — module has been receiving and completing passes across beats).
- Wire dump 21:43:23.07 (/var/folders/18/257zzylx4h1gbkcvs4cnpqqc0000gn/T/opencode-anthropic-auth-dumps/2026-07-17T21-43-23-074Z-00001-ses_l7l9...body.json): 333 messages, RAW shape — messages[0] is the raw post-marker user ("What did we do so far?"), NO m0 (no <project-docs>/<session-history>), 86 historical thinking blocks intact, and Anthropic 400s on the frozen historical assistant at index 65.

Conclusion: the module runs and commits (boundary minted at beat 4, store healthy), but its output array NEVER replaces the wire — the adapter's apply is a silent no-op, and every "serve" since has been raw passthrough WITHOUT any failure log. The last three fix rounds (assistant-run strip, D2 clearing port) were validated module-side but the wire never carried module output at all.

## Investigate exactly this seam

packages/plugin/src/hooks/magic-context/rust-mode-transform.ts (`runRustModeTransform`, `applyNativeMessagesVerbatim`) and its integration in transform.ts / messages-transform.ts / hook glue:
1. OpenCode's experimental.chat.messages.transform contract: how does the TS pipeline deliver its result? (The TS pipeline mutates the messages ARRAY IN PLACE — splice/length=0/push — check transform.ts's normal return path.) If the rust path builds a new array and returns it, or assigns to a local, the hook returns the ORIGINAL args.messages untouched -> silent raw serve. Also check the wrapper's plumbing: does createMessagesTransformHandler return the rust path's array, and does the caller (plugin messages-transform adapter) actually use the returned value, or rely on in-place mutation?
2. Why is there NO log line for a successful rust pass? The adapter must log one info line per pass: decision, served_from, messages in/out count. Absence of logging is what let this run silent for 4 beats.
3. Check the response handling: serve_native flag on the request; whether the module's ok response carried native_messages for these passes (module-side attach requires serve_native=true — if the adapter never sets it, applyNativeMessagesVerbatim's absent-throw should have fired... unless that throw is swallowed somewhere. Find where the throw goes.)

## Fix requirements

1. Apply the module output through the SAME mechanism the TS pipeline uses (in-place array mutation of the hook's messages reference; keep returning the array too). No new contract invention.
2. FAIL-LOUD INVARIANT: after apply on a pass where the module reports a present boundary, assert the wire array's first non-system message is the module-rendered m0 (synthetic-marked, sessionID-scoped). Assertion failure -> loud error log + LKG ladder, never silent raw.
3. One info log line per rust pass: `rust pass: decision=<...> served_from=<...> in=<n> out=<m> applied=<bool>`.
4. Regression test at the HOOK level (not the internal function): drive the actual transform handler with a fake module client returning a transformed array; assert the hook's OUTPUT (the array OpenCode would serialize) is the module array, both via return value and via the original reference the caller holds. A mutant that returns-without-mutating must fail the test.
5. Rebuild the plugin dist (bun run build) as part of your verification, then LIVE-VERIFY like round 4 did: /Users/ufukaltinok/.opencode/bin/opencode run -s ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO -m anthropic/claude-opus-4-8 "Wire beat R5: reply OK" from ~/Work/Projects/CortexKit/benchmarks (binary NOT on your PATH — use the absolute path). Success = assistant reply, no 400, AND the newest wire dump for the session shows messages[0] containing <project-docs> (m0 on the wire). Check the dump dir /var/folders/18/.../opencode-anthropic-auth-dumps with find (ls globbing overflows). If a 400 remains AFTER m0 is proven on the wire, capture the failing message coordinates and report — do not iterate past your scope.

## Gates
Focused suites (rust-mode-transform, module-wire, module-state-sync) + full plugin typecheck + biome. cargo untouched unless evidence demands; if module change needed, justify. Comments explain the hook's in-place contract invariant. No em-dashes. Report: convicted mechanism file:line, fix, test names, live-beat proof (DB row + dump m0 evidence).
