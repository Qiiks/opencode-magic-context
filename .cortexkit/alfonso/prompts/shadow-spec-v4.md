# MC Shadow Transform — spec v4

(v1 → v2: folds all 10 findings from Oracle round 1 (bg_f05038e0, BLOCK): namespaced
shadow session, flat-block-id/ordinal wire contract, third boundary state, fenced
state_sync, dedicated dispatch arms, sender-side denormalization, quarantine.
v2 → v3: folds Oracle round 2 (bg_5892744e, BLOCK): ordinal source of truth pinned to
canonical OpenCode raw order, DeclaredTrimValidated formalized as four concrete
predicates, ACK-based state_sync scheduling, shadow_generation on reset and all ops,
in-memory-only trigger eval as a hard tested requirement.
v3 → v4: folds Oracle round 3 (bg_ea029dd5, REVISE-narrow): cold-start rule,
transactional shadow_reset lifecycle, generation-scoped ordinal memoization, cached
resolver requirement, continuity predicate vs covered-system re-emit.)

Goal: a dev-flagged parallel lane where the OpenCode plugin, after running its own TS
transform, fire-and-forgets the same pass to the MC Rust module (via subc) and compares
the two outputs at the CK level. Run for ~a week on the dogfood session to mature the
Rust transform against live traffic before any cutover. Zero latency or behavior impact
on the real lane.

## Architecture

```
messages.transform (TS, unchanged, returns real output)
        │
        ├── real lane: return TS output to OpenCode
        └── shadow sender (flag-gated, one-in-flight per session, strict FIFO)
              → subc route bound as BindIdentity.session = "shadow:<real_sid>"
                 ops, in order per pass:
                   [state_sync]        (only when TS-side watermarks advanced)
                   shadow_transform    (dedicated dispatch arm, NOT plain transform)
              ← divergence report (persisted module-side + summary to plugin log)
```

## Wire contract (the part v1 got wrong)

**Shadow session identity (F1).** The route binds `shadow:<real_sid>`. All durable rows
(cache state, meta, compartments, mirror rows) live under the shadow key. PIN: shadow
rows are NEVER promoted to the real session id at cutover — cutover starts a fresh
lineage; a `shadow:` prefix check in the module rejects any non-shadow op arriving on a
shadow binding and vice versa.

