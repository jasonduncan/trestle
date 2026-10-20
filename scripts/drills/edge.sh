#!/usr/bin/env bash
# Edge drill: the real deploy/Caddyfile in front of two production-mode
# replicas, on this machine (design doc §12.2, §16.1; security review SR-01).
#
# The Caddyfile is used as written except for what only a real host has:
# the site address becomes https://localhost:PORT with Caddy's local CA
# (`tls internal`, not installed into the system trust store), the
# upstreams and asset roots point here, and the admin API and HTTP port
# move to spare ports. The diff is printed into the evidence.
#
# Checks: TLS and security headers; SPA, admin and asset caching; internal
# paths hidden (also through encoded dot segments); unknown API paths stay
# problems, never the SPA; password sign-in, cookie flags, Origin and CSRF
# through the edge; stalled and trickling uploads expire, directly at a
# replica and through the edge, and capacity returns; oversized bodies are
# refused at the edge; a stopped replica is skipped without failed requests;
# the edge overwrites a spoofed X-Real-IP.
#
# Not covered (needs a real host): ACME certificates, public DNS, real
# client addresses through NAT, firewalling of the admin and metrics ports.
#
# Requires caddy, PostgreSQL and node. Creates and drops its own database;
# starts processes on spare ports and stops them by PID. Writes
# docs/evidence/edge-drill.json.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
EVIDENCE="${1:-$ROOT/docs/evidence/edge-drill.json}"
EDGE_PORT=18543
EDGE_HTTP_PORT=18580
EDGE_ADMIN=18544
API_A=18501
API_B=18502
ORIGIN="https://localhost:$EDGE_PORT"
NORTH=11111111-1111-4111-8111-111111111111
BODY_TIMEOUT_MS=3000
MAX_IN_FLIGHT=2
STAMP="$(date +%s)"
DB="trestle_edge_$STAMP"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/trestle-edge.XXXXXX")"
PIDS=()
CHECKS=()

cleanup() {
  for pid in "${PIDS[@]+"${PIDS[@]}"}"; do
    kill -TERM "$pid" 2>/dev/null || true # only processes this script started
    wait "$pid" 2>/dev/null || true
  done
  [ -z "${KEEP:-}" ] && dropdb --if-exists --force "$DB" 2>/dev/null || true
  [ -z "${KEEP:-}" ] && rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "ok: $1"; CHECKS+=("$1"); }
ms() { python3 -c 'import time; print(int(time.time()*1000))'; }
# curl through the edge, trusting only Caddy's local CA.
edge() { curl -sS --cacert "$WORK/data/caddy/pki/authorities/local/root.crt" "$@"; }
header() { # file name -> value (case-insensitive, last one wins)
  awk -v n="$(echo "$2" | tr 'A-Z' 'a-z')" 'BEGIN{FS=": "} { k=tolower($1) } k==n { sub(/\r$/,""); v=substr($0, length($1)+3) } END{print v}' "$1"
}
status() { head -1 "$1" | awk '{print $2}'; }

for port in $EDGE_PORT $EDGE_HTTP_PORT $EDGE_ADMIN $API_A $API_B $((API_A + 10)) $((API_B + 10)); do
  if nc -z 127.0.0.1 "$port" 2>/dev/null; then fail "port $port is in use"; fi
done
command -v caddy >/dev/null || fail "caddy is not installed (brew install caddy)"
cd "$ROOT"
. "$HOME/.cargo/env" 2>/dev/null || true

# 1. Build: API binaries and both bundles. -----------------------------------
cargo build -q -p pilot -p pilot-migrations
BIN="$ROOT/target/debug"
(cd frontend && npm run -s build >/dev/null)
PRODUCT="$ROOT/frontend/product/dist"
ADMIN="$ROOT/frontend/admin/dist"
[ -f "$PRODUCT/index.html" ] && [ -f "$ADMIN/index.html" ] || fail "frontend builds missing"

# 2. Database, a school and a teacher with a password. -----------------------
createdb "$DB"
export DATABASE_URL="postgres://localhost/$DB"
(cd "$WORK" && "$BIN/pilot-migrate" >/dev/null)
(cd "$WORK" && TRESTLE_DEV_TOKENS="x=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa" "$BIN/pilot" seed-dev >/dev/null)
INVITE="$(cd "$WORK" && TRESTLE_ENV=production TRESTLE_PUBLIC_ORIGIN="$ORIGIN" \
  "$BIN/pilot" account invite --email edge.teacher@example.com --school "$NORTH" --role teacher)"
