# Linux Server Deployment

This project can run on a Linux server in headless mode. The server builds the existing Docker image locally, so Node, Rust, and the native build dependencies remain inside Docker rather than being installed directly on the host.

## Prerequisites

- Ubuntu/Debian Linux with Docker Engine and the Docker Compose plugin.
- Git access to this repository.
- A reverse proxy such as Nginx or Caddy if the Web UI or API will be exposed through HTTPS.

## First deployment

```bash
sudo mkdir -p /opt
sudo git clone https://github.com/cc1027114472/Antigravity-Manager.git /opt/antigravity-manager
cd /opt/antigravity-manager
sudo cp deploy/server/.env.example .env
sudo editor .env
sudo bash deploy/server/deploy.sh
```

Set a strong `API_KEY` and a separate strong `WEB_PASSWORD` in `.env`. Keep this file only on the server; do not commit it.

The application data is persisted at `/opt/antigravity-manager/data`. The service listens on `127.0.0.1:8045`, so it is not directly exposed on the public network.

## Updating

```bash
cd /opt/antigravity-manager
sudo bash deploy/server/deploy.sh
```

The script fetches `origin/main`, performs a fast-forward-only update, rebuilds the image, and recreates the container. It does not remove the data directory or unrelated Docker containers such as `cli-proxy-api`.

## Verify and inspect logs

```bash
docker ps --filter name=antigravity-manager
curl -i http://127.0.0.1:8045/
docker logs --tail=200 -f antigravity-manager
```

For Nginx or Caddy, use `http://127.0.0.1:8045` as the upstream and terminate TLS at the reverse proxy.

## Rollback

The deployment script prints the previous revision. To explicitly roll back:

```bash
cd /opt/antigravity-manager
git log --oneline -5
git checkout <known-good-commit>
docker compose --env-file .env -f docker/docker-compose.server.yml up -d --build
```

Return to the main branch later with:

```bash
git checkout main
git pull --ff-only origin main
```
