# Architecture

This document describes every Valkvork process, the ports each one opens, where state lives, and how commands and data move between the processes for each operation. [`spec.md`](../spec.md) gives the requirements and the reasons for the design. [`docs/components/`](components/index.md) describes each component in depth.

Valkvork runs long-lived container services across a fleet of worker VMs, in the manner of HashiCorp Nomad, with one deliberate simplification: **one controller backed by embedded SQLite, with no Raft or consensus.** The controller's availability comes from cloud primitives (a single-instance auto-scaling group, a persistent disk and backups), not from clustering.

Two rules shape everything below:

- **The data plane outlives the control plane.** Losing the controller, or the network to it, never stops, restarts or changes a running container. Agents keep running and wait.
- **One brain.** Only the controller decides what runs where and how many. Agents carry out commands and report facts. The one exception is restarting a crashed container in place, which the agent does on its own under the service's restart policy.

## Processes

| Process | Binary | Runs | Decides | Talks to |
|---|---|---|---|---|
| Controller | `valkvork-controller` | One per cluster | Desired state, placement, replacement, rolling updates, drains, autoscaling | Agents over each agent's own stream. Operators over the admin API and the web UI. Its SQLite database |
| Agent | `valkvork-agent` | One per worker VM | Nothing, except restarting a crashed container in place | The controller, by dialling out. The local Docker Engine over its Unix socket. cgroup v2 files |
| Docker Engine | `dockerd` + `containerd` | One per worker VM | Nothing | Only the agent, over `/var/run/docker.sock` |
| Workloads | your images | Any number per worker | — | Users reach them on host ports, through your load balancer. Never through Valkvork |
| CLI | `valkvork` | An operator's machine or CI | Nothing | The controller's admin API |
| Web UI | served by the controller | An operator's browser | Nothing | The controller's admin listener, same port as the admin API |

```
                               OPERATORS
       ┌──────────────────────┐            ┌──────────────────────┐
       │ valkvork CLI / CI    │            │ Browser (web UI)     │
       └──────────┬───────────┘            └──────────┬───────────┘
                  │ gRPC AdminService                 │ HTTPS /ui, JSON, SSE
                  │ bearer admin token                │ session cookie + CSRF
                  └────────────────┬──────────────────┘
                                   ▼  :50052  one listener, split by content type
   ┌────────────────────────────────────────────────────────────────────┐
   │ CONTROLLER · valkvork-controller                                   │
   │                                                                    │
   │   core task ── owns all state, the only SQLite writer              │
   │     reconciler · scheduler · dispatcher · health · rollouts        │
   │     drains · autoscaler · history pruning · chart samples          │
   │                                                                    │
   │   SQLite (WAL, synchronous=FULL)      /metrics :9400               │
   └──────────────────────────────▲──────────────────┬──────────────────┘
                                  │                  │
       Register, Heartbeat (2 s), │  one gRPC stream │ RegisterAck,
       CommandResult              │  per agent,      │ StartWorkload,
                                  │  opened by agent │ StopWorkload
                                  │  :50051          ▼
   ┌──────────────────────────────┴─────────────────────────────────────┐
   │ WORKER VM · valkvork-agent                   /metrics 127.0.0.1:9401│
   │                                                                    │
   │   connection ── supervisor ── ContainerRuntime ── DockerRuntime    │
   │                     │                                  │           │
   │                     │ reads cgroup v2 CPU/memory       ▼           │
   │                     └──────────────────────────── app containers   │
   └────────────────────────────────────────────────────────▲───────────┘
                                                            │ host ports, e.g. :8080
                               end users ──► load balancer ─┘ (never through Valkvork)
```

## Ports

| Process | Port | Default | Reachable from | Protocol and authentication |
|---|---|---|---|---|
| Controller, agent stream | `agent_listen_addr` | `0.0.0.0:50051` | Worker subnets only | gRPC over TLS. Agents present the join token in their first message |
| Controller, admin API and web UI | `admin_listen_addr` | `0.0.0.0:50052` | Operators and CI only | gRPC (`application/grpc`) with a bearer admin token, and HTTPS `/ui` with a session cookie. Same TLS certificate. See [admin_api.md](components/admin_api.md) and [web_ui.md](components/web_ui.md) |
| Controller metrics | `metrics_listen_addr` | `0.0.0.0:9400` | The monitoring system | Plain HTTP `GET /metrics` |
| Agent metrics | `metrics_listen_addr` | `127.0.0.1:9401` | The worker itself | Plain HTTP. Must be a loopback address; the agent refuses to start otherwise |
| Agent | none | | | The agent opens no other listener. It only dials out |
| Workloads | `host_port` from manifests | | Your load balancer | Whatever the application speaks |

Without `tls_cert_file`/`tls_key_file` the controller serves both gRPC listeners in plaintext and logs a warning. That is for development only.

## Where State Lives

