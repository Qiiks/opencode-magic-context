# Git dedup heuristic: project-identity content anchors

> **S10 / SUBC note.** This document extracts the project-identity resolution
> semantics in `packages/plugin/src/features/magic-context/memory/project-identity.ts`.
> The resulting anchor is a **deduplication heuristic only**: it is advisory
> evidence and must never trigger an automatic merge. SUBC measured a 1-in-3
> false-positive rate among root-commit collisions in a 171-repository sample.
> Merge operations remain exclusively merge-directive operations (S10).

## Contract

The content-anchor derivation takes a directory path and returns either:

- `git:<lexmin-root-sha>`; or
- `NONE` when the directory has no usable root-commit anchor.

`NONE` is the cross-implementation projection of a strict resolution failure.
It is distinct from the production wrapper's `dir:<md5-12>` fallback. The
wrapper remains allowed to use that directory identity for Magic Context's
local storage, but a `dir:` fallback is **not** a content anchor and must be
reported as `NONE` by this heuristic.

The source's strict operation is `resolveProjectIdentityStrict(directory)`.
The production `resolveProjectIdentity(directory)` wraps it with fallback and
cooldown policy; both layers are specified below because a Rust implementation
that probes during the source cooldown can produce different evidence.

## Derivation

### 1. Directory and `.git` discovery

1. Compute `canonical = path.resolve(input_directory)`. This is lexical path
   normalization, not a filesystem realpath.
2. Check the directory itself and every lexical ancestor for an entry named
   `.git` with `existsSync`. A `.git` **file** counts; this is required for
   worktrees and submodules. `existsSync` follows a symlink at the `.git`
   entry, so a symlinked git metadata entry counts as well. A directory must
   first be accessible and must actually be a directory.
3. If the lexical walk does not find `.git`, call `realpathSync.native` on the
   resolved directory and repeat the ancestor walk on that real path. This is
   the symlinked-checkout path. A failed realpath does not make the path a git
   repository.
4. If neither walk finds `.git`, strict resolution stops without launching
   git and fails as `not_git_repo`; the anchor is `NONE`.

The successful identity cache is process-local and keyed by `path.resolve`
(the cache stores only successful `git:` identities). A cache hit is returned
before the directory-stat and `.git` checks. A directory fallback is cached
only for the no-`.git` case. This means a directory that later gains a `.git`
entry is re-probed rather than being stuck on its old `dir:` identity.

### 2. Exact git probe

For a path with git metadata, invoke exactly:

```text
git rev-list --max-parents=0 HEAD
```

The source calls `execFileSync` with:

```text
cwd: canonical
encoding: "utf8"
env: { ...process.env, LC_ALL: "C", LANG: "C" }
stdio: ["ignore", "pipe", "pipe"]
timeout: 5000ms
```

The 5,000 ms timeout is part of the contract. The locale settings make the
error text classification stable. No remote URL, branch name, filesystem
basename, or commit message participates in the anchor.

Parse stdout by splitting on newlines. For each line, trim whitespace, take
at most the first 64 characters, and retain it only if it matches
`^[0-9a-f]{7,64}$`. Sort the retained strings in ascending JavaScript default
string order and take the first one. Git emits full lowercase 40-hex object
IDs for this command, so the normative comparison is byte-wise lexicographic
comparison of the complete 40-hex strings (ASCII order: `0`–`9`, then `a`–`f`),
not comparison of a prefix and not traversal order. The source's `slice(0, 64)`
and 7–64 parser bounds are included for exact compatibility with unusual git
output, but a normal successful result is always a full 40-hex SHA.

Return `git:<selected_root_sha>`, cache it under `canonical`, remember it as
the last successful git identity, and clear any cooldown for that path.

### 3. Multiple roots and traversal order

`rev-list --max-parents=0 HEAD` can print more than one root for a history
with unrelated roots (for example, a merge made with
`--allow-unrelated-histories`). Do **not** use the first output line. The
anchor is the lexicographic minimum of the valid root set. This makes the
anchor a pure function of the root set even if git enumerates that set in a
different order.

### 4. Failures, `NONE`, and cooldown

Strict failures never return a `git:` anchor. The relevant classifications are:

