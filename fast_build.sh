#!/usr/bin/env bash
set -e

export PATH="$HOME/.cargo/bin:$PATH"

BUILD_DIR="/tmp/abv-build"
SRC_DIR="/mnt/d/GOWorks/fanzhongli/Antigravity-Manager"

mkdir -p "$BUILD_DIR"
mkdir -p "$BUILD_DIR/src"

echo "=== 同步源码与依赖 ==="
rsync -a \
  --exclude 'node_modules' \
  --exclude 'dist' \
  --exclude 'target' \
  --exclude '.git' \
  "$SRC_DIR/src-tauri/" "$BUILD_DIR/src-tauri/"

rsync -a "$SRC_DIR/src/locales" "$BUILD_DIR/src/"
rsync -a "$SRC_DIR/dist" "$BUILD_DIR/"

echo "=== 增量编译 Rust 二进制 ==="
cd "$BUILD_DIR/src-tauri"
cargo build --release --bin antigravity_tools

echo "=== 复制回 Windows ==="
cp "$BUILD_DIR/src-tauri/target/release/antigravity_tools" "$SRC_DIR/antigravity-tools-linux"
echo "=== 完成 ==="
ls -lh "$SRC_DIR/antigravity-tools-linux"
