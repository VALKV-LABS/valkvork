#!/usr/bin/env bash
# Controller memory at the NFR-01 load (AT-11): 50 simulated agents, 500
# workloads, 10 web UI sessions, churn, and the controller's RSS against
# 50 MiB.
#
#   scripts/load-test.sh            # the controller built for this machine
#   scripts/load-test.sh --linux    # the static musl build from dist/ (make dist),
#                                   # in a container: what nodes run
#
# Environment: NODES, WORKLOADS, SERVICES, UI_SESSIONS, DURATION (seconds of
# full load), LIMIT_MIB, CHURN (true|false). Uses ports 50071, 50072 and 9420
# on localhost, and a fresh directory under target/load-test/.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MODE=native
[ "${1:-}" = "--linux" ] && MODE=linux
W="$ROOT/target/load-test/run-$(date +%s)"
mkdir -p "$W/tls" "$W/db"
log() { printf '\033[1m==>\033[0m %s\n' "$*"; }

(
  cd "$W/tls"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -keyout ca.key -out ca.pem -days 2 -subj "/CN=vk-load-ca" 2>/dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -keyout server.sec1.key -out server.csr -subj "/CN=localhost" 2>/dev/null
  printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n' > ext.cnf
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out server.pem -days 2 -extfile ext.cnf 2>/dev/null
  openssl pkcs8 -topk8 -nocrypt -in server.sec1.key -out server.key
  chmod 0644 server.key
)
openssl rand -hex 24 > "$W/join-token"
openssl rand -hex 32 > "$W/admin-token"

# Paths as the controller sees them: $W natively, /w in the container.
P="$W"
[ "$MODE" = linux ] && P=/w
cat > "$W/controller.toml" <<EOF
agent_listen_addr   = "0.0.0.0:50071"
admin_listen_addr   = "0.0.0.0:50072"
metrics_listen_addr = "0.0.0.0:9420"
database_path       = "$P/db/valkvork.db"
tls_cert_file       = "$P/tls/server.pem"
tls_key_file        = "$P/tls/server.key"
join_token_file     = "$P/join-token"
admin_token_file    = "$P/admin-token"
recovery_grace_period_seconds = 0
EOF

export SDKROOT="${SDKROOT:-/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk}"
[ -d "$SDKROOT" ] || unset SDKROOT
log "Building the harness (release)"
cargo build -q --release -p valkvork-loadtest --manifest-path "$ROOT/Cargo.toml"

cleanup() {
  if [ "$MODE" = linux ]; then
    docker rm -f vk-load >/dev/null 2>&1 || true
  elif [ -n "${CPID:-}" ]; then
    kill "$CPID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

if [ "$MODE" = linux ]; then
  [ -x "$ROOT/dist/valkvork-controller" ] || { echo "dist/valkvork-controller missing: run make dist" >&2; exit 1; }
  log "Starting dist/valkvork-controller (static musl) in a container"
  docker rm -f vk-load >/dev/null 2>&1 || true
  docker run -d --name vk-load -p 50071:50071 -p 50072:50072 -p 9420:9420 \
    -v "$W:/w" -v "$ROOT/dist:/dist:ro" alpine:3.20 \
    /dist/valkvork-controller --config /w/controller.toml >/dev/null
  RSS_CMD="docker exec vk-load cat /proc/1/status"
  LOGS="docker logs vk-load"
else
  log "Building and starting valkvork-controller (release)"
  cargo build -q --release -p valkvork-controller --manifest-path "$ROOT/Cargo.toml"
  "$ROOT/target/release/valkvork-controller" --config "$W/controller.toml" > "$W/controller.log" 2>&1 &
  CPID=$!
  RSS_CMD="ps -o rss= -p $CPID"
  LOGS="cat $W/controller.log"
fi

for _ in $(seq 30); do
  nc -z localhost 50072 >/dev/null 2>&1 && break
  sleep 1
done
nc -z localhost 50072 >/dev/null 2>&1 || { $LOGS | tail -20; echo "controller did not start" >&2; exit 1; }

log "Running the load ($MODE)"
set +e
"$ROOT/target/release/valkvork-loadtest" \
  --agent-endpoint https://localhost:50071 \
  --admin-endpoint https://localhost:50072 \
  --ca-file "$W/tls/ca.pem" \
  --join-token-file "$W/join-token" \
  --admin-token-file "$W/admin-token" \
  --nodes "${NODES:-50}" --workloads "${WORKLOADS:-500}" --services "${SERVICES:-10}" \
  --ui-sessions "${UI_SESSIONS:-10}" --duration-seconds "${DURATION:-120}" \
  --churn "${CHURN:-true}" --limit-mib "${LIMIT_MIB:-50}" \
  --rss-command "$RSS_CMD"
rc=$?
set -e
echo "controller ERROR log lines: $($LOGS 2>&1 | grep -c '"level":"ERROR"' || true)"
echo "Logs and database: $W"
exit $rc
