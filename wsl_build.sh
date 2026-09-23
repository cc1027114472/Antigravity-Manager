#!/bin/bash
set -e

LINUX_BUILD=/tmp/abv-build
SRC=/mnt/d/GOWorks/fanzhongli/Antigravity-Manager

echo "=== 复制源码到 Linux 路径 ==="
rm -rf "$LINUX_BUILD"
mkdir -p "$LINUX_BUILD"
rsync -a \
    --exclude='node_modules' \
    --exclude='dist' \
    --exclude='target' \
    --exclude='.git' \
    --exclude='release-linux' \
    "$SRC/" "$LINUX_BUILD/"

echo "=== pnpm install ==="
cd "$LINUX_BUILD"
pnpm install 2>&1 | tail -10

echo "=== pnpm build ==="
pnpm run build 2>&1

echo "=== 复制 dist 回 Windows 路径 ==="
rsync -a --delete "$LINUX_BUILD/dist/" "$SRC/dist/"

echo "=== build 完成，dist 内容 ==="
ls -lh "$LINUX_BUILD/dist/"
ls -lh "$LINUX_BUILD/dist/assets/" | head -10
