#!/usr/bin/env bash
# End-to-end test of the whole stack in containers (Docker or Podman).
#
#   ./e2e.sh            build, start a fresh stack, test, leave it running
#   ./e2e.sh --down     the same, then remove the containers and volumes
#   ./e2e.sh --no-build reuse existing images
#
# ENGINE=docker|podman and COMPOSE="docker compose" pick the tools; otherwise
# the first working engine is used. See README.md.
set -euo pipefail
cd "$(dirname "$0")"

DOWN=0 BUILD=1
for arg in "$@"; do
	case $arg in
	--down) DOWN=1 ;;
	--no-build) BUILD=0 ;;
	*) echo "usage: $0 [--down] [--no-build]" >&2; exit 2 ;;
	esac
done

detect() {
	if [ -n "${ENGINE:-}" ] && [ -n "${COMPOSE:-}" ]; then return; fi
	if [ -n "${ENGINE:-}" ]; then
		case $ENGINE in
		podman) if command -v podman-compose >/dev/null; then COMPOSE=podman-compose; else COMPOSE="podman compose"; fi ;;
		*) COMPOSE="$ENGINE compose" ;;
		esac
		return
	fi
	if command -v docker >/dev/null && docker info >/dev/null 2>&1; then
		ENGINE=docker
		if docker compose version >/dev/null 2>&1; then COMPOSE="docker compose"; else COMPOSE=docker-compose; fi
	elif command -v podman >/dev/null && podman info >/dev/null 2>&1; then
		ENGINE=podman
		if command -v podman-compose >/dev/null; then COMPOSE=podman-compose; else COMPOSE="podman compose"; fi
	else
		echo "no working docker or podman found" >&2
		exit 1
	fi
}
detect
echo "engine: $ENGINE, compose: $COMPOSE"

# `podman compose` hands off to docker-compose, which talks to Podman's socket.
if [ "$ENGINE" = podman ] && [ "$COMPOSE" = "podman compose" ]; then
	sock=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/podman/podman.sock
	if [ ! -S "$sock" ]; then
		echo "starting podman.socket"
		systemctl --user start podman.socket 2>/dev/null || {
			podman system service --time=0 "unix://$sock" >/dev/null 2>&1 &
			sleep 2
		}
	fi
fi

SECRET=${MM_SECRET:-e2e-secret}
GATE=10.77.0.20:25565
EDITOR_URL=http://localhost:${MM_EDITOR_PORT:-8080}
PROM_URL=http://localhost:${MM_PROMETHEUS_PORT:-9090}
GRAFANA_URL=http://localhost:${MM_GRAFANA_PORT:-3000}

step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
bot() { "$ENGINE" exec mm-bot mmbot "$@"; }
sql() { "$ENGINE" exec mm-postgres psql -U minimodes -d minimodes -Atq -c "$1"; }
compose() { $COMPOSE "$@"; }

wait_for() { # description, attempts, command...
	local what=$1 tries=$2
	shift 2
	for _ in $(seq "$tries"); do
		if "$@" >/dev/null 2>&1; then return 0; fi
		sleep 2
	done
	fail "timed out waiting for $what"
}

healthy() { [ "$("$ENGINE" inspect -f '{{.State.Health.Status}}' "$1" 2>/dev/null)" = healthy ]; }

logs_on_failure() {
	local rc=$?
	if [ $rc -ne 0 ]; then
		echo >&2
		for c in mm-lobby mm-gamehost-1 mm-gamehost-2 mm-gate; do
			echo "---- $c (last 40 lines)" >&2
			"$ENGINE" logs --tail 40 "$c" >&2 2>&1 || true
		done
	fi
	if [ "$DOWN" = 1 ]; then compose down -v >/dev/null 2>&1 || true; fi
	return $rc
}
trap logs_on_failure EXIT

if [ "$BUILD" = 1 ]; then
	step "build images"
	compose build
fi

step "start a fresh stack"
compose down -v >/dev/null 2>&1 || true
compose up -d
for c in mm-postgres mm-lobby mm-gamehost-1 mm-gamehost-2; do
	wait_for "$c to be healthy" 90 healthy "$c"
done
echo "all containers healthy"

step "Gate: the shards register themselves"
wait_for "Gate to accept logins" 30 "$ENGINE" exec mm-bot sh -c "nc -z ${GATE/:/ }"

step "bots join through the real Gate"
bot join "$GATE" 3 3

