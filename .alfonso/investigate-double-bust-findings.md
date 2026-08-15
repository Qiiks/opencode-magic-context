# Post-fold double-bust investigation

## Conclusion

The second bust is caused by a late `step-finish` structural part on a completed OpenCode assistant, not by marker relocation, M0/M1 rematerialization, todo anchoring, or a reasoning-strip decision.

On the assistant's first continuation request, the transform input can be a snapshot taken before `step-finish` is visible. On a later request, the same persisted assistant includes `step-finish`. Magic Context's structural cleanup replaces that part with `{type:"text",text:""}`; OpenCode's Anthropic serialization normalizes the empty trailing sentinel to a single-space text block. The provider body therefore changes from:

```text
[text(" "), thinking, tool_use]
```

to:

```text
[text(" "), thinking, tool_use, text(" ")]
```

at an old assistant inside the cached prefix. The thinking and tool-use blocks remain byte-identical. This is the exact fleet signature in the retained wire-dump window.

The fix removes trailing whitespace-only text blocks statelessly from non-newest assistants after final structural/reasoning cleanup. It deliberately preserves leading whitespace and the newest assistant.

## Sessions and clocks

Dashboard times are local time (UTC+2); auth-dump filenames and plugin logs are UTC.

| Session | ID | Evidence |
| --- | --- | --- |
| CKCRED (`/Work/Projects/CortexKit/claustrum`, title `CKCRED`) | `ses_100a028aaffeVG0zdK3qwcEXf8` | Original 12:37 local double plus later retained wire dumps |
| SUBC (`/Work/Projects/CortexKit/subconscious`) | `ses_12a4fa38dffe81Fz7Y2AsWb5Cg` | Original 15:16 local double plus exact 14:24-14:26Z wire dumps |

## Exact wire proof: SUBC

`bun packages/plugin/scripts/analyze-cache-busts.ts ses_12a4fa38dffe --all-rows --show-diff` reports two busts in eight retained requests:

- `14:24:42Z`: first divergence `message[575](assistant)`.
- `14:25:44Z`: first divergence `message[583](assistant)`.

### `message[575]`

The consecutive bodies are:

- `2026-08-15T14-24-23-707Z-003634-...body.json`: 3 blocks — `text(" ")`, `thinking`, `tool_use(toolu_01DeeoyDJrAWS2HJg3xtgFCd)`.
- `2026-08-15T14-24-42-116Z-003639-...body.json`: the same 3 blocks plus trailing `text(" ")`.

The raw OpenCode message is `msg_005ce98df001L9pw971jRHo4Rz`. Its durable parts are:

| Part | Created UTC | Updated UTC |
| --- | --- | --- |
| `step-start` | 14:23:49.257 | 14:23:49.258 |
| `reasoning` | 14:23:49.271 | 14:24:07.678 |
| `tool` | 14:24:07.681 | 14:24:15.961 |
| `step-finish` | 14:24:15.978 | 14:24:15.979 |

There is no raw text part, whitespace-only or otherwise. The trailing provider block is derived from the late structural part.

### `message[583]`

The consecutive bodies are:

- `2026-08-15T14-25-31-585Z-003649-...body.json`: 3 blocks — `text(" ")`, `thinking`, `tool_use(toolu_01BbRQMJLM7LyqQE9yoSD3em)`.
- `2026-08-15T14-25-44-839Z-003653-...body.json`: the same 3 blocks plus trailing `text(" ")`.

The raw OpenCode message is `msg_005cfa9d5001vGCSrkJR4iNNkO`. Its durable parts are `step-start`, `reasoning`, `tool`, and `step-finish`; `step-finish` was created at `14:25:25.791Z`. Again, the row contains zero whitespace text parts.

### Timing interpretation

The durable `step-finish` timestamps precede the auth-dump timestamps by several seconds. Therefore the evidence does **not** say that SQLite committed a literal trailing text part after the HTTP body was dumped. It says:

1. the first transform request used an in-memory/source snapshot without the terminal structural part;
2. the body was dumped later by the auth transport;
3. the next transform source included `step-finish`;
4. structural cleanup deterministically converted the newly visible part to an empty text sentinel, which the Anthropic serializer exposed as `" "`.

This is a late-arriving source-snapshot mutation, not asymmetric reasoning stripping. Structural cleanup behaves consistently on the parts each pass receives.

The plugin logs independently show the structural count advancing by two as each assistant's `step-start`/`step-finish` pair becomes visible:

- SUBC `14:24:20.086Z`: `strippedParts=586`; `14:24:33.396Z`: `strippedParts=588`.
- SUBC `14:25:28.934Z`: `strippedParts=594`; `14:25:42.018Z`: `strippedParts=596`.

