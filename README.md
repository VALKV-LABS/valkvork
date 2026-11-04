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
- Built-in web UI *(not implemented yet)*
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

### Prerequisites

- Rust (stable) and `protoc` (`brew install protobuf` / `apt install protobuf-compiler`)
- Docker Engine on the worker (spec: 27.0+ with `live-restore`, cgroup v2)
- On macOS, if linking fails with an SDK error:
  `export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk`

### Building

```bash
cargo build --release          # native binaries in target/release/
cargo test --workspace         # unit and behaviour tests

# Static musl binaries for Linux (NFR-03): writes dist/valkvork-controller,
# dist/valkvork-agent and dist/valkvork
docker buildx build --platform linux/amd64 --target export --output dist/ .
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

[autoscaler]
enabled          = true
cooldown_seconds = 180
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

Add `-o json` for machine-readable output. Exit codes: `0` success, `1`
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

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt
```

## License

Apache 2.0

## Roadmap

- [x] M0: Foundations (proto, TLS, register, heartbeat)
- [x] M1: Run workloads (manifests, SQLite, apply/plan/delete, scheduler, Docker runtime)
- [x] M2: Resilience (health, LOST/replacement, reconnect rules, recovery grace, adoption, local restarts)
- [ ] M3: Change management + UI: rolling updates, drain, autoscaler, events, disk limits and host volumes are done; web UI and metrics remain
- [ ] M4: Production: Packer image in [goldenimage#7](https://github.com/VALKV-LABS/goldenimage/pull/7); Terraform, Litestream and runbooks remain
- [ ] P2: Native containerd driver, block volumes, mTLS, secrets, etc.
