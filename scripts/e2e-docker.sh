#!/bin/sh
# End-to-end check of the platform workloads plan's Stage 1 features against
# real Docker: a controller, an agent and the CLI, built by `make dist`, run
# Postgres with a secret, a bind address, a command and a health check.
#
#   make e2e
#
# It runs inside a docker:cli container on the Docker host's network, with the
# Docker socket, the binaries at /dist, and a working directory under
# /var/tmp/vk-e2e on the Docker host itself (so Postgres can own its data
# directory; folders shared from macOS cannot be chowned). Each run uses a new
# run-<time> directory there. It uses ports 50061, 50062, 9410, 9411, 15432
# and 18080 on the Docker host and the service ids global-pg, bad-secret,
# web-check, pg-init, pg-fail and cache-demo, so do not run it next to a dev cluster with
# those services.
set -eu
W=/var/tmp/vk-e2e/run-$(date +%s)
BIN=/dist
fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "PASS: $*"; }

# Remove this test's containers however it ends.
cleanup() {
  for svc in global-pg bad-secret web-check pg-init pg-fail cache-demo; do
    for c in $(docker ps -aq --filter "label=valkvork.service_id=$svc"); do
      docker stop "$c" >/dev/null 2>&1 || true
      docker rm "$c" >/dev/null 2>&1 || true
    done
  done
}
trap cleanup EXIT

IP="$(ip -4 route get 1.1.1.1 | awk '{for(i=1;i<=NF;i++) if($i=="src") print $(i+1)}')"
ip -4 addr | grep -q "inet $IP/" || fail "advertise address $IP is not on this host"
echo "advertise address: $IP"

apk add -q --no-cache openssl >/dev/null
mkdir -p "$W/secrets" "$W/data" "$W/db" "$W/pgdata" "$W/tls"
(
  cd "$W/tls"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -keyout ca.key -out ca.pem -days 2 -subj "/CN=vk-e2e-ca" 2>/dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -keyout server.sec1.key -out server.csr -subj "/CN=localhost" 2>/dev/null
  printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n' > ext.cnf
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out server.pem -days 2 -extfile ext.cnf 2>/dev/null
  openssl pkcs8 -topk8 -nocrypt -in server.sec1.key -out server.key
)
chmod 0700 "$W/secrets"
PW="$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
printf '%s\n' "$PW" > "$W/secrets/pg_password"
echo "join-$(head -c 8 /dev/urandom | od -An -tx1 | tr -d ' \n')" > "$W/join-token"
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$W/admin-token"

cat > "$W/controller.toml" <<EOF
agent_listen_addr   = "127.0.0.1:50061"
admin_listen_addr   = "127.0.0.1:50062"
metrics_listen_addr = "127.0.0.1:9410"
database_path       = "$W/db/valkvork.db"
tls_cert_file       = "$W/tls/server.pem"
tls_key_file        = "$W/tls/server.key"
join_token_file     = "$W/join-token"
admin_token_file    = "$W/admin-token"
recovery_grace_period_seconds = 0
EOF
cat > "$W/agent.toml" <<EOF
controller_endpoint = "https://localhost:50061"
ca_file             = "$W/tls/ca.pem"
join_token_file     = "$W/join-token"
node_class          = "app"
data_dir            = "$W/data"
docker_socket       = "/var/run/docker.sock"
registry_auth_file  = "$W/none.json"
advertise_ip        = "$IP"
metrics_listen_addr = "127.0.0.1:9411"

[reserved]
cpu    = 0.25
memory = "256MiB"

[secrets]
file_dirs = ["$W/secrets"]

[host_volumes.pgdata]
path = "$W/pgdata"
EOF
"$BIN/valkvork-controller" --config "$W/controller.toml" --check-config >/dev/null
"$BIN/valkvork-agent" --config "$W/agent.toml" --check-config >/dev/null
"$BIN/valkvork-controller" --config "$W/controller.toml" > "$W/controller.log" 2>&1 &
sleep 1
"$BIN/valkvork-agent" --config "$W/agent.toml" > "$W/agent.log" 2>&1 &

export VALKVORK_ADDR=https://localhost:50062
export VALKVORK_CA_FILE=$W/tls/ca.pem
VALKVORK_ADMIN_TOKEN="$(cat "$W/admin-token")"
export VALKVORK_ADMIN_TOKEN
vk() { "$BIN/valkvork" "$@"; }
for _ in $(seq 30); do vk node list 2>/dev/null | grep -q HEALTHY && break; sleep 1; done
vk node list | grep -q HEALTHY || fail "agent did not register"
pass "agent registered"

