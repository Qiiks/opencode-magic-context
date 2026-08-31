# External PR #399 — historian temperature review

Date: 2026-08-31  
PR head: `55ad3d123e047b47895da0842401a027ddb9d1ed`  
Base: `1ce9652137ae99431c15dedec9804aaaef2e8337`

## Verdict

**Request changes.** The Pi implementation is directionally correct and the error-shadowing diagnosis is especially strong, but two merge blockers remain:

1. **The tests do not pin the two defaults whose removal is the core fix.** With `temperature: historian?.temperature ?? 0.1` restored in `index.ts` and `temperature = 0.1` restored in `pi-historian-runner.ts` under a `NON-VACUITY BREAK`, all 93 PR-focused tests still passed. Add one resolver assertion that omission remains `undefined` while an explicit value survives, plus one runner assertion that an omitted temperature stays omitted at the spawned historian attempt. Those assertions should fail independently when either default is restored.
2. **The common documentation and PR scope are broader than the implementation.** `CONFIGURATION.md` says omitted temperature is never sent, but this PR changes only Pi. OpenCode still synthesizes `0.1`, and the Rust producer still sends fixed `0.1`. Narrow the documentation/description to Pi until the other legs are fixed. Also replace “Reasoning models reject the parameter outright” with “Some reasoning models reject or constrain the parameter”; Anthropic thinking accepts only `1`, so the current absolute statement contradicts the PR's own provider table.

Do **not** expand this contributor PR into the OpenCode/Rust fix. The follow-up is scoped below.

## Claim-chain adjudication

| Claim | Result | Source adjudication |
|---|---|---|
| `10f80e58` added Pi `0.1` defaults in two places and first shipped in v0.41.0 | **Verified, with a cross-leg chronology correction** | The commit adds `historian?.temperature ?? 0.1` in `packages/pi-plugin/src/index.ts` and `temperature = 0.1` in `pi-historian-runner.ts`. `git tag --contains 10f80e58` returns `v0.41.0`. The same commit also adds OpenCode's `temperature: 0.1`. Rust's fixed temperature predates it: `5183053ce6e82edcbf4ba09c63fed214c21a8bb3` added `HISTORIAN_TEMPERATURE` on 2026-07-03. |
| Reasoning/thinking providers reject `0.1` | **Verified directly for OpenAI; consistent with the Anthropic constraint** | A live isolated Pi request to `openai-codex/gpt-5.4` returned `stopReason:"error"`, empty content, zero usage, and `Codex error: Unsupported parameter: temperature`. The same request without the temperature returned `PONG`. Anthropic's stated behavior is a constraint rather than an outright rejection of every temperature: thinking permits `temperature=1`. |
| Every fallback fails because the request shape is shared | **Qualified verified** | Pi carries the same `temperature` through first, repair, fallback, and editor calls, and every model attempt receives the same environment-backed request rewrite. Therefore every fallback that rejects `temperature=0.1` fails for the same cause. A fallback that accepts `0.1` can succeed, so this is not universal across arbitrary chains. The reporter's 16-success to 32-failure timeline is plausible empirical evidence but is not independently present in repository state. |
| The provider 400 is hidden by the empty-assistant branch | **Verified** | `subagent-runner.ts` captures `finalErrorMessage`, but the empty/null text branch settles `no_assistant` before the `finalStopReason === "error"` branch, dropping the captured provider error. |
| The PR restores omission, independently applies both calibration knobs, and surfaces the provider error | **Verified for Pi** | Both Pi defaults are removed; the calibration extension spreads `temperature` and output-token fields independently; and the empty-assistant message appends `finalErrorMessage` without changing its reason code. |

## Standard six axes

### 1. Correctness

The Pi code change is correct at each touched seam:

- config omission remains `undefined` in `resolveHistorianFromConfig`;
- the runner no longer reintroduces `0.1` by destructuring;
- the child environment omits `MAGIC_CONTEXT_HISTORIAN_TEMPERATURE` when undefined;
- the calibration extension does not add top-level or nested temperature fields when undefined;
- `maxOutputTokens` remains independently applicable to top-level OpenAI/Anthropic-style keys and nested Gemini/Bedrock shapes; and
- explicit zero remains valid because checks use `!== undefined`, not truthiness.

### 2. Control flow and fallback behavior

The request-shape diagnosis is sound. `PiSubagentRunner.runModelChain` spawns fresh attempts but reuses `options.temperature`; `runPiHistorian` passes that option to first, repair, configured fallback, active-model fallback, and editor calls. Removing the implicit value fixes every Pi attempt at once.

