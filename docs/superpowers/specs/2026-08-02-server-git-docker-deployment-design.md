# Linux Server Git + Docker Deployment Design

## Goal

Provide a repeatable deployment flow in which the Linux server pulls this repository, builds the headless Docker image locally, and starts Antigravity Manager without deleting persistent application data.

## Scope

- Add a production Compose configuration for the headless server runtime.
- Add a server deployment script that bootstraps or updates a checkout and runs the Compose build/start flow.
- Keep secrets in a server-local `.env` file rather than in Git.
- Document first-time installation, updates, verification, and rollback.
- Leave the existing desktop/Tauri release workflows and the unrelated `cli-proxy-api` container unchanged.

## Architecture

The server checkout lives at `/opt/antigravity-manager`. The deployment script uses the configured Git repository and branch, updates the checkout with a fast-forward-only operation, and runs `docker compose` with `docker/Dockerfile`. The application runs in headless mode on container port 8045 and is published only on `127.0.0.1:8045`; an external Nginx or Caddy instance may provide HTTPS and public access.

Application state is stored on the host at `/opt/antigravity-manager/data` and mounted at `/root/.antigravity_tools`. The script never removes this directory, Docker images, or the existing container during a normal deployment.

## Components

### `docker/docker-compose.server.yml`

- Builds the repository's existing `docker/Dockerfile`.
- Uses a stable container name and `restart: unless-stopped`.
- Loads runtime values from the server-local `.env` file.
- Publishes `127.0.0.1:8045:8045`.
- Persists `/root/.antigravity_tools`.

### `deploy/server/deploy.sh`

- Uses strict shell error handling.
- Verifies Git, Docker, Docker Compose, and the repository checkout.
- Clones the repository on first run.
- Fetches the configured branch and performs a fast-forward-only update on later runs.
- Preserves `.env` and the data directory.
- Builds and starts the service, then reports container status and a local TCP/HTTP check.
- Supports `REPO_URL`, `DEPLOY_DIR`, `DEPLOY_BRANCH`, and `COMPOSE_FILE` environment overrides.

### `deploy/server/.env.example`

Documents the required API key and optional web password without containing real credentials.

### `docs/deployment-linux.md`

Documents the complete operator workflow, including first deployment, updates, logs, health checks, and rollback to the previous Git revision.

## Failure and rollback behavior

- A Git update must be fast-forward-only; local server changes are not overwritten.
- A failed Docker build stops the deployment before replacing the running container.
- The previous Git revision is printed before updating so an operator can restore it with an explicit checkout and rebuild.
- Data remains on the host independently of the container lifecycle.
- The script does not stop or remove unrelated containers.

## Verification

- Shell syntax validation with `bash -n`.
- Compose interpolation/config validation where Docker Compose is available.
- Frontend build validation with `npm run build`.
- Review of the generated diff and confirmation that no credentials or user-owned untracked files are staged.