| Data | Owner | Stored | Survives a controller crash? |
|---|---|---|---|
| Services: spec, version, target replicas, status, rollout status | Controller | SQLite, committed before any reply | Yes |
| Workloads: desired state, status, node, reason, restart count | Controller | SQLite | Yes |
| Nodes: identity, capacity, class, volumes, draining, status | Controller | SQLite. `last_heartbeat_at` is flushed every 30 s and on status changes | Yes. Liveness is rebuilt from heartbeats during the recovery grace period |
| Cluster events, scaling events | Controller | SQLite, 30 days | Yes |
| Node liveness, usage, open streams | Controller | Memory only | No. Agents reconnect and register again |
| Outstanding commands | Controller | Memory only | No. Re-derived from workload state and re-sent |
| Per-workload CPU samples (60 s window), chart samples (60 min) | Controller | Memory only | No. They refill |
| UI sessions | Controller | Memory only | No. Operators sign in again |
| What actually runs | Agent and Docker | Docker; the agent re-reads it at start | Yes, the containers keep running |
| `node_id` | Agent | `data_dir/node_id` | Yes |
| "This container failed for good" marker | Agent | `data_dir/failed/<workload_id>` | Yes, so a failed container is not restarted when the agent restarts |

**Desired state lives in the controller and actual state comes from agents.** The controller never assumes a workload runs until an agent reports it running.

## Inside the Controller

Everything that changes state runs in one Tokio task, the **core** ([controller_core.md](components/controller_core.md)). The gRPC handlers, the web UI and the metrics endpoint are thin adapters that send it messages and wait for the answer. There are no locks around cluster state and exactly one SQLite writer.

```
 AgentService.Connect ──┐
 AdminService (gRPC) ───┤   mpsc
 Web UI (/ui/api) ──────┼────────►  core task ──── SQLite (one write connection)
 /metrics ──────────────┘             │  ▲
                                      │  └── timers: reconcile 5 s, health 1 s,
                                      │      heartbeat flush 30 s, autoscale 10 s,
                                      │      chart sample 60 s, prune 10 min
                                      │
                                      ├──► per-agent stream senders (commands)
                                      ├──► admin replies (after commit)
                                      └──► broadcast "changed" (UI live updates)

 history reads (events, scaling events) ── read-only SQLite connection
```

Each step of the core ends the same way:

1. The handler changes the in-memory tables and marks rows dirty.
2. All dirty rows and new events are written in **one transaction**, fsync'd.
3. Only then are commands sent to agents, replies sent to callers, and a change notification broadcast.

So an acknowledged `apply` is on disk, and an agent is never asked to start a workload the database does not know about. If a commit fails, the controller exits and systemd restarts it; it recovers from SQLite.

## Operations, Step by Step

### An agent joins, then reports

```
 agent                                         controller
   │ read agent.toml, load node_id               │
   │── TLS connect :50051 ───────────────────────►│
   │── Register (identity, capacity, class,      │ check join token
   │   host volumes, runtime, full inventory) ───►│ upsert node → HEALTHY
   │                                             │ compare inventory with workloads
   │◄─────────────────────────────── RegisterAck ─│ (always the first message back)
   │◄──────────── re-sent unresolved commands ────│
   │── Heartbeat every 2 s: node usage and ──────►│ last_seen = controller clock
   │   per-container state, CPU, memory, disk    │ feed the autoscaler window
```

The controller uses its own clock for liveness and never trusts agent timestamps. See [agent_stream.md](components/agent_stream.md) and [node_health.md](components/node_health.md).

### `valkvork apply` of a new or changed service

```
 operator         controller                                  agent           Docker
   │── Apply ───────►│ parse, validate (VE-01…07), canonicalise  │               │
   │                 │ classify: created / unchanged /           │               │
   │                 │   scale-only / policy-only / replacing    │               │
   │                 │ write service row, COMMIT (fsync)         │               │
   │◄── change ──────│                                           │               │
   │                 │ reconcile: live < target → PENDING rows   │               │
   │                 │ scheduler: filter, score → ASSIGNED       │               │
   │                 │ COMMIT, then ── StartWorkload ───────────►│               │
   │                 │◄──────────────────────── CommandResult ───│── pull ──────►│
   │                 │                                           │── create ────►│
   │                 │                                           │── start ─────►│
   │                 │◄────────────────── Heartbeat: RUNNING ────│               │
   │                 │ status → RUNNING                          │               │
```

An identical re-apply writes nothing and sends nothing. A "replacing" change bumps the service version and starts a rolling update. See [manifests.md](components/manifests.md), [reconciler.md](components/reconciler.md) and [scheduler.md](components/scheduler.md).

### Rolling update and drain

Both use start-new-then-stop-old, `max_parallel` workloads at a time:

```
 for each outdated workload (or each workload on a draining node):
   create a replacement (PENDING, replaces_workload_id = old) → schedule → start
   wait until the replacement has been RUNNING for min_healthy_time
   stop the old workload (STOPPING → STOPPED)
```

