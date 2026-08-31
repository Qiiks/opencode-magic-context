#!/usr/bin/env bash
set -Eeuo pipefail

CONTAINER=${DRIVE_RIG_CONTAINER:-mc-drive}
REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
REMOTE_ROOT=/tmp/mc-mid-turn-ab

if ! command -v docker >/dev/null 2>&1; then
    printf 'required command is missing: docker\n' >&2
    exit 1
fi
if [[ $(docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null || true) != true ]]; then
    printf 'container is not running: %s\n' "$CONTAINER" >&2
    exit 1
fi

# Copy only the instrument and the existing mock provider. The source checkout,
# databases, auth files, and product bundle remain outside this ephemeral path.
docker exec "$CONTAINER" rm -rf "$REMOTE_ROOT"
docker exec "$CONTAINER" mkdir -p "$REMOTE_ROOT/src/mock-provider" "$REMOTE_ROOT/scripts"
docker cp "$REPO_ROOT/packages/e2e-tests/src/mock-provider/server.ts" \
    "$CONTAINER:$REMOTE_ROOT/src/mock-provider/server.ts" >/dev/null
docker cp "$REPO_ROOT/packages/e2e-tests/src/mid-turn-prefix-ab.ts" \
    "$CONTAINER:$REMOTE_ROOT/src/mid-turn-prefix-ab.ts" >/dev/null
docker cp "$REPO_ROOT/packages/e2e-tests/scripts/run-mid-turn-prefix-ab.ts" \
    "$CONTAINER:$REMOTE_ROOT/scripts/run-mid-turn-prefix-ab.ts" >/dev/null

exec docker exec \
    -e ANTHROPIC_API_KEY \
    -e ANTHROPIC_AUTH_TOKEN \
    -e ANTHROPIC_MESSAGES_URL \
    -e MC_MIDTURN_AB_TTL \
    -e MC_MIDTURN_AB_STEPS \
    -e MC_MIDTURN_AB_APPLY_AT \
    -e MC_MIDTURN_AB_DROP_CHARS \
    -e MC_MIDTURN_AB_MODEL \
    "$CONTAINER" bun "$REMOTE_ROOT/scripts/run-mid-turn-prefix-ab.ts" "$@"
