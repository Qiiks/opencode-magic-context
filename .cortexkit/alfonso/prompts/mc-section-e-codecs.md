# Implement section-E harness-model codecs (mc-module)

You are implementing the harness-model codec half (spec §E) in the Rust
workspace (branch `subc-migration`; your worktree forks from it). The plan and
the authoritative spec sections are embedded below — the design is FINAL
(SUBC-ruled); implement exactly. The plan's mid rules and the
ExtractedBoundary input-not-anchor pin are load-bearing.

## Plan (authoritative)

# MC module: section-E harness-model codecs — implementation plan v2 (rulings folded)

SUBC rulings (pm_f27c3b39):
- MID IDENTITY IS BIDIRECTIONAL: stable-per-message across passes (same content
  ⇒ same mid, monotonic-once-assigned) AND never reused for different content.
- Pi mid: prefer responseId when present AT FIRST SIGHT; entries first seen
  without one pin the pi-ts-<timestamp> fallback FOREVER (a later
  streaming-settle that gains a responseId keeps the pinned fallback mid — the
  value is opaque, only stability matters). First-seen pin via sidecar.
  OpenCode mid = info.id (immutable; pin uniformly anyway).
- Boundary signal: typed field {messages, boundary: Option<ExtractedBoundary>}.
  ExtractedBoundary is a decoded FACT (the harness's own compaction marker), an
  INPUT — NOT a caller-supplied cache anchor; the module still owns the
  boundary-present decision. State this in the type's doc comment.

Implement codec spec §E (E.1 OpenCode MessageV2 ↔ CK, E.2 Pi AgentMessage ↔ CK)
as module-side Rust with goldens generated from CAPTURED harness arrays. This
unblocks the plugin leg's shadow-mode byte-compare later; it touches ZERO
plugin code (sequencing constraint: no packages/plugin|pi-plugin edits until
MITM + own-harness are proven end-to-end — dev-time fixture generation reading
harness data is explicitly allowed, same as the decay-curve/tokenizer goldens).

## Authority

docs/specs/codec.md §A/§B/§C/§E (E.4.1-E.4.3 all RESOLVED; no open design
points). CK#1 (docs/specs/ck-message.md) for the CK shapes. Spec #5 §8 pins
this module as the codec-boundary owner on the plugin leg.

## Shape

New crate module `crates/mc-module/src/codec/` (or a new `mc-codec` crate if
the surface warrants — decide at implementation by size; lean crate-internal
module first):

- `codec/opencode.rs` — E.1: `decode_opencode(&[MessageV2Json]) ->
  Vec<CkIngressMessage>` + `encode_opencode(&[CkWireMessage], &DecodeSidecar)
  -> Vec<MessageV2Json>`.
- `codec/pi.rs` — E.2: same pair over Pi session-entry JSON.
- `codec/sidecar.rs` — the harness-only bookkeeping (HarnessMeta) that decode
  strips and encode re-attaches for round-trip identity (ids, usage,
  timestamps, step-finish/snapshot/patch parts).

Both decode legs bind:
- A.4 canonical tool ids (OpenCode callID verbatim; Pi split-pipe `callId|itemId`
  → canonical callId with itemId to provider_extras; id-less → deterministic
  synthesis).
- E.1 wire-reachability table (ignored text → no CK; empty-text Text{""}
  FAITHFUL — the ""→" " substitution is the harness's, decoding to " " is the
  B.3 trap; step-start → Opaque immovable; compaction → EXTRACTED to the
  boundary signal, NOT CK content; subtask → Opaque).
- E.2 table (textSignature/thoughtSignature → provider_extras;
  redacted thinking → RedactedReasoning{data}; ToolResult.details →
  harness-only per E.4.3).
- E.3 origin decode (producing model → origin{provider, model, api}; codec
  NEVER bakes the cross-model strip).

## Round-trip contract (A.1, wire-projected)

Round-trip identity is at the WIRE-PROJECTED level for OpenCode (parts the
harness drops on conversion may decode to nothing). The golden therefore has
two assert layers:
1. decode→encode reproduces the wire-bound parts byte-identically (and
   harness-only parts via sidecar re-attach).
2. A wire-projection oracle: run OpenCode's own toModelMessagesEffect (via a
   Bun script against the installed SDK, like gen-golden.ts does for
   decay-curve) over BOTH the original array and the round-tripped array;
   assert identical projected wire JSON. Same for Pi via its serializer
   entry points. This is the definitive A.1 assert — it tests the contract
   as stated instead of a stricter raw-part identity the spec explicitly
   does not promise.

