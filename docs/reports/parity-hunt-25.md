# Parity Hunt #25 — widening, emergency discard-last, and two deep roams

Date: 2026-09-01
Standing protocol: #15150, counter 1/3
Series entering this hunt: 46 defects / 24 hunts
Canonical behavior: TypeScript

## CLEAN-OR-FINDINGS verdict

**CLEAN-OR-FINDINGS verdict: CLEAN — zero atomic behavior defects found; the retirement counter advances to 2/3.**

This hunt did not treat mutual absence, an empty newest corpus window, or unlike-session provider bytes as proof. It widened the live capture window until both TypeScript and Rust Anthropic lanes had provider bodies, executed the admitted value-space comparison, drove the missing TypeScript emergency discard-last call site, and traversed two additional producer-to-consumer families. Every deliberate break used the exact `NON-VACUITY BREAK` token and was restored.

## Mandatory seed 1 — lane-resolved live comparison after deliberate widening

The newest requested lower bound, `2026-09-01T23-59-59-999Z`, admitted no bodies, agreeing with Hunt #24 rather than contradicting it. The new widening control was then used deliberately:

```text
python3 scripts/audit-transform-wire-parity.py --live --date 2026-09-01 \
  --after 2026-09-01T23-59-59-999Z --min-provider-bodies 7990 \
  --per-session 100 --skip-live-rpc --skip-live-rust-oracle
```

The read-only audit widened to `2026-08-31T04-28-14-508Z`. Those are the search bounds for this comparison: the empty requested instant through the oldest body needed to admit all 7,990 bodies in the two capture directories. The result was written outside the repository; no provider prose was retained.

### Admission denominator

- 7,990 provider bodies were inventoried.
- 582 bodies resolved to a lane; 7,408 remained lane-unverified inventory.
- Resolved Anthropic: 100 TypeScript bodies and 200 Rust bodies.
- Resolved OpenAI Responses: 282 TypeScript bodies and zero Rust bodies, so that family remained an explicit evidence gap rather than a comparison.
- Eight collision-free short session hashes resolved through readable live project configuration: six TypeScript and two Rust. SQLite was opened read-only.

### Executed Anthropic value-space comparison

The 100 TypeScript and 200 Rust Anthropic bodies produced a real two-lane comparison:

| Axis | Result |
|---|---|
| system message count | matched: `1` |
| system block count | matched: `4` |
| tool call/result pairing | matched: balanced, valid adjacency |
| dropped-placeholder shapes | matched: user isolated and before-tool-call classes |
| empty-content shapes | divergent unlike-session inventory: Rust additionally contained empty assistant and user text blocks |
| signed-reasoning positions | divergent unlike-session inventory: both shared positions 1 and 2; Rust additionally contained positions 0 and 3 |

The two divergent axes are not reconvicted. Hunt #14 already drove identical raw/session shapes through both implementations: empty assistant/user text did not reproduce as a lane-only MC producer class, and position 0 is the adjudicated provider-native signed-thinking vector that Rust must not mutate to mimic TypeScript adapter residue. Position 3 is likewise positional inventory from unlike histories, not a same-input byte claim. The current full plugin, Rust, and Pi replay surfaces remain green below. No adjacency or provider-wire invariant fired.

**Examined and cleared.** The widening tool converted Hunt #24's zero-lane gap into an admitted live comparison without weakening lane coordinates or privacy rules.

## Mandatory seed 2 — TypeScript emergency discard-last call site

`compartment-runner.test.ts` now drives the TypeScript `runCompartmentAgentWithLease` path with:

- four eligible messages followed by the normal five-message protected tail;
- two adjacent historian compartments `[1,2]` and `[3,4]`, with the second ending exactly at the eligible chunk edge;
- durable provider-overflow recovery armed through `recordOverflowDetected`; and
- publication through the real runner/storage call site rather than a direct shared-predicate argument.

Both compartments persist with endpoints `[2,4]`, and the historian ledger records `discarded_last = 0`.

**Executed red-first evidence.** Replacing the emergency arm with `NON-VACUITY BREAK` made the focused Bun test fail at `compartment-runner.test.ts:1382`: received endpoints `[2]`, expected `[2,4]`. Restoring the arm passed in the full 4,279-test plugin suite.

**Examined and cleared.** TypeScript agrees with the Hunt #24 Pi and Rust fixtures: emergency recovery disables discard-last at the chunk edge.

## Free roam 1 — migration-49 mural write family and Claude Code inheritance

The roam followed every current artifact entrypoint rather than stopping at the shared composer:

