#!/usr/bin/env bash
# Migrates a database with the last release tag's code, then upgrades it
# with this commit's code and verifies. Applied migrations must keep their
# digests across releases (ledger compatibility). Skips before the first tag.
set -euo pipefail
tag="$(git describe --tags --abbrev=0 --match 'v*' HEAD 2>/dev/null || true)"
if [ -z "$tag" ]; then
  echo "No release tag yet; skipping the upgrade check."
  exit 0
fi
base="${TRESTLE_TEST_DATABASE_URL:-postgres://localhost/postgres}"
db="trestle_ci_upgrade_$$"
url="${base%/*}/$db"
old="$(mktemp -d)"
psql "$base" -qc "CREATE DATABASE $db"
trap 'psql "$base" -qc "DROP DATABASE IF EXISTS $db WITH (FORCE)"; git worktree remove --force "$old" || true' EXIT
git worktree add --detach "$old" "$tag"
echo "Migrating with $tag"
# Its own target directory: same-version crates from two checkouts must
# not share build artifacts.
(cd "$old" && CARGO_TARGET_DIR="$old/target" DATABASE_URL="$url" cargo run -q -p pilot-migrations --bin pilot-migrate)
echo "Upgrading with $(git rev-parse --short HEAD)"
DATABASE_URL="$url" cargo run -q -p pilot-migrations --bin pilot-migrate
DATABASE_URL="$url" cargo run -q -p pilot -- verify
