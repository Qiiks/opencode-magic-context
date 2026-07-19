# Implement the MC module connection slice (#406), design v3

You are implementing a fully-designed slice in the Rust workspace of this repo
(branch `subc-migration` — your worktree forks from it). The design doc is
embedded below; it is FINAL (3 SUBC contract rounds + an Oracle REVISE already
folded). Do not re-litigate design decisions; implement them exactly. All
clauses marked as Oracle findings (F1/F2/F3/F6/F7) and SUBC rulings are
load-bearing.

## Scope

Crates: `crates/mc-module` only (touch `mc-store` only if a diagnostics field
needs `ModuleMeta`; none is required by the design). Files you will mainly
touch: `src/transform.rs` (wire structs, response build), `src/lib.rs`
(handle_transform_value validation + error/response paths), new
`src/healing.rs`, `tests/real_daemon.rs` (one v2 wire vector).

## The design doc (verbatim, authoritative)

<design>
# MC module: connection slice (#406) — design v3 (post Oracle bg_5fbc635f REVISE)

Oracle findings folded: raw-wire parse layer so contract codes are typed (not
serde bad_request); tail_delta explicitly modeled (serde would silently IGNORE
an unknown field today — worse than unrejected); need_full_sync becomes a
SUCCESS-SHAPED response (consumer contract: application-level rejections are
ordinary response bytes; Error frames route to fatal handling); error frames
are UNPAIRED for LKG (consumers pair by their own request fingerprint and
never update LKG from errors); v2 test matrix replaces the single defer
vector.

SUBC rulings folded (pm_5c09d950): served_from closed enum; HARD-REQUIRE
serializer_profile with producer-first cutover (no transition default);
need_full_sync as the single canonical re-send code; fingerprint echo-only,
never recomputed (store-and-compare even in future staleness checks).

Goal: upgrade the transform wire request from the slice-2 internal shape to the
full #4 connection contract, and implement the two consumer-facing behaviors the
richer shape enables: serializer-profile quirk residuals and
full-array-fingerprint staleness/LKG validity. This is the last wire piece the
MITM leg's daemon-down LKG path needs (spec §10).

## Grounding (source, as of subc-migration HEAD ea836e8a)

- Current wire request (`transform.rs:132`): `{kind, v, session_id,
  render_config, messages: CkIngressMessage[], usage?, provider_error?}` plus a
  legacy `items` shim. `boundary_present` deliberately absent (module-computed,
  poison-surface rationale in the doc comment).
