# Component: Autoscaler

## High-Level Function

The autoscaler changes a service's `target_replicas` from its measured CPU utilisation (FR-AUT). It only works within existing capacity: adding nodes is the cloud ASG's job, driven by the controller's metrics (spec §10.4). The code is in `crates/valkvork-controller/src/core/autoscale.rs`.

A service opts in with an `[autoscale]` table:

```toml
[autoscale]
min_replicas       = 3
max_replicas       = 10
target_cpu_percent = 75
```

## The Metric

Agents report each container's CPU usage in millicores, averaged since the previous heartbeat, read from cgroup v2 `cpu.stat` (see [agent.md](agent.md#telemetry)). The core keeps each RUNNING workload's samples for the last `autoscaler.window_seconds` (60 s).

A service's **utilisation** is the mean, over its eligible workloads, of `average CPU over the window ÷ cpu limit × 100` (FR-AUT-01). Eligible means RUNNING, of the current version (outdated workloads in a rollout are excluded), and started at least 30 s ago (startup spikes are excluded). If fewer than half of the RUNNING workloads have samples, there is no reading, and missing data never causes a scale-down.

The same figure is shown as the service's CPU in the CLI, the UI and `valkvork_service_cpu_utilization`.

## Evaluation

Every `autoscaler.evaluation_interval_seconds` (10 s), for each autoscaled service (FR-AUT-02):

| Utilisation | Sustained for `sustain_seconds` (120 s) at every evaluation | New target |
|---|---|---|
| above `target × 1.1` | scale up | `min(max, ceil(current × util / target))` |
| below `target × 0.9` | scale down | `max(min, ceil(current × util / target), ceil(current / 2))`: at most half the replicas per step |
| within ±10 % of target | dead zone, no action | |

Any evaluation inside the dead zone resets the sustain timer. Worked example from the spec: 3 replicas, target 75 %, utilisation 91 % for 2 minutes → `ceil(3 × 91 / 75) = 4`.

## Guards

No action is taken (FR-AUT-03, FR-AUT-04):

- during `cooldown_seconds` (180 s) after the service's last scaling action;
- while a rollout is in progress, or the service is being deleted;
- during the controller's recovery grace period;
- without a reading (see above).

Scale-up is also skipped while the service has unplaced PENDING workloads. More replicas would not help, and the shortage is an infrastructure signal instead.

## Effect and Audit

An action sets `target_replicas` and `last_scaled_at`, writes a `scaling_events` row (from, to, the utilisation, and a reason such as `cpu 91.0% > 82.5% for 120s`) and a `SCALE` cluster event with actor `autoscaler`, and increments `valkvork_scaling_actions_total{service,direction}` (FR-AUT-05). The new count takes effect through the normal reconcile pass.

## Interaction with Apply and Manual Scale

With `[autoscale]` present, `replicas` in the manifest is used only when the service is created. Re-applying updates min, max and target and clamps the current count into `[min, max]`, but never resets it to `replicas`, so CI applies do not undo autoscaling (FR-MAN-04). A manual `valkvork scale` is clamped to `[min, max]` the same way, and the autoscaler continues from it.

## Configuration

```toml
# controller.toml (defaults)
[autoscaler]
enabled                     = true
evaluation_interval_seconds = 10
window_seconds              = 60
sustain_seconds             = 120
cooldown_seconds            = 180
```

## Requirements

Per-container CPU comes from cgroup v2 files on the worker. With the agent on macOS against a Docker VM, those files are not visible, CPU reads 0 % and the autoscaler never acts. On a Linux worker it works.
