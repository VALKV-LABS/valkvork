# Component: Agent (`valkvork-agent`)

## High-Level Function

The agent runs on every worker VM. It reports facts to the controller and carries out its commands; it decides nothing, apart from restarting a crashed container in place under the service's restart policy (spec §4). It opens one outbound connection to the controller and no listeners except an optional loopback metrics endpoint (FR-ARC-01). The code is in `crates/valkvork-agent/src/`.

| Module | Doc | Does |
|---|---|---|
| `connection.rs` | [agent_stream.md](agent_stream.md) | The stream to the controller, heartbeats, commands, reconnects |
| `supervisor.rs` | [supervisor.md](supervisor.md) | Local workload lifecycle |
| `runtime.rs` | [container_runtime.md](container_runtime.md) | The `ContainerRuntime` trait and `DockerRuntime` |
| `identity.rs` | this page | Node id, hostname, address |
| `telemetry.rs` | this page | cgroup v2 and `/proc` readers |
| `metrics.rs` | [metrics.md](metrics.md#agent) | Prometheus endpoint |

## Startup

```
valkvork-agent --config /etc/valkvork/agent.toml
  1. load agent.toml (unknown keys rejected) and the join token file
  2. load or create node_id
  3. connect to Docker; retry with backoff until it answers (dockerd may still be starting)
  4. read the host's CPUs and memory from Docker, subtract [reserved]
  5. adopt every container labelled valkvork.workload_id (FR-AGE-07)
  6. start the event watcher, the 10 s resync, disk checks and the metrics endpoint
  7. connect to the controller and register with the full inventory
```

`--check-config` validates the file, the token file and the CA bundle, then exits. The golden image's bootstrap runs it before replacing a config. Stopping the agent (SIGTERM, Ctrl-C) leaves every container running (AT-04).

## Identity

`node_id` is a UUIDv5 of `/etc/machine-id` in a fixed namespace, written to `<data_dir>/node_id` on first boot and read from there afterwards (FR-AGE-02). So it is stable across agent restarts, and distinct for every VM cloned from an image whose build truncated `/etc/machine-id` (FR-AGE-01, AT-14). Without a machine id (macOS, some containers), the agent uses a random UUID and logs a warning; that is for development only.

| Reported | From |
|---|---|
| `hostname` | `/proc/sys/kernel/hostname`, `/etc/hostname`, `$HOSTNAME` or `hostname` |
| `ip_address` | `advertise_ip` if set, else the local address the kernel would route to the controller (a UDP "connect", which sends nothing) |
| `node_class` | `node_class` in `agent.toml` |
| `host_volumes` | The names under `[host_volumes]` |
| `agent_version` | The crate version |
| `runtime` | `docker/<engine version>` |

## Capacity

Allocatable capacity is what Docker reports for the host, minus `[reserved]`:

```
allocatable_cpu_millicores = Docker NCPU × 1000 − ceil(reserved.cpu × 1000)
allocatable_memory_bytes   = Docker MemTotal − reserved.memory
```

`[reserved]` (default 0.25 cores, 384 MiB) keeps room for the OS, dockerd, containerd and the agent. The golden image sets it per instance from GKE's node reservation tiers instead (see [deployment.md](../deployment.md#golden-image)). The scheduler places workloads by their requests against this figure. Reading it from Docker means a Docker VM (Colima, Docker Desktop) reports the VM's resources, which is where the containers actually run.

## Telemetry

Every heartbeat (default 2 s), the agent reports node CPU and memory and, for each managed container, state, restart count, CPU, memory and disk use (FR-AGE-03):

| Value | Source |
|---|---|
| Node CPU % | `/proc/stat`, busy time between two samples |
| Node memory used | `/proc/meminfo`: `MemTotal − MemAvailable` |
| Container CPU (millicores) | cgroup v2 `cpu.stat` `usage_usec`, difference since the previous heartbeat divided by elapsed time |
| Container memory | cgroup v2 `memory.current` |
| Container disk | The alloc directory size plus the writable layer size; see [supervisor.md](supervisor.md#ephemeral-disk) |

cgroup files are read directly, never through the Docker stats API, which is too slow to call for every container every 2 s. The runtime driver gives the candidate paths: `/sys/fs/cgroup/system.slice/docker-<id>.scope` (systemd cgroup driver) and `/sys/fs/cgroup/docker/<id>` (cgroupfs). When neither exists, as on macOS where the files are inside the Docker VM, CPU and memory are reported as 0.

## Files

| Path | Holds |
|---|---|
| `/etc/valkvork/agent.toml` | Configuration ([configuration.md](configuration.md#agent)) |
| join token file, CA bundle | From the secrets store, at boot |
| `/etc/valkvork/registry-auth.json` | Optional registry credentials, Docker `config.json` format |
| `<data_dir>/node_id` | Node identity |
| `<data_dir>/alloc/<workload_id>/` | Each workload's ephemeral disk, mounted at `/alloc`; mode `1777`, so any container user can write to it |
| `<data_dir>/failed/<workload_id>` | Marker for a container that failed for good, so adoption does not restart it |

## Running It

The agent runs as root, because it drives the Docker socket and performs every bind mount for workloads (FR-STO-03). In production it runs under systemd with `Requires=docker.service`, so a dockerd restart also restarts the agent, which re-adopts its containers. See [deployment.md](../deployment.md).
