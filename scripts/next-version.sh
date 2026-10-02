#!/bin/sh
# Prints the version the commits since the last release call for, or nothing
# when none of them changes what a user gets.
#
# Conventional commits since the newest vX.Y.Z tag, merge commits aside:
#   a `!` after the type, or a BREAKING CHANGE footer   major
#   feat                                               minor
#   fix, perf                                          patch
#   anything else (docs, ci, chore, refactor, …)       no release
# Before 1.0 a breaking change is a minor, as Cargo reads 0.x versions.
set -eu

last=$(git describe --tags --abbrev=0 --match 'v[0-9]*.[0-9]*.[0-9]*' 2>/dev/null) || {
  echo "next-version: no vX.Y.Z tag to count from" >&2
  exit 1
}
subjects=$(git log --no-merges --format=%s "$last..HEAD")
bodies=$(git log --no-merges --format=%b "$last..HEAD")

if printf '%s\n' "$subjects" | grep -Eq '^[a-z]+(\([^)]*\))?!:' ||
  printf '%s\n' "$bodies" | grep -Eq '^BREAKING[ -]CHANGE:'; then
  bump=major
elif printf '%s\n' "$subjects" | grep -Eq '^feat(\([^)]*\))?:'; then
  bump=minor
elif printf '%s\n' "$subjects" | grep -Eq '^(fix|perf)(\([^)]*\))?:'; then
  bump=patch
else
  exit 0
fi

v=${last#v}
major=${v%%.*}
rest=${v#*.}
minor=${rest%%.*}
patch=${rest#*.}
[ "$major" = 0 ] && [ "$bump" = major ] && bump=minor

case $bump in
  major) echo "$((major + 1)).0.0" ;;
  minor) echo "$major.$((minor + 1)).0" ;;
  patch) echo "$major.$minor.$((patch + 1))" ;;
esac
