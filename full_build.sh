#!/usr/bin/env bash
set -e

BUILD_DIR="/tmp/abv-build"
SRC_DIR="/mnt/d/GOWorks/fanzhongli/Antigravity-Manager"

echo "=== 1. 同步源码到 WSL 临时构建目录 ==="
mkdir -p "$BUILD_DIR"
rsync -a --delete \
  --exclude 'node_modules' \
  --exclude 'dist' \
  --exclude 'target' \
  --exclude '.git' \
  "$SRC_DIR/" "$BUILD_DIR/"

cd "$BUILD_DIR"

echo "=== 2. 前端构建 ==="
export PATH="$HOME/.cargo/bin:$PATH"
pnpm run build

echo "=== 3. 复制前端构建产物回 Windows ==="
mkdir -p "$SRC_DIR/dist"
rsync -a "$BUILD_DIR/dist/" "$SRC_DIR/dist/"

echo "=== 4. Rust 后端编译 (Release) ==="
cd "$BUILD_DIR/src-tauri"
cargo build --release --bin antigravity_tools

echo "=== 5. 复制 Linux 二进制回 Windows ==="
cp "$BUILD_DIR/src-tauri/target/release/antigravity_tools" "$SRC_DIR/antigravity-tools-linux"

echo "=== 编译完成 ==="
ls -lh "$SRC_DIR/antigravity-tools-linux"
