# PR #256 review — Oh My Pi support

**Reviewed head:** `c683ddb98f958216af14c0037d8d2d955dbf177c` (`pr-256`)
**Base:** `437435b73c96571d35194fce85c9f9235f588899`
**Scope inspected:** all 43 changed files / 2,198 added and 195 removed lines. Line references below are against the reviewed PR head.

## Verdict: MERGE-AFTER-CHANGES

The PR has a useful OMP-only shape in several places (new adapter, helpers, setup, doctor, and path tests), and `detectOmpBinary()` probes the distinct `omp` executable rather than treating `pi` as OMP. That is the correct first-line answer to the plain-Pi mis-detection concern.

It is not merge-ready as a full harness leg, however. It changes active plain-Pi behavior in the shared Pi runtime and dashboard, performs OMP session discovery without proving OMP is present, and has no real-host CI coverage. It also routes OMP models through the Pi-only provider translation contract without an OMP mapping/round-trip test, and misses documented OMP package/config overlay path surface.

## Required changes

### 1. Keep the Pi child-extension path byte-for-byte outside a positively identified OMP host

**Files:** `packages/pi-plugin/src/subagent-runner.ts:178-200`, `packages/pi-plugin/src/subagent-runner.test.ts:289-311`

This is a direct touch to Pi extension discovery immediately downstream of the `MAGIC_CONTEXT_PI_SUBAGENT` entry guard. The guard itself at `packages/pi-plugin/src/index.ts:700-706` is unchanged, but the changed runner constructs explicit `--extension` paths for those guarded child processes.

Before this PR, relative allowlist entries always resolved from `~/.pi/agent`. The new `getHostAgentSettingsDir()` changes them whenever `PI_CODING_AGENT_DIR` is set, with no OMP-host predicate. `PI_CODING_AGENT_DIR` is also a supported ordinary-Pi setting, so a plain Pi installation can now load a different provider/extension path even when OMP is absent. The added test explicitly locks in that plain-Pi-visible semantic change by setting only `PI_CODING_AGENT_DIR`; it never establishes an OMP process.

Move the OMP-specific resolution behind an explicit, tested OMP-host boundary (or inject it from an OMP-only adapter). Preserve the existing Pi default and `PI_CODING_AGENT_DIR` behavior unless that Pi behavior change is separately reviewed. Add a negative regression test that runs the Pi runner with OMP absent and verifies the pre-PR argv, including the `MAGIC_CONTEXT_PI_SUBAGENT` guard and extension-discovery behavior.

### 2. Do not turn the Pi dashboard scanner into an unconditional all-OMP-profile scanner

**Files:** `packages/dashboard/src-tauri/src/pi_sessions.rs:84-237`; `packages/dashboard/src-tauri/src/commands.rs:1040-1204`

`pi_sessions.rs` replaces one Pi root with all of these roots on every Pi scan: stock Pi, `PI_CODING_AGENT_DIR`, every directory under `~/.omp/profiles`, the current OMP profile, and equivalent XDG roots. This happens without detecting an `omp` binary or otherwise establishing that OMP is installed. It then deduplicates all results as Pi sessions.

Consequences:

- A plain Pi dashboard with OMP absent but a `PI_CODING_AGENT_DIR` now returns different sessions from before.
- Stale `.omp` data is surfaced as Pi data even after OMP is removed.
- All profiles, rather than the active profile, are scanned and collapsed into the Pi session list. There is no OMP harness discriminator in the dashboard result.
- The model endpoint named `get_available_pi_models` now probes and returns OMP models after a Pi miss. With OMP absent its value usually remains empty, but it adds executable/login-shell probes to the plain-Pi failure path and retains Pi naming/caching for OMP values.

Keep Pi scanning isolated and add a separately named OMP scanner/model source. Invoke the OMP source only after explicit OMP detection, make the active/profile policy deliberate, and retain the exact old Pi result when OMP is absent. Add tests for the OMP-absent direction, a Pi `PI_CODING_AGENT_DIR` install, stale OMP roots, active-vs-inactive profiles, and the dashboard-facing harness classification.

### 3. Complete the OMP path and configuration contract before claiming full support

**Files:** `packages/cli/src/lib/paths.ts:163-249`, `packages/cli/src/lib/paths-omp.test.ts:1-98`, `packages/cli/src/commands/setup-omp.ts:30-105`, `packages/cli/src/commands/doctor-omp.ts:157-220,294-320`

The resolver models `.omp`, profiles, `PI_CONFIG_DIR`, `PI_CODING_AGENT_DIR`, and an initialized XDG layout, but does not model OMP's documented `PI_PACKAGE_DIR` package/plugin location or `PI_CONFIG_FILES` overlays. `getOmpPluginsDir()` therefore hard-codes `dataRoot/plugins`, and `getOmpPluginsLockPath()` derives from it. Setup, removal, cache clearing, and doctor all depend on those paths.

