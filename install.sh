#!/usr/bin/env bash
# Development install: build the binary and put it on your PATH. The /board
# skill needs no installing: fleet starts every agent with it loaded.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${FLEET_BIN_DIR:-$HOME/.local/bin}"

cargo build --release --manifest-path "$ROOT/Cargo.toml"

mkdir -p "$BIN_DIR"
ln -sfn "$ROOT/target/release/fleet" "$BIN_DIR/fleet"
printf 'cli    %s -> %s\n' "$BIN_DIR/fleet" "$ROOT/target/release/fleet"

# No board to make: each fleet makes its own, the first time fleet is started
# in its workspace.

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) printf '\nnote: %s is not on your PATH\n' "$BIN_DIR" >&2 ;;
esac
