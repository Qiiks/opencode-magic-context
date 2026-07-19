# Mason: build the palace v3 prompt-iteration harness

Branch from `subc-migration` HEAD. Build a historian-v2-style iteration harness for the memory-palace
authoring prompt, in `packages/plugin/scripts/experiments/visual-memory/`. This is a DEV HARNESS (not
production code) — same role the historian replay harness played for the v8.x prompt calibration:
sweep prompt-variant × model over a FROZEN corpus, and emit a comparison table + the rendered PNGs so
we can iterate the authoring prompt empirically on cheap/mid models.

## What already exists (REUSE, do not rewrite)
- `author-palace.ts` — reads per-category spec JSONs, validates (polarity ⊘+mechanism rule in
  `validate`), compacts cues (`compactCue`), lays out rooms/bands, writes `palace.txt` + a coverage
  sidecar JSON. Has `validate`, `coverage` accounting, `subsetForCapacity` (importance-ordered drop for
  the single-page budget), `isExactToken` (anchor detection).
- `build-palace.ts` — reads `palace.txt` + coverage, renders to PNG via the pxpipe atlas, computes
  `patchTokens` (28×28 patch formula — the VERIFIED token cost, matches real billing), utilization
  (contentPixels/canvasPixels), font comparison, emits a report.
- The authoring PROMPT variants live at `.cortexkit/alfonso/prompts/palace-trial-round2.md` and
  `palace-author-flash-trial.md` — read them to learn the spec-JSON output contract the models must emit
  (per-category `<palace>`/room/entry XML→ or the JSON the scripts consume; MATCH whatever
  author-palace.ts's `readSpecs` actually parses — verify at source, don't assume).

## The missing piece = the iteration driver (`run-palace-trial.ts`)
One command that runs the full loop for one (prompt, model) cell:
1. FROZEN CORPUS: first build a one-time frozen fixture `corpus/palace-corpus.json` — snapshot the
   current project's ACTIVE memory pool from context.db (the same memories author-palace consumes:
   id, category, content, importance). Freeze it checked-in so prompt A vs B is compared on IDENTICAL
   input across every run (no live-pool drift). Provide a `--rebuild-corpus` flag to re-snapshot, but the
   default MUST read the frozen fixture. Corpus builder reads the store read-only.
2. AUTHOR: call the target model via OpenRouter (key at ~/.config/openrouter.key, OpenAI-compatible
   /chat/completions) with the candidate prompt (system) + the corpus rendered as the user turn, temp
   0.1. This is the step round-2 did by hand per model — automate it. Capture raw output, parse to the
   spec shape author-palace consumes, fail-closed on unparseable (record as a parse failure, do ONE
   error-fed retry as round-2 did, then give up for that cell).
3. RENDER: feed the parsed specs through author-palace.ts (validate + layout + palace.txt + coverage)
   then build-palace.ts (PNG + metrics). Wire these as imported functions if clean, else shell out — but
   the driver must capture their structured outputs (coverage sidecar, the metrics report), not just
   scrape stdout.
4. SCORE one cell → a row: {model, prompt, parse: ok/retry/fail, coverage: N/total, validator_failures
   (by class), anchor_fidelity_%, room_count, image_tokens, utilization_%}. Anchor fidelity = fraction
   of source exact-anchor tokens (paths, code identifiers, error codes — use author-palace's
   `isExactToken`) that survive VERBATIM in the emitted cues; compute it in the driver from corpus vs
   palace.txt, don't trust the model to self-report.
5. MATRIX MODE: accept `--models a,b,c` and `--prompts p1.md,p2.md`; run every cell, write PNGs to
   `trials/<prompt>__<model>/palace-page*.png`, and emit `trials/REPORT.md` with the full matrix table +
   per-model cost estimate (from OpenRouter usage in the response, else note) + a VIABLE /
   VIABLE-WITH-CAVEATS / NOT-VIABLE verdict per model for the unsupervised nightly build-palace task.

## Constraints
- Reuse author-palace.ts + build-palace.ts as the layout/render/metrics engines — the harness OWNS only
  corpus-freeze, the model call, orchestration, and the scoring table. If those two scripts need a small
  refactor to export their core functions (vs run-on-import), do the MINIMAL extraction and keep their
  standalone CLI behavior working.
- Deterministic where it can be: same corpus + same model output → same palace.txt → same PNG bytes +
  same metrics. The only nondeterminism is the model call itself.
- No production code touched. Everything under scripts/experiments/visual-memory/.
- Frozen corpus fixture + trials/ output dir: gitignore the PNGs (binary) but COMMIT the corpus fixture
  and REPORT.md template so trials are reproducible.

## Gate + report
- `bunx tsc --noEmit` clean for the new/changed scripts (respect tsconfig.scripts.json if the repo
  excludes scripts from the main tsc — check).
- Run ONE smoke cell end-to-end: prompt = the round-2 prompt, model = deepseek/deepseek-v4-flash (cheap),
  corpus = the frozen fixture. Confirm it produces a palace.txt, a PNG, and a populated metrics row.
  Include that smoke row in your report so I can see the harness actually closed the loop.
- Report: the exact commands to run a single cell and a matrix, where outputs land, and the smoke-cell
  metrics. Commit to subc-migration with co-author trailer
  `Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>`.