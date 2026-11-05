#!/usr/bin/env bash
# Print the tag the given commit (default HEAD) should get, or nothing if it
# should get none. Used by .github/workflows/tag.yml after CI passes on main.
#
#   - no vX.Y.Z tag yet         → v<version in crates/valkvork-controller/Cargo.toml>
#   - the commit already tagged, or older than the latest tag → nothing
#   - otherwise, from the commit messages since the last tag:
#       "#skip-tag" on this commit → nothing
#       "#major"     → X+1.0.0
#       "#minor"     → X.Y+1.0        default  → X.Y.Z+1
set -euo pipefail

commit="${1:-HEAD}"
cd "$(dirname "$0")/.."

semver_tags() { git tag -l 'v[0-9]*.[0-9]*.[0-9]*' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' || true; }

if [ -n "$(git tag --points-at "$commit" | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' || true)" ]; then
  exit 0   # already tagged
fi

last=$(semver_tags | sort -V | tail -1)
if [ -z "$last" ]; then
  echo "v$(sed -n 's/^version = "\(.*\)"$/\1/p' crates/valkvork-controller/Cargo.toml | head -1)"
  exit 0
fi

# CI runs can finish out of order: never tag a commit older than the latest tag.
git merge-base --is-ancestor "$commit" "$last" && exit 0

# "#skip-tag" applies to the commit being tagged only.
git log -1 --format=%B "$commit" | grep -q '#skip-tag' && exit 0
messages=$(git log --format=%B "$last..$commit")

IFS=. read -r major minor patch <<< "${last#v}"
if grep -q '#major' <<< "$messages"; then
  echo "v$((major + 1)).0.0"
elif grep -q '#minor' <<< "$messages"; then
  echo "v$major.$((minor + 1)).0"
else
  echo "v$major.$minor.$((patch + 1))"
fi
