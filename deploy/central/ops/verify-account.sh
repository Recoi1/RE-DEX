#!/bin/sh
# Confirms an account's address by hand, as its mailed code would (no SMTP account yet: for test accounts only).
#   ops/verify-account.sh COMPOSE_DIR USERNAME
set -eu
[ $# -eq 2 ] || { echo "usage: $0 COMPOSE_DIR USERNAME" >&2; exit 2; }
case "$2" in "" | *[!A-Za-z0-9_.-]*) echo "not a username: $2" >&2; exit 2 ;; esac
cd "$1"
# the same UPDATE as POST /v1/verify-email, plus an audit line naming this path
docker compose exec -T postgres psql -U redex -d redex -X -q -v ON_ERROR_STOP=1 -v u="$2" <<'SQL'
WITH v AS (
    UPDATE accounts SET email_verified_at = coalesce(email_verified_at, now()), updated_at = now()
    WHERE username_norm = lower(:'u') RETURNING id, username
), a AS (
    INSERT INTO audit_log (account_id, event, ip, detail) SELECT id, 'email_verified', 'admin', '{"by":"ops/verify-account.sh"}' FROM v
)
SELECT CASE WHEN count(*) = 1 THEN 'verified ' || min(username) ELSE 'no account named ' || :'u' END AS result FROM v;
SQL
