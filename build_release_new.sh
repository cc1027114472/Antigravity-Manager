#!/usr/bin/env bash
set -e
source $HOME/.cargo/env
cd /mnt/d/GOWorks/fanzhongli/Antigravity-Manager
tar -zcf dist.tar.gz -C dist .
cd src-tauri
cargo check --bin antigravity_tools
cargo build --release --bin antigravity_tools
ls -lh target/release/antigravity_tools
