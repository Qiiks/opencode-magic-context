# GitHub issue #401 — Pi drop-liveness investigation

Date: 2026-08-31

Issue: `cortexkit/magic-context#401`

Reported version: `0.41.0`

Client: Pi

## Verdict

The comparison identified in the report is not inverted. It belongs to the historian trigger, not the pending-drop executor. It asks whether cheaper, already-queued reclamation is projected to put the session below the post-drop target; if so, starting a summarizer in the same pass would be redundant. The comparison entered this repository in `f41ebbf11bd1d9aa47a52fedb775d8780a564096` (`feat: extract magic-context into standalone OpenCode plugin`, 2026-03-17). Its original inline intent was: “Force at 80% — only skip if drops alone bring usage well below the relative target.” `6d604b6e314862df1ad6f38ae729bafb390f6350` later broadened the estimate to automatic drops specifically to “prevent premature historian firing when auto-cleanup alone would free enough space.” The current implementation retains that division at `packages/plugin/src/hooks/magic-context/compartment-trigger.ts:613-637` and `:736-756`.

The two quoted samples do not prove a permanent drop-executor wedge or a stale-tag double count:

1. Pi evaluates the historian only **after** its synchronous drop pipeline (`packages/pi-plugin/src/context-handler.ts:2852-2913`, then `:2965-2982`).
2. The `79.2%`/`79.6%` historian samples are provider pressure from the preceding assistant usage record, not a recount of the just-mutated outgoing message array (`context-handler.ts:3799-3894`). They therefore cannot establish that the current pass failed to mutate the wire.
3. The second sample follows a `ctx_reduce` tool call. A Pi tool loop is classified as mid-turn (`packages/pi-plugin/src/read-session-pi.ts:147-190`), and a base `execute` decision is deliberately downgraded to `defer` below the force band (`packages/plugin/src/hooks/magic-context/boundary-execution.ts:34-47`; Pi wiring at `context-handler.ts:2627-2659`). Such a pass queues more drops, lowers the projection, and leaves the prior usage sample unchanged. This is the leading explanation for the two supplied lines, but the omitted boundary and pending-op log lines are required to convict it.
4. On the next non-mid-turn pass, pressure remains above `T=70`, so the scheduler returns `execute` (`packages/plugin/src/features/magic-context/scheduler.ts:82-94`) and the deferred work is eligible to drain. The existing Pi integration contract explicitly covers release on a fresh user turn (`packages/pi-plugin/src/boundary-execution-pi.test.ts:76-100`).

There is not enough session evidence to convict a durable liveness defect. The three primary candidates adjudicate as follows:

| Candidate | Source verdict | What the excerpt establishes |
|---|---|---|
| **A. Drain-gate wedge** | **Plausible for these passes, not proven durable.** Mid-turn deferral can eat every pass in a long tool loop; protected newest tags can remain queued across later execute passes. An in-flight historian is excluded on the quoted passes because Pi would log `in-flight, skipping` instead of evaluating the redundancy condition. | Projection fell after `ctx_reduce` while the sampled usage rose, which is consistent with a queue growing on a deferred pass. The missing `[boundary-exec]` and `pending ops WILL ...` lines decide it. |
| **B. Stale projection** | **False for already-dropped/status-retained tags; possible eligibility optimism for protected pending tags.** Pi passes an active-only post-apply tag set, so applied drops and retained token counts cannot stay in the projection. Protected active pending tags are still projected before they become executable. | A falling projection proves more active mass was considered reclaimable; it does not show whether that mass was protected, pass-deferred, or stale. |
| **C. Usage-basis divergence** | **False in the reporter's source revision.** Commit `5acc0014` contains the #385 alignment commit `73c9c639`, and both scheduler and historian route the same persisted/live numerator through `resolvePiPressureSnapshot` over the same `usableSoft` limit. | The `[session_meta]` label names the selected persisted source before the shared live floor/redivision; it is not a separate historian-only percentage basis. A scheduler log below 70% beside a historian log at 79% for the same pass would contradict current source and indicate a runtime/version/config anomaly. |

No runtime patch is proposed from the supplied evidence. Changing the historian comparison would start an expensive summarizer precisely when the cheaper reclaim queue says it can provide enough headroom, contrary to the recovered design intent. The scoped self-healing belt below is justified as follow-up design, but implementing it without the missing pass evidence would be speculative.