# ── global-pg, with every Stage 1 feature ──────────────────────────────────
cat > "$W/global-pg.toml" <<EOF
[service]
id           = "global-pg"
image        = "docker.io/library/postgres:16.4-alpine"
replicas     = 1
kill_timeout = "30s"
command      = ["postgres", "-c", "wal_level=logical"]

[resources]
cpu    = 0.5
memory = "512MiB"

[constraints]
required_class = "app"

[[ports]]
name           = "postgres"
container_port = 5432
host_port      = 15432
host_ip        = "advertise"

[[volumes]]
type   = "host"
source = "pgdata"
target = "/var/lib/postgresql/data"

[env]
POSTGRES_USER = "valkv"
POSTGRES_DB   = "valkv_global"
PGDATA        = "/var/lib/postgresql/data/pgdata"

[secrets]
POSTGRES_PASSWORD = "file:$W/secrets/pg_password"

[check]
type     = "tcp"
port     = "postgres"
interval = "2s"
failures = 3
grace    = "60s"

[update]
min_healthy_time = "5s"
EOF
vk plan -f "$W/global-pg.toml"
vk apply -f "$W/global-pg.toml" >/dev/null
for _ in $(seq 90); do vk workloads 2>/dev/null | grep global-pg | grep -q HEALTHY && break; sleep 2; done
vk workloads
vk workloads | grep global-pg | grep -q 'RUNNING.*HEALTHY' || { tail -20 "$W/agent.log"; fail "global-pg did not become healthy"; }
pass "global-pg RUNNING and HEALTHY (tcp check)"

CID="$(docker ps -q --filter label=valkvork.service_id=global-pg)"
[ -n "$CID" ] || fail "no container"
BIND="$(docker port "$CID" 5432/tcp)"
[ "$BIND" = "$IP:15432" ] || fail "published on $BIND, want $IP:15432"
pass "5432 published on the advertise address only ($BIND)"

docker inspect -f '{{json .Config.Cmd}}' "$CID" | grep -q 'wal_level=logical' || fail "command not applied"
pass "command applied"

PSQL="docker run --rm --network host -e PGPASSWORD=$PW docker.io/library/postgres:16.4-alpine psql -h $IP -p 15432 -U valkv -d valkv_global -Atc"
[ "$($PSQL 'show wal_level')" = "logical" ] || fail "wal_level is not logical"
pass "psql with the secret password connects; wal_level = logical"
if docker run --rm --network host -e PGPASSWORD=wrong docker.io/library/postgres:16.4-alpine \
    psql -h "$IP" -p 15432 -U valkv -d valkv_global -Atc 'select 1' >/dev/null 2>&1; then
  fail "a wrong password was accepted"
fi
pass "a wrong password is refused"
$PSQL 'create table if not exists survives(x int); insert into survives values (42)' >/dev/null

# The value is nowhere the controller keeps or shows.
vk status global-pg > "$W/status.txt"
grep -q "file:$W/secrets/pg_password" "$W/status.txt" || fail "status does not show the reference"
if grep -q "$PW" "$W/status.txt"; then fail "status shows the password"; fi
if grep -arq "$PW" "$W/db"; then fail "the password is in the controller database"; fi
if grep -q "$PW" "$W/controller.log" "$W/agent.log"; then fail "the password is in a log"; fi
pass "password absent from status, the database and both logs; the reference is shown"

# ── an unresolvable secret ──────────────────────────────────────────────────
cat > "$W/bad.toml" <<EOF
[service]
id       = "bad-secret"
image    = "docker.io/library/nginx:1.27.2-alpine"
replicas = 1

[resources]
cpu    = 0.1
memory = "64MiB"

[secrets]
POSTGRES_PASSWORD = "file:/etc/shadow"
EOF
vk apply -f "$W/bad.toml" >/dev/null
for _ in $(seq 20); do vk events 2>/dev/null | grep -q 'SECRET_UNRESOLVED' && break; sleep 1; done
vk events | grep 'SECRET_UNRESOLVED' | head -1
vk events | grep -q 'bad-secret workload FAILED: SECRET_UNRESOLVED: POSTGRES_PASSWORD (file:/etc/shadow)' \
  || fail "no SECRET_UNRESOLVED event naming the reference"
[ -z "$(docker ps -aq --filter label=valkvork.service_id=bad-secret)" ] || fail "a container was created"
pass "unresolvable secret: FAILED with the reference named, nothing created"
vk delete service bad-secret >/dev/null

