# Palace: knapsack-to-one-image inversion

Repo: ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Scope ONLY packages/plugin/scripts/experiments/visual-memory/. Dev harness; gate = `bun test author-palace.test.ts` + the two repro renders below.

## New contract (replaces fit-everything + emergent pages)

The authoring model receives the overflow memory pool, SELECTS the most important, compresses cues as hard as it can, and returns rooms whose entries are ORDERED BY IMPORTANCE (descending) within each room, rooms themselves importance-ranked within each category. The renderer then KNAPSACKS into exactly ONE 1092x1092 page, always: place content greedily in importance order until the page is full, and DROP whatever does not fit. Page count is a constant 1. Nothing is rejected for volume; importance ordering IS the truncation policy.

## Renderer changes (author-palace.ts)

1. renderPalace emits exactly one 1092x1092 page. Delete the multi-page packer path from the real render (bands/pages/subsetForCapacity across pages). Keep a much simpler placement: iterate placement units in priority order, place while they fit, stop-or-skip when full (see 4).
2. Priority order for placement: category order fixed (PROJECT_RULES, ARCHITECTURE, CONSTRAINTS, CONFIG_VALUES, NAMING), rooms within a category in manifest order (the model ranks them), entries within a room in manifest order (the model ranks them). Placement unit = room; if a whole room does not fit in remaining space, TRIM ITS TAIL ENTRIES (lowest importance last) until it fits or has fewer than 2 entries, in which case skip the room entirely and continue to the next (a later smaller room may still fit).
3. The coverage sidecar reports what RENDERED vs what was dropped: rendered ids, dropped-by-trim ids, dropped-by-skip ids, plus renderedMemoryCount / droppedMemoryCount. placements only contains rendered ids.
4. Every loop keeps hard iteration ceilings with loud throws (preserve the existing guard style).
5. Delete or bypass validator rules that enforce VOLUME: exactly-once coverage (uncovered ids are fine now, they mean the author chose selection; duplicated ids remain a defect), cue char budgets become WARNINGS ONLY in all modes (they are authoring-quality diagnostics, not gates). KEEP as hard rules: duplicate ids, category mismatch, memory-id leakage (#N in cue), polarity marker + mechanism rules, merge integrity.
6. MAX_PALACE_CHARS and page-count checks in build-palace.ts adjust to the 1-page contract (canvasHeightCells fixed to one page height).

## Prompt changes (write author-trial-v7.md from v6)

Rewrite the task framing: "You will not fit everything; that is expected. Select the memories that matter most, compress each cue as hard as possible (pidgin relations, CJK where shorter, drop connective prose), and ORDER everything by importance: rooms most-important-first, entries within each room most-important-first. The renderer fills one fixed-size image top-down in your order and drops the tail that does not fit, so anything you rank low may not render." Remove the exactly-once self-check (replace with: every id you DO emit appears at most once). Keep the polarity rules and worked examples. Keep merge semantics. Room budget guidance stays (fewer fatter hubs) but as guidance.

## Verify

1. Deterministic re-render of the SAVED v6 manifests (env PALACE_RENDER_DESPITE_VALIDATOR=1 bun run-palace-trial.ts --model ollama-cloud/deepseek-v4-flash --prompt author-trial-v6.md --think false --reuse-manifests): must complete in seconds, produce EXACTLY palace-page1.png (one page), and the coverage sidecar must show rendered + dropped splits. Report imageTokens (expect ~1,521 for one 1092x1092 page) and renderedMemoryCount vs 424.
2. v5 manifests same command: also one page, no hang.
3. bun test author-palace.test.ts green after updating tests to the new contract (page-stacking tests become single-page truncation tests; add a test that a low-priority room is dropped when the page is full and the sidecar reports it).
NOTE: trials/*/raw-*.xml and corpus/palace-corpus.json are gitignored and contain private memory text: never commit them or quote their content; synthetic fixtures only.

Commit with the contract inversion explained. Report: rendered/dropped counts on v6 and v5 manifests, token count, test names.