## Gate-by-gate Pi drain trace

At `T=70`, the scheduler's base decision is `execute` whenever the resolved pressure is at least 70% (`scheduler.ts:82-94`). Every gate between that decision and pending-op persistence/application is below.

### 1. Harness mode

`runPipeline` exits to the compaction-off pipeline when compaction is disabled (`packages/pi-plugin/src/context-handler.ts:4404-4408`). The normal pending-op lane also checks `!args.compactionOff` before loading work (`:4794-4803`). This can persist indefinitely only when the user explicitly runs compaction-off mode; the normal `ctx_reduce` surface is documented as unavailable in that mode, so it does not fit the quoted normal trigger logs.

### 2. Mid-turn boundary deferral and `deferred_execute_state`

Pi computes the base scheduler decision, classifies the event with `isMidTurnPi`, and applies the shared deferral rule (`context-handler.ts:2526-2543`, `:2627-2659`). A latest assistant tool call or unpaired tool result keeps `isMidTurnPi` true (`read-session-pi.ts:147-190`). Below the derived force band, an execute pass becomes defer and records `deferred_execute_state` (`context-handler.ts:2650-2657`).

That flag is intentionally drain-on-success only; it does not promote a later defer decision (`context-handler.ts:2660-2671`). This is safe because the threshold decision is idempotent: while usage stays at 79%, the next non-mid-turn pass again decides execute. Mid-turn can hold for many consecutive **context passes** during one long tool-using turn, but it cannot by itself hold for many consecutive completed user turns. A genuine user message after the previous assistant makes `isMidTurnPi` false, as pinned at `boundary-execution-pi.test.ts:76-100`.

At `T=70`, the force band is 85%, not 72%, because `escalationBands` uses `max(85, T+2)` (`packages/plugin/src/shared/escalation-bands.ts:9-17`). Thus 79.6% does not bypass mid-turn deferral.

### 3. Bust-opportunity gate

Pending ops are authorized by one of: effective scheduler `execute`, force materialization, explicit pending-materialization/flush, or an m[0] hard fold that actually executed (`context-handler.ts:4823-4841`). `historyRefreshSessions` is not itself the pending-op gate; it controls history injection rebuilding. A deferred publication may also authorize the pass, but only when the late-consumption gate sees execute/force pressure (`:4501-4520`, `:4835-4841`).

At 79% and outside mid-turn, scheduler execute is a valid bust opportunity every pass. During mid-turn, the effective decision is defer and ordinary queued drops remain cache-stable.

### 4. In-flight historian veto (`compartmentRunning` equivalent)

The shared OpenCode postprocess calls this condition `compartmentRunning`. Pi has a separate pipeline and uses `inFlightHistorian.has(sessionId)` (`context-handler.ts:4633-4652`, `:4839-4841`). A normal execute/deferred drain is vetoed while that promise is live. Force materialization (85% for `T=70`) or a successfully persisted hard fold bypasses the veto.

This gate can be effectively always on for many closely spaced passes while one historian run is active. A Pi historian attempt has a 600,000 ms default timeout (`packages/pi-plugin/src/pi-historian-runner.ts:133-135`), and a configured fallback chain is tried serially (`pi-historian-runner.ts:923-985`). Each child attempt has the same per-attempt timeout (`:947-968`). A provider rejection that emits a terminal error is normally much shorter: Pi detects the terminal event and gives the child at most a 2,000 ms drain grace (`packages/pi-plugin/src/subagent-runner.ts:1551-1577`). `no_assistant` is fallback-eligible (`subagent-runner.ts:1849-1855`), so a reasoning-only fallback chain may keep the single outer run live for roughly the sum of each provider rejection plus up to two seconds of drain grace per candidate. A silent/hung candidate can instead consume its full timeout. No-assistant/provider-400 failures are not classified as transient by the outer historian retry helper, so the 2–3 second and 6–8 second transient backoffs do not multiply this particular failure (`pi-historian-runner.ts:141-183`).

