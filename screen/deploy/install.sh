#!/usr/bin/env bash
# Install xBloom Screen on a Raspberry Pi (Raspberry Pi OS Bookworm or newer, 64-bit).
#
#   curl -fsSL https://raw.githubusercontent.com/uaepro/xbloom-screen/main/screen/deploy/install.sh | sudo bash
#
# Options (pass after `bash -s --` when piping):
#   --binary PATH     install this xbloom-screen binary instead of downloading one
#   --tag TAG         release to download (default: screen-latest, the newest build of main)
#   --from-source     build on the Pi with cargo (Pi 4/5 recommended; ~20 min)
#   --port PORT       admin panel port (default 80)
#   --uninstall       remove the service, binary and udev rule (keeps /var/lib/xbloom-screen)
set -euo pipefail

REPO="uaepro/xbloom-screen"
TAG="screen-latest"
BINARY=""
FROM_SOURCE=0
PORT=80
UNINSTALL=0

while [ $# -gt 0 ]; do
  case "$1" in
    --binary) BINARY="$2"; shift 2 ;;
    --tag) TAG="$2"; shift 2 ;;
    --from-source) FROM_SOURCE=1; shift ;;
    --port) PORT="$2"; shift 2 ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

say() { printf '\033[1;33m▸\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || echo "")"

if [ "$UNINSTALL" -eq 1 ]; then
  systemctl disable --now xbloom-screen.service 2>/dev/null || true
  rm -f /etc/systemd/system/xbloom-screen.service /etc/udev/rules.d/99-xbloom-screen.rules /usr/local/bin/xbloom-screen
  systemctl daemon-reload
  say "removed (data kept in /var/lib/xbloom-screen)"
  exit 0
fi

ARCH="$(uname -m)"
case "$ARCH" in
  aarch64|arm64) ASSET="xbloom-screen-aarch64-linux.tar.gz" ;;
  *) [ -n "$BINARY" ] || [ "$FROM_SOURCE" -eq 1 ] || die "no prebuilt binary for $ARCH — use a 64-bit OS, --binary, or --from-source" ;;
esac

say "installing system packages"
apt-get update -qq
apt-get install -y -qq bluez libinput10 libxkbcommon0 libfontconfig1 libdbus-1-3 ca-certificates curl >/dev/null
systemctl enable --now bluetooth >/dev/null 2>&1 || true
rfkill unblock bluetooth 2>/dev/null || true

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
if [ -n "$BINARY" ]; then
  install -m 0755 "$BINARY" /usr/local/bin/xbloom-screen
elif [ "$FROM_SOURCE" -eq 1 ]; then
  say "building from source (this takes a while)"
  apt-get install -y -qq git build-essential pkg-config libinput-dev libudev-dev libxkbcommon-dev libfontconfig-dev libdbus-1-dev >/dev/null
  if ! command -v cargo >/dev/null; then
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  git clone --depth 1 "https://github.com/$REPO" "$TMP/src"
  (cd "$TMP/src/screen" && cargo build --release -p xbloom-screen --no-default-features --features backend-linuxkms)
  install -m 0755 "$TMP/src/screen/target/release/xbloom-screen" /usr/local/bin/xbloom-screen
else
  say "downloading $ASSET ($TAG)"
  curl -fsSL "https://github.com/$REPO/releases/download/$TAG/$ASSET" -o "$TMP/x.tar.gz"
  tar -xzf "$TMP/x.tar.gz" -C "$TMP"
  install -m 0755 "$TMP/xbloom-screen" /usr/local/bin/xbloom-screen
fi
/usr/local/bin/xbloom-screen --version

say "creating the xbloom service user"
id xbloom >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin xbloom
for g in video render input bluetooth; do
  getent group "$g" >/dev/null && usermod -aG "$g" xbloom
done

fetch() { # fetch <file>: from the checkout next to this script, else from GitHub
  if [ -n "$SRC_DIR" ] && [ -f "$SRC_DIR/$1" ]; then cat "$SRC_DIR/$1"; else curl -fsSL "https://raw.githubusercontent.com/$REPO/main/screen/deploy/$1"; fi
}

say "installing the backlight rule and the service"
fetch 99-xbloom-screen.rules > /etc/udev/rules.d/99-xbloom-screen.rules
udevadm control --reload-rules
udevadm trigger --subsystem-match=backlight || true
fetch xbloom-screen.service | sed "s#--port 80#--port $PORT#" > /etc/systemd/system/xbloom-screen.service
systemctl daemon-reload
systemctl enable --now xbloom-screen.service

IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
SUFFIX=""
[ "$PORT" = "80" ] || SUFFIX=":$PORT"
say "done — the screen should show the setup wizard now"
echo "   admin panel:  http://${IP:-<pi-ip>}${SUFFIX}   (or http://$(hostname).local${SUFFIX})"
echo "   logs:         journalctl -u xbloom-screen -f"
