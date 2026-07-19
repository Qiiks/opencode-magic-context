# Isolated Docker rig for the rust-mode clone drive

## Why
Driving cloned OpenCode sessions via `opencode -s <id>` on the host boots a second full OpenCode server in-process. That second server contends with the user's production `opencode serve` (same opencode.db, same context.db, shared CPU), and today it stalled the prod serve and caused a cache-bust storm on the user's live sessions. Ruling: all drive OpenCode runs move into a Docker container with COPIES of every piece of data they need. Nothing the container does may touch the host's opencode.db, context.db, auth.json, or config files.

## Deliverables (all under scripts/drive-rig/ in this repo, committed)

1. `Dockerfile` — linux/arm64, base with bun + curl + git + sqlite3 + socat + tmux. Install OpenCode inside the image via the official install script (curl -fsSL https://opencode.ai/install | bash) pinned/verified to the CURRENT host version (run `opencode --version` on host; install the same version inside; the installer supports version pinning via env or flag, check it). Do NOT copy the macOS binary.
2. `prepare.sh` (runs on host) — builds a snapshot dir (default ~/.cache/mc-drive-rig/snapshot/):
   - opencode.db: consistent copy via `sqlite3 "$src" "VACUUM INTO '$dst'"` (never cp of a live WAL db). Source: ~/.local/share/opencode/opencode.db
   - context.db: same VACUUM INTO. Source: ~/.local/share/cortexkit/magic-context/context.db
   - auth: copy ~/.local/share/opencode/auth.json (contains the provider keys; the container uses ollama-cloud/deepseek-v4-flash for beats, plus anthropic when needed).
   - config: copy ~/.config/cortexkit/magic-context.jsonc AND ~/.config/opencode/ (config + tui.json if present). Strip nothing; the container gets identical config, EXCEPT: remove any `shadow_transform` block and keep `subc` connection block (see bridge below).
   - repo under test: fresh git clone of ~/Work/Projects/CortexKit/benchmarks into the snapshot (plus its .cortexkit/magic-context.jsonc which carries transform_mode:"rust" — verify it survives the clone; it may be untracked, so copy the .cortexkit dir explicitly).
   - plugin dists: copy packages/plugin/dist from THIS repo checkout on the host (the drive must run the freshest dist; the user config references the dev path — rewrite the plugin path in the container's opencode config to the mounted dist location).
3. subc bridge (rust mode needs the HOST ck-mc daemon; macOS UDS cannot be bind-mounted into a linux container):
   - `bridge-host.sh`: socat TCP4-LISTEN:8790,bind=127.0.0.1,reuseaddr,fork UNIX-CONNECT:<socket path from ~/.local/share/cortexkit/run/subc-connection.json>. Parse the socket path from the connection file with jq at start; refuse to start if missing.
   - container entry runs: socat UNIX-LISTEN:<same path as host connection file names>,fork TCP4:host.docker.internal:8790 in the background, and places a copy of the connection file at the same absolute path inside the container so the subc-client resolves identically. Result: the container plugin talks to the host daemon transparently; module store state for the drive session (already seeded) carries over.
4. `run.sh` — docker build + docker run with: --cpus=4 --memory=8g, no host volume mounts except the snapshot dir (read-write, it IS the container's data), --add-host=host.docker.internal:host-gateway equivalent for Docker Desktop (it resolves natively on macOS Docker Desktop, verify), env HOME set so XDG paths resolve to the snapshot layout. Inside: tmux session `drive` started detached. The operator attaches with `docker exec -it mc-drive tmux attach`.
5. `verify.sh` — proves isolation and function from outside:
   - container running, tmux alive
   - `opencode --version` inside matches host
   - launch `opencode -s ses_OqknfoW2O3LTOcjLvOMQoREVPtz1` inside tmux, wait, then assert: the MC log INSIDE the container shows a `rust pass:` line for the session (bridge + rust mode working); the HOST opencode.db and context.db mtimes did NOT change during the container turn (isolation proof).

## Constraints
- The clone session ses_OqknfoW2O3LTOcjLvOMQoREVPtz1 must exist in the snapshot opencode.db (it does on host; VACUUM INTO preserves it).
- Do not install or run subc/broca inside the container; the bridge is the design.
- Everything scripted and idempotent: re-running prepare.sh rebuilds the snapshot from current host state; run.sh tears down a previous container (docker rm -f mc-drive) before starting.
- If the OpenCode linux install script fails on arm64 or the version pin is impossible, STOP and report rather than improvising a different binary source.
- Keep scripts dependency-light: bash + jq + sqlite3 + docker only on the host side.

## Gates
- shellcheck-clean scripts (if shellcheck present; otherwise careful quoting), Dockerfile builds, verify.sh passes end-to-end on this machine. Report the verify.sh output verbatim, including the host-mtime isolation proof and the rust-pass log line from inside the container.
- Commit everything under scripts/drive-rig/ with a README.md documenting the flow (prepare → bridge-host → run → verify → docker exec tmux attach). No em-dashes in any text you write.
