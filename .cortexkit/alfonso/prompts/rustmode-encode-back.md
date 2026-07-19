# Rust MC mode — module encode-back: serve OpenCode-native messages on the transform response

Part of the per-project Rust MC cutover (plan: `.alfonso/plans/rust-mc-mode-v1.md` v2). Module-side only (crates/mc-module). Work on current branch HEAD (contains U2's host_directives — do not disturb it).

## The contract this creates

Today the transform response carries `ck_messages: Vec<CkWireMessage>` (canonical CK). The consumers (thalamus, broca) encode CK back to their provider wire themselves. The upcoming OpenCode thin adapter must NOT contain codec logic — it applies the module's output verbatim. So: when a transform request carries `serve_native: true` (new optional request field, `#[serde(default)]`), and the serializer profile is `opencode-aisdk`, the ok-response ADDITIONALLY carries `native_messages`: the full output array encoded back to OpenCode message-with-parts JSON via the existing section-E opencode codec (`crates/mc-module/src/codec/opencode.rs`).

Rules:
1. `serve_native` absent/false → response byte-identical to today (golden test).
2. `serve_native: true` on a non-opencode profile → typed error `serve_native_unsupported_profile` (fail loud, no silent ignore).
3. Encode-back fidelity: for messages the transform did NOT touch, the encoded form must be byte-identical to the ingress form the harness sent (the codec retains original values — verify how `original`/retained bytes flow: decode(x) then encode must yield x for untouched messages). For touched messages (tag overlays, drops, m0/m1 synthetics), encode produces valid OpenCode message JSON that the OpenCode serializer would accept: m0/m1 synthetic user messages must carry the `synthetic: true` part flag convention the TS renderer uses today (find the exact shape in the TS plugin's inject-compartments.ts and match it; the adapter will splice these directly into the live array).
4. Round-trip golden: extend the existing codec goldens (crates/mc-module/testdata/codec/) with a serve-native golden built from REAL redacted opencode.db shapes (the redaction convention exists in that testdata dir — follow it): decode → transform(defer, no changes) → encode-back → assert byte-identity with ingress for every untouched message, and assert the m0/m1/synthetic-todo messages match the pinned expected shapes.
5. Size: native_messages rides the same response; the response can exceed frame limits for huge sessions — mirror the request-side paging convention if a response paging mechanism already exists; if none exists, measure honestly: compute the response size for a large fixture and REPORT whether response paging is needed (do not build response paging speculatively; report the number and stop).

## Tests

- Golden: response without serve_native byte-identical across cc/owned/shadow/opencode profiles.
- serve_native round-trip fidelity as above.
- Unsupported-profile typed error.
- `cargo test -p mc-module`, clippy clean.

Commit in the worktree; do not push. Report the exact response field shape and the m0/m1 native shapes as implemented (they become the adapter contract).
