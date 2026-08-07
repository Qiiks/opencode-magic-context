#!/usr/bin/env bash
set -euo pipefail

# Regenerate DG-1..3 and leave the provenance hash in the committed JSON for review.
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
exec bun "$repo_root/crates/mc-module/gen/gen-differential-golden.ts"
