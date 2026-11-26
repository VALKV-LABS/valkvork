# Block Volumes (Design)

*Spec §7.3, FR-STO-10 … FR-STO-14. Status: built, stages 1 to 4. Not built: I/O limits (FR-STO-15), resizing, snapshots, multi-attach.*

Host volumes are directories that never leave their node, so a stateful service is pinned to one VM: if it dies, the service waits for it (see [placement-and-storage.md](placement-and-storage.md)). Block volumes are cloud disks (AWS EBS first) that Valkvork creates, attaches to whichever node runs the workload, and moves to another node when that one fails, without ever letting two nodes write to the same disk.

## In a Manifest

```toml
[[volumes]]
type   = "block"
name   = "orders-db"                  # cluster-wide; the volume outlives the service
size   = "100GiB"
class  = "gp3"                        # gp3 (default), gp2, io1, io2, st1, sc1
target = "/var/lib/postgresql/data"
# iops       = 6000                   # gp3: 3000-16000, at most 500 per GiB
# throughput = "250MiB"               # gp3: 125-1000 MiB/s, at most iops / 4
```

- **Single-writer, always.** One live workload uses a block volume at a time, cluster-wide. `read_only` and `single_writer = false` are rejected. The existing stop-first updates and drains apply unchanged: the old workload stops and its container is gone before the new one starts.
- **Created on first placement** (FR-STO-10), in the zone of the node chosen, and reused by every later placement: by name, so a replacement, a rolling update or a redeployed service finds the same data. Two services naming the same volume share it, one writer at a time, like a host volume.
- **Outlives its service.** `valkvork delete service` keeps the volume; `valkvork volume delete <name>` destroys it, and refuses while a workload uses it.
- `size` and `class` apply when the volume is created; changing them later is refused (resizing is not in scope).
- A workload with a block volume needs an agent with the `block_volumes` feature.

## Decisions

| Question | Decision | Why |
|---|---|---|
| Who talks to the cloud | The **controller**, for create, attach, detach, fence and delete | Fencing needs one authority that knows who held the volume last; agents only see their own node |
| Who handles the device | The **agent**: find it, `mkfs` if blank, mount, bind into the container, unmount | Only the node sees its block devices |
| Cloud client | A **small EC2 Query API client** in the controller: SigV4 with `ring` (already a dependency), credentials from the instance role (IMDSv2), the actions below only | Keeps static musl binaries and NFR-01's memory budget; the AWS SDK for Rust is large and pulls in its own HTTP and TLS stacks |
| Fencing | **Force-detach**, never a timeout alone | A standard EBS volume attaches to one instance at a time, so detaching it from the old instance is what stops that instance writing. Terminating the instance can be added as an option |
| Delete service | **Keeps** its volumes | Data loss must be an explicit act |
| Multi-attach (io2), snapshots, resize, GCP | **Not now** | Single-writer covers the platform's stateful services; the rest can follow |

## The Pieces

```
            manifest ──► controller core (actor, owns the volumes table)
                              │  VolumeIntent: create / attach / detach / fence
                              ▼
                      volume manager task ──► VolumeDriver (aws-ebs | local)
                              │  VolumeResult (as a core message)
                              ▼
            StartWorkload { block volumes } ──► agent: find device, mkfs?, mount, bind
```

### Controller: the core decides, a task acts

The core is one actor that handles each message in microseconds, so it never waits on the cloud. It owns a `volumes` table and decides what should happen; a **volume manager** task runs the calls through a `VolumeDriver` and reports back with `CoreMsg::VolumeResult`, like agent messages. Every transition is written to SQLite before it is acted on, so a controller restart resumes: on start the core re-sends the intent for every volume in a transitional state, and drivers make each call idempotent (they describe the volume first).

```rust
#[async_trait]
trait VolumeDriver: Send + Sync {
    async fn create(&self, name: &str, size_bytes: u64, class: &str, zone: &str) -> Result<CloudVolume>;
    async fn describe(&self, cloud_id: &str) -> Result<CloudVolume>;        // state, zone, attachment
    async fn attach(&self, cloud_id: &str, instance_id: &str) -> Result<()>;
    async fn detach(&self, cloud_id: &str, instance_id: &str, force: bool) -> Result<()>;
    async fn delete(&self, cloud_id: &str) -> Result<()>;
    async fn instance_state(&self, instance_id: &str) -> Result<InstanceState>; // running, stopped, terminated, ...
}
```

Drivers:

