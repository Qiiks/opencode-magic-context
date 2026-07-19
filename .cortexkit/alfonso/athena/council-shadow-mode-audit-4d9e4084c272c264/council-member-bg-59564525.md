## Finding 1: `state_sync` compartment wire shape still does not match Rust serde
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts:584-604`; `crates/mc-module/src/lib.rs:266-274`
- **Confidence**: high
- **Issue**: TS sends compartments as nested `start`/`end` objects, but Rust requires flat `start_message`, `end_message`, `start_message_id`, `end_message_id`. Any state sync containing a compartment will reject with invalid params, blocking shadow coverage.
- **Evidence**: TS emits `start: { flat_id, bare_message_id, absolute_ordinal }` and `end: {...}`. Rust `ShadowCompartmentWire` has required `start_message: i64` and `end_message: i64`; no aliases/defaults. TS tests only assert `compartments: expect.any(Array)`, while Rust tests hand-build the old flat shape.
- **Suggested Fix**: Align schemas and add a cross-language fixture/serde test using actual TS builder output.

## Finding 2: Shadow enqueue does unbounded synchronous work on the live transform path
- **Severity**: high
- **Location**: `transform.ts:459,2186`; `shadow-sender.ts:1018-1059,361-395`; `read-session-raw.ts:76-89`
- **Confidence**: high
- **Issue**: “Fire-and-forget” starts only after expensive synchronous prep. The live path deep-clones messages, full-scans OpenCode message/part tables to resolve ordinals, deep-clones output, and reads hint decisions before queueing.
- **Evidence**: `resolveOrdinalsForShadow()` calls `readRawSessionMessages()` and builds a full map; DB implementation selects all session messages and all parts. `cloneJson` is JSON stringify/parse.
- **Suggested Fix**: Move ordinal resolution/denormalization into the background queue or cap/skip shadow for large inputs; avoid full DB scans by using targeted ordinal reads or already-computed metadata.

## Finding 3: Incremental sync can silently leave stale shadow compartments after recomp/replacement
- **Severity**: high
- **Location**: `shadow-sender.ts:524-528,553-560,646-647,696-735`; `compartment-runner-recomp.ts:110-123`; `mc-module/src/lib.rs:184-200`; `mc-store/src/lib.rs:2122-2125`
- **Confidence**: high
- **Issue**: Watermarks track only `MAX(sequence)`, and sync sends only compartments with `sequence > acked`. Recomp deletes/reinserts compartments and records an `m0_mutation`, but Rust ignores `m0_mutations` and TS does not resend same-sequence compartments. Shadow state can retain old compartments with no loud failure.
- **Evidence**: Rust `ShadowStateSyncWire` has no `m0_mutations` or `watermarks` field; store only upserts supplied compartments, no replace/delete.
- **Suggested Fix**: Add a compartment epoch/hash or force full replace/reset on recomp/truncation; have Rust consume explicit watermarks and delete stale rows.

## Finding 4: Subc request timeout can leave the socket framed incorrectly and not reopen
- **Severity**: medium
- **Location**: `shadow-sender.ts:1192-1204,1379-1388,1398-1402,783-788`
- **Confidence**: medium
- **Issue**: On timeout, the transport does not destroy the socket or clear routes. If `readFrame()` consumed a header and times out reading the body, the next read can start mid-body. Also `"subc request timeout"` is not classified as a connection failure, so route reopen/backoff is not triggered.
- **Evidence**: `unaryJson()` has no catch/cleanup around `readTerminalFor`; `isConnectionFailure()` only matches backoff/connection/ECONN strings.
- **Suggested Fix**: Destroy socket and clear routes on any read/write/protocol/timeout error; classify timeouts as connection failures; add partial-frame timeout tests and a max frame length.

## Summary
Findings: 3 high, 1 medium. I checked the relevant PARITY docs and existing shadow tests; the main gaps are not documented intentional divergences. Verdict: **HOLD**. The most important blocker is the TS/Rust `state_sync` compartment schema mismatch, which prevents meaningful production soak coverage for compartmented sessions.