Additionally, setup and `doctor --force` read effective OMP settings from the current repository and then run unscoped `omp config set` commands. OMP has project settings and config overlays. The PR needs to establish, in code and docs, whether that command writes project or user state, and ensure a project-tier OMP file cannot cause a global settings change without an explicit user-scope confirmation. This is a new tier-trust surface even though the Magic Context JSONC schema is unchanged.

Use OMP's authoritative path/config APIs where possible rather than maintaining a second resolver. At minimum cover `PI_PACKAGE_DIR`, `PI_CONFIG_FILES`, default and named profiles, `PI_CODING_AGENT_DIR`, and XDG with host-produced fixtures. Add scope-aware tests for an untrusted `<repo>/.omp/config.yml` and a force repair.

### 4. Give OMP its own provider-translation contract and prove both directions

**Files:** `packages/cli/src/lib/omp-helpers.ts:68-97`, `packages/cli/src/commands/setup-omp.ts:11-17`, `packages/cli/src/commands/setup-pi.ts:183-232`, `packages/pi-plugin/src/subagent-runner.ts` (model spawn translation); no change to `packages/plugin/src/shared/harness-provider-map.ts`

The OMP model parser preserves OMP `selector` strings, then OMP setup reuses `writeMagicContextConfig()`, which calls `piModelRefToCanonical()`. The shared child runner later uses the inverse Pi-oriented translation. That is a cross-harness conversion pipeline, but the translation chokepoint still documents and tests only Pi (`openai-codex` and `google-antigravity`). In particular, an OMP selector with `openai/...` is subject to Pi's preferred `openai-codex/...` spawn rewrite without proof that it is the correct OMP selector.

Either add an explicit OMP mapping at the chokepoint with canonical -> OMP and OMP -> canonical operations, or document and test that OMP deliberately accepts the exact Pi forms. Add an end-to-end round-trip test from representative OMP `models --json` output through setup's persisted shared config and the OMP child invocation. Include changed prefixes and opaque/scoped selectors. A parser-only test is not enough for a bidirectional translation boundary.

### 5. Add real OMP CI coverage before adding a maintained harness leg

**Files:** no `.github/workflows/**` or other CI configuration is changed.

Every new OMP test is a unit test with a fake binary, injected dependency, or self-authored path fixture. They do exercise some local error handling, but none proves the actual OMP CLI JSON shapes, plugin manifest loading, profile/XDG path behavior, `omp config` write scope, or that the Pi extension runs under OMP with the subagent guard intact.

Add a pinned supported OMP version (the PR names `17.1.7`) to CI and run at least one Linux smoke path that installs the locally built package through `omp plugin`, starts a real OMP session, and checks Magic Context registration. Cover default and named-profile/relocated paths plus an OMP-absent plain-Pi regression job. Without this, each future Pi-parity change gains an untestable host-specific branch.

## Harness detection and entry-gating assessment

- **CLI binary detection:** `packages/cli/src/lib/omp-helpers.ts:23-34` looks only for `omp`, so it does not misclassify a stock `pi` executable as OMP. `packages/cli/src/adapters/index.ts:7-13` adds OMP as a third adapter; a normal plain-Pi install still selects Pi when `omp` is absent.
- **Runtime detection:** the extension is intentionally registered in both `pi.extensions` and `omp.extensions` (`packages/pi-plugin/package.json:72-84`), but it has no runtime OMP identity. This is acceptable only if every shared behavior remains host-neutral. Required change 1 does not meet that condition.
- **Mandatory Pi-path flag:** `subagent-runner.ts` is a shared Pi extension path and was changed. The `MAGIC_CONTEXT_PI_SUBAGENT` environment guard itself remains byte-identical, but its child extension resolution does not.

## Config, data, schema, and fence assessment

- **Magic Context schema/migration:** no schema, migration, `HarnessId`, or database fence changes are in the PR. No #9597-style migration sequencing is triggered.
- **Session discriminator:** OMP deliberately uses the existing `harness='pi'` discriminator; `HarnessId` remains `"opencode" | "pi"`. This avoids a migration, but mixes Pi and OMP session-scoped rows and dashboard presentation. Treat that as an explicit compatibility decision, not evidence of distinct OMP session support.
- **New data/config paths:** OMP adds config, plugin-lock/cache, profile, XDG, and session paths in `paths.ts`; the missing `PI_PACKAGE_DIR`/`PI_CONFIG_FILES` coverage and unscoped effective-config repair are blockers above.
- **Project-tier trust:** Magic Context's own project JSONC loader and its existing sanitization are not modified. OMP's project config is newly consulted indirectly through `omp config get`/`set` in setup and doctor, so the host setting scope must be verified and protected.

## Shared-code blast radius

The following table lists every touched production file that is imported by an existing plain-Pi or OpenCode path. “OMP absent” means no `omp` host should be present; only the listed deliberate behavior changes are acceptable after the required fixes.