- **`aws-ebs`**: EC2 `CreateVolume` (tagged `valkvork:volume=<name>`, `Cell=<cell>`, encrypted), `DescribeVolumes`, `AttachVolume`, `DetachVolume` (`Force` for fencing), `DeleteVolume`, `DescribeInstances`. The controller's instance role needs exactly those on volumes tagged for its cell (an IAM change in valkv-platform-infra).
- **`local`**: a volume is a directory under a shared path; attach and detach are records; `instance_state` reads a file the test controls. It runs the whole state machine, the scheduler and the agent's mount path in `make e2e` and in unit tests, without AWS.

### Volume states

```
            create            attach(node)                 detach
  (none) ──────────► AVAILABLE ───────────► ATTACHED(node) ───────► AVAILABLE
     │ CREATING          ▲   ATTACHING          │ node UNHEALTHY and the
     │                   │                      │ workload must move:
     └─► FAILED          └──── DETACHING ◄──────┘ FENCING (force-detach)
```

| State | Meaning |
|---|---|
| `CREATING` | `CreateVolume` sent; the zone is the placed node's |
| `AVAILABLE` | Exists, attached nowhere |
| `ATTACHING(node)` | `AttachVolume` sent to the node's instance |
| `ATTACHED(node)` | Attached; the node may mount it |
| `DETACHING(node)` | Normal detach, after the agent reported it unmounted |
| `FENCING(node)` | Force-detach from a node that is gone or partitioned |
| `DELETING` | `valkvork volume delete` |

### A workload's start, with a block volume

1. The **scheduler** places the workload only on a node that (a) supports `block_volumes`, (b) is in the volume's zone, if the volume exists, (c) has a free attachment slot, and (d) when no other live workload, on any node, uses the volume. The workload becomes ASSIGNED, as now.
2. The **core** sees an ASSIGNED workload whose volumes are not `ATTACHED` to its node and asks the manager to create and/or attach them. No `StartWorkload` is sent yet; the start timeout covers the wait.
3. When every volume is `ATTACHED` to the node, the core sends `StartWorkload`, which carries each block volume: name, cloud id, target.
4. The **agent** finds the device by its id (on Nitro, `/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_vol…`), runs `mkfs.ext4` only if `blkid` finds no filesystem, mounts it at `<data_dir>/block/<name>` (`noatime`), and bind-mounts that into the container. On stop it unmounts after the container is gone and reports the volume released.
5. **Atomic bind** (FR-STO-12): if create, attach or mount fails, the workload FAILS with `VOLUME_ATTACH_FAILED: <cause>` and the service's backoff applies. A failed create leaves no row (drivers find a half-created volume by name on the retry). A failed attach leaves the volume AVAILABLE, unattached, recorded with its error, and the next placement reuses it: the table keeps it, so nothing is orphaned, and a volume that may already hold data is never deleted automatically.
6. A failed detach or fence leaves the volume ATTACHED to its old node, so nothing else attaches it, and is tried again after 10 s (`VOLUME_RETRY`).

The volume **stays attached** after the workload stops: a restart, or a stop-first update on the same node, takes seconds. It is detached only when a workload on another node needs it, or when it is deleted.

### Fencing (FR-STO-13)

A new placement on node B needs a volume still attached to node A:

| Node A | Then |
|---|---|
| HEALTHY, its workload stopped and the volume released | Normal detach, then attach to B |
| UNHEALTHY; `DescribeInstances` says stopped or terminated | Detach (force if EC2 still lists the attachment), then attach to B |
| UNHEALTHY; the instance is still running (a partition) | **Force-detach**, then attach to B. From then on A cannot write to the disk; when A reconnects, the inventory rules (FR-CON-03) stop its old container, and its agent unmounts the dead device |
| The cloud call fails | Stay in `FENCING`, retry with backoff, and never attach to B |

The controller never attaches to B because of a timeout alone. A replacement for a workload on an UNHEALTHY node is placed as today (after the node is marked UNHEALTHY); fencing then adds the time EC2 takes to detach (seconds to a minute).

### Topology

Each agent reports, in `Register`:

| Field | From | Used for |
|---|---|---|
| `zone` | IMDS `placement/availability-zone`, or agent.toml `zone` | The volume's zone filter (FR-STO-14) and where a new volume is created |
| `instance_id` | IMDS `instance-id`, or agent.toml `instance_id` | Attach, detach and fencing |
| `max_block_volumes` | agent.toml `block_volumes.max` (default 20) | The attachment-slot filter: Nitro instances allow about 28 attachments, network interfaces and the root disk included |

### Agent

- New feature `block_volumes`; a `[block_volumes]` section in agent.toml (`enabled`, `max`, `fs = "ext4"`, `mount_dir`).
- Device lookup with a timeout (attachments take a few seconds to appear); `mkfs` only on a device with no filesystem signature, so it never formats data.
- Reports mounted block volumes in `Register` and heartbeats, so the controller knows when one is released.
- Refuses to mount a device that is mounted elsewhere on the node.

