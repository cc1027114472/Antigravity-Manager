#!/usr/bin/env bash
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
export ABV_DIST_PATH="${ABV_DIST_PATH:-$DIR/dist}"
export PORT="${PORT:-8045}"
export RUST_LOG="${RUST_LOG:-info}"
# Optional:
#   API_KEY / ABV_API_KEY
#   WEB_PASSWORD / ABV_WEB_PASSWORD
exec "$DIR/antigravity-tools" --headless "$@"
