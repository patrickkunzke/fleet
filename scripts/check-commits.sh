#!/bin/sh
# Fails when a commit in BASE..HEAD, merge commits aside, is not a
# conventional commit. The release reads these to choose the next version.
#
#   sh scripts/check-commits.sh origin/main HEAD
set -eu

base=$1
head=${2:-HEAD}
pattern='^(feat|fix|perf|refactor|docs|test|build|ci|chore|style|revert)(\([a-z0-9._/-]+\))?!?: [^ ]'

bad=$(git log --no-merges --format='%h %s' "$base..$head" | while read -r hash subject; do
  printf '%s\n' "$subject" | grep -Eq "$pattern" || printf '  %s %s\n' "$hash" "$subject"
done)

if [ -n "$bad" ]; then
  echo "Not conventional commits (type(scope): summary, e.g. 'feat: …', 'fix(board): …'):"
  echo "$bad"
  echo
  echo "Reword them with git rebase -i $base, then push again."
  exit 1
fi
echo "every commit is a conventional commit"
