# CC parity phase 2: Oracle BLOCK remediation (6 findings, 3 blocking)

Repo: this worktree (magic-context), branch base = subc-migration HEAD (contains the
merged G1/G2/G5 delta plus disposition fields). Rust work in crates/mc-module +
crates/mc-store. The Oracle audit findings below are source-verified; implement all
of them. Read the cited code first; keep every existing test green and extend the
suites named per finding. Cache doctrine is unchanged: bytes rendered for a message
must be a deterministic pure function of durable state after first render; nothing
first-applies mid-prefix on later passes; false windows serve raw verbatim.

## F1 (BLOCK): G2 temporal decisions must be persisted, not derived

temporal_gap_overlays (transform.rs ~2543) derives gaps from the previous PRESENT
array element, but ingress allows sparse increasing ordinals: for [1,3] the marker
on 3 derives from 1, and if ordinal 2 later reappears (sparse-ordinal recovery,
Claude Code re-sends), the already-rendered prefix of 3 would change on a defer.

Fix: make G2 decision-persisted exactly like G1 hints. New table mc_temporal_marks
(session_id, block_id, marker_text, created_at, PK(session_id, block_id)) in
migration 19 (marker_text may be empty = durable no-marker decision). At first
sight of a user message on an active pass (same trigger discipline as hints),
compute the marker ONCE from the predecessor visible at that moment, insert the
row, and render from rows forever after. The overlay reads only persisted rows.
Since already-active sessions rendered derived markers under tfe2, bump
TAGGER_FEATURE_EPOCH 2 -> 3 (one more coordinated fold; update the staged-pin test
constants and the constant's doc comment).

Tests: sparse-ordinal fixture [1,3] renders a marker for 3 frozen at first sight;
ordinal 2 appearing later changes NOTHING (byte-identical replay assert); empty
decision durable; false-window verbatim.

## F2 (BLOCK): hint frontier must be an atomic ordinal watermark

user_hint_frontier_open (mc-store ~2449) orders by tag number, but a late-restored
older ordinal mints a HIGHER tag number, so it can backfill after a re-cut; a
trailing assistant/system message closes nothing (only hint rows advance the
frontier); pending_rewrite passthrough returns before maybe_append_user_hint so
active first sight is missed; the frontier check and INSERT OR IGNORE are not
atomic (concurrent transforms can render different hints for the same message);
and block id is hardcoded mid#0, missing user messages whose first TEXT block
follows media/opaque blocks.

Fix: store a monotonic max_seen_ordinal per session (new column on the same
migration 19, or a one-row table), advanced on EVERY active pass inside the same
transaction as any hint insert. Eligibility = message ordinal > stored watermark
at the atomic check-and-insert (single transaction; on conflict return the
canonical stored row and render THAT). Evaluate hints for the newest user message
even when the pass takes the pending_rewrite passthrough (hints are overlay-only
and the passthrough already applies the overlay). Target the first TEXT block of
the message (scan block kinds), not literal #0. The same watermark naturally
covers F1's mark inserts (share it).

Tests: late-restored older ordinal cannot gain a hint (fail-first: revert to tag-
number ordering, test fails); trailing assistant message still closes the user's
frontier; concurrent double-insert returns one canonical row (loser renders
winner's bytes); user message with [media, text] gets the hint on the text block;
pending_rewrite pass computes the hint.

## F3 (BLOCK): wrapup input needs a freshness fence + bounded cache

latest_transform_requests is refreshed only on successful transform completion; a
transform that commits a re-cut and THEN rejects leaves the OLD array paired with
NEW store state, and wrapup would summarize retired bytes. Cache also unbounded
across sessions.

Fix: replace the raw map with a snapshot state machine per session:
InFlight (set at transform START, before any commit) -> Ready{request, revert_epoch
observed at completion} (set only on SUCCESS). session.wrapup requires Ready AND
snapshot.revert_epoch == current meta revert epoch; otherwise respond
{ok:false, disposition:"failed", summary:"wrapup unavailable until a full session
transform has been observed"} (reuse the existing arm). Add a byte-accounted LRU
cap across sessions (suggest 64 MiB total, evict oldest Ready; InFlight entries
are markers only). Session lifecycle eviction stays.

Tests: transform that commits a re-cut then rejects leaves the snapshot InFlight
and wrapup refuses (fail-first: with the old map the stale array would be used);
epoch mismatch refuses; LRU eviction works and evicted session refuses wrapup
honestly.

## F4 (REVISE): global wrapup budget + honor historian backoff

Busy joins do not consume the round cap, so pathological interleaving can extend
the op unboundedly; and wrapup skips the durable failure-backoff gate the organic
path enforces.

Fix: one absolute deadline for the whole op = historian::MAX_WRAPUP_REQUEST_BUDGET
minus a small margin (compute remaining budget before each round/join; expire ->
failed disposition with progress summary). Honor the durable failure backoff
(HISTORIAN_FAILURE_BACKOFF_MS) at op entry AND between rounds: if backoff is
active, fail with a summary naming the remaining backoff. No override in this
round (a manual override needs its own design).

Tests: budget expiry mid-joins produces failed with rounds preserved; entry under
active backoff refuses with the backoff named.

## F5 (REVISE): reversed eligible range when offset > cached terminal

resolve_wrapup_boundary: protected_tail_start.max(offset) then clamp_ordinal can
produce start < offset when compartment coverage exceeds the cached transcript's
terminal ordinal, yielding a reversed eligible_head range. Return an explicit
empty range anchored at offset in that case (eligible_head = offset..offset,
boundary_reason "manual-wrapup-empty").

Test: coverage beyond cached terminal yields the empty range, wrapup responds
nothing_to_compact.

## F6: adversarial test sweep (from the audit's action plan)

Beyond the per-finding tests: pending-passthrough hint computation, Busy/Emergency
interleaving (wrapup joins, never double-drives, budget still bounds), and a
rejected post-recut transform followed by wrapup (must refuse).

Gates: cargo test -p mc-module -p mc-store, clippy --all-targets -- -D warnings,
cargo fmt --all -- --check, check_comments. House rules: comments explain WHY,
never reference this audit; NO em dashes; commit with
Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>.