step "lobby UI through Gate"
bot lobby-ui "$GATE"

step "create, publish and switch servers through Gate"
bot gate "$GATE"
games=$(sql "SELECT count(*) FROM games WHERE name LIKE 'Gated %'")
[ "$games" -ge 1 ] || fail "the published game is not in Postgres"

step "Caddy → the web editor"
game_id=$(sql "SELECT id FROM games ORDER BY id DESC LIMIT 1")
link=$(bot editor-link "$game_id" http://caddy:8080 | tail -1)
path=${link#http://caddy:8080}
code=$(curl -s -o /dev/null -w '%{http_code}' -D /tmp/mm-e2e-headers.$$ -c /tmp/mm-e2e-jar.$$ "$EDITOR_URL$path")
case $code in 200 | 302 | 303) ;; *) fail "editor link answered $code" ;; esac
grep -qi '^strict-transport-security' /tmp/mm-e2e-headers.$$ || fail "Caddy added no HSTS header"
code=$(curl -s -o /dev/null -w '%{http_code}' "$EDITOR_URL/metrics")
[ "$code" = 404 ] || fail "/metrics is reachable through Caddy ($code)"
rm -f /tmp/mm-e2e-headers.$$ /tmp/mm-e2e-jar.$$
echo "editor link ok; /metrics not exposed"

step "Prometheus scrapes every process"
check_targets() {
	local out
	out=$(curl -sf "$PROM_URL/api/v1/query?query=up%7Bjob%3D~%22minimodes-.*%22%7D%3D%3D1")
	[ "$(printf '%s' "$out" | grep -o '"process"' | wc -l)" -ge 3 ]
}
wait_for "3 healthy scrape targets" 30 check_targets
curl -sf "$PROM_URL/api/v1/query?query=mm_shards_connected" | grep -q '"value"' || fail "mm_shards_connected missing"
curl -sf "$PROM_URL/api/v1/rules" | grep -q ProcessDown || fail "alert rules not loaded"
echo "targets up, alert rules loaded"

step "Grafana provisions the dashboard"
dash() { curl -sf -u admin:admin "$GRAFANA_URL/api/search?query=Minimodes" | grep -q Minimodes; }
wait_for "the Minimodes dashboard" 30 dash
curl -sf -u admin:admin "$GRAFANA_URL/api/datasources/uid/prometheus/health" | grep -qi '"status":"OK"' || fail "Grafana can't reach Prometheus"
echo "dashboard and datasource ok"

step "backup to the S3 stand-in"
"$ENGINE" exec mm-backup sh -c 'echo marker > /var/lib/minimodes/worlds/e2e-marker.mmw'
"$ENGINE" exec mm-backup minimodes-backup.sh
today=$("$ENGINE" exec mm-backup date -u +%F)
before=$(sql "SELECT count(*) FROM games")

step "damage the data, then restore"
compose stop lobby gamehost-1 gamehost-2
"$ENGINE" exec mm-backup rm -f /var/lib/minimodes/worlds/e2e-marker.mmw
"$ENGINE" exec mm-postgres psql -U minimodes -d minimodes -q -c "TRUNCATE games CASCADE" >/dev/null
[ "$(sql 'SELECT count(*) FROM games')" = 0 ] || fail "TRUNCATE did not empty games"
"$ENGINE" exec mm-backup minimodes-restore.sh "$today"
after=$(sql "SELECT count(*) FROM games")
[ "$after" = "$before" ] || fail "games after restore: $after (expected $before)"
"$ENGINE" exec mm-backup test -s /var/lib/minimodes/worlds/e2e-marker.mmw || fail "world file not restored"
echo "restored $after game(s) and the world file"

step "the network comes back up on the restored data"
compose start lobby gamehost-1 gamehost-2
for c in mm-lobby mm-gamehost-1 mm-gamehost-2; do
	wait_for "$c to be healthy" 90 healthy "$c"
done
bot join "$GATE" 1 2

step "a shard stops on SIGTERM and Gate/the lobby stay up"
"$ENGINE" stop -t 15 mm-gamehost-2 >/dev/null
sleep 2
healthy mm-lobby || fail "the lobby went down with the shard"
"$ENGINE" start mm-gamehost-2 >/dev/null
wait_for "gamehost-2 to be healthy" 60 healthy mm-gamehost-2
bot join "$GATE" 1 2

printf '\nALL E2E CHECKS PASSED\n'
