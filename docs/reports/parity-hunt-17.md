# Parity hunt 17 — the retirement hunt

## Verdict, method, and fence

**FINDINGS — NOT CLEAN.** Hunt 17 convicted and fixed one session-lifecycle defect, **H17-01 RUST-DELETE-RETRY**. The required non-live differ repair is a tooling seed and does not count as a finding. Because the behavior verdict is not CLEAN, standing protocol #15150 does **not** retire and the consecutive-CLEAN counter resets from **2/3 to 0/3**.

TypeScript remains canonical. The work started at `03defe694a7429385afc628cc098ae43b58f549b`; Rust admission remains the two configured projects. No serializer, schema, migration, provider, authority, or deployment fence moved. Live SQLite access was read-only/query-only. Reports contain only counts, fixed class names, hashes, byte lengths, and short session prefixes; no provider prose, full session ids, project paths, RPC credentials, or database rows are committed. No master push is part of this work.

The rolling lower bound was `2026-08-31T07-31-31-335Z`, selected from the newest capture visible at the start of the sweep. Both UTC dates and both Darwin capture roots were audited.

## Tooling seed — non-live lane restored and made non-vacuous

Served system bytes can omit project roots. The old non-live flow therefore left every such capture `unverified`, excluded every body from the TS/Rust denominator, and printed an empty report that could be mistaken for health.

The differ now joins rootless captures to decisive, in-window durable authority evidence: TypeScript `transform_decisions` rows with observed cache operands and Rust `mc_cache_state.last_activity_at`. A session observed in both stores is `ambiguous` and remains excluded rather than being guessed. The report exposes resolved/remaining counts and the effective rule. If no body enters either TS or Rust, non-live mode exits nonzero with `non-live provider differ refused`; it never emits an empty green-looking report.

The regression creates two rootless captures, first proves the command refuses without coordinates, then proves the decision stores recover one TS and one Rust denominator. It also creates a mixed-authority/config-flip shape and requires `durable_authority_ambiguous`. The final real non-live sweep was non-empty:

| Root/date slices | TS | Rust | Still unverified | Result |
|---|---:|---:|---:|---|
| Anthropic, Aug 31 + Sep 1 | 2,028 | 64 | 0 | 2,092/2,092 recovered from durable authority |
| OpenAI Responses, Aug 31 + Sep 1 | 60 | 0 | 4,089 | non-empty TS denominator; absent durable coordinates remain excluded |

All four non-live reports had zero unexplained wire invariants. The unlike-session Anthropic value spaces remain observations, not same-input convictions.

## Full sweep

### All three same-input replay arms

The hermetic replay command was run for `anthropic`, `mock-anthropic`, and `openai-responses`. Real Anthropic retained three exact signed-thinking position differences already adjudicated as the provider-native safety exception; **unadjudicated divergence count was zero**. Mock Anthropic and OpenAI Responses each had zero divergence. The Responses arm matched the dropped tool sentinel, tool/result pairing, empty-content classification, and signed-reasoning position.

### Newest live corpus, RPC, and maintenance

The two date slices covered **6,213 bodies**: 2,080 Anthropic and 4,133 OpenAI Responses. Privacy-safe coordinates admitted **653** bodies: Rust Anthropic 205, TypeScript Anthropic 189, and TypeScript Responses 259; 5,560 bodies remained unverified inventory and no session-hash collision was observed.

The broad Aug 31 slice still contains the exact H13-04 TypeScript Responses capture (`sha256=4506f17b…`, 26,464 bytes) adjudicated in Hunt 15. Three Sep 1 empty tool-content captures remain lane-unverified raw-provider inventory. The current same-input Responses arm is clean, so neither class is promoted into a transform finding. Broad-window Anthropic empty-text, dropped-shell, and reasoning-position classes remain unlike-session value spaces. The post-`93409fea` Sep 1 live slice has no dropped-placeholder value-space divergence.