## Goldens (captured, not fabricated)

Generator scripts (Bun, committed under crates/mc-module/testdata/codec/):
- OpenCode: read 2-3 REAL session slices from opencode.db (a dense one with
  tool arcs + reasoning + compaction + subtask + files, a sparse one, and one
  with aborted/errored turns), sanitize (strip user content beyond structure
  via the existing redaction helpers where needed), emit
  `opencode-golden.json` with the raw parts + expected wire projection.
- Pi: same from a Pi JSONL session (the 019de471 session shape: responseId
  identity, split-pipe tool ids, thinking signatures, toolResult messages,
  custom_message entries, compaction entries, aborted assistants).
- LF-pinned via .gitattributes (the vendored-fixture discipline).

Rust tests consume the goldens; a generator-side non-vacuity guard asserts
every table row of E.1/E.2 appears at least once across the fixtures (a
fixture set that never exercises `ignored=true` or redacted thinking is a
vacuous golden).

## Deliberately out of scope

- The compaction-boundary EXTRACTION wiring into the connection layer (E.4.1
  says the signal feeds the boundary the connection layer tracks — that is
  plugin-leg integration, not codec work; the codec emits the extracted signal
  as a typed side-channel value and stops).
- Any plugin-side shadow-mode harness (later, at cutover).
- Encode-side same-model signature stripping (render/quirk-pass job per E.3).

## Settled (was: open questions)

Both ruled — see header. The sidecar mid-pin is belt-and-suspenders on Pi
(a not-yet-settled streaming entry is always the newest assistant, inside the
protected live tail, so coverage/reduction can never reach an entry still on a
fallback mid) but pinned unconditionally per the ruling.


## Codec spec sections 0/A/B/C/E (authoritative, embedded because gitignored)

## 0. The codec contract

A codec is a bidirectional pair for one edge:
```
decode(native_message) -> CkMessage          // native edge -> canonical
encode(CkMessage)      -> native_message      // canonical -> native edge (== project()+render() for a wire codec)
```
- **Harness codec** (`MessageV2`/`AgentMessage`): `decode` runs when MC reads the
  harness array into CK for the transform; `encode` runs when MC hands the
  transformed CK back to the harness, which then serializes to a provider wire.
  So a harness codec's `encode` output is re-serialized by the HARNESS, not by us
  (the §A.3 second-order arm).
- **Wire codec** (`WireFamily`): `decode` runs on the MITM leg (a captured provider
  request -> CK) and `encode` == llm-runner's existing `render()` on the owned/MITM
  re-send (CK -> provider wire bytes). So a wire codec's `encode` IS the byte-
  authoritative render.

