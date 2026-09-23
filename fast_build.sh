#!/usr/bin/env bash
set -e

BUILD_DIR="/tmp/abv-build"
SRC_DIR="/mnt/d/GOWorks/fanzhongli/Antigravity-Manager"

echo "=== 同步源码 ==="
rsync -a \
  --exclude 'node_modules' \
  --exclude 'dist' \
  --exclude 'target' \
  --exclude '.git' \
  "$SRC_DIR/src-tauri/" "$BUILD_DIR/src-tauri/"

echo "=== 增量编译 Rust 二进制 ==="
cd "$BUILD_DIR/src-tauri"
cargo build --release --bin antigravity_tools

echo "=== 复制回 Windows ==="
cp "$BUILD_DIR/src-tauri/target/release/antigravity_tools" "$SRC_DIR/antigravity-tools-linux"
echo "=== 完成 ==="
ls -lh "$SRC_DIR/antigravity-tools-linux"
