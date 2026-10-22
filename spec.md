# Valkvork — Product & Technical Specification

| | |
| :--- | :--- |
| **Version** | 1.0 (MVP), draft |
| **Date** | 2026-10-21 |
| **Supersedes** | v0 (`spec.v0.md`: "Aero" FRD + Manifest Pipeline + Deployment Blueprint + Storage FRS) |
| **Status** | Draft. Open decisions are listed in §16 |

**Conventions.** **MUST**, **SHOULD** and **MAY** follow RFC 2119. Every requirement has a stable ID (`FR-AGE-03`, `NFR-04`, …). Tests and code reference these IDs, and they are never renumbered or reused. Each requirement is tagged with a phase:

* **[MVP]**: required for the first production release.
* **[P2]**: next phase, designed for now (schema and proto leave room for it) but not built yet.
* **[P3]**: research or long-term. Not committed.

---

## 1. Overview

### 1.1 What Valkvork is
Valkvork is a lightweight workload orchestrator and autoscaler written in Rust. It follows the model of HashiCorp Nomad, with one deliberate simplification: **one control node backed by embedded SQLite, with no Raft or consensus.** The control node's availability comes from cloud primitives (a single-instance auto-scaling group, a persistent disk, and continuous backup to object storage), not from clustering.

### 1.2 Design principles
1. **The data plane outlives the control plane.** Losing the controller, or losing the network to it, never stops a running workload. Agents keep everything running and wait.
2. **One brain.** Only the controller makes placement, scaling and replacement decisions. Agents carry out commands and report facts. The single exception is local restart-on-failure, delegated by policy (FR-AGE-06).
3. **Desired state lives in the controller; actual state comes from agents.** The controller never assumes a workload is running until an agent reports it running.
4. **Declarative and idempotent.** Applying the same manifest twice is a no-op.
5. **Immutable workers.** Worker VMs come from Packer images and are replaced, never patched.
6. **Small and dependency-free.** Static musl binaries, `rustls`, no glibc or OpenSSL on hosts.

### 1.3 MVP goals
* Run long-lived containerised **services** across a fleet of up to 50 worker VMs.
* Declarative TOML manifests with `apply`, `plan` and `delete`.
* Spread scheduling with resource and node-class filtering.
* Automatic replacement of workloads on failed nodes.
* Rolling updates on spec change.
* CPU-based horizontal autoscaling with cooldown.
* Basic storage: ephemeral disk limits and node-local host volumes.
* A built-in web UI served by the controller binary itself, like Nomad's `/ui`, with no separate deployment and no third-party code (FR-UI).
* Production deployment on AWS (GCP is equivalent and documented where it differs).

### 1.4 Non-goals for MVP
* Multi-controller HA or distributed consensus.
* Affinity/anti-affinity, taints/tolerations, spread across failure domains. *Node-class filtering is the only placement constraint.*
* Overlay networking or service mesh. Workloads use host ports (§8).
* Built-in service discovery or DNS. *The load balancer integration in §8 covers MVP traffic.*
* Batch and system job types (Nomad `batch` / `system`) [P2].
* Application health checks (HTTP/TCP) [P2]. MVP liveness means "the container process is running".
* Secrets management [P2]. Env vars are stored in plaintext (FR-SEC-04).
* Network-attached volumes, I/O QoS, and storage pooling (§7 phases these).
* Container runtimes other than Docker Engine. *A native containerd driver is planned for [P2] (FR-AGE-10).*

### 1.5 Glossary and Nomad mapping

| Valkvork term | Nomad equivalent | Meaning |
| :--- | :--- | :--- |
| **Controller** (`valkvork-controller`) | Server | Single control node. Owns desired state, scheduling and autoscaling. |
| **Agent** (`valkvork-agent`) | Client | Daemon on every worker VM. Drives the container runtime (Docker Engine in MVP) and reports state. |
| **CLI** (`valkvork`) | `nomad` CLI | Operator tool that talks to the controller's Admin API. |
| **Manifest** | Job spec (HCL) | TOML file declaring one service. |
| **Service** | Job, `type = "service"`, one group, one task | Logical application with a replica count. |
| **Workload** | Allocation | One container instance of a service, bound to at most one node. |
| **Node class** | `node_class` | Label that the agent reports and manifests filter on. |
| **Reconcile pass** | Evaluation | Compares desired with actual state and emits work. |
| **Drain** | Node drain | Migrates workloads off a node before it is retired. |
| **Host volume** | `host_volume` | Named directory on a node that workloads can mount. |
| **Web UI** | Nomad UI (`/ui`) | Operator UI embedded in the controller binary (FR-UI). |

---

## 2. Architecture

### 2.1 Components

| Component | Runs on | Responsibilities |
| :--- | :--- | :--- |
| `valkvork-controller` | Control node (1 instance) | Admin API, **built-in web UI**, agent stream server, SQLite state, reconciler, scheduler, autoscaler, metrics. |
| `valkvork-agent` | Every worker VM | Outbound stream to the controller, telemetry, container lifecycle through the runtime driver (FR-AGE-09), local restarts, disk enforcement. |
| `valkvork` | Operator laptop or CI | `apply`, `plan`, `delete`, `scale`, `status`, `events`, `node list`, `node drain` / `undrain`. |
| Web UI | Operator's browser | Served by `valkvork-controller` at `/ui`. Nothing to install (FR-UI). |

### 2.2 The big picture: who talks to whom

Read it from top to bottom. Operators change *desired state*. The controller turns that into *commands*. Agents carry them out and report *facts* back. User traffic goes straight to the containers and never passes through Valkvork.

```
                              OPERATORS
      ┌─────────────────────┐            ┌─────────────────────┐
      │ valkvork CLI  /  CI │            │  Browser  (Web UI)  │
      └──────────┬──────────┘            └──────────┬──────────┘
                 │ [1] gRPC AdminService            │ [2] HTTPS /ui
                 │     apply · plan · scale · drain │     pages · actions · live view
                 └────────────────┬─────────────────┘
                                  ▼  :50052  (TLS + admin token or UI session)
   ┌────────────────────────────────────────────────────────────────────┐
   │ CONTROL PLANE · 1 VM · valkvork-controller                         │
   │ Decides what runs, where, and how many. Never runs app containers. │
   │                                                                    │
   │   SQLite (desired state) ────── [6] WAL stream ──────► S3 backup   │
   │                                                                    │
   └──────────────────────────────▲───────────────────┬─────────────────┘
                                  │                   │
     [4] agent → controller       │                   │   [3] controller → agent
     Register (on connect)        │  one gRPC stream  │   StartWorkload
     Heartbeat (every 2 s)        │    per agent,     │   StopWorkload
     CommandResult (acks)         │ opened by agent   │
                                  │  port :50051      │
   ┌──────────────────────────────┴───────────────────▼─────────────────┐
   │ DATA PLANE · N worker VMs · one valkvork-agent each                │
   │ Does what it is told; reports facts. Keeps apps up if cut off.     │
   │                                                                    │
   │   valkvork-agent ──► Docker API ──► dockerd ──► containerd         │
   │         ▲                                            │             │
   │         │ [7] reads CPU/memory from cgroup files     ▼             │
   │         └─────────────────────────────────── app containers        │
   │                                                     ▲              │
   └─────────────────────────────────────────────────────┼──────────────┘
                                                         │ [5] user traffic → host ports (e.g. :8080)
                                End users ──► Public LB ─┘   never passes through Valkvork
```

| # | From → To | What flows | When | Channel |
| :--- | :--- | :--- | :--- | :--- |
| 1 | CLI / CI → controller | Manifests, scale, drain, delete. Plan results come back. | On operator action | gRPC `AdminService`, :50052 |
| 2 | Browser → controller | Same actions as [1], plus a live view of the cluster | On page load, then SSE push | HTTPS `/ui`, :50052 |
| 3 | Controller → agent | `StartWorkload`, `StopWorkload` | Whenever desired ≠ actual | Agent's own stream, :50051 |
| 4 | Agent → controller | `Register` (identity, capacity, container inventory). `Heartbeat` (usage, per-container state and CPU). `CommandResult` (acks). | On connect; every 2 s; per command | Agent's own stream, :50051 |
| 5 | End users → containers | Application requests | Always | Public LB → worker host ports (§8.2) |
| 6 | Controller → S3 | SQLite WAL frames (backup) | Continuous | Litestream (§10.3) |
| 7 | Containers → agent | CPU and memory counters | Every 2 s | Local cgroup v2 files (FR-AGE-03) |

* **FR-ARC-01 [MVP]:** Agents MUST open connections **outbound only**. Agents expose no network listener apart from an optional `127.0.0.1` metrics endpoint. Commands travel down the agent's own stream, so the controller never initiates a connection to a worker.
* **FR-ARC-02 [MVP]:** The controller MUST use its own clock (time of receipt) for all liveness decisions and MUST NOT trust agent timestamps.
* **FR-ARC-03 [MVP]:** Desired state (services, target replicas, workload intent) lives only in SQLite on the controller. Actual state (which containers exist and their status) comes only from agent reports.
* **FR-ARC-04 [MVP]:** Application traffic MUST NOT pass through the controller or the agent. If both are down, apps keep serving (NFR-02).

### 2.3 Inside the controller: how a change becomes a command

Every arrow is an in-process channel between Tokio tasks. The **reconciler** is the heart of the loop. It runs on every state-change event and on a 5 s tick, compares desired state (SQLite) with actual state (the node table), and emits work. Scheduler decisions (`ASSIGNED`) are also committed through the writer task. That arrow is left out for clarity.

```
   ┌────────────────┐  validated spec  ┌──────────────┐  commit  ┌──────────┐
   │ Admin API + UI │─────────────────►│ Writer task  │─────────►│  SQLite  │
   └────────────────┘                  └───▲──────┬───┘          └──────────┘
                                           │      │ "state changed" event
   ┌────────────────┐  new replica count   │      ▼
   │   Autoscaler   │──────────────────────┘ ┌──────────────┐ PENDING  ┌────────────┐
   └───────▲────────┘                        │  Reconciler  │─────────►│ Scheduler  │
           │ per-service CPU (60 s avg)      └──────▲───────┘          └─────┬──────┘
           │                                        │ actual state           │ node chosen
   ┌───────┴────────────────────────────────────────┴───────┐          ┌─────▼──────┐
   │ Node table (in memory): liveness, usage, containers    │          │ Dispatcher │
   └───────────────────────────▲────────────────────────────┘          └─────┬──────┘
                               │ Register · Heartbeat · CommandResult        │ Start · Stop
   ════════════════════════════╧ agent streams (:50051) ═════════════════════▼═══════
```

### 2.4 Key flows, step by step