TOKEN="$(echo "$INVITE" | grep -o 'token=[A-Za-z0-9_-]*' | head -1 | cut -d= -f2)"
[ -n "$TOKEN" ] || fail "no invite token in: $INVITE"
PASSWORD="quiet harbour lanterns at dusk"

# 3. Two production replicas. ------------------------------------------------
serve() { # port
  (cd "$WORK" && exec env RUST_LOG=warn TRESTLE_ENV=production DATABASE_URL="$DATABASE_URL" \
    TRESTLE_BIND="127.0.0.1:$1" TRESTLE_METRICS_BIND="127.0.0.1:$(($1 + 10))" \
    TRESTLE_CURSOR_KEY="$(openssl rand -base64 48)" TRESTLE_SESSION_KEY="$SESSION_KEY" \
    TRESTLE_PASSWORD_LOGIN=true TRESTLE_PUBLIC_ORIGIN="$ORIGIN" \
    TRESTLE_CLIENT_ADDRESS_HEADER=X-Real-IP TRESTLE_ADDRESS_RATE_PER_SECOND=1 TRESTLE_ADDRESS_BURST=200 \
    TRESTLE_MAX_IN_FLIGHT=$MAX_IN_FLIGHT TRESTLE_BODY_TIMEOUT_MS=$BODY_TIMEOUT_MS \
    "$BIN/pilot" serve >"$WORK/api-$1.log" 2>&1) &
  PIDS+=($!)
  eval "PID_$1=$!"
  for _ in $(seq 1 100); do curl -fs "http://127.0.0.1:$1/readyz" >/dev/null && return 0; sleep 0.1; done
  cat "$WORK/api-$1.log" >&2; fail "replica on $1 did not become ready"
}
SESSION_KEY="$(openssl rand -base64 48)" # shared: either replica verifies a session
serve $API_A
serve $API_B
pass "two production-mode replicas start (password sign-in, X-Real-IP, body deadline ${BODY_TIMEOUT_MS} ms)"

# 4. The edge, from the real Caddyfile. --------------------------------------
python3 - "$ROOT/deploy/Caddyfile" "$WORK/Caddyfile" <<EOF
import sys
s = open(sys.argv[1]).read()
def sub(old, new):
    global s
    assert s.count(old) == 1, old
    s = s.replace(old, new)
sub("admin localhost:2019", "admin localhost:$EDGE_ADMIN\n\thttp_port $EDGE_HTTP_PORT\n\tskip_install_trust")
sub("app.example.com {", "localhost:$EDGE_PORT {\n\ttls internal")
sub("10.0.0.11:8000 10.0.0.12:8000", "127.0.0.1:$API_A 127.0.0.1:$API_B")
sub("root * /srv/trestle/admin", "root * $ADMIN")
assert s.count("root * /srv/trestle/product") == 2
s = s.replace("root * /srv/trestle/product", "root * $PRODUCT")
open(sys.argv[2], "w").write(s)
EOF
caddy validate --config "$WORK/Caddyfile" --adapter caddyfile >/dev/null 2>&1 || fail "rendered Caddyfile is invalid"
(cd "$WORK" && exec env XDG_DATA_HOME="$WORK/data" XDG_CONFIG_HOME="$WORK/config" \
  caddy run --config "$WORK/Caddyfile" --adapter caddyfile >"$WORK/caddy.log" 2>&1) &
PIDS+=($!)
for _ in $(seq 1 100); do
  [ -f "$WORK/data/caddy/pki/authorities/local/root.crt" ] && edge -o /dev/null "$ORIGIN/" 2>/dev/null && break
  sleep 0.1
done
edge -o /dev/null "$ORIGIN/" || { cat "$WORK/caddy.log" >&2; fail "edge did not start"; }
DIFF="$(diff "$ROOT/deploy/Caddyfile" "$WORK/Caddyfile" || true)"
pass "deploy/Caddyfile validates and serves with only host, upstream and root substitutions"

