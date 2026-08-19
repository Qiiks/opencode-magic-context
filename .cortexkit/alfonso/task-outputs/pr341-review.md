# Six-axis review: external PR #341

- **PR:** `cortexkit/magic-context#341`, “fix: recover stale context limits after model switches”
- **Reviewed head:** `8549d117a9e9df3b102abf87b7d4a696623f138a`
- **PR base:** `b27a6d7672cb9c8a119998b9aa0ae1b9017cc87f`
- **Independent comparison:** `57d467a75c771ecd4d5e4efdbc8ac035ebae8062`
- **Sibling migration claimant:** PR #340 head `1975b8a81574d0881b88d3a0291016e3a7911510` (v79)
- **Review date:** 2026-08-19

## Verdict: needs-rework

The PR correctly repairs the immediate OpenCode event-ordering failure: it waits for provider-accepted terminal evidence, resets the old model's pressure state before consuming the new model's first successful sample, keeps overflow evidence ahead of ordinary usage, and stops turning an accepted prompt into a detected provider ceiling. Its positive regression is behavior-based rather than shape-only, and all 14 current GitHub checks pass.

It is not merge-ready for three blocking reasons:

1. The accepted prompt is applied only to the usage snapshot (`100%` and `last_usage_context_limit`), not to the shared effective geometry. The stale 128k catalog still wins in `resolveContextLimit`, `resolveTrustedContextLimit`, and `resolveContextWindowGeometry`. Scheduler token thresholds, history budgets, protected-tail boundaries, the emergency ladder, status/sidebar denominators, and Rust-mode wire geometry therefore continue using 128k after the reported 258,901-token success.
2. The unrelated `MAX_SANE_LIMIT` change from 3M to `Number.MAX_SAFE_INTEGER` changes geometry—and potentially HARD-fold budget identity—for >3M OpenCode and Pi sessions that never experienced #331.
3. The v80 migration intentionally depends on PR #340's v79 but does not contain v79. Landing #341 first can advance a database to v80; the current append-only migration selection then cannot select a subsequently added v79. The two PR heads also conflict in the migration files. This fence mover must be rebased after #340 and held for a coordinated restart window.

## 1. Mechanism match

### What matches the proven mechanism

The central event-path changes are directionally correct:

- `isSuccessfulHostEvent()` rejects attached errors and `finish: "error"`, and requires a terminal `completedAt` or non-empty finish. Numeric usage from failed or nonterminal events no longer becomes pressure evidence.
- Input, cache-read, and cache-write values must be non-negative safe integers, and their sum must remain a safe integer.
- `createEventHook()` tracks a separate `pressureModelBySession`. A tokenless model-bearing event may update the live routing model, but it does not consume the pressure-model switch. The old pressure model is cleared immediately before the first accepted terminal sample or matching overflow from the new model.
- A successful sample above stale geometry rewarms provider metadata first. If it remains above the resolved value, the live and persisted usage snapshot becomes `{ inputTokens, percentage: 100 }`, `observed_safe_input_tokens` advances, and `last_usage_context_limit` records the accepted-token floor.
- The final PR no longer calls `recordDetectedContextLimit()` for a successful prompt. `detected_context_limit` remains provider-overflow evidence, so an accepted prompt is not mislabeled as a provider ceiling.
- Overflow-first ordering is explicitly covered when the first new-model event is an overflow. The pressure-model reset runs before `createEventHandler()` records the new model's overflow, and the next same-model success does not erase the recovery state.

This is materially better than the first PR commit, which wrote the successful sample into `detected_context_limit` with `prompt_only` provenance. Commit `ce09be23058d0203e33675756dd733b98b3af778` corrected that provenance error.

### Blocking mechanism gap: the floor does not enter shared geometry

For the exact report shape, the final state is internally split:

- successful terminal input: `258,901`
- stale SDK/catalog usable limit: `128,000`
- `contextUsageMap` and persisted percentage: `100%`
- persisted `last_usage_context_limit`: `258,901`
- `resolveContextLimit(...)`: still `128,000`
- `resolveTrustedContextLimit(...)`: still `128,000`
- `resolveContextWindowGeometry(...).usableSoft`: still derived from `128,000`

