#!/usr/bin/env bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

# Keep DNS working
printf 'nameserver 8.8.8.8\nnameserver 1.1.1.1\n' > /etc/resolv.conf

# Switch to Aliyun mirrors
sed -i 's|http://archive.ubuntu.com/ubuntu|http://mirrors.aliyun.com/ubuntu|g' /etc/apt/sources.list 2>/dev/null || true
sed -i 's|http://security.ubuntu.com/ubuntu|http://mirrors.aliyun.com/ubuntu|g' /etc/apt/sources.list 2>/dev/null || true
if [ -f /etc/apt/sources.list.d/ubuntu.sources ]; then
  sed -i 's|http://archive.ubuntu.com/ubuntu|http://mirrors.aliyun.com/ubuntu|g' /etc/apt/sources.list.d/ubuntu.sources
  sed -i 's|http://security.ubuntu.com/ubuntu|http://mirrors.aliyun.com/ubuntu|g' /etc/apt/sources.list.d/ubuntu.sources
fi

pkill -9 apt-get 2>/dev/null || true
pkill -9 apt 2>/dev/null || true
pkill -9 dpkg 2>/dev/null || true
sleep 1
dpkg --configure -a || true

apt-get update -qq
apt-get install -y -qq \
  curl wget build-essential pkg-config file \
  libssl-dev libgtk-3-dev libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev \
  libsoup-3.0-dev libjavascriptcoregtk-4.1-dev \
  perl cmake clang libclang-dev git ca-certificates \
  golang-go

# Node.js 20 from NodeSource (or Ubuntu node if NodeSource fails)
if ! command -v node >/dev/null 2>&1; then
  if curl -fsSL https://deb.nodesource.com/setup_20.x | bash -; then
    apt-get install -y -qq nodejs
  else
    apt-get install -y -qq nodejs npm
  fi
fi

WSL_USER=$(getent passwd | awk -F: '$3>=1000 && $3<65534 {print $1; exit}')
echo "WSL_USER=$WSL_USER"

if [ -n "$WSL_USER" ] && [ ! -x "/home/$WSL_USER/.cargo/bin/rustc" ]; then
  # Use Chinese rustup mirror if helpful
  export RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
  export RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup
  su - "$WSL_USER" -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"
fi

echo "=== versions ==="
node -v
npm -v
su - "$WSL_USER" -c 'source "$HOME/.cargo/env" && rustc -V && cargo -V'
