# Component: Node Health and Reconnect Rules

## High-Level Function

This component decides when a node is gone, replaces its workloads, and reconciles what a node reports when it comes back. It guarantees that a healed partition never leaves more replicas than desired, and that a controller restart never stops a healthy workload. The code is in `crates/valkvork-controller/src/core/agents.rs`.

## Liveness

Every message from an agent (Register, Heartbeat, CommandResult) sets the node's `last_seen` to the **controller's** clock (FR-ARC-02). Agent timestamps are never used.

Once a second the core checks every HEALTHY node. One with no message for `node_unhealthy_timeout_seconds` (10 s, five missed heartbeats) becomes UNHEALTHY (FR-CON-02). A broken stream does not shorten the timer: a node whose stream dropped has the full 10 s to reconnect.

When a node becomes UNHEALTHY:

1. its `last_heartbeat_at` is written, and its stream is closed;
2. its ASSIGNED, RUNNING and STOPPING workloads become LOST;
3. each LOST workload with desired state RUN gets one replacement: PENDING, current version, `replaces_workload_id` set;
4. outstanding commands for the node are dropped;
5. a NODE event records how many workloads were lost and replaced.

The reconciler then places the replacements on other nodes (spec AT-06: dispatched within 15 s of the failure).

A node returns to HEALTHY only by registering again. If heartbeats arrive on a stream from a node already marked UNHEALTHY (a slow link that recovered), the controller closes that stream, so the agent reconnects and registers with its full inventory.

## Reconnect Rules (FR-CON-03)

When an agent registers, the controller first answers `RegisterAck`, then compares the agent's inventory, every Valkvork container on the host, with the workload table:

| Reported container | Action |
|---|---|
| Workload ASSIGNED or RUNNING on this node | Update its status from the report. No command |
| Workload LOST, its replacement still PENDING | **Re-adopt**: the lost workload becomes RUNNING again and the pending replacement is deleted |
| Workload LOST, its replacement ASSIGNED or RUNNING | Send `StopWorkload`; the old copy goes STOPPING, then STOPPED |
| Workload LOST, no replacement exists | Re-adopt it |
| Desired state STOP, terminal, or recorded on another node | Send `StopWorkload` (again) |
| No workload row at all (orphan) | Send `StopWorkload`, except during the recovery grace period |

It also checks rows that say they are on this node but that the agent did not report:

| Row | Action |
|---|---|
| ASSIGNED | The start was lost (agent restart, partition): send `StartWorkload` again. Commands are idempotent |
| RUNNING | The container vanished: FAILED, `CONTAINER_MISSING` |
| STOPPING or LOST | Gone: STOPPED |

Finally every command still outstanding for the node is re-sent.

The same checks run on every heartbeat, so an orphan created while the node was connected is stopped too (after the grace period), and a container removed by the agent confirms its STOPPING workload as STOPPED.

Together these rules mean a partition that heals converges on exactly the target count, and a workload pinned to a host volume on the partitioned node is kept rather than churned.

## Controller Restart Recovery

On startup the controller loads desired state from SQLite and enters a **recovery grace period** of `recovery_grace_period_seconds` (30 s) (FR-CON-04). The grace period is longer than the agents' maximum reconnect backoff (15 s, +20 % jitter), so every live agent reconnects within it.

| During the grace period | |
|---|---|
| Every node's in-memory `last_seen` | Set to the controller's start time |
| Marking nodes UNHEALTHY | Skipped |
| Creating replacements | None, since no node goes UNHEALTHY |
| Stopping orphans | Skipped |
| Autoscaler | Skipped |
| Everything else | Normal: agents register, statuses update, applies work |

When it ends, normal reconciliation resumes. A controller restart never stops or restarts a healthy running workload (AT-03).

## The Agent's Side

During a partition the agent never stops, restarts or changes a container because the controller is unreachable (FR-AGE-05). It only reconnects, then sends `Register` with its full inventory and lets the controller decide. See [agent_stream.md](agent_stream.md#reconnects).
