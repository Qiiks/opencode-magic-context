# Rust hermetic E2E CI gates

The Rust hermetic E2E gate exercises the production-shaped path:

```text
opencode serve → Magic Context plugin → ck-subc → ckdev-mc-e2e
```

[`scripts/run-rust-hermetic-e2e.sh`](../scripts/run-rust-hermetic-e2e.sh) is the
single invocation for the local release script and both CI jobs. It derives test
files from `packages/e2e-tests/mode-manifest.json`, verifies the private Rust
source workspaces, builds the current `ck-subc` and `ckdev-mc-e2e` pair into its
e2e-owned Cargo target directory, and requires a real positive Bun pass summary.
A missing prerequisite, crash, or zero-test run is never treated as a pass.

## Active design: option A — GitHub-hosted Ubuntu runners

The release workflow runs `E2E (Rust hermetic)` on `ubuntu-latest` for `v*` tags.
The nightly drift job in `ci.yml` runs the same hosted leg at 03:17 UTC from the
default branch. The latter is deliberately schedule-only: it does not run on
pushes or pull requests, so private-source credentials cannot reach PR code.

Ubuntu is the correct hosted OS for this stack. The harness rejects only Windows
and otherwise uses portable Unix facilities (process spawning, signals, XDG
directories, and daemon sockets); it has no macOS-only branch. GitHub-hosted
Linux is therefore source-feasible. The cost class is **medium recurring hosted
minutes and cache storage**: plan 25–45 minutes from a cold cache and 12–25
minutes from a warm cache. Those are planning estimates; record the first cold
and warm run durations before making a service-level claim.

Each job checks out `cortexkit/commons` and `cortexkit/subconscious` beside
`$GITHUB_WORKSPACE`, matching this repository's `../commons` and
`../subconscious` Cargo path dependencies. It restores/saves only
`packages/e2e-tests/.cache/rust-e2e-cargo-target`, keyed by this repository's
`Cargo.lock`, both sibling lockfiles, runner OS, and architecture. The shared
script performs the authoritative builds into that cache and runs the suite.

## Required Actions secrets

Create these **repository** Actions secrets on `cortexkit/magic-context`:

| Secret | Value shape | Repository granted access |
| --- | --- | --- |
| `COMMONS_READ_DEPLOY_KEY` | Complete unencrypted OpenSSH `ed25519` private key, including `-----BEGIN OPENSSH PRIVATE KEY-----` and `-----END OPENSSH PRIVATE KEY-----`; do not base64-encode or quote it. | `cortexkit/commons` only |
| `SUBCONSCIOUS_READ_DEPLOY_KEY` | Complete unencrypted OpenSSH `ed25519` private key, including its begin/end lines; do not base64-encode or quote it. | `cortexkit/subconscious` only |

The workflow's raw `ssh-agent` step writes both keys to a temporary `0700`
directory, adds them only long enough to clone the siblings, then stops the
agent and removes the temporary key files before installing dependencies or
running repository code. Do not replace these with a personal access token or a
single reused deploy key: GitHub deploy keys are repository-scoped, and two
independent read-only keys keep the blast radius to the required sources.

### Mint and install the read-only deploy keys

Run the following from an administrator workstation authenticated to GitHub with
permission to manage deploy keys on both sibling repositories and Actions
secrets on `cortexkit/magic-context`. The commands keep key contents out of
shell arguments and history.

```bash
KEY_DIR="$HOME/.config/cortexkit/magic-context-rust-e2e-deploy-keys"
install -d -m 700 "$KEY_DIR"

ssh-keygen -t ed25519 -a 100 \
  -f "$KEY_DIR/commons-read-deploy-key" \
  -N '' \
  -C 'git@github.com:cortexkit/commons.git'
ssh-keygen -t ed25519 -a 100 \
  -f "$KEY_DIR/subconscious-read-deploy-key" \
  -N '' \
  -C 'git@github.com:cortexkit/subconscious.git'

# `gh repo deploy-key add` is read-only unless --allow-write is supplied.
gh repo deploy-key add "$KEY_DIR/commons-read-deploy-key.pub" \
  --repo cortexkit/commons \
  --title 'magic-context Rust hermetic CI (read-only)'
gh repo deploy-key add "$KEY_DIR/subconscious-read-deploy-key.pub" \
  --repo cortexkit/subconscious \
  --title 'magic-context Rust hermetic CI (read-only)'

# Read private-key contents from stdin; never paste them into a command line.
gh secret set COMMONS_READ_DEPLOY_KEY --repo cortexkit/magic-context \
  < "$KEY_DIR/commons-read-deploy-key"
gh secret set SUBCONSCIOUS_READ_DEPLOY_KEY --repo cortexkit/magic-context \
  < "$KEY_DIR/subconscious-read-deploy-key"
```

Confirm that both secret names appear in **Settings → Secrets and variables →
Actions** for `cortexkit/magic-context`, then remove the local private-key copies
according to the team's approved key-retention policy. Keep each public key
listed as a read-only deploy key on only its matching sibling repository.

No repository variable enables this lane. If either secret is absent, the
credential-preflight job succeeds with a GitHub Actions warning titled **Rust
hermetic E2E skipped** and a run-summary entry naming the exact missing secret.
The Rust job is visibly skipped. Release publishing accepts only that explicit
skipped state; an enabled Rust job that fails blocks publishing.

## First hosted-run check

After minting the secrets, inspect the **E2E (Rust hermetic)** job summary. Its
first line has this exact shape, with the two 40-character commit IDs:

```text
Rust hermetic sibling checkouts: commons=<sha>; subconscious=<sha>
```

Confirm both SHAs are the intended sibling revisions, then confirm the shared
script's `Build ck-subc and ckdev-mc-e2e, then run Rust hermetic e2e` step passed.
The nightly drift job writes the same line. This makes the first secret-backed
run verifiable at a glance without revealing either key.

## Security and maintenance

Read-only deploy keys still let trusted workflow code read private sibling
source. Protect release tags and workflow changes, restrict who can modify the
default branch, and rotate or revoke each key immediately if a secret may have
been exposed. The release job is tag-only and the CI job is schedule-only; do
not add either private-source job to `pull_request`, fork-triggered, or manual
unprotected workflows.

## Option B: m1bench self-hosted runner — RETIRED

> **RETIRED:** `m1bench` no longer exists. No workflow targets a self-hosted
> `m1bench` label or relies on a persistent runner checkout.

The former option B design used a dedicated Mac runner with pre-provisioned
sibling source. It is retained here only as historical context. Do not recreate
its repository variable, runner label, or persistent sibling checkout path;
option A's hosted deploy-key design is the active release and nightly gate.

## Alternative C: prebuilt daemon artifact (not wired)

A prebuilt `ck-subc` binary alone does not make the current harness portable.
`buildHermeticBinaries()` deliberately builds current-tree `ck-mc`, which
compiles against private `commons` and `subconscious` path dependencies. A future
artifact design needs signed, versioned binaries plus a compatibility tuple and
either private published crates or an immutable signed source bundle for both
siblings. Until then, the hosted source checkouts remain the safe current design.
