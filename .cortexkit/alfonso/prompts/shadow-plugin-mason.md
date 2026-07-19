# Task: MC shadow-mode — PLUGIN SIDE (TypeScript, packages/plugin/)

Implement the plugin-side sender half of the shadow-transform lane per the spec in
`.cortexkit/alfonso/prompts/shadow-spec-v4.md` (read it FIRST, in full). A parallel task
builds the module side in `crates/` — do NOT touch `crates/`. Where the spec describes
module behavior, that is the wire peer you talk to, not something you build. Build
against the spec's wire shapes exactly; integration happens after both merge.

You own ONLY `packages/plugin/`. Everything must be dead-off when the flag is off.

## Deliverables

1. **Config**: `shadow_transform: { enabled: z.boolean().default(false) }` in
   MagicContextConfigSchema with .describe() (dev flag: mirrors every transform pass to
   the MC Rust module over subc and byte-compares outputs; no behavior impact).
   USER-TIER ONLY: strip it in project-security.ts like the other trust-tier fields.
   Regenerate assets/magic-context.schema.json via scripts/build-schema.ts.

2. **Shadow sender** (new module, e.g. src/hooks/magic-context/shadow-sender.ts, wired
   from the transform seam AFTER the TS output is finalized — find the right seam in
   messages-transform.ts/transform.ts where both the input array and the final output
   array are in scope):
   - fire-and-forget, NEVER awaited on the hot path, NEVER throws into it (fail-open,
     every failure counted + logged debug-level),
   - per-session strict FIFO, one in-flight op, in-flight cap 4 (drop-oldest, counted),
   - subc connection via the standard connection file discovery
     (~/.local/share/cortexkit/run/subc-connection.json) — read how other consumers
     discover it; if unreachable, count + skip (no retry storm: reprobe with backoff),
   - route bound as BindIdentity.session = "shadow:<real_sid>".

3. **Ordinal resolver**: canonical raw order (time_created ASC, id ASC, summary rows
   excluded) via readRawSessionMessages() inside the scoped raw-message cache — NEVER
   readRawSessionMessageById(), NEVER hook-array position. Per-session id→ordinal memo
   scoped to {sid, shadow_generation}; unresolvable → skip pass (counted);
   re-resolution mismatch → shadow_reset. Reuse the existing scoped-cache plumbing the
   trigger/ctx_expand paths use.

4. **Wire ops per spec**: state_sync (delta mirror: TS compartment rows translated to
   flat CK block ids — derive the flat id from the compartment end message + block
   index rules documented in the spec; memories + mutation-log deltas;
   last_todo_state), shadow_transform (input + annotated absolute ordinals + sender-
   denormalized ts_output + pass_inputs {now_ms snapshot taken at TS pass start,
   model_key, usage, effective threshold, cache_ttl} + ts_decision + declared_trim +
   shadow_generation), shadow_reset.

5. **ACK bookkeeping**: in-memory last_acked watermarks/seq/generation; sync scheduling
   keys on ACKed state; CAS/seq/generation reject → no transforms until full resync;
   cold start (restart/route reopen) → reset-then-sync-then-resume, adopt generation
   from reset ACK.

6. **Sender-side denormalization**: strip §N§ prefixes using the tagger's exact
   per-part knowledge (never regex over arbitrary content) and <ctx-search-hint> blocks
   by exact block identity; list every strip in the request's normalizations field.

7. **declared_trim**: derived from the TS compaction-marker state + the boundary
   compartment row; flat_boundary_id computed once per marker advance and persisted in
   the sender's bookkeeping (in-memory is fine given the cold-start rule).

## Tests
- Flag-off: zero code paths execute (no connection attempt, no ordinal resolution).
- FIFO/cap/drop-oldest; failure isolation (sender exception cannot reach the transform
  return path — prove with a throwing fake).
- Ordinal resolver: post-trim window resolves original ordinals (fixture with a
  compaction marker); revert-style re-resolution mismatch triggers reset; unresolvable
  skips.
- Denormalizer: §N§ strip via tagger state on a fixture with prefixed tool outputs +
  a tool output whose CONTENT legitimately starts with "§12§" (must survive).
- ACK bookkeeping: dropped sync → resend before next transform; reject → transforms
  gated; cold-start sequence.
- Wire-shape golden: serialized state_sync + shadow_transform requests for a fixture
  pass match the spec's field inventory (this is the contract the module mason builds
  against).

Gates: bun test packages/plugin (full suite), tsc, biome lint. check_comments before
committing.

## Rules
- Base: subc-migration HEAD.
- Commit trailer: Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>
- The transform hot path must be provably untouched when disabled: the only added cost
  with the flag off is one boolean check.
- Comments explain invariants, never process artifacts (no Oracle/spec-version refs).
- Ambiguity against real code → STOP and ask.
