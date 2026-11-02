#!/usr/bin/env bash
set -e
source $HOME/.cargo/env
cd /mnt/d/GOWorks/fanzhongli/Antigravity-Manager/src-tauri
cargo build --release --bin antigravity_tools
ls -lh target/release/antigravity_tools
