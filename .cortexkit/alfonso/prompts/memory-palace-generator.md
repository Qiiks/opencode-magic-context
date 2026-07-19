# Build the memory-palace image generator (experiment, no production wiring)

Branch from `subc-migration`. Deliverable: a working generator under
`packages/plugin/scripts/experiments/visual-memory/` that turns the budget-trimmed memory tail
into ONE dense "memory palace" PNG, plus the rendered output for human review. This is an
EXPERIMENT artifact — no production code paths touched, no config flags yet.

## Input
`/tmp/visual-memory/trimmed-memories-source.txt` — 334 real memories in `#id: content` lines
grouped under `<CATEGORY>` tags. (If missing, regenerate with the existing
`render-trimmed-memories.ts` in the same experiments dir.)

## Step 1 — AUTHOR the palace text (the judgment-heavy part; you author it yourself, in chunks)
Transform the memories into `palace.txt`: a 2D text layout of "rooms" using box-drawing
characters, rendered later as pixels. Authoring rules (each is load-bearing, learned from prior
experiment rounds):

1. CLUSTER by hub: group memories sharing a subject (Pi, historian, shadow/subc, dashboard,
   embeddings, release/CI, workspace/memory, config, ...). A room = one box with the hub name in
   its top border. State the hub ONCE; entries never repeat it.
2. COMPRESS each memory to anchors + relations: keep exact identifiers (file paths, function
   names, env vars, version numbers, error codes) — they are the ctx_search keywords; delete
   connective prose, articles, restated subjects. Pidgin style. Single CJK chars and symbols
   (→ ← ⊘ ✓ ⚠ 記 影 鎖 修 漏) welcome where they compress a concept.
3. POLARITY RULE (non-negotiable): any negative rule ("must not", "never", "excludes",
   "instead of") carries an explicit ⊘ on the excluded thing AND a terse mechanism parenthetical
   — e.g. `⊘array-binds (node:sqlite reads as named-params→throw)`. Nouns without polarity
   invert in recall; nouns with ⊘+mechanism verified 8/8. Positive-only facts need no marker.
4. NO memory ids in the output. No `#1234` anywhere — the surface is search-cue only.
5. EVERY memory must be represented (merge true duplicates, but no silent drops). Track
   coverage: the script asserts count(source memories) == count(entries + merges) via a sidecar
   JSON you emit (memory id -> room/line mapping) so coverage is verifiable even though ids
   don't render.
6. DETERMINISTIC layout: rooms ordered by (category, hub name); entries within a room by source
   memory id ascending. Double-line borders (╔═) for rooms whose peak memory importance >= 70,
   single (┌─) otherwise. Fixed max width 152 chars per line, wrap by hand at word boundaries.
7. Size target: whole palace <= 20,000 chars (aim lower; the win is compression).

Author in chunks (the source is 63k chars) — read a category, write its rooms, move on. Do NOT
delegate the authoring to a summarization one-liner; the anchor selection is the product.

## Step 2 — RENDER script
`build-palace.ts` in the experiments dir: reads `palace.txt`, renders via
`import { renderTextToImages } from "/Users/ufukaltinok/Work/OSS/pxpipe/src/core/library.ts"`
with `{ reflow: false }` (the 2D layout must not be reflowed — box art dies), writes
`/tmp/visual-memory/palace-page<N>.png`, prints: chars, pages, image tokens
(ceil(w*h/750) per page), text-token equivalent (chars/4), compression ratios vs prose-as-text
and vs prose-as-image (3431 tokens, the measured baseline). Assert droppedChars == 0.

Note: renderTextToImages may split pages at ~28k chars / height caps — fine, but verify box art
isn't split mid-room; if it is, split rooms across pages yourself at authoring time.

## Step 3 — VERIFY + report
- Run the render, confirm 0 dropped chars, report the token numbers.
- Copy final PNGs to ~/Desktop/visual-memory/ (palace-page*.png) for human review.
- Commit the experiments-dir files (palace.txt, build-palace.ts, coverage sidecar) with trailer
  `Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>`.
- Result JSON: chars, imageTokens, coverage counts, and the room list with entry counts.

## Constraints
- Do not touch anything outside packages/plugin/scripts/experiments/visual-memory/.
- bun only; no new dependencies (pxpipe imported by absolute path from the sibling checkout).
