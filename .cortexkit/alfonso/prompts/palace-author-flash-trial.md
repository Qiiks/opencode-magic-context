# DS4F palace-authoring feasibility trial

Branch from the palace worktree branch `alfonso/task/bg_7923203b-build-memory-palace-generator`
(it has the generator + validator + the frontier-authored spec JSONs to compare against).
Work in packages/plugin/scripts/experiments/visual-memory/.

GOAL: determine whether deepseek/deepseek-v4-flash can author palace cue specs at acceptable
quality, or whether the future dreamer `build-palace` task must recommend a stronger model.
This is a HARNESS TRIAL, not production code.

## Build `author-trial.ts`
A single-shot, non-agentic harness (the classify-memories harness pattern — one prompt in, one
manifest out, fail-closed parse):

1. INPUT: one category's memories from /tmp/visual-memory/trimmed-memories-source.txt
   (run per-category; trial covers PROJECT_RULES and ARCHITECTURE — the two biggest/hardest).
2. PROMPT: system prompt teaching the authoring rules — write it from the existing validator's
   requirements (author-palace.ts validate()): cluster into hub-noun rooms (concrete system
   nouns; abstract only when no noun covers >=70% of entries), one cue per memory (or
   mergeInto), EVERY memory covered, cues = anchors + relations pidgin (exact identifiers
   verbatim: paths/functions/env vars/versions; drop connective prose; symbols → ← ⊘ ∵ ≺ ≻ ∅ ∀
   allowed), THE POLARITY RULE (every negative rule carries ⊘ on the excluded thing + a terse
   mechanism parenthetical immediately after), no memory ids in cues, no hub-noun repetition
   inside cues, importance passthrough. Schema-by-example (DS4F needs literal examples, not
   abstract schema): include 3-4 example entries lifted from the frontier spec files of a
   category NOT being tested (e.g. spec-naming.json) so there's no answer leakage.
   Output format: the exact spec-JSON array shape the validator reads.
3. MODEL CALL: deepseek/deepseek-v4-flash via OpenRouter (key at ~/.config/openrouter.key),
   temperature 0.1, max_output_tokens generous (16k). Same call pattern as the existing
   calibration harnesses (see scripts/experiments/ for prior art; test-historian-prompt.ts
   style). One call per category.
4. VALIDATE: run the model's spec through the EXISTING validator logic (import/extract from
   author-palace.ts — do not fork the rules). Fail-closed JSON parse (missing/truncated root =
   reject, not partial-apply).
5. REPORT per category:
   - Coverage: memories covered / total (validator's uncovered list).
   - Hard failures: validator error classes hit (missing polarity mechanism, hub repetition,
     id leakage, broken anchors, unbalanced parens), with counts + 3 examples each.
   - Anchor fidelity: % of load-bearing exact tokens from the source memory that survive into
     the cue (measure with the validator's isExactToken over source vs cue).
   - Room quality (qualitative): list DS4F's room names next to the frontier spec's room names
     for the same category.
   - Side-by-side: 6 sample memories, frontier cue vs DS4F cue, verbatim.
   - Retry behavior: if the first output fails fail-closed parsing, ONE retry with the error
     appended; report whether retry recovered.
6. Also run the SAME trial prompt once against openai/gpt-5.6-sol via OpenRouter if the key
   supports it (single category, PROJECT_RULES only) as a sanity baseline — if the harness
   itself is broken, the frontier model failing tells us that.

## Deliverables
- author-trial.ts committed to the experiments dir (+ the trial system prompt as a .md next to
  it), gitignore-safe (no keys).
- /tmp/visual-memory/trial-ds4f-<category>.json raw outputs + a TRIAL-REPORT.md in the
  experiments dir with the metrics above and a one-paragraph verdict: SHIP-ON-FLASH /
  SHIP-WITH-STRONGER-MODEL-RECOMMENDED, with the specific failure classes that drove it.
- Do NOT modify author-palace.ts / build-palace.ts beyond exporting the validator if needed.
- Commit with trailer Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>.
