# Memory Palace v3 Prompt Trials

This development harness compares a frozen corpus across prompt and model cells.

## Commands

```sh
bun packages/plugin/scripts/experiments/visual-memory/run-palace-trial.ts --rebuild-corpus
bun packages/plugin/scripts/experiments/visual-memory/run-palace-trial.ts --model deepseek/deepseek-v4-flash --prompt packages/plugin/scripts/experiments/visual-memory/author-trial-system-prompt.md
bun packages/plugin/scripts/experiments/visual-memory/run-palace-trial.ts --models deepseek/deepseek-v4-flash,deepseek/deepseek-v4-pro --prompts prompt-a.md,prompt-b.md
```

Raw responses, palace text, coverage, metrics, and PNGs land in each named trial directory. PNGs are intentionally ignored.

## Matrix

| Model | Prompt | Parse | Coverage | Validator failures | Anchor fidelity | Rooms | Image tokens | Utilization | OpenRouter cost | Verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- | --- |
| deepseek/deepseek-v4-flash | author-trial-system-prompt.md | retry | 415/424 | missing polarity mechanism: 1 | — | — | — | — | $0.018723 | NOT-VIABLE |

## Per-model cost estimate

| Model | Cells | Prompt tokens | Completion tokens | OpenRouter cost | Verdict |
| --- | ---: | ---: | ---: | --- | --- |
| deepseek/deepseek-v4-flash | 1 | 38135 | 49030 | $0.018723 | NOT-VIABLE |

## Smoke cell

- **Model/prompt:** deepseek/deepseek-v4-flash × author-trial-system-prompt.md
- **Parse and coverage:** retry; 415/424
- **Rendered metrics:** not rendered image tokens; not rendered; anchor fidelity not available.
- **Output:** `trials/author-trial-system-prompt__deepseek-deepseek-v4-flash`
- **Failure:** validator: negative rule missing polarity marker in cue 4961: TUI code exported via `./tui` in package.json→`src/tui/entry.mjs` resolves raw `index.tsx` or compiled fallback; TUI changes take effect without rebuilding server bundle

## Verdict policy

A cell is **VIABLE** only when parsing, full validation, rendering, and full coverage succeed with at least 85% anchor fidelity. A parse-recovered or low-anchor cell is **VIABLE-WITH-CAVEATS**. Any parse, validation, coverage, or rendering failure is **NOT-VIABLE**.
