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
| ollama-cloud/deepseek-v4-pro | author-trial-v3.md | fail | 49/424 | — | — | — | — | — | not returned by OpenRouter | NOT-VIABLE |
| ollama-cloud/deepseek-v4-flash | author-trial-v3.md | fail | 49/424 | — | — | — | — | — | not returned by OpenRouter | NOT-VIABLE |
| ollama-cloud/glm-5.2 | author-trial-v3.md | fail | 49/424 | — | — | — | — | — | not returned by OpenRouter | NOT-VIABLE |

## Per-model cost estimate

| Model | Cells | Prompt tokens | Completion tokens | OpenRouter cost | Verdict |
| --- | ---: | ---: | ---: | --- | --- |
| ollama-cloud/deepseek-v4-pro | 1 | 5810 | 23644 | not returned by OpenRouter | NOT-VIABLE |
| ollama-cloud/deepseek-v4-flash | 1 | 5810 | 21404 | not returned by OpenRouter | NOT-VIABLE |
| ollama-cloud/glm-5.2 | 1 | 18099 | 44099 | not returned by OpenRouter | NOT-VIABLE |

## Verdict policy

A cell is **VIABLE** only when parsing, full validation, rendering, and full coverage succeed with at least 85% anchor fidelity. A parse-recovered or low-anchor cell is **VIABLE-WITH-CAVEATS**. Any parse, validation, coverage, or rendering failure is **NOT-VIABLE**.