Crucially, the quoted `historian trigger eval: usage=... checking trigger` line proves this veto was **not** active on those exact passes. `maybeFireHistorian` returns early with `historian trigger eval: in-flight, skipping` whenever `inFlightHistorian` is present (`context-handler.ts:3783-3786`). Because `runPipeline` runs before `maybeFireHistorian`, a pass that printed the quoted trigger evaluation had already traversed the pending-op lane with `historianRunning=false`.

### 5. Per-tag protected-window gate

Once a pass is authorized, `applyPendingOperations` protects the newest configured active tags (`packages/plugin/src/hooks/magic-context/apply-operations.ts:55-86`). A pending tag in that set is skipped and remains queued (`:88-90`). The tool description explicitly says newest tags stay queued until they age out (`packages/plugin/src/tools/ctx-reduce/constants.ts:1-10`). This gate can survive many passes or turns if fewer than the configured number of newer tags arrive. It cannot be distinguished from a pass-level defer using the two trigger lines alone.

The projection currently counts all active pending-drop tags, including protected pending tags (`compartment-trigger.ts:210-236`). That is not stale-status double counting, but it can be optimistic about **when** protected reclamation becomes available. If diagnostics show that all surviving projected mass is protected across repeated non-mid-turn execute passes, that is the best scoped liveness-fix candidate: project only pending tags eligible on the current pass, or make the redundancy skip age/failure-aware. Doing so needs a cross-harness policy decision because newest-tag protection intentionally promises deferred, not immediate, reclamation.

### 6. Target/application behavior

Pi reloads pending ops only on an eligible pass and applies them against targets built from that pass's transcript (`context-handler.ts:4791-4813`, `:4850-4888`). Protected targets continue; ordinary absent non-synthetic targets persist their dropped state and remove their op, while incomplete tool targets retry (`packages/plugin/src/hooks/magic-context/apply-operations.ts:88-176`). Exceptions preserve the pending signal and fail the transform rather than silently consuming work (`context-handler.ts:4902-4908`).

### 7. Channel, marker, contention, and lease state

History/materialization channels do not impose a second veto on ordinary pending drops. They can add an explicit materialization opportunity (`context-handler.ts:4639-4652`, `:4835-4841`). Compaction-marker coverage, append availability, CAS, and injection-contention checks occur later when draining deferred history state (`:5548-5638`); they can preserve deferred history/materialization signals but do not undo already-applied pending ops.

The compartment lease only decides whether a historian process may start. It is acquired before `runPiHistorian`, renewed while active, and released in `finally` (`context-handler.ts:3613-3631`, `:3711-3717`; lease primitives at `packages/plugin/src/features/magic-context/compartment-lease.ts:13-38`, `:53-58`). Lease contention does not gate the synchronous pending-op executor.

## Secondary check: v0.41.0 / PR #399 blast radius

Commit `10f80e58` first shipped in v0.41.0 and defaulted Pi historian temperature to `0.1` in both config resolution and runner destructuring. PR #399 verified live that OpenAI reasoning models can reject the parameter, while Anthropic thinking constrains it. The empty terminal response is classified as `no_assistant`, and every rejecting configured/session fallback sees the same inherited temperature. This is a real v0.41.0 historian regression and a plausible reason a reporter's historian repeatedly fails.

It is **not**, by itself, a between-run permanent pending-drop wedge:

- While the run promise is live, it vetoes normal drains as described above.
- Terminal failure clears `compartmentInProgress` in `runPiHistorian`'s `finally` (`pi-historian-runner.ts:1477-1490`).
- `spawnPiHistorianRun` then releases the lease and removes the in-memory promise (`context-handler.ts:3711-3717`).
- Persisted failure count/error/time are diagnostics and restart-recovery inputs (`packages/plugin/src/features/magic-context/storage-meta-persisted.ts:1845-1891`); they are not consulted by the pending-op gate.
- The historian-drain failure timestamp throttles emergency historian catch-up, not pending-op execution (`storage-meta-persisted.ts:907-917`).
- On the first subsequent non-mid-turn high-pressure pass, `runPipeline` executes before `maybeFireHistorian` can start another run. Therefore there is a drain window between terminal failure and retry.

A failing reasoning chain can still produce a high veto duty cycle during long active turns, especially with several fallbacks or a hung attempt. It composes with mid-turn deferral to starve many consecutive passes, but the quoted trigger-evaluation lines are themselves the gap in which the in-flight veto was absent. Those lines therefore do not establish “projection skip + historian failure = exact deadlock.” They remain consistent with the simpler same-turn deferral described above.