# ── a stop-first update keeps the data ──────────────────────────────────────
sed -i 's/kill_timeout = "30s"/kill_timeout = "20s"/' "$W/global-pg.toml"
vk apply -f "$W/global-pg.toml" >/dev/null
sleep 3
for _ in $(seq 90); do
  vk workloads 2>/dev/null | grep -E 'global-pg +2 ' | grep -q HEALTHY && break
  sleep 2
done
vk workloads | grep global-pg
NEW="$(docker ps -q --filter label=valkvork.service_id=global-pg)"
[ "$NEW" != "$CID" ] || fail "the update did not replace the container"
[ "$($PSQL 'select x from survives')" = "42" ] || fail "data lost across the update"
pass "replacing update (stop-first): new container, data kept"

# ── agent restart: adoption keeps the check ─────────────────────────────────
kill "$(pidof valkvork-agent)"
sleep 1
"$BIN/valkvork-agent" --config "$W/agent.toml" > "$W/agent2.log" 2>&1 &
# The restarted agent's own log, not the controller's last view, which is
# still HEALTHY from before the restart.
for _ in $(seq 30); do grep -q "health check passing" "$W/agent2.log" && break; sleep 1; done
grep -q "health check passing" "$W/agent2.log" || { tail -12 "$W/agent2.log"; fail "adopted workload's check never passed"; }
[ "$(docker ps -q --filter label=valkvork.service_id=global-pg)" = "$NEW" ] || fail "agent restart replaced the container"
sleep 3
vk workloads | grep -E 'global-pg +2 ' | grep -q 'RUNNING.*HEALTHY' || { vk workloads; fail "adopted workload not HEALTHY"; }
if grep -q "$PW" "$W/agent2.log"; then fail "the password is in a log"; fi
pass "agent restart: container adopted, its check passes again"

# ── metrics ─────────────────────────────────────────────────────────────────
wget -qO- http://127.0.0.1:9410/metrics | grep '^valkvork_workload_health' | tr '\n' ' '; echo
wget -qO- http://127.0.0.1:9410/metrics | grep -qF 'valkvork_workload_health{health="HEALTHY"} 1' || fail "metric"
pass "valkvork_workload_health reports it"

# ── a failing health check restarts the container, then fails it ───────────
cat > "$W/web.toml" <<EOF
[service]
id       = "web-check"
image    = "docker.io/library/nginx:1.27.2-alpine"
replicas = 1

[resources]
cpu    = 0.1
memory = "64MiB"

[[ports]]
name           = "http"
container_port = 80
host_port      = 18080
host_ip        = "127.0.0.1"

[check]
type     = "http"
port     = "http"
path     = "/missing"
interval = "1s"
failures = 2

[restart]
attempts = 2
delay    = "1s"
EOF
vk apply -f "$W/web.toml" >/dev/null
for _ in $(seq 60); do vk events 2>/dev/null | grep -q 'web-check workload FAILED: UNHEALTHY' && break; sleep 1; done
vk events | grep 'web-check' | head -4
vk events | grep -q 'web-check workload UNHEALTHY' || fail "no UNHEALTHY event"
vk events | grep -q 'web-check workload FAILED: UNHEALTHY' || fail "not FAILED after its restarts"
pass "http check failing (404): restarted under the policy, then FAILED UNHEALTHY"
vk delete service web-check >/dev/null

# ── a batch job, with a secret inside a value ───────────────────────────────
cat > "$W/init.toml" <<EOF
[service]
id       = "pg-init"
type     = "batch"
image    = "docker.io/library/postgres:16.4-alpine"
replicas = 1
command  = ["sh", "-c", "psql \"\$DATABASE_URL\" -v ON_ERROR_STOP=1 -c 'create table if not exists runs(at timestamptz default now()); insert into runs default values'"]

[resources]
cpu    = 0.1
memory = "64MiB"

[secrets]
POSTGRES_PASSWORD = "file:$W/secrets/pg_password"

[env]
DATABASE_URL = "postgresql://valkv:\${secret:POSTGRES_PASSWORD}@$IP:15432/valkv_global"
EOF
vk apply -f "$W/init.toml" >/dev/null
for _ in $(seq 60); do vk workloads 2>/dev/null | grep pg-init | grep -q COMPLETED && break; sleep 1; done
vk workloads | grep pg-init
vk workloads | grep pg-init | grep -q COMPLETED || { tail -20 "$W/agent.log"; fail "pg-init did not complete"; }
[ "$($PSQL 'select count(*) from runs')" = "1" ] || fail "the job's insert is missing"
vk status pg-init | grep -qF 'DATABASE_URL = "postgresql://valkv:${secret:POSTGRES_PASSWORD}@' || fail "status does not show the reference"
if vk status pg-init | grep -qF "$PW"; then fail "status shows the password"; fi
sleep 3
[ -z "$(docker ps -aq --filter label=valkvork.service_id=pg-init)" ] || fail "the completed container was not removed"
vk status pg-init | grep -q '^status: *COMPLETED' || { vk status pg-init | head -4; fail "the service does not show COMPLETED"; }
vk status | grep pg-init | grep -q '1/1 done' || fail "the service list does not show 1/1 done"
pass "batch job COMPLETED once (service status COMPLETED, 1/1 done), connecting with \${secret:...} in its URL; not shown, container removed"