Both RPC operator lanes were observed. One Rust and one TypeScript session had stable direct-before/direct-after values, matched sidebar/status usage, context limit, compartment count, pending operations, and authority-correct total tags. Both aged Caveman candidates had zero ordering inversions and all **18** TS/Rust byte/hash cells matched. Engine truth, the decision window, historian no-fire, producer, mural, wrapup, Hunt-12 source contracts, and the aggregate unexplained bucket were empty.

Standing observations remain observations: one real Pi JSONL still combines native compaction history with a pending marker; post-cutoff Rust dreamer-applier, recomp, and wrapup command rows are zero; and post-cutoff Rust compartment/memory publish coverage is zero. Their executable maintenance/replay contracts pass, so absence is not promoted into a behavior defect.

### Fresh frame: Pi surfaces, config flips, and lifecycle edges

The sweep deliberately left the report series' usual axes:

- **Auto-search across OpenCode, Pi, and Rust.** OpenCode and Pi share durable per-message decisions, retry/no-hint semantics, augmentation suppression, and the shared hint renderer. Rust uses authority-local memory/compartment lexical candidates but carries the same enabled/min-length/threshold and frozen-replay controls. The available-corpus distinction was already bounded in Hunt 4; 23 OpenCode/config-flip tests, 34 Pi auto-search/todo tests, and six Rust auto-search tests passed.
- **Synthetic todo across harnesses.** OpenCode E2E passed 7 tests, Pi E2E passed 8 after building its required ignored `dist`, and 13 Rust synthetic-todo tests passed. State replacement, terminal removal, priority normalization, synthetic pair identity, and replay remained aligned.
- **TS↔Rust config flip-back.** The primary checkout drains memory/note authority before lifting the host write fence, while linked worktrees do not initiate the drain. The focused flip-back regressions passed.
- **Session deletion.** This read found H17-01 below.

## H17-01 RUST-DELETE-RETRY — fixed

**Conviction.** TypeScript deletion first writes `pending_session_cleanup`; if its transaction fails, maintenance retries from that durable marker. Rust deletion was fire-and-forget: `rust-mode-transform.ts::clearSession` deleted its in-process route/cache, caught and logged `session.delete` failure, and the event handler immediately cleared `context.db`, including the cleanup marker and session→project binding. If subc or the module store was unavailable at that instant, module-owned tags, cache state, overlays, producer ledgers, and session notes could remain indefinitely with no coordinate from which to retry. This was a user-visible deletion/lifecycle asymmetry, not an evidence gap.

**Fix.** A Rust deletion is now awaited before host rows are cleared. Its initial cleanup marker is atomically classified `opencode:rust`; the ordinary host sweeper excludes that class. A failed call therefore retains both the marker and `session_projects` coordinate. The process-wide maintenance timer retries only through the matching project registration, closes the transport route after each attempt, and clears host state only after the idempotent module deletion succeeds. A lazy cleanup transport is retained even after `transform_mode` flips back to TypeScript, so a restart/config flip cannot strand the retry. No schema change was needed.

Regressions prove that a rejected module deletion propagates while closing its route, host tags and the Rust marker survive, the ordinary sweeper cannot consume the marker, the project-scoped retry calls the module, and successful acknowledgement atomically removes host state and the marker.

**Executed non-vacuity mutations.** Every temporary break used the exact `NON-VACUITY BREAK` token and was restored:

1. Disabling durable lane adoption made the differ suite fail in `test_non_live_mode_recovers_rootless_lanes_or_refuses_loudly` because the recovered run refused with an empty denominator.
2. Inverting the Rust-marker exclusion made both cleanup contracts fail: ordinary TS cleanup stopped retrying while the Rust marker was incorrectly consumed before module acknowledgement.

The restored differ passed 9/9; the restored lifecycle files passed 120/120. No mutation token remains.

## Retirement package — 17-hunt series