# 5. TLS, headers, caching. --------------------------------------------------
edge -D "$WORK/h" -o "$WORK/b" --http2 "$ORIGIN/"
[ "$(status "$WORK/h")" = 200 ] && grep -q '<div id="root"' "$WORK/b" || fail "SPA entry"
head -1 "$WORK/h" | grep -q "HTTP/2" || fail "HTTP/2"
[ "$(header "$WORK/h" strict-transport-security)" = "max-age=63072000; includeSubDomains" ] || fail "HSTS"
[ "$(header "$WORK/h" x-content-type-options)" = nosniff ] || fail "nosniff"
[ "$(header "$WORK/h" x-frame-options)" = DENY ] || fail "frame options"
[ "$(header "$WORK/h" referrer-policy)" = no-referrer ] || fail "referrer"
header "$WORK/h" content-security-policy | grep -q "default-src 'self'" || fail "CSP"
[ -z "$(header "$WORK/h" server)" ] || fail "Server header present"
[ "$(header "$WORK/h" cache-control)" = no-cache ] || fail "entry document revalidated"
pass "HTTPS over HTTP/2 with HSTS, CSP, nosniff, frame and referrer policy; no Server header"

edge -D "$WORK/h" -o "$WORK/b" "$ORIGIN/lessons/some/deep/link"
[ "$(status "$WORK/h")" = 200 ] && grep -q '<div id="root"' "$WORK/b" || fail "deep link"
ASSET="$(grep -o '/assets/[^"]*\.js' "$PRODUCT/index.html" | head -1)"
edge -D "$WORK/h" -o /dev/null -H "Accept-Encoding: gzip, zstd" "$ORIGIN$ASSET"
[ "$(status "$WORK/h")" = 200 ] || fail "asset $ASSET"
[ "$(header "$WORK/h" cache-control)" = "public, max-age=31536000, immutable" ] || fail "asset caching"
header "$WORK/h" content-encoding | grep -Eq "gzip|zstd" || fail "compression"
edge -D "$WORK/h" -o /dev/null "$ORIGIN/assets/missing-$STAMP.js"
[ "$(status "$WORK/h")" = 404 ] || fail "missing asset is not the SPA"
pass "deep links get the SPA (no-cache); hashed assets immutable and compressed; a missing asset is 404"

edge -D "$WORK/h" -o /dev/null "$ORIGIN/admin"
[ "$(status "$WORK/h")" = 308 ] && [ "$(header "$WORK/h" location)" = /admin/ ] || fail "admin redirect"
edge -D "$WORK/h" -o "$WORK/b" "$ORIGIN/admin/resources/lessons"
[ "$(status "$WORK/h")" = 200 ] && grep -q '/admin/assets/' "$WORK/b" || fail "admin deep link"
[ "$(header "$WORK/h" cache-control)" = no-cache ] || fail "admin entry caching"
ADMIN_ASSET="$(grep -o '/admin/assets/[^"]*\.js' "$ADMIN/index.html" | head -1)"
edge -D "$WORK/h" -o /dev/null "$ORIGIN$ADMIN_ASSET"
[ "$(status "$WORK/h")" = 200 ] && [ "$(header "$WORK/h" cache-control)" = "public, max-age=31536000, immutable" ] || fail "admin asset"
pass "/admin redirects to /admin/; admin deep links and immutable admin assets"

# 6. Internal paths and API confinement. -------------------------------------
for path in /healthz /readyz /metrics /api/../metrics /api/%2e%2e/metrics /api/%2E%2E/readyz /_meta/../metrics; do
  edge --path-as-is -D "$WORK/h" -o "$WORK/b" "$ORIGIN$path"
  code="$(status "$WORK/h")"
  [ "$code" = 404 ] || fail "$path returned $code"
  ! grep -q "trestle_http_requests_total\|\"ready\"" "$WORK/b" || fail "$path leaked internal output"
done
pass "/healthz, /readyz, /metrics hidden, including through ../ and %2e%2e"

edge -D "$WORK/h" -o "$WORK/b" "$ORIGIN/api/v1/no-such-thing"
[ "$(status "$WORK/h")" = 404 ] || fail "unknown API path"
header "$WORK/h" content-type | grep -q "application/problem+json" || fail "unknown API path is not a problem"
! grep -q '<div id="root"' "$WORK/b" || fail "unknown API path got the SPA"
edge -D "$WORK/h" -o "$WORK/b" "$ORIGIN/_meta/v1/nope"
[ "$(status "$WORK/h")" = 404 ] && ! grep -q '<div id="root"' "$WORK/b" || fail "unknown meta path"
pass "unknown /api and /_meta paths are problem+json 404s, never the SPA"

