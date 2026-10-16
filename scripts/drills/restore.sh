#!/usr/bin/env bash
# Backup, restore and forward-repair drill (M5; design doc §16.3, §18 M5).
#
#   1. A pilot database with real API traffic is backed up (pg_dump -Fc).
#   2. Incident: a faulty bulk change overwrites grade levels in one school,
#      and legitimate writes continue afterwards.
#   3. Restore rehearsal: the backup is restored into a NEW database (never
#      over the live one), verified against migration history and served by
#      a second replica.
#   4. Forward repair: the damaged values are copied back from the restored
#      copy in one audited transaction, keeping every post-incident write.
#      Rows edited after the incident are not overwritten; they are listed
#      for review. Repaired rows get a new version, so stale ETags fail.
#
# Creates and drops its own databases; starts servers on spare ports and
# stops them by PID. Writes docs/evidence/m5-restore.json.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
EVIDENCE="${1:-$ROOT/docs/evidence/m5-restore.json}"
LIVE_PORT=18301
RESTORED_PORT=18302
NORTH=11111111-1111-4111-8111-111111111111
SOUTH=22222222-2222-4222-8222-222222222222
DEV_TOKENS="alice-dev-token=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa,bob-dev-token=bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb,carol-dev-token=cccccccc-cccc-4ccc-8ccc-cccccccccccc,olivia-dev-token=dddddddd-dddd-4ddd-8ddd-dddddddddddd"
STAMP="$(date +%s)"
DB="trestle_drill_$STAMP"
RESTORED="${DB}_restored"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/trestle-restore.XXXXXX")"
PIDS=()