| File | Existing consumer(s) | OMP-absent effect in this PR |
|---|---|---|
| `packages/cli/src/adapters/index.ts` | Unified CLI setup/doctor | Adds an `omp` filesystem/PATH probe; selected existing adapter remains the same when OMP is absent. |
| `packages/cli/src/adapters/types.ts` | All CLI adapters | Type-only `omp` union; no direct runtime change. |
| `packages/cli/src/commands/doctor.ts` | Unified doctor | OMP dispatch only; Pi/OpenCode dispatch is unchanged. |
| `packages/cli/src/commands/migrate.ts` | OpenCode-to-Pi migration | Existing Pi target remains unchanged; OMP target adds a new path. |
| `packages/cli/src/commands/setup-pi.ts` | Pi setup | **Changes Pi failure semantics:** after config-write failure it now removes a package entry just added by setup, where pre-PR Pi left it. Useful but unrelated to OMP; split or explicitly review it. |
| `packages/cli/src/commands/setup.ts` | Unified setup | OMP dispatch only; existing branches unchanged. |
| `packages/cli/src/index.ts` | CLI entry point | Help text only for existing commands. |
| `packages/cli/src/lib/harness-select.ts` | Unified setup/doctor | Adds `omp` validation/probing and prompt text; existing selected adapter is unchanged when OMP is absent. |
| `packages/cli/src/lib/paths.ts` | OpenCode and Pi CLI commands/adapters | Existing exported paths are unchanged; only OMP exports are added. |
| `packages/cli/src/lib/v22-backfill-commands.ts` | Shared doctor backfill | Wording only (`Pi-compatible`); no behavior change. |
| `packages/dashboard/src-tauri/src/commands.rs` | Dashboard for OpenCode and Pi | **Changes the Pi model miss path** by probing/returning OMP models; otherwise an extra failed probe. |
| `packages/dashboard/src-tauri/src/pi_sessions.rs` | Pi dashboard session source, consumed by shared DB/dashboard paths | **Changes results even without an OMP binary** by scanning `PI_CODING_AGENT_DIR` and any on-disk `.omp` roots. |
| `packages/pi-plugin/package.json` | Pi package installation/loading | Marks Pi host peers optional and adds an OMP manifest. Verify package-manager behavior on stock Pi in the new CI smoke test. |
| `packages/pi-plugin/src/subagent-runner.ts` | Pi extension, ctx-aug, dreamer | **Changes explicit extension paths whenever `PI_CODING_AGENT_DIR` is set**, even on stock Pi. |
| `packages/plugin/scripts/build-config-docs.ts` | Docs generation | Terminology only; no runtime behavior. |

The other touched production files are OMP-only additions (`adapters/omp.ts`, `commands/doctor-omp.ts`, `commands/setup-omp.ts`, and `lib/omp-helpers.ts`) or documentation/tests.

## Test-quality assessment

Good local coverage exists for JSON parsing, some rollback behavior, profile-name validation, and selected error paths. It is not sufficient to validate a new harness contract:

- `omp-helpers.test.ts` uses `process.execPath`, not OMP.
- `adapters/omp.test.ts` provides one shell-script response with a hand-written `plugin list --json` shape.
- `paths-omp.test.ts` validates the PR's own path reimplementation rather than host-observed paths.
- `setup-omp.test.ts` calls only `beforeWrite` with a fake script; it does not execute plugin registration or the full wizard transaction.
- `doctor-omp.test.ts` injects all OMP operations and tests only a synthetic healthy response/missing binary.
- The dashboard test parses a hand-authored model JSON string but does not test detection, model-source labelling, or Pi/OMP separation.
- There is no OMP-absent test for the changed Pi runner or dashboard scanner.

No schema test is needed because no schema changed. Real-host CI and negative-direction tests are needed because the failure modes here are silent path/protocol mismatches rather than ordinary type failures.

## Maintenance cost

The OMP delta is partly contained (four substantial OMP-only CLI modules), but it is also smeared through the Pi setup wizard, Pi child runner, shared path module, unified adapter/selection commands, migration command, and dashboard Pi scanner/model endpoint. Future Pi-parity work would now need to preserve three modes: stock Pi, OMP running Pi-compatible extension semantics, and OMP-specific host paths/settings. Keep host detection and dashboard/session/model sources separate, and give the provider/path logic one explicit tested compatibility boundary; otherwise this PR adds a harness leg that CI cannot safely maintain.

## Checks performed

- Fetched `origin pull/256/head` as local ref `pr-256`; reviewed the full merge-base diff and all changed-file names.
- `git diff --check 437435b73c96571d35194fce85c9f9235f588899...pr-256` passed (no whitespace errors).
- Confirmed no CI workflow/configuration file is in the PR diff.
- Compared OMP path and extension assumptions with the upstream OMP documentation for profiles, `PI_*` path variables, config precedence, and legacy Pi-compatible extension loading. Exact supported-version behavior still needs a pinned real-host CI test.
