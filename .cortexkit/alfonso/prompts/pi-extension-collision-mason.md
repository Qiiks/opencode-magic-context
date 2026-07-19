# Task: Pi subagent extension-collision retry (issue #222)

## Root cause (source-confirmed with the Pi team — do not re-derive)

Hidden Pi subagent children (`pi --print`, spawned by
`packages/pi-plugin/src/subagent-runner.ts`) load the user's Pi extensions (deliberate
since v0.30.4: extension-registered providers are the only source of some users'
models). In Pi print mode, extensions load BEFORE stdin is read; an extension that
starts a turn at load time (or from an `input` handler) makes the child's
`AgentSession.prompt()` throw:

    Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.

and the child exits code=1 without `agent_end`. Every model in the chain fails
identically (the collision is model-independent), so the historian wedges. Pi has no
print-mode flag to queue the initial prompt, and no per-extension disable. Reverting to
always `--no-extensions` is NOT acceptable (it regresses extension-provided models —
the reason v0.30.4 removed it).

## The fix: targeted one-shot degrade

In `packages/pi-plugin/src/subagent-runner.ts`:

1. Detect the collision signature on a failed run: non-zero exit AND stderr containing
   `Agent is already processing` (match on that stable prefix; do not require the whole
   sentence). Detection must be a named helper, not an inline string test.
2. When detected AND the spawn did not already use `--no-extensions`: retry the run
   ONCE with `--no-extensions` appended (same model, same prompt, same everything
   else). CRITICAL: the retry keeps the explicit MC lean `--extension SUBAGENT_ENTRY_PATH`
   already emitted by buildArgs (~line 1148) — Pi's resource loader semantics are
   "skip DISCOVERED extensions, keep explicitly provided --extension paths"
   (confirmed with the Pi team), so `--no-extensions` + our explicit entry gives
   isolation from user extensions without losing ctx_* tools in the child. This is a per-RUN retry that wraps the existing model-chain loop at the
   `runOnce` call level — NOT a new chain iteration (a collision is model-independent,
   so retrying other models with extensions on is pure waste; the existing chain
   fallback continues to apply INSIDE the degraded retry if the model itself then
   fails).
3. Log clearly on degrade, framed as extension interference (NOT model failure):
   `pi subagent: a loaded Pi extension started an agent turn before the child's prompt could run; retrying with an isolated extension set (user extensions disabled for this run)`.
   Use the session logger, one line, no stack. SECOND distinct log when the degraded
   retry then fails model resolution (model only existed via a disabled extension
   provider): report it separately — `model unavailable in isolated retry: it is
   provided by a disabled extension; configure it through models.json or add a
   built-in/provider-configured fallback` — keeping the two failure modes distinct
   in the run report and historian failure notice.
4. The degrade is per-run, not sticky: the next historian/dreamer run tries WITH
   extensions again (the collision may be intermittent, and a sticky latch would
   silently cost users their extension models forever).
5. Env-guard interplay: the degraded spawn still sets MAGIC_CONTEXT_PI_SUBAGENT=1 and
   all existing flags; `--no-extensions` composes with them (it did before v0.30.4).

## Tests (packages/pi-plugin/src/subagent-runner.test.ts or a new co-located file)
- Collision stderr + exit 1 → exactly one retry with `--no-extensions` in argv; success
  on retry → run reports ok, with the degrade logged.
- Collision on retry too (still failing) → normal failure path, no infinite retry.
- Non-collision failure (other stderr, exit 1) → NO extension-less retry (existing
  chain behavior unchanged).
- Already-`--no-extensions` spawn (if any caller passes it) with collision stderr → no
  retry loop.
- Degrade is not sticky: a second `run()` after a degraded run spawns WITH extensions
  again.
Use the existing spawn test seams/fakes in the runner tests — do NOT spawn real pi.

## Gates
bun test packages/pi-plugin (full), tsc, biome. check_comments before commit.

## Rules
- Base: subc-migration HEAD.
- Only packages/pi-plugin/. Update PARITY.md ONLY if you believe OpenCode needs a
  counterpart (it does not — OpenCode children are in-process sessions, no spawn).
- Comment the WHY (print-mode loads extensions before stdin; collision is
  model-independent; per-run not sticky because extension models must keep working for
  well-behaved setups). No issue numbers in comments.
- Commit trailer: Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>