The reason is resolver precedence. `resolveTrustedContextLimit()` returns the SDK/catalog result before consulting model-matched persisted usage, while `resolveContextWindowGeometry()` never consults persisted usage at all. The PR therefore records a lower bound without making it the effective floor used by the shared consumers.

There is a second provenance ambiguity in the opposite direction. When no catalog entry exists, `resolveTrustedContextLimit()` can return `last_usage_context_limit` as an exact trusted limit. After this PR that field may contain only an accepted-prompt lower bound, not a provider-reported ceiling. The implementation needs explicit lower-bound semantics rather than overloading a field whose readers treat it as a limit.

**Required change:** introduce or clearly model a same-model accepted-prompt floor and apply it through the common limit/geometry chokepoints. The effective soft and hard budgets must be at least the accepted prompt when no matching provider-overflow proof exists. A matching provider overflow must retain first precedence and cap geometry according to its recorded `prompt_only` / `combined` / `unknown` provenance. Do not write the accepted sample into `detected_context_limit`, and do not describe it as a provider ceiling.

## 2. Cache safety (#4975 doctrine)

### Transform bytes and m0/m1 triggers

The event-ordering, terminal-validation, usage timestamp, and overflow-state changes do not directly rewrite transform bytes or add an m0/m1 marker. On the reported model switch, `cachedM0ModelKey` already produces the natural `model_change` HARD fold; the event fix itself does not create an additional HARD reason.

However, that absence is partly a symptom of the mechanism gap: because the accepted floor never reaches `usableSoft` or `historyBudgetTokens`, the corrected geometry cannot reach the render-budget identity either.

If the required shared floor is added, the proof arrives after the switched model's first successful response. The preceding switch request may already have HARD-folded with stale geometry. The next pass can therefore require one corrective `render_config` HARD fold when its history-budget identity changes. That is acceptable only for a session with proven stale geometry; it must be tested as one self-consuming correction, not a recurring fold.

### Unrelated cache-affecting expansion

Changing `MAX_SANE_LIMIT` from 3,000,000 to `Number.MAX_SAFE_INTEGER` is not needed for #331: 258,901 is already within the existing sanity range. It changes both OpenCode SDK resolution and Pi's shared `isSaneLimit()` behavior for every catalog/model window above 3M.

For such unaffected sessions, the prior result could be “reject metadata and fall back”; the new result is a potentially enormous `usableSoft`. That changes scheduler and history-budget denominators. `historyBudgetTokens` participates in `renderBudgetIdentity()`, and a recorded render-budget identity change is a `render_config` HARD trigger in `mustMaterialize()`. Thus this scope expansion can bust caches on sessions that did not experience the stale-switch bug.

**Required change:** remove the >3M policy change and its synthetic 10M test from this fix. If future model windows require a new upper-bound policy, land it separately with an explicit cache-impact assessment, OpenCode/Pi parity tests, and release treatment.

### Tokenless timing

Separating `last_usage_observed_at` from `last_response_time` is sound. A genuine tokenless terminal response may refresh provider-cache timing without extending old usage forever; the one-hour usage TTL remains anchored to the original usage observation. This changes the TTL surface outside #331, but in the cache-safe direction: a successful response correctly refreshes response timing, while stale pressure still expires.

## 3. Migrations

### Rule audit

| Requirement | Result |
|---|---|
| Version allocation | Claims v80. No numeric collision with current v78 or PR #340's v79. |
| Fence bump | `LATEST_SUPPORTED_VERSION` moves from 78 to 80. This is a schema-fence mover. |
| Fresh database shape | `last_usage_observed_at INTEGER NOT NULL DEFAULT 0` is present in `initializeDatabase()`. |
| Upgrade repair | Both migration v80 and initialization call `ensureColumn()`. |
| Legacy backfill | Usage-bearing rows copy `last_response_time` into the new observation timestamp; guarded for partial legacy schemas. |
| Idempotence | v80 replay test runs migrations twice and asserts one ledger row. |
| `clearSession()` | No separate entry is needed: this is a column on `session_meta`, and `clearSession()` deletes the whole session row. |
| Co-located test | `migrations-v80.test.ts` is present. |

