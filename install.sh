#!/bin/sh
# ratatosk installer: downloads the latest GitHub release for this machine and
# installs the binary.
#
#   curl -fsSL https://raw.githubusercontent.com/maxeliseyev/ratatosk/main/install.sh | sh
#
# Options (environment variables):
#   RATATOSK_VERSION   tag to install, e.g. v0.2.0 (default: latest release)
#   RATATOSK_PREFIX    install directory (default: /usr/local/bin, falls back to ~/.local/bin)
#   RATATOSK_REPO      GitHub repo (default: maxeliseyev/ratatosk)
#
# The old VPROXY_VERSION / VPROXY_PREFIX / VPROXY_REPO / VPROXY_LIBC names are
# still accepted for scripts written before the rename.
set -eu

REPO="${RATATOSK_REPO:-${VPROXY_REPO:-maxeliseyev/ratatosk}}"
VERSION="${RATATOSK_VERSION:-${VPROXY_VERSION:-}}"
PREFIX="${RATATOSK_PREFIX:-${VPROXY_PREFIX:-}}"

say() { printf '%s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required"; }

need curl
need tar

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin)
    case "$arch" in
      arm64|aarch64) asset="ratatosk-macos-arm64" ;;
      x86_64) asset="ratatosk-macos-x86_64" ;;
      *) die "unsupported macOS architecture: $arch" ;;
    esac ;;
  Linux)
    case "$arch" in
      x86_64|amd64) asset="ratatosk-linux-x86_64" ;;
      aarch64|arm64) asset="ratatosk-linux-arm64" ;;
      *) die "unsupported Linux architecture: $arch (build from source: cargo build --release)" ;;
    esac
    # Static musl build on Alpine and other systems without glibc, or on request.
    libc="${RATATOSK_LIBC:-${VPROXY_LIBC:-}}"
    if [ -z "$libc" ]; then
      if [ -f /etc/alpine-release ] || ! (ldd --version 2>&1 | grep -qi glibc); then
        libc=musl
      fi
    fi
    [ "$libc" = musl ] && asset="$asset-musl" ;;
  *) die "unsupported OS: $os" ;;
esac

if [ -z "$VERSION" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$VERSION" ] || die "cannot determine the latest release of $REPO"
fi

base="https://github.com/$REPO/releases/download/$VERSION"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "Downloading $asset $VERSION ..."
if ! curl -fsSL -o "$tmp/$asset.tar.gz" "$base/$asset.tar.gz" 2>/dev/null; then
  # Releases cut before the rename still carry vproxy-* assets.
  legacy_asset="$(printf '%s' "$asset" | sed 's/^ratatosk-/vproxy-/')"
  [ "$legacy_asset" != "$asset" ] || die "cannot download $asset.tar.gz"
  say "No $asset in $VERSION; trying legacy $legacy_asset"
  asset="$legacy_asset"
  curl -fsSL -o "$tmp/$asset.tar.gz" "$base/$asset.tar.gz" \
    || die "cannot download $asset.tar.gz"
fi
curl -fsSL -o "$tmp/$asset.tar.gz.sha256" "$base/$asset.tar.gz.sha256" \
  || die "cannot download $asset.tar.gz.sha256 (refusing to install an unverified binary)"
expected="$(awk '{print $1}' "$tmp/$asset.tar.gz.sha256")"
if command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$tmp/$asset.tar.gz" | awk '{print $1}')"
else
  actual="$(sha256sum "$tmp/$asset.tar.gz" | awk '{print $1}')"
fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || die "sha256 mismatch for $asset.tar.gz"
say "sha256 OK"

tar -C "$tmp" -xzf "$tmp/$asset.tar.gz"
bin="$tmp/$asset/ratatosk"
[ -x "$bin" ] || bin="$tmp/$asset/vproxy"
[ -x "$bin" ] || die "binary not found in the archive"

if [ -z "$PREFIX" ]; then
  if [ -w /usr/local/bin ] || command -v sudo >/dev/null 2>&1; then
    PREFIX=/usr/local/bin
  else
    PREFIX="$HOME/.local/bin"
  fi
fi
mkdir -p "$PREFIX" 2>/dev/null || true
if [ -w "$PREFIX" ]; then
  install -m 755 "$bin" "$PREFIX/ratatosk"
else
  say "Installing to $PREFIX needs sudo"
  sudo install -m 755 "$bin" "$PREFIX/ratatosk"
fi

say "Installed: $PREFIX/ratatosk ($("$PREFIX/ratatosk" --version 2>/dev/null || echo "$VERSION"))"
case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *) say "note: $PREFIX is not in PATH" ;;
esac
cat >&2 <<MSG

Next steps:
  echo 'vless://...' > server.txt
  ratatosk -f server.txt -p 1080            # local SOCKS5 proxy
  sudo ratatosk --tui --tun -f server.txt   # system-wide (asks to install sing-box if missing)
  docker run -p 1080:1080 ghcr.io/maxeliseyev/ratatosk -s 'https://...'   # or the container
  ratatosk service install -f server.txt    # keep it running in the background
MSG