# 7. Password sign-in through the edge. --------------------------------------
edge -D "$WORK/h" -o "$WORK/b" -X POST "$ORIGIN/api/v1/auth/password/set" \
  -H "Content-Type: application/json" -H "Origin: $ORIGIN" \
  -d "{\"token\":\"$TOKEN\",\"password\":\"$PASSWORD\"}"
[ "$(status "$WORK/h")" = 200 ] || { cat "$WORK/b"; fail "set password"; }
edge -D "$WORK/h" -o "$WORK/b" -X POST "$ORIGIN/api/v1/auth/password/login" \
  -H "Content-Type: application/json" -H "Origin: https://evil.example" \
  -d "{\"email\":\"edge.teacher@example.com\",\"password\":\"$PASSWORD\"}"
[ "$(status "$WORK/h")" = 403 ] || fail "sign-in from another origin was $(status "$WORK/h")"
edge -D "$WORK/h" -o "$WORK/b" -X POST "$ORIGIN/api/v1/auth/password/login" \
  -H "Content-Type: application/json" -H "Origin: $ORIGIN" \
  -d "{\"email\":\"edge.teacher@example.com\",\"password\":\"$PASSWORD\"}"
[ "$(status "$WORK/h")" = 200 ] || { cat "$WORK/b"; fail "sign-in"; }
COOKIE_LINE="$(header "$WORK/h" set-cookie)"
echo "$COOKIE_LINE" | grep -q "^__Host-" || fail "cookie is not __Host-: $COOKIE_LINE"
for attr in Secure HttpOnly "SameSite=Strict" "Path=/"; do
  echo "$COOKIE_LINE" | grep -qi "$attr" || fail "cookie lacks $attr: $COOKIE_LINE"
done
echo "$COOKIE_LINE" | grep -qi "Domain=" && fail "cookie has a Domain"
COOKIE="$(echo "$COOKIE_LINE" | cut -d';' -f1)"
edge -o "$WORK/b" -H "Cookie: $COOKIE" "$ORIGIN/api/v1/auth/session"
CSRF="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["csrf_token"])' "$WORK/b")"
LESSON='{"title":"Through the edge","grade_level":4}'
edge -D "$WORK/h" -o /dev/null -X POST "$ORIGIN/api/v1/lessons" -H "Cookie: $COOKIE" \
  -H "Origin: $ORIGIN" -H "Trestle-Tenant: $NORTH" -H "Content-Type: application/json" -d "$LESSON"
[ "$(status "$WORK/h")" = 403 ] || fail "write without CSRF was $(status "$WORK/h")"
edge -D "$WORK/h" -o "$WORK/b" -X POST "$ORIGIN/api/v1/lessons" -H "Cookie: $COOKIE" -H "X-CSRF-Token: $CSRF" \
  -H "Origin: $ORIGIN" -H "Trestle-Tenant: $NORTH" -H "Content-Type: application/json" -d "$LESSON"
[ "$(status "$WORK/h")" = 201 ] || { cat "$WORK/b"; fail "write with session and CSRF"; }
[ "$(header "$WORK/h" cache-control)" = "private, no-store" ] || header "$WORK/h" cache-control | grep -q no-store || fail "API response cacheable"
pass "password sign-in via the edge: __Host- Secure HttpOnly cookie, foreign Origin refused, write needs CSRF"

# 8. Stalled and trickling uploads (SR-01). ----------------------------------
# Directly at a replica, stalls hold the API's permits until
# TRESTLE_BODY_TIMEOUT_MS, then get 408 and the capacity returns. Through
# the edge, the API still frees its permits at its deadline; Caddy keeps the
# client's connection until read_body (15 s), then answers 408.
uploads() { # mode port stalls ca deadline_ms
  python3 -I - "$@" "$ORIGIN" <<'EOF'
import json, socket, ssl, sys, threading, time, urllib.error, urllib.request
mode, port, n, ca, deadline_ms, origin = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4], int(sys.argv[5]), sys.argv[6]
edge = mode == "edge"
host = "localhost" if edge else "127.0.0.1"
ctx = ssl.create_default_context(cafile=ca) if edge else None
if ctx:
    ctx.set_alpn_protocols(["http/1.1"])
def connect():
    s = socket.create_connection((host, port))
    if ctx:
        s = ctx.wrap_socket(s, server_hostname="localhost")
    s.settimeout(30)
    return s
