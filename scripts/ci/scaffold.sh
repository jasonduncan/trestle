#!/usr/bin/env bash
# `trestle new demo` against this checkout: build, test, migrate, verify,
# and export OpenAPI, exactly as a new user would.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
base="${TRESTLE_TEST_DATABASE_URL:-postgres://localhost/postgres}"
work="$(mktemp -d)"
db="trestle_ci_scaffold_$$"
psql "$base" -qc "CREATE DATABASE $db"
trap 'psql "$base" -qc "DROP DATABASE IF EXISTS $db WITH (FORCE)"' EXIT

cargo build -q -p trestle-cli
trestle="$root/target/debug/trestle"
cd "$work"
"$trestle" new demo --trestle-path "$root"
cd demo
cp "$root/Cargo.lock" .
export CARGO_TARGET_DIR="$root/target/scaffold"
export DATABASE_URL="${base%/*}/$db"
cargo test
"$trestle" check
"$trestle" migrate
"$trestle" verify
"$trestle" openapi export > openapi.json
test -s openapi.json
echo "Scaffold OK"
