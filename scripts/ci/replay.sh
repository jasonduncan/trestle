#!/usr/bin/env bash
# Applies pilot's whole migration history to an empty database and checks
# that the catalog matches it.
set -euo pipefail
base="${TRESTLE_TEST_DATABASE_URL:-postgres://localhost/postgres}"
db="trestle_ci_replay_$$"
url="${base%/*}/$db"
psql "$base" -qc "CREATE DATABASE $db"
trap 'psql "$base" -qc "DROP DATABASE IF EXISTS $db WITH (FORCE)"' EXIT
DATABASE_URL="$url" cargo run -q -p pilot-migrations --bin pilot-migrate
DATABASE_URL="$url" cargo run -q -p pilot -- verify