cleanup() {
  for pid in "${PIDS[@]+"${PIDS[@]}"}"; do
    kill -TERM "$pid" 2>/dev/null || true # only processes this script started
    wait "$pid" 2>/dev/null || true
  done
  if [ -z "${KEEP:-}" ]; then
    dropdb --if-exists --force "$DB" 2>/dev/null || true
    dropdb --if-exists --force "$RESTORED" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

ms() { python3 -c 'import time; print(int(time.time()*1000))'; }
q() { psql -qAtd "$1" -c "$2"; }
fingerprint() { # a digest of every lesson's state
  q "$1" "SELECT count(*) || ':' || md5(coalesce(string_agg(id || '|' || title || '|' || grade_level || '|' || status || '|' || version || '|' || coalesce(duration_minutes::text, '-'), ',' ORDER BY id), '')) FROM lessons"
}
api() { # port method path token tenant [json [extra curl args...]]
  local args=(-fsS -X "$2" "http://127.0.0.1:$1$3" -H "Authorization: Bearer $4-dev-token" -H "Trestle-Tenant: $5")
  if [ $# -ge 6 ]; then args+=(-H "Content-Type: application/json" -d "$6" "${@:7}"); fi
  curl "${args[@]}"
}
serve() { # port database
  # exec: $! must be the server's own PID, not a subshell's.
  (cd "$WORK" && exec env RUST_LOG=warn DATABASE_URL="postgres://localhost/$2" TRESTLE_ENV=development \
    TRESTLE_BIND="127.0.0.1:$1" TRESTLE_DEV_TOKENS="$DEV_TOKENS" "$BIN/pilot" serve) &
  PIDS+=($!)
  for _ in $(seq 1 100); do curl -fs "http://127.0.0.1:$1/readyz" >/dev/null && return 0; sleep 0.1; done
  echo "server on $1 did not become ready" >&2; return 1
}

for port in $LIVE_PORT $RESTORED_PORT; do
  if nc -z 127.0.0.1 "$port" 2>/dev/null; then echo "port $port is in use" >&2; exit 1; fi
done
cd "$ROOT"
. "$HOME/.cargo/env" 2>/dev/null || true
cargo build -q -p pilot -p pilot-migrations
BIN="$ROOT/target/debug"

# 1. A live database with traffic. -------------------------------------------
createdb "$DB"
(cd "$WORK" && DATABASE_URL="postgres://localhost/$DB" "$BIN/pilot-migrate" >/dev/null)
(cd "$WORK" && DATABASE_URL="postgres://localhost/$DB" TRESTLE_DEV_TOKENS="$DEV_TOKENS" "$BIN/pilot" seed-dev >/dev/null)
serve $LIVE_PORT "$DB"
for i in $(seq 1 20); do
  api $LIVE_PORT POST /api/v1/lessons alice $NORTH "{\"title\":\"Northside lesson $i\",\"grade_level\":$((1 + i % 12)),\"duration_minutes\":$((20 + i))}" >/dev/null
done
for i in $(seq 1 5); do
  api $LIVE_PORT POST /api/v1/lessons bob $SOUTH "{\"title\":\"Southside lesson $i\",\"grade_level\":$((1 + i % 12))}" >/dev/null
done
before=$(fingerprint "$DB")

# Backup.
t0=$(ms)
pg_dump -Fc --no-owner -f "$WORK/backup.dump" "$DB"
backup_ms=$(( $(ms) - t0 ))
backup_bytes=$(wc -c < "$WORK/backup.dump" | tr -d ' ')
backup_sha=$(shasum -a 256 "$WORK/backup.dump" | cut -d' ' -f1)

# 2. Incident, then legitimate writes. ---------------------------------------
damaged=$(q "$DB" "WITH d AS (UPDATE lessons SET grade_level = 1, version = version + 1, updated_at = now()
  WHERE school_id = '$NORTH' AND grade_level <> 1 RETURNING 1) SELECT count(*) FROM d")
api $LIVE_PORT POST /api/v1/lessons alice $NORTH '{"title":"Written after the incident","grade_level":9}' >/dev/null
edited_id=$(q "$DB" "SELECT id FROM lessons WHERE title = 'Northside lesson 5'")
edited_version=$(q "$DB" "SELECT version FROM lessons WHERE id = '$edited_id'")
api $LIVE_PORT PATCH "/api/v1/lessons/$edited_id" alice $NORTH '{"title":"Northside lesson 5 (edited after the incident)"}' \
  -H "If-Match: \"lessons.view:$edited_version\"" >/dev/null
probe_id=$(q "$DB" "SELECT id FROM lessons WHERE title = 'Northside lesson 7'")
probe_etag="\"lessons.view:$(q "$DB" "SELECT version FROM lessons WHERE id = '$probe_id'")\""

# 3. Restore rehearsal into a new database. ----------------------------------
t0=$(ms)
createdb "$RESTORED"
pg_restore --no-owner --exit-on-error -d "$RESTORED" "$WORK/backup.dump"
restore_ms=$(( $(ms) - t0 ))
restored=$(fingerprint "$RESTORED")
schema_verify=$(cd "$WORK" && DATABASE_URL="postgres://localhost/$RESTORED" "$BIN/pilot" schema verify 2>&1 | tail -1 | tr -d '"\\')
pending=$(cd "$WORK" && DATABASE_URL="postgres://localhost/$RESTORED" "$BIN/pilot-migrate" --plan | grep -vc Applied || true)
t0=$(ms)
serve $RESTORED_PORT "$RESTORED"
ready_ms=$(( $(ms) - t0 ))
served=$(api $RESTORED_PORT GET "/api/v1/lessons?limit=100" alice $NORTH | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["items"]))')

# 4. Forward repair from the restored copy. ----------------------------------
psql -qAtd "$RESTORED" -c "\\copy (SELECT id, grade_level, version FROM lessons WHERE school_id = '$NORTH') TO '$WORK/repair.csv' CSV"
cat > "$WORK/repair.sql" <<SQL
CREATE TEMP TABLE repair (id uuid PRIMARY KEY, grade_level smallint NOT NULL, version bigint NOT NULL);
\\copy repair FROM '$WORK/repair.csv' CSV
-- Repair only rows whose sole change since the backup was the damage.
WITH fixed AS (
  UPDATE lessons l
     SET grade_level = r.grade_level, version = l.version + 1, updated_at = now()
    FROM repair r
   WHERE l.id = r.id AND l.school_id = '$NORTH'
     AND l.version = r.version + 1 AND l.grade_level = 1 AND r.grade_level <> 1
  RETURNING l.id
)
INSERT INTO audit_events (id, actor_id, tenant_id, operation, operation_version, target_type, target_id, request_id)
SELECT gen_random_uuid(), '00000000-0000-0000-0000-000000000000', '$NORTH',
       'repair.lessons.grade_level', '1.0.0', 'lesson', id::text, 'drill-$STAMP'
  FROM fixed;
-- Damaged rows that were not repaired (changed again after the incident).
\copy (SELECT l.title FROM lessons l JOIN repair r ON r.id = l.id WHERE l.grade_level = 1 AND r.grade_level <> 1 ORDER BY l.title) TO '$WORK/review.csv' CSV
SQL
t0=$(ms)
psql -q -1 -v ON_ERROR_STOP=1 -d "$DB" -f "$WORK/repair.sql"
repair_ms=$(( $(ms) - t0 ))
repaired=$(q "$DB" "SELECT count(*) FROM audit_events WHERE request_id = 'drill-$STAMP'")
needs_review=$(tr -d '"' < "$WORK/review.csv" | paste -sd ';' -)
still_damaged=$(grep -c . "$WORK/review.csv" || true)
kept_after_incident=$(q "$DB" "SELECT count(*) FROM lessons WHERE title IN ('Written after the incident', 'Northside lesson 5 (edited after the incident)')")
# Assignments, not inline substitutions, so a failing query stops the drill.
south_sql="SELECT count(*) || ':' || md5(string_agg(id::text || '|' || grade_level || '|' || version, ',' ORDER BY id)) FROM lessons WHERE school_id = '$SOUTH'"
south_live=$(q "$DB" "$south_sql")
south_backup=$(q "$RESTORED" "$south_sql")
[ -n "$south_live" ] || { echo "empty Southside fingerprint" >&2; exit 1; }
southside_untouched=$( [ "$south_live" = "$south_backup" ] && echo true || echo false)
stale_status=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "http://127.0.0.1:$LIVE_PORT/api/v1/lessons/$probe_id" \
  -H "Authorization: Bearer alice-dev-token" -H "Trestle-Tenant: $NORTH" -H "Content-Type: application/json" \
  -H "If-Match: $probe_etag" -d '{"title":"Stale write"}')

