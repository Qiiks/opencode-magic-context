# Build U1: §N§ tagging + facade ctx_reduce + Channel-1 nudge for the MC Rust module (pre-built, flip-gated)

Repo: /Users/ufukaltinok/Work/Projects/CortexKit/magic-context, branch `subc-migration`. Rust module
only (`crates/`). Do NOT touch `packages/`. Do NOT push.

## Mission and sequencing context
This is the Claude-Code-parity "cutover 3" feature set, PRE-BUILT now so it can be flipped later.
The deploy chain is: cutover 1 (guidance prepend, AIPROXY building) -> cutover 2 (U0 full-array
apply) -> cutover 3 (THIS, flipped). You build everything BEHIND a per-profile capability gate that
is FALSE for every profile, so the merged build is byte-inert on all legs until the one-line flip
commit (mine, at the deploy window). The build must be provably inert: with the gate off, every
existing golden vector and test passes byte-identically.

## Reference implementations (read these first — the TS plugin is the semantic oracle)
- packages/plugin/src/hooks/magic-context/tag-messages.ts (tag minting + prefix injection semantics)
- packages/plugin/src/features/magic-context/tagger.ts + storage-tags.ts (durable tag rows, token counts)
- packages/plugin/src/tools/ctx-reduce/tools.ts (range parsing: "3-5", "1,2,9", "1-5,8")
- packages/plugin/src/hooks/magic-context/ctx-reduce-nudge.ts (Channel-1 threshold math)
- crates/mc-module/src/selection.rs (flat-block identity model: tool blocks = <call_id>#call / #result)
- crates/mc-module/src/transform.rs (reduction machinery: pending_agent_drops queue, frozen red:* units,
  build_output overlay pipeline, tail_reclaim gates at ~762-779)
- crates/mc-store/src/lib.rs (migrations — find the current highest version and add the next)

## Locked design pins (from the ratified cc-leg-parity design; violating any is a rework)
1. TAG MINTING IS FIRST-OBSERVATION, NOT BUST-GATED. A new block gets its tag number on the first
   pass that sees it (defer or execute — matches the OpenCode tagger). Numbers are monotonic,
   assigned in wire order, durable, and NEVER reassigned. Same block id => same tag number forever.
2. TAGS ARE A build_output OVERLAY — never frozen units, never mutating the ingress projection or
   block identity. The §N§ prefix is applied to the OUTGOING bytes at render time from durable tag
   rows; the stored/projected block content stays pristine. (Reductions frozen-unit machinery is a
   SEPARATE system; do not entangle them.)
3. DROP WINS OVER TAG: a block that is reduced (red:* frozen unit / drop placeholder) renders its
   reduction; no tag prefix is applied on top of a reduction placeholder.
4. CHANNEL-1 CLEAR ONLY STOPS FUTURE APPENDS: once a nudge reminder is appended to a given tool
   result's outgoing bytes it is part of that block's stable byte identity and must replay
   identically on every later pass; "clearing" the nudge state only prevents NEW appends to newer
   blocks. Byte-stability across defer passes is absolute.
5. Byte-determinism everywhere: for a fixed (durable state, input array), output bytes are identical
   across passes. The §N§ prefix for a block never changes once minted; prefix format matches the
   TS plugin's exactly (read tag-transcript/tag-messages for the exact bytes: prefix goes on text
   content and tool outputs the way OpenCode renders them — mirror the shared tag-transcript
   primitive's placement rules).

## The gate
Add to `crates/mc-module/src/healing.rs`:
```rust
/// Whether §N§ tagging + agent-facing reduction surface is active for this profile.
/// Flipped per-profile at the cutover-3 deploy window together with TAGGER_FEATURE_EPOCH.
pub const fn tagging_enabled(_profile: SerializerProfile) -> bool { false } // all-false until flip
```
Every new behavior in this build keys on `tagging_enabled(profile)`. The existing
`TAGGER_FEATURE_EPOCH` const (lib.rs) stays 0 — the flip commit bumps it (it is already folded into
render_config identity, so the flip forces a coordinated HARD per session; that is the designed
transition and not your concern beyond keeping the epoch referenced correctly).

## Unit A — durable tag rows (mc-store)
New migration (next version): table `mc_tags`:
  session_id TEXT, tag_number INTEGER, block_id TEXT, kind TEXT (message|tool_call|tool_result),
  token_count INTEGER, created_at_ms INTEGER,
  PRIMARY KEY (session_id, tag_number), UNIQUE (session_id, block_id).
Access methods on McStore: mint-or-get batch (given ordered new block ids -> assign next numbers
atomically), load-all-for-session (id->number map + number->id), sum token counts for a set.
Minting writes OUTSIDE the CAS pass-commit? NO — minting must be durable regardless of pass outcome
(first-observation semantics survive a rejected pass), so write tags in their own small transaction
before the pass-commit, idempotent via UNIQUE(block_id). Token count computed once at mint using
mc-tokenizer estimate over the block's rendered text.

## Unit B — §N§ overlay in build_output (mc-module transform.rs)
When `tagging_enabled(profile)`: after existing overlays (reductions, synthetic todo), apply tag
prefixes to outgoing blocks from the loaded tag map. Placement mirrors the TS tag-transcript rules:
user/assistant text blocks get the §N§ prefix on the text; tool results get it on the result
content. Never prefix: system messages, synthetic parts (m0/m1, synthetic todo pair), reduction
placeholders (pin 3), blocks without a mint (should not happen — mint pass runs before render; if a
block is somehow unminted, fail-loud in debug, skip-silent in release).
Mint step: on every transform pass (defer AND execute), diff the wire's taggable block ids against
the loaded map and mint the new ones in wire order (pin 1).

## Unit C — facade ctx_reduce (mc-module lib.rs facade routing)
Add `ctx_reduce` to the MCP facade dispatch (same shape as the existing ctx_memory/ctx_search/
ctx_expand/ctx_note handlers; FacadeScope resolution identical — conversation-key-scoped like
ctx_expand, NOT project-scoped). Input: `drop` string with ranges ("3-5", "1,2,9", "1-5,8") —
port the TS parser semantics exactly (including validation errors for malformed input). Resolve tag
numbers -> block ids via mc_tags; enqueue into the EXISTING `pending_agent_drops` queue
(INSERT OR IGNORE dedup — already built in RP-A). Unknown/never-minted numbers: reject that number
in the response text, process the valid remainder (match TS behavior — read the TS tool's response
wording and mirror its contract: queued-not-immediate framing). The DRAIN stays gated on
tail_reclaim (existing RP-A machinery, untouched) — your handler only APPENDS, which is safe on all
profiles today (documented: queue drains when the profile's tail_reclaim flips).
Response text: mirror the TS ctx_reduce's compact confirmation (queued count + skipped numbers).
Gate: if !tagging_enabled(profile of the bound session) -> typed error "tagging not active for this
session's profile" (the gateway hides the tool anyway; this is defense in depth).

## Unit D — Channel-1 nudge overlay
When `tagging_enabled(profile)` AND the session's reclaimable pressure crosses the threshold
(port the TS math: severity over the working window, reclaimable >= usable/3, where reclaimable =
sum of token_count over active (undropped) tags older than the working window; usable from the
request's context_limit), append the system-reminder nudge text to the NEWEST tool result block's
outgoing bytes (TS Channel-1 semantics; read ctx-reduce-nudge.ts for the exact reminder wording and
the re-fire suppression rules — port both). Durable state: a small channel1 row (last-appended
block_id + fired_at) in ModuleMeta or a store row, so replay is deterministic: every block that ever
received an append re-receives EXACTLY the same bytes on every later pass (pin 4 — keep an
append-set, not a single latest pointer).

## Tests (all must be non-vacuous; state for each how it fails if the logic is reverted)
1. INERTNESS: with tagging_enabled all-false (as built), the full existing suite + golden vectors
   byte-identical. Add one explicit test: a transform pass over a real fixture produces identical
   output with the U1 code present vs the pre-U1 expectation (reuse an existing golden).
2. Mint determinism: same session driven twice from empty store -> identical number assignment;
   numbers monotonic in wire order; re-pass mints nothing new; rejected pass (force a TransformError
   after minting) still leaves mints durable.
3. Overlay byte-stability: defer pass N and N+1 render identical §-prefixed bytes; a NEW tail block
   gets the next number without disturbing earlier bytes.
4. Drop-wins-over-tag: reduce a tagged block -> placeholder rendered WITHOUT prefix; other tags
   unaffected.
5. Facade ctx_reduce: range parsing (valid, mixed-valid, malformed), dedup (double-submit = one
   queue row), unknown numbers rejected in response, queue rows keyed by block id.
6. Channel-1: threshold fires once, append replays byte-identically on later passes, clear stops
   future appends but past appends persist, suppression window honored.
7. (Use the test-only gate seam: make tagging_enabled injectable for tests — e.g. a
   #[cfg(test)] override or a test-context flag — WITHOUT making the production const non-const.
   Document the seam.)

## Gate before returning
cargo test -p mc-module -p mc-store; cargo clippy -p mc-module -p mc-store --all-targets -- -D
warnings; cargo fmt --check; check_comments. All green. Report exact counts, every file:line, the
migration version you claimed, and any deviation from the pins WITH justification. Commit on
subc-migration with co-author trailer (check git log convention). Do NOT push. Do NOT flip the gate.
