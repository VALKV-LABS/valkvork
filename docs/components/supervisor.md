# Component: Supervisor

## High-Level Function

The supervisor owns the agent's view of its containers and carries out the controller's commands (FR-AGE-04). It also does the agent's only independent work: restarting crashed containers in place (FR-AGE-06), adopting containers after an agent restart (FR-AGE-07), and enforcing ephemeral disk limits (FR-AGE-08). It reaches Docker only through the [`ContainerRuntime`](container_runtime.md) trait. The code is in `crates/valkvork-agent/src/supervisor.rs`.

## State

One entry per workload, keyed by workload id:

| Field | Meaning |
|---|---|
| service id, service version | From the command or the container's labels |
| container id | Once created |
| state | `STARTING`, `RUNNING`, `RESTARTING`, `STOPPED` or `FAILED` |
| reason, exit code | Why it failed or last exited |
| restart count, recent restarts | For the restart policy |
| restart policy, kill timeout, disk limit | From the command or labels |
| stopping | A stop was requested; exits are expected and never restarted |
| CPU, memory, disk | Latest telemetry |

`report()` turns these into the `WorkloadStatus` list sent in every heartbeat and in `Register`.

## Starting a Workload

```
StartWorkload
  known workload id?           → ack, nothing to do
  Docker answers within 3 s?   → no: reject RUNTIME_UNAVAILABLE
  record STARTING, ack, then in the background:
    1. create <data_dir>/alloc/<workload_id>/
    2. resolve volumes: each source must be a [host_volumes] entry in agent.toml,
       and must not be a protected path            → else FAILED HOST_VOLUME_NOT_FOUND / FORBIDDEN_MOUNT
    3. pull the image with registry credentials    → else FAILED IMAGE_PULL_FAILED
    4. create vk-<workload_id>                     → else FAILED CREATE_FAILED
    5. start it                                    → else FAILED START_FAILED
    6. RUNNING
```

The container gets:

| Setting | Value |
|---|---|
| Name | `vk-<workload_id>` (unique, so a repeated create finds the existing one) |
| CPU | `NanoCpus = millicores × 1,000,000` |
| Memory | `Memory = MemorySwap = memory_bytes`: no swap |
| Environment | The manifest's `[env]`, plus `VALKVORK_WORKLOAD_ID` and `VALKVORK_SERVICE_ID` |
| Ports | `PortBindings` for each `[[ports]]` entry, TCP |
| Mounts | `/alloc` → the alloc directory, plus one bind mount per volume |
| Restart policy | `no`: the agent owns restarts |
| Privileged | never |
| Labels | `valkvork.workload_id`, `valkvork.service_id`, `valkvork.service_version`, `valkvork.restart` (`attempts/interval/delay`), `valkvork.kill_timeout`, `valkvork.disk_bytes` |

The labels carry everything the agent needs to manage the container after it restarts, so no local database is needed.

A host volume is mounted read-only if either the manifest or `agent.toml` says so.

## Stopping a Workload

```
StopWorkload
  mark stopping, ack, then: stop with the kill timeout (SIGTERM, wait, SIGKILL),
  remove the container, delete its alloc directory and failed marker, forget it
```

A stop that arrives while the start is still pulling is finished as soon as the start returns. If removing the container fails, the entry is kept, so the controller keeps seeing it and asks again.

## Restart Policy

The agent learns about exits from Docker's event stream (`die` and `oom` events, filtered by label), not by polling. When a RUNNING, non-stopping container exits:

```
attempts used in the last `interval` < `attempts`?
  yes → RESTARTING, wait `delay`, start the same container again → RUNNING
  no  → FAILED with reason OOM_KILLED (after an oom event) or EXITED; leave the container stopped
```

The default policy is 3 attempts within 5 minutes, 5 s apart. An exit with code 0 counts as a failure too, since services are meant to run forever. This works with the controller unreachable. Replacing a FAILED workload is the controller's decision; it then asks the agent to remove the container.

A failed workload also gets a marker file, `<data_dir>/failed/<workload_id>`, holding the reason.

## Adoption

At startup, and whenever a resync finds a labelled container it does not know, the agent adopts existing containers instead of restarting them (FR-AGE-07, AT-04):

| Container state | Becomes |
|---|---|
| running or restarting | RUNNING, untouched |
| exited, with a failed marker | FAILED, with the marker's reason, untouched |
| exited, no marker (it died while the agent was down) | handled as an exit under the restart policy |
| created, never started (the agent died mid-start) | started |

Adopting labelled containers it did not create keeps the inventory complete, so the controller sees and stops orphans.

## Safety Net

The event stream is the primary signal. Every 10 s, and each time the stream reconnects (for example after dockerd restarts), a resync compares RUNNING entries with Docker: an exited container is handled as an exit, and one that no longer exists becomes FAILED with `CONTAINER_MISSING`.

## Ephemeral Disk

Each workload's disk use is the size of its alloc directory, measured every 10 s, plus its writable layer (`SizeRw`), measured every 60 s because the call is expensive. When the total exceeds the manifest's `disk` (default 1 GiB), the workload becomes FAILED with `EPHEMERAL_DISK_EXCEEDED` and its container is stopped. The restart policy does not apply (FR-AGE-08).

This is a polling check, so there is a soft margin. Container logs live under Docker's data root and are bounded by Docker's log rotation, not by `disk`.

## Protected Paths

The Docker socket gives root on the host, so no workload may mount it (FR-SEC-06). A volume is refused with `FORBIDDEN_MOUNT` when its host path, as configured or after resolving symlinks, contains or lies inside any of:

- the configured `docker_socket`;
- `/var/run/docker.sock`, `/run/docker.sock`, `/var/lib/docker`, `/run/containerd`, `/var/lib/containerd`;
- Docker's actual data root, as Docker reports it (for example `/data/docker` on the golden image).

So `/`, `/var/run`, `/data/docker/volumes` and a symlink to the socket are all refused.