Counts use atomic, independently observable behavior defects. Evidence gaps, residual/structural briefs, intentional transport differences, and instrumentation-only additions are excluded. Hunt 10 has no standalone report in the tree; its three fixes are recoverable from the intervening commits and architecture refresh: transactional module tag totals, LKG representation freeze, and sentinel retention across marker-window absence.

| Defect class | Hunts 1–16 | Hunt 17 | Series total |
|---|---:|---:|---:|
| Provider-visible rendering, cache, and replay | 12 | 0 | 12 |
| Facade/public contracts | 2 | 0 | 2 |
| Nudge/display semantics | 1 | 0 | 1 |
| Producer/config propagation | 6 | 0 | 6 |
| Authority routing, maintenance, and lifecycle side effects | 8 | 1 | 9 |
| Operator truth, security, and diagnostics | 7 | 0 | 7 |
| Runtime test-isolation/config resolution | 1 | 0 | 1 |
| **Total** | **37** | **1** | **38** |

The series built permanent instruments: the dump/live differ and privacy-safe lane joins; provider-family and unexplained-class matrices; RPC/direct snapshot brackets; real Pi JSONL and Caveman oracles; generated boundary, nudge-hygiene, Caveman, temporal, facade, injection, and codec goldens; three same-input provider replay arms and their mutation contract; maintenance, wrapup/recomp, LKG, lifecycle, FTS, authority, no-fire, and Channel-2 suites.

**Retirement decision:** deferred. Hunts 15 and 16 are the two consecutive CLEAN rounds; H17-01 resets the counter. Nothing in the active protocol retires in this change. If a future third consecutive CLEAN round is reached, only scheduled active hunting retires. The replay arms, maintenance contracts, generated goldens, differ suites, denominator refusal, provider matrices, and ordinary package gates remain standing CI forever.

After eventual retirement, protocol #15150 must mechanically rearm when any one of these occurs:

1. a change under `crates/mc-module/src/codec/**`, `ck-wire`, a `serve_native`/serializer-profile implementation, or the TypeScript provider projection/serializer;
2. a new provider family, wire family, serializer profile, or provider replay arm;
3. a new harness leg or a host message-schema integration for OpenCode, Pi, OMP, Claude/direct, or another transport;
4. a change to transform-authority routing, module state sync, subc protocol bindings, config `transform_mode` transitions, or session clone/revert/migrate/delete lifecycle;
5. a context/module/store migration that adds, removes, or changes a session-scoped, cache-identity, authority, or operator-status field;
6. a render-affecting config surface for prompt bytes, model identity, geometry, thresholds, auto-search, todo synthesis, nudges, or maintenance;
7. any standing replay/differ/maintenance contract goes red, emits an unadjudicated class, observes a coordinate collision, or refuses because a required lane denominator is empty.

These are path/schema/enum/test outcomes, not reviewer judgment calls.

## Verification

- `python3 scripts/audit-transform-wire-parity.test.py` — 9 passed.
- Four non-live newest-window differs with context/module stores — passed with non-empty denominators; explicit empty input refused nonzero.
- Two live newest-window audits over both capture roots, with RPC and Rust oracle — completed; 6,213 bodies, 653 admitted, aggregate source-contract unexplained bucket empty.
- Three `replay-transform-wire-parity.ts` provider arms — zero unadjudicated divergences.
- Plugin typecheck — passed.
- Focused changed lifecycle suites — 120 passed; timer-driven retry suite — 14 passed.
- OpenCode/Pi/Rust fresh-roam unit and E2E contracts — passed as counted above.
- Full plugin suite with a 30-second per-test margin — 4,261 passed / 1 failed under parallel load; the sole unrelated failure hit its own fixed 10-second polling ceiling. That file and the three earlier load-timeout files passed focused, 108/108.
- Pi plugin build required by its E2E harness — passed; generated `dist` remains ignored.
- `git diff --check` — passed.
