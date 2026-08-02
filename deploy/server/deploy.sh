#!/usr/bin/env bash
set -Eeuo pipefail

REPO_URL="${REPO_URL:-https://github.com/cc1027114472/Antigravity-Manager.git}"
DEPLOY_DIR="${DEPLOY_DIR:-/opt/antigravity-manager}"
DEPLOY_BRANCH="${DEPLOY_BRANCH:-main}"
COMPOSE_FILE="${COMPOSE_FILE:-docker/docker-compose.server.yml}"

log() {
  printf '[deploy] %s\n' "$*"
}

fail() {
  printf '[deploy] ERROR: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

require_command git
require_command docker

docker compose version >/dev/null 2>&1 || fail 'Docker Compose plugin is unavailable'

if [[ -e "$DEPLOY_DIR" && ! -d "$DEPLOY_DIR" ]]; then
  fail "deployment path exists and is not a directory: $DEPLOY_DIR"
fi

if [[ ! -d "$DEPLOY_DIR/.git" ]]; then
  if [[ -e "$DEPLOY_DIR" && -n "$(find "$DEPLOY_DIR" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
    fail "deployment directory is not an empty Git checkout: $DEPLOY_DIR"
  fi

  log "cloning $REPO_URL into $DEPLOY_DIR"
  mkdir -p "$(dirname "$DEPLOY_DIR")"
  git clone --branch "$DEPLOY_BRANCH" --single-branch "$REPO_URL" "$DEPLOY_DIR"
fi

cd "$DEPLOY_DIR"
[[ -f "$COMPOSE_FILE" ]] || fail "Compose file not found: $COMPOSE_FILE"

if [[ ! -f .env ]]; then
  fail "missing $DEPLOY_DIR/.env; create it from deploy/server/.env.example"
fi

previous_revision="$(git rev-parse HEAD)"
log "updating checkout from origin/$DEPLOY_BRANCH"
git fetch --prune origin "$DEPLOY_BRANCH"
git merge --ff-only "origin/$DEPLOY_BRANCH"
new_revision="$(git rev-parse HEAD)"

mkdir -p "$DEPLOY_DIR/data"

log 'validating Docker Compose configuration'
docker compose --env-file .env -f "$COMPOSE_FILE" config >/dev/null

log 'building the server image'
docker compose --env-file .env -f "$COMPOSE_FILE" build

log 'starting Antigravity Manager'
docker compose --env-file .env -f "$COMPOSE_FILE" up -d

sleep 2
container_status="$(docker inspect --format '{{.State.Status}}' antigravity-manager 2>/dev/null || true)"
[[ "$container_status" == running ]] || {
  docker compose --env-file .env -f "$COMPOSE_FILE" ps
  docker compose --env-file .env -f "$COMPOSE_FILE" logs --tail=100
  fail "antigravity-manager is not running"
}

if command -v curl >/dev/null 2>&1; then
  curl --fail --silent --show-error --max-time 10 http://127.0.0.1:8045/ >/dev/null \
    || log 'warning: HTTP root check did not return success; inspect logs before exposing the service'
fi

log "deployment complete: $new_revision"
log "previous revision: $previous_revision"
log 'persistent data: /opt/antigravity-manager/data'