### Concrete self-healing belt recommendation

Do not couple the historian redundancy skip to historian failure count. That would start a summarizer already known to be failing and add provider load while doing nothing to make pending drops executable. Instead, make the skip conditional on observed reclaim progress:

1. Persist a small per-session redundancy-skip observation: evaluation sequence/time, projected post-drop percentage, actual pressure percentage, total pending count, projected pending bytes, protected projected bytes, and whether the immediately preceding transform was an eligible non-mid-turn execute pass.
2. Arm a strike only when `projected <= target`, pending projected bytes are nonzero, and the preceding pass was one on which pending ops should have been executable. Mid-turn defers, in-flight-historian vetoes, compaction-off passes, and failed transforms do not consume the liveness budget.
3. Clear the strike counter when pending count/bytes decreases or actual pressure falls by a conservative epsilon consistent with the promised reclaim. Increment it when two consecutive eligible observations show no queue progress and actual pressure is flat or rising.
4. At `N=2` consecutive eligible misses, disqualify the redundancy skip once and allow the historian to fire if its normal tail/boundary checks pass. Record a named `reclaim_projection_stalled` reason, then impose the normal in-flight/lease controls. Reset after a successful pending-op drain or historian publication.
5. Keep the force/emergency bands unchanged. The belt is an escape hatch below force, not a second emergency policy.

This design self-heals all three proposed deadlock shapes: a hidden drain veto eventually stops consuming budget and exposes two eligible misses; protected/optimistic projection records the protected mass and escapes once its promise does not realize; and any future basis regression appears as flat/rising canonical pressure across eligible misses. It also avoids treating ordinary same-turn deferral as a liveness failure. Cross-harness tests should mutate both directions: (a) stale/overpromising projection at `T < usage < force` must yield a reclaim path within two eligible passes, and (b) real pending progress on either pass must keep the historian suppressed.

## Projection accounting adjudication

`estimateProjectedPostDropPercentage` receives active-only tags and computes:

- denominator: `byteSize + reasoningByteSize` for every active tag;
- queued-drop numerator: the same mass only for active tags whose tag numbers remain in `pending_ops`;
- reasoning numerator: uncleared, old-enough message reasoning, excluding queued-drop tags;
- result: `usage.percentage × (1 - droppableBytes / totalActiveBytes)`, clamped at 100% reclaim.

Source: `packages/plugin/src/hooks/magic-context/compartment-trigger.ts:210-266`.

On Pi, `result.activeTags` is reloaded after pending operations have run (`packages/pi-plugin/src/context-handler.ts:5061-5069`) and then passed to the trigger (`:2977-2981`, `:4015-4033`). `applyPendingOperations` changes applied tags to `dropped` and removes their pending rows (`packages/plugin/src/hooks/magic-context/apply-operations.ts:159-175`). Consequently:

- already-dropped tags are not in the active denominator;
- already-applied drops are not in the pending numerator;
- retained token/byte accounting on a dropped DB row does not make this projection count it;
- pending protected tags remain active and queued, so they are projected even though they are not executable yet.

Candidate B as stated—already-dropped tags or retained `token_count` being counted forever—is false in the TypeScript/Pi path. The only source-supported projection optimism is eligibility timing for protected pending tags, not stale status.

## Candidate C: Pi usage-basis alignment

The scheduler and historian use the same canonical pressure basis introduced by `73c9c639` for #385: `max(session_meta.last_input_tokens, ctx.getContextUsage().tokens) / resolvePiWindowGeometry(...).usableSoft`.

The read sites are:

- main transform reads `session_meta.lastContextPercentage/lastInputTokens` or the first-turn Pi fallback at `context-handler.ts:2385-2405`, resolves `usableSoft` at `:2477-2482`, then calls `resolvePiPressureSnapshot` at `:2509-2515`; the resulting pair goes directly to `scheduler.shouldExecute` at `:2526-2536`;
- historian rereads the same session-meta pair or first-turn fallback at `:3799-3884`, resolves the same Pi usable limit at `:3839-3843`, then calls the same `resolvePiPressureSnapshot` at `:3885-3890`;
- shared resolver chooses the larger persisted/live numerator and divides once by `usableContextLimit` (`packages/pi-plugin/src/pi-pressure.ts:107-152`).

