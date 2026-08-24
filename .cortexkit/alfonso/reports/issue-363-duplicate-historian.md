# GitHub #363 — duplicate Historian investigation

**Verdict:** the supplied occurrence is two **serial** Historian attempts after an empty-output validation failure, not two overlapping trigger-fired producers. The running plugin was cached as **0.38.1**, not 0.40.0; that is material because 0.38.1 predates the per-harness Historian model wiring introduced in 0.39.0. No source change is warranted from this report.

## Evidence inventory

All fields from the `doctor --issue` bundle were considered:

- Reporter/environment: macOS darwin arm64, Node 26.3.0, OpenCode CLI 1.18.18; Desktop settings and `/Applications/OpenCode.app` were also detected. The plugin is registered in both OpenCode and TUI config.
- Requested plugin version is 0.40.0, but the active `@latest` cache is `~/.cache/opencode/packages/@cortexkit/opencode-magic-context@latest` with **cached 0.38.1** and latest 0.40.0. The log separately warns that `@latest` leaves the active package unpinned and can be deleted mid-session.
- Config locations are `~/.config/opencode/opencode.jsonc`, `~/.config/opencode/tui.jsonc`, and `~/.config/cortexkit/magic-context.jsonc`; all parse successfully and have no detected conflict. The config sets both OpenCode Historian and Dreamer to `lmstudio/qwen/qwen3.8-27b`, Pi equivalents to qwen3.5-9b, disables Sidekick, and enables Git indexing. Magic Context compaction is on; native compaction auto/prune are off. Every boot's resolved-config fetch failed and used file-based compaction detection, so `opencode debug config` remains authoritative for the server's resolved config.
- Storage exists at `~/.local/share/cortexkit/magic-context`, with a 2.2 MB `context.db`. No recent OpenCode sessions were readable at diagnostic time; there were no project or legacy historian dumps (the legacy temp directory count is zero), no recorded durable `historian_runs` (or the schema predates v24), no current session-meta failure records, and no recent error-shaped log lines. The log was present (20 KB).
- The bundle shows multiple plugin boots and separate RPC servers at 15:56:00, 15:56:41, and 16:08:38. Each detected another RPC server and selected a new port. This is evidence of multiple OpenCode/plugin processes, not evidence that they won a Historian run concurrently.
- The only historian failure signal is session `ses_fd2e5a199ffeOeNO6K60sW9BVr`, chunk 19–29, `Historian returned no assistant output.`, with failure count 2. Its stated fallback model is the same local Qwen model and `twoPass=false`.

## The reported pair, reconstructed

The timestamps settle the reported pair:

| Time | Log evidence | Meaning |
| --- | --- | --- |
| 15:57:43.893 | `historian: creating child session (agent=historian, model=agent:historian)` | First child starts. Its child events resolve to `lmstudio/qwen/qwen3.8-27b`. |
| 16:07:43.919 | `historian: prompt completed (attempt 1/3)` | The first prompt returned; it was not still in flight. |
| 16:07:43.933 | `KEEPING ... (failed)` then `retrying historian with lmstudio/qwen/qwen3.8-27b (session-model last resort 1/1)` and a new child creation | Output extraction/validation found no usable assistant output and explicitly initiated the second child. |
| 16:17:43.964–16:17:43.987 | Second `prompt completed (attempt 1/3)`, then the same empty-output validation failure and failure count 2 | Second serial attempt also returned no usable output. |

The ten-minute intervals match the configured default prompt budget (`packages/plugin/src/config/schema/magic-context.ts:19`, 600,000 ms) but are **not** timeout retries: the log says `prompt completed`. A timeout follows `promptWithTimeout`'s abort path (`packages/plugin/src/shared/model-suggestion-retry.ts:170-216`), force-stops the server-side child with `POST /session/{id}/abort` (`:219-239`), and is non-retryable (`:241-270`). The supplied log has none of those timeout/abort messages.

**Severity: High user-visible cost, no duplicate publication observed.** A slow local producer made each failed attempt cost about ten minutes, and the retry looked like a duplicate prompt. It did not create two simultaneous producers for this instance.

## Trigger → runner single-flight

