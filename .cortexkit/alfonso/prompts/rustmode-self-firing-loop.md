# Investigate + fix: rust-mode drive session self-fired ~200 identical turns (suspected channel2 directive loop)

Repo: this worktree (branch from `subc-migration` HEAD). Investigation-FIRST. The loop is DEAD (its TUI process exited); evidence is on disk. Do NOT start any opencode process against the real clone session until your fix is proven in unit tests — reproduce mechanisms from code + evidence, not by relaunching the loop.

## Evidence

Session `ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO` (transform_mode rust), 2026-07-17 ~23:18-23:45 UTC: ~200 near-identical Anthropic requests at ~8-10s cadence (dumps: /var/folders/18/257zzylx4h1gbkcvs4cnpqqc0000gn/T/opencode-anthropic-auth-dumps/*ses_l7l9*, sequence numbers 00016..00205), each prompt=233,993 tokens with cached=233,991 and new=0 output. Window census from opencode.db (~/.local/share/opencode, read-only): 331 assistant rows vs 19 user rows in the loop window — turns fired WITHOUT new real user input. Assistant replies are tiny ("OK" variants, finish=stop, 3 parts). 11 user messages contain the "Magic Context Config Warning" (caveman_text_compression TS-only warning from the rust-mode config path). MC log (steady): `rust pass: decision=SOFT+ served_from=transform in=N out=N applied=true` with N growing by 1 per pass.

## Candidate mechanisms, in priority order (verify each at source)

1. CHANNEL-2 DIRECTIVE LOOP (prime suspect): in rust mode the module emits `host_directives.channel2` deterministically when its durable trigger math says nudge (crates/mc-module, channel2 directive emission) and the TS adapter delivers it through the existing lease (rust-mode-transform.ts / channel-2 delivery machinery). Delivery = promptAsync synthetic user message = a NEW TURN = a new pass = possibly the directive again. In TS mode the lease + one-shot claim + severity math prevent refire; verify what the RUST path does: does the adapter CAS-claim before delivering? Does the module's emission stay armed because its durable state never records delivery (the TS lease records, but does the adapter report consumption back / does the module check its own ledger)? Does the synthetic turn's response reset the trigger (usage unchanged → trigger still true → emit again)? Find the exact refire cycle and note that assistant "OK" replies match a model answering a nudge repeatedly.
2. CONFIG-WARNING INJECTOR: 11 warning user messages — the rust-mode config warning (caveman inert) appears to deliver per-PASS or per-resolve rather than once per process/session (U0's warning surface). Each warning message also starts a turn. Find the dedup gate (or its absence) on that delivery path.
3. Anything else that converts a completed assistant turn (finish=stop) into a new prompt without user input on the rust path (todo directives, flush promotion, auto-search steer).

## Fix requirements

1. Channel2 on the rust leg: delivery must be once-per-claim through the SAME CAS lease discipline as TS mode, and the synthetic nudge turn must NOT itself re-trigger emission (the TS system gates on severity/reclaimable math AND claim state; the module directive must be consumed exactly once and its consumption must be visible to whatever decides next-pass emission — implement the consumption record on whichever side the evidence convicts, module durable state or adapter, matching the U2 design's "TS lease stays delivery authority" pin).
2. Config warning: once per process per session (or per config-content change), never per pass. 
3. LOOP BREAKER BACKSTOP (defense in depth, this class must never run 200 iterations again): in the rust adapter, track synthetic-turn causation — if the adapter observes more than 3 consecutive turns whose newest user message is synthetic (nudge/warning-injected, no real user input), suppress further host-directive delivery and warning injection for the session until a real user message arrives, with ONE loud log line. Pure in-memory, no schema.
4. Tests: refire-cycle regression (module emits directive, adapter delivers once, second pass with unchanged usage does NOT deliver again), config-warning dedup test, loop-breaker test (synthetic-turn cascade suppressed at 3), all fail-first where feasible.

## Gates
Focused suites (rust-mode-transform, channel-2/nudge suites, config warning path) + cargo test -p mc-module if module-side changes + typecheck + biome. Report: convicted cycle with file:line for each hop (emission -> delivery -> turn -> re-emission), fixes, test names. No em-dashes.
