# Component: Controller Core

## High-Level Function

The core is the one Tokio task in `valkvork-controller` that owns all cluster state. Every input arrives as a message: agent registrations, heartbeats and command results, admin requests from gRPC and the web UI, and timer ticks. Because only the core changes state, there are no locks around it, and the core owns the only SQLite write connection (FR-CON-01's "writer task"). The code is in `crates/valkvork-controller/src/core/`.

## Messages

```rust
pub enum CoreMsg {
    AgentConnected { register, conn_id, tx },   // first message of a stream
    AgentMsg { node_id, conn_id, msg },         // Heartbeat or CommandResult
    AgentDisconnected { node_id, conn_id },
    Admin(AdminOp),                             // every admin and UI operation
}
```

The gRPC adapters (`grpc.rs`), the web UI (`ui/`) and the metrics endpoint (`http.rs`) hold a cloneable `CoreHandle`. `CoreHandle::ask` sends an `AdminOp` that carries a reply channel and waits for the answer. The core's inbox is bounded (1024 messages), so a burst of requests waits rather than growing memory without limit.

`AdminOp` covers `Apply` (and plan, as a dry run), `Delete`, `Scale`, `SetDraining`, `GetService`, `ListServices`, `ListWorkloads`, `ListNodes`, `Overview`, `ServiceSeries` and `Metrics`. The CLI and the web UI use the same operations, so there is one code path for each action (FR-UI-05).

## In-Memory State

| Field | Holds |
|---|---|
| `nodes` | Every known node: its row, `last_seen` (controller clock), the open stream's sender, latest usage |
| `services` | Every service row, including the canonical spec |
| `workloads` | Every workload row, terminal ones included until pruned |
| `svc_rt` | Per service, runtime only: failure streak, last failure, autoscaler window start times |
| `assigned_at` | When each ASSIGNED workload was bound, for the start timeout |
| `outstanding` | Commands sent and not yet acknowledged, one per workload |
| `cpu_samples` | Per workload, the last 60 s of CPU readings |
| `metrics`, `series` | Counters and the 60-minute chart ring buffers. See [metrics.md](metrics.md) |

At startup the core loads nodes, services and workloads from SQLite, sets every node's `last_seen` to the start time, and enters the recovery grace period. See [node_health.md](node_health.md#controller-restart-recovery).

## Commit, Then Send

Handlers never write to SQLite or send anything directly. They change memory and record side effects:

| Recorded | By |
|---|---|
| Dirty node, service and workload ids | `touch_node`, `touch_service`, `wl()` (mutable access that marks dirty), `insert_workload` |
| Deleted rows | `remove_workload`, `remove_service` |
| Cluster and scaling events | `event`, `scaling_event` |
| Heartbeat times to flush | `flush_heartbeats` |
| Commands to send | `queue_start`, `queue_stop` |
| Replies and other callbacks | `after_commit` |

When the handler returns, `finish()` writes everything in one transaction with `synchronous = FULL`, so the commit is fsync'd. Only after the commit succeeds does it send the RegisterAck and commands to agents, run reply callbacks, and broadcast a change notification for the web UI. So:

- an acknowledged `Apply`, `Scale`, `Delete` or drain is on disk (NFR-06);
- an agent is never told to start a workload the database has not recorded;
- a failed commit is fatal: the controller logs it and exits, and systemd restarts it from the last good state.

An `Apply` that changes nothing (AT-01) produces an empty batch: no write, no command, no event.

## Timers

| Timer | Interval | Does |
|---|---|---|
| Reconcile | `reconcile_interval_seconds` (5 s) | Re-send unacknowledged commands, then a full reconcile pass |
| Health | 1 s | Mark silent nodes UNHEALTHY (skipped during the grace period) |
| Heartbeat flush | `heartbeat_flush_interval_seconds` (30 s) | Write `last_heartbeat_at` for healthy nodes |
| Autoscaler | `autoscaler.evaluation_interval_seconds` (10 s) | Evaluate every autoscaled service, then reconcile |
| Chart sample | 60 s | Append one point per service and per node class |
| Prune | 10 min | Delete old terminal workloads and events. See [state_store.md](state_store.md#retention) |

A reconcile pass also runs after every message that may change desired or actual state, so `apply` → `StartWorkload` normally takes milliseconds rather than waiting for the tick (NFR-05).

## Dispatcher

Commands are tracked per workload in `outstanding`, with the command id, the target node, the message, when it was sent and whether it was already re-sent (FR-CON-09):

| Situation | Action |
|---|---|
| The node has an open stream | Sent right after the commit |
| The node has no open stream | Kept, and sent when the node registers again |
| The stream's buffer is full | Kept; retried on the next tick |
| No `CommandResult` within `command_timeout_seconds` (30 s) | Re-sent once |
| Still nothing after the second timeout | Dropped. The reconciler re-derives it: STOPPING workloads get a new stop, and ASSIGNED ones are covered by the start timeout |
| A newer command for the same workload | Replaces the older one; an ack for the old command id is ignored |
| The node goes UNHEALTHY | Its commands are dropped; its workloads go LOST |
| The node registers again | Every command still outstanding for it is re-sent |

Command ids are `<unix ms>-<sequence>`. A `CommandResult` only resolves the command whose id it names.

A rejected `StartWorkload` (for example `RUNTIME_UNAVAILABLE`, when dockerd is down) unbinds the workload and returns it to PENDING, so the scheduler places it again, possibly elsewhere. A rejected stop is re-issued by the reconciler.

## Agent Streams

Each stream has an outbound channel of 256 messages. Only the core holds its sender: dropping it ends the stream, which is how the controller forces an agent to reconnect and register again (for example when heartbeats arrive from a node already marked UNHEALTHY). Messages from a superseded stream, identified by `conn_id`, are ignored. See [agent_stream.md](agent_stream.md).

## Change Notifications

After every commit that wrote something, the core increments a sequence number and sends it on a `tokio::sync::broadcast` channel of 64 slots. Each web UI client subscribes through `CoreHandle::subscribe`. A client that falls behind is disconnected and reconnects; the core never waits for it (FR-UI-09).
