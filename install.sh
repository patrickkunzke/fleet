#!/usr/bin/env bash
# Development install: symlink the CLI and the skill, create the database.
# Once the Rust binary exists this is replaced by `brew install me/tap/fleet`.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${FLEET_BIN_DIR:-$HOME/.local/bin}"
SKILL_DIR="$HOME/.claude/skills/board"

mkdir -p "$BIN_DIR"
ln -sfn "$ROOT/cli/board.sh" "$BIN_DIR/fleet"
printf 'cli    %s -> %s\n' "$BIN_DIR/fleet" "$ROOT/cli/board.sh"

if [ -e "$SKILL_DIR" ] && [ ! -L "$SKILL_DIR" ]; then
  printf 'skill  %s exists and is not a symlink — leaving it alone\n' "$SKILL_DIR" >&2
else
  ln -sfn "$ROOT/skills/board" "$SKILL_DIR"
  printf 'skill  %s -> %s\n' "$SKILL_DIR" "$ROOT/skills/board"
fi

"$ROOT/cli/board.sh" init

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) printf '\nnote: %s is not on your PATH\n' "$BIN_DIR" >&2 ;;
esac