def head(length):
    return (f"POST /api/v1/auth/password/login HTTP/1.1\r\nHost: localhost:{port}\r\n"
            f"Origin: {origin}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n").encode()
def status(s):
    data = b""
    while b"\r\n" not in data:
        try:
            chunk = s.recv(4096)
        except OSError:
            break
        if not chunk:
            break
        data += chunk
    return int(data.split(b" ")[1]) if data.startswith(b"HTTP/") else 0
def other():
    url = f"{'https' if edge else 'http'}://{host}:{port}/api/v1/auth/methods"
    try:
        return urllib.request.urlopen(url, context=ctx, timeout=10).status
    except urllib.error.HTTPError as e:
        return e.code
r, codes, times = {}, [], []
t0 = time.time()
stalled = [connect() for _ in range(n)]
for s in stalled:
    s.sendall(head(100) + b"{")
time.sleep(0.5)
r["unrelated_while_stalled"] = other()
def probe():
    time.sleep(max(0, deadline_ms / 1000 + 1.5 - (time.time() - t0)))
    r["unrelated_after_api_deadline"] = other()
prober = threading.Thread(target=probe)
prober.start()
def wait(s):
    code = status(s)
    codes.append(code)
    times.append(int((time.time() - t0) * 1000))
threads = [threading.Thread(target=wait, args=(s,)) for s in stalled]
for t in threads:
    t.start()
for t in threads:
    t.join()
prober.join()
r["stalled_status"], r["stalled_released_ms"] = sorted(set(codes)), [min(times), max(times)]
r["unrelated_after"] = other()
t1, s, alive = time.time(), connect(), True
s.sendall(head(1000) + b"[")
while alive and time.time() - t1 < 20:  # one byte every 200 ms
    time.sleep(0.2)
    try:
        s.sendall(b" ")
    except OSError:
        alive = False
r["trickle_status"], r["trickle_released_ms"] = status(s), int((time.time() - t1) * 1000)
print(json.dumps(r))
EOF
}
CA="$WORK/data/caddy/pki/authorities/local/root.crt"
DIRECT="$(uploads direct $API_B $MAX_IN_FLIGHT "$CA" $BODY_TIMEOUT_MS)"
echo "direct: $DIRECT"
python3 - "$DIRECT" "$BODY_TIMEOUT_MS" <<'EOF' || fail "direct uploads: $DIRECT"
import json, sys
r, d = json.loads(sys.argv[1]), int(sys.argv[2])
assert r["unrelated_while_stalled"] == 503, r
assert r["unrelated_after_api_deadline"] == 200, r
assert r["stalled_status"] == [408], r
assert d <= r["stalled_released_ms"][0] and r["stalled_released_ms"][1] < d + 2000, r
assert r["unrelated_after"] == 200, r
assert r["trickle_status"] == 408 and d <= r["trickle_released_ms"] < d + 2000, r
EOF
pass "directly at a replica, stalled sign-in uploads hold capacity only until the ${BODY_TIMEOUT_MS} ms deadline; a trickle cannot extend it"
# More stalls than both replicas' permits together, so both fill however the
# edge spreads them; the extra ones get 503 (Caddy answers when it closes).
EDGE_UPLOADS="$(uploads edge $EDGE_PORT $((MAX_IN_FLIGHT * 2 + 2)) "$CA" $BODY_TIMEOUT_MS)"
echo "edge: $EDGE_UPLOADS"
python3 - "$EDGE_UPLOADS" <<'EOF' || fail "edge uploads: $EDGE_UPLOADS"
import json, sys
r = json.loads(sys.argv[1])
assert r["unrelated_while_stalled"] == 503, r
assert r["unrelated_after_api_deadline"] == 200, r
assert 408 in r["stalled_status"] and set(r["stalled_status"]) <= {408, 503}, r
assert r["stalled_released_ms"][1] < 18000, r
assert r["unrelated_after"] == 200, r
assert r["trickle_status"] == 408 and r["trickle_released_ms"] < 18000, r
EOF
pass "through the edge, API capacity returns at its deadline while Caddy holds the stalled client until read_body (15 s); trickles included"
python3 -c 'import sys; sys.stdout.write("[" + "1," * 600000 + "1]")' > "$WORK/big.json"
code="$(edge -o /dev/null -w '%{http_code}' -X POST "$ORIGIN/api/v1/auth/password/login" \
  -H "Content-Type: application/json" -H "Origin: $ORIGIN" --data-binary @"$WORK/big.json")"
