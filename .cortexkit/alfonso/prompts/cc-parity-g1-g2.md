# CC-leg parity: G1 auto-search hints (lexical) + G2 temporal gap comments

Repo: this worktree (magic-context), branch base = subc-migration HEAD. Rust work in
`crates/mc-module` (+ `crates/mc-store` for one new table). Both features are
CC-leg-only surfaces in the module transform, gated on `cc_u1_active` (the U1 active
surface), OFF for every other profile. Read `crates/mc-module/src/transform.rs`
(overlay: `apply_tag_overlay_to_message`, channel-1 substrate:
`maybe_append_channel1_nudge` + `mc_channel1_appends` in mc-store) before designing.

## Cache-safety doctrine (non-negotiable, both features)

- Bytes rendered for a message must be a DETERMINISTIC pure function of durable
  state after first render: compute-once-persist-forever, replay byte-identical on
  every later pass (active passes; false/dormant windows serve raw verbatim —
  reversibility, same as the tag overlay).
- New content attaches ONLY at first sight of a message (when it is the newest —
  tail-end, zero prefix impact). Never first-apply anything mid-prefix on a later pass.
- Overlay-only: never mutate ingress projection, mint provenance (`taggable_source`
  bytes), or identity. Mutated blocks must clear retained pass-through bytes via the
  shared clear site in `apply_tag_overlay_to_message` (see the recent
  `target.mark_modified()` fix — your edits ride the SAME shared clear site).

## G2: temporal gap comments (build first, smaller)

TS reference: deterministic HTML comments `<!-- +12m -->` / `<!-- +2h 15m -->`
prepended to user messages when the gap since the previous message exceeds a
threshold (read `packages/plugin/src/hooks/magic-context/temporal-awareness.ts` for
format + thresholds; match the rendered format EXACTLY).

Module design (zero new state — reuse tag mint times):
- `mc_tags` rows carry the mint timestamp (`mint_or_get_tags` persists now_ms at
  first sight). First-seen time of a message = mint time of its first tagged block.
  Mint times are immutable → the rendered comment is deterministic forever.
- On active passes, for each USER-role message with a tagged block #0, compute gap =
  mint_time(this message's first block) − mint_time(previous ordinal's first block,
  any role). If gap ≥ the TS threshold, prepend the gap comment to the user text
  block IN THE OVERLAY, before the §N§ tag prefix (final shape:
  `<!-- +12m -->\n§5§ user text` — check TS for whether comment+newline precedes;
  match TS placement).
- Sessions activated mid-life: blocks minted in one pass share one timestamp → gaps
  = 0 → no comments. Correct by construction; test it.
- Expose the gap computation as a pure function; unit-test the format against TS
  goldens (generate a small fixture table from the TS implementation's outputs).

## G1: auto-search hints (lexical v1)

TS reference: `packages/plugin/src/hooks/magic-context/auto-search-hint.ts` +
auto-search-runner (hint block appended to the newest user message listing likely-
relevant memory/compartment fragments). Module v1 is LEXICAL ONLY (no embedding
lane): reuse the facade's lexical search SQL (`crates/mc-module` facade ctx_search
path — memories FTS/LIKE + compartment search) scoped to the session's project.

Design:
- New mc-store table `mc_user_hints` (session_id, block_id, hint_text, created_at;
  PK (session_id, block_id)) — the persisted-append substrate, same shape as
  `mc_channel1_appends`. Migration 18 (additive CREATE TABLE; bump the migration
  list; follow the existing migration test conventions in mc-store).
- Trigger: on an ACTIVE pass, when the newest message is a USER message whose first
  block has no `mc_user_hints` row yet AND no row was ever written for a LATER
  ordinal (never first-apply behind the frontier): run the lexical query over the
  user text (strip §N§ prefixes + system-reminder wrappers from the query text; cap
  query to first 500 chars), take top 3 results above a floor, render a compact
  block:
  `\n\n<ctx-search-hint>\nYour memory may contain related fragments:\n- <one-line each>\nIf relevant, run ctx_search to retrieve full context. Otherwise ignore.\n</ctx-search-hint>`
  (mirror the TS wording; cap total hint at 600 chars). Zero results → persist an
  EMPTY row (so the decision is durable and the message is never re-evaluated).
- Apply: overlay appends the persisted hint_text (when non-empty) to the user text
  block, AFTER the tag prefix application, same shared clear site. Replays on every
  active pass from the row; false windows verbatim.
- Bound the hot-path cost: one indexed lookup to detect "newest user needs hint";
  the lexical SQL only runs once per user message ever. No embedding calls anywhere.

## Tests (fail-first where marked)

- G2: format goldens vs TS; determinism across passes (two actives byte-identical);
  gap-0 mid-life activation; false-window verbatim (reversibility); mutant: remove
  the comment-prepend → golden test fails.
- G1: hint computed once (second pass replays bytes without re-running SQL — assert
  via a query-count seam); frontier rule (older user message never gains a hint
  after a newer one was processed); empty-result durable skip; false-window
  verbatim; wire-deserialized fixtures ONLY for output assertions (retained-bytes
  lesson: build fixtures through serde deserialization, see `wire_item` helper).
- Full gates: cargo test -p mc-module -p mc-store, clippy --all-targets, fmt.

House rules: comments explain WHY (invariants/failure modes), never reference this
task or audits; NO em dashes anywhere (comments, strings, docs); commit with
Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>.