**Severity: protected for normal operation; one operational weakness is masked by the DB lease.**

1. Each transform reads durable `session_meta` from SQLite (`packages/plugin/src/features/magic-context/storage-meta-session.ts:78-110`). The transform gate requires `!sessionMeta.compartmentInProgress` before evaluating a trigger (`packages/plugin/src/hooks/magic-context/transform.ts:1661-1667`), and `checkCompartmentTrigger` repeats that guard (`compartment-trigger.ts:433-439`). When fired, the transform writes `compartmentInProgress: true` before starting the runner (`transform.ts:1691-1698`). Thus this latch is DB-backed, not process-local.
2. The same process additionally uses `activeRuns: Map<string, ActiveCompartmentRun>` (`packages/plugin/src/hooks/magic-context/compartment-runner.ts:31-35`). `startCompartmentAgent` checks and sets it synchronously (`:111-118`, `:164-181`), so repeated transforms in one Node/Bun process cannot start a second run. The map deliberately retains the real promise after a caller-side await timeout (`:153-156`); the 95% transform timeout merely proceeds without waiting (`transform-compartment-phase.ts:253-290`, `:421-437`). It cannot start a timeout retry while that promise is running.
3. A transform may execute many times while a slow local prompt is awaited, but its durable latch and local map prevent a normal re-fire. The runner marks the durable flag true at entry and clears it on completion/failure (`compartment-runner-incremental.ts:194`, `:930-943`).

There is a narrow **operational** seam: a losing process clears `compartmentInProgress` after failing lease acquisition (`compartment-runner.ts:129-140`). That permits later transforms to re-evaluate and attempt the runner while the winning process still owns the lease, but they skip at the lease; it is not a second child producer. A lease loss would need to coincide with a missed five-minute lease expiry/renewal window to progress further.

## Multi-process behavior

**Severity: medium operational risk, not the cause evidenced here.**

The active-run map is process-local, exactly as expected for separate OpenCode server/TUI processes. Cross-process exclusion instead comes from `compartment_state_lease`:

- The table is durable SQLite state (`packages/plugin/src/features/magic-context/storage-db.ts:957-964`).
- Acquisition is a single `INSERT ... ON CONFLICT ... WHERE holder_id matches OR expires_at <= now` statement (`compartment-lease.ts:13-37`), with a 5-minute TTL and 1-minute renewal (`:3-4`, `:40-51`).
- A loser **skips**, rather than queues, and logs `compartment lease held by another process` (`compartment-runner.ts:129-140`). The test suite exercises exactly one winner through two DB handles and two separate Bun subprocesses (`packages/plugin/src/features/magic-context/compartment-lease.test.ts:101-158`).

Therefore the multiple RPC-server observations can produce lease-loser attempts, but do not explain two Historian child sessions. The serial retry in the supplied timestamps explains them directly.

## Publication boundary and R12 fence

**Severity: high integrity protection; scope differs by path.**

For the Rust module/state-sync path, R12 is a true concurrent-publisher backstop, not merely a sync-during-run guard:

- `McStore::publish_historian_chunk` executes under one fenced transaction and verifies row-version CAS, firing sequence, producer run ID, selected-range identities, revert epoch, and the captured compartment-set generation before append (`crates/mc-store/src/lib.rs:11569-11695`).
- The generation check is explicitly inside that transaction (`:11660-11679`); range append then independently returns a typed overlap rejection (`:11681-11694`).
- The producer state machine treats overlap/fence rejection as a stale race and returns the firing to Idle without publishing (`crates/mc-module/src/historian.rs:545-577`). Tests cover a second publisher CAS conflict (`mc-store/src/lib.rs:21860-21922`) and a changed compartment generation rejecting a stale overlapping publish (`mc-module/src/historian.rs:4171-4250`).
- Plugin state sync preserves the delta and reports `retry_busy` when the historian owns that snapshot (`packages/plugin/src/hooks/magic-context/module-state-sync.ts:1848-1853`; test at `module-state-sync.test.ts:276-300`).