1. migration 49 creates `mc_project_mural_artifacts` for both fresh and incrementally migrated stores;
2. an OpenCode-profile transform capability-gates and host-feeds the rendered data URL;
3. `upsert_project_mural_artifact` writes by resolved project identity and suppresses same-hash updates;
4. a Claude Code-profile transform ignores request-supplied mural bytes, reloads the project artifact, and feeds it through the common m0 composer; and
5. the transform response contains the inherited OpenCode artifact, not the untrusted Claude Code request value.

A new handler-level Rust fixture permanently joins those entrypoints. It complements the existing direct store hash-gate test and composer tests by proving the route/project handoff.

**Executed red-first evidence.** Two independent mutations were restored:

- forcing the store UPSERT to update on an equal hash failed the new test at `mc-module/src/lib.rs:16929`, where the supposedly unchanged project artifact had different bytes/timestamp;
- replacing Claude Code inheritance with `NON-VACUITY BREAK`/`None` failed at `mc-module/src/lib.rs:16954`, where the response no longer contained the OpenCode artifact.

**Examined and cleared.** No missing write entrypoint, hash-gate bypass, or compose-inheritance defect was found.

## Free roam 2 — overflow report ledger consumption

The roam traced the one-directional forwarding fact from `buildWindowReport` through JSONL rotation/parsing and `export-window-reports.ts` into `fusiform-window-report/v1`:

- known forwarding providers emit only `path_may_forward: true`;
- direct or unknown routes omit the field rather than asserting `false`;
- the parser accepts only literal `true`, so legacy/malformed `false` cannot become a promotable non-forwarding claim;
- the exporter likewise copies only literal `true`; and
- `served_by_hint` remains observed evidence and is not inferred into the forwarding field.

**Executed red-first evidence.** Mutating the parser's literal-true guard with `NON-VACUITY BREAK` failed `export-window-reports.test.ts:74`: the forwarding report lost `path_may_forward: true`. The restored producer/exporter tests passed.

**Examined and cleared.** The unrepresentable-false contract survives its actual consumer boundary.

## Three-way ledger

### Examined and cleared

- Deliberately widened live corpus admission and real TypeScript/Rust Anthropic value-space comparison.
- TypeScript emergency discard-last through the runner call site, including persisted endpoints and historian telemetry.
- Migration-49 fresh/migrated table presence, OpenCode host-fed writes, same-hash suppression, project identity, and Claude Code compose inheritance.
- Overflow report forwarding truth from producer through JSONL parser and Fusiform export.
- Full TypeScript plugin, Rust workspace, Pi plugin, and parity differ surfaces.

### Examined and convicted

- None.

### Skipped with rationale

- Pi `copySessionStateForClone` migration-delta inventory: skipped after the two selected free roams reached producer-to-consumer mutation depth; the full Pi suite still ran its 19 clone-inheritance cases.
- OpenAI Responses live two-lane comparison: zero Rust bodies resolved in the widened window, so 282 TypeScript bodies remain one-lane evidence only.
- Six other provider families: zero live bodies in the widened window.
- Raw provider bodies, full identifiers, and paths: prohibited by the privacy contract; only counts, aggregate shapes, and short hashes were inspected.
- Live SQLite mutation, fence movement, migrations, and master push: prohibited; live reads remained read-only and all mutations were hermetic and restored.

## Build dependency note

The shared `subc-protocol` path dependency had advanced from 0.16.0 to 0.17.0 while this worktree's lockfile and manifest call site still expected the old infallible `build_provenance` API. The lockfile now records 0.17.0, and the manifest unwraps the validated compile-time provenance with a targeted expectation. This is build compatibility, not a parity finding; manifests did not change.

## Verification

- `cd packages/plugin && bun test --parallel --timeout 30000` — passed: 4,279 tests, 0 failed.
- `cd packages/plugin && bun run typecheck` — passed.
- `cargo test --workspace` — passed with unfiltered result lines: mc-module 1,017 passed / 4 ignored; mc-store 134 passed; all other workspace targets passed.
- `cargo fmt --all -- --check` — passed after formatting the new Rust fixture.
- `cd packages/pi-plugin && bun test` — passed: 906 tests, 0 failed, including 19 clone-inheritance cases.
- `cd packages/pi-plugin && bun run typecheck` — passed.
- `python3 scripts/audit-transform-wire-parity.test.py` — passed: 9 tests.
- `bun test ./scripts/audit-transform-wire-parity-live.test.ts` — passed: 3 tests.
- `cd packages/plugin && bun test scripts/export-window-reports.test.ts` — passed: 1 test.
- `git diff --check` — passed.
- AFT completed language phases but did not return a fresh metrics terminal result; executable typechecks and suites above are authoritative.
- Sidekick comment review found no unclear changed comments.

No `NON-VACUITY BREAK` marker remains in source, tests, or fixtures; the report retains the required evidence labels only.

CLEAN-OR-FINDINGS verdict: CLEAN