- Note #406 pins the #4 shape: `{session_id, serializer_profile, render_config,
  full_array_fingerprint, payload}` where `payload = {full: CK[]} |
  {tail_delta}`. Two load-bearing extras:
  1. `serializer_profile` REQUIRED (Edge C): maps to a healing-coverage table →
     `quirk_residual = provider_requirement − serializer_healing(provider,
     profile)`; gates the reasoning-strip merge-term.
  2. `full_array_fingerprint`: whole-input identity, DISTINCT from the cache
     boundary anchor (de-conflated by an earlier Oracle). Drives delta
     staleness + LKG-replay validity (spec §7/§10).
- Spec §3: ALWAYS-FULL is the baseline; `tail_delta` is an optional
  optimization the module may reject with NEED_FULL. The MITM leg has no delta
  source, so v1 of this slice implements FULL only and rejects `tail_delta`
  with a typed error (forward-compatible field, not dead code: the wire shape
  is frozen now so consumers don't re-integrate later).
- Spec §10 (daemon-down): the #4 CONNECTION layer (consumer side) owns
  LKG-replay; the module's obligation is (a) clean Error results (never
  partial/raw), already true, and (b) providing the fingerprint echo the
  consumer needs to validate its cached LKG against the current live array.

## Changes

### C1. Wire request: add `serializer_profile` + `full_array_fingerprint`

`TransformRequest` gains:

- `serializer_profile: String` — REQUIRED on v2 wire. Known profiles v1:
  `"owned-llmrunner"`, `"claude-code-anthropic"` (ai-proxy CC leg),
  `"opencode-aisdk"`, `"pi"` (plugin leg, inert until section-E). Unknown
  profile → typed error `unknown_serializer_profile` (fail-loud; a silently
  wrong healing table is a wire-corruption class).
- `full_array_fingerprint: Option<String>` — caller-computed identity of the
  raw array it handed us (algorithm caller-owned and opaque to the module; the
  module never recomputes it, only echoes). OPTIONAL on the wire, by role: it
  serves consumer-side LKG pairing only, so LKG consumers (ai-proxy) send it
  and fail-closed consumers (llm-runner, no LKG replay) omit it. v2's
  hard-require covers ONLY serializer_profile.

Wire `v` bumps to 2 and `serializer_profile` is HARD-REQUIRED: missing is the
same typed failure as unknown (`unknown_serializer_profile`). Implementation
pin (Oracle F1): the raw wire struct parses `serializer_profile:
Option<String>` (and `tail_delta: Option<Value>`), and semantic validation
runs POST-parse in `handle_transform_value` BEFORE any binding/store access —
a non-Option serde field would surface as generic `bad_request` (lib.rs
serde-failure mapping), and `#[serde(default)]` would be the silent-default
corruption this design rejects. No transition
default — a silent missing→owned-llmrunner default would mask a producer that
should send `opencode-aisdk` and get wrong healing (subtle corruption class).
Cutover is PRODUCER-FIRST: llm-runner adds `serializer_profile:
"owned-llmrunner"` to its transform request BEFORE this module ships v2 (SUBC
brokering; it rides llm-runner's emergency-budget timeout commit), so the
module-v2-deployed-but-producer-silent window never exists. ai-proxy's value
leg is v2-native with `"claude-code-anthropic"`. Legacy `items` shim unchanged.

### C2. Response: fingerprint echo + `served_from`

`TransformResponse` gains:

- `full_array_fingerprint: Option<String>` — verbatim echo of the request
  field, on ALL SUCCESS-SHAPED responses (normal, child-session pass-through,
  and need_full_sync). The consumer's LKG cache stores `(fingerprint,
  response)`; on daemon-down it replays its cached response only if its CURRENT
  array fingerprint equals the cached one (spec §10's "anchor matches" check
  made precise). ERROR FRAMES ARE UNPAIRED (Oracle F3: ErrorBody carries only
  {code, message}): the consumer pairs by its own request fingerprint and MUST
  NOT update LKG from an error — stated in spec §2 as a consumer obligation.
- `served_from` — a CLOSED enum, not a free-form string: `"transform"` today,
  `"daemon_lkg"` reserved for a future daemon-side LKG shortcut (serve cached
  bytes without a transform pass). A future value is a typed addition a
  consumer matches on, never a string to guess at. v1 always emits
  `"transform"`.

### C3. Healing-coverage table + quirk residual

New `mc-module/src/healing.rs`:

```
pub enum SerializerProfile { OwnedLlmRunner, ClaudeCodeAnthropic, OpencodeAiSdk, Pi }
pub struct HealingCoverage {
    pub drops_empty_content: bool,      // empty text/thinking blocks dropped by serializer
    pub autofills_reasoning: bool,      // reasoning_content auto-filled when required
    pub merges_consecutive_assistants: bool, // serializer merges adjacent assistant msgs
}
pub fn coverage(profile) -> HealingCoverage
pub fn quirk_residual(profile) -> QuirkResidual   // what the MODULE must still do
```

Values ported from the verified healing-profiles table (memory 7688 +
ck-message-field-inventory.md):

- owned-llmrunner: heals like Pi (drop empties universally, autofill
  reasoning_content, never merges assistants) → residual EMPTY.
- pi: same → residual EMPTY.
- opencode-aisdk: empties healed for anthropic+bedrock only; @ai-sdk/anthropic
  merges consecutive assistants → residual = `[dropped]` sentinel on
  non-anthropic empties + reasoning-strip-from-merged-assistants (both inert
  until the plugin leg activates; the table entry exists, the transform
  branches on it, tests pin it).
- claude-code-anthropic: CC emits native Anthropic wire; Anthropic requires
  non-empty blocks and merges nothing client-side → residual =
  `[dropped §N§]`-style non-empty placeholders only (which the module already
  emits for reductions) → effectively EMPTY beyond current behavior.

v1 consumers are both residual-empty, so C3 lands as the TABLE + the gate wiring
(reasoning-strip merge-term keyed on `merges_consecutive_assistants`) with
profile-parameterized tests, NOT new transform mutations. The point is the
seam: when opencode-aisdk activates, the residual turns on by table entry, not
by new plumbing.

### C4. `tail_delta` rejection arm

`payload` stays implicit (the `messages` field IS the full payload). Oracle F2:
today an unknown `tail_delta` field would be silently IGNORED by serde (no
deny_unknown_fields), so a delta-shaped request would reach the transform with
a defaulted payload — strictly worse than unrejected. The raw wire struct
models `tail_delta: Option<serde_json::Value>` explicitly and rejects it before
binding/store access.

VEHICLE (Oracle F6, amends the earlier error-frame plan): `need_full_sync` is
a SUCCESS-SHAPED response `{status: "need_full_sync", served_from:
"transform"}`, NOT a subc Error frame. The consumer client contract
(subc-client-rs consumer docs) routes Error frames to fatal handling;
application-level flow control must be ordinary response bytes so the
consumer's NEED_FULL loop is a plain match on status, not an error-handler
special case. The CODE stays canonical per SUBC's ruling; only the vehicle
changes. Contract split: `unknown_serializer_profile` (a misconfig, fail loud)
STAYS an Error frame; `need_full_sync` (normal flow control) is a success
response. Success responses gain `status: "ok" | "need_full_sync"`.

## What this slice does NOT do

- No delta ingestion (Edge A ruled always-full baseline; #4 delta is an
  optimization for a consumer that doesn't exist yet).
- No daemon-side LKG cache (consumer-owned per spec §10; `served_from` reserves
  the vocabulary only).
- No plugin-leg activation (sequencing constraint, memory 7755). The
  opencode-aisdk/pi table entries ship inert behind profile selection.
- No change to `boundary_present` handling (stays module-computed; the
  fingerprint is EXPLICITLY not a cache anchor — C2 wording pins the
  de-conflation).

## Cache-safety notes

- `serializer_profile` appears in TWO places with distinct roles and the SAME
  value (wire-shape confirm with SUBC/LLMRUNNER): the top-level field is the
  operational healing-table key the module branches on each pass; the caller
  ALSO folds the profile id into the `render_config` identity string it
  composes (alongside system-hash/tool-set/model-key). The render_config copy
  makes a mid-session profile flip a HARD by construction, from day one — no
  deferred "remember to add the fold trigger at opencode-aisdk activation"
  hazard (the exact deferred-marker class note #412 warns about). Cost: a flip
  between two residual-empty profiles folds unnecessarily; accepted — a
  serializer change is a harness-grade event, rare and conservative-correct.
  The module cross-checks nothing between the two (render_config stays opaque);
  consistency is the caller's composition rule, stated in the spec.
- The fingerprint is echo-only: no module state keyed on it, no staleness
  decision made module-side in v1. It cannot influence bytes. Boundary pin for
  ALL future work: the module never RECOMPUTES a fingerprint (recompute =
  module owns the algorithm = defeats caller-owned). Even the future staleness
  check is store-and-compare of opaque strings (store last accepted, compare
  incoming), never recompute-over-held-canonical.
- All new fields are additive on the wire; defer passes with identical inputs
  remain byte-identical (existing golden vectors unaffected; V-suite gains a
  v2-wire vector variant only).

## Tests

1. v2 request round-trip: profile + fingerprint in, echo out on BOTH the
   normal path and the child-session (mc-historian:) pass-through path,
   `served_from` constant, `status: "ok"`.
2. Missing profile → same typed `unknown_serializer_profile` failure as an
   unrecognized one (hard-require; producer-first cutover makes the window
   moot).
3. Unknown profile → `unknown_serializer_profile` typed error, no store write.
4. `tail_delta` present → `need_full_sync` typed error, no store write.
5. Healing table: per-profile coverage values pinned against the
   field-inventory table (a table edit fails a test, not a review).
6. Residual gate: reasoning-strip merge-term fires only for profiles with
   `merges_consecutive_assistants` (parameterized; asserts owned/pi/CC do NOT
   strip, opencode-aisdk WOULD).
7. v2 parse matrix (Oracle F7 — the risk is parse/validation/error ROUTING,
   not only byte identity): success echo on hard AND defer; missing profile →
   typed error; unknown profile → typed error; tail_delta → need_full_sync
   SUCCESS response with no store write (assert row_version unchanged); legacy
   `items` shim still parses; fingerprint-absent success (no echo field, not
   null-echo).
8. Defer byte-identity with the new fields present (v2-wire defer replay).
9. Real-daemon: v2 transform request over the live wire with profile +
   fingerprint, echo verified.

</design>

## Tests (all required, from the design's matrix)

1. v2 round-trip: profile + fingerprint in -> echo out on normal AND
   child-session pass-through, served_from "transform", status "ok".
2. Missing profile -> Error frame "unknown_serializer_profile" (assert the
   TYPED code, not bad_request), no store write (row_version unchanged).
3. Unknown profile -> same, typed.
4. tail_delta present -> SUCCESS response status "need_full_sync",
   served_from "transform", fingerprint echoed, NO store write.
5. Healing table values pinned per profile (a table edit fails a test).
6. Residual gate parameterized: no profile strips reasoning today;
   opencode-aisdk's merges_consecutive_assistants is true in the table.
7. Legacy items shim still parses (with profile present).
8. Fingerprint-absent success: echo field OMITTED from the JSON (assert absent
   key, not null).
9. Defer byte-identity with v2 fields present (drive a HARD then a defer with
   identical v2 inputs; assert byte-identical ck_messages).
10. real_daemon.rs: one v2 vector over the live wire (profile + fingerprint,
    echo verified). Follow the existing harness patterns in that file.

## Gates (all must pass before you commit)

cargo test -p mc-module && cargo test -p mc-store && cargo test -p mc-core
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
The real_daemon integration test (it spawns a real subc daemon; see the file
header for how it resolves the daemon binary).

## Working rules

- Comments explain invariants and failure modes for a reader with no context;
  NEVER reference Oracle findings, SUBC rulings, design versions, or note
  numbers in code comments.
- Commit with a clear message when green. Do not push.
- If a design clause is impossible as written against the actual source,
  STOP and ask (background question) rather than improvising a workaround.