The reporter-linked revision `5acc00142d453d78cceb4c92ddcc354c98d095d4` contains `73c9c639`, whose explicit intent was to remove Pi's hidden `0.85` denominator and route “scheduler, historian, logs, status, and footer through one token/window snapshot.” The alignment test pins the same token/window pair through scheduler and log consumers (`packages/pi-plugin/src/pi-pressure-alignment.test.ts:41-96`). The issue's own pairs independently identify that basis: `195902 / 0.792 ≈ 247351` and `196841 / 0.796 ≈ 247288`, both rounding around the approximately 247.4k usable-soft window pinned by the #385 specimen—not a second raw or safety-scaled denominator.

Candidate C therefore does not hold in that source. The historian log labels `[session_meta]` because that was the persisted source selected before the shared live floor/redivision; it is not a separate historian-only percentage basis. First-turn fallback can differ only before a valid assistant usage sample exists, which does not fit `[session_meta]`. If a doctor bundle nevertheless shows `transform ... usage < 70 decision=defer` and `historian trigger eval: usage=79.x% [session_meta]` for the same context event, that is evidence of a stale installed build, mixed plugin copies, per-project config resolution mismatch, or an unmodeled runtime race—not the checked-in 0.41.0 algorithm. The canonical fix in that event is to move every divergent consumer back to `resolvePiPressureSnapshot` with the `usableSoft` result; the required sites are the main scheduler call, historian trigger input, status detail, footer, and transform log, matching #385's precedent.

## Diagnostics that discriminate the remaining cases

Request `npx @cortexkit/magic-context@latest doctor --issue` and select the affected session. The sanitized issue bundle includes config—including the historian model/fallback chain—and the recent Pi log (`packages/cli/src/lib/logs-pi.ts:53-122`). For #401 the decisive lines are:

1. scheduler and boundary pass class: `transform: usage=... decision=...` plus `[boundary-exec] base=... midTurn=... effective=...`;
2. pending-op count and application outcome: `pending ops WILL APPLY/WILL NOT APPLY ... pendingOps=N`;
3. drop application timestamp/order: the timestamped apply line followed by the next pressure sample and transform decision;
4. historian lifecycle: `historian[...] spawned`, terminal/child-exit timing, `historian trigger eval: in-flight`, recorded failure count/error, and fallback model labels;
5. whether the configured historian or every fallback is a reasoning/thinking model affected by the v0.41.0 temperature regression.

The candidate signatures are:

- **A / pass veto:** historian 79.x%, main transform also 79.x%, but boundary shows `base=execute midTurn=true effective=defer`, or `pending ops WILL NOT APPLY`; an in-flight veto instead shows `historian trigger eval: in-flight, skipping` and no redundancy evaluation on that pass.
- **A / protected tags:** repeated non-mid-turn `decision=execute` and `pending ops WILL APPLY` while the positive pending count and projection remain stable. A narrow follow-up should compare pending tag numbers with the newest protected-tag cutoff; public diagnostics should not include raw prompt content.
- **B / stale status:** `pending_ops` falls or tags become dropped while the next trigger's active pending mass/projection does not change. Current source should make this impossible; observing it points to installed-build or DB-state corruption.
- **C / basis divergence:** the main transform logs `<70% decision=defer` while the historian in that same event logs approximately `79% [session_meta]`. Current aligned source should log the same canonical token/usable-soft pair at both sites.

The two issue lines alone contain only the historian percentage and projection. They omit the exact fields needed to select A, protected projection optimism, B, or C.

## Reply draft

Thanks for the concrete timestamps and for tracing the condition. The edit is a reasonable reading because that log line does not currently name which subsystem is skipping. The `projected post-drop <= target` check is the **historian/summarizer trigger's redundancy skip**, not the drop executor: it means “queued drops are projected to reclaim enough on the next eligible cache-busting pass, so do not also start the more expensive historian on this pass.” The drop executor is earlier in Pi's context pipeline and is driven by the scheduler/boundary decision. We recovered that intent from `f41ebbf1` (“only skip if drops alone bring usage well below the relative target”), with `6d604b6e` later extending the estimate specifically to avoid premature historian runs when cheaper cleanup suffices. Reversing the comparison would therefore make the historian run when drops are most sufficient.

