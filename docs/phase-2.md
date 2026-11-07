# Phase 2 and Beyond

The [spec](../spec.md) tags each requirement with a phase. **[MVP]** is implemented (gaps are in [NEXT_STEPS.md](../NEXT_STEPS.md)). **[P2]** is the next phase: designed now, so the schema and protos leave room for it, but not built. **[P3]** is long-term research, not committed.

This page collects every P2 and P3 item, and for each one: what it is, what already exists to build on, a design sketch against the current code, and the questions to settle first. The sketches are starting points, not decisions.

## At a Glance

| Item | Spec | Phase | Builds on |
|---|---|---|---|
| [Network block volumes with fencing](#network-block-volumes-with-fencing) | FR-STO-10 … 15, §7.3 | P2 | Host volumes, the scheduler's filters, the agent's mount path |
| [Native containerd driver](#native-containerd-driver) | FR-AGE-10 | P2 | The `ContainerRuntime` trait |
| [Per-node mutual TLS](#per-node-mutual-tls) | FR-SEC-02, decision 6 | P2 | Server TLS and the join token |
| [Secret references](#secret-references) | FR-SEC-04 | P2 | `[env]`, the agent's start path |
| [Application health checks](#application-health-checks) | §1.4 | P2 | `WorkloadStatus`, rollouts' `min_healthy_time` |
| [Dynamic ports and load balancer registration](#dynamic-ports-and-load-balancer-registration) | FR-NET-03 | P2 | Static host ports, the scheduler's port filter |
| [Batch and system jobs](#batch-and-system-jobs) | §1.4 | P2 | The reconciler and the workload state machine |
| [RBAC and a read-only role](#rbac-and-a-read-only-role) | FR-UI-06, FR-SEC-03 | P2 | Admin token auth, UI sessions, the audit log |
| [Hard ephemeral disk quotas](#hard-ephemeral-disk-quotas) | FR-AGE-08 | P2 | The polling disk check |
| [aarch64](#aarch64) | NFR-08 | P2 | **Mostly done** |
| [GCP parity](#gcp-parity) | decision 8, §10 | P2 | The golden image's role bootstrap |
| [Topology anti-affinity](#topology-anti-affinity-and-failure-domain-spread) | FR-STO-21, §1.4 | P3 | The scheduler's score |
| [Out-of-band volume reconciliation](#out-of-band-volume-reconciliation) | FR-STO-20 | P3 | Block volumes |
| [Local NVMe pooling](#local-nvme-pooling) | FR-STO-22 | P3 | Host volumes |
| [Continuous controller backups (Litestream)](#continuous-controller-backups-litestream) | §10.3, NFR-06 | Later | Daily EBS snapshots of the controller volume |
| [Infrastructure autoscaling from controller metrics](#infrastructure-autoscaling-from-controller-metrics) | §10.4 | Later | The Prometheus metrics, `node drain`, `node remove`, per-class ASGs |
| [Automatic removal of dead nodes](#automatic-removal-of-dead-nodes) | FR-CON-08 | Later | `valkvork node remove` |

## Suggested Order

The [platform workloads plan](platform-workloads.md) takes precedence. It schedules secret references, health checks and batch jobs first, along with a few things not listed here: a bind address per port, `command` and `args`, and secrets inside values.

1. **The MVP gaps that P2 builds on are done:** node removal (`valkvork node remove`) and stop-first updates for single-writer volumes, which block volumes will reuse.
2. **Health checks.** Small, self-contained, and they make rollouts, drains and load balancer registration safer.
3. **Secret references**, then **per-node mTLS**: both close security gaps that production users hit first.
4. **Block volumes with fencing**: the largest item, and the one that makes stateful services practical.
5. **Dynamic ports and load balancer registration**, then **batch and system jobs**.
6. **RBAC**, then **containerd**, **hard disk quotas** and **GCP**, as demand appears.

---

## Network Block Volumes with Fencing

*FR-STO-10 … FR-STO-15, spec §7.3.* Today the only volumes are host directories that never move (see [placement-and-storage.md](placement-and-storage.md)). Phase 2 adds cloud block volumes, declared inline and provisioned during scheduling:

```toml
[[volumes]]
type   = "block"
name   = "orders-db"
size   = "100GiB"
class  = "gp3"
target = "/data"
# iops_limit = 5000      # FR-STO-15
# bps_limit  = "250MiB"
```

**Requirements**

- **Inline provisioning** (FR-STO-10): the volume is created on first placement, recorded in a `volumes` table, and reused by later placements.
- **Built-in drivers** (FR-STO-11) for AWS EBS and GCP Persistent Disk, compiled in: no CSI plugin jobs.
- **Atomic bind** (FR-STO-12): provision, attach and mount are part of the workload's start. If any step fails, roll back by detaching, delete the volume if it was created during this placement, and fail the workload with `VOLUME_ATTACH_FAILED`.
- **Fencing before reattach** (FR-STO-13): a single-writer volume is never attached to a new node until the cloud API confirms the old instance is stopped or terminated (terminating it if necessary), or the volume has been force-detached. Never a timeout alone.
- **Zone topology** (FR-STO-14): a volume's availability zone is a hard placement filter.
- **Block I/O QoS** (FR-STO-15): `iops_limit` and `bps_limit` written to cgroup v2 `io.max` for the volume's device.

**Design sketch**

| Area | Change |
|---|---|
| Manifest | Extend `RawVolume` in `valkvork-common/src/validation.rs` (it currently rejects `name`, `size`, `class`) with a `block` variant; validate size, class and limits |
| Schema | Migration 2: `volumes(volume_id, service_id, name, cloud_id, size, class, zone, attached_node_id, state, created_at, …)` |
| Nodes | `Register` gains `zone` and `instance_id` (new proto fields): the agent reads them from instance metadata |
| Controller | A `VolumeDriver` trait (`create`, `attach`, `detach`, `delete`, `describe_instance`), with an AWS implementation. The controller needs an IAM role for EC2 volume calls |
| Scheduler | New filter: same zone as the volume, if it exists; capacity for attachments per instance type |
| Reconciler | A per-workload volume step before `StartWorkload`: create if missing → fence the previous holder → attach → start. Rollback on failure |
| Agent | Find the device by volume id (on Nitro, the NVMe serial), `mkfs` only if blank, mount under `data_dir/volumes/<id>`, bind-mount into the container; unmount on stop |
| Updates | Stop-first rollouts and drains already exist for single-writer host volumes; a block volume is single-writer, so it reuses them |

**Questions to settle:** multi-attach (io2) or single-writer only; whether a volume outlives `delete service` (probably yes, with an explicit `delete volume`); snapshots and backups; how the controller authenticates to the cloud API in local development.

## Native containerd Driver

*FR-AGE-10.* A `ContainerdRuntime` talking to containerd's gRPC API directly, in its own `valkvork` namespace, removing dockerd from the node and its memory.

**Already there:** the agent reaches the runtime only through the `ContainerRuntime` trait (`valkvork-agent/src/runtime.rs`); no Docker type leaks out. `agent.toml` has `runtime`, which accepts only `"docker"` today.

**It must reach parity on:** image pull with registry auth, snapshot and unpack; OCI spec generation (limits, mounts, labels); networking through the CNI `bridge` and `portmap` plugins, including IP allocation and cleanup; stdout and stderr capture with rotation; event-driven exit detection; adoption by label.

**Exit criteria:** the acceptance suite passes unchanged on a containerd-only image, and the per-node memory saving is measured.

**Note:** containers cannot move between drivers (Docker uses containerd's `moby` namespace, the native driver `valkvork`). A node changes driver only by being drained and replaced with a new image.

## Per-node Mutual TLS

*FR-SEC-02, open decision 6.* Today agents verify the controller's certificate and authenticate with a shared join token. Phase 2 gives each node its own client certificate.

**Design sketch**

1. On first join, the agent generates a key and sends a CSR with its `Register` (new proto field), authenticated by the join token.
2. The controller, acting as a CA (for example with `rcgen`), signs a short-lived certificate whose subject is the `node_id`, and returns it in `RegisterAck`.
3. The agent stores it in `data_dir` and uses it as its client identity from then on; it renews before expiry.
4. The agent listener then requires client certificates, and the controller checks that the certificate's `node_id` matches the one in `Register`. The join token is needed only to enrol.
5. Removing a node (`valkvork node remove`) revokes its certificate.

**Questions:** where the CA key lives and how it is backed up; certificate lifetime; a migration window where both modes are accepted.

## Secret References

*FR-SEC-04.* Today `[env]` values are plaintext in SQLite, the Litestream replica, `valkvork status` and the UI, so manifests must not carry secrets. Phase 2 adds references that the agent resolves at start:

```toml
[secrets]
DB_PASSWORD = "ssm:/valkv/prod/orders/db_password"
API_KEY     = "secretsmanager:prod/orders/api-key"
```

**Design sketch:** the controller stores and shows only the reference. `StartWorkload` carries the references; the agent resolves them with the instance's role (AWS SSM / Secrets Manager, GCP Secret Manager) and passes the values as environment variables or files under `/alloc/secrets/`. Values are never logged, never reported back, and are redacted in the UI. A changed reference is a replacing change; a changed *value* in the secrets store is not seen until the workload restarts.

**Questions:** environment variables or files (files can be rotated without a restart); caching on the agent while the secrets store is unreachable, so a partition does not stop local restarts; how a rotation triggers a rollout.

## Application Health Checks

*Spec §1.4.* Today liveness means "the container process is running".

```toml
[check]
type     = "http"          # or "tcp"
port     = "http"          # a [[ports]] name
path     = "/healthz"
interval = "5s"
timeout  = "2s"
failures = 3
```

**Design sketch:** the agent runs the checks against the container's address, and adds `healthy` to `WorkloadStatus` (a new proto field). Effects:

- a rollout or drain waits for the new workload to be **healthy** for `min_healthy_time`, not just running;
- repeated failures restart the container in place under the restart policy, as an exit does;
- the controller exposes health in the API, UI and metrics, and load balancer registration (below) only registers healthy targets.

**Questions:** separate readiness and liveness; checks during a partition (they keep running locally, by FR-AGE-05's spirit); exec checks.

## Dynamic Ports and Load Balancer Registration

*FR-NET-03.* Today ports are static, so a service with a `host_port` runs at most one replica per node, and the load balancer finds replicas by health-checking every worker on that port.

**Design sketch**

- `host_port = 0` lets Docker (or CNI `portmap`) pick a free port; the agent reports the actual port in `WorkloadStatus` (new field), and the scheduler stops treating such ports as a per-node limit.
- A **service catalogue** in the admin API: `ListEndpoints(service)` returns `node_ip:port` for RUNNING (and, with health checks, healthy) workloads.
- **Direct target registration:** the controller adds and removes those targets in the load balancer (for AWS, ELBv2 `RegisterTargets` / `DeregisterTargets`) as workloads start and stop, draining before deregistration. This removes the health-check lag and allows several replicas per node.

**Questions:** one target group per service or per port; deregistration delay versus `kill_timeout`; what the controller does when the load balancer API is unavailable (it must not block the reconciler).

## Batch and System Jobs

*Spec §1.4.* Today every service is a long-running `service`: an exit, even with code 0, is a failure.

| Type | Behaviour |
|---|---|
| `batch` | Runs to completion. Exit 0 is success: a new terminal status `COMPLETED`, no restart. Optional `parallelism` and `completions`; a failed task retries under the restart policy |
| `system` | One workload on every eligible node, following nodes as they join and leave (like Nomad `system` jobs). `replicas` does not apply |

**Design sketch:** `service.type` in the manifest; a migration for the new status (the `workloads.status` CHECK constraint); the reconciler counts slots per node for `system`, and stops creating replacements for completed `batch` work. Retention needs a rule for finished batch runs.

## RBAC and a Read-only Role

*FR-UI-06, FR-SEC-03.* Today one admin token grants everything, in the CLI and the UI.

**Design sketch:** named tokens with roles, configured in `controller.toml` (for example `admin` and `viewer`), checked per RPC in the admin interceptor and per endpoint in the UI. The audit log records the token's name instead of `cli`. A viewer session in the UI hides the action buttons, and the server refuses the calls regardless.

## Hard Ephemeral Disk Quotas

*FR-AGE-08.* Today the `disk` limit is enforced by polling: the alloc directory every 10 s and the writable layer every 60 s, with a soft margin.

**Design sketch:** on overlay2 over XFS mounted with `pquota`, pass Docker's `--storage-opt size=` for the writable layer and an XFS project quota for the alloc directory. That needs XFS for the data volume; the golden image formats `/data` as ext4 today. Keep the polling check as a fallback where quotas are unavailable.

## aarch64

*NFR-08 lists aarch64 as P2, but it is mostly done.* The release workflow builds static binaries and images for arm64 on native runners, the golden image builds arm64 AMIs, and everything in local development runs on arm64 (Apple silicon with Colima). Remaining: an arm64 Linux worker in the CI matrix, with real cgroup telemetry, and a run of the acceptance tests there.

## GCP Parity

*Open decision 8, spec §10.* The MVP is AWS-first. GCP needs: a stateful managed instance group of size 1 for the controller with a persistent disk and a fixed internal IP; Secret Manager instead of SSM in the golden image's bootstrap; Persistent Disk in the block-volume driver; Cloud Monitoring for the infrastructure scaling loop. The golden image already has OCI sources; a GCP source would follow the same pattern.

---

## Later: Only If Needed

Not scheduled. Each one replaces something that works well enough today, so it is built only when a real need appears.

### Continuous Controller Backups (Litestream)

*Spec §10.3, NFR-06.* **Today:** the cell takes a daily EBS snapshot of the controller's data volume (goldenimage `controller_backup_*`, 7 kept). A restore loses up to a day of desired state: services, versions, replica targets. That is acceptable because the database holds no customer data. Workloads keep their data on the agents, and services can be re-applied from their manifests.

**What Litestream would add:** it streams the SQLite WAL to a versioned S3 bucket, so at most a few seconds are lost, with point-in-time restore. A replacement controller in another AZ could restore on its own at boot (`litestream restore` when the volume is missing or fails `PRAGMA integrity_check`).

**Build it when** losing a day of changes becomes unacceptable, for example when many teams deploy often, or when the controller must recover across AZs without an operator.

**Sketch:** add the `litestream` binary and a systemd unit to the golden image, replicating `/data/valkvork/db/valkvork.db`; an S3 bucket with versioning and a lifecycle rule; IAM for the controller role; and the restore step in the bootstrap, before the controller starts.

### Infrastructure Autoscaling from Controller Metrics

*Spec §10.4, loop B.* **Today:** nodes are added and removed by changing the cell's instance count. The application loop (loop A, the autoscaler) works within the capacity that exists.

**What it would add:** per-class worker ASGs that scale from the controller's own signals, published to CloudWatch:
- scale out when `valkvork_pending_unplaced_workloads{class} > 0` for 1 minute, or `valkvork_allocated_ratio{class}` is above 0.8;
- scale in when the ratio stays below 0.4 for 15 minutes.

Every scale-in and instance refresh would pass through an ASG lifecycle hook that runs `valkvork node drain`, waits until the node is empty, lets termination continue, then runs `valkvork node remove`. The admin API listens on the controller's localhost, so the hook runs those commands on the controller, for example through SSM Run Command.

**Build it when** load changes faster than an operator can change node counts. It needs the per-class ASGs (NEXT_STEPS) first.

### Automatic Removal of Dead Nodes

*FR-CON-08 (node removal).* **Today:** a terminated worker stays UNHEALTHY in the node list, its LOST workloads already replaced elsewhere, until an operator runs `valkvork node remove <id>` or clicks **Remove** in the UI. Nothing breaks while it is listed; it is clutter.

**What it would add:** an optional controller setting, for example `remove_unhealthy_nodes_after_hours = 24`, that removes a node once it has been UNHEALTHY that long, exactly as `node remove` does: its LOST workloads become STOPPED (`NODE_REMOVED`), the row is deleted, and an AUDIT event is written with actor `controller`. If the node comes back, it joins as a new node.

**Build it when** nodes are replaced often enough that removing them by hand is a chore, and the scale-in hook above is not in place to call `node remove` itself.

## Phase 3 (Research)

### Topology Anti-affinity and Failure-domain Spread

*FR-STO-21, spec §1.4.* "Replicas must not share a failure domain", per zone, rack or host. Today the scheduler spreads by allocation ratio only, and same-service count is merely a tie-break. A first step would be to report a `zone` per node (needed for block volumes anyway) and add a score term, or a hard filter, for replicas already in that zone.

### Out-of-band Volume Reconciliation

*FR-STO-20.* Detect cloud-side volume resizes and snapshots and record them without restarting workloads. Depends on block volumes.

### Local NVMe Pooling

*FR-STO-22.* Discover local NVMe devices and expose them as managed host volumes or LVM pools. The spec recommends against building a distributed filesystem; if pooling is wanted, integrate an existing system such as Ceph. Decision tracked in spec §16.
