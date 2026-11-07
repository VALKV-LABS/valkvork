# Component: Reconciler

## High-Level Function

The reconciler compares desired state (each service's target replicas and version) with actual state (workload rows, updated from agent reports) and emits work: new PENDING workloads, placements, and stops (FR-CON-05). It runs inside the core after every state change and on a 5 s tick. The code is in `crates/valkvork-controller/src/core/reconcile.rs`.

## Workload Lifecycle

Each workload has a **desired state** (`RUN` or `STOP`, the controller's intent) and a **status** (the last known fact):

| Status | Meaning | Terminal |
|---|---|---|
| `PENDING` | Created, not placed. `status_reason` says why, e.g. `NO_NODE: insufficient memory on 2` | no |
| `ASSIGNED` | Bound to a node; `StartWorkload` sent | no |
| `RUNNING` | The agent reports the container running | no |
| `STOPPING` | `StopWorkload` sent | no |
| `LOST` | Its node went UNHEALTHY while ASSIGNED, RUNNING or STOPPING. Becomes STOPPED when the node reconnects without it, its service is deleted, or the node is removed | no |
| `STOPPED` | Stopped on request | yes |
| `FAILED` | Failed for good: restarts exhausted, pull failed, start timeout, disk limit, container missing | yes |

A workload is **live** when its desired state is RUN and its status is not STOPPED, FAILED or LOST.

## One Pass

```
reconcile()
  1. start timeouts:  ASSIGNED longer than start_timeout (5 min) → FAILED START_TIMEOUT
  2. stuck stops:     STOPPING, no outstanding command, node connected → StopWorkload again
  3. for each service:
       DELETING → stop every workload; remove the service once all are terminal
       otherwise → migrations (rolling update, drain), then replica count
  4. schedule every PENDING workload (desired RUN), oldest first
```

A pass is cheap at the spec's scale (50 nodes, 500 workloads), so it runs after every message rather than tracking what changed.

## Replica Count

The reconciler counts **slots**: live workloads that are not an in-flight replacement for another live workload. During a rollout or drain, a service briefly runs `target + n` containers while the count stays at `target`.

| Condition | Action |
|---|---|
| slots < target | Insert PENDING workloads for the difference, unless the failure backoff is active |
| slots > target | Stop the excess, choosing PENDING first, then ASSIGNED, then RUNNING on the most-allocated node, newest first |

Stopping a PENDING workload marks it STOPPED at once; it never reached a node. Stopping an ASSIGNED or RUNNING one sets it STOPPING and sends `StopWorkload`.

## Rolling Updates and Drains

A **replacing** apply bumps the service's `version` (see [manifests.md](manifests.md#change-classification)). A workload whose `service_version` is lower is **outdated**. A workload on a node with `draining = true` must also move. Both use the same mechanism (FR-CON-07, FR-CON-08):

1. For a workload that must move and has no replacement yet, create one: PENDING, current version, `replaces_workload_id` set to the old workload. At most `update.max_parallel` replacements are in flight at once.
2. The scheduler places the replacement like any other workload. A draining node is never chosen.
3. Once the replacement has been RUNNING for `update.min_healthy_time`, stop the old workload (`status_reason` ROLLED_OUT or DRAINED).
4. When no live workload is outdated, the rollout status returns to NONE and a `DEPLOY` event records "rollout complete".

The RUNNING count never drops below the target (AT-09).

### Stop-First for Single-Writer Volumes

A service that mounts a **single-writer** host volume (any writable host volume not declared `single_writer = false`, see [placement-and-storage.md](../placement-and-storage.md#single-writer-and-shared-volumes)) cannot run its old and new workload side by side. Its updates and drains run in the opposite order:

1. Pick the oldest workload that must move, if fewer than `update.max_parallel` of the service's live workloads are still coming up (not yet RUNNING for `min_healthy_time`).
2. Check that its replacement could be placed once the old workload is gone. If not, for example a drain where no other node declares the volume, leave it RUNNING and record one `WORKLOAD` event saying why.
3. Stop the old workload (`ROLLED_OUT` or `DRAINED`) and create its replacement at once. The replacement stays PENDING (`NO_NODE: single-writer host volume pgdata in use on 1`) until the old container is gone, then is placed, usually on the same node.
4. The rollout stays IN_PROGRESS until the last replacement is healthy.

Each replica is down between its old container stopping and its new one starting. Halting works as above, except that the replica whose new version failed has no old copy to fall back to: it is retried under the failure backoff until the service is fixed, for example by applying the previous manifest again.

**Halting.** If a workload of the current version fails during a rollout, `rollout_status` becomes HALTED. No more outdated workloads are replaced, and the old ones keep running. The next apply that changes anything resets the halt and the rollout resumes or is superseded. There is no automatic rollback.

**Cancelling.** If a workload no longer needs to move (its node was undrained), its in-flight replacement is stopped (`MIGRATION_CANCELLED`), so the service does not keep an extra replica. Stop-first moves have nothing to cancel: the old workload is already stopping.

**Drain completion.** A drain is done when the node has no non-terminal workloads. A workload that cannot be placed elsewhere, for example one that needs a host volume only that node has, stays where it is, and its replacement stays PENDING with a `NO_NODE` reason. For a stop-first service no replacement is created; a `WORKLOAD` event explains why the workload was not moved.

## Failures and Backoff

When a workload of the service's current version fails (FR-CON-05):

- the service's failure streak goes up by one;
- new replacements wait 5 s, 10 s, 20 s … doubling up to 5 minutes after the last failure;
- after 5 failures in a row, the service is marked DEGRADED and an event is emitted;
- a rollout in progress halts.

A workload of the current version reaching RUNNING clears the streak and returns a DEGRADED service to ACTIVE. An apply that changes anything also clears it.

When a workload fails, the controller also sends `StopWorkload`, so the agent removes the stopped container. Agents leave failed containers in place for the controller to decide.

## Deletes

`delete service <id>` marks the service DELETING (FR-CON-10). Each pass then:

- stops every non-terminal workload (PENDING ones go straight to STOPPED);
- marks LOST workloads STOPPED: their node is gone, and if the copy comes back it is an orphan and gets stopped then;
- once every workload is terminal, deletes the service row, which cascades to its workload history, and records a `DEPLOY` event.

## Events

The reconciler and its callers record `cluster_events` for creates and version changes (DEPLOY), every operator action (AUDIT, actor `cli` or `ui:<id>`), rollout completion and halts (DEPLOY), failures and DEGRADED (WORKLOAD), and node health changes (NODE).
