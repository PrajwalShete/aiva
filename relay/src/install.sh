#!/bin/sh
# aiva installer — https://github.com/PrajwalShete/aiva
#
#   curl -fsSL __AIVA_SERVER__/install.sh | sh
#   curl -fsSL __AIVA_SERVER__/install.sh | sh -s -- --no-setup
#
# Options: --version <x.y.z>  --no-setup  --dry-run
# Env:     AIVA_VERSION, AIVA_INSTALL_DIR (default ~/.aiva/bin)
set -eu

SERVER="__AIVA_SERVER__"
VERSION="${AIVA_VERSION:-latest}"
INSTALL_DIR="${AIVA_INSTALL_DIR:-$HOME/.aiva/bin}"
LINK_DIR="$HOME/.local/bin"
RUN_SETUP=1
DRY_RUN=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --no-setup) RUN_SETUP=0; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    *) echo "aiva install: unknown option $1" >&2; exit 2 ;;
  esac
done

say() { printf '%s\n' "$*"; }
fail() { printf 'aiva install: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) os=darwin ;;
  Linux) os=linux ;;
  *) fail "this installer supports macOS and Linux." ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=arm64 ;;
  x86_64 | amd64) arch=x64 ;;
  *) fail "unsupported CPU $(uname -m)." ;;
esac
asset="aiva-$os-$arch"
url="$SERVER/dl/$VERSION/$asset"

if [ "$DRY_RUN" = 1 ]; then
  say "Would download $url"
  say "Would install to $INSTALL_DIR/aiva and link $LINK_DIR/aiva"
  [ "$RUN_SETUP" = 1 ] && say "Would then run: aiva setup"
  exit 0
fi

command -v curl >/dev/null 2>&1 || fail "curl is required."
mkdir -p "$INSTALL_DIR" "$LINK_DIR"
tmp="$INSTALL_DIR/.aiva-download.$$"
trap 'rm -f "$tmp" "$tmp.sha256"' EXIT INT TERM

say "Downloading aiva for $os/$arch..."
curl -fSL --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 300 --retry 3 --retry-delay 1 --progress-bar "$url" -o "$tmp" || fail "download failed: $url"

if curl -fsSL --proto '=https' --connect-timeout 10 --max-time 20 --retry 3 --retry-delay 1 "$url.sha256" -o "$tmp.sha256" 2>/dev/null; then
  expected=$(cut -d ' ' -f 1 < "$tmp.sha256")
  if command -v shasum >/dev/null 2>&1; then actual=$(shasum -a 256 "$tmp" | cut -d ' ' -f 1); else actual=$(sha256sum "$tmp" | cut -d ' ' -f 1); fi
  [ "$expected" = "$actual" ] || fail "checksum mismatch; download is corrupt or tampered with."
fi

chmod 755 "$tmp"
[ "$os" = darwin ] && xattr -d com.apple.quarantine "$tmp" 2>/dev/null || true
[ "$os" = darwin ] && say "Verifying with macOS (the first launch of a new version can take up to ~30 s)..."
"$tmp" --version >/dev/null 2>&1 || fail "the downloaded binary doesn't run on this machine."
mv -f "$tmp" "$INSTALL_DIR/aiva"
ln -sf "$INSTALL_DIR/aiva" "$LINK_DIR/aiva"
say "Installed $("$INSTALL_DIR/aiva" --version) at $INSTALL_DIR/aiva"

case ":$PATH:" in
  *":$LINK_DIR:"*) ;;
  *)
    line='export PATH="$HOME/.local/bin:$PATH"'
    case "${SHELL:-}" in
      */zsh) rc="$HOME/.zshrc" ;;
      */bash) if [ "$os" = darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi ;;
      *) rc="" ;;
    esac
    if [ -n "$rc" ]; then
      grep -qsF "$line" "$rc" || printf '\n# Added by the aiva installer\n%s\n' "$line" >> "$rc"
      say "Added ~/.local/bin to PATH in $rc (new terminals pick it up; or run: . $rc)"
    else
      say "Add ~/.local/bin to your PATH:  $line"
    fi
    ;;
esac

export AIVA_SERVER="$SERVER"
[ "$RUN_SETUP" = 1 ] || exit 0
if [ -f "${AIVA_HOME:-$HOME/.aiva}/config.json" ]; then
  "$INSTALL_DIR/aiva" restart >/dev/null 2>&1 || true
  say "Updated. Your existing setup was kept — check it with: aiva status"
elif (exec </dev/tty) 2>/dev/null; then
  exec "$INSTALL_DIR/aiva" setup --server "$SERVER" </dev/tty
else
  say "Next, run:  aiva setup"
fi