The RUNNING count never drops below the target. If a new-version workload fails, the rollout halts and the old workloads keep running until the next apply. See [reconciler.md](components/reconciler.md#rolling-updates-and-drains).

### A worker dies or is cut off, then comes back

```
 agent A          controller                                 agent B
   │ dies/cut off   │                                           │
   │                │ 10 s with no message from A               │
   │                │ A → UNHEALTHY; its workloads → LOST        │
   │                │ one replacement per LOST workload          │
   │                │ schedule onto B ── StartWorkload ─────────►│
   │                │◄────────────────────── Heartbeat RUNNING ──│
   │ link restored  │                                           │
   │── Register ───►│ inventory still has the old copy          │
   │                │ replacement placed → stop the old copy    │
   │◄─ StopWorkload │ old copy → STOPPED; replicas exact again  │
```

If the replacement could not be placed (for example, the workload needs a host volume only A has), the old copy is re-adopted instead and the pending replacement deleted. Throughout, agent A never touches its containers on its own. See [node_health.md](components/node_health.md).

### The controller restarts

```
 controller starts → load nodes, services, workloads from SQLite
                   → every node's last_seen = now; enter a 30 s recovery grace period
 during grace:       no node is marked UNHEALTHY, no replacements, no orphan stops
 agents reconnect:   Register with their inventory → normal FR-CON-03 handling
 after grace:        normal reconciliation
```

No healthy running workload is stopped or restarted by a controller restart. The grace period is longer than the agents' maximum reconnect backoff (15 s, ±20 %).

### The autoscaler

Every 10 s, for each service with `[autoscale]`: take the 60 s average CPU of its RUNNING workloads as a percentage of their limit; if it stays above `target × 1.1` for 2 minutes, raise `target_replicas` (and below `target × 0.9`, lower it, by at most half). Then a 3-minute cooldown. The change goes through the normal reconcile path. See [autoscaler.md](components/autoscaler.md).

### The web UI

```
 browser                          controller (:50052)
   │── GET /ui/ ──────────────────►│ embedded, gzipped HTML/CSS/JS (non-gRPC request)
   │── POST /ui/api/login ────────►│ admin token → session cookie + CSRF token
   │── GET /ui/api/overview ──────►│ same core operations as AdminService
   │── GET /ui/api/events (SSE) ──►│ one "change" event per committed state change
   │── POST …/drain + CSRF header ►│ same code path as AdminService.DrainNode, audited
```

See [web_ui.md](components/web_ui.md).

## Failure Behaviour

| Failure | Detected by | What happens | Effect on workloads |
|---|---|---|---|
| Controller process crash | systemd | Restart, load SQLite, 30 s recovery grace | None. Scaling and replacement pause briefly |
| Controller database write fails | The core | The controller exits; systemd restarts it from the last commit | None |
| Worker dies | 10 s without a message | Node UNHEALTHY, workloads LOST, replacements placed | Replicas on that node are down until replacements start |
| Network partition | 10 s without a message | Replacements placed; on heal, lost copies are stopped or re-adopted | Briefly more replicas than the target, then exact |
| Agent crash or upgrade | systemd | The agent restarts and re-adopts its containers by label | None |
| dockerd restart | systemd | `live-restore` keeps containers alive; `Requires=docker.service` restarts the agent, which re-adopts | None |
| Container crash | Docker events | Restart in place under the restart policy, then FAILED; the controller replaces it with backoff, and marks the service DEGRADED after 5 failures in a row | Degraded capacity for that service |
| Bad image | Agent reports IMAGE_PULL_FAILED | Workload FAILED; a rollout halts with the old replicas still running | None during a rollout |
| Out of capacity | Scheduler | Workloads stay PENDING with a `NO_NODE: …` reason; `valkvork_pending_unplaced_workloads` rises | Delayed scale-up |

## Code Map

| Path | What it is |
|---|---|
| `crates/valkvork-proto` | `valkvork.v1` protos: the agent stream and AdminService |
| `crates/valkvork-common` | Manifest parsing and validation, configuration, shared models, units, TLS helpers |
| `crates/valkvork-controller/src/core/` | The core task: `mod.rs` (commit and dispatch), `agents.rs`, `reconcile.rs`, `autoscale.rs`, `admin.rs`, `observe.rs` |
| `crates/valkvork-controller/src/scheduler.rs` | Placement (pure functions) |
| `crates/valkvork-controller/src/db.rs` | Schema migrations, load, batched commits, read-only history queries |
| `crates/valkvork-controller/src/grpc.rs`, `http.rs`, `ui/` | gRPC adapters, the content-type split and metrics listener, the web UI |
| `crates/valkvork-controller/ui/` | The UI's HTML, CSS and JavaScript, embedded at build time |
| `crates/valkvork-agent/src/` | `connection.rs`, `supervisor.rs`, `runtime.rs`, `telemetry.rs`, `identity.rs`, `metrics.rs` |
| `crates/valkvork-cli` | The `valkvork` binary |
