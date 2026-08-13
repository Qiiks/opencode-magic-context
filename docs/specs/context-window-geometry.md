# Context-window geometry: measured provider behavior

Measured and first-party-sourced context-window behavior that catalog data (models.dev) cannot express. This document records the findings that drive Magic Context's usable-window derivation; the curation and serving of this data class is planned to move to the Fusiform module once Magic Context runs fully on subc.

Every claim carries its evidence class: `documented` (official provider docs), `source-verified` (first-party client source), `measured` (observed on the wire / provider error), or `stated` (operator-confirmed live behavior).

## Why catalogs are insufficient

1. **Windows are per access path, not per model.** The same model ID resolves to different windows depending on how it is reached (platform API vs OAuth backend vs gateway). models.dev is path-blind by construction.
2. **Catalog output fields carry placeholders.** Rows with `output == context` or `output: 0` are placeholders, not bounds; a consumer that trusts them as bounds reserves absurd amounts (or zero).
3. **Enforcement and advertisement are different quantities.** A backend can advertise one window while admitting a larger one.

## Provider geometry classes

| geometry | validation behavior | usable input | providers (evidence) |
|---|---|---|---|
| `shared_upfront` | 400 up front when prompt + requested output exceed the window | `window − requested_output`, dynamic per request | OpenAI platform API (documented: the window "encompass[es] input, output, and reasoning tokens"; 400 with `truncation: disabled`) |
| `shared_truncating` | prompt-only wall: 400 only when the prompt alone exceeds the window; over-window output truncates (`stop_reason: model_context_window_exceeded`) | `window` minus a small margin | Anthropic Messages, Claude 4.5+ (documented; version-gated — older models 400 on the combined sum). xAI Grok (measured: `maximum prompt length is N` is prompt-only) |
| `separate` | input and output are independent quotas; smaller output buys no input headroom | `input_token_limit` | Google Gemini (documented: `input_token_limit` / `output_token_limit` are separate model constants) |

Reasoning/thinking tokens count inside the output budget and the window for OpenAI and Anthropic (documented). Gemini thinking accounting is inferred as output-budget consumption (no official quote located — verify via live `usageMetadata` before relying on it).

## Harness request-shape classes

The usable-input derivation also depends on what the harness actually requests for output:

| harness | requested output cap | shape |
|---|---|---|
| OpenCode | `B = min(model.limit.output, 32k env default)`, recomputed per request but stable step-to-step; Anthropic fixed-thinking variants wire `B + thinkingBudget` | stable constant → exact wall computable (source-verified: transform.ts, llm/request.ts) |
| Pi | `min(catalog, window − current_context − 4096)`, recomputed every call | dynamic/circular → a wall derived from it would chase its own denominator; use a floor-based wall instead (source-verified: simple-options.ts). Codex-via-Pi: cap omitted on the wire by contract (the private Codex endpoint has no `max_output_tokens` field — source-verified against openai/codex) |
| Broca | caller-supplied constant frozen at admission; flat 4096 default when omitted; no catalog clamp (deliberate: 186 catalog models publish `output: 0` placeholders) | non-circular; the caller must set and clamp its own value (source-verified: broca-catalog live.rs; WAL-verified: only 4096 and 32000 observed in production) |

## Measured per-model findings (2026-08)

| model / path | window | output cap | notes |
|---|---|---|---|
| GPT-5.6 family, ChatGPT OAuth / Codex backend | **enforcement admits ≥340k** (stated, operator-confirmed live success; half-open bracket — no failure ever observed on this path). Advertised: NOT established by source — the 272,000 in codex-rs is the unknown-slug FALLBACK constant (`models-manager/src/model_info.rs:125`, `used_fallback_model_metadata: true`), not a proven server-delivered value; the operator-reported "Codex reduced to 272k while still admitting 372k internally" is grade `stated`. A logged live `ModelInfo` payload from the backend would settle advertised properly | backend accepts no `max_output_tokens` field at all (source-verified: `ResponsesApiRequest` carries no output field) | advertised and enforcement are different quantities on this path. Volatility watch: enforcement may tighten toward the advertised value; a provider-proven overflow below the admitted line is the canary |
| GPT-5.6 family, platform API | ~1.05M combined (documented) | 128k | catalog row is correct for this path only |

**Codex-path double-clamp note:** the Codex harness applies its OWN usable-window clamps client-side — `effective_context_window_percent` (default 95, server-configurable per model) and a 90% auto-compact threshold (`codex-rs/core/src/session/turn_context.rs:208`, `openai_models.rs:355,459`). Any consumer computing its own reservation on this path is stacking a second clamp on top of the harness's; when diagnosing early/late compaction on Codex, start from the fact that BOTH exist. MC's derivation is downstream of the provider wall only — it does not subsume the harness clamp.

**Store-contamination warning (checked 2026-08-13):** MC's usage-reported limit lane on this path echoes the RESOLVED HARNESS CONFIG (catalog/auth-plugin overrides), not a server-delivered value — production rows for gpt-5.6-sol cluster at 244k / 308k / 372k, which map to our own override eras (input-carve era, reserve eras, context override), not to backend announcements. Those rows are evidence of our derivation history and MUST NOT be minted as measured window cells. The clean settle for `window.advertised` on the Codex path remains a live wire capture of the backend's ModelInfo payload.
| Claude Opus 5, OAuth and API | 1M both paths (documented); prompt-only wall on 4.5+ | 128k (`max_tokens` cap on 1M-window models) | only plan gating differs on OAuth; 1M is default, no beta header |
| Grok 4.6, xAI direct | 500k, prompt-only wall (measured) | no fixed cap; default 128k, ceiling = window − prompt (documented) | catalog `output=500k` is a placeholder |
| DeepSeek V4 Flash, ollama-cloud | full 1M (`num_ctx` is a no-op on cloud) | **hard 65,536 cap, HTTP 400 above it** (documented: ollama/ollama#16890) | no catalog row carries the 64k cap; native-API rows (384k output) do not apply to this path |
| Kimi K3, Moonshot API | 1M (documented) | 131,072 default, configurable to 1M (documented) | the catalog "131k" is default output misread as context by some aggregators |
| Gemini 3.5 Flash, Antigravity and generateContent | 1M input / 65k output, both paths (documented) | 65,536 | separate quotas; Antigravity harness self-compacts around 135k (harness behavior, not a window) |

## Placeholder detection rules

- `output >= context` → treat output as absent for reservation purposes.
- `output <= 0` → treat as absent (never clamp a request to a zero placeholder).
- A catalog `input` smaller than `context` may be an allocation artifact of an auth plugin rather than an API constraint; verify against the provider's actual validation before treating it as a hard cap.

## Resolution order (Magic Context)

User/provider-hook override (auth plugins are the path-correction lane) → models.dev → provider-proven detected overflow limit (provenance-tagged `prompt_only` / `combined`, caps the catalog when smaller) → model-matched usage-reported limit. The detected-overflow lane is the empirical backstop that catches an unknown path difference before any human documents it.
