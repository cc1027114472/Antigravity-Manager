#!/usr/bin/env bash
set -euo pipefail

# Keep DNS
printf 'nameserver 8.8.8.8\nnameserver 1.1.1.1\n' > /etc/resolv.conf 2>/dev/null || true

SRC="/mnt/d/GOWorks/fanzhongli/Antigravity-Manager"
BUILD="$HOME/antigravity-linux-build"
OUT="$SRC/release-linux"

WSL_USER=$(getent passwd | awk -F: '$3>=1000 && $3<65534 {print $1; exit}')
if [ "$(id -u)" -eq 0 ]; then
  exec su - "$WSL_USER" -c "bash $SRC/scripts/wsl-build-linux.sh"
fi

source "$HOME/.cargo/env"

# Cargo China mirror
mkdir -p "$HOME/.cargo"
cat > "$HOME/.cargo/config.toml" <<'EOF'
[source.crates-io]
replace-with = "ustc"

[source.ustc]
registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"

[net]
git-fetch-with-cli = true
EOF

echo "==> Preparing build tree at $BUILD"
mkdir -p "$BUILD"
rsync -a --delete \
  --exclude target \
  --exclude node_modules \
  --exclude .git \
  --exclude release-linux \
  "$SRC/src-tauri/" "$BUILD/src-tauri/"
mkdir -p "$BUILD/src"
rsync -a --delete "$SRC/src/locales/" "$BUILD/src/locales/"
rsync -a --delete "$SRC/dist/" "$BUILD/dist/"

# Ensure Go is available for BoringSSL
export PATH="/usr/local/go/bin:/usr/bin:$PATH"
echo "Go: $(go version 2>/dev/null || echo missing)"
echo "Rust: $(rustc -V)"

cd "$BUILD/src-tauri"
echo "==> cargo build --release (this may take a while)"
cargo build --release --bin antigravity_tools

mkdir -p "$OUT"
cp -f "$BUILD/src-tauri/target/release/antigravity_tools" "$OUT/antigravity-tools"
rsync -a --delete "$BUILD/dist/" "$OUT/dist/"

cat > "$OUT/start.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
export ABV_DIST_PATH="${ABV_DIST_PATH:-$DIR/dist}"
export PORT="${PORT:-8045}"
export RUST_LOG="${RUST_LOG:-info}"
# Optional:
#   API_KEY / ABV_API_KEY
#   WEB_PASSWORD / ABV_WEB_PASSWORD
exec "$DIR/antigravity-tools" --headless "$@"
EOF
chmod +x "$OUT/antigravity-tools" "$OUT/start.sh"

cat > "$OUT/README.txt" <<'EOF'
Antigravity Tools - Linux Headless (WebUI)

启动:
  ./start.sh

或:
  ABV_DIST_PATH=./dist PORT=8045 ./antigravity-tools --headless

访问:
  http://localhost:8045

可选环境变量:
  API_KEY / ABV_API_KEY          API 鉴权密钥
  WEB_PASSWORD / ABV_WEB_PASSWORD  Web 登录密码
  PORT                           默认 8045
  ABV_DIST_PATH                  前端静态资源目录

数据目录:
  ~/.antigravity_tools
EOF

echo "==> Done"
ls -lh "$OUT/antigravity-tools"
file "$OUT/antigravity-tools"
echo "Output: $OUT"