## Original 15:16 SUBC double

The original dumps have rotated out, so the exact old provider body cannot be recovered. The remaining raw rows and metadata match the proven mechanism:

- `msg_00590a941001wFrCI9akdMS1Bm` (the assistant before the pressure fold) has `step-start`, `reasoning`, `tool`, `step-finish`, no text part. `step-finish` was created at `13:16:15.848Z` (`15:16:15.848` local).
- The fold transform began at `13:16:16.734Z`; the next defer transform began at `13:16:36.055Z`.
- `msg_00590e17d001mfsRKnEya5Q2Go` similarly ended with `step-finish` at `13:16:33.259Z`, immediately before the next transform.
- `transform_decisions` records the `401,822`-token pass as `execute`, `materialized=1`, `pressure_refold`, followed by the `402,360`-token pass as `defer`.
- Both transform logs report M0/M1 `rematerialized=false, reason=cache_hit`.

This data is consistent with the same late-terminal-part transition and contradicts an M0 rematerialization explanation. Without the rotated bodies, the precise old message index is not independently provable; the retained 14:24/14:25 SUBC bodies prove the byte mechanism itself twice.

## CKCRED cross-check and fleet scope

The original CKCRED double has the same raw lifecycle:

- `msg_004ffc87d001La7JjF3NuCTn6W` has `step-start`, `reasoning`, `tool`, `step-finish`, no text part; `step-finish` arrived at `10:37:57.925Z` (`12:37:57.925` local), immediately before the pressure-fold transform.
- `msg_004fff34d001gp7MQCbNtGwoz4` has `step-start`, `tool`, `step-finish`, no text part; `step-finish` arrived at `10:38:11.326Z`, immediately before the following defer transform.
- Logs for all three requests report M0/M1 cache hits, not rematerialization.

Later retained CKCRED dumps provide direct wire confirmation. Between `14:22:03Z` and `14:22:47Z`, `message[797]` changes from:

```text
[text(" "), thinking(586 bytes), tool_use(toolu_01GdapZpPi9cPC9q3qNqhzeD)]
```

to the same blocks plus a trailing `text(" ")`.

A cache-control-neutral comparison of every consecutive body in the retained dump directory found 44 first message-body mutations across 9 sessions. All 44 were exactly one appended trailing whitespace-only text block on an assistant; none had another message-content shape. Thus this is **the fleet signature for the retained window**, not merely a SUBC coincidence. This statement is limited to the retained window; it does not claim every historical cache bust has this cause.

## Why the suspected Aug-14 changes are not causal

### `3cc1a135` — synthetic-head predicate / marker reconciliation

The marker reconciler only decides where the synthetic summary sits relative to the M0/M1 head. The wire divergences are deep historical assistant blocks (`message[575]`, `message[583]`, CKCRED `message[797]`), with all earlier messages unchanged. No summary row moves at those indices. The current marker rows cannot reconstruct old placement, but the direct body diff excludes a head/summary relocation.

### `3adb2e41` — canonical model keys / M0 CAS

For both sessions, current `cached_m0_model_key` equals `last_observed_model_key` in canonical form:

- CKCRED: `anthropic/claude-opus-5`.
- SUBC: `anthropic/claude-fable-5`.

More importantly, the logs on the actual fold and following defer passes say `injected m[0]/m[1] (rematerialized=false, reason=cache_hit)`. The byte diff is an appended block on an old assistant, not M0 content. Canonicalization did not force the second bust.

### `526c0c52` / `3306131c` — frozen reasoning strips

Across each proven diff, the thinking signature/content and tool-use block are byte-identical. Only a terminal text block appears. The frozen-reasoning machinery may run on the same pass, but it is not the first divergent byte and cannot explain the exact 3-block-to-4-block change.

### Synthetic todo and deferred marker state

Neither proven divergent message is a synthetic todo anchor or summary. Current `session_meta` has no synthetic todo anchor for either session. Marker state is persisted, but the direct first divergence is not at the synthetic head or summary boundary.

## Fix implemented

### TypeScript lane

`stripTrailingWhitespaceFromHistoricalAssistants` in `strip-content.ts`:

- walks assistant messages on every canonical-Anthropic pass;
- skips the captured newest assistant;
- removes one or more trailing text parts whose text trims to empty;
- preserves leading whitespace and all meaningful blocks;
- leaves wholly blank messages intact so provider framing does not synthesize replacement content;
- is stateless and does not depend on a watermark, CAS, pass class, or live array position.

`finalizeMessageRepresentation` invokes it after merged-reasoning replay/detection, so a late `step-finish` sentinel cannot survive into the served historical assistant. Compaction-off mode skips it together with structural cleanup.

