#!/usr/bin/env bash
# Development install: build the binary, link it, link the skill, create the
# database. Replaced by `brew install me/tap/fleet` once there is a tap.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${FLEET_BIN_DIR:-$HOME/.local/bin}"
SKILL_DIR="$HOME/.claude/skills/board"

cargo build --release --manifest-path "$ROOT/Cargo.toml"

mkdir -p "$BIN_DIR"
ln -sfn "$ROOT/target/release/fleet" "$BIN_DIR/fleet"
printf 'cli    %s -> %s\n' "$BIN_DIR/fleet" "$ROOT/target/release/fleet"

if [ -e "$SKILL_DIR" ] && [ ! -L "$SKILL_DIR" ]; then
  printf 'skill  %s exists and is not a symlink — leaving it alone\n' "$SKILL_DIR" >&2
else
  ln -sfn "$ROOT/skills/board" "$SKILL_DIR"
  printf 'skill  %s -> %s\n' "$SKILL_DIR" "$ROOT/skills/board"
fi

"$BIN_DIR/fleet" board init

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) printf '\nnote: %s is not on your PATH\n' "$BIN_DIR" >&2 ;;
esac
