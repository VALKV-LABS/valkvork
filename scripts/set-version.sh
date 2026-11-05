#!/usr/bin/env bash
# Stamp a release version into every workspace crate, so the binaries report
# it in --version (the golden image installer checks that). The release
# workflow runs this on its checkout; the committed version stays as it is.
#
#   scripts/set-version.sh 0.1.3
set -euo pipefail

version="${1:?usage: set-version.sh X.Y.Z}"
version="${version#v}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a version: $version" >&2; exit 1; }
cd "$(dirname "$0")/.."

for manifest in crates/*/Cargo.toml; do
  # The first `version = ` line is the [package] version.
  awk -v v="$version" '!done && /^version = "/ { print "version = \"" v "\""; done = 1; next } { print }' \
    "$manifest" > "$manifest.tmp" && mv "$manifest.tmp" "$manifest"
done

# Keep Cargo.lock in step, so `cargo build --locked` still works.
awk -v v="$version" '
  /^name = "valkvork(-[a-z]+)?"$/ { ours = 1; print; next }
  ours && /^version = "/ { print "version = \"" v "\""; ours = 0; next }
  { ours = (ours && $0 != "") ? ours : 0; print }
' Cargo.lock > Cargo.lock.tmp && mv Cargo.lock.tmp Cargo.lock

echo "stamped version $version"
