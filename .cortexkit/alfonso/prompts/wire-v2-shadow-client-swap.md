# Wire v2: replace hand-rolled SubcShadowTransport with @cortexkit/subc-client 0.4.0

Branch from `subc-migration` HEAD. This is MC's committed flip-day obligation from the frozen wire
spec round (#subc-wire-v1-final msg 41): DELETE the hand-rolled subc framing in the shadow sender
and adopt the blessed shared TS client, inheriting the v2 wire (21-byte envelope, ver=2), endpoint
epoch validation, RouteHandle with connection token, on_bound semantics, prefix-first reads, and
the late-RouteOpen GOODBYE rule — none of which we want to re-implement privately.

## Scope — packages/plugin/src/hooks/magic-context/shadow-sender.ts (+ package.json)
1. Add `@cortexkit/subc-client@0.4.0` as a dependency of packages/plugin.
2. DELETE the hand-rolled transport internals: SubcShadowTransport's private framing
   (SUBC_HEADER_LEN/SUBC_PROTOCOL_VERSION/FRAME_* constants, SocketReader, writeFrame/readFrame,
   readTerminalFor, parseErrorBody, authenticateSubcClient/proof/writeAuthMessage/readAuthMessage,
   connectTcp, readConnectionInfo — everything below the ShadowTransport interface that exists only
   to speak the wire). Keep the ShadowTransport INTERFACE and the sender/queue logic (processPass,
   performReset, seed budget, ordinal memo — all of that is lane logic, untouched).
3. Reimplement SubcShadowTransport ON TOP of the shared client: connect via the client using the
   connection file (same default path resolution as today: subc block in user config /
   getDefaultConnectionFile()), open/bind one route per session to module "magic-context" (same
   RouteTarget + BindIdentity shape the module expects — kind tool_provider, shadow:<sid> session
   namespace preserved EXACTLY; the Rust module keys shadow lineages on that prefix), send the
   flat-wire JSON bodies unchanged (toFlatWireBody output is the same bytes), honor the existing
   per-call timeout (SHADOW_SEND_TIMEOUT_MS) via the client's deadline mechanism or an outer race,
   and close routes via the client (closeSession keeps its semantics).
4. Preserve ALL existing failure semantics the sender relies on: isConnectionFailure /
   isPeerReject / isSeedBoundaryReject classification must still work — map the client's typed
   errors into the same discriminants (adjust those predicates to read the client's error codes,
   do not weaken them to catch-alls). Fail-open stays absolute: no client error may propagate past
   the sender's existing catch boundaries.
5. Wire-shape invariant: the module sees IDENTICAL request bodies (method + flat params) to today.
   The shadow-wire fixture (shadow-wire-fixture.test.ts + generate-shadow-wire-fixture.ts) asserts
   body bytes — it must pass UNCHANGED (if the fixture embeds envelope-level bytes rather than
   bodies, update only the envelope layer of the fixture generator and say so in the report).
6. Tests: existing shadow-sender.test.ts uses a mock ShadowTransport for most cases — those must
   pass untouched. For the transport itself, add/adapt a test against a fake server speaking v2
   (the shared client package ships test helpers per SUBC; if it doesn't, a minimal fake using the
   client's own connect surface is fine). Delete tests that only exercised the deleted hand-rolled
   framing (auth handshake bytes, frame parsing) — the shared client owns those now.
7. Gate: bun test src/hooks/magic-context/ (full dir), bunx tsc --noEmit, bunx biome check.
   Also `bun run build` must succeed (dist bundling with the new dependency — verify the client
   package doesn't break the single-file bundle; if it needs an external/asset arrangement, report
   rather than hack).

## Constraints
- Node/Bun compatibility: the plugin runs under Bun (OpenCode) — the client is Bun-first per SUBC;
  if any Node-ism breaks under Bun, report it (SUBC owns the client) rather than shimming.
- Do NOT touch the Rust module or the seed/queue/budget logic from commit 8b3fa315.
- The lane stays default-off (`shadow_transform.enabled`) and fail-open. No config changes.
- Commit with co-author trailer `Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>`.