cat > "$EVIDENCE" <<EOF
{
  "drill": "backup, restore and forward repair",
  "date": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "commit": "$(git rev-parse HEAD)",
  "postgres": "$(postgres --version)",
  "backup": { "tool": "pg_dump -Fc --no-owner", "bytes": $backup_bytes, "sha256": "$backup_sha", "ms": $backup_ms },
  "incident": { "rows_damaged": $damaged, "then": "one lesson created and one lesson edited through the API" },
  "restore_rehearsal": {
    "target": "a new database; the live database is never overwritten",
    "ms": $restore_ms,
    "fingerprint_matches_backup_time": $( [ "$before" = "$restored" ] && echo true || echo false ),
    "schema_verify": "$schema_verify",
    "migrations_pending_after_restore": $pending,
    "replica_ready_ms": $ready_ms,
    "lessons_served_by_restored_replica": $served
  },
  "forward_repair": {
    "method": "copy pre-incident values from the restored copy in one transaction; only rows whose sole change was the damage; audited; versions bumped",
    "ms": $repair_ms,
    "rows_repaired": $repaired,
    "rows_left_for_review": $still_damaged,
    "left_for_review": "$needs_review",
    "post_incident_writes_kept": $kept_after_incident,
    "other_tenant_untouched": $southside_untouched,
    "other_tenant_fingerprint": "$south_live",
    "pre_repair_etag_now_gets": $stale_status
  }
}
EOF
cat "$EVIDENCE"
[ "$before" = "$restored" ] && [ "$pending" = "0" ] && [ "$repaired" = "$((damaged - 1))" ] && [ "$still_damaged" = "1" ] \
  && [ "$kept_after_incident" = "2" ] && [ "$southside_untouched" = "true" ] && [ "$stale_status" = "412" ]
