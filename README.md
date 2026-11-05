# Valkvork

Valkvork is a lightweight workload orchestrator and autoscaler written in Rust, following the model of HashiCorp Nomad with one deliberate simplification: **one control node backed by embedded SQLite, with no Raft or consensus**.

## Features

- Single controller with SQLite state store
- Multiple worker agents (valkvork-agent) on worker VMs
- Declarative TOML manifests
- Spread scheduling with resource and node-class filtering
- Automatic replacement of workloads on failed nodes
- Rolling updates on spec change
- CPU-based horizontal autoscaling with cooldown
- Basic storage: ephemeral disk limits and node-local host volumes
- Built-in web UI at `/ui` on the admin port, embedded in the controller binary
- Prometheus metrics on the controller (`:9400`) and agent (`127.0.0.1:9401`)
- Production deployment on AWS via the [goldenimage](https://github.com/VALKV-LABS/goldenimage) Packer image

See [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md) for what is built and what remains.

## Architecture

```
                              OPERATORS
      ┌─────────────────────┐            ┌─────────────────────┐
      │ valkvork CLI  /  CI │            │  Browser  (Web UI)  │
      └──────────┬──────────┘            └──────────┬──────────┘
                 │ [1] gRPC AdminService            │ [2] HTTPS /ui
                 │     apply · plan · scale · drain │     pages · actions · live view
                 └────────────────┬─────────────────┘
                                  ▼  :50052  (TLS + admin token or UI session)
   ┌────────────────────────────────────────────────────────────────────┐
   │ CONTROL PLANE · 1 VM · valkvork-controller                         │
   │ Decides what runs, where, and how many. Never runs app containers. │
   │                                                                    │
   │   SQLite (desired state) ────── [6] WAL stream ──────► S3 backup   │
   │                                                                    │
   └──────────────────────────────▲───────────────────┬─────────────────┘
                                  │                   │
     [4] agent → controller       │                   │   [3] controller → agent
     Register (on connect)        │  one gRPC stream  │   StartWorkload
     Heartbeat (every 2 s)        │    per agent,     │   StopWorkload
     CommandResult (acks)         │ opened by agent   │
                                  │  port :50051      │
   ┌──────────────────────────────┴───────────────────▼─────────────────┐
   │ DATA PLANE · N worker VMs · one valkvork-agent each                │
   │ Does what it is told; reports facts. Keeps apps up if cut off.     │
   │                                                                    │
   │   valkvork-agent ──► Docker API ──► dockerd ──► containerd         │
   │         ▲                                            │             │
   │         │ [7] reads CPU/memory from cgroup files     ▼             │
   │         └─────────────────────────────────── app containers        │
   │                                                     ▲              │
   └─────────────────────────────────────────────────────┼──────────────┘
                                                         │ [5] user traffic → host ports (e.g. :8080)
                                End users ──► Public LB ─┘
```

## Components

| Component | Binary | Responsibilities |
| :--- | :--- | :--- |
| Controller | `valkvork-controller` | Admin API, SQLite state, reconciler, scheduler, autoscaler |
| Agent | `valkvork-agent` | Stream to controller, container lifecycle, telemetry |
| CLI | `valkvork` | Operator tool: apply, plan, delete, scale, status, etc. |

## Quick Start

To try it on your machine (controller and agent against local Docker, TLS on):

```bash
make dev-up    # start a local cluster; prints the UI URL and admin token
make demo      # deploy the example services
make ui        # open the web UI
make help      # all targets
```

See [docs/local-development.md](docs/local-development.md) for the CLI setup,
things to try, and the limits of a one-node local cluster. The rest of this
section covers building and configuring Valkvork for real hosts.

### Prerequisites

- Rust (stable) and `protoc` (`brew install protobuf` / `apt install protobuf-compiler`)
- Docker Engine on the worker (spec: 27.0+ with `live-restore`, cgroup v2)
- On macOS, if linking fails with an SDK error:
  `export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk`

### Building

```bash
make build-release             # native binaries in target/release/
make ci                        # fmt check, clippy -D warnings, tests

# Static musl binaries for Linux (NFR-03) in dist/
make dist PLATFORM=linux/amd64
```

### Configuration

Every key has a default and unknown keys are rejected. Both binaries accept
`--check-config` to validate a file (including token and TLS files) and exit.

#### Controller (`/etc/valkvork/controller.toml`)

```toml
agent_listen_addr = "0.0.0.0:50051"   # agent streams
admin_listen_addr = "0.0.0.0:50052"   # admin API (CLI)
tls_cert_file     = "/etc/valkvork/controller.pem"
tls_key_file      = "/etc/valkvork/controller.key"   # PKCS#8 PEM
database_path     = "/var/lib/valkvork/db/valkvork.db"
join_token_file   = "/etc/valkvork/join-token"       # or join_token = "..."
admin_token_file  = "/etc/valkvork/admin-token"      # or admin_token = "...", >= 32 random bytes

# Optional tuning (spec defaults shown)
node_unhealthy_timeout_seconds = 10
recovery_grace_period_seconds  = 30
command_timeout_seconds        = 30
start_timeout_seconds          = 300

metrics_listen_addr = "0.0.0.0:9400"   # Prometheus; remove to disable

[autoscaler]
enabled          = true
cooldown_seconds = 180

[ui]
enabled             = true     # web UI at https://<controller>:50052/ui/
session_ttl_seconds = 43200
```

Without `tls_cert_file`/`tls_key_file` the controller serves plaintext and
logs a warning (development only).

#### Agent (`/etc/valkvork/agent.toml`)

```toml
controller_endpoint = "https://controlplane.valkvork.internal:50051"
ca_file             = "/etc/valkvork/ca.pem"
join_token_file     = "/etc/valkvork/join-token"
node_class          = "general"
data_dir            = "/var/lib/valkvork"
runtime             = "docker"
docker_socket       = "/var/run/docker.sock"
registry_auth_file  = "/etc/valkvork/registry-auth.json"   # optional, Docker config.json format
# advertise_ip      = "10.0.1.23"                          # default: route towards the controller

[reserved]
cpu    = 0.25
memory = "384MiB"

[host_volumes.pgdata]
path      = "/data/pgdata"
read_only = false
```

The agent runs as root: it drives the Docker socket and bind-mounts host
volumes and the per-workload `/alloc` directory.

### Running

```bash
valkvork-controller --config /etc/valkvork/controller.toml
valkvork-agent      --config /etc/valkvork/agent.toml

export VALKVORK_ADDR=https://controlplane.valkvork.internal:50052
export VALKVORK_CA_FILE=/etc/valkvork/ca.pem
export VALKVORK_ADMIN_TOKEN=...

valkvork plan -f service.toml            # diff + placement check, writes nothing
valkvork apply -f service.toml
valkvork status [service]
valkvork workloads [--service S] [--node N] [--status RUNNING]
valkvork scale service <id> <n>
valkvork delete service <id>
valkvork events [--kind NODE|WORKLOAD|DEPLOY|SCALE|AUDIT]
valkvork node list | drain <id> | undrain <id>
```

### Web UI

Open `https://<controller>:50052/ui/` and sign in with the admin token. The
UI shares the admin listener with gRPC (requests are split by content type),
so it needs no extra port or certificate. It covers the overview, services
(scale, delete), nodes (drain, undrain), workloads, events and deploy
(plan, then apply). Pages update live over Server-Sent Events and fall back
to polling. Every action is recorded as an `AUDIT` event with actor
`ui:<session>`.

The assets are hand-written HTML, CSS and JavaScript with no third-party
code, compiled into the binary and gzipped at build time
(`crates/valkvork-controller/ui/`). Responses carry the CSP
`default-src 'self'; img-src 'self' data:; frame-ancestors 'none'`, the
session cookie is `HttpOnly; SameSite=Strict` (and `Secure` with TLS), and
mutating requests need the `X-Valkvork-CSRF` header.

### Metrics

`GET /metrics` in the Prometheus text format:

- controller (`metrics_listen_addr`, default `0.0.0.0:9400`):
  `valkvork_nodes{status,class}`, `valkvork_workloads{status}`,
  `valkvork_pending_unplaced_workloads{class}`,
  `valkvork_allocated_ratio{class,resource}`,
  `valkvork_service_cpu_utilization{service}`,
  `valkvork_scaling_actions_total{service,direction}`,
  `valkvork_schedule_duration_seconds`, plus agent connections, outstanding
  commands, recovery grace, uptime and database size;
- agent (`metrics_listen_addr`, default `127.0.0.1:9401`, loopback only):
  `valkvork_agent_reconnects_total`, `valkvork_agent_connected`, heartbeats,
  commands by result, local restarts and workloads by state.

The CLI takes `-o json` for machine-readable output. Exit codes: `0` success, `1`
server or transport error, `2` validation error.

## Manifest Examples

### Stateless Web Service

```toml
[service]
id = "frontend-web"
image = "docker.io/library/nginx:1.27.2-alpine-slim"
replicas = 3

[resources]
cpu = 0.5
memory = "512MiB"

[constraints]
required_class = "general"

[[ports]]
name = "http"
container_port = 80
host_port = 8080

[autoscale]
min_replicas = 3
max_replicas = 10
target_cpu_percent = 75
```

### Stateful Service with Host Volume

```toml
[service]
id = "postgres-main"
image = "docker.io/library/postgres:16.4"
replicas = 1

[resources]
cpu = 2.0
memory = "4GiB"
disk = "20GiB"

[[volumes]]
type = "host"
source = "pgdata"
target = "/var/lib/postgresql/data"
```

## Project Structure

```
crates/
├── valkvork-proto/        valkvork.v1 protos (agent stream + AdminService)
├── valkvork-common/       manifest parsing/validation, config, models, units, TLS helpers
├── valkvork-controller/
│   └── src/
│       ├── core/          the single actor that owns state and the SQLite writer:
│       │   ├── mod.rs        commit-then-send, dispatcher (command retries)
│       │   ├── agents.rs     Register/FR-CON-03, heartbeats, node health
│       │   ├── reconcile.rs  scaling, rollouts, drains, deletes, backoff
│       │   ├── autoscale.rs  FR-AUT
│       │   ├── admin.rs      apply/plan/scale/drain + read views
│       │   └── tests.rs      behaviour tests (no network)
│       ├── scheduler.rs   spread placement (pure)
│       ├── db.rs          schema migrations, load, batched commits, read-only history
│       ├── grpc.rs        AgentService / AdminService adapters, admin auth
│       └── server.rs      listeners and TLS
├── valkvork-agent/
│   └── src/
│       ├── runtime.rs     ContainerRuntime trait + DockerRuntime (bollard)
│       ├── supervisor.rs  start/stop, restart policy, adoption, disk limits, telemetry
│       ├── connection.rs  outbound stream, Register/heartbeat, backoff
│       ├── identity.rs    node id, hostname, address
│       └── telemetry.rs   cgroup v2 and /proc readers
└── valkvork-cli/          `valkvork` binary
```

Also: `examples/` (sample manifests), `scripts/dev.sh` (the local cluster
behind `make dev-*`), `docs/` (guides).

## Development

```bash
make test      # unit and behaviour tests
make ci        # fmt check, clippy -D warnings, tests
make fmt
```

## License

Apache 2.0

## Roadmap

- [x] M0: Foundations (proto, TLS, register, heartbeat)
- [x] M1: Run workloads (manifests, SQLite, apply/plan/delete, scheduler, Docker runtime)
- [x] M2: Resilience (health, LOST/replacement, reconnect rules, recovery grace, adoption, local restarts)
- [x] M3: Change management + UI (rolling updates, drain, autoscaler, events, disk limits, host volumes, metrics, web UI)
- [ ] M4: Production: Packer image in [goldenimage#7](https://github.com/VALKV-LABS/goldenimage/pull/7); Terraform, Litestream and runbooks remain
- [ ] P2: Native containerd driver, block volumes, mTLS, secrets, etc.