vk plan -f "$W/init.toml" | grep -q 'batch: run again' || fail "plan does not say it runs again"
vk apply -f "$W/init.toml" >/dev/null
for _ in $(seq 60); do [ "$($PSQL 'select count(*) from runs')" = "2" ] && break; sleep 1; done
[ "$($PSQL 'select count(*) from runs')" = "2" ] || fail "re-apply did not run it again"
for _ in $(seq 30); do [ "$(vk workloads | grep -c 'pg-init.*COMPLETED')" = "2" ] && break; sleep 1; done
[ "$(vk workloads | grep -c 'pg-init.*COMPLETED')" = "2" ] || fail "not two COMPLETED runs"
pass "re-applying a finished batch job runs it again (v2), and it completes"
vk delete service pg-init >/dev/null

cat > "$W/fail.toml" <<EOF
[service]
id       = "pg-fail"
type     = "batch"
image    = "docker.io/library/postgres:16.4-alpine"
replicas = 1
command  = ["sh", "-c", "exit 3"]

[resources]
cpu    = 0.1
memory = "64MiB"

[restart]
attempts = 1
delay    = "1s"
EOF
vk apply -f "$W/fail.toml" >/dev/null
for _ in $(seq 60); do vk events 2>/dev/null | grep -q 'pg-fail workload FAILED' && break; sleep 1; done
vk workloads | grep pg-fail | head -3
vk workloads | grep pg-fail | grep -q 'FAILED' || fail "a failing batch job did not fail"
vk workloads | grep pg-fail | grep -E 'FAILED +- +1 ' >/dev/null || fail "it was not restarted once under its policy first"
pass "a batch job exiting 3 is restarted under its policy, then FAILED (and retried by the controller)"
vk delete service pg-fail >/dev/null

# ── an ephemeral volume (examples/ephemeral-volume.toml) ────────────────────
cp /examples/ephemeral-volume.toml "$W/cache.toml"
vk apply -f "$W/cache.toml" >/dev/null
for _ in $(seq 60); do [ "$(vk workloads 2>/dev/null | grep -c 'cache-demo.*RUNNING')" = 2 ] && break; sleep 1; done
[ "$(vk workloads | grep -c 'cache-demo.*RUNNING')" = 2 ] || { tail -20 "$W/agent.log"; fail "cache-demo did not run 2 replicas"; }
CIDS="$(docker ps -q --filter label=valkvork.service_id=cache-demo)"
for c in $CIDS; do
  WID="$(docker inspect -f '{{index .Config.Labels "valkvork.workload_id"}}' "$c")"
  SRC="$(docker inspect -f '{{range .Mounts}}{{if eq .Destination "/var/cache/nginx"}}{{.Source}}{{end}}{{end}}' "$c")"
  [ "$SRC" = "$W/data/volumes/$WID/nginx-cache" ] || fail "mounted from $SRC"
  docker exec "$c" sh -c 'echo hi > /var/cache/nginx/probe'
  [ -f "$SRC/probe" ] || fail "a write in the container is not in $SRC"
done
[ "$(ls "$W/data/volumes" | wc -l)" -ge 2 ] || fail "not one directory per workload"
pass "ephemeral volume: one directory per workload, mounted at its target, writable (nginx's own cache dirs: $(ls "$SRC" | tr '\n' ' '))"
vk delete service cache-demo >/dev/null
for _ in $(seq 30); do [ -z "$(docker ps -aq --filter label=valkvork.service_id=cache-demo)" ] && break; sleep 1; done
sleep 1
[ -z "$(ls "$W/data/volumes" 2>/dev/null)" ] || fail "ephemeral volume directories left behind: $(ls "$W/data/volumes")"
pass "ephemeral volumes are deleted with their workloads"

vk delete service global-pg >/dev/null
for _ in $(seq 30); do [ -z "$(docker ps -aq --filter label=valkvork.service_id=global-pg)" ] && break; sleep 1; done
echo "ALL PASSED"
