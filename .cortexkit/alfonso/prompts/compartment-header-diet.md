# Compartment header diet: markdown headings replace XML compartment tags on the wire

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Ufuk-approved design. This changes the RENDERED wire format of compartments inside <session-history> (m0/m1) — it does NOT touch the historian's producer/parser vocabulary (the historian still EMITS `<compartment start=.. end=.. title=..>` blocks; parsing, validation, seeds, reference-retrieval prompt blocks all stay XML — that's the producer contract, unchanged).

## The format change (renderer only)

Today (decay-render.ts:119-129, shared OpenCode+Pi):
```
<compartment start="62704" end="62776" start-date="2026-06-08" end-date="2026-06-09" title="Designed the protected-tail redesign">
body
</compartment>
```
self-closing for P4/empty: `<compartment ... />`

New:
```
## 62704-62776 · 2026-06-08→09 · Designed the protected-tail redesign
body
```
- Heading line: `## {start}-{end} · {date-range} · {title}`. No closing tag; the next `## ` heading or the </session-history> wrapper ends a compartment.
- Date range rules (temporal awareness on): same-day → single date `2026-06-08`; same-month → `2026-06-08→09`; different month/year → `2026-06-08→2026-07-02`. Dates absent (temporal off or unknown) → omit the ` · date` segment entirely.
- Title-only (P4/self-close today) → bare heading line, no body.
- Bodies unchanged byte-for-byte (P-tier text, U:/TC: lines). If a body line could start with "## " (check the tier corpus + escapeXmlContent's current role), guard it: indent-escape body lines starting with "## " by prefixing a single space (deterministic, documented in a comment). XML-escaping of body content is no longer needed for the heading format — but ONLY remove escaping if you verify nothing else depends on it; otherwise keep body escaping as-is for this change (smaller diff wins).
- The <session-history> outer wrapper STAYS (stable block boundary).

## Where (all must land in one batch)

1. packages/plugin/src/hooks/magic-context/decay-render.ts — the render functions (tiered, legacy, self-closing arms). Update decay-render.test.ts expectations.
2. Sidebar/accounting parsers that read the RENDERED format back: m0-token-breakdown.ts:86 (builds per-compartment strings for token attribution — must build the new heading format) + its tests; execute-status.ts:152 (same pattern); grep for any other consumer parsing `<compartment` FROM RENDERED m0 (not producer output) — tag-content-primitives.ts:19 comment, temporal-awareness.ts docs, read-session-db.ts:181 comment, transform.ts:375 comment: update comments to describe the heading format.
3. GUIDANCE + TOOL DESCRIPTIONS (Ufuk explicitly flagged — do not miss any): packages/plugin/src/agents/magic-context-prompt.ts:77+99 (ctx_expand guidance: "summarized into <compartment> blocks … pass its start/end attributes" → "summarized under `## start-end · date · title` headings inside <session-history> — pass the heading's start/end range"); packages/plugin/src/tools/ctx-expand/constants.ts:3 (same rewrite, keep the worked example: `## 120-245 · … · Fixed tagger collision` → ctx_expand(start=120, end=245)); search packages/pi-plugin/src for the same guidance/tool-desc texts (Pi mirrors both) and update; crates/mc-module/assets/guidance*.txt (both variants) — same text lives there for the CC leg, update identically.
4. Rust renderer parity: crates/mc-module/src/decay_render.rs mirrors decay-render.ts byte-for-byte — port the same change, then REGENERATE the differential golden from the TS side (find the generator next to the fixture in crates/mc-module testdata — decay-render golden; never hand-edit). Also m0 composition tests that assert `<compartment` in composed m0 (search crates/mc-module for the string).
5. Rust render epoch: add a compartment-render epoch component to M0ContentEpoch (crates/mc-module/src/compartment_coverage.rs) following the exact mre pattern: new constant COMPARTMENT_RENDER_FORMAT_EPOCH=1 in lib.rs, folded as "cre1" component (omitted at zero is moot since it ships at 1 — mirror how mre1 shipped), so every module session takes ONE coordinated HARD on first pass under the new binary. Extend the fold test.
6. Dashboard: search packages/dashboard for `<compartment` parsing of rendered m0 (the m0 breakdown view if any reads rendered bytes vs DB rows — DB-row readers are unaffected).

## Cache-coordination facts (context for your comments; no extra code needed)
- TS lane: m0 is composed only on HARD materializations; cached m0 replays byte-identical until the next natural fold — so the format lands per-session on its next fold with no gratuitous bust. m1's newest-compartment rendering changes on the next cache-busting pass (already busting). Comments should state the invariant, not this rationale.
- Module lane: the cre epoch component self-coordinates the one HARD (same mechanism as mre1).

## Tests (non-vacuous)
- decay-render: all three arms (tiered, legacy, title-only) render headings; date-range compression rules each covered; body starting with "## " is guarded; byte-stability: same inputs → identical output across calls.
- m0-token-breakdown + execute-status: attribution still sums correctly over the new format (update fixtures).
- Rust: differential golden regenerated + green (TS and RS byte-identical over the shared fixture); cre component in the fold test; existing m0-compose tests updated.
- Guidance: a grep-style test or assertion is NOT needed — but run a final `grep -rn "<compartment" packages/plugin/src packages/pi-plugin/src crates/mc-module/src crates/mc-module/assets` and justify every remaining hit in your report (producer contract = expected; rendered-format = must be zero).

## Gates
packages/plugin + packages/pi-plugin: bun test, typecheck, lint. cargo test -p mc-module -p mc-store, clippy -D warnings, fmt. check_comments clean (comments describe the heading format and its invariants; never this task or "diet"). Report: token-savings estimate measured on a real rendered m0 (use the biggest session's compartment set from context.db — read-only), the final grep justification table, and confirmation the historian producer path is untouched.