[ "$code" = 413 ] || fail "a 1.2 MB body through the edge was $code"
pass "the edge refuses bodies over 1 MB (413) instead of buffering them"

# 9. A stopped replica. ------------------------------------------------------
eval "STOPPED=\$PID_$API_A"; kill -TERM "$STOPPED"; wait "$STOPPED" 2>/dev/null || true
failed=0
for i in $(seq 1 30); do
  code="$(edge -o /dev/null -w '%{http_code}' -H "Cookie: $COOKIE" -H "Trestle-Tenant: $NORTH" "$ORIGIN/api/v1/lessons?limit=1")"
  [ "$code" = 200 ] || failed=$((failed + 1))
  sleep 0.1
done
[ "$failed" = 0 ] || fail "$failed of 30 requests failed after a replica stopped"
edge -D "$WORK/h" -o /dev/null -X POST "$ORIGIN/api/v1/lessons" -H "Cookie: $COOKIE" -H "X-CSRF-Token: $CSRF" \
  -H "Origin: $ORIGIN" -H "Trestle-Tenant: $NORTH" -H "Content-Type: application/json" -d '{"title":"After failover","grade_level":5}'
[ "$(status "$WORK/h")" = 201 ] || fail "write after failover"
WRITES="$(psql -qAtd "$DB" -c "SELECT count(*) FROM lessons WHERE title IN ('Through the edge', 'After failover')")"
[ "$WRITES" = 2 ] || fail "expected exactly 2 edge writes, found $WRITES"
pass "a stopped replica is skipped: 30 of 30 reads and the next write succeed; no write ran twice"

# 10. Spoofed client address (last: it drains the edge's own bucket). --------------------------------------------------
# Unauthenticated traffic is charged to X-Real-IP. The edge overwrites it,
# so rotating it does not escape the per-address limit. Directly at a
# replica (no edge) the same rotation gets a fresh bucket each time. One
# replica is left, with a burst of 200 per address.
limited=0
for i in $(seq 1 250); do
  code="$(edge -o /dev/null -w '%{http_code}' -H "X-Real-IP: 203.0.113.$((i % 250))" -H "X-Forwarded-For: 198.51.100.$((i % 250))" "$ORIGIN/api/v1/auth/methods")"
  [ "$code" = 429 ] && limited=$((limited + 1))
done
[ "$limited" -gt 0 ] || fail "rotating X-Real-IP escaped the address limit through the edge"
direct_limited=0
for i in $(seq 1 250); do
  code="$(curl -s -o /dev/null -w '%{http_code}' -H "X-Real-IP: 192.0.2.$((i % 250))" "http://127.0.0.1:$API_B/api/v1/auth/methods")"
  [ "$code" = 429 ] && direct_limited=$((direct_limited + 1))
done
[ "$direct_limited" = 0 ] || fail "control: direct rotation was limited ($direct_limited)"
pass "the edge overwrites a spoofed X-Real-IP ($limited of 250 rotated requests limited; 0 directly)"

# Evidence. -------------------------------------------------------------------
python3 - "$EVIDENCE" "$DIFF" "{\"direct\": $DIRECT, \"edge\": $EDGE_UPLOADS}" "$(caddy version | cut -d' ' -f1)" "${CHECKS[@]}" <<'EOF'
import json, platform, subprocess, sys
out, diff, stall, caddy, checks = sys.argv[1], sys.argv[2], json.loads(sys.argv[3]), sys.argv[4], sys.argv[5:]
commit = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
dirty = bool(subprocess.run(["git", "status", "--porcelain", "--untracked-files=no"], capture_output=True, text=True).stdout.strip())
json.dump({
    "drill": "edge", "result": "passed", "commit": commit, "working_tree_modified": dirty,
    "caddy": caddy, "machine": f"{platform.system()} {platform.machine()}",
    "caddyfile_substitutions": diff.splitlines(),
    "uploads": stall, "checks": checks,
    "not_covered": ["ACME certificates and renewal", "public DNS", "client addresses through real NAT/CDN",
                    "firewalling of the Caddy admin API and the metrics listener"],
}, open(out, "w"), indent=2)
open(out, "a").write("\n")
EOF
echo "Edge drill passed. Evidence: $EVIDENCE"