**shadow_transform request** carries everything the TS pass actually saw (F3, F7):
- `input`: raw hook array (OpenCode shapes), each message annotated with
  `absolute_ordinal`. ORDINAL SOURCE OF TRUTH (round-2 blocker #1): the ordinal is the
  message's position in the canonical raw session order — `time_created ASC, id ASC`,
  summary rows excluded, matching `read-session-raw.ts` — resolved through the plugin's
  scoped raw-message cache (the same ordinal universe `ctx_expand` uses), NEVER from
  hook-array position (a post-trim window renumbers positions). Rules: (a) if any hook
  message's ordinal cannot be resolved, SKIP the shadow pass for that message array
  (counted, logged — never guessed); (b) ordinals for a given session are
  monotone-append: a message id must never re-resolve to a different ordinal; the
  plugin memoizes id→ordinal per session and any re-resolution mismatch (revert/prune
  reshuffle, index rebuild) triggers `shadow_reset` rather than sending renumbered
  coordinates. The module adopts supplied absolute ordinals as the durable coordinate
  (sparse contract already permits gaps); the codec's positional `index+1` ordinal is
  not used in shadow.
- `ts_output`: the TS transform's output array, sender-side denormalized (below).
- `pass_inputs`: `{ now_ms (TS pass-local Date.now snapshot), model_key, usage
  {input_tokens, limit}, effective_execute_threshold, cache_ttl, provider_error? }` —
  the module uses these verbatim; no receipt-time clock, no bind-time model.
- `ts_decision`: `{ class (defer/soft/hard as TS classified it), marker_state
  { marker_message_id?, advanced_this_pass } }` for report attribution.
- `declared_trim` (D1): `{ flat_boundary_id, boundary_bare_message_id,
  boundary_absolute_ordinal, next_absolute_ordinal }` — present whenever the TS lane's
  compaction marker has ever advanced (OpenCode pre-trims the hook array at the marker).
  The flat_boundary_id is computed by the plugin from the TS compartment row's end
  message + the codec's flattening rules and persisted alongside the sync bookkeeping —
  never re-derived per pass from bare marker/user message ids.
- `shadow_generation`: monotonic per-session reset epoch (below), on EVERY op.

**state_sync request** (F2, F5): delta-mirror keyed on TS watermarks, translated to the
module's identity vocabulary BEFORE the wire:
- compartments: TS rows with `start/end` carried BOTH as flat CK block ids (translated
  by the plugin: `<message_id>#<block_index>` per the codec's flattening rules) AND bare
  OpenCode message ids + absolute ordinals (diagnostics/trim validation).
- memory rows + mutation-log deltas, `last_todo_state`.
- `expected_shadow_seq`: monotonic per-session sync sequence.
Applied module-side as ONE fenced transaction in the `publish_historian_chunk` mold:
row writes + `mc_cache_state.row_version` bump + seq CAS (`expected_shadow_seq` mismatch
= typed reject, plugin re-syncs from scratch). Never bare
`replace_compartments`/`append_compartments`.

## Module-side design

**Dedicated dispatch arms (F6).** `state_sync` and `shadow_transform` are new explicit
arms in the dispatch (below method/kind precedence, alongside the facade arm). A plain
`transform` arriving on a shadow binding is rejected (typed). The shadow arm:
- runs the full classifier/compose/build_output pipeline (that IS the thing under test),
- historian: trigger evaluated IN-MEMORY ONLY for the report (fire/no-fire + boundary
  snapshot). HARD requirement (round-2 #5): the shadow arm must NOT call
  `prepare_historian_fire` (which persists no-fire diagnostics via `record_no_fire` and
  can spawn producers) — it calls the pure trigger evaluation directly; a test asserts
  a shadow pass leaves `last_no_fire`/`last_failure`/scheduler observation state
  untouched and spawns nothing,
- commits pass state under the shadow key normally (CAS discipline unchanged).

**BoundaryState three-way (F4).** `boundary_present: bool` becomes
`BoundaryState::{LivePresent, DeclaredTrimValidated, Absent}`:
- `LivePresent`: anchor flat block id found in the live array (today's check, unchanged
  for non-shadow profiles).
- `DeclaredTrimValidated` (round-2 blocker #2 — formalized as FOUR predicates, ALL
  required):
    1. `declared.flat_boundary_id == core.boundary_id` (durable boundary identity),
    2. `declared.boundary_absolute_ordinal == meta.coverage_ordinal` (durable coverage
       agreement),
    3. the durable tail compartment's `end_message_id` matches BOTH the declared flat
       id and the declared ordinal (store row agreement),
    4. the first NON-SYNTHETIC, NON-SYSTEM live message's absolute ordinal equals
       `declared.next_absolute_ordinal` (continuity — nothing unsummarized was
       trimmed). System-role rows are exempt from the continuity check because the
       module re-emits covered system messages at head by design (the system-trim
       exemption), and synthetic slots are module-owned.
  Treated as present for classification; mint-absent accepts a minted boundary equal to
  the declared trim id only under this validated state. Any predicate failing =
  `Absent` + `trim-mismatch` divergence — the fail-loud arms are never suppressed by a
  partially-valid declaration.
- `Absent`: neither — the existing fail-loud arms (Layer-2 pending_rewrite, #423
  re-cut) fire EXACTLY as today. Declared-trim that FAILS validation is `Absent` plus a
  `trim-mismatch` divergence record, never silent adoption.
Targeted tests required: declared-trim × {pending_rewrite arm, #423 re-cut, mint-absent
guard} interaction matrix — the bypass hazards the Oracle flagged.

**Synthetic todo (F8).** Known structural divergence: TS injects a synthetic tool part
into the anchor assistant message; the module emits an assistant+tool CK message pair.
Auto-classified `synthetic-todo` expected-divergent class in shadow (compare skips those
blocks, presence/content compared, shape not). Shape alignment (module OpenCode-profile
injecting into the anchor assistant message) is a SEPARATE work item — it is a real
plugin-leg deliverable, not a shadow hack; do not block shadow on it.

**Compare basis (F9).** Structural CK canonical JSON: sorted keys, canonical number
formatting, block-by-block diff — never ad-hoc byte strings. The codec's dropped part
types are listed in the report metadata so "identical" is honest about what it covers.

## Sender-side (plugin)

**Denormalization at the source, not regex at the sink (F9).** The plugin OWNS tagger
state and its own mutations, so it strips what it knows it applied before sending
`ts_output`: §N§ prefixes removed via the tagger's exact per-part map (never regex over
content), `<ctx-search-hint>` blocks removed by exact block identity. Every strip is
listed in the request (`normalizations: [...]`) and echoed in the report. Caveman: OFF
in the dogfood session (not normalizable).

**Ordering + ACK-based sync (F5, F7, round-2 #3).** One in-flight op per session,
strict FIFO (state_sync → shadow_transform per pass). The sender tracks
`last_acked_shadow_watermarks` (per watermark family) + `last_acked_shadow_seq` from
module ACKs — sync scheduling keys on ACKED state, not on TS-side advancement: if a
sync was dropped/rejected, the sender re-sends the full outstanding delta before the
next shadow_transform even when TS watermarks did not advance again. Any CAS/seq
reject = NO shadow_transform for the session until a resync succeeds (transforms
against a stale mirror are cascade noise, not signal). In-flight cap 4 passes/session,
drop-oldest with counter.

**Reset generation (round-2 #4, round-3 #2).** `shadow_generation` (reset epoch) is
persisted module-side in shadow meta and carried on every op. `shadow_reset` rides the
same per-session FIFO and is ONE fenced transaction: read current generation → wipe +
recreate shadow rows → `generation += 1` → `shadow_seq = 0` → clear quarantine; the ACK
returns the new generation, which the sender adopts before sending anything else. Any
in-flight or late op carrying a stale generation is rejected (typed, counted) — a stale
op can never recreate wiped state (store `load` returns default state on absent rows,
so an un-generationed late commit would silently resurrect a lineage).

**Cold start (round-3 #1).** Plugin-side ACK/seq/generation bookkeeping is in-memory
only. After plugin restart (or route close/reopen) that state is UNKNOWN: the sender
must not send `shadow_transform` on trust — it issues `shadow_reset`, waits for the
ACK's generation, then full `state_sync`, then resumes transforms. Restart cost is one
re-sync, never a corrupt compare.

**Ordinal memoization lifecycle (round-3 #3).** The id→ordinal memo is scoped to
`{real_sid, shadow_generation}` and cleared on reset, cold start, route close, and
re-resolution mismatch (which itself triggers reset). Resolution MUST go through
`readRawSessionMessages()` inside the scoped raw-message cache (`readRawSessionMessageById()`
bypasses the active cache and must not be used for ordinal derivation).

**Quarantine (F10).** Reports cannot attribute which lane is wrong, and a committed
divergent shadow state makes every later pass cascade noise. On first hard divergence
(byte/decision mismatch not in an expected class): mark the shadow session QUARANTINED
module-side; subsequent passes are recorded (decision-only, cheap) but not
byte-compared; the report for the quarantining pass carries the full replay payload
(input + pass_inputs + declared_trim + both outputs). A `shadow_reset` management op
wipes shadow rows for the session and re-syncs from scratch to resume byte-compare.
Divergence rows: pass seq, class, first-diverging (mid, block, field), bounded byte
prefixes, normalization list, TS decision, RS decision + state hash.

**Trust + config (D6, unchanged).** `shadow_transform: { enabled: false }` user-tier
ONLY (stripped in project-security.ts). No socket knob; standard subc discovery.

## Phases

- **P0 — ingress proof**: sender + decode-only op; proves hook shapes + absolute-ordinal
  annotation through the codec; surfaces shape gaps as typed errors.
- **P1 — defer-pass compare**: sessions/passes before any fold (no declared_trim, no
  state_sync); replay-path compare + sender denormalization.
- **P2 — the meat**: state_sync (fenced), DeclaredTrimValidated arm + interaction test
  matrix, SOFT/HARD compose compare, trigger-decision compare, quarantine + reset.
- **P3 — reductions mirror** (tag→flat-block translation for agent drops) + ratchet +
  week-long soak with daily per-class convergence report.

## Required test matrix (accumulated from both rounds)
- declared_trim × {pending_rewrite arm, #423 re-cut, mint-absent guard} interactions,
  including each of the four predicates failing individually.
- marker-advance/state_sync skew: TS marker advances at pass N, sync rejected/dropped,
  pass N+1 arrives with trimmed array + stale mirror → must be blocked by the ACK gate
  (no transform), not misclassified.
- shadow_reset during in-flight FIFO: stale-generation op after wipe is rejected and
  cannot recreate state.
- shadow pass leaves historian/scheduler durable state untouched (spawn count 0,
  no_fire/last_failure unwritten).
- plugin ordinal derivation: post-trim window resolves original ordinals; revert/prune
  re-resolution mismatch triggers shadow_reset; unresolvable ordinal skips the pass.
- quarantine: first hard divergence quarantines; decision-only recording continues;
  shadow_reset resumes byte-compare.
- plugin restart with unknown ACK state: reset-then-sync-then-resume, no trust-based
  transform; generation from the reset ACK adopted.
- covered-system head re-emit does not fail DeclaredTrimValidated continuity.
