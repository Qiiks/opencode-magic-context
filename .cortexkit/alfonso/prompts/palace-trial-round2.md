# Palace authoring trial round 2 — prompt v2 + mid-tier model matrix

Branch from `subc-migration` HEAD (contains the round-1 trial harness at
packages/plugin/scripts/experiments/visual-memory/: author-trial.ts, author-trial-system-prompt.md,
TRIAL-REPORT.md, and the frontier spec-*.json files). Round-1 verdict: DS4F failed fail-closed
JSON parsing (markdown fences, both categories); gpt-5.6-sol passed but is too expensive to
recommend and over-fragmented rooms (55 vs frontier's 7). Round 2 = prompt v2 + cheaper models.

## PROMPT V2 (apply all — this is the historian-v2 calibration playbook)

1. OUTPUT FORMAT: switch from JSON to a fail-closed XML manifest — this repo's dreamer house
   style (see src/features/magic-context/dreamer/manifest-parser.ts and the classify-memories
   prompt for the pattern; DS4F passes strict XML manifest parsing nightly in production — its
   round-1 failure was a JSON fence-emission pathology, not a judgment failure). Shape:
   <palace category="..."><room name="..."><entry id="7863" importance="82">cue text</entry>
   <merge id="8255" into="8391"/></room>...</palace>
   Parser: strict root element required (missing/truncated root = reject, never partial-apply),
   same fail-closed discipline as manifest-parser.ts. Update author-trial.ts's parse step; keep
   the one error-fed retry rule. Convert parsed XML entries to the SpecEntry shape and reuse the
   EXISTING validator unchanged.
2. ROOM BUDGET: hard prompt rule — 4 to 8 rooms per category; every room >= 3 entries unless the
   category itself is tiny; prefer FEWER, FATTER hubs. Name the failure explicitly: "do not
   create one room per tool or per test type."
3. FORMAT ANCHORING + ANTI-DELIBERATION: "Your reply must begin with <palace and end with
   </palace>. No markdown fences, no preamble, no commentary." Plus one fully WORKED example in
   the system prompt: a source memory verbatim -> the compressed cue, annotated with what was
   deleted and why the mechanism parenthetical stays.
4. Keep: temperature 0.1, schema-by-example drawn from a NON-tested category, the polarity rule
   text, anchor-verbatim rule, importance passthrough.

## MODEL MATRIX (all via OpenRouter, key at ~/.config/openrouter.key)

- deepseek/deepseek-v4-flash   (re-test: if XML fixes it, cheapest wins)
- deepseek/deepseek-v4-pro
- moonshotai/kimi-k2           (or the current kimi flagship name on OpenRouter; note substitution)
- google/gemini-3.5-flash

Categories: PROJECT_RULES + ARCHITECTURE (same as round 1). If a model name 404s, substitute the
closest mid-tier from the same family and note it in the report.

## METRICS per cell (model x category)

parse pass/retry-recovered/fail · coverage n/N · validator failure classes with counts ·
anchor fidelity % (existing measure) · room count (vs frontier 7 for PROJECT_RULES, 16 for
ARCHITECTURE) · 4 side-by-side cues vs frontier.

## REPORT

Extend TRIAL-REPORT.md with a "Round 2" section: matrix table, cost-per-run estimate per model
(OpenRouter pricing if retrievable, else note), and a per-model verdict: VIABLE /
VIABLE-WITH-CAVEATS / NOT-VIABLE for the unsupervised nightly build-palace dreamer task.
Commit with trailer Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>.
