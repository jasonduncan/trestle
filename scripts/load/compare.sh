#!/usr/bin/env bash
# M5 load comparison (design doc §19.3): the pilot API on Trestle versus a
# hand-written Axum baseline doing the same SQL, authentication, tenancy,
# validation and audit work. Release builds, same database, same pool size.
#
# Creates and drops its own database; starts both servers on spare ports and
# stops them by PID. Writes docs/evidence/m5-load.json.
#
# Usage: scripts/load/compare.sh [seconds-per-run] [rounds]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SECONDS_PER_RUN="${1:-15}"
ROUNDS="${2:-3}"
OUT="$ROOT/docs/evidence/m5-load.json"
TRESTLE_PORT=18201
BASELINE_PORT=18202
POOL_SIZE=10
DEV_TOKENS="alice-dev-token=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa,bob-dev-token=bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb,carol-dev-token=cccccccc-cccc-4ccc-8ccc-cccccccccccc,olivia-dev-token=dddddddd-dddd-4ddd-8ddd-dddddddddddd"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/trestle-load.XXXXXX")"
DB="trestle_load_$(date +%s)"
export DATABASE_URL="postgres://localhost/$DB"
PIDS=()
# `exec` inside each ( … ) & so that $! is the server's own PID, not a
# subshell's: cleanup must stop exactly the processes this script started.

cleanup() {
  for pid in "${PIDS[@]+"${PIDS[@]}"}"; do
    kill -TERM "$pid" 2>/dev/null || true # only processes this script started
    wait "$pid" 2>/dev/null || true
  done
  dropdb --if-exists --force "$DB" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

for port in $TRESTLE_PORT $BASELINE_PORT; do
  if nc -z 127.0.0.1 "$port" 2>/dev/null; then echo "port $port is in use" >&2; exit 1; fi
done

cd "$ROOT"
. "$HOME/.cargo/env" 2>/dev/null || true
echo "building release binaries …"
cargo build --release -q -p pilot -p pilot-migrations -p loadtest
BIN="$ROOT/target/release"

createdb "$DB"
(cd "$WORK" && "$BIN/pilot-migrate" >/dev/null)
(cd "$WORK" && TRESTLE_DEV_TOKENS="$DEV_TOKENS" "$BIN/pilot" seed-dev >/dev/null)
psql -qd "$DB" -c "INSERT INTO lessons (id, school_id, title, grade_level, created_by)
  SELECT gen_random_uuid(), '11111111-1111-4111-8111-111111111111', 'Seed lesson ' || g, 1 + g % 12,
         'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa'
  FROM generate_series(1, 2000) AS g; ANALYZE;"

# Trestle as served: perimeter on (admission, per-credential buckets,
# metrics, headers, request IDs); limits raised so they never reject.
(cd "$WORK" && exec env RUST_LOG=warn TRESTLE_ENV=development TRESTLE_BIND="127.0.0.1:$TRESTLE_PORT" \
  TRESTLE_DEV_TOKENS="$DEV_TOKENS" TRESTLE_DB_POOL_SIZE=$POOL_SIZE \
  TRESTLE_MAX_IN_FLIGHT=4096 TRESTLE_RATE_PER_SECOND=1000000 TRESTLE_RATE_BURST=1000000 \
  TRESTLE_CURSOR_KEY="load-test cursor key, at least 32 bytes long" \
  "$BIN/pilot" serve) &
PIDS+=($!)
(cd "$WORK" && exec env TRESTLE_DEV_TOKENS="$DEV_TOKENS" \
  "$BIN/loadtest" baseline --bind "127.0.0.1:$BASELINE_PORT" --pool-size $POOL_SIZE) &
PIDS+=($!)

for i in $(seq 1 100); do
  if curl -fs "http://127.0.0.1:$TRESTLE_PORT/readyz" >/dev/null &&
     curl -fs -o /dev/null -H "Authorization: Bearer alice-dev-token" \
       -H "Trestle-Tenant: 11111111-1111-4111-8111-111111111111" \
       "http://127.0.0.1:$BASELINE_PORT/api/v1/lessons"; then break; fi
  sleep 0.2
done

"$BIN/loadtest" compare \
  --trestle "http://127.0.0.1:$TRESTLE_PORT" \
  --baseline "http://127.0.0.1:$BASELINE_PORT" \
  --seconds "$SECONDS_PER_RUN" --rounds "$ROUNDS" \
  --out "$WORK/raw.json"

python3 - "$WORK/raw.json" "$OUT" "$SECONDS_PER_RUN" "$ROUNDS" "$POOL_SIZE" <<'EOF'
import json, os, platform, subprocess, sys, datetime
raw, out, seconds, rounds, pool = sys.argv[1:6]
def run(*cmd):
    try: return subprocess.check_output(cmd, text=True).strip()
    except Exception as e: return f"unavailable ({e})"
data = json.load(open(raw))
report = {
  "measurement": "M5 load comparison: pilot on Trestle vs hand-written Axum baseline (matched work)",
  "date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
  "commit": run("git", "rev-parse", "HEAD"),
  "machine": {
    # Coarse on purpose: no hardware model or OS build in published evidence.
    "cores": str(os.cpu_count()),
    "os": platform.system(),
    "postgres": run("postgres", "--version"),
    "rustc": run("rustc", "--version"),
  },
  "method": {
    "builds": "cargo --release, same workspace lockfile",
    "database": "one local PostgreSQL 16 database, 2,000 seeded lessons in the tenant under test, shared by both servers",
    "pool_size_each": int(pool),
    "load": f"closed loop; a 10s prewarm of every scenario on both targets, then {seconds}s per run after a 3s warm-up, {rounds} rounds alternating which target runs first; one bearer token and tenant (alice at Northside)",
    "scenarios": {
      "get": "GET /api/v1/lessons/{id}: token digest lookup, membership query, scoped read (2 pool acquisitions, 2 queries)",
      "list": "GET /api/v1/lessons: membership query, first page of 20 ordered by title with a signed cursor",
      "create": "POST /api/v1/lessons: membership query, JSON validation, transaction with role recheck FOR SHARE, insert, audit row",
    },
    "trestle": "pilot serve as deployed: registry routing, contract validation, executor, perimeter (admission semaphore, per-credential token bucket with limits raised so it never rejects, Prometheus counters and histograms, security headers, request IDs and tracing spans at RUST_LOG=warn)",
    "baseline": "examples/loadtest/src/baseline.rs: the same SQL text and order, token digests, validation rules, cursor signing, ETag/Location; no perimeter, metrics or tracing",
    "caveats": [
      "The load generator, both servers and PostgreSQL share one laptop; absolute numbers are not capacity figures.",
      "Database-bound routes measure the database as much as the framework; the difference between targets is the signal, not the totals.",
      "Trestle's figures include perimeter features the baseline lacks; that is the cost of the framework as deployed.",
      f"{rounds} rounds per cell; medians are reported. Variance between rounds is visible in runs[]."
    ],
  },
  "target": "provisional: no more than 20% p95 overhead versus the matched Axum baseline (design doc §19.3), a goal, not a guarantee",
  **data,
}
json.dump(report, open(out, "w"), indent=2)
open(out, "a").write("\n")
print(json.dumps(report["summary"], indent=2))
EOF