The enriched error remains `reason: "no_assistant"`, so fallback eligibility is unchanged. It also retains `meta.sawProtocolOutput: true`, so a legitimate provider-emitted empty response does not become the isolated `--no-extensions` retry used for silent exit-0 failures.

### 3. Backward compatibility and #7948

For Pi, explicit flash calibration remains functional end to end:

- `historian.temperature` remains accepted by the shared `0..2` schema;
- `resolveHistorianFromConfig` preserves an explicit value;
- the subagent runner exports that value to the child; and
- existing tests assert that `0.1` reaches `MAGIC_CONTEXT_HISTORIAN_TEMPERATURE`, while calibration tests assert that explicit `0.1` reaches every supported payload shape.

There is no config migration: absence changes behavior, while explicit values retain behavior. Cross-leg preservation is incomplete; see the leg matrix.

### 4. Tests and mutation honesty

What is good:

- the calibration tests fail when the independent calibration logic is restored to its old coupled implementation (three failures in the negative control);
- the error regression fails exactly when the provider-error append is removed (one failure, 86 filtered tests); and
- existing empty-success tests still pass.

Merge-blocking gap:

- restoring both implicit `0.1` defaults leaves every added test green (93/93). The suite manually passes `undefined` into the calibrator but never proves production config/runner omission. The two root-cause removals therefore are not mutation-pinned.

### 5. Documentation and schema

The schema already models `temperature` as optional and bounded `0..2`, so no schema migration is needed. `CONFIGURATION.md` now explains opt-in behavior, but it is a common harness table and therefore overstates what this Pi-only patch accomplishes. The generated reference at `packages/docs/src/content/docs/reference/configuration.md` also remains generic. Documentation should distinguish Pi's new omission behavior from the still-defaulted OpenCode and fixed Rust legs.

### 6. Scope, hygiene, and hazards

- Two focused commits with accurate Pi implementation boundaries.
- `git diff --check` passed.
- No lockfile, manifest, generated output, credential path, config file, schema fence, or authority fence changed.
- Security/reviewer checks shown by GitHub passed; no credential material was printed or committed.
- The PR is mergeable, but GitHub reported `mergeStateStatus: UNSTABLE`; the visible checks were review/security checks rather than the package test/typecheck gate.

## A. Leg consistency

`10f80e58` was a parity commit, but the three runtime legs do not now share safe reasoning-model behavior.

| Leg | What the parity work established | Covered by PR #399? | Same reasoning-model hazard? | Chain behavior |
|---|---|---:|---:|---|
| **Pi TypeScript** | Added the calibration extension, config default, runner default, and propagation across first/repair/fallback/editor attempts. | **Yes** | Fixed when temperature is omitted. Explicit incompatible values still fail by design. | Empty provider errors remain fallback-eligible `no_assistant`; every rejecting attempt had received the same implicit value. |
| **OpenCode TypeScript** | Added `resolveHistorianAgentOverrides`, which returns `{ temperature: 0.1, maxTokens: 32000, ...userOverrides }` for historian, recomp, and editor agents. | **No** | **Yes.** A reasoning model still receives `0.1` unless the user explicitly overrides it. | `promptSyncWithModelSuggestionRetry` advances the configured model suggestions, but all attempts use the same registered agent override, so every temperature-rejecting model sees the same cause. |
| **Rust `mc-module` / Broca** | Hunt #11 treated Rust's existing producer shape as the parity baseline; the literal Rust temperature constant came from `5183053ce`, not `10f80e58`. `HistorianProducer::start` always sends `generation.temperature=0.1`. | **No** | **Yes.** The request is provider-independent and unconditional. | Broca owns error classification. Permanent/transient model failures advance eligible fallbacks; auth errors skip that provider; overflow stops. Every attempted model still receives `0.1`. Exhaustion persists `last_failure` plus backoff. Ordinary incremental firing is background and the transform continues without a new fold; inline emergency/wrapup paths return the producer failure/degraded result. |

## B. #7948 preservation by leg

- **Pi:** pass. Explicit `historian.temperature: 0.1` remains available and tested through environment and payload calibration.
- **OpenCode:** the explicit knob works because user overrides spread after defaults, but omission still synthesizes `0.1`; this PR does not solve reasoning compatibility there.
- **Rust:** fail for the requested “explicit config on every leg” condition. The module has no historian-temperature config/wire field; `0.1` is a fixed constant. Flash calibration exists, but it is not opt-in or user-selectable.

## C. Fleet self-check

The workstation check opened both SQLite databases read-only and verified `PRAGMA query_only=1`. No provider payloads, prompt text, credentials, or full project paths were retained.

Two windows were inspected:

