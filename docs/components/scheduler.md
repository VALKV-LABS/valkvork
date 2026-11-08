# Component: Scheduler

## High-Level Function

The scheduler picks a node for each PENDING workload (FR-CON-06). It spreads workloads across nodes, the opposite of bin-packing, so losing one node costs as few replicas as possible. It is a set of pure functions over a capacity snapshot, in `crates/valkvork-controller/src/scheduler.rs`, which makes it easy to test in isolation.

Placement uses **requested** resources from the manifest (the requests are also the limits in this release), never observed usage.

## Capacity Snapshot

For each pass, the core builds a `Snapshot` with one `NodeCapacity` per node:

| Field | From |
|---|---|
| Allocatable CPU (millicores) and memory | The node's Register message: total minus `[reserved]` in `agent.toml` |
| Allocated CPU and memory | Sum of requests of live workloads bound to the node (desired RUN, not terminal, not LOST) |
| Used host ports | Host ports of every non-terminal, non-LOST workload on the node, including STOPPING ones, because the port stays bound until the container is gone |
| Host volumes | Names the agent advertised |
| Host volume users | For each volume, how many non-terminal, non-LOST workloads on the node mount it, and how many of those mounts are single-writer. STOPPING ones count, because the directory is in use until the container is gone |
| Workloads per service | For the tie-break |
| Healthy, draining | Node status and flag |
| Agent features | The manifest features the node's agent listed in its Register (`command`, `health_checks`, `host_ip`, `secrets`) |

Each placement calls `Snapshot::reserve`, so placements within one pass account for each other.

## Filter

A node is eligible only if all of these hold:

1. `status = HEALTHY` and not draining;
2. its `node_class` equals the manifest's `required_class`, if one is set;
3. its agent supports every manifest feature the spec uses (`command`/`entrypoint`, `[check]`, a port's `host_ip`, `[secrets]`). Protobuf ignores fields a receiver does not know, so an older agent would otherwise start the container without them; the workload waits instead, with `agent lacks secrets (upgrade it) on 1`;
4. free CPU and free memory cover the request;
5. none of the manifest's static `host_port`s is in use on it;
6. it advertises every host volume the manifest mounts;
7. no single-writer volume the manifest mounts has another user on the node, and no shared volume it mounts has a single-writer user.

## Score

Among eligible nodes, choose the lowest `max(cpu_ratio, memory_ratio)` **after** adding the workload, where each ratio is allocated over allocatable. Ties go to the node with the fewest workloads of the same service, then to the lowest `node_id`, so placement is deterministic.

## When Nothing Fits

The workload stays PENDING, and `status_reason` explains why, counted over all nodes:

```
NO_NODE: no nodes registered
NO_NODE: no healthy, non-draining nodes
NO_NODE: no healthy nodes in class gpu
NO_NODE: insufficient memory on 2, host_port 8080 in use on 1 in class general
NO_NODE: host volume pgdata missing on 3
NO_NODE: host volume pgdata missing on 2, single-writer host volume pgdata in use on 1
```

It is retried on every pass, so it is placed as soon as capacity appears. Unplaced workloads are counted in `valkvork_pending_unplaced_workloads{class}`, the signal for adding nodes (spec §10.4).

## Pinning and Volumes in Practice

Node class is the only placement label, and host volumes pin a service to the nodes that declare them. [placement-and-storage.md](../placement-and-storage.md) shows how to pin to one VM, and what happens to a host-volume service when its node fails.

## Static Host Ports

Workloads use Docker's bridge network with static host-port mappings (FR-NET-01). A node can run only one workload per host port, so a service with a `host_port` can have at most one replica per eligible node. `plan` reports this as `placement: 1/3 workloads placeable` before you apply.

## Feasibility for `plan` and Warnings

`plan` runs the same `place` against a copy of the snapshot, for as many workloads as the change needs, and reports how many fit. Apply and plan also warn with `UNSATISFIABLE_NOW` (VW-01) when no current healthy node of the class could fit even one replica, or none has the required host volumes. It is a warning: the service is created and waits for capacity.

## Performance

A placement is a linear scan over nodes. `valkvork_schedule_duration_seconds` records each one; at 50 nodes it is well under the spec's 10 ms.
