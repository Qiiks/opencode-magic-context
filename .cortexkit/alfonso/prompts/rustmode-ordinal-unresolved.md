# Fix: authority pass fails with `rust ordinal unresolved` on the live drive session

Repo: this worktree (branch from `subc-migration` HEAD). TS plugin (module-wire.ts) most likely. Investigation-first with the LIVE repro — the failing pass is reproducible on demand.

## Evidence

Session `ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO` (drive clone, transform_mode rust). With the apply-seam fix live (22:07:10Z): `rust transform failed; attempting LKG replay: rust ordinal unresolved` then `rust pass: decision=error served_from=none in=309 out=309 applied=false`. The module is fine (store pass trace clean) — the ADAPTER fails before sending, in `resolveOrdinalsForModule` (packages/plugin/src/hooks/magic-context/module-wire.ts:55-189).

That function returns `{ok:false, reason:"unresolved", messageId}` when a wire message id has no ordinal in the memo AND is not part of the CONTIGUOUS unpersisted tail suffix. Note the call site (rust-mode-transform.ts ~434) throws `rust ordinal ${resolved.reason}` and DISCARDS resolved.messageId — first task: include the messageId (and its index + role) in the thrown error so this class is diagnosable from logs forever.

## Steps

1. Improve the error as above; rebuild dist (bun run build).
2. Reproduce live: cd /Users/ufukaltinok/Work/Projects/CortexKit/benchmarks && /Users/ufukaltinok/.opencode/bin/opencode run -s ses_l7l9CptsEWvdm4I6pTsAcPaYCVBO -m anthropic/claude-opus-4-8 "Ordinal probe: reply OK" (binary is NOT on your PATH — absolute path required). Read the new error line from /var/folders/18/257zzylx4h1gbkcvs4cnpqqc0000gn/T/opencode/magic-context/magic-context.log.
3. Diagnose THAT message id against opencode.db (~/.local/share/opencode/opencode.db, read-only): does a row exist for it? What is its time_created relative to the memo anchor's newest page? Is it a compaction-summary row (json_extract(data,'$.summary')=1)? Is it mid-array on the wire with persisted messages AFTER it (breaking the contiguous-suffix assumption)? Candidate causes to check in order:
   (a) OpenCode's in-memory array contains an unpersisted or reordered message that is NOT at the tail (e.g. a synthetic/nudge/summary-adjacent message), breaking the contiguous-suffix rule;
   (b) the memo anchor page-walk misses rows whose time_created ties or precedes the anchor (check readRawSessionMessageOrdinalPage's anchor comparison for >= vs > off-by-one on (timeCreated, id) ties — the clone has many identical time_created values from the remint);
   (c) the summary-row filter (isRawCompactionSummaryInfo) filters the WIRE copy but the memo/canonical count treats the same row differently, shifting expectations;
   (d) stale memoAnchor from a prior generation surviving a case that should clear it.
4. Fix per evidence. If the contiguous-suffix assumption is genuinely violated by a legitimate OpenCode wire shape, extend the provisional rule to handle that exact shape (justify why it cannot mis-assign ordinals to genuinely-persisted-but-unpaged rows — the mismatch self-heal must still catch true drift); if it is an anchor/tie bug, fix the page-walk comparison with a regression test using identical time_created values.
5. LIVE-VERIFY end to end: same opencode run command; success = assistant reply, no 400, AND the newest wire dump for the session (find /var/folders/18/257zzylx4h1gbkcvs4cnpqqc0000gn/T/opencode-anthropic-auth-dumps -name "*ses_l7l9*body.json" -newermt "<start time>") shows messages[0] carrying <project-docs> (module m0 on the wire — this would be the FIRST such beat; the per-pass log must show applied=true). If a NEW distinct failure appears after ordinals resolve, report it precisely and stop — do not chain fixes beyond your scope.

## Gates
Focused suites (module-wire, rust-mode-transform, shadow-sender) + typecheck + biome; regression test for the convicted cause (fail-first). Comments explain the provisional-suffix invariant. No em-dashes. Report: convicted mechanism, messageId evidence trail, fix, live-beat proof.