1. Since the parity commit time (`2026-08-28T07:22:26Z`):
   - 150 durable historian rows across 12 OpenCode project identities and one Pi project identity;
   - OpenCode: 60 success, 60 no-op, 0 failed;
   - Pi: 27 success, 3 no-op, 0 failed;
   - 88 historian invocations, all completed on `google/antigravity-gemini-3.7-flash`;
   - zero `no_assistant`/empty-output signatures, zero temperature signatures, and zero current failure counters in this window.
2. Since the v0.41.0 tag time (`2026-08-30T17:27:25Z`):
   - 16 durable rows across three OpenCode identities and one Pi identity;
   - 15 success, one no-op, 0 failed;
   - 15 completed historian invocations, again all `google/antigravity-gemini-3.7-flash`;
   - zero `no_assistant`, temperature, or current failure signatures.

The module store had 211 post-parity active state rows (including drill traffic), zero current `last_failure`, zero `no_assistant`, and zero temperature signatures. Three organic sessions had nonzero firing sequences and were idle with no current failure. Rust stores do not retain a complete failure ledger equivalent to `historian_runs`, so cleared historical failures cannot be excluded from this snapshot.

**Fleet conclusion:** our own recorded historians have not been silently failing on reasoning chains, because the observed chain was exclusively the flash model that accepts the calibration. This bounds our blast radius; it does not refute the reporter's reasoning-chain failure.

## D. Live reproduction

A cheap reasoning-capable model was available (`pi auth check --model openai-codex/gpt-5.4 --json --no-refresh` returned ready), so the request-shape claim was tested directly with session persistence, skills, and discovered extensions disabled. The explicit historian calibration source extension was loaded.

- With `MAGIC_CONTEXT_HISTORIAN_TEMPERATURE=0.1` and max output 32000: `openai-codex-responses`, empty content, `stopReason:"error"`, zero tokens, `Codex error: Unsupported parameter: temperature`.
- With temperature absent: the same model/prompt returned `PONG` with `stopReason:"stop"` (1,215 input, 24 output, 16 reasoning tokens; approximately $0.0034).

This is a direct positive/negative control for the provider request shape.

## E. Error-shadowing and empty-success preservation

The PR does **not** reorder classification or turn the provider rejection into `model_failed`. It keeps the empty-content outcome as `no_assistant` and only enriches its message. Consequently:

- a legitimate `stopReason:"stop"` with whitespace-only output remains `no_assistant` with the original text;
- a provider `stopReason:"error"` with empty output remains fallback-eligible but now exposes the captured provider error;
- a non-empty error response still reaches the existing `model_failed` branch; and
- `DreamerProviderOutputFailureError` is an OpenCode-only post-manifest classifier in `packages/plugin`, while this change is Pi child-protocol handling in `packages/pi-plugin`; there is no shared branch or reclassification interaction.

The existing empty-success regression passed, and the focused suite passed 93/93.

## Follow-up scope (separate from PR #399)

Open one maintainer follow-up covering exactly the two uncovered legs:

1. **OpenCode:** remove the synthesized `temperature: 0.1` from `resolveHistorianAgentOverrides`, preserve explicit `historian.temperature`, and test omission/explicit `0.1` across historian, historian-recomp, and historian-editor registrations.
2. **Rust/module:** add an optional trusted historian-temperature config/wire value; make omission exclude `generation.temperature` from every Broca attempt; keep `max_output_tokens` independent; preserve explicit `0.1`; and add fake-Broca request tests for absent versus explicit temperature across primary/fallback attempts. Autonomous module configuration needs the same optional field rather than a new hidden default.
3. **Docs:** once both land, make the common docs truthfully state repository-wide opt-in behavior and retain the flash-calibration guidance.

Broca itself should not guess model families or silently rewrite temperature; the producer should send the intended optional generation shape, and Broca should continue returning provider diagnostics/classification.

## Verification performed

- PR body and full diff read before source inspection.
- `git diff --check 1ce9652..origin/pr-399` — passed.
- `bun install --frozen-lockfile` — passed; lockfile and manifests unchanged.
- `bun run typecheck` in `packages/pi-plugin` — passed.
- `bun run lint` in `packages/pi-plugin` — passed with two unrelated existing non-null-assertion warnings.
- `bun test` in `packages/pi-plugin` — 885 passed, one unrelated timing failure (`tail-hygiene-walk-pi` p95 20.3ms versus 15ms); no changed behavior failed.
- Focused PR tests — 93 passed, 0 failed.
- Error-message negative control — failed exactly the new test as expected.
- Calibration negative control — failed three new assertions as expected.
- Default-removal mutation probe — all 93 focused tests stayed green, establishing the blocker above.
- Live reasoning-model reproduction/control — rejection with `0.1`, success without it.
- Live fleet SQLite checks — query-only verified.
