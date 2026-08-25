# Rust hermetic E2E release gate

The release workflow has a dedicated `E2E (Rust hermetic)` job. It exercises the
production-shaped path:

```text
opencode serve → Magic Context plugin → ck-subc → ckdev-mc-e2e
```

The invocation is [`scripts/run-rust-hermetic-e2e.sh`](../scripts/run-rust-hermetic-e2e.sh),
the same script used by `scripts/release.sh`. It derives all test files from
`packages/e2e-tests/mode-manifest.json`, verifies the private Rust source
workspaces, and requires a real positive Bun pass summary. It never converts a
missing prerequisite or zero collected tests into a pass.

## Active design: m1bench self-hosted release runner

The selected design is a dedicated Mac runner named and labelled `m1bench`. The
job has `runs-on: [self-hosted, m1bench]`, is present only in `release.yml`, and
that workflow is triggered only by `push.tags: v*`. It does **not** run for
`pull_request`, including forks. The job further narrows its `GITHUB_TOKEN` to
`contents: read` and does not receive a private-repository deploy key or PAT.

The job is enabled only after the repository variable
`RUST_E2E_M1BENCH_ENABLED` is set to the literal value `true`. Until then, the
hosted preflight job emits a visible GitHub Actions warning named
`Rust hermetic E2E skipped` and the self-hosted job is visibly **Skipped** in
the release graph. This is intentional: a green preflight never falsely claims
the Rust test ran.

> GitHub Actions cannot reliably query whether a matching self-hosted runner is
> online before scheduling a job. Do not set the variable until m1bench is online
> and has the required sibling checkouts. If it goes offline after enablement,
> the release job remains queued and blocks publication rather than being skipped.

### Register the runner

Run these commands as the dedicated non-admin runner account on m1bench. They
make the standard Actions workspace layout place this repository at
`~/actions-runner/_work/magic-context/magic-context`, so the Cargo paths
`../commons` and `../subconscious` resolve correctly.

```bash
# On an administrator workstation with access to cortexkit/magic-context.
# Registration tokens expire quickly; create one immediately before config.sh,
# then transfer its value to m1bench through an approved secret channel.
gh api --method POST repos/cortexkit/magic-context/actions/runners/registration-token \
  --jq .token

# On m1bench, as the dedicated runner account. Paste the one-time token only
# into this shell; do not put it in a profile, file, or command history.
read -r -s -p "Runner registration token: " RUNNER_TOKEN; echo
mkdir -p ~/actions-runner && cd ~/actions-runner
RUNNER_TAG="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
  https://github.com/actions/runner/releases/latest | awk -F/ '{print $NF}')"
RUNNER_VERSION="${RUNNER_TAG#v}"
curl -fL -o actions-runner.tar.gz \
  "https://github.com/actions/runner/releases/download/${RUNNER_TAG}/actions-runner-osx-arm64-${RUNNER_VERSION}.tar.gz"
tar xzf actions-runner.tar.gz
./config.sh --url https://github.com/cortexkit/magic-context \
  --token "$RUNNER_TOKEN" --name m1bench --labels m1bench \
  --work _work --unattended
sudo ./svc.sh install
sudo ./svc.sh start
```

Confirm the runner is **Idle** in **Settings → Actions → Runners**. GitHub adds
the `self-hosted` label automatically; confirm both `self-hosted` and `m1bench`
are shown. Do not register this runner at organization scope or attach it to a
public runner group that permits arbitrary repositories.

Install Rust through `rustup` for the same runner account and accept the Xcode
Command Line Tools licence. The workflow installs Bun and OpenCode per run, but
Cargo, git, `curl`, and the macOS developer tools must be available to the
service account:

```sh
rustup toolchain install stable
rustup default stable
xcode-select --install       # if the Command Line Tools are not already installed
xcodebuild -license accept   # after Xcode or CLT installation, if prompted
```

### Provision and maintain the private sibling sources

The runner must hold read-only working copies beside the Actions checkout. Use
reviewed, compatible commits; do not let a pull-request workflow update these
paths. The release job prints each revision before tests start. Provision them
from an operator account that already has private-repository access, then remove
write permission from the runner account. Do not leave a private Git key under
the persistent runner account.

