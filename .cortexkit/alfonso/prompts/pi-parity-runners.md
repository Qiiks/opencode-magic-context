# Task: Pi parity — runner resilience + notification fixes (4 findings)

Repo: this worktree (magic-context master). Changes in packages/pi-plugin (plus tests). Reference: packages/plugin. Read packages/pi-plugin/PARITY.md first — fix ACCIDENTAL divergences only.

## Finding 1: Pi historian/wrapup omit the session-model last-resort fallback

OpenCode threads the live session's model as a final fallback after configured fallback_models (transform.ts:536, 1018-1029; compartment-runner-historian.ts:494-514; wrapup passes it too, wrapup-orchestrator.ts:182-193). Pi's live historian passes only historianModel + fallbackModels (context-handler.ts:2878-2888), the Pi fallback loop confirms the chain is just fallbackModels (pi-historian-runner.ts:709-720), and Pi wrapup omits it (ctx-wrapup.ts:363-393). Pi recomp ALREADY has the correct pattern (ctx-recomp.ts:187-193) — reuse it.

Fix: add `fallbackModelId?: string` (session-model last resort) to the Pi historian args, thread the current session model from the live-historian call site and from /ctx-wrapup, and append it after configured fallbacks exactly like OpenCode (dedupe if it equals the configured model or an existing fallback).

## Finding 2: Pi historian lacks the same-model transient retry

OpenCode retries transient prompt failures on the SAME model with backoff before falling through the chain (MAX_HISTORIAN_RETRIES=2, compartment-runner-historian.ts:39, 333-389; transient classification 571-596: 429/rate-limit/timeout/5xx). Pi tries each model exactly once (subagent-runner.ts:389-407, 1058-1065; pi-historian-runner.ts:635-640). Intermittent provider hiccups fail Pi compaction where OpenCode recovers.

Fix: wrap Pi's historian run attempts with a small transient-retry helper matching OpenCode's retryable classification and backoff (reuse/port the transient-error classifier — check shared/model-suggestion-retry.ts for an existing shared classifier before writing a new one). Retry the SAME model up to 2 times on transient failures, then proceed to the fallback chain. Bound total time; abort signals must short-circuit.

## Finding 3: Pi historian failures never notify the user

The Pi runner supports notifyIssue (pi-historian-runner.ts:233-234, invoked via notify() at :274-281), but the live call site omits it (context-handler.ts:2878-2899) — failures are recorded in the DB but the user never sees a notice. OpenCode proactively notifies (compartment-runner-incremental.ts:471-482 → 155-162). Use the same notice wording/framing OpenCode uses (buildHistorianFailureNotice — transient reframe) delivered via Pi's sendCtxStatusMessage path.

Fix: wire notifyIssue at the Pi live-historian call site (and the wrapup call site if it also omits it), matching OpenCode's throttling semantics (only notify on repeated failure / the same conditions OpenCode uses — check the OpenCode call site's gating so Pi doesn't become NOISIER than OpenCode).

## Finding 4: /ctx-embed start has no progress reporting on Pi

OpenCode reports live embed progress (hook.ts:441-448, 452-463, 474-481). Pi's /ctx-embed start awaits runEmbedDrain and sends only a terminal message (commands/ctx-embed.ts:158-164; runEmbedDrain calls embedSessionCompartmentChunks WITHOUT onProgress at :48-54). Long backfills look hung.

Fix: thread onProgress through Pi's runEmbedDrain and emit periodic progress via sendCtxStatusMessage — throttle to at most one message per N compartments or M seconds (pick sensible values; do NOT spam a message per chunk — Pi progress is chat-visible, unlike OpenCode's sidebar). A start message ("embedding X compartments...") + throttled progress + terminal summary is the right shape.

## Tests (non-vacuous)

- Fallback: configured model fails validation → chain reaches the session model; session model == configured model → no duplicate attempt.
- Transient retry: first attempt throws a 429-class error, second succeeds → run succeeds with 1 retry, same model; non-transient error → immediate fallback advance; abort → no retry.
- Notify: a failing run invokes the notify path with the transient framing; gating matches OpenCode (no notice storm on every pass).
- Embed progress: a multi-chunk drain emits start + throttled progress + terminal; single-chunk emits start + terminal only.

## Gates

cd packages/pi-plugin && bun test --timeout 60000, bun run typecheck, repo-root bun run lint, check_comments. Do not modify packages/plugin behavior. Commit with trailer: `Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>`