The stuck-at-79% symptom is still important. In the two quoted lines, the second evaluation follows `ctx_reduce`; on Pi that tool-call context is normally a mid-turn pass, and below the 85% force band a 79.6% base-execute is deliberately deferred until the next turn boundary to preserve the provider cache. The logged 79.6% is also the preceding assistant's `[session_meta]` usage sample, not a post-transform recount, so it can rise while the newly queued projection falls. That is the leading explanation for these two samples, not yet a conviction about later turn boundaries. Other primary checks are newest-tag protection, stale installed/DB state, and a scheduler/trigger basis mismatch; current 0.41.0 source rules out the last mismatch by routing both through the same token/usable-soft snapshot, but the runtime logs should confirm it.

As a secondary check, there is also a v0.41.0 Pi historian regression under review in PR #399: if your configured historian chain uses reasoning/thinking models, the plugin's implicit `temperature: 0.1` can make those requests fail (often surfaced only as `no_assistant`). That cannot be the mechanism shown in your excerpt: while a historian is in flight Pi logs `historian trigger eval: in-flight, skipping`, and a terminal failure logs the historian failure/count, whereas your lines show a never-firing trigger evaluating and declining on projection. It could still be an additional problem once the historian eventually fires. Could you share which historian model/fallbacks you use?

Please run `npx @cortexkit/magic-context@latest doctor --issue` and attach the sanitized output for this session. The bundle will show the configured historian chain, scheduler decisions, boundary pass classes (`midTurn`/effective execute or defer), pending-op counts, drop-application timestamps, and historian spawn/failure timing. Those fields will tell us whether this was repeated same-turn deferral, protected newest tags, stale installed/DB state, a runtime-only pressure mismatch, or an unexpected executor failure. If the chain is reasoning-only, PR #399's separate v0.41.0 regression may also affect later historian attempts; its fix is already in review.

## v0.41.1 observability follow-up

The follow-up keeps the scheduler and drop behavior unchanged while making the diagnostic surfaces explicit:

- Historian redundancy skips now identify the summarizer as the subsystem that declined to run, retain the usage/force-band, projected, and target percentages, and say that queued or automatic reclaim is expected on the next eligible execute pass.
- The shared boundary decision carries a resolved `deferReason` into OpenCode postprocess and Pi. A base `execute` downgraded by the mid-turn boundary is logged as `mid_turn_boundary`; a genuine below-threshold base defer remains `scheduler_defer`.
- Pending-op `WILL APPLY` and `WILL NOT APPLY` lines use a durable `COUNT(*)` queue depth. If that diagnostic read fails, they emit `not loaded (deferred pass)` rather than presenting an unloaded array as `pendingOps=0`.
- No Rust twin of either user-facing log line exists in the current module, so no Rust wire or diagnostic output changed.

### Mutation evidence

Each guard was deliberately reverted once, the relevant test was run, and the original implementation was restored before this report was amended:

| Mutation | Diagnostic guard | Executed check | Recorded failing assertion |
|---|---|---|---|
| Replace carried defer reasons with the constant `scheduler_defer` | Mid-turn downgrade must report `mid_turn_boundary` | `bun test packages/pi-plugin/src/context-handler.test.ts` | `packages/pi-plugin/src/context-handler.test.ts:174` |
| Replace durable queue depth with deferred-pass array length | A queued durable operation must not be reported as `pendingOps=0` | `bun test packages/pi-plugin/src/context-handler.test.ts` | `packages/pi-plugin/src/context-handler.test.ts:174` and `:232` |
| Restore both anonymous historian redundancy-skip strings | Both skip sites must identify the historian and next eligible execute pass | `bun test packages/plugin/src/hooks/magic-context/compartment-trigger.test.ts` | `packages/plugin/src/hooks/magic-context/compartment-trigger.test.ts:34` |

The queue-depth mutation produced the observed lie directly: the refusal lines became `pendingOps=0` while the durable queue contained one row. The restored focused checks passed, confirming the tests are non-vacuous against all three guarded regressions.
