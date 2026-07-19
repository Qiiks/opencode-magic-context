# Fix: notification protocol — instance-epoch cursors + exact-ID ACKs (W3-G #1 High, #2 Medium)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration, packages/plugin/src/. Two verified findings in the TUI notification protocol (server: shared/rpc-server.ts + shared/rpc-notifications.ts; client: tui/data/notification-socket.ts; shared/rpc-client.ts for endpoint record). Verify each at source first.

## Finding 1 (HIGH): cursors are not scoped to a server instance
rpc-notifications.ts:17-18 — `nextNotificationId` starts at 1 per server process. notification-socket.ts:66-68 — client cursor map `lastHandledIdByCursor` + dedup set `handledNotificationIds` persist across server restarts (module state in the long-lived TUI process). rpc-client.ts resolveEndpoint reads the port record which ALREADY carries `instance_id`. Failure: server restarts → IDs restart at 1 → surviving TUI's hello sends the OLD high cursor → server prunes the new backlog to that watermark (rpc-notifications prune-on-hello path) AND reused low IDs hit the dedup set → notifications acknowledged without the handler running. Symptom: /ctx-* dialogs silently lost after plugin restart until IDs exceed the old watermark.

FIX: thread the server's `instance_id` through the hello/connect flow and key ALL client cursor + dedup state by that epoch. On epoch change: clear cursors + dedup set BEFORE sending hello (a fresh server has no backlog the old cursor could legitimately prune). Server side: include instance_id in the hello ack (or health/port record — pick the least-invasive carrier already available; the port record has it, but the client must detect a NEW instance on reconnect through the SAME port too, so the ack/first-frame carrier is safer). Cursor keys become `${instanceId}:${scope}`. Dedup set scoped the same way (or cleared on epoch change).

## Finding 2 (MEDIUM): high-watermark ACKs delete unconsumed notifications
notification-socket.ts:148-150,187-219 — messages dispatch concurrently; client ACKs the highest consumed ID per scope. rpc-notifications.ts:156-178 — server ack deletes everything <= watermark for the scope. Failure: action N is slow/failed (returns false / awaiting an RPC load), toast N+1 completes first → ACK N+1 deletes unconsumed N. Symptom: a command result dialog vanishes because an unrelated toast raced past it.

FIX: switch to exact-ID acknowledgement. Client ACKs the specific IDs it fully handled (handler ran to completion, truthy result); server removes exactly those queue rows. Queue sizes are tiny (<=100), no watermark optimization needed. ALSO serialize notification handling on the client (a simple promise chain) so concurrent dialog actions cannot replace each other — this is load-bearing for exactness (a handler that hasn't finished must not be ACKed).

## Protocol compatibility
Both sides ship together in one plugin version, but a mid-upgrade skew window exists (old TUI ↔ new server or vice versa for one restart). Make the server accept BOTH ack shapes: legacy `{ cursor: N }` watermark (existing behavior) and new `{ ids: [..] }` exact form. Client always sends the new form. Version the hello with a `protocol: 2` field so the server knows the client clears state on epoch change (a protocol-1 client keeps legacy semantics). Do not break the legacy path — it dies naturally next release.

## Tests (all must be non-vacuous)
1. RESTART test: real second server MODULE state (the existing two-server test shares nextNotificationId — the Oracle proved it vacuous; use a child process, or export a __resetNotificationStateForTests and simulate the restart by resetting server module state while keeping client state). Assert: post-restart hello does NOT prune the new server's backlog, and a reused ID IS delivered (dedup cleared).
2. Exact-ACK test: two notifications, handler for N blocks/fails while N+1 completes; assert N survives on the server queue and is re-delivered on reconnect.
3. Serialization test: two dialog-action notifications arrive back-to-back; assert handlers run sequentially (second starts after first settles).
4. Skew test: legacy `{cursor}` ack still prunes as before.

## Constraints
- No changes to tui/index.tsx display logic, sidebar, or rpc-handlers command surface. This is the transport/protocol layer only.
- Keep the W2-C fixes intact: auth-before-accept on upgrade, scoped cursors (your epoch keying generalizes them), port-file nonce naming.
- packages/plugin: bun test (full), typecheck, lint, check_comments must pass.
- Comments explain invariants (why epoch-keying, why exact ACK), never referencing this audit.

Report: per-finding fix summary, test evidence, any deviation + why.
