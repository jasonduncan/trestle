#!/usr/bin/env bash
# Database TLS drill (M5): proves the runtime and migration binaries connect
# with sslmode=verify-full against a server whose certificate chains to a
# given CA, and that plaintext or wrongly-trusted connections fail.
#
# Runs a throwaway PostgreSQL on a spare port with its own data directory
# and certificates; never touches the development server. Stops only the
# instance it started (pg_ctl -D <its data dir>).
#
# Usage: scripts/drills/db-tls.sh [evidence.json]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
EVIDENCE="${1:-$ROOT/docs/evidence/m5-db-tls.json}"
PORT="${TLS_DRILL_PORT:-55433}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/trestle-tls-drill.XXXXXX")"
DATA="$WORK/data"

cleanup() {
  if [ -f "$DATA/postmaster.pid" ]; then
    pg_ctl -D "$DATA" -m fast stop >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

cd "$WORK"
# A private CA and a server certificate for the name localhost only.
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=trestle drill CA" \
  -keyout ca.key -out ca.crt >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
  -keyout server.key -out server.csr >/dev/null 2>&1
printf "subjectAltName=DNS:localhost\n" > san.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 1 \
  -extfile san.ext -out server.crt >/dev/null 2>&1
chmod 600 server.key
# A second CA that did not sign the server certificate.
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=untrusted CA" \
  -keyout other.key -out other.crt >/dev/null 2>&1

initdb -D "$DATA" -U trestle --auth=trust >/dev/null
cat >> "$DATA/postgresql.conf" <<EOF
port = $PORT
listen_addresses = '127.0.0.1'
unix_socket_directories = '$WORK'
ssl = on
ssl_cert_file = '$WORK/server.crt'
ssl_key_file = '$WORK/server.key'
EOF
# TLS only over TCP.
cat > "$DATA/pg_hba.conf" <<EOF
hostssl all all 127.0.0.1/32 trust
hostnossl all all 0.0.0.0/0 reject
EOF
pg_ctl -D "$DATA" -l "$WORK/server.log" -w start >/dev/null

BASE="postgres://trestle@localhost:$PORT/postgres"
cd "$ROOT"
. "$HOME/.cargo/env" 2>/dev/null || true
cargo build -q -p pilot -p pilot-migrations

attempt() { # name, url, command...
  local name="$1" url="$2"; shift 2
  if DATABASE_URL="$url" "$@" >"$WORK/$name.out" 2>&1; then echo ok; else echo refused; fi
}

verify_full=$(attempt verify-full "$BASE?sslmode=verify-full&sslrootcert=$WORK/ca.crt" \
  target/debug/pilot-migrate)
schema=$(attempt schema "$BASE?sslmode=verify-full&sslrootcert=$WORK/ca.crt" \
  target/debug/pilot schema verify)
plaintext=$(attempt plaintext "$BASE?sslmode=disable" target/debug/pilot-migrate --plan)
wrong_ca=$(attempt wrong-ca "$BASE?sslmode=verify-full&sslrootcert=$WORK/other.crt" \
  target/debug/pilot-migrate --plan)
wrong_host=$(attempt wrong-host \
  "postgres://trestle@127.0.0.1:$PORT/postgres?sslmode=verify-full&sslrootcert=$WORK/ca.crt" \
  target/debug/pilot-migrate --plan)
ssl_in_use=$(psql "host=localhost port=$PORT user=trestle dbname=postgres sslmode=verify-full sslrootcert=$WORK/ca.crt" \
  -tAc "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")

reason() { tail -1 "$WORK/$1.out" | tr -d '"\\' | cut -c1-200; }

cat > "$EVIDENCE" <<EOF
{
  "drill": "database TLS",
  "date": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "postgres": "$(postgres --version | sed 's/"//g')",
  "server": "throwaway instance on a spare port, hostssl only, certificate for DNS name localhost signed by a drill CA; the mismatched-host case connects to 127.0.0.1, which the certificate does not name",
  "results": {
    "migrate_with_verify_full": "$verify_full",
    "schema_verify_with_verify_full": "$schema",
    "plaintext_connection": "$plaintext",
    "verify_full_with_untrusted_ca": "$wrong_ca",
    "verify_full_with_mismatched_host": "$wrong_host",
    "psql_session_reports_ssl": "$ssl_in_use"
  },
  "refusal_reasons": {
    "plaintext_connection": "$(reason plaintext)",
    "verify_full_with_untrusted_ca": "$(reason wrong-ca)",
    "verify_full_with_mismatched_host": "$(reason wrong-host)"
  },
  "expected": {
    "migrate_with_verify_full": "ok",
    "schema_verify_with_verify_full": "ok",
    "plaintext_connection": "refused",
    "verify_full_with_untrusted_ca": "refused",
    "verify_full_with_mismatched_host": "refused",
    "psql_session_reports_ssl": "t"
  }
}
EOF
cat "$EVIDENCE"
[ "$verify_full $schema $plaintext $wrong_ca $wrong_host $ssl_in_use" = "ok ok refused refused refused t" ]