The MC Transform sits BETWEEN `decode` and `encode` and is provider-neutral; all
edge knowledge lives in the codecs (§1 of CK#1).

## A. Shared conformance obligations (NORMATIVE - both halves bind these)

CK#1 hands the codec layer SIX load-bearing obligations. Stated ONCE here; §E and
§F each satisfy all six for their edges. A half MUST NOT restate or weaken them.

**A.1 Per-family losslessness (the convergence guarantee).** For any native
block/message `B` the codec decodes, `encode(decode(B))` MUST equal `B`
byte-faithful for the SAME edge (round-trip identity). `decode` MUST produce
`Opaque` for ANY block with no typed-core equivalent (CK#1 §5.13) and MUST NEVER
fail-loud on an unrecognized block. The core-classifier is a SMALL FIXED RECOGNIZER
("is this one of the 7 typed-core kinds? -> typed; ELSE -> `Opaque` by default");
the ELSE branch is unenumerated by design (CK#1 §5.13.5). There is NO shared block
enumeration across codecs - each owns its edge's open set. SCOPE: round-trip
identity is asserted over BYTE-AFFECTING data only (A.6); pure orchestration
bookkeeping is exempt (CK#1 §2.2).

**A.2 Arc derivation - Tool AND Approval (codec-derived, never raw-parsed).** For
any `Opaque` block that is half of a correlated group, `decode` MUST populate
`OpaqueArc { kind: Tool|Approval, id, role: Request|Response }` on BOTH halves
(CK#1 §5.13.3): tool arcs key on the server-tool use id / `*_tool_result`
`tool_use_id`; approval arcs key on `mcp_approval_request`/`response` `approvalId`.
`kind` MUST separate families so reclaim never cross-groups. The transform branches
on `arc`, NEVER on `raw`; arc derivation is therefore the codec's job and MUST be
deterministic + complete for the edge's arc shapes. Conformance: for every arc the
edge can emit, `decode` populates matching `arc` on both halves; a drop-one-half
test keeps or drops the whole arc.

**A.3 `Opaque.raw` byte-fidelity - per source (CK#1 §5.13.2).** A codec MUST capture
`raw` so the frozen-unit replays byte-identical:
- `OpaqueSource::Wire(family)` (wire codec): `raw` = the VERBATIM captured wire
  bytes; `encode` = emit-verbatim; byte-identity FREE.
- `OpaqueSource::Harness(id)` (harness codec): `raw` = a LOSSLESS serialization of
  the harness part; byte-identity rides the SECOND-ORDER CONTRACT (the harness
  serializer is deterministic over a decoded CK; a no-op transform yields
  byte-identical harness output) - the codec's obligation is the LOSSLESS-CAPTURE
  (so `decode(raw)` reproduces the exact part the harness re-serializes), NOT
  emit-verbatim. A codec MUST NOT re-serialize `raw` through a non-canonical path.

**A.4 Deterministic id-less synthesis (CK#1 §5.6.1 rule 2).** When a native message
lacks a tool-call id (Gemini optional ids), `decode` MUST synthesize the CK id as a
PURE function of stable inputs - `(message ordinal, part ordinal, tool name,
hash(input))` - NEVER a clock or counter, injective within the message. The same
id-less message MUST yield the same CK id across passes (defer-stability).
Target-native ALIASING on encode (canonical id invalid for target, CK#1 §5.6.1
rule 3) MUST be pure-deterministic + INJECTIVE (hash-carried, fail-loud on
collision), applied to both call+result.

**A.5 System-derive-on-render, never independent (CK#1 §2.1 / §5.11).** System
bytes are CK content (leading `Role::System` messages). A codec whose edge has a
separate top-level system field MUST DERIVE it from those leading System messages on
`encode`, and MUST decode a native top-level system into leading System messages.
It MUST NOT both keep a System message AND emit an independent system field
(double-count), nor drop the System message (loss).

**A.6 Byte-affecting-vs-bookkeeping projection line (CK#1 §2.2).** For every native
field, the codec MUST classify: a field a PROVIDER converter/renderer reads
(byte-affecting) rides `provider_extras` on its typed block and is IN `project()`;
a field only the agent loop / harness bookkeeping reads is HARNESS-ONLY (carried in
`HarnessMeta` if a harness round-trip needs it, stripped by `project()`, NOT a
wire-losslessness obligation). Round-trip conformance (A.1) asserts WIRE bytes, not
bookkeeping fields.

## B. Cross-cutting determinism + byte-stability (NORMATIVE)

These bind the codec to the cache-core frozen-set (SUBC's layer; the reviewer
confirms each against the cache-core contract):

**B.1 `decode` is a pure function of the native message.** No clock, RNG, counter,
or cross-call mutable state in `decode` (A.4 is the specific instance). Same native
input -> same `CkMessage` every call. A codec MUST NOT read ambient state that can
change between passes.

**B.2 `encode` is a pure function of `(CkMessage, FrozenRenderConfig)` - a CLOSED
set.** The `FrozenRenderConfig` is the ONLY non-CK input, and it is a CLOSED
ENUMERATED set (NOT "and whatever else" - any byte-affecting encode input outside
this set is an unfrozen bust input by definition). It is exactly:
`{ target wire family, model / wire_model_id, resolved tool set, tool_choice,
generation params, response_format, cache-policy / breakpoint config,
the frozen reasoning positional bits (is_last_assistant_turn / merge-group
membership, CK#1 §6.1.1), the target-native alias map basis (CK#1 §5.6.1 r3),
provider_options, serializer_profile_id }`. NOTE: system bytes are NOT in this set -
system is CK CONTENT (leading `Role::System` messages, CK#1 §2.1/§5.11), caught by
the content anchor; listing it here would double-represent it (category error).
`serializer_profile_id` IS in the set - the quirk seam (clear-shape, segmentation
guard, residual, CK#1 §6) is parameterized by it = a byte-affecting non-CK render
input. `encode` MUST read NOTHING byte-affecting outside this set. The
set is FROZEN within a cache epoch (a model/provider switch is a HARD bust); given
it, `encode(ck)` is byte-deterministic across defer passes - the reason the MC
Transform's frozen RENDER UNITS replay byte-identical. This is the cache-core's
`FrozenRenderConfig` contract (llm-runner freezes exactly this at run-start; the
codec binds it). Making the set CLOSED is what makes B.3 CHECKABLE: you cannot audit
"no new input" against an open "etc."

**B.3 The codec introduces NO new bust input - the FOUR concrete classes.**
Decoding/encoding MUST NOT surface a byte-affecting datum the transform did not
already freeze. Anything byte-affecting the renderer cannot re-derive deterministically
MUST be carried frozen (in `provider_extras` or a frozen render param), never
recomputed. The four concrete ways a codec injects an unfrozen bust - each MUST be
frozen-or-excluded:
- **(a) Nonces / timestamps** in any field the renderer emits (the identity-lead
  class, §8): MUST be frozen or stripped, never live-recomputed on a defer pass.
- **(b) Non-deterministic iteration order.** `provider_extras` is recursive-BTreeMap
  canonical (CK#1 §5.9). ANY OTHER map the codec serializes - tool-call input JSON
  object keys, nested `Opaque`-summary structures, nested `ResultBlock` ordering -
  MUST be canonical-ordered too; otherwise two logically-equal requests diverge on
  key order = a silent bust.
- **(c) The target-native alias map (CK#1 §5.6.1 r3)** is per-request-DERIVED, so it
  is a "looks-stateful-but-must-be-pure" trap: it MUST be a pure fn of
  `(canonical_id, frozen target family)` (already mandated in A.4), never a stateful
  counter/registry. B.3 cross-refs A.4 explicitly so the audit catches it here too.
- **(d) Id-less tool-call id synthesis (CK#1 §5.6.1 r2, A.4)** is the same
  looks-stateful-but-must-be-pure trap on the DECODE side: a wire/harness tool call
  with no native id (Gemini optional ids) MUST synthesize the CK id as a pure fn of
  `(message ordinal, part ordinal, tool name, hash(input))`, NEVER a clock/counter
  (the shipped Pi `google.ts` `Date.now()` pattern is exactly this bug - a different
  id every pass = a silent bust + broken pairing). B.3 names it as the 4th class so
  the audit surface matches A.4/F.6.

**B.4 Round-trip + idempotence test surface (shared).** Each half SHALL ship: (a)
A.1 round-trip identity over a corpus of real edge messages incl. every typed-core
kind + representative `Opaque` blocks + arcs; (b) a CROSS-PASS defer-stability test
(decode the same native twice -> identical CK; encode the same CK twice under frozen
config -> identical bytes); (c) the keystone test (CK#1 §2) at request granularity
for that edge; (d) a CROSS-EPISODE defer-stability test - re-decode + re-encode after
a simulated episode boundary (new run, `FrozenRenderConfig` reproduced from durable
WAL state) yields byte-identical output. (d) is the codec-side gate for the
lineage-cumulative-vs-per-episode distinction: without it a codec can pass every
within-run test yet bust at the episode boundary (the class that bit the
identity-lead). It binds the durable frozen reasoning-clear watermark + m0/m1
boundary surviving run boundaries (cache-policy core).

**B.5 The SOFT+ cache anchor is BOUNDARY-PRESENCE, NOT a covered-prefix fingerprint
(NORMATIVE - pins a conflation trap).** The SOFT+ (defer) replay validity decision is
NOT a content hash over the covered prefix; it is BOUNDARY-PRESENCE + FROZEN-BYTE
REPLACEMENT. Source (MC shipped `inject-compartments.ts:258-292`): on a defer pass
the cached frozen m0/m1 injection is replayed verbatim AND the covered prefix is
spliced out by BOUNDARY-ID MATCH ONLY (`findIndex(info.id === compartmentEndMessageId)`);
the covered region is REPLACED by the frozen `<session-history>` bytes, so an in-prefix
content edit is summarized-away (intentional lossiness) - there is NO collision
surface because there is no fingerprint on the covered region. A revert that REMOVES
the boundary id reconciles on the next cache-busting pass (m0 rematerializes against
the live array); boundary-presence decides replay, boundary-removal reconciles on next
bust. The COVERAGE descriptor in core state = the boundary id + the frozen payload
(both already persisted), so a CAS-retry re-splices the same boundary against the same
bytes. DISTINCT MECHANISM (do not conflate): `computeRawRangeFingerprint`
(`read-session-true-raw-tokens.ts:650-672`) is the HISTORIAN IN-FLIGHT SNAPSHOT
validator (it checks the runner's raw chunk matches the trigger's fire-decision view -
a cross-view staleness guard between two opencode.db reads); it is deliberately
length-/id-/ordinal-based + content-stable so runtime-field drift does not
false-invalidate the snapshot, and its same-length-edit residual is a HISTORIAN
content-quality edge (a slightly-stale chunk summary), NEVER a stale cache. The cache
anchor and the historian-snapshot fingerprint are different paths, different code,
different failure modes.

## C. Harness-resolution principle (NORMATIVE - resolves harness fields within CK#1, no CK change)

Harness-specific data does NOT need a new CK field. Three disjoint cases, each
resolved within the blessed CK#1 shape:
1. **Harness RENDER-CONDITION fields** (a flag that makes the harness include/skip
   wire content, e.g. OpenCode `ignored` on a text part - the converter skips
   ignored text, `message-v2.ts:208`) are resolved AT DECODE: the harness codec
   applies the condition and emits only wire-bound CK content (ignored text -> no CK
   content / a dropped unit). CK only ever holds wire-bound content; the condition
   never reaches the transform.
2. **Harness-NAMED provider data** (a field the harness names locally but that is
   PROVIDER data on the wire, e.g. Pi `textSignature` setting the OpenAI-Responses
   item id+phase) maps to `provider_extras` (provider-keyed), NOT a harness field.
3. **Harness-specific PARTS** with no typed-core equivalent (OpenCode `step-start`
   separator, `compaction-part`, `subtask`) -> `Opaque{source: Harness}`; their
   byte-affecting render is the harness serializer's job on re-encode (the A.3
   Harness second-order arm).
This is why CK#1 §3 + §5.13.4 are FINAL: the codec layer absorbs harness specificity
without changing the canonical. (Pure bookkeeping - response ids, usage, timestamps
- is HARNESS-ONLY per A.6, in `HarnessMeta` or dropped.)

## E. Harness-model codecs (MC-owned)

Two harness codecs, each binding §A.1-A.6 + §B + §C. Decode runs when MC reads the
harness array into CK; encode hands transformed CK back to the harness (which then
runs ITS serializer to the provider wire - the §A.3 Harness second-order arm, so a
harness codec's job is LOSSLESS capture, not wire-byte authority). Grounded at source
(OpenCode `message-v2.ts` `toModelMessagesEffect` + `packages/sdk/.../types.gen.ts`
Part union; Pi `types.ts` AgentMessage + `providers/`), 2026-06-28.

### E.1 OpenCode `MessageV2` <-> CK

**CRITICAL framing (source-confirmed, `prompt.ts:1357` vs `:1364`).** The
`experimental.chat.messages.transform` hook receives RAW `MessageV2`
(`{ info: Message, parts: Part[] }[]`, plugin SDK `index.ts:282-289`), mutates it,
and hands it back; `toModelMessagesEffect` (the part->wire conversion) runs AFTER, on
the HARNESS side. So the MC harness codec decode/encode operates on RAW MessageV2
parts, NOT model-messages, and the codec MUST NOT pre-apply any harness conversion
step - those are the harness serializer's deterministic job on the encoded output
(the §A.3 Harness second-order arm). Two consequences:
- **Codec round-trip identity (A.1) is at the WIRE-PROJECTED level**, not the raw
  part level: parts the harness will drop anyway (ignored text, non-wire-reachable
  parts) MAY decode to no CK content, because the harness re-conversion of the
  encoded array yields the same wire. encode reproduces the wire-bound parts; the
  harness re-derives the rest deterministically.
- **The codec MUST NOT bake a CONTEXT-DEPENDENT harness substitution into a CK
  literal** (the B.3 trap): the harness emits the empty-separator as `" "` ONLY
  `when hasSignedReasoning`, else the raw `""` (`message-v2.ts:281`). A codec that
  decoded a bare `text:""` to `Text{" "}` would inject `" "` even where the harness
  would keep `""` = an unfrozen byte divergence. So decode the RAW value faithfully
  and leave the context-dependent substitution to the harness.

Wire-reachability (from `toModelMessagesEffect`): only `text`, `reasoning`,
`step-start`, `tool`, `file` produce wire content; `step-finish` / `snapshot` /
`patch` / `agent` / `retry` are NOT pushed = harness-only by A.6.

| MessageV2 part | CK kind | rule / binding |
|---|---|---|
| `text` (`ignored=false`, non-empty) | `Text{text}` | `metadata` (signatures) -> `provider_extras`; STRIPPED on cross-model by the harness/render per origin (§E.3), NOT by the codec |
| `text` with `ignored=true` | (none) | §C case 1 render-condition: harness skips ignored text on conversion -> decode emits NO CK content; wire-level A.1 holds |
| `text` with `text===""` (empty separator) | `Text{""}` (FAITHFUL raw) | decode the RAW empty verbatim; the context-dependent `""->" "` substitution (`message-v2.ts:281`, only `when hasSignedReasoning`) is the HARNESS's deterministic job on re-serialize (A.3), NOT the codec's. Decoding to `Text{" "}` is FORBIDDEN (B.3 unfrozen-byte trap) |
| `reasoning` | `Reasoning{text, signature}` | signature in `metadata`; cross-model strip is render-side (origin, §E.3). Empty-text + redacted blob -> `RedactedReasoning` |
| `tool` (status `completed`/`error`) | `ToolCall` + `ToolResult` | `callID` -> canonical id (A.4); `input`->call.input; `output`(+`attachments`) -> `ToolResultOutput`; `metadata.providerExecuted` -> `provider_executed` (A.6/§5.6.3); `time.compacted` "[Old tool result content cleared]" is an OpenCode-native compaction artifact MC produces -> decode faithfully as the cleared-output content |
| `file` | `Media` | `mime`->`media_type` RAW; data/url -> tagged `MediaSource` (DataBytes/DataBase64/Url); non-media file (text/plain, directory) -> the harness turns it into `[Attached ...]` text on conversion (§C case 1: codec leaves it; harness derives the text) |
| `step-start` | `Opaque{Harness:"opencode", kind:"step-start"}` | §C case 3: wire-reachable byte-affecting separator; raw = lossless capture (A.3 Harness arm); IMMOVABLE (§5.13.4) |
| `compaction` | **CACHE-CORE BOUNDARY SIGNAL (E.4.1, RESOLVED)** | NOT a CK ContentKind, NOT Opaque, NOT HarnessMeta: extracted to the cache-core compaction-boundary the connection layer (spec #4) already tracks; the transform/cache-core reads it to place m0/m1 + advance the watermark; it is REPLACED by the m0/m1 synthesis on render |
| `subtask` | `Opaque{Harness:"opencode", kind:"subtask"}` | lossless capture; the harness derives its "The following tool was executed by the user" text on conversion. Transform never needs to distinguish it (E.4.2) |
| `step-finish`, `snapshot`, `patch`, `agent`, `retry` | `HarnessMeta` / dropped | NOT wire-reachable = harness-only bookkeeping (A.6); wire-level A.1 unaffected if dropped |
| message ids / session id / model meta / tokens / errors | `HarnessMeta` / `origin` | bookkeeping -> harness-only; the producing model -> `origin` (§E.3) |

### E.2 Pi `AgentMessage` / `UserMessage` <-> CK

| Pi content | CK kind | rule / binding |
|---|---|---|
| `TextContent` (+`textSignature`) | `Text` | `textSignature` is byte-affecting (sets OpenAI-Responses item id+phase, `openai-responses-shared.ts:178-198`) -> `provider_extras` (§C case 2), NOT a harness field |
| `ThinkingContent` (+`thinkingSignature`, `redacted=false`) | `Reasoning{text, signature}` | `thinkingSignature` -> `signature` (typed-neutral); cross-model strips (origin) |
| `ThinkingContent` (`redacted=true`) | `RedactedReasoning{data}` | `data` = the opaque encrypted payload in `thinkingSignature`; NEVER emptied (§5.5) |
| `ImageContent` | `Media{Image}` | `data`(base64) -> `DataBase64`; `mimeType` -> `media_type` RAW |
| `ToolCall` (+`thoughtSignature`) | `ToolCall` | `arguments`->`input`; `thoughtSignature` (Google opaque thought sig) -> `provider_extras` (§C case 2); id-less -> A.4 deterministic synthesis |
| tool result content | `ToolResult` | `ToolResult.details` (no typed CK home, byte-affecting?) -> `provider_extras` if a converter reads it, else harness-only (A.6 classification per field) |
| `AssistantMessage.{api, provider, model}` | `MessageOrigin` | the producing model -> `origin` (§E.3); `model` is the native key, NOT `responseModel` (§5.10.1) |
| `responseModel`, `responseId`, `usage`, `stopReason`, `timestamp` | `HarnessMeta` / dropped | response bookkeeping = harness-only (A.6); NOT a wire-losslessness obligation |

### E.3 Origin + cross-model (binds §5.10.1)

Both harnesses already branch rendered bytes on same-model: OpenCode strips
`providerMetadata`/signatures when `differentModel` (`message-v2.ts:285,324`); Pi's
serializer strips cross-model tool/thinking signatures. The codec DECODES the
producing model into `origin {provider, model, api}` (Pi carries it on
`AssistantMessage`; OpenCode from the message's model metadata) and the ENCODE-side
same-model branch (origin vs `FrozenRenderConfig.model`) is what the downstream quirk
pass applies - the codec does NOT bake the strip into CK (CK keeps the signature in
`provider_extras`; the render strips per origin). This is a B.2 frozen input
(`model` is in the closed set).

### E.4 Design points

- **E.4.1 `compaction` part decode target - RESOLVED (SUBC, the compaction-marker
  owner).** A MessageV2 `compaction` part / Pi compaction entry decodes to NEITHER a
  CK `ContentKind`, NOR `Opaque`, NOR `HarnessMeta`: it is EXTRACTED to the
  CACHE-CORE COMPACTION-BOUNDARY SIGNAL the connection layer (spec #4) already tracks
  (the `filterCompacted` boundary on the plugin leg / the in-memory virtual boundary
  on MITM). Rationale: the boundary is LOAD-BEARING cache-core state (the m0/m1
  boundary, a LINEAGE-durable frozen unit) the transform MUST interpret to place
  m0/m1 + advance the watermark - so it cannot be `Opaque` (never-interpreted by
  contract). It is NOT wire content (it is REPLACED by the m0/m1 synthesis on
  render). So the harness compaction part FEEDS the existing boundary mechanism;
  it does not become CK content. Spec #4 (§9 compaction-marker edge) states the
  extraction; §E references it.
- **E.4.2 `subtask` - RESOLVED.** `Opaque{Harness:"opencode", kind:"subtask"}`
  (lossless + A.1 round-trip; the harness derives its user-text on conversion). The
  transform never needs to distinguish a subtask user-turn from a real one today, so
  Opaque is correct (no information the transform branches on).
- **E.4.3 Pi `ToolResult.details` classification - RESOLVED (source-verified).**
  HARNESS-ONLY. Source check (`~/Work/OSS/pi-mono/packages/ai/src/providers/`,
  2026-06-28): ZERO provider serializers read `.details` when building wire content
  (`ToolResultMessage<TDetails>.details?` exists on the type, `types.ts:314-319`, but
  no anthropic/openai/google serializer references it). Not byte-affecting -> A.6
  harness-only (carried in `HarnessMeta` if a harness round-trip needs it, else
  dropped; never a wire-losslessness obligation).



## CK shapes

The CK wire types live in crates/mc-store/src/ck_wire.rs (CkWireMessage,
CkWireBlock, CkKind, MediaSource, etc.) re-exported through mc-module. Use
them; do NOT invent parallel types. CkIngressMessage is in
crates/mc-module/src/transform.rs.

## Fixture generation

Bun generator scripts go in crates/mc-module/testdata/codec/ (see
crates/mc-core/testdata/ and the mc-tokenizer golden generator for the
established pattern: a committed .ts generator + a committed .json fixture +
.gitattributes LF pinning). Read REAL data:
- OpenCode: ~/.local/share/opencode/opencode.db (read-only!), pick 2-3 session
  slices covering: tool arcs (completed + error + running), reasoning parts
  with signatures, ignored text, empty-text separators, file parts, a
  compaction part, a subtask part, step-start/step-finish.
- Pi: ~/.pi/agent/sessions/**/*.jsonl, covering: split-pipe tool ids
  (callId|itemId), thinking with signatures, redacted thinking if findable,
  toolResult messages, custom_message entries, compaction entries, an aborted
  assistant (stopReason aborted, empty content), entries with and without
  responseId.
Sanitize user text (replace long text runs with length-preserving placeholder
text — structure matters, content doesn't; keep short strings verbatim only
when they are structural like '[dropped 5]').
The wire-projection oracle: for OpenCode run the installed SDK's
toModelMessagesEffect equivalent if invokable from a Bun script against
packages/plugin's node_modules; if that proves impractical inside the
generator, document why and fall back to asserting decode->encode raw-part
identity for wire-reachable parts + sidecar re-attach for the rest (the
weaker assert), with a TODO marker naming the projection oracle as follow-up.
Do NOT fabricate expected values by hand — every expected byte comes from
running real code over real data. Non-vacuity guard: the generator asserts
every E.1/E.2 table row class appears >=1 time across fixtures and FAILS
loudly listing missing classes if not (then pick richer sessions).

## Gates

cargo test -p mc-module (+ mc-core, mc-store --features test-support), clippy
--workspace --all-targets -- -D warnings, fmt --check, check_comments. Commit
when green; do not push. Comments explain invariants for context-free readers;
never reference spec section numbers alone without stating the rule, never
reference SUBC/Oracle/plan versions.

## If blocked

If a spec clause contradicts the actual source shapes you find in the
fixtures, STOP and ask (background question) with the concrete example rather
than improvising.