| Class | Source classification | Cooldown behavior in the production wrapper |
| --- | --- | --- |
| `not_git_repo` | No `.git` fast-path metadata, or stderr contains `not a git repository`, `does not have any commits yet`, `ambiguous argument 'HEAD'`, or `unknown revision or path` | No cooldown for the no-metadata fast path. An empty repository has `.git`, so its failed probe enters the metadata fallback path below. |
| `git_timeout` | `ETIMEDOUT`, `SIGTERM`, `SIGKILL`, or an error with `killed === true` | Cooldown when `.git` metadata is present. |
| `git_missing` | git spawn error `ENOENT` | Cooldown when `.git` metadata is present. |
| `dubious_ownership` | stderr contains `detected dubious ownership` | Cooldown when `.git` metadata is present; the source also records a one-shot warning recommending `git config --global --add safe.directory <canonical>`. |
| `unknown` | Any other git failure, including no valid root hash in successful-looking stdout | Cooldown when `.git` metadata is present. |
| `permission_denied` | `EACCES` or `EPERM` while accessing the directory or spawning git | Not silently coerced by `resolveProjectIdentity`; the `OrFallback` belt-and-suspenders wrapper may return a `dir:` identity, which is still `NONE` for this anchor contract. |

For every non-`permission_denied` strict error, the production wrapper
computes `fallback = dir:<md5-12>` from the UTF-8 MD5 of the full lexical
`path.resolve` string. If `.git` metadata is present, it records a cooldown
until `now + 5 * 60 * 1000` milliseconds. During that five-minute window it
**does not invoke git again**:

- if a successful `git:` identity is known for the exact path, an ancestor,
  or the corresponding realpath ancestor, return that last-known `git:`
  identity;
- otherwise return the deterministic `dir:` fallback.

The ancestor lookup is why a previously resolved repository can keep a
nested session on the same git identity during a transient failure. A
cold-start failure therefore returns a `dir:` value from the production
wrapper but `NONE` to this heuristic. After the cooldown expires, the next
call probes git again. A successful probe refreshes the identity and clears
cooldown. Explicit test-only cooldown clearing has the same re-probe effect.

For a no-`.git` directory, the wrapper caches the `dir:` fallback and does not
set a cooldown. For an empty repository, `.git` exists and `rev-list` fails;
the source consequently takes the metadata branch and applies the five-minute
cooldown even though the eventual anchor remains `NONE`.

### 5. `$HOME` session guard

The low-level resolver does not itself protect `$HOME`. The session entry point
`resolveProjectIdentityForSession(directory, allowHomeProject = false)` does:

- realpath-canonicalize both the configured home and the input;
- treat the exact home directory **and any directory whose nearest git root is
  the home directory** as the protected home project;
- return `undefined` unless `allowHomeProject` is true; and
- when opted in, return `dir:<md5-12(canonical-home)>` for both the home and its
  descendants, never a git content anchor.

An implementation of this spec used as a content-anchor resolver should report
`NONE` for that guarded session case. A normal repository below `$HOME` whose
nearest git root is not `$HOME` remains eligible for `git:` resolution.

## Resolution edge-case family

The expected outputs below are also pinned in
[`git-dedup-goldens.json`](./git-dedup-goldens.json). The generator creates
real repositories in a temporary directory, derives expected values with a
separate real-git probe, and validates every value against the TypeScript
strict resolver before writing or byte-comparing the fixture.

| Case | Expected output | Required interpretation |
| --- | --- | --- |
| ordinary repository | `git:<its full root SHA>` | A repository with one commit anchors to that commit. |
| non-git directory | `NONE` | The `.git` stat-walk fast path avoids launching git. |
| empty repository | `NONE` | `.git` is present, but `HEAD` has no commit; the wrapper uses a fallback and cooldown, not a content anchor. |
| grafted/unrelated history | `git:<lexmin of all root SHAs>` | The fixture makes two independent roots and merges them with `--allow-unrelated-histories`; output order is not authoritative. |
| shallow clone | `git:<shallow tip SHA>` | `rev-list --max-parents=0 HEAD` sees the shallow boundary as a root. It does **not** recover the original repository's true root. |
| submodule directory | `git:<submodule's own root SHA>` | The submodule's `.git` file is found and git runs with the submodule as `cwd`; never use the parent superproject's root. |
| linked worktree | `git:<main checkout's root SHA>` | The worktree `.git` file resolves to the same history. |
| direct bare-repository directory | `NONE` | A bare repository has no `.git` entry, so this source resolver's required stat-walk does not recognize the bare directory. |
| linked worktree of a bare repository | `git:<bare repository's root SHA>` | The linked checkout has a `.git` file and is the resolvable form. |
| trailing slash | same `git:` anchor as the repository path | `path.resolve` removes the spelling difference. |
| symlinked checkout path | same `git:` anchor as the target | The realpath fallback finds the target's `.git` ancestry. |
| `$HOME` or a child inheriting `$HOME/.git` through the session entry point | `NONE` | The session guard returns `undefined` by default, or a home `dir:` identity after opt-in; neither is a content anchor. |