### Blocking sequencing and integration risk

PR #341's own v80 test seeds v79 as already applied, which documents the required order. The branch itself nevertheless contains v80 without v79 and loosens `migrations-armed-replay.test.ts` to permit that gap.

That gap is unsafe to ship. `runMigrations()` computes the current upstream version as the maximum applied version and selects candidates only when `candidate.version > currentVersion`. If a user receives #341 first, v80 can be recorded. Adding #340's v79 later leaves `79 > 80` false, so v79 is not selected for that database.

A read-only `git merge-tree` of PR #340 and PR #341 also reports conflicts in `migrations.ts`, `storage-db.ts`, the armed-replay test, and the older migration fence assertions. The temporary gap does not make the PRs integration-ready.

**Required change:** merge/rebase on PR #340 first, retain this migration as v80, remove the temporary gap allowance, resolve the migration/fence test conflicts, and update `STRUCTURE.md` from the post-#340 v79 description to v80. Because this moves the shared schema fence, hold deployment for the restart window required by #14025.

## 4. Test adequacy

### Strengths

- The main positive regression is behavior-based: a successful output-length response after a real model-switch reset proves a prompt above stale 128k, then asserts `100%`, the accepted lower-bound value, and absence of a detected-overflow cap.
- Error-bearing, `finish:error`, and nonterminal numeric events assert that live pressure, persisted pressure, historian failure state, and pending transform decisions are not overwritten.
- Fractional, negative, and safe-integer-overflow usage is rejected.
- Overflow-first model switching is covered.
- Tokenless usage TTL behavior is covered across a database/process restart.
- Migration fresh-shape, backfill, fence, and idempotence behavior is covered.

These tests are mutation-sensitive for the event-path behavior; they would not pass merely because fields exist.

### Gaps

- There is no auditable red-first commit in the published history. The first implementation commit, `fdc66ff1338907a08c27435e62fb2ca80b446767`, adds the fix and 231 lines of event-handler tests together. Later commits revise both implementation and expectations.
- The positive test uses a synthetic 10,000,000-token prompt, which couples the bug regression to the unrelated >3M policy expansion.
- The exact positive report shape is absent. `258,901` appears only in tests proving failed/nonterminal events are ignored; it is not the successful case.
- The positive test switches directly on the successful terminal event. It does not include the reporter's tokenless new-model event between old-model usage and the accepted 258,901-token completion.
- No test follows the corrected state into scheduler token thresholds, history budgets, protected-tail boundaries, the emergency ladder, sidebar/status output, or Rust-mode wire geometry.
- No test proves that a matching provider overflow still wins after an accepted-prompt floor has been recorded.

### Independent 258,901 regression

**Yes—the in-house regression adds coverage the PR lacks.** The test on `57d467a75c771ecd4d5e4efdbc8ac035ebae8062` performs this exact sequence:

1. successful old-model usage;
2. tokenless new-model `message.updated` carrying zero token fields;
3. successful new-model terminal input of 258,901 against stale 128,000 metadata.

It asserts 258,901 / 100%, a model-keyed persisted lower bound, and no detected overflow or emergency-recovery latch. This directly protects the pressure-model/live-model split introduced by the PR. Port it as a follow-up test after the mechanism changes, with commit credit to the independent diagnosis at `57d467a75`; unlike the 10M test, it does not require relaxing the 3M sanity policy.

## 5. Blast radius

