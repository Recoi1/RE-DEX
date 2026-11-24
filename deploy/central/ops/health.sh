#!/bin/bash
# Every few minutes (redex-health.timer): restarts what is unhealthy. The backend's /healthz over HTTPS through Caddy
# (database round trip), the world's TLS port, and any container Docker marks unhealthy.
#   ops/health.sh COMPOSE_DIR ACCOUNTS_HOST WORLD_HOST
set -u
[ $# -eq 3 ] || { echo "usage: $0 COMPOSE_DIR ACCOUNTS_HOST WORLD_HOST" >&2; exit 2; }
cd "$1" || exit 2
restart() { echo "health: $1; restarting $2"; docker compose restart "$2"; }
for c in $(docker ps --filter health=unhealthy --filter label=com.docker.compose.project=redex-central --format '{{.Label "com.docker.compose.service"}}'); do
    restart "$c is unhealthy" "$c"
done
# a fresh deploy gets a few minutes (Caddy's first certificates, the world waiting for its copy)
up=$(docker inspect -f '{{.State.StartedAt}}' redex-central-backend-1 2>/dev/null) || { echo "health: backend container missing"; docker compose up -d; exit 1; }
[ $(( $(date +%s) - $(date -d "$up" +%s) )) -lt 300 ] && exit 0
curl -fsS --max-time 10 --resolve "$2:443:127.0.0.1" "https://$2/healthz" >/dev/null || restart "https://$2/healthz failed" backend
timeout 5 bash -c 'exec 3<>/dev/tcp/127.0.0.1/7777' 2>/dev/null || restart "port 7777 closed" world
exit 0
