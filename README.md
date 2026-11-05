# Valkvork

Valkvork is a lightweight workload orchestrator and autoscaler written in Rust, following the model of HashiCorp Nomad with one deliberate simplification: **one controller backed by embedded SQLite, with no Raft or consensus**. The controller's availability comes from cloud primitives, a single-instance auto-scaling group, a persistent disk and backups, rather than from clustering.

- Declarative TOML manifests with `apply`, `plan` and `delete`; re-applying the same manifest is a no-op
- Spread scheduling with CPU, memory, node-class, host-port and host-volume filtering
- Automatic replacement of workloads on failed nodes, with exact replica counts after a partition heals
- Rolling updates and node drains, starting each new workload before stopping the old one
- Local restart-on-failure that keeps working while the controller is unreachable
- CPU-based horizontal autoscaling with a dead zone, sustain period and cooldown
- Ephemeral disk limits and node-local host volumes
- A built-in web UI at `/ui`, embedded in the controller binary, with no third-party code
- Prometheus metrics on the controller and the agent
- Static musl binaries with rustls: no glibc or OpenSSL on hosts

**The data plane outlives the control plane:** losing the controller, or the network to it, never stops or restarts a running container.

```
 operators: valkvork CLI / CI, browser
        │  gRPC AdminService and HTTPS /ui, :50052 (TLS, admin token)
        ▼
 valkvork-controller ── SQLite        one instance: desired state, scheduling, autoscaling
        ▲
        │  one gRPC stream per agent, opened by the agent, :50051 (TLS, join token)
        │
 valkvork-agent ── Docker ── your containers ◄── load balancer ◄── users
   on every worker VM
```

[docs/architecture.md](docs/architecture.md) describes every process, port and data flow; [docs/components/](docs/components/index.md) covers each component in depth.

## Quick Start

To try it on your machine, with a controller and an agent against your local Docker and TLS on:

```bash
make dev-up    # build, start a local cluster; prints the UI URL and admin token
make demo      # deploy examples/web.toml and examples/api.toml
make ui        # open the web UI
make help      # every target
```

Then use the CLI:

```bash
eval "$(make -s env)"
target/debug/valkvork status
target/debug/valkvork apply -f examples/api.toml
```

You need Rust, `protoc`, `openssl` and a running Docker (Docker Desktop, Colima, OrbStack or Docker Engine). See [docs/local-development.md](docs/local-development.md) for things to try and the limits of a one-node local cluster.

## A Manifest

```toml
[service]
id       = "frontend-web"
image    = "docker.io/library/nginx:1.27.2-alpine-slim"
replicas = 3

[resources]
cpu    = 0.5          # cores: reserved for scheduling and enforced as a limit
memory = "512MiB"

[[ports]]
name           = "http"
container_port = 80
host_port      = 8080

[autoscale]
min_replicas       = 3
max_replicas       = 10
target_cpu_percent = 75
```

Every key, the validation rules and how changes roll out: [docs/components/manifests.md](docs/components/manifests.md).

## Documentation

| Doc | Covers |
|---|---|
| [architecture.md](docs/architecture.md) | Processes, ports, where state lives, each operation step by step, failure behaviour |
| [components/](docs/components/index.md) | Controller core, state store, reconciler, scheduler, node health, autoscaler, admin API, web UI, metrics, agent, agent stream, supervisor, container runtime, manifests, CLI, configuration, security |
| [placement-and-storage.md](docs/placement-and-storage.md) | Pinning services to nodes, host volumes, and what happens to data on node failure |
| [local-development.md](docs/local-development.md) | Running a cluster on your machine |
| [deployment.md](docs/deployment.md) | Artefacts and releases, the golden image, systemd, the production layout, upgrades |
| [spec.md](spec.md) | The product and technical specification; requirement IDs such as `FR-CON-03` refer to it |
| [NEXT_STEPS.md](NEXT_STEPS.md) | MVP work not built yet |
| [phase-2.md](docs/phase-2.md) | Phase 2 and 3 roadmap: block volumes, containerd, mTLS, secrets, health checks, dynamic ports, batch jobs, RBAC, … |

## Development

```bash
make build     # debug binaries in target/debug/
make test      # unit and behaviour tests; no Docker needed
make ci        # what CI runs: fmt check, clippy -D warnings, tests
make dist      # static musl binaries in dist/ (PLATFORM=linux/amd64 to cross-build)
```

| Path | |
|---|---|
| `crates/valkvork-proto` | `valkvork.v1` protos: the agent stream and AdminService |
| `crates/valkvork-common` | Manifests, configuration, shared models, TLS helpers |
| `crates/valkvork-controller` | The controller, including the web UI (`ui/` holds its assets) |
| `crates/valkvork-agent` | The agent |
| `crates/valkvork-cli` | The `valkvork` CLI |
| `examples/` | Sample manifests |
| `scripts/dev.sh` | The local cluster behind `make dev-*` |

Pull requests run CI (fmt, clippy, tests, static build). Pushes to `main` publish `valkvlabs/valkvork-controller` and `valkvlabs/valkvork-agent` as `edge`; tags `vX.Y.Z` publish releases. See [docs/deployment.md](docs/deployment.md#release-workflow).

## License

Apache 2.0
