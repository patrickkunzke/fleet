#!/bin/sh
# Puts the fleet binary at bin/fleet in this plugin's folder: what `/fleet
# start` runs when there is no fleet on PATH.
#
# It downloads the prebuilt binary of the release named by the plugin's
# `version`, for this machine, and checks it against the release's
# SHA256SUMS, as scripts/install.sh does for herdr. Unlike that, it cannot
# build from source: an installed plugin has no source. It says why it gave
# up instead, and installing fleet another way works as well.
#
#   FLEET_DOWNLOAD_URL=<url>    download from <url>/v<version>/ instead of the
#                               GitHub release
set -u

cd "$(dirname "$0")/.." || exit 1

fail() { printf 'fleet: %s\n' "$*" >&2; exit 1; }

version=$(sed -n 's/^ *"version": *"\([^"]*\)".*/\1/p' .claude-plugin/plugin.json | head -n 1)
[ -n "$version" ] || fail "the plugin's manifest has no version"
tag="v$version"

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  *) fail "there is no prebuilt fleet for $(uname -s)" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) fail "there is no prebuilt fleet for $(uname -m)" ;;
esac
asset="fleet-$arch-$os"

base="${FLEET_DOWNLOAD_URL:-https://github.com/patrickkunzke/fleet/releases/download}"
base="${base%/}/$tag"

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 2 --connect-timeout 10 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -T 10 -O "$2" "$1"; }
else
  fail "neither curl nor wget is installed to download fleet"
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  fail "neither sha256sum nor shasum is installed to check the download"
fi

mkdir -p bin
tmp=$(mktemp -d "bin/.download.XXXXXX") || fail "could not create a download folder in $(pwd)/bin"
trap 'rm -rf "$tmp"' EXIT

fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "could not download $base/SHA256SUMS: the $tag binaries may still be building, a few minutes after a release"
expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "the $tag release has no $asset"
fetch "$base/$asset" "$tmp/$asset" || fail "could not download $base/$asset"
actual=$(sha256 "$tmp/$asset")
[ "$actual" = "$expected" ] || fail "the downloaded $asset does not match SHA256SUMS (got $actual, expected $expected)"

chmod 755 "$tmp/$asset"
reported=$("$tmp/$asset" --version 2>/dev/null)
[ "$reported" = "fleet $version" ] || fail "the downloaded binary did not run or is not $version (it said: ${reported:-nothing})"

# Rename, never copy over: macOS kills a binary rewritten in place.
mv -f "$tmp/$asset" bin/fleet || fail "could not move the binary into $(pwd)/bin"
printf 'fleet: installed %s %s at %s/bin/fleet\n' "$asset" "$tag" "$(pwd)"