**Flow 1: an agent joins, then reports steadily.**
```
   Agent                                          Controller
     │                                              │
     │  boot: read agent.toml, load node_id         │
     │── TLS connect :50051 ───────────────────────►│
     │── Register ─────────────────────────────────►│  check join token
     │     identity, capacity, class, volumes,      │  upsert node → HEALTHY
     │     inventory of running containers          │  compare inventory (FR-CON-03)
     │◄────────────────────────────── RegisterAck ──│
     │                                              │
     │── Heartbeat ────────────────────────────────►│  update node table (in memory)
     │     node usage + per-container stats         │  feed autoscaler window
     │      … repeats every 2 s …                   │
     │                                              │
     │  ~~ stream breaks ~~                         │  10 s with no message → UNHEALTHY
     │    containers keep running (FR-AGE-05)       │  workloads → LOST, replaced elsewhere
     │    retry 1 s, 2 s, 4 s … max 15 s            │
     │── Register (full inventory) ────────────────►│  re-adopt or stop old copies (FR-CON-03)
```

**Flow 2: `valkvork apply` takes a service from 1 to 3 replicas.**
```
  Operator        Controller                             Agent          Docker
     │                │                                   │               │
     │── apply ──────►│ validate · diff → scale-only      │               │
     │                │ COMMIT target_replicas=3, fsync   │               │
     │◄── scale-only ─│                                   │               │
     │                │ reconcile: 1 live < 3 → 2 PENDING │               │
     │                │ schedule: node B → ASSIGNED       │               │
     │                │── StartWorkload ─────────────────►│               │
     │                │◄────────────────── CommandResult ─│── pull+create►│
     │                │                                   │── start ─────►│
     │                │◄───────────── Heartbeat: RUNNING ─│               │
     │                │ status → RUNNING                  │               │
```
Rolling updates (FR-CON-07) and drains (FR-CON-08) follow the same path, with one difference: each new workload must reach `RUNNING` before its predecessor gets `StopWorkload`.

**Flow 3: a worker dies or is cut off, then comes back.**
```
  Agent A         Controller                             Agent B
     │                │                                   │
     │ dies / cut off │                                   │
     │                │ 10 s, no message from A           │
     │                │ A → UNHEALTHY; its workloads LOST │
     │                │ create replacements (PENDING)     │
     │                │ schedule onto B → ASSIGNED        │
     │                │── StartWorkload ─────────────────►│
     │                │◄───────────── Heartbeat: RUNNING ─│
     │                │                                   │
     │ link restored  │                                   │
     │── Register ───►│ inventory still has old copy      │
     │                │ replacement RUNNING → stop old    │
     │◄─ StopWorkload │                                   │
     │                │ old copy → STOPPED; replicas exact│
```
If the replacement could not be placed (for example, the workload is pinned to a host volume on A), the old copy is **re-adopted** instead and the pending replacement is cancelled (FR-CON-03).

**Flow 4: the autoscaler adds a replica.**
```
  Agents          Controller (autoscaler, every 10 s)
     │                │
     │── Heartbeats ─►│ per-container CPU, every 2 s
     │                │ frontend-web: 60 s avg CPU = 91 %
     │                │ target 75 % → scale-up line = 82.5 % (×1.1)
     │                │ above the line for 120 s?  yes
     │                │ not in cooldown, no rollout, no unplaced PENDING
     │                │ desired = ceil(3 × 91 / 75) = 4   (max 10)
     │                │ write target_replicas = 4, start 3-min cooldown
     │                │ → continues as Flow 2 from "reconcile"
```

**Flow 5: an operator uses the web UI.**
```
  Browser         Controller (:50052)
     │                │
     │── GET /ui ────►│ embedded HTML/CSS/JS served from the binary
     │── login ──────►│ admin token → HttpOnly session cookie
     │── GET /ui/api ►│ JSON snapshot: nodes, services, workloads
     │── SSE stream ─►│ subscribe /ui/api/events
     │◄─ change event │ pushed on every state change
     │── drain node ─►│ same code path as AdminService.DrainNode, audited
```

### 2.5 Information flow summary

| Data | Produced by | Consumed by | Stored where | Lost on controller crash? |
| :--- | :--- | :--- | :--- | :--- |
| Service spec, target replicas | Operator, autoscaler | Reconciler | SQLite (fsync), S3 replica | No |
| Workload intent and status | Reconciler, scheduler, agent reports | Reconciler, UI, CLI | SQLite | No |
| Node liveness and usage | Agent heartbeats | Health check, scheduler, autoscaler, UI | Memory. `last_heartbeat_at` is flushed every 30 s. | Yes, rebuilt from heartbeats within the 30 s grace period (FR-CON-04) |
| Per-container CPU and memory | Agent (cgroups) | Autoscaler, UI charts | Memory (60 s window, 60 min UI ring buffer) | Yes, refills within minutes |
| In-flight commands | Dispatcher | Agent | Memory | Yes, re-derived by reconcile and re-sent |
| Scaling events, cluster events, audit trail | Autoscaler, controller, operators | UI, CLI | SQLite | No |
| User traffic | End users | Containers | n/a | n/a, never touches Valkvork |

### 2.6 Units (used consistently everywhere)

| Quantity | Manifest (human) | Proto / DB (machine) |
| :--- | :--- | :--- |
| CPU | `cpu = 0.5` (cores, float) | `cpu_millicores = 500` (integer) |
| Memory / disk | `memory = "512MiB"` (IEC units: `KiB`, `MiB`, `GiB`) | `memory_bytes = 536870912` (integer) |
| Durations | `"10s"`, `"5m"` | integer seconds or milliseconds, suffixed in the field name |
| Timestamps | n/a | integer Unix epoch milliseconds |

---

## 3. Lifecycle model

### 3.1 Workload state machine
Each workload row has two fields. **`desired_state`** (`RUN` | `STOP`) is the controller's intent. **`status`** is the last known fact.

| Status | Meaning | Terminal |
| :--- | :--- | :--- |
| `PENDING` | Created, not yet placed on a node. `status_reason` explains why if it cannot be placed. | no |
| `ASSIGNED` | Bound to a node, and `StartWorkload` has been sent. Waiting for the agent to report it running. | no |
| `RUNNING` | Agent reports the container running. | no |
| `STOPPING` | `StopWorkload` has been sent. Waiting for the agent to confirm. | no |
| `LOST` | Its node went `UNHEALTHY` while the workload was `ASSIGNED`, `RUNNING` or `STOPPING`. A replacement has been created. | no |
| `STOPPED` | Stopped on request. | **yes** |
| `FAILED` | Failed permanently: restart policy exhausted, image pull failed, start timeout, OOM after retries, or ephemeral disk exceeded. | **yes** |

```
             place               agent reports running
 PENDING ───────────► ASSIGNED ───────────────────────► RUNNING
    ▲                    │  │                              │  │
    │ no ack / start     │  │ agent reports failure        │  │ desired_state = STOP
    │ timeout (retry)    │  ▼                              │  ▼
    └────────────────────┘ FAILED ◄─────────────────────── │ STOPPING ──► STOPPED
                                    restarts exhausted     │
       node UNHEALTHY (from ASSIGNED / RUNNING / STOPPING) ▼
                                                         LOST ──► STOPPED (copy stopped on reconnect)
                                                           └────► RUNNING (re-adopted, see FR-CON-03)
```

### 3.2 Node states
* `status`: `HEALTHY` or `UNHEALTHY`, driven by heartbeats (FR-CON-02).
* `draining`: a boolean flag, independent of status. A draining node receives no new placements (FR-CON-08).

### 3.3 Service versions
Every service has an integer `version`. It is incremented by any apply that changes a **replacement-requiring field**: image, resources, env, ports, volumes or restart policy. Each workload records the `service_version` it was started with. A workload whose version is lower than the service's version is *outdated* and is replaced by the rolling update (FR-CON-07). Changes to `replicas` or `[autoscale]` never bump the version.

---

## 4. Functional requirements: Agent (`valkvork-agent`)

* **FR-AGE-01 [MVP]: Packaging**
  * MUST compile to a single statically linked binary for `x86_64-unknown-linux-musl`.
  * The Packer image MUST contain the agent binary at `/usr/local/bin/valkvork-agent`, its systemd unit, and **Docker Engine** (`docker-ce` ≥ 27.0, which installs `containerd.io` as a dependency).
  * `/etc/docker/daemon.json` MUST set the following (example in §10.2):
    * `"live-restore": true`, so containers survive dockerd restarts and upgrades (NFR-02);
    * the systemd cgroup driver;
    * the `local` log driver with rotation;
    * `overlay2` storage.
  * The host MUST use **cgroup v2** (unified hierarchy).
  * The Packer build MUST **truncate `/etc/machine-id`** and remove `/var/lib/valkvork/` before the image is captured. Otherwise every VM cloned from the image shares one identity (see FR-AGE-02).

* **FR-AGE-02 [MVP]: Configuration and identity**
  * The agent MUST read `/etc/valkvork/agent.toml`, which cloud-init renders at boot (example in §10.2). It holds the controller endpoint, CA bundle, join token path, `node_class`, reserved resources, and host volumes.
  * `node_id` MUST be a UUIDv5 derived from `/etc/machine-id`, persisted to `/var/lib/valkvork/node_id` on first boot, and stable across agent restarts.
  * The first message on every stream MUST be `Register`. It carries the identity, hostname, IP, node class, allocatable capacity (total minus `[reserved]`), host volumes, agent version, and a **full inventory of the Valkvork containers already on the host**.

* **FR-AGE-03 [MVP]: Heartbeat and telemetry**
  * The agent MUST send a `Heartbeat` every **2 s** containing node CPU and memory usage and a `WorkloadStatus` for every Valkvork-managed container.
  * Each `WorkloadStatus` MUST include per-container CPU usage (millicores, averaged since the last heartbeat, from cgroup `cpu.stat`) and memory usage (`memory.current`). The autoscaler depends on these (FR-AUT-01).
  * The agent MUST read these values directly from the container's cgroup v2 files, at the path given by the runtime driver. With Docker's systemd cgroup driver that path is `/sys/fs/cgroup/system.slice/docker-<container_id>.scope/`. The agent MUST NOT use the Docker stats API, which is too slow to call for every container every 2 s.