| Consumer | PR #341 result |
|---|---|
| OpenCode event usage percentage | **Covered.** Live and persisted usage become 100% for the accepted sample. |
| Percentage scheduler threshold | **Covered locally.** The 100% snapshot remains urgent. |
| Token-configured scheduler threshold | **Incomplete.** The scheduler is still passed the stale catalog `resolvedContextLimit`. |
| History budget / decay budget | **Incomplete.** `resolveTrustedContextLimit()` returns stale catalog geometry before the persisted floor. |
| Protected-tail bands | **Incomplete.** `boundaryContextLimit` remains stale; the equality guard also rejects persisted boundary usage when `last_usage_context_limit` differs from `resolvedContextLimit`. |
| Emergency ladder / tiered drop ceiling | **Incomplete.** `emergencyCeilingTokens` is derived from stale geometry, so accepted 258,901-token sessions can be over-evicted against a 128k-based ceiling. |
| Hard-wall emergency percentage | **Incomplete.** TypeScript recomputes hard-wall pressure from `windowGeometry.usableHard`, which remains stale. |
| Sidebar and `/ctx-status` | **Incomplete/inconsistent.** `usagePercentage` can show 100 while `contextLimit`, native percentage, and `windowGeometry` still expose 128k/202%-class denominators. |
| Rust transform mode | **Not covered.** The host passes stale `resolvedContextLimit` and stale geometry; Rust recomputes `input / geometry`, so the 202%-class pressure survives in the Rust scheduler, historian trigger, hard wall, and status. |
| Pi | **No mechanism implementation or immunity test.** Pi has its own live `contextWindow` pressure path, so the original host race may not apply, but the PR still changes Pi behavior through the shared >3M sanity bound. |

The PR therefore fixes only the OpenCode event snapshot, not the complete context-limit authority surface.

## 6. Code fit

### Good fit

- Uses safe-integer and non-negative validation rather than falsy token checks.
- Uses `?? 0` when summing optional token fields, preserving valid zeroes.
- Keeps detected-limit provenance reserved for actual overflow evidence in the final revision.
- Separates response timing from usage-observation timing instead of conflating their TTLs.
- Cleans the process-local pressure-model map on session deletion.

### Changes requested for house fit

- Give the accepted sample explicit **floor** vocabulary in types, comments, and resolvers. `lastUsageContextLimit` is now overloaded between reported/resolved geometry and “largest successful prompt”; readers currently treat it as an exact trusted limit.
- Replace the PR-number migration comments (`PR #340 owns v79`) after rebasing with durable domain wording explaining that v79 is the memory-evidence migration and v80 follows it.
- Add a short comment at `last_usage_observed_at || last_response_time` explaining that SQL `0` is the intentional legacy/unknown sentinel. The fallback appears to violate the falsy-value contract unless that meaning is explicit; add a direct test for the zero-sentinel fallback and expiry.
- Correct the positive test's claim that “shared metadata and live pressure both use the proven lower bound”: shared geometry consumers do not currently use it.

## Exact change requests

1. **Propagate a model-keyed accepted-prompt floor through the shared effective geometry.** Apply it to soft/hard context resolution, scheduler token denominators, history budgets, protected-tail boundaries, emergency ceilings, sidebar/status, and Rust wire geometry. Preserve matching provider-overflow precedence and provenance; never write the floor as `detected_context_limit`.
2. **Add cross-surface regressions.** Start from 258,901 accepted over stale 128k and assert consistent denominator/floor behavior in TypeScript scheduler/history/protected-tail/emergency paths, sidebar/status, and Rust mode. Add a floor-then-provider-overflow test proving overflow wins. Add a Pi test that either proves the race is inapplicable or implements the equivalent floor.
3. **Remove the `MAX_SANE_LIMIT` relaxation and 10M coupling from this PR.** Handle >3M model policy separately with cache-impact approval and OpenCode/Pi tests.
4. **Integrate migrations in order.** Land/rebase on #340 v79, keep this as v80, remove the temporary migration gap, resolve the existing conflicts, update `STRUCTURE.md`, and deploy only in the schema-fence restart window.
5. **Port the in-house tokenless 258,901 regression as a credited follow-up.** Credit the independent diagnosis at `57d467a75`; keep the exact tokenless-switch sequence and the assertions that no overflow ceiling or recovery latch is invented.
6. **Make lower-bound and zero-sentinel semantics explicit.** Update provenance vocabulary/comments and add the direct falsy-sentinel test described above.

## Verification notes

- Reviewed the complete eight-commit PR diff and the direct diff against independent commit `57d467a75`.
- Compared migration allocation and a read-only merge tree against PR #340.
- `git diff --check` passed for both the PR patch and the PR-to-independent-diagnosis delta.
- GitHub reports 14 successful checks at the reviewed PR head; `[code]smith` is skipped.
- No merge, post, push, or PR mutation was performed.