```sh
# On m1bench, replace these quoted placeholders with the actual accounts.
RUNNER_ACCOUNT="REPLACE_WITH_RUNNER_ACCOUNT"
OPERATOR_ACCOUNT="REPLACE_WITH_OPERATOR_ACCOUNT"
WORK_PARENT="/Users/${RUNNER_ACCOUNT}/actions-runner/_work/magic-context"
sudo install -d -o "$RUNNER_ACCOUNT" -g staff "$WORK_PARENT"
STAGING="$(sudo -u "$OPERATOR_ACCOUNT" mktemp -d)"
sudo -u "$OPERATOR_ACCOUNT" git clone git@github.com:cortexkit/commons.git "$STAGING/commons"
sudo -u "$OPERATOR_ACCOUNT" git clone git@github.com:cortexkit/subconscious.git "$STAGING/subconscious"
sudo mv "$STAGING/commons" "$STAGING/subconscious" "$WORK_PARENT/"
sudo rmdir "$STAGING"
sudo chown -R "$RUNNER_ACCOUNT":staff "$WORK_PARENT/commons" "$WORK_PARENT/subconscious"
sudo chmod -R a-w "$WORK_PARENT/commons" "$WORK_PARENT/subconscious"

# Before enabling a release, transfer ownership briefly to the operator, fetch
# and checkout the two approved revisions, then return them read-only to the
# runner. Substitute reviewed commits; never use an unreviewed PR head.
sudo chown -R "$OPERATOR_ACCOUNT":staff "$WORK_PARENT/commons" "$WORK_PARENT/subconscious"
sudo -u "$OPERATOR_ACCOUNT" git -C "$WORK_PARENT/commons" fetch origin
# sudo -u "$OPERATOR_ACCOUNT" git -C "$WORK_PARENT/commons" checkout <approved-commons-commit>
sudo -u "$OPERATOR_ACCOUNT" git -C "$WORK_PARENT/subconscious" fetch origin
# sudo -u "$OPERATOR_ACCOUNT" git -C "$WORK_PARENT/subconscious" checkout <approved-subconscious-commit>
sudo chown -R "$RUNNER_ACCOUNT":staff "$WORK_PARENT/commons" "$WORK_PARENT/subconscious"
sudo chmod -R a-w "$WORK_PARENT/commons" "$WORK_PARENT/subconscious"
```

The source relationship is required today: Magic Context's `Cargo.toml` has
path dependencies into both siblings, and the hermetic harness builds
`ck-subc` from `subconscious` plus `ck-mc` from this checkout. A prebuilt
`ck-subc` alone is not an adequate replacement.

Verify the installation from a checkout at
`$WORK_PARENT/magic-context` before enabling the gate:

```sh
scripts/run-rust-hermetic-e2e.sh
```

Enable only after that command passes and the runner is idle:

```sh
gh variable set RUST_E2E_M1BENCH_ENABLED \
  --repo cortexkit/magic-context --body true
```

To intentionally disable the lane during maintenance, delete the variable (or
set it to any value other than `true`). The next tag run will include the loud
preflight warning and a visibly skipped Rust job:

```sh
gh variable delete RUST_E2E_M1BENCH_ENABLED --repo cortexkit/magic-context
```

### Residual security risk

A self-hosted runner is persistent state. A maintainer who can cause a matching
release tag to execute arbitrary repository code can read the sibling source
that the runner account can read. Restrict release-tag creation, protect the
release workflow, use a dedicated runner account and host, keep no deployment
or cloud credentials available to workflow code, and patch/reimage it regularly. The
trigger restriction prevents public PR and fork code from reaching the runner,
but does not remove the trusted-maintainer risk.

## Alternative A: hosted runner with read-only checkout credentials (not wired)

This remains feasible on Linux. The harness rejects only Windows and uses
standard Unix process spawning, signals, XDG directories, and daemon sockets;
it contains no Darwin branch. The repository has separately verified that
`ck-mc` compiles for Linux ARM64, but this change does not repeat a Cargo build
while the release box gate is held.

A hosted implementation needs **two** read-only source credentials, not just
one for `subconscious`:

1. Create an independent `ed25519` deploy key for each private repository:
   `cortexkit/commons` and `cortexkit/subconscious`.
2. Add each public key as a read-only deploy key on its one repository.
3. Store the corresponding PEM private keys as repository Actions secrets named
   `COMMONS_READ_DEPLOY_KEY` and `SUBCONSCIOUS_READ_DEPLOY_KEY`.
4. On a tag-only hosted job, load the keys only long enough to clone both
   siblings beside `$GITHUB_WORKSPACE`; cache only
   `packages/e2e-tests/.cache/rust-e2e-cargo-target`, keyed by the three Cargo
   lockfiles and runner architecture.

Do not use a broad personal access token. A fine-grained token is acceptable
only if it is read-only and restricted to exactly those two repositories, but
two repo-scoped deploy keys have the smaller blast radius. This option exposes
private-source read credentials to any workflow code reachable from a release
tag, so it requires protected tags and workflow review. It is deliberately not
implemented in `release.yml`.

## Alternative C: prebuilt daemon artifact (not wired)

A `ck-subc` binary by itself does **not** make the current harness portable.
`buildHermeticBinaries()` deliberately rebuilds the current-tree `ck-mc` and
warns that mixing it with a prebuilt daemon may exercise incompatible sibling
revisions. In addition, `ck-mc` directly compiles against private `commons` and
`subconscious` path dependencies.

The SUBC-side request for a viable artifact design is therefore:

1. Publish a signed, versioned `ck-subc` binary for macOS ARM64 and Linux
   AMD64/ARM64 with SHA-256 checksums and a machine-readable compatibility
   manifest.
2. Publish the `subc-*` and `cortexkit-*` Rust crate set required by
   `mc-module` to a private registry, **or** publish a signed, immutable source
   bundle containing both sibling revisions.
3. Record a compatibility tuple `(ck-subc version, subc crate revision, commons
   crate revision)` and make the Magic Context job select that tuple rather than
   downloading `latest`.
4. Authenticate downloads with short-lived OIDC or a repository-scoped
   read-only credential; verify checksum and signature before execution.

Until that contract exists, option C is scoped work for the SUBC and commons
owners, not a safe replacement for the m1bench source checkouts.
