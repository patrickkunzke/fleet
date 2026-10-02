#!/bin/sh
# Puts the fleet binary at target/release/fleet: herdr's build step for fleet.
#
# It downloads the prebuilt binary of the release named by herdr-plugin.toml's
# `version`, for this machine, checks it against the release's SHA256SUMS, and
# falls back to `cargo build --release --locked` when there is no such binary
# or it cannot be checked. A prebuilt binary is the release commit's, so a
# checkout on any other commit, or with changes, builds what it has.
#
#   FLEET_BUILD=source          always build from source
#   FLEET_DOWNLOAD_URL=<url>    download from <url>/v<version>/ instead of the
#                               GitHub release
#
# Adapted from herdr-projects (scripts/install.sh), Copyright (c) 2026 Elias
# Stravik, MIT licensed — see NOTICE.
set -u

cd "$(dirname "$0")/.." || exit 1

say() { printf 'fleet install: %s\n' "$*" >&2; }

build_from_source() {
  [ -n "${tmp:-}" ] && rm -rf "$tmp"
  say "$1"
  say "building from source instead: cargo build --release --locked (a minute or two)"
  if ! command -v cargo >/dev/null 2>&1; then
    say "cargo is not installed. Install Rust (https://rustup.rs), then install fleet again."
    exit 1
  fi
  exec cargo build --release --locked
}

[ "${FLEET_BUILD:-}" = source ] && build_from_source "FLEET_BUILD=source is set"

version=$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' herdr-plugin.toml | head -n 1)
[ -n "$version" ] || build_from_source "herdr-plugin.toml has no version"
tag="v$version"

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  *) build_from_source "there is no prebuilt fleet for $(uname -s)" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) build_from_source "there is no prebuilt fleet for $(uname -m)" ;;
esac
asset="fleet-$arch-$os"

if [ -d .git ] || [ -f .git ]; then
  if [ -n "$(git status --porcelain --untracked-files=no 2>/dev/null)" ]; then
    build_from_source "this checkout has uncommitted changes"
  fi
  head=$(git rev-parse HEAD 2>/dev/null)
  release=$(git rev-parse -q --verify "refs/tags/$tag^{commit}" 2>/dev/null)
  if [ -z "$release" ]; then
    # A clone without tags: ask origin. `^{}` is the commit an annotated tag
    # points at; a lightweight tag has none.
    release=$(GIT_TERMINAL_PROMPT=0 git ls-remote origin "refs/tags/$tag" "refs/tags/$tag^{}" 2>/dev/null |
      awk '{ sha = $1 } $2 ~ /\^\{\}$/ { peeled = $1 } END { print (peeled != "" ? peeled : sha) }')
  fi
  if [ -z "$release" ]; then
    build_from_source "could not find the $tag tag here or on origin"
  elif [ "$head" != "$release" ]; then
    build_from_source "this checkout is not the $tag release commit"
  fi
fi

base="${FLEET_DOWNLOAD_URL:-https://github.com/patrickkunzke/fleet/releases/download}"
base="${base%/}/$tag"

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 2 --connect-timeout 10 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -T 10 -O "$2" "$1"; }
else
  build_from_source "neither curl nor wget is installed"
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  build_from_source "neither sha256sum nor shasum is installed to check the download"
fi

mkdir -p target/release
tmp=$(mktemp -d "target/release/.download.XXXXXX") || build_from_source "could not create a download folder"
trap 'rm -rf "$tmp"' EXIT

say "downloading $asset $tag"
# The binaries are uploaded a few minutes after the release is made; an
# install in between builds from source.
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || build_from_source "the $tag release has no SHA256SUMS yet"
expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || build_from_source "the $tag release has no $asset"
fetch "$base/$asset" "$tmp/$asset" || build_from_source "could not download $base/$asset"
actual=$(sha256 "$tmp/$asset")
[ "$actual" = "$expected" ] ||
  build_from_source "the downloaded $asset does not match SHA256SUMS (got $actual, expected $expected)"

chmod 755 "$tmp/$asset"
reported=$("$tmp/$asset" --version 2>/dev/null)
[ "$reported" = "fleet $version" ] ||
  build_from_source "the downloaded binary did not run or is not $version (it said: ${reported:-nothing})"

# Rename, never copy over: macOS kills a binary rewritten in place.
mv -f "$tmp/$asset" target/release/fleet || build_from_source "could not move the binary into target/release"
say "installed the prebuilt $asset $tag"
