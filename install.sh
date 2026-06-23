#!/usr/bin/env bash
# Build lite-cli in release mode and install it to PREFIX (default ~/.local/bin).
# On macOS, re-sign after copy — `cp` invalidates the ad-hoc signature on Apple
# Silicon and the kernel then kills the binary (zsh: killed, exit 137).
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local/bin}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$SCRIPT_DIR/target/release/lite"

echo "==> building release"
cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"

echo "==> installing to $PREFIX/lite"
mkdir -p "$PREFIX"
cp "$BIN" "$PREFIX/lite"

if [ "$(uname -s)" = "Darwin" ]; then
  echo "==> re-signing (macOS)"
  codesign -s - -f "$PREFIX/lite"
fi

echo "==> done"
"$PREFIX/lite" --version || true

case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *) echo "note: $PREFIX is not on your PATH — add it to use \`lite\` directly" ;;
esac
