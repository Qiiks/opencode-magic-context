# Wire v2: rebuild ck-mc against subconscious f4c66051

Branch from `subc-migration` HEAD. The sibling checkout `../subconscious` is already at the
frozen build pin f4c66051 (wire v2: 21-byte envelope, ver=2, epoch u32, RouteHandle/on_bound in
subc-client-rs 0.2.0). Our crates use path-deps to it, so the rebuild picks v2 up automatically.

## Task
1. `cargo build` the workspace (crates/mc-core, mc-store, mc-tokenizer, mc-module) against the
   moved sibling. Fix any compile drift from subc-client-rs 0.2.0 API changes (expected candidates:
   on_bound optional callback added to the handler trait, RouteHandle types in consumer APIs used by
   the historian producer in crates/mc-module/src/historian_producer.rs, BindDecision shape).
   Semantics must not change: on_bind stays decision-only + binding-map install (that convention is
   pinned in the frozen spec); we emit no route traffic at bind so on_bound stays unimplemented
   unless the trait requires a body — then it's a no-op with a comment stating why.
2. Regenerate any golden vectors whose bytes legitimately change. EXPECT ck_wire_golden and codec
   goldens to be UNCHANGED (they are CK-message-level, not wire-envelope-level) — if one changes,
   STOP and report why instead of blindly re-pinning hashes.
3. Run the full gate: `cargo test -p mc-module -p mc-store -p mc-core -p mc-tokenizer`, the
   real_daemon integration test (it spawns the sibling daemon — now v2 — against our rebuilt
   module, so it is the real cross-version proof), `cargo clippy --workspace -- -D warnings`,
   `cargo fmt --check`.
4. Build the staged release binary: `cargo build --release -p mc-module` (bin ck-mc). Report the
   binary path + git SHA in your result. Do NOT deploy/bounce anything — SUBC deploys at flip-day.

## Constraints
- No behavior changes to transform/historian/facade logic. This is a wire-layer rebuild only.
- If the SDK renamed/moved something with multiple plausible mappings, read the SDK source in
  ../subconscious (crates/subc-client-rs) rather than guessing.
- Commit with co-author trailer `Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>`.