The ordinary OpenCode TypeScript incremental runner uses the separate legacy `context.db` table. Its publish transaction first verifies the same process's durable lease with `BEGIN IMMEDIATE` (`packages/plugin/src/hooks/magic-context/compartment-runner-incremental.ts:621-643`) before appending and committing (`:701-721`). That prevents a stale lease holder from publishing after a second holder wins. It does **not** use the Rust R12 predicate directly, and its schema only enforces `UNIQUE(session_id, sequence)`, not a range-overlap constraint (`packages/plugin/src/features/magic-context/storage-db.ts:890-913`; explicitly documented at `compartment-storage.ts:275-281`). For identical stale snapshots, the same sequence offset also makes a second append fail, but the lease is the intended cross-process publisher guard on this OC/TS path.

Thus two overlapping producers could at worst consume model time if a lease expires; normal publication allows only one holder. The supplied bundle has no compartment rows/telemetry showing two successful publications, so it is not evidence of the stronger data-integrity failure.

## Retry and fallback analysis

**Severity: confirmed cause of the visible double call.**

`runValidatedHistorianPass` runs a primary child; present-but-invalid output receives a repair child, while failed or missing output proceeds to the fallback chain (`packages/plugin/src/hooks/magic-context/compartment-runner-historian.ts:97-215`). The actual supplied failure occurs even earlier: `runHistorianPrompt` returns `Historian returned no assistant output.` after successful prompt completion (`:443-479`), and `runFallbackHistorianPass` creates a fresh child for each fallback (`:506-623`). This is serial by `await`, never a timeout-concurrent retry.

In actual 0.40 source, the reporter's exact configuration should avoid retrying the identical explicit primary as the session-model last resort:

- Per-harness config resolves `historian.opencode.model` into the primary model (`packages/plugin/src/shared/model-resolution.ts:104-120`) and hook construction forwards it (`packages/plugin/src/hooks/magic-context/hook.ts:353-364`, `:1091-1098`).
- The fallback builder drops a candidate whose model and qualifier equal the primary (`compartment-runner-historian.ts:551-569`).

The doctor bundle says the process instead ran cached 0.38.1. That release predates this per-harness model wiring (introduced for 0.39.0), which explains the first log's `model=agent:historian` rather than an explicit model override and why the same live-session Qwen model was retried as the last resort. The cache/version mismatch is therefore the most likely reason 0.40's same-model de-duplication was not active.

## Precise draft reply

> Thanks — the attached diagnostics identify the two calls as serial recovery attempts, not two overlapping Historian triggers. The first child started at 15:57:43, completed at 16:07:43, and returned no assistant output. Only then did Magic Context create the second child as the session-model last-resort retry; it also completed with no assistant output at 16:17:43. The ten-minute spacing is the prompt budget, but these are not timeout retries because both calls logged `prompt completed` rather than an abort/timeout.
>
> There is also a version mismatch: the doctor bundle reports `latest: 0.40.0` but the active `@latest` cache is `0.38.1`. That older runtime predates the 0.40 per-harness model de-duplication, so it can retry your same local Qwen model after an empty result. Please fully restart OpenCode until doctor reports cached 0.40.0, and pin the plugin to `@cortexkit/opencode-magic-context@0.40.0` instead of `@latest` while reproducing.
>
> If it still happens after the cache reports 0.40.0, please attach **one read-only artifact**: the output of `sqlite3 -readonly "$HOME/.local/share/cortexkit/magic-context/context.db" "SELECT subagent, provider_id, model_id, started_at, ended_at, status, error FROM subagent_invocations WHERE session_id = 'ses_fd2e5a199ffeOeNO6K60sW9BVr' AND subagent LIKE 'historian%' ORDER BY started_at;"`. That single timeline distinguishes a genuine overlap (second `started_at` before the first `ended_at`) from a completed-empty-output fallback like this report. Do not modify the database.

## Recommended issue disposition

Close/reclassify this occurrence as **stale cached runtime plus empty-output fallback** after the reporter confirms a restart on cached 0.40.0. Reopen as a concurrency bug only if the requested invocation timeline shows overlapping historian rows after that upgrade; then inspect lease-expiry/renewal logs and the `compartment_state_lease` row, rather than treating two sequential child sessions as duplicate publication.
