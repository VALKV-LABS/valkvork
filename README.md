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
- Built-in web UI
- Production deployment on AWS (GCP equivalent)

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
| Controller | `valkvork-controller` | Admin API, Web UI, SQLite state, reconciler, scheduler, autoscaler |
| Agent | `valkvork-agent` | Stream to controller, container lifecycle, telemetry |
| CLI | `valkvork` | Operator tool: apply, plan, delete, scale, status, etc. |

## Quick Start

### Prerequisites

- Rust 1.70+ (for async traits and other features)
- Docker Engine 27.0+
- SQLite 3.39+
- CMake 3.10+ (for building some dependencies)
- OpenSSL development libraries (for some dependencies)

### Building

```bash
# Clone the repository
git clone https://github.com/valkvlabs/valkvork
cd valkvork

# Build in release mode with musl target
cargo build --release --target x86_64-unknown-linux-musl
```

### Configuration

#### Controller

Create `/etc/valkvork/controller.toml`:

```toml
[agent]
listen_addr = "0.0.0.0:50051"

[admin]
listen_addr = "0.0.0.0:50052"

[database]
path = "/var/lib/valkvork/db/valkvork.db"

[security]
join_token = "your-secret-join-token"
admin_token = "your-secret-admin-token"
ca_file = "/etc/valkvork/ca.pem"
cert_file = "/etc/valkvork/cert.pem"
key_file = "/etc/valkvork/key.pem"

[cluster]
node_unhealthy_timeout_seconds = 10
recovery_grace_period_seconds = 30
heartbeat_interval_ms = 2000

[autoscaler]
evaluation_interval_seconds = 10
cooldown_seconds = 180
scale_up_threshold = 1.1
scale_down_threshold = 0.9
```

#### Agent

Create `/etc/valkvork/agent.toml`:

```toml
controller_endpoint = "https://controlplane.valkvork.internal:50051"
ca_file = "/etc/valkvork/ca.pem"
join_token_file = "/etc/valkvork/join-token"
node_class = "general"
data_dir = "/var/lib/valkvork"
runtime = "docker"
docker_socket = "/var/run/docker.sock"

[reserved]
cpu = 0.25
memory = "384MiB"

[[host_volumes]]
name = "pgdata"
path = "/data/pgdata"
read_only = false
```

### Running

```bash
# Start controller
valkvork-controller

# Start agent
valkvork-agent

# Apply a manifest
valkvork apply -f service.toml

# Check status
valkvork status

# List nodes
valkvork node list
```

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
valkvork/
├── Cargo.toml                 # Workspace root
├── spec.md                    # Full specification
├── crates/
│   ├── valkvork-proto/        # gRPC protocol definitions
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── agent.v1.proto
│   │       └── admin.v1.proto
│   │
│   ├── valkvork-common/       # Shared types and utilities
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── config.rs
│   │       ├── errors.rs
│   │       ├── models.rs
│   │       ├── tls.rs
│   │       ├── units.rs
│   │       └── validation.rs
│   │
│   ├── valkvork-controller/   # Controller service
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── main.rs
│   │       ├── database.rs
│   │       ├── server.rs
│   │       ├── state.rs
│   │       └── ...
│   │
│   ├── valkvork-agent/        # Worker agent
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── main.rs
│   │       ├── agent.rs
│   │       ├── container_runtime.rs
│   │       ├── heartbeat.rs
│   │       ├── node.rs
│   │       └── stream.rs
│   │
│   └── valkvork-cli/          # CLI tool
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs
│           ├── client.rs
│           └── commands.rs
│
└── examples/                 # Example manifests
    ├── frontend.toml
    ├── ml-processor.toml
    └── postgres.toml
```

## Development

### Running Tests

```bash
# Unit tests
cargo test

# Integration tests
cargo test -- --test-threads=1
```

### Code Style

```bash
# Format code
cargo fmt

# Check linting
cargo clippy
```

## License

Apache 2.0

## Contributing

See CONTRIBUTING.md

## Roadmap

- [x] M0: Foundations (proto, TLS, register, heartbeat)
- [ ] M1: Run workloads (manifests, SQLite, apply/plan/delete, scheduler)
- [ ] M2: Resilience (health, LOST/replacement, reconnect, recovery)
- [ ] M3: Change management + UI (updates, drain, autoscaler, events, UI)
- [ ] M4: Production (Packer, Terraform, runbooks)
- [ ] P2: Native containerd driver, block volumes, mTLS, secrets, etc.