### The EBS Driver (stage 3)

- **The client** (`crates/valkvork-controller/src/aws`): EC2 Query API requests signed with SigV4 (`ring`), over HTTPS with the Mozilla roots compiled in (`webpki-roots`), so the controller image needs no certificate store. Credentials come from `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, else the instance role through IMDSv2, refreshed five minutes before they expire. Responses are parsed by a small nesting-aware XML reader.
- **Create** looks for the cell's volume by its tags first, then calls `CreateVolume` (encrypted, `ClientToken` `vk-<cell>-<name>`, tags `valkvork:volume`, `valkvork:cell`, `Name`) and waits until it is available. **Attach** picks a free device name (`/dev/sdf` … `/dev/sdz`) from `DescribeInstances` and waits until EC2 reports it attached. **Detach** (with `Force` for fencing) waits until the volume is available. Each wait polls every 2 s for up to 180 s.
- **The agent** finds the device by the volume id (`/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_vol…`, or an NVMe controller whose serial is the id), waits up to 60 s for it, runs `mkfs` only when `blkid` finds no filesystem, mounts it at `<data_dir>/block/<name>` with `noatime`, and bind-mounts that into the container. After the last container using it is gone it unmounts it; a mounted volume counts as in use, so the controller never detaches it under a filesystem. Mounts and unmounts are serialised, so a stop-first update on the same node never reuses a mount that is being taken down.
- **IAM** (goldenimage `block_volumes_enabled`): the controller's role may create volumes only with its cell's `valkvork:cell` tag, attach, detach and delete only volumes with that tag and only on its cell's instances, and use KMS only through EC2.

### Operations (stage 4)

- `valkvork volume list` and the UI's **Volumes** page show every volume: state, size, class and provisioning, zone, node, the services that name it, cloud id and the last error.
- `valkvork volume delete <name> --yes` (or **Delete** on the page, confirmed by typing the name) destroys a volume: DELETING, then the manager detaches it (forced if its node is gone) and deletes it, and the row goes. It is refused while a service names it (the next placement would create it again), while it is mid-operation, or while a healthy node still has it mounted. A failed delete leaves it as it was, with the error. Recorded in the audit log.
- gp3 `iops` and `throughput` are applied at creation. Changing them, or `size` or `class`, later does not change an existing volume.
- Metrics: `valkvork_block_volumes{state}` and `valkvork_volume_operations_total{op,result}`.

## Stages

| Stage | Contents | Needs AWS |
|---|---|---|
| **1. Topology and model** | Agents report `zone`, `instance_id`, `max_block_volumes`; `type = "block"` in manifests (validation, `block_volumes` feature, always single-writer); the `volumes` table; the scheduler's zone, slot and cluster-wide single-writer filters; the `VolumeDriver` trait and the `local` driver. No agent advertises `block_volumes` yet, so such workloads stay PENDING with a clear reason | No |
| **2. Volume manager and agent mount** | The state machine, intents and results, start gating, rollback (`VOLUME_ATTACH_FAILED`), resume after a controller restart; `StartWorkload` with block volumes; the agent's device lookup, `mkfs`, mount, unmount and release reports; `make e2e` with the `local` driver | No |
| **3. EBS and fencing** | The EC2 client (SigV4, IMDSv2 credentials), the `aws-ebs` driver, fencing; IAM for the controller's role in goldenimage (`block_volumes_enabled`; valkv-platform-infra's `platform-cell` still needs the same); verified on a dev cell: create, move after a node is stopped, force-detach from a partitioned node | Yes |
| **4. Operations** | `valkvork volume list/delete`, the UI's Volumes page, gp3 `iops`/`throughput`, metrics, docs. I/O limits (FR-STO-15) are left for later | No |

## Verified

On AWS on 2026-11-25 (cell `aws-us-west-1-valkvork`, this code baked into the golden image): agents reported `us-west-1a` and their real instance ids from IMDSv2; with the `local` driver, a block volume was created in the node's zone, attached to its instance, mounted read-write, and kept its data across a stop-first update. Moving a volume between nodes and fencing need real disks (stage 3) and are covered by unit tests until then.

## Open Questions

- **Delete with the service.** A `[[volumes]] delete_with_service = true` for scratch data, later, if wanted.
- **Encryption keys.** Encrypted with the account's default EBS key; a `kms_key` setting if a customer needs their own.
- **Snapshots.** Daily snapshots of every Valkvork volume through AWS Data Lifecycle Manager on the `valkvork:volume` tag, set up in infrastructure, before building anything in Valkvork.