* **FR-AGE-04 [MVP]: Command execution**
  * Commands arrive on the agent's own stream (FR-ARC-01). Every command MUST get a `CommandResult` ack. The ack means "accepted or rejected". The workload's actual progress is reported through `WorkloadStatus`.
  * Commands MUST be idempotent, keyed by `workload_id`. A repeated `StartWorkload` for a workload that already exists is a no-op ack. A `StopWorkload` for an unknown workload acks success.
  * **Start:** pull the image. Registry credentials come from `/etc/valkvork/registry-auth.json` (Docker config format) and are sent as `X-Registry-Auth`. Then create a container named `vk-<workload_id>` with:
    * `NanoCpus = millicores × 1 000 000`;
    * `Memory` and `MemorySwap` both set to `memory_bytes`, so the container has no swap;
    * env vars;
    * `PortBindings` for ports and bind `Mounts` for volumes;
    * labels `valkvork.workload_id`, `valkvork.service_id`, `valkvork.service_version`;
    * **`RestartPolicy = "no"`**, because the agent owns restarts (FR-AGE-06).

    Then start the container. The container name is unique, so a create that hits `409 Conflict` is treated as "already exists". This keeps the command idempotent.
  * **Stop:** call `POST /containers/{id}/stop?t=<kill_timeout>`. Docker sends `SIGTERM` (or the image's `StopSignal`), waits `kill_timeout` (default 10 s, set per service), then sends `SIGKILL`. Then remove the container and its alloc directory.

* **FR-AGE-05 [MVP]: Autonomy under partition (blast-radius limit)**
  * If the stream breaks, the agent MUST NOT stop, restart or modify any container. Its only action is reconnecting.
  * Reconnects MUST use exponential backoff: start at 1 s, double each attempt, cap at **15 s**, ±20 % jitter.
  * After reconnecting, the agent sends `Register` with the full inventory. The controller then decides what happens to each container (FR-CON-03).

* **FR-AGE-06 [MVP]: Local restart policy**
  * If a container exits non-zero or is OOM-killed, the agent MUST restart it in place according to the service's restart policy (default: 3 attempts within 5 min, 5 s delay). This works even while the controller is unreachable. The agent detects exits from the runtime's event stream (Docker `/events`, filtered by label), not by polling. Docker's own restart policy is always `no` (FR-AGE-04), so the agent is the only component that restarts containers.
  * When attempts run out, the agent reports `FAILED` with a reason (`EXITED`, `OOM_KILLED`, …) and leaves the container stopped. Replacing it is the controller's decision.
  * A container that exits with code 0 is also reported, then treated as a failure for a `service` workload.

* **FR-AGE-07 [MVP]: Adoption after agent restart**
  * Restarting or upgrading the agent process MUST NOT restart containers. On startup the agent lists existing containers by the `valkvork.workload_id` label through the runtime driver and resumes managing them. Restarting dockerd itself is covered by `live-restore` (FR-AGE-01, AT-15).

* **FR-AGE-08 [MVP]: Ephemeral disk enforcement**
  * Each workload gets `/var/lib/valkvork/alloc/<workload_id>/`. The agent MUST count two things against the declared `disk` limit:
    * the alloc directory, measured every 10 s;
    * the container's writable layer (Docker `SizeRw`), measured every 60 s because the call is expensive.

    If the total exceeds the limit, the agent MUST stop the workload and report `FAILED` with reason `EPHEMERAL_DISK_EXCEEDED`. The restart policy does not apply in this case.
  * The MVP enforces this limit by polling, so there is a soft margin. Hard quotas (overlay2 on XFS with `pquota`, Docker `--storage-opt size=`) are [P2]. Container logs live under Docker's data root and are bounded by log rotation, not by `disk`.

* **FR-AGE-09 [MVP]: Runtime driver abstraction**
  * The agent MUST access the container runtime only through an internal `ContainerRuntime` trait: `pull`, `create`, `start`, `stop`, `remove`, `list_by_label`, `inspect`, `events`, `cgroup_path`, `writable_layer_size`. No Docker-specific type may leak outside the driver module.
  * The MVP implementation is **`DockerRuntime`**. It uses the Docker Engine API over `/var/run/docker.sock` through the `bollard` crate.
  * The controller, the proto and the manifests stay runtime-agnostic. `Register.runtime` reports the driver and version (e.g. `docker/27.3.1`) for observability and for future mixed fleets.

* **FR-AGE-10 [P2]: Native containerd driver**
  * **`ContainerdRuntime`** talks to containerd's gRPC API directly, in its own namespace (`valkvork`). This removes dockerd from the node, saving a daemon and its memory, and the extra failure point it brings.
  * It MUST reach parity with `DockerRuntime` on every point below before it can become a node's default:
    * image pull with registry auth, plus snapshot and unpack;
    * OCI spec generation (limits, mounts, labels);
    * networking through the CNI `bridge` and `portmap` plugins, including IP allocation and cleanup;
    * stdout/stderr capture with rotation;
    * event-driven exit detection;
    * adoption by label.
  * It is selected with `runtime = "containerd"` in `agent.toml`.
  * Containers cannot move between drivers: Docker keeps its containers in containerd's `moby` namespace, while the native driver uses `valkvork`. A node therefore switches driver only by being **drained and replaced** with a new image, which matches the immutable-worker model (§10.4).
  * Exit criteria: the full acceptance suite (§14) passes unchanged on a containerd-only image, and the per-node RAM saving is measured.

---

## 5. Functional requirements: Controller (`valkvork-controller`)

* **FR-CON-01 [MVP]: State store**
  * Desired state MUST be persisted in SQLite in WAL mode, with `synchronous = FULL`. An acknowledged `Apply` MUST survive a controller crash or power loss.
  * All writes MUST go through **one writer task** that owns the write connection. Reads use a separate read-only connection pool. This removes `SQLITE_BUSY` contention.
  * Node liveness and telemetry MUST live in an in-memory concurrent map (`DashMap` or equivalent). Heartbeats MUST NOT write to SQLite on every message: `last_heartbeat_at` is flushed every 30 s and on every status transition.
  * Schema changes MUST go through versioned migrations (`schema_migrations` table).

* **FR-CON-02 [MVP]: Node health**
  * A node MUST be marked `UNHEALTHY` when no message has arrived from it for **10 s** (5 missed heartbeats, configurable as `node_unhealthy_timeout`). A broken stream does not shorten this timer.
  * When a node becomes `UNHEALTHY`, its `ASSIGNED`, `RUNNING` and `STOPPING` workloads move to `LOST`. For each `LOST` workload with `desired_state = RUN`, the controller creates one replacement (`PENDING`, `replaces_workload_id` set).
  * A node returns to `HEALTHY` on its next `Register`.

* **FR-CON-03 [MVP]: Reconnect and reconciliation of reported containers**

  When an agent registers, the controller compares the agent's inventory with the workload table:

  | Reported container | Controller action |
  | :--- | :--- |
  | Workload is `ASSIGNED`/`RUNNING` on this node | Update status. No command. |
  | Workload is `LOST` and its replacement is still `PENDING` | **Re-adopt:** set the lost workload to `RUNNING` and delete the pending replacement. |
  | Workload is `LOST` and its replacement is `ASSIGNED`/`RUNNING` | Send `StopWorkload`. The lost copy goes `STOPPING`, then `STOPPED`. |
  | Workload has `desired_state = STOP` | Send `StopWorkload` again. |
  | No matching row (orphan) | Send `StopWorkload`. This is skipped during the recovery grace period. |

  This rule set guarantees that a healed partition never leaves more replicas than desired. It also avoids churn when no replacement could be placed, for example a workload pinned to a host volume.

* **FR-CON-04 [MVP]: Controller restart recovery**
  * On startup the controller MUST load desired state from SQLite and enter a **recovery grace period** of 30 s (greater than the agent's maximum backoff). During the grace period it does not mark nodes `UNHEALTHY`, does not create replacements, and does not stop orphans.
  * During the grace period, the in-memory heartbeat time of every node is initialised to the controller's start time.
  * Once the grace period ends, normal reconciliation resumes. A controller restart MUST NOT stop or restart any healthy running workload.

* **FR-CON-05 [MVP]: Reconcile loop**
  * Runs on events (apply, delete, node change, workload status change) and on a 5 s periodic tick. One pass per service at a time.
  * For each `ACTIVE` service: `live` = workloads with `desired_state = RUN` and a status other than `STOPPED`, `FAILED` or `LOST`.
    * `live < target_replicas`: insert `PENDING` workloads for the difference.
    * `live > target_replicas`: set `desired_state = STOP` on the excess and dispatch `StopWorkload`. Excess is chosen in this order: `PENDING`, then `ASSIGNED`, then `RUNNING` on the most-allocated node, newest first.
  * **Replacement backoff:** consecutive `FAILED` workloads of the same service version delay further replacements exponentially (5 s, 10 s, 20 s, …, capped at 5 min). After 5 consecutive failures the service is marked `DEGRADED` and an event is emitted. This prevents hot loops on a bad image.

* **FR-CON-06 [MVP]: Scheduler (spread placement)**
  * **Filter.** A node is eligible only if all of the following hold:
    * `status = HEALTHY` and not `draining`;
    * its `node_class` matches `required_class`, if one is set;
    * `free_cpu ≥ request` and `free_mem ≥ request`, where free = allocatable − Σ requests of workloads with `desired_state = RUN` that are not in a terminal status or `LOST` on that node;
    * every static `host_port` is free on the node;
    * every required host volume exists on the node.
  * **Score (least-allocated / spread).** Pick the node with the lowest `max(cpu_alloc_ratio, mem_alloc_ratio)` after placement. Break ties by fewest workloads of the same service on the node, then by `node_id`.
  * Placement is based on **requested** resources (= limits in MVP), not observed usage.
  * A workload that cannot be placed stays `PENDING` with `status_reason` (e.g. `NO_NODE: insufficient memory in class high-perf`). It is retried when capacity changes. Unplaced workloads are exported as a metric that drives infrastructure scaling (§10.4).
  * A single placement decision SHOULD take under 10 ms at the NFR-04 scale. Placements within one pass account for each other.

* **FR-CON-07 [MVP]: Rolling updates**
  * When `version` increases, the controller replaces outdated workloads `max_parallel` at a time (default 1). For each one it **starts the new workload first**, waits until the new workload has been `RUNNING` for `min_healthy_time` (default 10 s), then stops the old one.
  * If a new workload reaches `FAILED`, the rollout **halts**: `rollout_status = HALTED`, and the old workloads keep running. The next `apply` resumes or supersedes the rollout. MVP has no automatic rollback.
  * Autoscaling is suspended for a service while its rollout is in progress.

* **FR-CON-08 [MVP]: Node drain**
  * `valkvork node drain <node_id>` sets `draining = true` and migrates the node's workloads `max_parallel` at a time, using start-new-then-stop-old like FR-CON-07. The drain is complete when the node has no non-terminal workloads. This is used by worker scale-in and image upgrades (§10.4).

* **FR-CON-09 [MVP]: Command dispatch**
  * The controller tracks outstanding commands per node in memory. A command without a `CommandResult` within 30 s is re-sent once. All unresolved commands are re-sent after a reconnect.
  * An `ASSIGNED` workload that is not `RUNNING` within `start_timeout` (default 5 min, which includes the image pull) is marked `FAILED` with reason `START_TIMEOUT` and replaced under the FR-CON-05 backoff.

* **FR-CON-10 [MVP]: Admin API and CLI**
  * The controller MUST serve the `AdminService` gRPC API (§11.2) and the built-in web UI (§5.1). Every request requires a bearer token (§9) or a UI session (FR-UI-06).
  * CLI commands: `apply -f`, `plan -f` (dry-run diff plus placement feasibility), `delete service <id>`, `scale service <id> <n>`, `status [service]`, `workloads [--service]`, `events [--kind]`, `node list`, `node drain <id>`, `node undrain <id>`. Exit codes: `0` success, `1` server or transport error, `2` validation error.
  * **Manual scale** (CLI or UI) changes `target_replicas` only. With `[autoscale]`, the value is clamped to `[min, max]` and the autoscaler carries on from it. Without autoscaling, the next `apply` restores the manifest's `replicas` (FR-MAN-02), and `plan` shows this drift beforehand.
  * **Delete:** the service moves to `DELETING`, all its workloads get `desired_state = STOP`, and the service row is removed once every workload is terminal.

* **FR-CON-11 [MVP]: History retention**
  * Terminal workloads MUST be kept for at least 24 h, or as the last 10 per service, whichever keeps more. After that they are pruned. `scaling_events` and `cluster_events` are kept for 30 days.

### 5.1 Built-in web UI (`FR-UI`)

Like Nomad, which serves its UI at `/ui`, the controller binary serves an operator UI. There is nothing extra to deploy, install or keep version-matched.

* **FR-UI-01 [MVP]: Self-contained, with no third-party code**
  * Every UI asset (HTML, CSS, JS, SVG icons) MUST be compiled into the controller binary with `include_bytes!` and pre-gzipped at build time.
  * The UI MUST use **hand-written vanilla HTML, CSS and ES-module JavaScript**. Not allowed: CDNs, web fonts, third-party JS/CSS libraries or frameworks, and any npm/Node build step.
  * Charts and bars are inline SVG or CSS drawn by our own code.
  * The UI works unchanged in air-gapped networks.
  * Size budget: < 250 KB of uncompressed assets in total.
  * Server side: plain HTTP handlers on the same `hyper`/`axum` stack that `tonic` already pulls in. No extra web framework or template engine.
  * Every response carries a strict CSP: `default-src 'self'; img-src 'self' data:; frame-ancestors 'none'`. No inline scripts.
* **FR-UI-02 [MVP]: Where it is served**
  * The UI lives on the Admin listener **:50052** under `/ui/`. Requests are split by content type: `application/grpc` goes to `AdminService`, everything else goes to the UI router.
  * That gives one port, one TLS certificate and one firewall rule (FR-SEC-05). `/` redirects to `/ui/`.
  * Operators can switch the UI off with `[ui] enabled = false`.
* **FR-UI-03 [MVP]: Pages**

  | Page | Shows | Actions |
  | :--- | :--- | :--- |
  | **Overview** | Nodes by class and status. CPU and memory allocated vs allocatable per class. Workloads by status. Unplaced `PENDING` count. Controller health (recovery grace, DB size, last backup age). Last 20 events. | n/a |
  | **Services** | id, image, version, running/target, CPU utilisation, status, rollout status. | n/a |
  | **Service detail** | Canonical spec (TOML view). Workloads with node, status, reason and restarts. Rollout progress. CPU sparkline (last 60 min). Scaling events. | **Scale**; **Delete** (confirmed by typing the service id) |
  | **Nodes** | id, hostname, class, status, draining, allocation bars, workload count, heartbeat age, agent and runtime versions. | n/a |
  | **Node detail** | Capacity vs allocated, workloads on the node, host volumes. | **Drain** / **Undrain** |
  | **Workloads** | Filter by service, node or status. Status reason, restart count, age. | n/a |
  | **Events** | Node health changes, failures, deploys and rollouts, scaling, operator actions. Filter by kind. | n/a |
  | **Deploy** | Paste or upload TOML, run **Plan** (diff and placement check), then **Apply**. | **Apply** |

* **FR-UI-04 [MVP]: Live updates**
  * The browser subscribes to Server-Sent Events at `/ui/api/events` and updates in place. If SSE drops, it falls back to polling every 5 s. No WebSockets.
* **FR-UI-05 [MVP]: One code path**
  * The UI's JSON endpoints (`/ui/api/*`) are thin adapters over the same internal service layer as `AdminService`. No business logic lives in the UI or its handlers.
  * `/ui/api` is internal and unversioned. Scripts and automation use the gRPC API or the CLI.
* **FR-UI-06 [MVP]: Authentication and audit**
  * Operators sign in with the admin token.
  * The controller then issues a random session cookie (`HttpOnly; Secure; SameSite=Strict; Path=/ui`). Sessions expire after 12 h and are kept in memory, so a controller restart signs everyone out.
  * Every mutating request MUST also carry an `X-Valkvork-CSRF` header matching the session's CSRF token.
  * Every UI action is written to `cluster_events` as `AUDIT`, with the action, its target and the session id.
  * A read-only viewer role is [P2], with RBAC.
* **FR-UI-07 [MVP]: Chart data**
  * The controller keeps an in-memory ring buffer of 1-minute aggregates per service and per node class, covering the last 60 min. It is not persisted. Longer history comes from Prometheus (NFR-07).
* **FR-UI-08 [MVP]: Usability**
  * Supports current Chrome, Firefox, Safari and Edge, and layouts down to 1024 px wide.
  * Light and dark themes follow `prefers-color-scheme`.
  * Status is always shown as **text plus colour**, never colour alone.
  * Every page has a deep-linkable URL, e.g. `/ui/services/frontend-web` or `/ui/nodes/<id>`.
* **FR-UI-09 [MVP]: Isolation from the control loop**
  * Each SSE client gets a bounded channel. Slow clients are dropped and reconnect.
  * UI load MUST NOT delay reconcile, scheduling or heartbeat handling.
  * The UI adds < 5 MB to controller RSS with 10 open sessions.

**Overview page wireframe**
```
┌ Valkvork ──────────────────── Overview  Services  Nodes  Workloads  Events  Deploy ┐
│ Controller: healthy · uptime 3d 4h · DB 12 MB · last backup 4 s ago                │
│                                                                                    │
│ NODES            WORKLOADS          UNPLACED          AUTOSCALING (last 1 h)       │
│ 12 healthy       148 running        0 pending         5 actions                    │
│  1 unhealthy       3 pending                                                       │
│  1 draining        2 failed                                                        │
│                                                                                    │
│ CAPACITY BY CLASS        CPU allocated             Memory allocated                │
│ general   (10 nodes)     ███████████░░░░  71 %     █████████░░░░░░  58 %           │
│ high-perf ( 4 nodes)     ██████░░░░░░░░░  40 %     ████████████░░░  80 %           │
│                                                                                    │
│ RECENT EVENTS                                                                      │
│ 12:04:51  NODE   ip-10-0-3-17 UNHEALTHY (no heartbeat 10 s), 4 workloads replaced  │
│ 12:03:10  SCALE  frontend-web 3 → 4 (cpu 91 % > 82.5 % for 120 s)                  │
│ 12:01:42  DEPLOY data-cruncher-v2 v4 → v5, rollout complete                        │
└────────────────────────────────────────────────────────────────────────────────────┘
```

---

## 6. Functional requirements: Manifests (`FR-MAN`) and Autoscaling (`FR-AUT`)

### 6.1 Manifest format

* **FR-MAN-01 [MVP]: TOML format.** One file declares one service. Recognised tables: `[service]`, `[resources]`, `[constraints]`, `[env]`, `[[ports]]`, `[[volumes]]`, `[autoscale]`, `[update]`, `[restart]`. **Unknown keys MUST be rejected** (`deny_unknown_fields`) so typos fail loudly.

| Key | Type | Default | Notes |
| :--- | :--- | :--- | :--- |
| `service.id` | string | required | DNS label, immutable. |
| `service.image` | string | required | OCI reference. A digest is allowed. |
| `service.replicas` | int ≥ 0 | required | `0` = keep the service defined but run nothing. With `[autoscale]`, see FR-MAN-04. |
| `service.kill_timeout` | duration | `"10s"` | Grace period between SIGTERM and SIGKILL. |
| `resources.cpu` | float > 0 | required | Cores. Converted to millicores and rounded **up** to the nearest 1 m. |
| `resources.memory` | size | required | ≥ `4MiB`. |
| `resources.disk` | size | `"1GiB"` | Ephemeral alloc directory limit (FR-AGE-08). |
| `constraints.required_class` | string | none (any node) | Matches the agent's `node_class`. |
| `env.<KEY>` | string | none | Plaintext, so not for secrets (FR-SEC-04). |
| `ports[].name` / `container_port` / `host_port` | string / int / int | none | Static host ports only in MVP (§8). |
| `volumes[]` | table | none | `type = "host"` only in MVP (§7). |
| `autoscale.min_replicas` / `max_replicas` / `target_cpu_percent` | int / int / float | none | Presence enables autoscaling. |
| `update.max_parallel` / `min_healthy_time` | int / duration | `1` / `"10s"` | FR-CON-07. |
| `restart.attempts` / `interval` / `delay` | int / duration / duration | `3` / `"5m"` / `"5s"` | FR-AGE-06. |

* **FR-MAN-02 [MVP]: Idempotent apply (upsert)**
  * The controller validates the manifest, canonicalises it, and computes `spec_hash` (SHA-256 of the canonical form).
  * If the service does not exist, it is created at version 1.
  * If the service exists, the hash is unchanged, and (for services without `[autoscale]`) the live `target_replicas` equals the manifest's `replicas`, nothing happens and the response says `unchanged`. If only the replica count differs because of a manual scale, the apply restores the manifest value as a scale-only change.
  * Otherwise the diff is classified as **scale-only** (`replicas`), **policy-only** (`[autoscale]`, `[update]`) or **replacing** (everything else). A replacing diff bumps `version`. The update is written in one transaction, and only replacing changes touch running workloads.
* **FR-MAN-03 [MVP]: Plan.** `valkvork plan -f` returns the same classification plus a placement feasibility check, and writes nothing.

  ```
  $ valkvork plan -f frontend.toml
  ~ service "frontend-web"   version 4 → 5   [rolling update, max_parallel=1]
      image:    nginx:1.27.1-alpine-slim → nginx:1.27.2-alpine-slim
      replicas: 3 (unchanged)
    placement: 3/3 replacement workloads placeable
  ```
* **FR-MAN-04 [MVP]: Replicas versus autoscaling.** When `[autoscale]` is present, `replicas` is used only on create, as the initial count. From then on the autoscaler owns `target_replicas`. Re-applying the manifest updates `min`/`max`/`target` and clamps the current count into `[min, max]`, but never resets it to `replicas`. Without this rule, every CI apply would undo autoscaling.

### 6.2 Validation rules

Validation runs before any database write. A failure writes nothing and returns gRPC `INVALID_ARGUMENT` with a machine-readable code and the TOML field path. The CLI exits with code `2`.

| ID | Rule | Code |
| :--- | :--- | :--- |
| VE-01 | `cpu > 0`, `memory ≥ 4MiB`, `disk > 0`, `replicas ≥ 0`. | `INVALID_RESOURCE_SPEC` |
| VE-02 | `image` is a valid OCI reference. A missing tag defaults to `:latest` and returns a **warning**. Note that re-applying `:latest` is a no-op and does not pull a new image, so pinned tags or digests are recommended. | `INVALID_IMAGE_REF` |
| VE-03 | `service.id` matches `^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$`. | `INVALID_SERVICE_ID` |
| VE-04 | `1 ≤ min_replicas ≤ max_replicas`, and `0 < target_cpu_percent < 100`. | `INVALID_AUTOSCALE_SPEC` |
| VE-05 | Port numbers are in 1–65535 and `host_port` values are unique within the manifest. `host_port` < 1024 returns a warning. | `INVALID_PORT_SPEC` |
| VE-06 | No unknown keys or tables. | `UNKNOWN_FIELD` |
| VE-07 | Every `[[volumes]]` entry has a known `type`, an absolute `target`, and a unique `target`. | `INVALID_VOLUME_SPEC` |
| VW-01 | **Warning only:** no current node of the required class could fit even one replica, or no node has the required host volume. | `UNSATISFIABLE_NOW` |

### 6.3 Examples

**Stateless web tier with autoscaling** (`frontend.toml`)
```toml
[service]
id       = "frontend-web"
image    = "docker.io/library/nginx:1.27.2-alpine-slim"
replicas = 3

[resources]
cpu    = 0.5
memory = "512MiB"

[constraints]
required_class = "general"

[[ports]]
name           = "http"
container_port = 80
host_port      = 8080

[autoscale]
min_replicas       = 3
max_replicas       = 10
target_cpu_percent = 75
```

**Heavy compute pinned to a high-performance class** (`ml-processor.toml`)
```toml
[service]
id       = "data-cruncher-v2"
image    = "internal-registry.net/analytics/cruncher:2.4.1"
replicas = 1
kill_timeout = "60s"     # allow in-flight batches to flush

[resources]
cpu    = 4.0
memory = "8GiB"
disk   = "20GiB"

[constraints]
required_class = "high-perf"

[env]
CRUNCHER_THREADS = "4"
```

**Stateful service on a host volume** (`postgres.toml`)
```toml
[service]
id       = "postgres-main"
image    = "docker.io/library/postgres:16.4"
replicas = 1             # host volumes are node-local: keep this at 1

[resources]
cpu    = 2.0
memory = "4GiB"

[[volumes]]
type   = "host"
source = "pgdata"        # must be declared in the node's agent.toml
target = "/var/lib/postgresql/data"
```

### 6.4 Apply-to-running trace (replicas 1 → 3)

```
valkvork apply -f app.toml
  │  validate → canonicalise → hash differs → diff = scale-only
  ▼
Controller writer task ── BEGIN; UPDATE services SET target_replicas = 3; COMMIT (fsync)
  │  emits ServiceChanged event
  ▼
Reconcile pass ── live (1) < target (3) → INSERT 2 × workloads(status = PENDING)
  ▼
Scheduler ── filter (HEALTHY, class, cpu/mem, ports) → score (least-allocated) → bind node_id
  │  status = ASSIGNED
  ▼
Agent stream ── ControllerMessage{StartWorkload} → agent acks CommandResult
  ▼
Agent ── pull image → create container (cgroup limits, labels) → start
  │  next Heartbeat reports state = RUNNING
  ▼
Controller ── status = RUNNING
```

### 6.5 Autoscaling (`FR-AUT`)

* **FR-AUT-01 [MVP]: Metric.** For each service, *CPU utilisation* = mean over its `RUNNING` workloads of `cpu_usage_millicores / cpu_limit_millicores × 100`, built from the per-workload samples in FR-AGE-03 and smoothed as a **60 s rolling average**. The following are excluded: workloads younger than 30 s (startup spikes) and outdated workloads during a rollout. Memory utilisation is collected and exported but is **not** a scaling signal in MVP.
* **FR-AUT-02 [MVP]: Evaluation.** Every 10 s, for each service with autoscaling enabled:
  * **Scale up** when utilisation > `target × 1.1` at every evaluation for **2 consecutive minutes**. `desired = min(max, ceil(current × util / target))`.
  * **Scale down** when utilisation < `target × 0.9` at every evaluation for 2 minutes. `desired = max(min, ceil(current × util / target), ceil(current / 2))`, so each action removes at most half the replicas.
  * The ±10 % band around the target is a dead zone and causes no action.
* **FR-AUT-03 [MVP]: Cooldown.** After any scaling action, that service gets a **3-minute** cooldown with no further actions.
* **FR-AUT-04 [MVP]: Safety guards.** No evaluation runs while any of the following is true:
  * a rollout is in progress;
  * the controller recovery grace period is active;
  * fewer than 50 % of the service's `RUNNING` workloads have samples in the window. Missing data never triggers a scale-down.

  Scale-up is also skipped while the service has unplaced `PENDING` workloads. In that case more replicas would not help, and the shortage is an infrastructure-scaling signal instead.
* **FR-AUT-05 [MVP]: Audit.** Every action writes a `scaling_events` row with from/to counts, the metric value and the reason. The action takes effect through the normal reconcile loop.

---

## 7. Storage (`FR-STO`)

### 7.1 Philosophy
Storage is a declarative primitive, defined in the same manifest and scheduled in the same pass as compute. Operators do not register volumes as a separate step, and they do not run storage plugins as user-managed jobs. The subsystem is delivered in phases, because the later phases (network volumes with fencing, and storage pooling) are each a large project on their own.

### 7.2 MVP storage [MVP]
* **FR-STO-01: Ephemeral alloc disk.** Each workload gets a per-workload directory with a declared size limit, enforced by FR-AGE-08. The directory is deleted when the workload stops.
* **FR-STO-02: Host volumes.**
  * Operators declare named directories in `agent.toml` (`[host_volumes.<name>]`). The agent reports them in `Register`.
  * A manifest that mounts `type = "host"` volumes can only be placed on nodes that advertise every volume it names (FR-CON-06 filter).
  * The agent bind-mounts the directory into the container, so the container needs no privileges.
  * **Data is node-local.** If the node dies, the workload stays `PENDING` until a node with that volume returns. FR-CON-03 then re-adopts the original container if it is still there.
* **FR-STO-03: No privileged containers.** The agent, which runs as root, performs every mount. Workloads MUST NOT need `privileged` or extra capabilities to use volumes, and MVP offers no `privileged` option at all.

### 7.3 Network block volumes [P2]
* **FR-STO-10: Inline provisioning.**
  ```toml
  [[volumes]]
  type   = "block"
  name   = "orders-db"
  size   = "100GiB"
  class  = "gp3"
  target = "/data"
  ```
  The volume is created on first placement, recorded in a `volumes` table, and reused by later placements.
* **FR-STO-11: Built-in drivers.** Drivers for AWS EBS and GCP Persistent Disk are compiled into the agent and controller. There are no CSI plugin jobs. Driver IPC, if any, uses a Unix socket at `/var/run/valkvork/storage.sock`, mode `0600`, owned by root, and verified with `SO_PEERCRED`.
* **FR-STO-12: Atomic bind.** Provision, attach and mount form part of the workload's start. If any step fails, the controller rolls back by detaching, and deletes the volume if it was created during this placement. The workload becomes `FAILED` with reason `VOLUME_ATTACH_FAILED`, and no volume is left orphaned.
* **FR-STO-13: Fencing before reattach.** A single-writer volume MUST NOT be attached to a new node until the old node is **fenced**. Fencing means the cloud API confirms the old instance is stopped or terminated (the controller terminates it if necessary), or the volume has been force-detached. The controller never relies on a timeout alone.
* **FR-STO-14: Zone topology.** A volume's availability zone becomes a hard placement filter.
* **FR-STO-15: Block I/O QoS.** `iops_limit` and `bps_limit` (e.g. `iops_limit = 5000`, `bps_limit = "250MiB"`) are written to cgroup v2 `io.max` for the volume's device (`major:minor`). They apply only to dedicated block devices, not to the shared root filesystem.

### 7.4 Long-term storage [P3]
* **FR-STO-20:** Out-of-band reconciliation: detect cloud-side volume resizes and snapshots and record them in the ledger without restarting workloads.
* **FR-STO-21:** Topology anti-affinity: replicas must not share a failure domain (disk tray, rack, switch).
* **FR-STO-22:** Local NVMe discovery and pooling. *Recommendation:* expose discovered NVMe devices as managed host volumes or LVM pools, rather than building a distributed filesystem. A distributed filesystem (replication, rebuild, consistency) is a separate product. If pooling is wanted, integrate an existing system such as Ceph instead. The decision is tracked in §16.

### 7.5 Positioning vs Nomad (target state)

| Dimension | Nomad today | Valkvork target | Phase |
| :--- | :--- | :--- | :--- |
| Volume setup | Volumes are created or registered separately from the job, then claimed by it. | Declared inline in the manifest and provisioned during scheduling. | P2 |
| Driver management | CSI node and controller plugins run as user-managed jobs. | Drivers are built into the agent and controller. | P2 |
| Detach on node loss | Depends on claim GC and plugin health, and stuck claims are a known operational pain point. | Verified fencing before reattach. | P2 |
| Block I/O limits | Not a first-class jobspec feature. | cgroup v2 `io.max` for each volume. | P2 |
| Local disk | `ephemeral_disk` and host volumes. | Ephemeral limits and host volumes now. Managed NVMe pools later. | MVP / P3 |

---

## 8. Networking and traffic [MVP]

Valkvork has two separate traffic paths. **They share nothing.**

| Path | Who → who | Goes through a load balancer? | Nomad equivalent |
| :--- | :--- | :--- | :--- |
| **Control traffic** | Agent → controller (gRPC :50051) | **No.** Direct connection. | Client → server RPC (:4647) |
| **Application ingress** | End users → your app containers (e.g. :8080) | **Yes.** Cloud LB → worker host ports. | Not built in. Usually Traefik/Fabio/nginx with Consul or Nomad service discovery. |

### 8.1 Control traffic (agent → controller)
* **FR-NET-04:** Agents dial the controller **directly**, the same way Nomad clients dial servers. There is no load balancer, because there is exactly one controller. The connection goes to a **stable DNS name** (`controlplane.valkvork.internal`) that resolves to a fixed private IP. In AWS that IP lives on an ENI, and the controller's boot script moves the ENI to the replacement instance (§10.3). Agents never need reconfiguring after a controller replacement.

### 8.2 Application ingress (users → workloads)
* **FR-NET-01:** Workloads use Docker's default `bridge` network with **static host-port mappings** (`PortBindings`, e.g. container :80 → host :8080). A node can run at most one workload per `host_port`, which the scheduler enforces as a resource. As a result, `replicas` for a service with a static port is limited to the number of eligible nodes.
* **FR-NET-02:** A public load balancer has one target group per `(node_class, host_port)`, for example "all `general` workers on :8080". Membership is static: the group is attached to the class's worker ASG, so every worker is a target whether or not it runs the service.
  * The LB **health-checks the host port on each worker.** This is not a check of the agent. It is the LB's way of discovering which workers currently run a replica:
    * a worker running `frontend-web` answers on :8080, passes, and gets traffic;
    * a worker without it refuses the connection, fails, and gets none.
  * When the scheduler moves a replica, the health checks follow within a few intervals. Neither the controller nor the agent has to call the LB API, so the MVP needs no service registry.
  * Trade-off: up to a few health-check intervals of misrouted or refused requests while a replica moves (about 10–30 s with a 5 s interval and 2 failures to mark unhealthy). Set the LB deregistration delay at or below `kill_timeout` so draining connections finish.
* **FR-NET-03 [P2]:** Dynamic host ports (`host_port = 0`), a service catalogue API, and **direct target registration**: the controller adds and removes `node_ip:port` targets in the LB as workloads start and stop. This removes the health-check lag and allows more than one replica per node. The native containerd driver (FR-AGE-10) uses CNI `portmap` for the same port model.

---

## 9. Security [MVP]

* **FR-SEC-01: Agent → controller.** The connection uses TLS (`rustls`), and the agent verifies the controller certificate against the CA bundle in `/etc/valkvork/ca.pem`. Agents authenticate with a **join token**, which cloud-init fetches at boot from a secrets store (AWS SSM Parameter Store or GCP Secret Manager). The token is never baked into the image. Unauthenticated `Register` calls are rejected.
* **FR-SEC-02 [P2]:** Mutual TLS with per-node certificates, issued by the controller from a CSR on first join and rotated automatically.
* **FR-SEC-03: Admin API and UI.** TLS plus a bearer admin token held in the controller's config. The token must be at least 32 random bytes. The web UI exchanges the same token for a session cookie with CSRF protection (FR-UI-06). RBAC is [P2].
* **FR-SEC-04: Secrets.** `[env]` values are stored in plaintext in SQLite and in the Litestream replica, so manifests MUST NOT carry secrets in MVP. Secret references, resolved by the agent at start, are [P2].
* **FR-SEC-05: Network.** Port 50051 is reachable from the worker subnets only. Port 50052 is reachable from the operator/CI CIDR or a bastion only. The agent itself opens no inbound ports. Workers accept inbound traffic only on workload host ports, and only from the load balancer's security group.
* **FR-SEC-06: Docker socket.** `/var/run/docker.sock` gives root on the host. Only the agent may use it. dockerd MUST NOT listen on TCP (`-H tcp://`). The agent MUST refuse to mount the Docker socket, or any path under `/var/lib/docker`, into a workload, even if a host volume points there.

---

## 10. Production deployment (AWS reference; GCP equivalents in brackets)

### 10.1 Topology

```
            Internet (end users → apps only)
                       │
          ┌────────────▼────────────┐
          │  Public ALB / NLB       │  app ingress only: one target group per
          └─────┬──────────────┬────┘  (node_class, host_port), health-checks host ports
                │              │
   ┌────────────▼─────┐ ┌──────▼───────────┐   Worker ASG per node_class
   │ Worker VM        │ │ Worker VM        │ … (Packer image, private
   │ agent+docker-ce  │ │ agent+docker-ce  │    subnets, multi-AZ)
   └────────┬─────────┘ └──────┬───────────┘
            │  outbound gRPC/TLS :50051
            └────────┬─────────┘
          ┌──────────▼──────────────────────────┐
          │ Controller VM  (ASG 1/1/1, one AZ)  │──► S3 bucket (Litestream WAL
          │ fixed-IP ENI + DNS name             │    replica, versioned)
          │ EBS volume: /var/lib/valkvork/db    │
          └─────────────────────────────────────┘
```

### 10.2 Worker image (Packer)
* **Contents:** `docker-ce` (which brings `containerd.io`) with the `daemon.json` below, `valkvork-agent`, the systemd unit, a CA bundle, chrony, and a cgroup v2-enabled kernel.
* **Excluded:** no join token, no registry credentials, no per-node state. These arrive through cloud-init at boot.
* **Final build step:** `truncate -s0 /etc/machine-id && rm -rf /var/lib/valkvork` (FR-AGE-01).

```ini
# /etc/systemd/system/valkvork-agent.service
[Unit]
Description=Valkvork Worker Agent
Wants=network-online.target
After=network-online.target docker.service
Requires=docker.service

[Service]
Type=simple
ExecStart=/usr/local/bin/valkvork-agent --config /etc/valkvork/agent.toml
Restart=always
RestartSec=3s
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
```
Containers are children of containerd shims, not of the agent or of dockerd. Restarting the agent unit leaves them alone (AT-04). With `live-restore`, restarting `docker.service` leaves them alone too (AT-15). `Requires=docker.service` means a dockerd restart also restarts the agent, which then adopts its containers again (FR-AGE-07).

```json
// /etc/docker/daemon.json
{
  "live-restore": true,
  "exec-opts": ["native.cgroupdriver=systemd"],
  "storage-driver": "overlay2",
  "log-driver": "local",
  "log-opts": { "max-size": "10m", "max-file": "3" }
}
```

```toml
# /etc/valkvork/agent.toml  (rendered by cloud-init)
controller_endpoint = "https://controlplane.valkvork.internal:50051"
ca_file             = "/etc/valkvork/ca.pem"
join_token_file     = "/etc/valkvork/join-token"
node_class          = "general"
data_dir            = "/var/lib/valkvork"
runtime             = "docker"                  # "containerd" in P2 (FR-AGE-10)
docker_socket       = "/var/run/docker.sock"

[reserved]               # kept back for the OS, dockerd/containerd and the agent
cpu    = 0.25
memory = "384MiB"

[host_volumes.pgdata]
path      = "/data/pgdata"
read_only = false
```

### 10.3 Controller availability (single node)
* **ASG:** `min = max = desired = 1` [GCP: a stateful MIG of size 1], **pinned to one AZ**, because EBS volumes are AZ-scoped.
* **Boot script** (user-data), in order:
  1. Attach the tagged EBS data volume. If the dead instance still holds it, force-detach first, after confirming that instance is terminated.
  2. Mount the volume at `/var/lib/valkvork/db`.
  3. Attach the fixed-IP ENI.
  4. If the volume is missing or the database fails `PRAGMA integrity_check`, run `litestream restore` from S3.
  5. Start `litestream replicate` and `valkvork-controller`.
* **Backups:** Litestream streams the WAL continuously to a versioned S3 bucket (RPO ≈ seconds). This copy covers losing the AZ or the volume. A nightly snapshot of the EBS volume gives defence in depth.
* **SQLite on block storage only:** never place the database on NFS, EFS or any other network filesystem.
* **Targets:** RTO < 5 min for instance loss (detect, boot, attach, recover) and < 15 min for AZ loss (manual or scripted ASG AZ switch plus S3 restore). The data plane keeps serving traffic throughout (NFR-02).

### 10.4 Two-layer autoscaling
* **Loop A: application (Valkvork).** The controller changes replica counts (§6.5). It works only within existing capacity.
* **Loop B: infrastructure (cloud ASG, one per node class).** The scale signals come from the **controller's** metrics, published to CloudWatch [Cloud Monitoring]. Host CPU is not used, because Valkvork places workloads by reservation, not by usage.
  * **Scale out** when `valkvork_pending_unplaced_workloads{class} > 0` for 1 min, **or** when `valkvork_allocated_ratio{class}` is above 80 %.
  * **Scale in** when `valkvork_allocated_ratio{class}` stays below 40 % for 15 min.
  * Every scale-in, and every instance refresh for an image upgrade, passes through an **ASG lifecycle hook**. The hook calls `valkvork node drain` (FR-CON-08) and lets termination proceed only after the drain completes.
* New nodes join automatically: the VM boots, the agent registers, and pending workloads are placed.

### 10.5 Failure matrix

| Failure | Detection | System response | Workload impact |
| :--- | :--- | :--- | :--- |
| Controller process crash | systemd | Restart, then 30 s recovery grace (FR-CON-04). | None. Scaling and replacement pause for about 30 s. |
| Controller VM loss | ASG health check | New VM, volume re-attach, ENI move (§10.3). | None to running workloads. No changes for up to 5 min. |
| Controller volume or AZ loss | Boot script | Restore from S3 with Litestream. | None to running workloads. Intent from the last seconds before the loss may be lost. |
| Worker VM dies | 10 s without heartbeat | `LOST`, then replacements placed (FR-CON-02). | Replicas on that node are down for ~10–15 s plus start time. |
| Network partition (worker ↔ controller) | 10 s without heartbeat | Replacements placed. On heal, apply the FR-CON-03 rules. | Replicas briefly exceed the target during the partition and converge exactly after it heals. |
| Agent crash or upgrade | systemd | Agent restarts and adopts its containers (FR-AGE-07). | None. |
| dockerd or containerd crash | Agent's runtime calls fail | systemd restarts the daemon. `live-restore` and the shims keep containers alive. The agent rejects commands with `RUNTIME_UNAVAILABLE` until the runtime is back (the controller retries under FR-CON-09), then re-adopts its containers. | None expected. New starts are delayed. |
| Container crash loop | Agent | Local restarts, then `FAILED`, then replacement with backoff, then `DEGRADED` (FR-CON-05). | Degraded capacity for that service. |
| Bad image or failed pull | Agent reports `IMAGE_PULL_FAILED` | Rollout halts and old replicas keep running (FR-CON-07). | None during a rollout. On a fresh create, nothing starts. |
| Cluster out of capacity | Scheduler | Workloads stay `PENDING`, the metric fires, and Loop B adds a node. | Delayed scale-up. |

---

## 11. Interface contracts

### 11.1 Agent stream (`valkvork.v1`)

```protobuf
syntax = "proto3";
package valkvork.v1;

// Agent-initiated, long-lived bidirectional stream. First AgentMessage MUST be `register`.
service AgentService {
  rpc Connect(stream AgentMessage) returns (stream ControllerMessage);
}

message AgentMessage {
  oneof msg {
    Register      register       = 1;
    Heartbeat     heartbeat      = 2;
    CommandResult command_result = 3;
  }
}

message ControllerMessage {
  oneof msg {
    RegisterAck   register_ack   = 1;
    StartWorkload start_workload = 2;
    StopWorkload  stop_workload  = 3;
  }
}

message Register {
  string node_id                   = 1;  // UUIDv5 of /etc/machine-id
  string join_token                = 2;
  string hostname                  = 3;
  string ip_address                = 4;
  string node_class                = 5;
  string agent_version             = 6;  // semver
  uint32 allocatable_cpu_millicores = 7; // capacity minus [reserved]
  uint64 allocatable_memory_bytes  = 8;
  repeated string host_volumes     = 9;
  repeated WorkloadStatus inventory = 10; // every Valkvork container on the host
  string runtime                   = 11; // driver/version, e.g. "docker/27.3.1" (FR-AGE-09)
}

message RegisterAck {
  bool   accepted              = 1;
  string reject_reason         = 2;
  uint32 heartbeat_interval_ms = 3;      // default 2000
}

message Heartbeat {
  uint64 seq                    = 1;
  double node_cpu_usage_percent = 2;
  uint64 node_used_memory_bytes = 3;
  repeated WorkloadStatus workloads = 4;
}

enum WorkloadState {
  WORKLOAD_STATE_UNSPECIFIED = 0;
  WORKLOAD_STATE_STARTING    = 1;        // pulling / creating
  WORKLOAD_STATE_RUNNING     = 2;
  WORKLOAD_STATE_RESTARTING  = 3;        // local restart policy in progress
  WORKLOAD_STATE_STOPPED     = 4;        // stopped on request
  WORKLOAD_STATE_FAILED      = 5;        // see reason
}

message WorkloadStatus {
  string workload_id          = 1;
  string service_id           = 2;
  uint64 service_version      = 3;
  string container_id         = 4;
  WorkloadState state         = 5;
  string reason               = 6;       // EXITED, OOM_KILLED, IMAGE_PULL_FAILED, EPHEMERAL_DISK_EXCEEDED, ...
  int32  exit_code            = 7;
  uint32 restart_count        = 8;
  uint32 cpu_usage_millicores = 9;       // average since previous heartbeat
  uint64 memory_used_bytes    = 10;
  uint64 disk_used_bytes      = 11;
}

message StartWorkload {
  string command_id            = 1;
  string workload_id           = 2;
  string service_id            = 3;
  uint64 service_version       = 4;
  string image                 = 5;
  uint32 cpu_millicores        = 6;
  uint64 memory_bytes          = 7;
  uint64 disk_bytes            = 8;
  map<string, string> env      = 9;
  repeated PortMapping ports   = 10;
  repeated VolumeMount volumes = 11;
  RestartPolicy restart        = 12;
  uint32 kill_timeout_seconds  = 13;
}

message PortMapping   { string name = 1; uint32 container_port = 2; uint32 host_port = 3; }
message VolumeMount   { string type = 1; string source = 2; string target = 3; bool read_only = 4; }
message RestartPolicy { uint32 attempts = 1; uint32 interval_seconds = 2; uint32 delay_seconds = 3; }

message StopWorkload {
  string command_id           = 1;
  string workload_id          = 2;
  uint32 kill_timeout_seconds = 3;
}

message CommandResult {
  string command_id = 1;
  string workload_id = 2;
  bool   accepted   = 3;
  string error      = 4;
}
```

### 11.2 Admin API

```protobuf
service AdminService {
  rpc Apply(ApplyRequest)                 returns (ApplyResponse);   // FR-MAN-02
  rpc Plan(ApplyRequest)                  returns (PlanResponse);    // FR-MAN-03, no writes
  rpc DeleteService(DeleteServiceRequest) returns (DeleteServiceResponse);
  rpc GetService(GetServiceRequest)       returns (ServiceDetail);
  rpc ListServices(ListServicesRequest)   returns (ListServicesResponse);
  rpc ListWorkloads(ListWorkloadsRequest) returns (ListWorkloadsResponse);
  rpc ListNodes(ListNodesRequest)         returns (ListNodesResponse);
  rpc DrainNode(DrainNodeRequest)         returns (DrainNodeResponse);
  rpc UndrainNode(UndrainNodeRequest)     returns (UndrainNodeResponse);
  rpc ScaleService(ScaleServiceRequest)   returns (ScaleServiceResponse);  // FR-CON-10 manual scale
  rpc ListEvents(ListEventsRequest)       returns (ListEventsResponse);    // cluster_events feed
}

message ApplyRequest  { string manifest_toml = 1; }
message ApplyResponse {
  string service_id = 1;
  uint64 version    = 2;
  string change     = 3;               // created | unchanged | scale-only | policy-only | replacing
  repeated string warnings = 4;        // VE-02, VE-05, VW-01
}
// Remaining messages are defined in proto/valkvork/v1/admin.proto.
```

**Compatibility rules.** Proto field numbers are never reused or renumbered, and removed fields are marked `reserved`. The controller MUST accept agents one minor version older than itself, so the controller is upgraded first and workers roll afterwards.

---

## 12. Persistence schema (SQLite)

```sql
-- Set once at database creation (persistent):
PRAGMA journal_mode = WAL;
-- Set on every connection:
PRAGMA foreign_keys = ON;
PRAGMA synchronous  = FULL;
PRAGMA busy_timeout = 5000;

CREATE TABLE schema_migrations (
    version    INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL                       -- unix ms
);

CREATE TABLE nodes (
    node_id                   TEXT PRIMARY KEY,       -- UUIDv5 from agent
    hostname                  TEXT NOT NULL,
    ip_address                TEXT NOT NULL,
    node_class                TEXT NOT NULL,
    agent_version             TEXT NOT NULL,
    runtime                   TEXT NOT NULL,          -- e.g. 'docker/27.3.1' (FR-AGE-09)
    allocatable_cpu_millicores INTEGER NOT NULL CHECK (allocatable_cpu_millicores > 0),
    allocatable_memory_bytes  INTEGER NOT NULL CHECK (allocatable_memory_bytes > 0),
    host_volumes              TEXT NOT NULL DEFAULT '[]',   -- JSON array of names
    status                    TEXT NOT NULL DEFAULT 'HEALTHY'
                              CHECK (status IN ('HEALTHY', 'UNHEALTHY')),
    draining                  INTEGER NOT NULL DEFAULT 0 CHECK (draining IN (0, 1)),
    last_heartbeat_at         INTEGER NOT NULL,       -- flushed lazily (FR-CON-01)
    created_at                INTEGER NOT NULL,
    updated_at                INTEGER NOT NULL
);

CREATE TABLE services (
    service_id          TEXT PRIMARY KEY,
    version             INTEGER NOT NULL DEFAULT 1,
    spec_json           TEXT NOT NULL,                -- canonical validated manifest
    spec_hash           TEXT NOT NULL,                -- sha256(spec_json), idempotency
    image               TEXT NOT NULL,
    cpu_millicores      INTEGER NOT NULL CHECK (cpu_millicores > 0),
    memory_bytes        INTEGER NOT NULL CHECK (memory_bytes > 0),
    required_class      TEXT,                         -- NULL = any class
    target_replicas     INTEGER NOT NULL CHECK (target_replicas >= 0),
    autoscale_enabled   INTEGER NOT NULL DEFAULT 0 CHECK (autoscale_enabled IN (0, 1)),
    min_replicas        INTEGER,
    max_replicas        INTEGER,
    target_cpu_percent  REAL,
    last_scaled_at      INTEGER,                      -- cooldown anchor (FR-AUT-03)
    status              TEXT NOT NULL DEFAULT 'ACTIVE'
                        CHECK (status IN ('ACTIVE', 'DEGRADED', 'DELETING')),
    rollout_status      TEXT NOT NULL DEFAULT 'NONE'
                        CHECK (rollout_status IN ('NONE', 'IN_PROGRESS', 'HALTED')),
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    CHECK (autoscale_enabled = 0
           OR (min_replicas >= 1 AND max_replicas >= min_replicas
               AND target_cpu_percent > 0 AND target_cpu_percent < 100))
);

CREATE TABLE workloads (
    workload_id          TEXT PRIMARY KEY,            -- UUIDv7 (time-ordered)
    service_id           TEXT NOT NULL REFERENCES services(service_id) ON DELETE CASCADE,
    service_version      INTEGER NOT NULL,
    node_id              TEXT REFERENCES nodes(node_id) ON DELETE SET NULL,
    container_id         TEXT,
    desired_state        TEXT NOT NULL DEFAULT 'RUN' CHECK (desired_state IN ('RUN', 'STOP')),
    status               TEXT NOT NULL DEFAULT 'PENDING'
                         CHECK (status IN ('PENDING', 'ASSIGNED', 'RUNNING', 'STOPPING',
                                           'LOST', 'STOPPED', 'FAILED')),
    status_reason        TEXT,
    replaces_workload_id TEXT,                        -- set on LOST/rollout replacements
    restart_count        INTEGER NOT NULL DEFAULT 0,
    created_at           INTEGER NOT NULL,
    started_at           INTEGER,
    updated_at           INTEGER NOT NULL
);
-- Env vars, ports and volumes live in services.spec_json, not per workload.
-- A service row is deleted only after all its workloads are terminal (FR-CON-10),
-- so ON DELETE CASCADE removes history only, never live workloads.

CREATE TABLE scaling_events (
    event_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    service_id     TEXT NOT NULL REFERENCES services(service_id) ON DELETE CASCADE,
    from_replicas  INTEGER NOT NULL,
    to_replicas    INTEGER NOT NULL,
    metric_value   REAL,
    reason         TEXT NOT NULL,                     -- e.g. "cpu 91.2% > 82.5% for 120s"
    created_at     INTEGER NOT NULL
);

CREATE TABLE cluster_events (                         -- UI Events page, `valkvork events`, audit
    event_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    kind           TEXT NOT NULL CHECK (kind IN ('NODE', 'WORKLOAD', 'DEPLOY', 'SCALE', 'AUDIT')),
    subject_type   TEXT NOT NULL CHECK (subject_type IN ('node', 'service', 'workload')),
    subject_id     TEXT NOT NULL,
    message        TEXT NOT NULL,
    actor          TEXT NOT NULL,                     -- 'controller', 'cli', 'ui:<session-id>'
    created_at     INTEGER NOT NULL
);

CREATE INDEX idx_workloads_service_status ON workloads(service_id, status);
CREATE INDEX idx_cluster_events_time      ON cluster_events(created_at);
CREATE INDEX idx_workloads_node           ON workloads(node_id) WHERE node_id IS NOT NULL;
CREATE INDEX idx_workloads_status         ON workloads(status);
CREATE INDEX idx_nodes_class_status       ON nodes(node_class, status);
CREATE INDEX idx_scaling_events_service   ON scaling_events(service_id, created_at);
```

The scheduler and reconciler work from an in-memory snapshot of these tables, refreshed by the writer task after each commit. Indexes support startup loading and Admin API queries, not hot-path scheduling.

---

## 13. Non-functional requirements

| ID | Requirement | Measured by |
| :--- | :--- | :--- |
| **NFR-01** | Memory: `valkvork-agent` < **15 MB RSS** with 20 running workloads and no commands in flight. This excludes the container runtime (dockerd, containerd), which is budgeted under `[reserved]`. `valkvork-controller` < **50 MB RSS** at 50 nodes and 500 workloads, including the web UI with 10 open sessions but excluding SQLite page cache, which is capped by `PRAGMA cache_size`. | AT-11 |
| **NFR-02** | Blast radius: loss of the controller or of the network never stops, restarts or modifies a running workload (FR-AGE-05, FR-CON-04). | AT-03, AT-05 |
| **NFR-03** | Zero runtime dependencies: static musl binaries, `rustls` (the `ring` provider simplifies static musl builds), and SQLite compiled in (`rusqlite` with `bundled`). No glibc or OpenSSL on hosts. | `ldd` reports "not a dynamic executable" |
| **NFR-04** | Scale target (MVP): 50 nodes, 500 workloads, 50 services. | Load test |
| **NFR-05** | Latency: `apply` → `StartWorkload` dispatched in < 1 s (p99). Node failure → replacement dispatched in ≤ 15 s. | AT-02, AT-06 |
| **NFR-06** | Durability: an acknowledged `Apply` survives `kill -9` and power loss. Litestream RPO ≤ 10 s. | AT-03, AT-10 |
| **NFR-07** | Observability: JSON `tracing` logs. Prometheus `/metrics` on controller `:9400` and agent `127.0.0.1:9401`. Minimum metric set: `valkvork_nodes{status,class}`, `valkvork_workloads{status}`, `valkvork_pending_unplaced_workloads{class}`, `valkvork_allocated_ratio{class,resource}`, `valkvork_service_cpu_utilization{service}`, `valkvork_scaling_actions_total{service,direction}`, `valkvork_schedule_duration_seconds`, `valkvork_agent_reconnects_total`. | Metrics test |
| **NFR-08** | Platform: Linux x86_64, kernel ≥ 5.10, cgroup v2, Docker Engine ≥ 27.0 with `live-restore`. The native containerd driver (containerd ≥ 1.7) and aarch64 are [P2]. | CI matrix |

---

## 14. Acceptance tests (end-to-end)

| ID | Scenario | Pass criteria |
| :--- | :--- | :--- |
| AT-01 | Apply the same manifest twice. | Second response is `unchanged`. No DB writes, no commands sent. |
| AT-02 | Change replicas 1 → 3 (image cached). | 3 workloads `RUNNING` within 10 s. Spread across nodes. |
| AT-03 | `kill -9` the controller for 2 min, then restart it. | No container restarted (container IDs and start times unchanged). No duplicates. State converges within 60 s. |
| AT-04 | `systemctl restart valkvork-agent`. | Containers keep running with the same IDs and are re-adopted. |
| AT-05 | Partition a worker from the controller for 60 s (iptables). | Workloads become `LOST`. Replacements are running elsewhere within 15 s of detection. After heal, the lost copies stop. Final count equals the target exactly. Lost containers ran for the whole partition. |
| AT-06 | Terminate a worker VM. | Replacement dispatched in ≤ 15 s. |
| AT-07 | Hold CPU load above target. | Scale-up roughly 2 min in. No second action within 3 min. A `scaling_events` row is written. |
| AT-08 | Submit each invalid manifest class (VE-01…VE-07). | Correct error code and field path. CLI exit code 2. DB unchanged. |
| AT-09 | Change the image on a 3-replica service. | Workloads replaced one at a time. RUNNING count never drops below 3. A bad image halts the rollout with the old replicas still running. |
| AT-10 | Terminate the controller VM in AWS. | Replacement serving within 5 min. All state intact. Agents reconnect without config changes. |
| AT-11 | Measure memory at the NFR-01 load. | Within limits. |
| AT-12 | Drain a node during ASG scale-in. | Workloads migrate before termination. No capacity dip below the target. |
| AT-13 | Exceed the ephemeral disk limit. | Workload `FAILED` with reason `EPHEMERAL_DISK_EXCEEDED`, then replaced. |
| AT-14 | Clone two VMs from one image. | Distinct `node_id`s. |
| AT-15 | `systemctl restart docker` on a worker with running workloads. | Containers keep the same IDs and start times (`live-restore`). The agent re-adopts them. No workload goes `LOST`. |
| AT-16 | Use the web UI with all outbound network blocked from the browser host, except to the controller. | All pages load, with no request leaving for any other origin. CSP enforced. A drain, scale or apply from the UI produces the same state and `cluster_events` entry as the CLI. A mutating request without the CSRF header gets `403`. |

---

## 15. Delivery milestones

| Milestone | Scope | Exit criteria |
| :--- | :--- | :--- |
| **M0: Foundations** | Cargo workspace (`valkvork-proto`, `-common`, `-controller`, `-agent`, `-cli`). Proto definitions. TLS stream. Register and heartbeat. `node list`. Static musl CI build. | Agents register and appear healthy. AT-14. |
| **M1: Run workloads** | Manifest parse and validation, SQLite and migrations, apply/plan/delete, scheduler, `ContainerRuntime` trait plus `DockerRuntime` start/stop, scale up/down. | AT-01, AT-02, AT-08. |
| **M2: Resilience** | Node health, `LOST` and replacement, FR-CON-03 reconnect rules, recovery grace, adoption, orphans, local restart, backoff/`DEGRADED`. | AT-03 … AT-06, AT-15. |
| **M3: Change management + UI** | Rolling updates, drain, autoscaler, scaling and cluster events, ephemeral disk limits, host volumes, metrics. Built-in web UI: read-only pages first, then actions. | AT-07, AT-09, AT-12, AT-13, AT-16. |
| **M4: Production** | Packer image. Terraform (VPC, worker ASGs per class, controller ASG + EBS + ENI, Litestream/S3, LB target groups, metric publishing, lifecycle hooks). Runbooks. | AT-10, AT-11. NFR load test. |
| **P2** | Native containerd driver (FR-AGE-10), block volumes with fencing, mTLS per-node certs, secrets, health checks, dynamic ports and catalogue, batch/system jobs, RBAC, aarch64. | n/a |

---

## 16. Open decisions

Each item records the decision this draft made and the alternative. Confirm or change before M1.

1. **Runtime (decided):** Docker Engine (`docker-ce` + `containerd.io`) for MVP, accessed through the `ContainerRuntime` trait. Docker provides port publishing, registry auth and log rotation out of the box. A native containerd driver follows in P2 (FR-AGE-10) to remove dockerd's footprint.
2. **`replicas = 0` is valid** and means "defined, but scaled to zero". v0 rejected zero. *Alternative:* reject it and require `delete`.
3. **Local restarts happen in the agent** (Nomad-style) so they keep working during partitions. *Alternative:* every restart is a controller decision, which is simpler but means no restarts while the controller is down.
4. **LOST-copy reconciliation** is "stop the lost copy if its replacement is placed, otherwise re-adopt it" (FR-CON-03). *Alternative:* always keep the oldest copy.
5. **Ingress uses static host ports and health-checked target groups.** *Alternative:* the controller registers LB targets directly in MVP.
6. **Agent authentication in MVP uses join token plus server TLS,** with per-node mTLS in P2. *Alternative:* full mTLS in MVP.
7. **NVMe pooling (FR-STO-22):** build a distributed filesystem, or integrate an existing one. The recommendation is to integrate or skip.
8. **Cloud scope:** AWS-first, with GCP parity in P2. *Alternative:* both clouds for MVP.

---

## Appendix A: Changes from v0

* **One name:** "Aero" / `aero-agent` / `aero.v1` became Valkvork / `valkvork-agent` / `valkvork.v1`. Four documents were merged into one.
* **Contradictions fixed:**
  * CPU was a `uint32` in the proto but `REAL` (0.5) in the database. It is now integer millicores everywhere.
  * The proto workload statuses did not match the table statuses. There is now a single state machine (§3.1).
  * The agent stream used `http://` despite a TLS requirement. It is now `https` with a CA bundle.
  * v0 said the agent "exposes a local gRPC interface" but also received commands over the stream. Commands now use the stream only.
  * v0 called the scheduler "bin-packing" while describing least-allocated, which is the opposite strategy. It is now named spread.
  * v0 listed "complex constraints" as out of scope while the storage section required anti-affinity. Anti-affinity moved to P3.
  * Limits were enforced by "cgroups-rs" in one place and by the runtime in another. The runtime (Docker Engine) now enforces them.
* **Runtime:** v0 allowed "containerd or docker-ce". v1 starts with Docker Engine behind a runtime trait. It requires `live-restore` and disables Docker's own restart policy. A native containerd driver is planned for P2.
* **Gaps filled:**
  * Per-workload CPU telemetry, without which the autoscaler could not work.
  * Node class reporting.
  * Hostname and IP in registration.
  * Command acks and idempotency.
  * Controller restart grace period, which prevents mass rescheduling after a controller restart.
  * Partition duplicate cleanup.
  * Rolling updates.
  * Drain.
  * Autoscaler hysteresis, min/max, and how autoscaling interacts with re-applying a manifest.
  * Registry credentials.
  * Admin API and auth.
  * Ports and ingress.
  * The `machine-id` cloning hazard.
  * Delete semantics.
  * Data retention.
* **Deployment corrections:**
  * EBS does not detach and reattach automatically, and it is AZ-scoped. The controller now uses a boot script, a single-AZ ASG, Litestream to S3, and a fixed-IP ENI.
  * The RTO was revised from "60 s" to < 5 min.
  * Infrastructure scaling now uses Valkvork's allocation and pending metrics instead of host CPU.
* **Storage phased:** MVP covers ephemeral limits and host volumes. P2 covers block volumes, fencing and QoS. P3 covers pooling and out-of-band sync. Unverifiable claims such as "sub-millisecond" and "cryptographically isolated sockets" were replaced with concrete mechanisms.
* **Schema:** timestamps are now integer Unix ms (SQLite has no datetime type). Added `version`, `spec_hash`, `desired_state`, `rollout_status`, `draining`, `node_class`, `scaling_events`, `cluster_events` and migrations.
* **Built-in web UI** (FR-UI): embedded in the controller binary, with no third-party code. v0 listed a UI as out of scope.
* **Diagrams:** §2 now shows the big picture with numbered flows, the controller's internal loop, step-by-step sequence flows and an information-flow table. Env vars moved from each workload row to the service spec.
