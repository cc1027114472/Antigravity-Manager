# Linux Server Git + Docker Deployment Implementation Plan

> **For agentic workers:** Execute this plan task-by-task with checkpoints. Steps use checkbox syntax for tracking.

**Goal:** Let a Linux server clone or update this repository, build the existing headless Docker image locally, and start Antigravity Manager with persistent data and server-local secrets.

**Architecture:** A checked-out repository under `/opt/antigravity-manager` is the Docker build context. `deploy/server/deploy.sh` performs a fast-forward-only Git update and runs `docker compose -f docker/docker-compose.server.yml up -d --build`. The Compose file publishes only `127.0.0.1:8045` and mounts `/opt/antigravity-manager/data` into `/root/.antigravity_tools`.

**Tech Stack:** Bash, Git, Docker Compose, existing Rust/Tauri headless Dockerfile.

---

### Task 1: Add production server Compose configuration

**Files:**
- Create: `docker/docker-compose.server.yml`

- [ ] Define a service named `antigravity-manager` using the existing `docker/Dockerfile` and repository root as build context.
- [ ] Configure `restart: unless-stopped`, `init: true`, `127.0.0.1:8045:8045`, and the persistent host directory `/opt/antigravity-manager/data:/root/.antigravity_tools`.
- [ ] Load `API_KEY`, `WEB_PASSWORD`, `LOG_LEVEL`, `ABV_MAX_BODY_SIZE`, and `ABV_PUBLIC_URL` from `.env` with safe defaults for non-secret optional values.
- [ ] Set `ABV_BIND_LOCAL_ONLY=false` because the host port itself is restricted to loopback.

### Task 2: Add server deployment script and secret template

**Files:**
- Create: `deploy/server/deploy.sh`
- Create: `deploy/server/.env.example`

- [ ] Make the script executable and use `set -Eeuo pipefail`.
- [ ] Support environment overrides `REPO_URL`, `DEPLOY_DIR`, `DEPLOY_BRANCH`, and `COMPOSE_FILE`.
- [ ] Clone on first run, preserve `.env`, create the data directory, and use `git fetch` plus `git merge --ff-only` on subsequent runs.
- [ ] Validate Git, Docker, and Compose before changing the checkout; fail without deleting data or unrelated containers.
- [ ] Run `docker compose config`, `docker compose build`, and `docker compose up -d` from the deployment directory.
- [ ] Print the previous and new revisions, container status, and a local HTTP check; return nonzero when the service is not running.
- [ ] Document that `.env` must be created on the server and must never be committed.

### Task 3: Document operator workflow

**Files:**
- Create: `docs/deployment-linux.md`

- [ ] Document Docker installation prerequisites, first-time setup, update commands, logs, status, local health verification, reverse proxy target, and explicit rollback.
- [ ] Include commands using the repository's actual paths and compose filename.
- [ ] Explain that the script does not remove the existing `cli-proxy-api` container.

### Task 4: Validate and commit deployment changes

**Files:**
- Verify: `docker/docker-compose.server.yml`
- Verify: `deploy/server/deploy.sh`
- Verify: `deploy/server/.env.example`
- Verify: `docs/deployment-linux.md`

- [ ] Run `bash -n deploy/server/deploy.sh`.
- [ ] Run `npm run build` to ensure the current frontend build remains valid.
- [ ] Run `docker compose -f docker/docker-compose.server.yml config` when Docker Compose is available.
- [ ] Review the diff and ensure no credentials or unrelated untracked files are staged.
- [ ] Commit only the four deployment files with a focused commit message.

### Task 5: Push and deploy to the server

- [ ] Push the deployment commit to `origin/main`.
- [ ] Connect to `154.36.173.146` over SSH and clone/update the repository under `/opt/antigravity-manager`.
- [ ] Create `/opt/antigravity-manager/.env` from the template and set secrets without printing them.
- [ ] Run `bash deploy/server/deploy.sh`.
- [ ] Verify the container, port 8045, and application logs; leave `cli-proxy-api` stopped.
