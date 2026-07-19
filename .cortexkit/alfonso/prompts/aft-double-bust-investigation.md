# Investigate: AFT session double bust after fold + ongoing tail-retention collapse (TS lane, prod)

Repo: this worktree (branch from `subc-migration` HEAD). INVESTIGATION FIRST — no fix until the mechanism is proven from wire dumps. This is the production TS lane (prod serve, dist built ~13:47 today from the rust-mode merge; rust mode is NOT active for this session — TS pipeline, transform_mode default).

## Evidence base (all on disk now)

Session: ses_313660571ffeZTsf4koSJwk50Q (AFT). Wire dumps: /var/folders/18/257zzylx4h1gbkcvs4cnpqqc0000gn/T/opencode-anthropic-auth-dumps (1431 requests). Analyzer: `bun packages/plugin/scripts/analyze-cache-busts.ts ses_31366 --since ... --until ... --show-diff`.

Two findings to explain:

FINDING A (double bust): 20:38:16 UTC pass = legitimate fold (m0 rematerialized, cachedPrefix collapsed to system[3] 24,660B, "Compaction pressure"). 20:40:41 UTC pass = message[1](assistant) — the FIRST TAIL ASSISTANT after the fold — changed bytes (hash e2ade170cc → dd34a419dc), busting from message[1] (548,498B retained of 1,270,297B). This is the boundary-adjacent representation-flip class: an assistant near the compaction boundary served in one shape on the fold pass and a different shape one pass later. We fixed one site of this class pre-v0.32.0 (fold-execute vs defer representation phase, `git log --grep` for the double-bust/representation work). Prove which bytes changed: extract message[1] from both dumps, byte-diff, identify the exact part-level difference (sentinel vs full shape? reasoning present vs cleared? tool block skeleton vs full? tag prefix?). Then trace the pipeline site that produces each shape and why the fold pass and the following pass disagree.

FINDING B (retention collapse, ONGOING): passes at 21:02 / 21:11 / 21:20 / 21:32 UTC show first-divergence at the NEWEST user message (message[254], [282], [294], [316] of 261/289/301/323 segs) with cachedPrefix@breakpoint stuck at message[0] = 548,498B on ~1.5MB prompts. Questions, in order: (1) is the analyzer verdict trustworthy here, or is this an artifact of how it maps usage.cache_read to breakpoints (read the analyzer source — you may be debugging the instrument)? (2) if real: with prompts this size Anthropic allows 4 cache_control breakpoints — where are they placed on these requests (extract cache_control positions from the dumps)? If the tail markers are missing or landing at positions that shift every pass, retention caps at m0's marker and the whole tail re-uploads every pass. Identify the marker-planning code path for the TS lane and why tail markers are absent/unstable on these passes.
NOTE: cross-check against usage numbers in the dumps' responses (cache_read_input_tokens / cache_creation_input_tokens) — the dashboard showed STABLE 100% rows at 20:37 and 20:46 UTC-2... verify which passes actually paid uncached tokens.

## Deliverable

A written verdict per finding: mechanism, file:line, whether it is a regression from a specific commit (check what the 13:47 dist contains vs prior dist — `git log --oneline` today's merges: rust-mode waves touched shared files; the TS-lane byte-neutrality claim rests on the mode gate) or a pre-existing class, and the minimal fix proposal. DO NOT implement fixes in this task — evidence and proposal only, banked as a report file in the worktree root (aft-bust-report.md). If Finding B is an active leak, say loudly how much it costs per pass so the priority is unambiguous.

## Gates
Report quality only; no code changes expected. No em-dashes.
