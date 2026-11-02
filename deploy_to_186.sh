#!/usr/bin/env bash
set -e
REMOTE="186.241.81.6"
PASS="qtvdHXCR2420"

echo "=== 1. Uploading release binary and dist.tar.gz to $REMOTE ==="
sshpass -p "$PASS" scp -o StrictHostKeyChecking=no /mnt/d/GOWorks/fanzhongli/Antigravity-Manager/src-tauri/target/release/antigravity_tools root@$REMOTE:/opt/antigravity-manager/antigravity-tools
sshpass -p "$PASS" scp -o StrictHostKeyChecking=no /mnt/d/GOWorks/fanzhongli/Antigravity-Manager/dist.tar.gz root@$REMOTE:/opt/antigravity-manager/dist.tar.gz

echo "=== 2. Rebuilding and recreating container on $REMOTE ==="
sshpass -p "$PASS" ssh -o StrictHostKeyChecking=no root@$REMOTE bash << 'EOF'
set -e
cd /opt/antigravity-manager
chmod +x antigravity-tools
rm -rf dist
mkdir -p dist
tar -zxf dist.tar.gz -C dist
echo "Extracted dist assets:"
ls -lh dist/assets

docker build -f Dockerfile.prebuilt -t antigravity-manager:server .
docker rm -f antigravity-manager 2>/dev/null || true
docker run -d \
  --name antigravity-manager \
  --restart unless-stopped \
  --env-file /opt/antigravity-manager/.env \
  -p 127.0.0.1:8045:8045 \
  -v /opt/antigravity-manager/data:/app/data \
  antigravity-manager:server

sleep 3
docker ps | grep antigravity-manager
curl -I http://127.0.0.1:8045/
EOF

echo "=== Deployment to 186.241.81.6 Complete! ==="
