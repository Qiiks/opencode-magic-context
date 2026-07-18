#!/usr/bin/env bash
set -Eeuo pipefail

CONTAINER=${DRIVE_RIG_CONTAINER:-mc-drive}
CONNECTION_FILE=${DRIVE_RIG_CONNECTION_FILE:-"$HOME/.local/share/cortexkit/run/subc-connection.json"}
SESSION_ID=${DRIVE_RIG_SESSION_ID:-ses_OqknfoW2O3LTOcjLvOMQoREVPtz1}
LOG_PATH=${DRIVE_RIG_LOG_PATH:-/snapshot/magic-context.log}
WAIT_SECONDS=${DRIVE_RIG_WAIT_SECONDS:-180}

if ! command -v docker >/dev/null 2>&1; then
    printf 'required command is missing: docker\n' >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    printf 'required command is missing: jq\n' >&2
    exit 1
fi
if [[ ! -r "$CONNECTION_FILE" ]]; then
    printf 'connection file is not readable: %s\n' "$CONNECTION_FILE" >&2
    exit 1
fi

running=$(docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null || true)
if [[ "$running" != true ]]; then
    printf 'container is not running: %s\n' "$CONTAINER" >&2
    exit 1
fi

tmux_ready=false
for ((elapsed = 0; elapsed < 30; elapsed++)); do
    if docker exec "$CONTAINER" tmux has-session -t drive 2>/dev/null; then
        tmux_ready=true
        break
    fi
    sleep 1
done
if [[ "$tmux_ready" != true ]]; then
    printf 'tmux drive session did not start\n' >&2
    exit 1
fi
echo 'container running: passed'
echo 'tmux drive session: passed'

resolve_opencode() {
    if command -v opencode >/dev/null 2>&1; then
        command -v opencode
    elif [[ -x "$HOME/.opencode/bin/opencode" ]]; then
        printf '%s\n' "$HOME/.opencode/bin/opencode"
    else
        printf 'host opencode executable was not found\n' >&2
        exit 1
    fi
}
host_opencode=$(resolve_opencode)
host_version=$("$host_opencode" --version)
container_version=$(docker exec "$CONTAINER" opencode --version)
if [[ "$container_version" != "$host_version" ]]; then
    printf 'OpenCode version mismatch: host=%s container=%s\n' "$host_version" "$container_version" >&2
    exit 1
fi
printf 'OpenCode version: %s, matches host\n' "$container_version"

subc_port=$(jq -er '(.port // .endpoints[0].port) | numbers' "$CONNECTION_FILE")
docker exec "$CONTAINER" sh -c \
    "socat -T 2 -u /dev/null TCP4:127.0.0.1:${subc_port}"
echo "subc bridge TCP probe on port ${subc_port}: passed"

opencode_db="$HOME/.local/share/opencode/opencode.db"
context_db="$HOME/.local/share/cortexkit/magic-context/context.db"
if [[ ! -f "$opencode_db" || ! -f "$context_db" ]]; then
    printf 'host database is missing\n' >&2
    exit 1
fi
opencode_before=$(stat -f %m "$opencode_db")
context_before=$(stat -f %m "$context_db")
docker exec "$CONTAINER" sh -c "rm -f '$LOG_PATH'"

docker exec "$CONTAINER" tmux send-keys -t drive C-c
sleep 1
docker exec "$CONTAINER" tmux send-keys -t drive "opencode -s $SESSION_ID" C-m
printf 'launched opencode -s %s inside tmux\n' "$SESSION_ID"

rust_line=''
for ((elapsed = 0; elapsed < WAIT_SECONDS; elapsed++)); do
    rust_line=$(docker exec "$CONTAINER" sh -c \
        "grep -F '[${SESSION_ID}] rust pass:' '$LOG_PATH' 2>/dev/null | tail -n 1" || true)
    if [[ -n "$rust_line" ]]; then
        break
    fi
    sleep 1
done

if [[ -z "$rust_line" ]]; then
    printf 'rust pass log line was not observed inside the container after %s seconds\n' "$WAIT_SECONDS" >&2
    docker exec "$CONTAINER" sh -c "tail -n 80 '$LOG_PATH' 2>/dev/null" || true
    exit 1
fi

opencode_after=$(stat -f %m "$opencode_db")
context_after=$(stat -f %m "$context_db")
printf 'host opencode.db mtime: before=%s after=%s\n' "$opencode_before" "$opencode_after"
printf 'host context.db mtime: before=%s after=%s\n' "$context_before" "$context_after"
if [[ "$opencode_before" != "$opencode_after" || "$context_before" != "$context_after" ]]; then
    printf 'host database mtime changed during the container turn\n' >&2
    exit 1
fi
echo 'host database mtime isolation: passed'
printf 'rust pass log line inside container: %s\n' "$rust_line"
echo 'drive rig verification: passed'
