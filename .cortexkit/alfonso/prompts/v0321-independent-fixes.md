# Two projector-independent fixes for v0.32.1

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Two small, real, standalone bugs from the (otherwise-deferred) cache-core batch. Both are independent of the deferred serializer-projector work — do NOT touch merged-assistant/wire-topology/COW/persist-before-publish code. Verify each at source first.

## Fix A — compartment title injection (shipped in v0.32.0)
The compartment-diet renderer interpolates the historian-authored compartment TITLE into a markdown heading (`## <range> · <dates> · <title>`) WITHOUT sanitizing it. A title containing a newline + `## ` or a literal `</session-history>` can forge history structure / close the wrapper early. Titles are historian-authored (an LLM), so this is untrusted output.

Sites: the decay renderer, BOTH harnesses:
- TS: packages/plugin/src/hooks/magic-context/decay-render.ts (the heading-building function — find where `title` / `c.title` is interpolated into the `## ...` line).
- Rust: crates/mc-module/src/decay_render.rs (the equivalent heading builder).
FIX: sanitize the title before interpolation — (1) collapse any newlines/control chars to a single space (single-line guarantee), (2) XML-escape it (the block lives inside `<session-history>`, so `<`/`>`/`&` must be escaped) OR strip `<`/`>` if escaping doesn't fit the markdown-heading context — match whatever the body guard (`guardCompartmentBody`, which already indents `## ` lines) does for consistency; pick the approach that makes a forged `</session-history>` or `\n## ` in a title inert. Apply identically in TS and Rust (byte-identical output — there are differential goldens between them; regenerate if needed).

CACHE EPOCH: this changes rendered bytes ONLY for compartments whose titles contain the pathological chars (clean titles escape to themselves = no change). To keep the change cache-deterministic, bump the decay-render epoch (currently `cre1` → `cre2`) using the module-side epoch self-fold pattern (memory #8603: profile_render_epoch / M0ContentEpoch, omitted-at-zero) so affected sessions take exactly ONE coordinated HARD fold rather than a mid-stream defer bust. If the TS side has a matching render-epoch constant, bump it in lockstep. Verify the epoch bump is the SAME mechanism the diet itself used.

Tests: a compartment title containing `\n## 999-999 · forged` and one containing `</session-history>` render inert (no new heading, no early close); a clean title renders byte-identical to before (modulo the epoch marker); TS/Rust differential golden matches.

## Fix B — Pi image-only toolResult → Anthropic 400
Pi replaces a dropped/processed image with `{type:"text", text:""}`. For an image-only `toolResult`, Pi's Anthropic adapter concatenates that to `content:""` and emits the `tool_result` — and unlike empty user/assistant messages, this path is NOT filtered, so Anthropic 400s on empty content. (Also, empty text collapses the message boundary.)

Site: packages/pi-plugin/src/strip-processed-images-pi.ts (the image→text replacement, ~lines 50-103).
FIX: use a stable NON-EMPTY marker `{ type: "text", text: "[image stripped]" }` for BOTH the user-message and toolResult image replacements. This preserves the paired tool_result content (no 400) AND preserves the message boundary. Constraints: the marker must NOT match the `[dropped]`/`[dropped N]` placeholder recognizer (it doesn't — good), and it replays deterministically (frozen-id replay path — the marker string is a pure constant, same on every pass). This is a one-time representation change: sessions with already-stripped images will bust cache ONCE on next pass then stay stable — acceptable and unavoidable for a representation fix. Do NOT change OpenCode's image handling (that's entangled with the deferred topology work; Pi-only here).

Tests: an image-only toolResult strips to `[image stripped]` (non-empty, boundary preserved); defer-pass replay is byte-identical; the marker is not treated as a `[dropped]` sentinel.

## Gates
Both harnesses' suites as touched: packages/plugin (bun test, typecheck, lint) for Fix A TS + Rust (cargo test -p mc-module, clippy, fmt) ; packages/pi-plugin (bun test, typecheck, lint) for Fix B. check_comments. Comments explain the invariant (why sanitize untrusted historian title; why non-empty image marker) — no audit/finding references. Report per-fix status + test evidence.
