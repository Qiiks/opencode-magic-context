# W3-G round 2 — upgrade-skew compatibility + sticky-cache inversion (v0.32.1)

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. An adversarial audit of the notification-protocol/W3-G delta found both one-release upgrade-skew directions deterministically disconnected, plus a sticky-cache inversion. Skew is REAL cross-process: port-file discovery lets a TUI from one install discover a server from another (user upgrades while an old `opencode serve` runs — the normal upgrade path). Verify each at source.

## Fix 1 (HIGH): new TUI + v0.32 server — discovery rejects every candidate
shared/rpc-client.ts:176-185 requires /health body instance_id to match the port record; v0.32 servers return only pid. FIX: accept a MISSING instance_id in the health body when the PID matches (explicit legacy case); continue rejecting a PRESENT-but-mismatched instance_id. Test with a frozen v0.32-shaped health response.

## Fix 2 (HIGH): v0.32 TUI + new server — WS 401 loop
shared/rpc-server.ts:56-63,243-249 accepts only the Authorization header; the v0.32 client sends ?token=. FIX: restore query-token acceptance for ONE release as a legacy fallback — prefer restricting it (e.g. only when no Authorization header is present), keep header auth primary. Comment it as a one-release skew bridge with removal note. Test with a frozen v0.32-shaped upgrade request (query token, no header).

## Fix 3 (MED): legacy-mode negotiation — no watermark advance past unconsumed notifications
tui/data/notification-socket.ts:195-243,295-350: against an OLD server (hello-ack lacks instanceId), the new client still advances high-watermark cursors while its exact-ID acks are ignored — a declined dialog (ID 1 false, ID 2 true) then reconnects with cursor 2 and the old server prunes both. FIX: negotiate mode off the hello-ack shape — missing instanceId = LEGACY MODE, and in legacy mode never advance the scope watermark past the oldest UNCONSUMED (handler-not-yet-true) notification. Buffer notifications received before hello-ack rather than processing against an unknown mode. Keep v2 mode exactly as-is for new servers.

## Fix 4 (MED): sticky-cache purge on transient server failure
Server side: plugin/rpc-handlers.ts (or wherever the sidebar snapshot handler builds) converts snapshot-build exceptions (e.g. SQLITE_BUSY) into an HTTP-200 all-zero snapshot. Client side: tui/data/context-db.ts:104-110,150-168 treats any successful zero response as authoritative and DELETES the cached snapshot. FIX both ends: server returns an explicit error envelope for build exceptions (not a fabricated zero snapshot); client treats the error envelope as transport-failure-class (serve sticky cache), while a GENUINE successful zero snapshot stays authoritative (deleted sessions must not resurrect). Test: server throws SQLITE_BUSY → client serves cached snapshot; server genuinely returns zeros → cache replaced.

## Fix 5 (LOW, shadow qualification): normalize subc timeout classification
hooks/magic-context/shadow-sender.ts:968-980,1105-1129: the inner 5s read/request timeouts throw un-coded Errors ("read timeout"/"subc request timeout") that fire BEFORE the outer 15s ETIMEDOUT timeout, so they classify as send_failures instead of connection failures → no route_reopen, and reseed cooldown/cap can be charged without lineage recovery. FIX: stamp code="ETIMEDOUT" on the inner timeout errors (or route them through the same classifier), and add a test that a 5s read timeout during reseed triggers connection-failure handling (route reopen) rather than burning a reseed attempt.

## Tests — the four-cell matrix (the audit's ask)
old-server/old-client is untestable here, but freeze the v0.32 SHAPES (health body without instance_id; ws upgrade with ?token; hello-ack without instanceId; watermark-only acks) as fixtures and cover: new-client+old-server (discovery succeeds via Fix 1; legacy mode via Fix 3; declined-dialog reconnect does NOT lose ID 1), old-client+new-server (WS connects via Fix 2 query token; legacy watermark acks processed), new+new (unchanged behavior — assert the v2 tests still pass byte-identical). Mirror any tui/ changes into tui-compiled/ and keep check:tui-compiled green.

## Gates
packages/plugin: bun test, typecheck, lint, check:tui-compiled, check_comments. Comments explain the one-release skew-bridge rationale and the authoritative-zero vs error-envelope distinction; no audit refs. Report per-fix status + test evidence.
