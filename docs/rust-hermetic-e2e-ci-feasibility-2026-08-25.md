# Rust hermetic E2E CI feasibility adjudication (2026-08-25)

## Decision

Adopt **B: a dedicated `m1bench` self-hosted runner for tag-triggered release
gates**. The implemented release job is disabled by default behind the explicit
repository variable `RUST_E2E_M1BENCH_ENABLED=true`; disabled state produces a
GitHub Actions warning and a visibly skipped job, not a silent green pass. See
[the operator runbook](../.github/RUST_E2E_CI.md).

This is the smallest secure change that exercises the actual current-source
Rust stack on every configured release. It does not mint secrets, register a
runner, or change SUBC infrastructure.

## Source findings

`packages/e2e-tests/src/rust-runner/hermetic-subc.ts` defines the executable
contract:

- `buildHermeticBinaries()` runs `cargo build --release -p mc-module` in this
  checkout, then hard-links or copies the result to `ckdev-mc-e2e`.
- The same function runs `cargo build --release -p subc-core --bins` in the
  sibling `subconscious` checkout and starts the resulting `ck-subc` daemon.
- The daemon runs with a temporary `XDG_RUNTIME_DIR`, writes a connection file,
  and the module is started as an external provider. The harness then starts a
  deterministic Broca producer and `opencode serve` against the same XDG data
  directory.
- Its only platform rejection is `win32`. The remaining APIs are portable Unix
  facilities (`spawn`, `SIGKILL`, `ps -o lstart`, XDG directories, and the
  daemon's TCP/Unix-socket transport); there is no macOS-only branch. Linux is
  therefore source-feasible. The owner-reported prior Linux ARM64 `ck-mc`
  compilation is consistent with this inspection, but was not re-run while the
  box-gate lock is held.

The required source is broader than the daemon checkout. Root `Cargo.toml`
points `cortexkit-*` dependencies at `../commons` and all `subc-*` dependencies
at `../subconscious`. The existing prerequisite detector enforces both
siblings. A daemon binary alone cannot compile current `ck-mc`, and the
harness intentionally avoids pairing `ck-mc` with a stale prebuilt component.

The group contains 31 manifest-derived Rust tests and executes serially with a
600-second Bun timeout. The new shared script checks its manifest selection and
requires a positive pass summary, so a crash, timeout, or zero-test run fails.

## Options

| Shape | Feasibility now | Planning duration* | Cost class | Security posture |
| --- | --- | --- | --- | --- |
| A. GitHub-hosted runner + private source credentials | Technically feasible on Linux, but needs two sibling checkouts and credentials | Cold 25–45 min; cached 12–25 min | Medium recurring hosted minutes and cache storage | Read-only private-source credentials are injected into a public-repo release workflow; tag protection is mandatory |
| B. `m1bench` self-hosted runner | Feasible now because the intended runner already has the private sibling/toolchain shape | Cold after cache wipe 25–45 min; warm 10–25 min | Low GitHub-minute cost; dedicated Mac capacity and maintenance | No Actions secret/PAT; persistent private source on runner means trusted tag code and host hardening remain in scope |
| C. Prebuilt private daemon artifact | Not sufficient as stated; needs an artifact-and-private-crate/source contract | Once upstream work exists: cold 10–25 min; warm 5–15 min | Upfront SUBC/registry/release work; low per-run cost | Best steady-state boundary if signed, pinned, and short-lived authenticated, but unavailable now |

\*These are conservative planning estimates, not measurements. Evidence for
scope is the two release Cargo builds, serial 31-file suite, and 600-second
per-test timeout above. Record cold and cached workflow durations after the
first two enabled runs before making a service-level claim. The repository has
no comparable CI timing trace to justify a more precise number.

### A — hosted runner + deploy keys

**Pros:** ephemeral worker, Linux execution is source-feasible, and
`actions/cache` can retain the e2e-owned Cargo target directory.

**Blockers/cost:** it needs read access to both `commons` and `subconscious`, not
only the daemon repository. Two per-repository read-only deploy keys minimize
scope, but they are still private-source credentials available to release-tag
workflow code. It also pays fresh hosted-cache restore and dependency build
costs.

**Disposition:** documented only. Do not add a broad PAT or unprotected secret
path merely to make this lane run.

### B — m1bench runner

**Pros:** source and toolchains remain local to the dedicated host; no
credential is exposed to Actions; current Cargo paths work without source
rewrites; Cargo's e2e-owned target remains cacheable. It directly meets the
owner directive to run the Rust leg inside every enabled release pipeline.

**Controls:** the self-hosted job exists only in the tag-only release workflow,
uses labels `[self-hosted, m1bench]`, uses a read-only `GITHUB_TOKEN`, and is
not present on `pull_request`. The preflight warning plus skipped job makes an
unprovisioned runner visible. If explicitly enabled runner capacity disappears,
the release blocks queued rather than publishing an untested Rust build.

**Residual risk:** a writer able to create a matching tag can execute code on a
persistent runner holding private sibling sources. Use a dedicated account and
host, protected tags/workflows, no unrelated credentials, and regular reimage.

**Disposition:** selected and implemented.

### C — prebuilt artifact

**Pros:** it is the cleanest eventual CI boundary: the public job fetches a
pinned, signed daemon contract rather than cloning private source trees.

**Blocker:** publishing only `ck-subc` does not solve `ck-mc`'s current private
path dependencies or the harness's same-revision coherence guarantee. SUBC and
commons must publish either compatible private Rust crates or a signed source
bundle alongside a platform binary and compatibility manifest.

**Disposition:** scoped in the runbook for the owning teams; not implemented in
this repository.

## Implemented release behavior

`release.yml` adds a hosted preflight and the self-hosted `E2E (Rust hermetic)`
job. The latter waits for the existing host E2E jobs, restores/saves the
isolated Cargo target cache using all three Cargo lockfiles, builds the plugin,
installs OpenCode, and invokes `scripts/run-rust-hermetic-e2e.sh`. All npm
publish jobs explicitly accept the Rust job's `skipped` result only when the
preflight emitted the visible disabled warning; a failed enabled Rust job blocks
publishing.

No nightly `ci.yml` self-hosted job is added. Release tags are the required
release gate, and avoiding a master-push self-hosted execution surface until
runner operation is established is the safer initial rollout. A future nightly
job must use an explicit `github.event_name == 'push'` and protected-master
condition; it must never run on `pull_request` or fork code.
