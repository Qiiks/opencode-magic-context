# Task: MC shadow-mode — MODULE SIDE (Rust, crates/)

Implement the module-side half of the shadow-transform lane per the spec in
`.cortexkit/alfonso/prompts/shadow-spec-v4.md` (read it FIRST, in full — it is the
contract; it went through 3 adversarial Oracle rounds and every clause is load-bearing).

You own ONLY `crates/` (mc-module, mc-store). Do NOT touch `packages/` — the plugin-side
sender is a separate task. Where the spec describes plugin behavior, that is context for
what arrives on the wire, not something you build.

## Deliverables

1. **Dispatch arms** (`crates/mc-module/src/lib.rs`): `state_sync`, `shadow_transform`,
   `shadow_reset` as explicit arms below method/kind precedence. Shadow ops are only
   accepted on a binding whose `BindIdentity.session` starts with `shadow:`; plain
   `transform` on a shadow binding is a typed reject and vice versa.

2. **Shadow meta** (mc-store): `shadow_generation` + `shadow_seq` + quarantine flag +
   ACKed-watermark bookkeeping persisted in the shadow session's meta (extend ModuleMeta
   or a side struct in the meta blob — your call, but it must ride the existing
   row_version CAS commit).

3. **state_sync** as ONE fenced transaction in the `publish_historian_chunk` mold:
   compartment/memory/mutation/todo mirror rows + row_version bump + seq CAS +
   generation check. Typed rejects for seq mismatch and stale generation.

4. **shadow_reset** as ONE fenced transaction: read generation → wipe/recreate shadow
   rows → generation+1 → shadow_seq=0 → clear quarantine → ACK returns new generation.

5. **BoundaryState three-way** (`transform.rs`): `LivePresent | DeclaredTrimValidated |
   Absent` with the FOUR predicates from the spec (all required; system-role/synthetic
   exemption on the continuity predicate). Predicate failure = Absent + trim-mismatch
   divergence record, never silent adoption. The existing fail-loud arms
   (pending_rewrite, #423 re-cut, mint-absent) must be provably unaffected for
   non-shadow profiles — do not change their behavior outside the shadow path.

6. **shadow_transform arm**: full classifier/compose/build_output pipeline using the
   request's pass_inputs verbatim (now_ms, model_key, usage, threshold, cache_ttl —
   never receipt-time clock or bind-time model); trigger evaluated via the PURE
   evaluation path only — must NOT call prepare_historian_fire; commits under the
   shadow key with normal CAS discipline; adopts supplied absolute_ordinals (never the
   codec's positional ordinals).

7. **Compare + divergence**: decode ts_output via the opencode codec, canonical
   structural CK compare (sorted keys, canonical numbers, block-by-block), expected
   divergence classes (synthetic-todo shape, agent-drop passes), quarantine on first
   hard divergence (decision-only recording after), divergence rows persisted
   (`shadow_divergences`: pass seq, class, first-diverging mid/block/field, bounded
   byte prefixes, normalization list from the request, TS decision, RS decision +
   state hash).

## Tests (the spec's Required test matrix section is the checklist — implement ALL of it)
Plus: gates are `cargo test -p mc-module --lib`, `cargo test -p mc-module --test
real_daemon`, `cargo test -p mc-store`, `cargo clippy --all-targets -- -D warnings`,
`cargo fmt --all -- --check`. All must pass. Run check_comments before committing.

## Rules
- Base: subc-migration HEAD (06a7514b or later).
- Commit with trailer: `Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>`
- No 0-as-sentinel for ordinals/seqs — Option<T> or explicit presence (this bug class
  has bitten three times; the 0-valued case must be tested).
- Comments explain invariants/failure modes, never reference Oracle rounds or spec
  version numbers.
- If a spec clause is ambiguous against the code you find, STOP and ask rather than
  improvising — this is a cache-critical surface.