### Rust/native lane

`strip_trailing_whitespace_from_historical_assistant` in `crates/mc-module/src/transform.rs` applies the same rule after surface strips. It is gated to:

- `SerializerProfile::OpencodeAiSdk`;
- provider `anthropic`;
- assistant messages that are not the reasoning/newest mutation exemption.

This catches the Rust OpenCode path where `codec/opencode.rs` decodes `step-start`/`step-finish` as opaque blocks and `apply_surface_strips` turns them into sentinels.

## Composition with `4a4b7b4b` Fix A

Fix A and this fix act on opposite ends of the assistant content array:

- **Leading whitespace stays.** The reasoning keep-rule treats it as sentinel-invisible so signed thinking remains the first meaningful block. Removing or treating it as prose would reintroduce the newest-to-historical reasoning transition.
- **Trailing whitespace goes once historical.** It follows thinking/tool content and carries no signed reasoning semantics. Removing it prevents a late terminal structural marker from changing cached bytes.
- **Both-leading-and-trailing shape:** `[" ", thinking, tool_use, " "]` becomes `[" ", thinking, tool_use]`. The leading sentinel still protects thinking classification; the trailing sentinel cannot bust the prefix.
- **Newest assistant stays byte-identical.** Signed reasoning and its surrounding blocks remain untouched until the assistant is historical.

The raw evidence corrects the original source attribution in Fix A's test commentary: in the proven sessions, OpenCode's structural sentinels produced the whitespace blocks; the model did not persist literal whitespace text parts. Fix A's behavioral rule remains correct.

## Sibling-site audit

| Site | Finding / action |
| --- | --- |
| TS `stripStructuralNoise` | Causal producer: converts `step-finish` to an empty text sentinel. Kept, with final trailing normalization added after all transforms. |
| TS final representation | Fixed; this is the last shared TS mutation point before serving/capture. |
| Caveman cleanup/replay | No change. It operates on tagged source text. The raw divergent rows contain no text part, and structural step markers are not caveman targets. |
| OpenCode tag walk / shared tag transcript | No change. Tagging sees the raw `step-finish` as structural rather than message text; there is no text identity or source-content row to freeze. The normalization belongs after tagging so it cannot perturb tag identity. |
| Rust `apply_surface_strips` | Causal twin: converts OpenCode opaque step markers to sentinels. Fixed by the post-strip OpenCode/Anthropic residual. |
| Rust `codec/opencode.rs` | Decodes `step-start`/`step-finish` as opaque blocks and re-encodes text sentinels. Covered by the shared Rust residual. |
| Rust `codec/pi.rs` | No change. Pi decodes native `text`, `thinking`, and `toolCall`; it has no OpenCode step marker class. |
| TS Pi `context-handler.ts` | No change. Its existing audit explicitly omits `stripStructuralNoise` because Pi `AgentMessage` has no `step-start`/`step-finish` parts and does not use the OpenCode AI-SDK merge path. |
| Rust `SerializerProfile::Pi` | Explicitly regression-tested to retain trailing native text. The new strip is gated to `OpencodeAiSdk`, preventing an accidental Pi semantic change. |
| Offline context-dump transform | It invokes structural cleanup for diagnostics, not the live provider serializer/final served representation. No production fix needed there. |
| LKG | Slots are process-memory snapshots and are overwritten by later successful captures; old fold/defer slots are not durable. The retained provider bodies supersede the need to infer this mechanism from current LKG state. |

## Regression and non-vacuity evidence

- TypeScript late-marker test constructs pass 1 without `step-finish`, then a fresh defer rebuild where the same assistant is historical and includes `step-finish`. The representations are byte-identical after normalization, with leading whitespace retained.
- TypeScript final-representation test covers the both-leading-and-trailing case and verifies the newest assistant's trailing block remains untouched.
- Rust test covers the same newest-to-historical transition and asserts canonical message bytes match; it also verifies `SerializerProfile::Pi` is not stripped.
- Non-vacuity mutation: temporarily replacing the TS normalizer with `return 0` made `keeps a late step-finish from changing the next defer representation` fail (`Expected: 1`, `Received: 0`). The deliberate break was restored before commit.

## Durable-state limitation

`session_meta` is a current-state row, not a per-pass history of rendered message arrays. It can rule out current model-key mismatch and show current marker/todo/frozen sets, but it cannot reproduce the old provider bodies. The current in-memory LKG slot likewise does not retain both historical passes. If provider dumps were unavailable, the next occurrence would need per-pass served-prefix fingerprints plus the first divergent message's pre/post part-type vector and source snapshot timestamp. For this occurrence, the retained bodies and raw part timelines pin the mechanism directly, so no further instrumentation is required to justify the fix.