The direct bare-directory result is intentional: “bare repo if resolvable”
means that a linked worktree (or another checkout path with `.git` metadata)
can resolve it; passing the bare object directory itself cannot satisfy this
implementation's `.git` discovery precondition.

Transient timeout, missing-git, dubious-ownership, and permission behavior is
specified normatively above rather than placed in the fixture. A real git
fixture cannot hermetically force a 5-second process timeout, a missing binary,
or OS/user ownership errors without changing the environment or using the
existing test hooks. The fixture does cover the real empty-repository failure
and the no-metadata fast path.

## False-positive family (same anchor is not proof of sameness)

These are deliberately confident-looking collisions. In every case below the
implementations **must produce the same anchor**, and the consumer must retain
that result as advisory evidence only:

- **Forks sharing history.** The original and a fork can retain the same root
  commit even after their branches diverge.
- **Vendored copies with intact `.git` history.** A copy embedded or carried
  into another project still exposes the original repository's root.
- **Monorepo splits preserving the root.** A split checkout or extracted
  working tree that retains the monorepo history shares the monorepo root,
  even if its visible files are different.
- **Backup copies.** `ponder` and `ponderbak` can be distinct working
  directories with the same intact git history and therefore the same anchor.
- **Separate repositories sharing early history.** The synthetic `pi` and
  `pi-mono` cases model SUBC's measured false positive: separate repositories
  can diverge after a shared early root and still collide on this heuristic.

The golden fixture contains paired entries for each family member and pins the
same `git:<sha>` string for both members. Golden-tested agreement on a false
positive is worse than divergence if it makes a collision look verified; the
warning and these enumerated limitations are part of the contract. A consumer
must not auto-merge, rewrite, archive, or otherwise combine project state from
an anchor alone. Merges happen exclusively through explicit S10
merge-directives.

## Consumers and fixture ownership

Entorhinal's Rust implementation must:

1. implement the same directory metadata walk, exact git command, timeout,
   locale, root parsing, full-SHA byte-wise lexmin rule, and `NONE` projection;
2. golden-test against this repository's
   `docs/specs/git-dedup-goldens.json` **by path reference**, not a copied
   fixture; and
3. treat `git:<sha>` as dedup evidence only. Merge authorization comes only
   from merge-directives (S10), never from a matching anchor.

The TypeScript-side command is:

```text
bun packages/plugin/scripts/gen-git-dedup-goldens.ts
```

It builds all synthetic cases with real git, validates every generated case by
calling `resolveProjectIdentityStrict`, and compares an existing fixture's
serialized bytes before accepting regeneration. Run it twice and compare the
fixture bytes; the committed fixture must be byte-identical across runs.

## Fixture content pin (cross-repo consumers)

The golden fixture `git-dedup-goldens.json` is consumed across repositories
(entorhinal's Rust dedup arm golden-tests against it by path reference). A
path-referenced golden with no content pin can drift silently: a
regeneration here changes what the consumer asserts without any commit on
the consumer's side, and CI stays green on both because each repo is
internally consistent — while the fixture's whole job is to catch exactly
a two-implementation divergence.

Rule (precedent: the CK-wire golden drift between broca and magic-context):

- Current fixture SHA-256:
  `ffced5697238bd60f58cc9e9be48ff7fcad01950313985fb5b97dc3ac45dfd49`
- Every consuming test in another repository MUST pin this hash as a
  constant and fail loud on mismatch.
- Any regeneration of the fixture MUST update this section's hash in the
  same commit, and the consuming repositories' pins are updated in a
  coordinated change. A pin mismatch in a consumer is the intended loud
  signal that the contract moved.

Scope note on path-variant coverage: the path-variant cases in THIS fixture
assert git-anchor invariance (the derivation reaches the same anchor from
`/p`, `/p/`, and a symlink to `/p`). They do NOT exercise cortexkit-paths
ancestor-canonicalisation of vanished paths — that is a different function
with a different failure mode, covered by the subconscious-side fixture
(SUBC's claimed item from the cutover D2 finding). Two fixtures, two
properties; neither substitutes for the other.
