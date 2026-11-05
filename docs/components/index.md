# Valkvork Component Index

[architecture.md](../architecture.md) shows how these fit together: every process, its ports, where state lives, and the data flow of each operation. [`spec.md`](../../spec.md) holds the requirements; requirement IDs such as `FR-CON-03` refer to it.

## Controller (`valkvork-controller`)

| Component | Doc | Purpose |
|-----------|-----|---------|
| `core` | [controller_core.md](controller_core.md) | The single task that owns cluster state: commit-then-send, the dispatcher, timers |
| `state_store` | [state_store.md](state_store.md) | SQLite schema, migrations, the one writer, read-only history, retention |
| `reconciler` | [reconciler.md](reconciler.md) | Replica counts, rolling updates, drains, deletes, start timeouts, failure backoff |
| `scheduler` | [scheduler.md](scheduler.md) | Spread placement: filters, least-allocated score, `NO_NODE` reasons |
| `node_health` | [node_health.md](node_health.md) | Liveness, LOST and replacement, reconnect rules (FR-CON-03), recovery grace |
| `autoscaler` | [autoscaler.md](autoscaler.md) | CPU-based horizontal scaling with sustain, dead zone and cooldown |
| `admin_api` | [admin_api.md](admin_api.md) | gRPC `AdminService`: operations, authentication, errors |
| `web_ui` | [web_ui.md](web_ui.md) | The built-in UI at `/ui`: pages, sessions, CSRF, SSE, embedded assets |
| `metrics` | [metrics.md](metrics.md) | Prometheus endpoints on the controller and the agent |

## Agent (`valkvork-agent`)

| Component | Doc | Purpose |
|-----------|-----|---------|
| `agent` | [agent.md](agent.md) | The agent process: startup, identity, capacity, telemetry |
| `agent_stream` | [agent_stream.md](agent_stream.md) | The one outbound stream: Register, Heartbeat, commands and acks, reconnect backoff |
| `supervisor` | [supervisor.md](supervisor.md) | Local workload lifecycle: start/stop, restart policy, adoption, disk limits |
| `container_runtime` | [container_runtime.md](container_runtime.md) | The `ContainerRuntime` trait and its Docker driver |

## Shared

| Component | Doc | Purpose |
|-----------|-----|---------|
| `manifests` | [manifests.md](manifests.md) | TOML format, validation (VE-01…07), canonical form, change classification |
| `cli` | [cli.md](cli.md) | The `valkvork` operator CLI |
| `configuration` | [configuration.md](configuration.md) | `controller.toml` and `agent.toml` reference |
| `security` | [security.md](security.md) | TLS, join and admin tokens, UI sessions, Docker socket protection |

## Building, Running and Deploying

| Doc | Covers |
|-----|--------|
| [local-development.md](../local-development.md) | `make dev-up`: a controller and an agent on your machine against local Docker |
| [deployment.md](../deployment.md) | Static binaries, images, the golden image, systemd units, production layout |
| [NEXT_STEPS.md](../../NEXT_STEPS.md) | What is not built yet |

## Data Flow Summary

```
operator ── CLI (gRPC) or browser (/ui) ──► controller :50052
                                               │
                                   core task ──┼── SQLite (one writer)
                                               │
          ┌────────────────────────────────────┘
          │ reconciler → scheduler → dispatcher
          ▼
agent stream :50051 (opened by the agent)
          │ StartWorkload / StopWorkload ▼        ▲ Register / Heartbeat / CommandResult
          ▼                                       │
       agent: connection → supervisor → ContainerRuntime → Docker → containers
                               └── cgroup v2 CPU/memory, alloc dir size ──┘
```
