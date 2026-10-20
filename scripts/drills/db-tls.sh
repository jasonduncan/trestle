#!/usr/bin/env bash
# Database TLS drill (M5): proves the runtime and migration binaries connect
# with sslmode=verify-full against a server whose certificate chains to a
# given CA, and that plaintext or wrongly-trusted connections fail. It also
# records what the weaker modes accept (SR-03: `require` checks no
# certificate, and `verify-ca` is documented to skip the host name), and that
# `pilot serve` in production refuses both for a non-local database before
# it connects.
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
# The weaker modes, observed rather than asserted: what they accept depends
# on the driver version, which is why production refuses them outright.
require_untrusted=$(attempt require-untrusted \
  "postgres://trestle@127.0.0.1:$PORT/postgres?sslmode=require&sslrootcert=$WORK/other.crt" \
  target/debug/pilot-migrate --plan)
verify_ca_wrong_host=$(attempt verify-ca-wrong-host \
  "postgres://trestle@127.0.0.1:$PORT/postgres?sslmode=verify-ca&sslrootcert=$WORK/ca.crt" \
  target/debug/pilot-migrate --plan)

# Production boot with a non-local DATABASE_URL: everything else is valid,
# so only the TLS mode can refuse. The host does not resolve; verify-full
# passes the boot checks and then fails to connect, the others must be
# refused before any connection is attempted.
boot() { # name, sslmode
  env -u TRESTLE_DEV_TOKENS -u TRESTLE_CLIENT_ADDRESS_HEADER TRESTLE_ENV=production \
    DATABASE_URL="postgres://trestle@db.invalid/postgres?sslmode=$2&sslrootcert=$WORK/ca.crt" \
    TRESTLE_PASSWORD_LOGIN=true TRESTLE_TRUST_PEER_ADDRESS=true \
    TRESTLE_CURSOR_KEY="$(printf 'c%.0s' $(seq 32))" TRESTLE_SESSION_KEY="$(printf 's%.0s' $(seq 32))" \
    TRESTLE_PUBLIC_ORIGIN=https://app.example.com TRESTLE_BIND=127.0.0.1:0 \
    target/debug/pilot serve >"$WORK/$1.out" 2>&1 || true
  if grep -q "refusing to start" "$WORK/$1.out" && grep -q "sslmode=verify-full" "$WORK/$1.out" \
    && ! grep -q "connect to database" "$WORK/$1.out"; then
    echo refused
  elif grep -q "connect to database" "$WORK/$1.out"; then
    echo accepted
  else
    echo unexpected
  fi
}
boot_require=$(boot boot-require require)
boot_verify_ca=$(boot boot-verify-ca verify-ca)
boot_verify_full=$(boot boot-verify-full verify-full)

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
    "psql_session_reports_ssl": "$ssl_in_use",
    "production_boot_with_require": "$boot_require",
    "production_boot_with_verify_ca": "$boot_verify_ca",
    "production_boot_with_verify_full": "$boot_verify_full"
  },
  "refusal_reasons": {
    "plaintext_connection": "$(reason plaintext)",
    "verify_full_with_untrusted_ca": "$(reason wrong-ca)",
    "verify_full_with_mismatched_host": "$(reason wrong-host)",
    "production_boot_with_require": "$(reason boot-require)",
    "production_boot_with_verify_ca": "$(reason boot-verify-ca)"
  },
  "weaker_modes_observed": {
    "require_with_untrusted_ca": "$require_untrusted",
    "verify_ca_with_mismatched_host": "$verify_ca_wrong_host",
    "verify_ca_with_mismatched_host_detail": "$(reason verify-ca-wrong-host)"
  },
  "expected": {
    "migrate_with_verify_full": "ok",
    "schema_verify_with_verify_full": "ok",
    "plaintext_connection": "refused",
    "verify_full_with_untrusted_ca": "refused",
    "verify_full_with_mismatched_host": "refused",
    "psql_session_reports_ssl": "t",
    "production_boot_with_require": "refused",
    "production_boot_with_verify_ca": "refused",
    "production_boot_with_verify_full": "accepted"
  },
  "notes": "weaker_modes_observed is informational, not asserted: require connecting to a server signed by an untrusted CA is the SR-03 weakness. With this driver build verify-ca also refuses a mismatched IP address (the hostname-skipping verifier does not recognize the newer rustls error), but that is incidental and version-dependent, so production accepts only verify-full for a non-local database. The boot cases use the unresolvable host db.invalid: verify-full passes the boot checks and then fails to connect; require and verify-ca are refused before any connection."
}
EOF
cat "$EVIDENCE"
[ "$verify_full $schema $plaintext $wrong_ca $wrong_host $ssl_in_use" = "ok ok refused refused refused t" ]
[ "$boot_require $boot_verify_ca $boot_verify_full" = "refused refused accepted" ]
