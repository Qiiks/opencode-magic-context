# Six-axis review: PR #350

- **PR:** [cortexkit/magic-context#350](https://github.com/cortexkit/magic-context/pull/350) — `fix(pi-plugin): support pi-web multi-session and RPC hosts`
- **Author:** [elrond298](https://github.com/elrond298) (Ke Cao) — first-time contributor (0 prior commits on this repo)
- **Head:** `elrond298/magic-context` @ `27f614b65b85cf2e9ef981688270dc2f7ac71023` (`fix/pi-web-compat` → `master`)
- **Scope:** 17 files, +685 / −257, almost entirely `packages/pi-plugin/`
- **Linked issue:** none on the PR. Closely related to [#247](https://github.com/cortexkit/magic-context/issues/247) (in-process `@gotgenes/pi-subagents` children OOM). No `Closes` / `Fixes`.
- **This document:** report-only. No merge, no GitHub posts, no push.

## Verdict

**Request changes.** Not a security-doctrine blocker.

The title’s “RPC hosts” does **not** widen Magic Context’s localhost RPC server. It means Pi’s RPC *mode* (`ctx.mode === "rpc"`) used by [pi-web](https://github.com/agegr/pi-web): notifications and modal dialogs through Pi’s own UI, not `packages/plugin/src/shared/rpc-server.ts`. Axis 2 (SECURITY) is therefore **pass** on the egress/exposure question this review was asked to treat as blocking.

The mechanism for the real pi-web seam — the process-global `#247` latch treating every second factory call as an in-process child — is the right diagnosis. The ALS + lifecycle-event replacement, once-per-process startup claim, Dreamer owner tracking, CLI discrimination, and `/ctx-dream` pre-registration are thoughtful. They are not yet enough to merge:

1. Dreamer registry is still module-local while the child guard was moved to `globalThis` because jiti `moduleCache: false` resets module state. Under that loading model, owner tracking cannot coordinate sibling sessions and each session starts its own Dreamer timer.
2. `session_shutdown` still drains *all* in-flight historians/recomps/dreamers in the module and was written assuming “shutdown ⇒ process is leaving.” That is false in pi-web.
3. RPC presentation captures `ctx` from `session_start` onto the `pi` object; a later start/switch overwrites it.
4. The Windows `#177` “never spawn bare `pi`” assertion was rewritten away rather than kept beside the new embedded-host test.
5. `packages/pi-plugin/PARITY.md` is not updated, despite new divergences (RPC dialogs; multi-session process model).
6. GitHub Actions CI/Smoke are `action_required` (first-time contributor). They have not run.

Do not merge until the must-fix list below is addressed and CI is approved and green.

---

## What this PR gets right

Credit first. This is a serious first contribution, not a drive-by.

- **Correct seam.** Today `PI_ACTIVE_LATCH` on `globalThis` makes the *second* Magic Context factory call in a process a no-op. That was the right hammer for `#247` (in-process `@gotgenes/pi-subagents` children re-importing the extension and fan-out `SessionManager.listAll()` → heap OOM). It is the wrong hammer for pi-web, where several independent `AgentSession`s share one Node process and each must initialize. Replacing “already active in this process” with “this async context is an in-process child” is the actual fix.
- **Uses the lifecycle `#247` itself described.** The issue notes that pi-subagents 18.x emits `subagents:child:session-created` immediately before `bindExtensions()`, and that `MAGIC_CONTEXT_PI_SUBAGENT=1` cannot mark in-process children because `process.env` is process-global. Listening on those events (and *not* on `spawning`, which would leak into the parent call chain) matches that contract. The spawned-child env guard is left in place.
- **Startup maintenance is claimed once per process** via another `Symbol.for` on `globalThis`, and session-history listing is deferred until the durable backfill lease is acquired. That is the right response to “N sessions must not N-scan JSONL.” The lazy `SessionProjectBackfillSource` API already existed; this PR uses it.
- **CLI discrimination is a real safety fix.** Reusing `process.argv[1]` whenever it exists on disk is how a Next.js/pi-web host would spawn another copy of itself as a “subagent.” Restricting reuse to `@earendil-works|oh-my-pi/pi-coding-agent/dist/cli.js` is the right instinct. Expanding generic-runtime matching to `node24` / `nodejs` is also correct.
- **Dreamer sibling ownership** is the right *shape* of fix: one session’s `session_shutdown` must not unregister a same-project sibling, and `/ctx-dream` must work before the first `before_agent_start`. Tests for sibling survival, worktree handoff, and `ensureRegistered` are present.
- **RPC command presentation** keeps model-invisible `ctx-status` entries (the PARITY.md #7 / wrapup rule) and only *adds* a presenter for Pi RPC UI. Progress stays on `notify`; detailed results go to `ctx.ui.custom` with a notify fallback. That is careful about not leaking status text into model context.
- **Tone and tests.** The author ran `bun run --cwd packages/pi-plugin test` locally (803 passed, claimed) and added regression coverage for the new contracts instead of only deleting the old latch tests.

---

## Axis 1 — Mechanism

**What breaks today for pi-web**

| Symptom | Cause |
|---|---|
| Second (and later) browser sessions never get Magic Context | `isPiMagicContextActiveInProcess()` no-ops every factory call after the first in the process. Independent pi-web sessions look identical to in-process subagent children. |
| `/ctx-dream` before the first model turn fails with “not registered” | Dreamer registration ran at boot from `process.cwd()` and again only in `before_agent_start`. A manual command in a fresh RPC session can run first. |
| Historian/dreamer spawn inside pi-web can re-exec the web server | `resolvePiInvocation()` treated any on-disk `argv[1]` as the Pi CLI (`#177` host re-invoke). In an embedded host that path is Next.js / pi-web, not `cli.js`. |
| Slash-command output is invisible or wrong in the web UI | Pi RPC mode has no TUI entry renderer in the same way; status lived on `appendEntry` + terminal notify. |
| Duplicate JSONL scans / v22 backfill if the latch were simply removed | Each full init scheduled `listSessions()` + `runDeferredV22Backfill`. That is exactly the `#247` OOM multiplier. |

**Does the fix address the real seam?**

Yes, for the latch. The ALS marker + `session-created` / `disposed` events is the right replacement *if* those events fire in the child’s async context before `bindExtensions()`. Issue #247 states that they do, for `@gotgenes/pi-subagents` 18.x.

The PR does **not** change RPC port discovery or Magic Context host binding. Those were not the pi-web breakage. Session identity is still Pi’s `sessionManager.getSessionId()`; this PR does not invent a new session key.

Residual mechanism gaps (expanded in axis 3):

- Dreamer `registeredProjects` is not on `globalThis`, unlike the child marker that was moved there for the same jiti reason.
- RPC presenter is a side-channel on `pi`, not on the command’s live `ctx`.
- `session_shutdown` comments and drains still assume process exit.

---

## Axis 2 — SECURITY (highest priority)

**Question asked:** our RPC server binds localhost with bearer tokens (`rpc-server.ts`). Does this PR widen binding to non-loopback hosts? If yes, what auth gates it, is it opt-in via user-tier config only, and does it leak the token or DB paths across the network? Any widening without explicit user-tier opt-in + auth is **BLOCKING**.

**Finding: no widening. Not blocking.**

Evidence:

- PR file list does not include `packages/plugin/src/shared/rpc-server.ts`, `rpc-utils.ts`, `rpc-handlers.ts`, or any dashboard serve/bind code.
- `packages/pi-plugin/` has **zero** references to `MagicContextRpcServer` / `rpc-server`.
- `MagicContextRpcServer.start()` on current master still hard-codes `hostname: "127.0.0.1"`, writes the bearer token to an owner-only port file, and logs `listening on 127.0.0.1:${port}`. Unchanged.
- “RPC hosts” in the title/body means Pi `ctx.mode === "rpc"`: `ctx.ui.notify` and `ctx.ui.custom` overlays. That is Pi’s session UI channel, already authenticated as part of the Pi/pi-web session, not a new listen address.

**pi-web’s own bind is out of this PR’s scope, but should not be confused with ours.**

[pi-web](https://github.com/agegr/pi-web) defaults to `127.0.0.1:30141` and documents `--hostname 0.0.0.0` as a remote-access footgun gated by `PI_WEB_PASSWORD` (HTTP Basic, not TLS). That is pi-web’s process, not Magic Context’s RPC server. This PR does not add a Magic Context flag to follow that hostname. Command status text (`/ctx-status`, etc.) does not include `context.db` paths; it would be visible to whoever already has the pi-web session.

**Other security notes (non-blocking):**

- CLI discrimination is security-*positive*: it prevents an embedded host from spawning itself with the user’s prompt on argv. Spawn remains `shell: false` (no cmd.exe injection). Good.
- Fallback after a non-Pi `argv[1]` is bundled `cli.js` then bare `pi`. Bare `pi` without a shell is the `#177` Windows ENOENT (availability), not an injection hole. Still a regression of a safety *test* (axis 5).
- `setStoragePrivatePermissionEnforcement` remains user-tier and process-wide. Multiple sessions in one process sharing it is consistent with “project config cannot alter it.”
- ALS `enterWith(true)` is not a sandbox. A missed child mark re-introduces the `#247` OOM (availability / DoS of the user’s own process), not an auth bypass.

**Egress/exposure doctrine:** no BLOCKING finding.

---

## Axis 3 — Multi-session correctness

The PR correctly drops “process == one session” for *initialization*. Several remaining maps still assume either one live factory or “shutdown means the process is dying.”

### 3a. Two loading models, one of which the PR already documents

The child-guard comment (kept and updated) says Pi’s jiti loader uses `moduleCache: false`, so **module-level state resets on every re-import**, which is why the latch had to live on `globalThis` via `Symbol.for`.

| State | Where it lives | After this PR |
|---|---|---|
| Child-init ALS | `globalThis` `Symbol.for` | Shared across jiti instances. Correct. |
| Startup-maintenance claim | `globalThis` `Symbol.for` | Shared. Correct. |
| Dreamer `registeredProjects` + owner map | module-level `Map` | **Not shared** across jiti instances. Owner tracking cannot see siblings. Each session that fully inits starts its own timer. |
| `inFlightHistorian` / `inFlightRecomp` / `inFlightDreams` | module-level | Isolated per jiti instance *or* shared if the factory is called N times on one module. |
| Todo snapshots | `snapshotsBySession` | Already session-keyed. Fine either way. |
| Channel-2 lease | SQLite `session_meta` keyed by `session_id` | Fine. Cross-process CAS already exists. |
| `openDatabase` handle | `@magic-context/core` module cache | Shared. Fine. |
| `setCtxReduceRegisteredGlobally`, `configurePiSubagentExtensions`, `setKeepSubagents` | core module cache | Last writer wins. Pre-existing boot-resolved design; now amplified across pi-web sessions with different project configs. |

**Must-fix:** put the Dreamer registry on the same process-global footing as the child marker (a `Symbol.for` holder on `globalThis`), then keep the owner map. Otherwise pi-web with two sessions in one repo double-schedules Dreamer (duplicate LLM spend, duplicate memory writes, races on shared DB). The owner-tracking tests only exercise one module instance, so they cannot catch this.

If the author has evidence that pi-web *does not* re-import via jiti and instead calls the factory N times on one module, that belongs in a comment next to `registeredProjects` — and then 3b becomes the must-fix instead. The current code comments claim the jiti model.

### 3b. `session_shutdown` is no longer process-exit

Current handler (unchanged in spirit) waits up to 5s on **every** in-flight historian, recomp, and dreamer in the module, then unregisters Dreamer identities this instance has seen. Comments still say “before the process exits.”

In pi-web, closing one browser session must not:

- stall on another session’s historian (`awaitInFlightHistorians()` is `Promise.allSettled` on the whole map);
- tear down a sibling Dreamer timer (owner tracking tries to fix this, but see 3a).

**Must-fix:** drain only the shutting-down `sessionId` (the maps are already keyed). Keep the 5s cap.

### 3c. RPC presenter captures the wrong `ctx`

```ts
pi.on("session_start", async (event, ctx) => {
  if (ctx.mode === "rpc") {
    setCtxStatusPresenter(pi, (content) => {
      // closes over this start's ctx
      ctx.ui.notify(...)
      void showCtxStatusDialog(ctx, content)
    });
  }
```

`setCtxStatusPresenter` is a `WeakMap<PiMessageSender, presenter>`. One `pi` ⇒ one presenter. A later `session_start` or session switch overwrites it. Background `sendCtxStatusMessage(pi, …)` (auto-embed, etc.) then notifies whichever session started last.

Slash-command handlers already have a live `ctx`. Prefer presenting from that `ctx` (`ctx.mode === "rpc"`) at the call site. Do not stash UI `ctx` on the extension object. OpenCode’s PARITY.md #6 already warns that notifications must be session-scoped because one process serves many sessions.

### 3d. ALS `enterWith` is the documented footgun

Node’s own docs prefer `AsyncLocalStorage.run()`. `enterWith` is used because the factory is not wrapped by this code — only the lifecycle event is. That is defensible **if** `session-created` runs in the child’s async branch, as `#247` describes.

Residual risks (should-fix, not merge-blocking by themselves):

- No test with two *concurrent* child factory calls in separate async contexts (the original `#247` request).
- `enterWith(false)` on `disposed` vs `enterWith(true)` on a sibling child in a shared context could unmark the sibling. Isolated async resources should be fine; a shared parent tick is not proven.
- Production wiring is mocked (`pi.events.on`); there is no test against `@gotgenes/pi-subagents` event names remaining stable.

Please cite the pi-subagents 18.x “emit then `bindExtensions()`” contract in the comment so the next reader does not “simplify” it to `spawning`.

### 3e. Cubic P2 on Dreamer handoff

When the active owner leaves and remaining owners have **different** `projectDir`s, the loop `for (const remainingOptions of remaining) registerPiDreamerProject(remainingOptions)` rebuilds on every different directory, tearing down the timer it just started. Same-directory siblings no-op after the first and are fine. Re-register **one** remaining owner (the map already retains the rest) or make timer cleanup identity-aware. Valid P2.

### 3f. Process-global config last-writer

`compactionOff` / `setCtxReduceRegisteredGlobally` stay boot-resolved per factory call. Two pi-web sessions with different project `enabled` / compaction flags clobber the shared core flag. Pre-existing for `/cd`; now cross-session. Acceptable to document in PARITY.md rather than fully redesign, but do not pretend tools are per-session after this PR.

---

## Axis 4 — Migration / schema

**No schema or migration change.**

- No new tables, columns, or version bumps.
- v22 deferred legacy-memory rekey still runs; it is only claimed once per process.
- Session→project backfill still uses the existing durable lease in `runSessionProjectBackfill`. The implementation already accepted a lazy `SessionProjectBackfillSource`. This PR only switches the Pi caller to that form and extends one test so a second call with `already_completed` does not invoke the source (`sourceCalls === 0`).
- Shared `context.db` via `openDatabase` cache is unchanged. Multiple sessions in one process sharing one handle is the intended model.

No migration notes, no fence impact, no PARITY schema-fence change required beyond the process-model note in axis 6.

---

## Axis 5 — Test adequacy

Author-claimed local run: `packages/pi-plugin` **803 passed**, plus the backfill file **9 passed**. GitHub CI has **not** confirmed this (axis checks).

| Claim | Coverage | Gap |
|---|---|---|
| Independent same-process sessions initialize | `index-in-process-latch.test.ts` “registers independent sessions” | Sequential, not concurrent. Shared test DB (`delete XDG_DATA_HOME` to use preload DB) rather than isolated stores. |
| Only marked in-process children no-op | “skips only the marked in-process child” + mutation-direction test | Marker is set by emitting on the **parent** mock then calling the child factory in the **same** test async context. Does not prove ALS isolation across real child branches. |
| Lifecycle listeners removed on shutdown | yes | Good. |
| Startup maintenance claimed once | yes, through full runtime init | Good. |
| Lazy backfill skips source when lease done | `session-project-backfill.test.ts` | Good, and it tests the existing API rather than inventing a new one. |
| Dreamer sibling / worktree transfer | `dreamer/index.test.ts` | Same module instance only (see 3a). No test that two jiti-like imports share one timer. |
| `/ctx-dream` `ensureRegistered` | `ctx-commands.test.ts` | Asserts the hook ran with `ctx.cwd`. Does not assert a previously-unregistered project actually dreams. |
| Embedded-host CLI discrimination | unit regex + rewritten spawn test | See `#177` below. |
| RPC notify vs dialog | `pi-command-utils.test.ts` | Helper + `ctx.ui.custom` mock. No test that presenter state does not leak across two `session_start`s. |
| Host-binding refusal | n/a | Correctly absent: this PR does not bind a host. |
| Parallel in-process children (`#247`) | **missing** | The original issue asked for this. |

**Contract change wearing a rename’s clothes (`#177`):**

Old test: `with no piBinary override, spawns the host runtime + cli.js (Windows-safe, #177)` asserted `command === process.execPath`, `spawnArgs[0] === process.argv[1]`, and **`command !== "pi"`**.

New test: `with no piBinary override, does not re-run an embedded host` asserts `spawnArgs[0] !== process.argv[1]` and `command.length > 0`. The bare-`pi` assertion is gone.

That old assertion was a claim that Windows must not `spawn("pi")` without a shell (npm installs `pi.cmd`, Node looks for a file named `pi`, ENOENT). For an embedded host, falling through to bundled `cli.js` is correct **when it resolves**; when it does not, the last resort is still bare `pi`. Keep a **second** test for the real Pi CLI `argv[1]` path (host re-invoke, not bare `pi`) and a third that when `argv[1]` is not a Pi CLI and bundled CLI exists, spawn is `execPath + bundled`, never `"pi"`. Do not delete the Windows claim to make the embedded-host test green.

The `#247` latch test rewrite (second init no-ops → second init registers) **is** a justified contract change. The child-specific skip test remains. That one is fine; say so in the commit/PR text so reviewers do not treat it like `#177`.

Red-first: not visible from the PR (single commit). Not a reject reason.

---

## Axis 6 — Code fit + PARITY.md

Fit is generally good: small helpers (`isPiCliScript`, `claimPiStartupMaintenance`, `syncDreamerProjectRegistration`), existing backfill API reused, env guard preserved, `shell: false` preserved.

Nits:

- `registerPiSubagentInitContextCleanup` is a second `session_shutdown` handler whose only job is `unsubscribe`. Could be folded into the existing shutdown handler so lifecycle and drain stay in one place. Not blocking.
- `shouldShowCtxStatusDialog` treats every non-`info` level as a dialog unless `rpcDisplay === "notification"`. Presenter is RPC-only, so TUI is unaffected. Fine if documented.
- Copied “Council finding #7” comment on Dreamer embedding config is pre-existing, not introduced here.

**PARITY.md is required and missing.** The file’s own maintenance section: update it whenever a deliberate Pi↔OpenCode divergence is introduced or changed.

Needed updates:

1. **§6 Transient UI.** Today: “Pi uses `ctx.ui.notify` toasts, not persistent dialogs.” This PR adds `ctx.ui.custom` overlay dialogs in RPC mode for `/ctx-status`, `/ctx-embed`, `/ctx-recomp`, `/ctx-session-upgrade`, `/ctx-dream`. OpenCode already uses RPC dialogs; Pi RPC mode is now closer to OpenCode TUI, while interactive Pi TUI stays on entries + toasts. Record that split, and that RPC presentation must stay session-scoped (OpenCode already says this).
2. **§7 / §11b / §13 process model.** “Pi is a single-process REPL where the command handler IS the turn” and “session_shutdown … before the process exits” are no longer the whole story. pi-web: many `AgentSession`s, one Node process, RPC mode, shutdown ≠ exit. Interactive `pi` CLI is unchanged.
3. **§1 / `#247` latch.** The process-global “second init no-ops” divergence is replaced by an ALS child mark. In-process `@gotgenes/pi-subagents` children still skip full init; independent same-process sessions do not. Write that down so the next audit does not re-report the old latch as missing.

Without those notes, the next council/Oracle pass will flag RPC dialogs and multi-session init as accidental drift.

---

## GitHub checks

| Check | Result | Notes |
|---|---|---|
| CI | **`action_required`** | First-time contributor. Workflow `32444368710` did not start (0s). |
| Smoke (opencode bring-up on Linux) | **`action_required`** | Same. `32444368796`. |
| Greptile Review | pass (confidence 5/5) | No actionable defects. Treat as non-authoritative. |
| cubic · AI code reviewer | pass | One P2: Dreamer remaining-owner loop (axis 3e). |
| Socket Security (project + PR alerts) | pass | |
| [code]smith | skipped | Autofix disabled. |
| Human reviews | none | Only cubic commented. Issue comments: 0. |

Commit status API: `pending`, empty statuses. **There is no green test gate on this PR.** Author’s 803-pass claim is unverified until a maintainer approves the workflow.

---

## Change requests

Drafted for a first-time contributor. Must-fix vs should-fix vs nits.

### Must-fix (before merge)

1. **Process-global Dreamer registry.** Store `registeredProjects` (and the owner map) on `globalThis` via `Symbol.for`, the same way the child-init ALS and startup-maintenance claim already survive jiti re-imports. Add a test that two separately imported registries still share one timer for one project identity — or, if pi-web does not re-import, document that invariant next to the map and add a test that two factory calls on the *same* module share one timer (the sibling test already almost does this).
2. **Session-scoped shutdown drains.** `awaitInFlightHistorians` / `awaitInFlightRecomps` / `awaitInFlightDreamers` on `session_shutdown` should join only the outgoing `sessionId`. Update the comments that say the process is exiting.
3. **Do not capture RPC `ctx` on `session_start`.** Present from the command handler’s live `ctx` when `ctx.mode === "rpc"`. Background `sendCtxStatusMessage` should not notify a different session. Add a test with two `session_start`s that a status from the first session does not use the second session’s `ui`.
4. **Restore the `#177` Windows claim as its own test**, and keep the new embedded-host test as a second case. Do not replace “never bare `pi`” with `command.length > 0`.
5. **Update `packages/pi-plugin/PARITY.md`** for §6 (RPC dialogs), process model (§7 / §11b / §13), and the `#247` latch → ALS child mark.
6. **Maintainer: approve CI** for this first-time contributor and require CI + Smoke green.

### Should-fix

7. Cubic P2: when handing off Dreamer, re-register **one** remaining owner, not every remaining options object (different worktree dirs will serially teardown the new timer).
8. Parallel in-process child factory test (the `#247` request): two concurrent `AsyncLocalStorage` branches, only the marked one no-ops.
9. Cite `@gotgenes/pi-subagents` 18.x “`session-created` then `bindExtensions()` in the child async branch; never mark on `spawning`” in the ALS comment.
10. Mention in the PR body that this refines `#247` rather than replacing it, and that the old “second init in the process is a no-op” test was rewritten *because* that contract was the pi-web bug.

### Nits

11. Fold `registerPiSubagentInitContextCleanup` into the main `session_shutdown` handler.
12. Prefer not deleting `XDG_DATA_HOME` in the latch tests; isolate data dirs so two full inits cannot share the preload database by accident.

---

## Suggested maintainer reply (not posted)

> Thanks for this — especially as a first contribution. The diagnosis is right: the `#247` process-global latch is what makes the second pi-web session skip Magic Context entirely, and routing child suppression through `subagents:child:session-created` / `disposed` plus AsyncLocalStorage is the correct seam. Once-per-process startup maintenance, not reusing a non-Pi `argv[1]`, Dreamer sibling ownership, and keeping `ctx-status` entries model-invisible while presenting them in RPC are all the right instincts.
>
> Two clarifications so we don’t talk past each other:
> 1. “RPC hosts” here is Pi RPC *mode* (`ctx.ui.notify` / `ctx.ui.custom`). It does not change Magic Context’s RPC server, which must stay on `127.0.0.1` with a bearer token. We checked; this PR does not touch that. Thank you for not widening it.
> 2. The old “second init in the process is a no-op” test *should* change. That contract is the bug for pi-web. Please keep the child-only skip test (you did).
>
> Before we can merge, we need:
> - Dreamer `registeredProjects` on `globalThis` (same jiti reason as the child marker), so two sessions in one repo don’t start two timers.
> - `session_shutdown` draining only that session’s in-flight work — in pi-web, shutdown is not process exit.
> - RPC presentation using the command’s live `ctx`, not a `session_start` closure on `pi`.
> - The `#177` “never spawn bare `pi`” test kept *alongside* the new embedded-host test.
> - `packages/pi-plugin/PARITY.md` updated for RPC dialogs, the multi-session process model, and the latch → ALS change.
>
> CI hasn’t run yet because this is a first-time-contributor PR; we’ll approve the workflow once the above is in.
>
> Really solid work. Happy to re-review quickly after those.

---

## Reviewer notes (internal)

- Read-only review. `gh pr view 350`, `gh pr diff 350`, `gh pr checks 350`, `gh issue view 247`, pi-web README (bind defaults). No checkout of the fork, no local application of the patch, no test run of the PR branch.
- Base of this worktree is `f99ddc17` (current master). PR is one commit on top of master from the fork.
- Blocking doctrine for non-loopback RPC bind was applied; it did not fire.
- Greptile 5/5 was not used as a substitute for the six axes.
