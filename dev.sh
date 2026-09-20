#!/usr/bin/env bash
# Redraw the fleet on every save.
#
# `fleet preview` invents its own board and its own tmux server, so this can
# run beside a real fleet without touching it — and a layout change is a save
# away from being on screen instead of a restart away.
#
#   ./dev.sh                 the session pane at 110x32
#   ./dev.sh 140x40 graph    a size and a view (session, graph, log)
#
# Deliberately no cargo-watch or fswatch: one fewer thing to install on a
# machine where the whole point is a fast loop.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SIZE="${1:-110x32}"
VIEW="${2:-session}"
BIN="$ROOT/target/debug/fleet"

# Debug, not release: this loop is about how soon the frame appears, and the
# installed binary is a symlink to the release one that must not change under
# the fleet somebody has open.
newest() {
  find "$ROOT/src" "$ROOT/Cargo.toml" -type f -print0 \
    | xargs -0 stat -f '%m' 2>/dev/null \
    | sort -rn | head -1
}

printf 'watching %s — %s, %s view. ^C to stop.\n' "${ROOT##*/}" "$SIZE" "$VIEW"

last=""
while :; do
  now="$(newest)"
  if [ "$now" != "$last" ]; then
    last="$now"
    if out="$(cargo build --manifest-path "$ROOT/Cargo.toml" 2>&1)"; then
      clear
      "$BIN" preview --size "$SIZE" --view "$VIEW"
      printf '\n%s  %s\n' "$(date +%H:%M:%S)" "$SIZE $VIEW"
    else
      clear
      printf '%s\n' "$out"
    fi
  fi
  sleep 0.4
done
