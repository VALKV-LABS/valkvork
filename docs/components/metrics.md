# Component: Metrics

## High-Level Function

Both daemons serve Prometheus metrics in the text exposition format at `GET /metrics` (NFR-07). Logs are JSON lines from `tracing`, controlled by `RUST_LOG` (default `info`).

| Process | Setting | Default | Code |
|---|---|---|---|
| Controller | `metrics_listen_addr` in `controller.toml` | `0.0.0.0:9400` | `controller/src/http.rs`, `core/observe.rs` |
| Agent | `metrics_listen_addr` in `agent.toml` | `127.0.0.1:9401` | `agent/src/metrics.rs` |

Remove the setting to disable an endpoint. The agent's address must be loopback; the agent refuses to start otherwise, because it opens no other listener (FR-ARC-01). Neither endpoint requires authentication, so restrict the controller's port to your monitoring network.

## Controller

The controller's values are computed from the core's in-memory state on each scrape, through the same message channel as every other request.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `valkvork_nodes` | gauge | `status`, `class` | Registered nodes |
| `valkvork_nodes_draining` | gauge | `class` | Draining nodes |
| `valkvork_workloads` | gauge | `status` | Workloads by status, every status listed even at 0 |
| `valkvork_workload_health` | gauge | `health` (`STARTING`, `HEALTHY`, `UNHEALTHY`) | Live workloads with a health check, by reported health |
| `valkvork_pending_unplaced_workloads` | gauge | `class` | PENDING workloads that could not be placed, by the service's required class (`any` when unconstrained); every class with nodes is listed |
| `valkvork_allocated_ratio` | gauge | `class`, `resource` (`cpu`, `memory`) | Requested over allocatable on healthy nodes |
| `valkvork_service_cpu_utilization` | gauge | `service` | The autoscaler's utilisation figure (%), when there is a reading |
| `valkvork_service_target_replicas` | gauge | `service` | Target replicas |
| `valkvork_scaling_actions_total` | counter | `service`, `direction` (`up`, `down`) | Autoscaler actions |
| `valkvork_schedule_duration_seconds` | histogram | | Time to place one workload |
| `valkvork_agent_registrations_total` | counter | | Accepted Register messages |
| `valkvork_agents_connected` | gauge | | Agents with an open stream |
| `valkvork_commands_outstanding` | gauge | | Commands not yet acknowledged |
| `valkvork_controller_recovery_grace` | gauge | | 1 during the recovery grace period |
| `valkvork_controller_uptime_seconds` | gauge | | Since start |
| `valkvork_db_size_bytes` | gauge | | SQLite file plus WAL |

Counters reset when the controller restarts; Prometheus' `rate()` handles that.

## Agent

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `valkvork_agent_reconnects_total` | counter | | Times the controller stream was re-established after a failure |
| `valkvork_agent_connected` | gauge | | 1 while registered |
| `valkvork_agent_heartbeats_total` | counter | | Heartbeats sent |
| `valkvork_agent_commands_total` | counter | `command` (`start`, `stop`), `result` (`accepted`, `rejected`) | Commands received |
| `valkvork_agent_local_restarts_total` | counter | | In-place restarts under the restart policy |
| `valkvork_agent_workloads` | gauge | `state` (`starting`, `running`, `restarting`, `stopped`, `failed`) | Managed containers |

To collect agent metrics, scrape them on the worker with a local collector (for example the Prometheus agent or the CloudWatch agent), since the port is not reachable from outside.

## Using Them for Infrastructure Scaling

The spec's second autoscaling loop (§10.4) scales each node class's ASG from the controller's metrics, not host CPU, because placement is by reservation:

- scale out when `valkvork_pending_unplaced_workloads{class} > 0` for 1 minute, or `valkvork_allocated_ratio{class}` stays above 0.8;
- scale in when `valkvork_allocated_ratio{class}` stays below 0.4 for 15 minutes, draining nodes through an ASG lifecycle hook first.

Publishing these to CloudWatch and wiring the hooks is deferred until needed; see [phase-2.md](../phase-2.md#infrastructure-autoscaling-from-controller-metrics).

## Example Alerts

```yaml
groups:
  - name: valkvork
    rules:
      - alert: ValkvorkNodeUnhealthy
        expr: sum(valkvork_nodes{status="UNHEALTHY"}) > 0
        for: 1m
      - alert: ValkvorkUnplacedWorkloads
        expr: sum by (class) (valkvork_pending_unplaced_workloads) > 0
        for: 5m
      # FAILED and STOPPED rows are history (kept 24 h), so alert on
      # commands that agents never acknowledge instead.
      - alert: ValkvorkCommandsStuck
        expr: valkvork_commands_outstanding > 0
        for: 5m
      - alert: ValkvorkAgentsDisconnected
        expr: valkvork_agents_connected < sum(valkvork_nodes{status="HEALTHY"})
        for: 2m
```
