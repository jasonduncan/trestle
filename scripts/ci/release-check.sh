#!/usr/bin/env bash
# Run on a release tag: the tag, the workspace version and CHANGELOG.md
# must agree. Usage: release-check.sh vX.Y.Z
set -euo pipefail
tag="${1:?usage: release-check.sh vX.Y.Z}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
if [ "v$version" != "$tag" ]; then
  echo "tag $tag does not match workspace version $version" >&2
  exit 1
fi
if ! grep -q "^## \[$version\] - [0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}" CHANGELOG.md; then
  echo "CHANGELOG.md has no dated section for $version" >&2
  exit 1
fi
echo "Release $tag is consistent."
