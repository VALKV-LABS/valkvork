# Placement and Storage

This guide covers how to control where a service runs, and how to give it storage that outlives a container. It describes what Valkvork supports today, what happens when a node fails, and what it deliberately does not do yet. The mechanisms are documented in [scheduler.md](components/scheduler.md) and [supervisor.md](components/supervisor.md).

## Choosing Nodes: Node Classes

Each agent has exactly one label, `node_class`, set in its `agent.toml`. A manifest can require it:

```toml
# agent.toml on the database VMs
node_class = "db"
```

```toml
# the manifest (examples/pinned.toml)
[constraints]
required_class = "db"
```

The service is then placed only on HEALTHY, non-draining nodes of class `db`, spread across them by the [scheduler](components/scheduler.md). Without `required_class`, any node will do.

When no node of the class exists or has room, the workload stays PENDING with a reason, and apply and plan warn:

```
$ valkvork apply -f examples/pinned.toml
service "pinned-worker": created (version 1)
  warning: UNSATISFIABLE_NOW at resources: no current healthy node (class db) can fit one replica or provide its host volumes

$ valkvork workloads --service pinned-worker
WORKLOAD   SERVICE        VER  NODE  STATUS   REASON
01a1a981…  pinned-worker  1    -     PENDING  NO_NODE: no healthy nodes in class db
```

It is placed as soon as a node of class `db` registers.

### Pinning to One Specific VM

There is no per-node pinning, and a node has only one label. To pin a service to a particular VM, give that VM a class of its own:

```toml
node_class = "db-1"      # agent.toml on that one VM
```

```toml
[constraints]
required_class = "db-1"
```

Host volumes pin too: a service that mounts a host volume only runs on nodes that declare it (below).

### Not Supported

The spec lists these as non-goals for this release (§1.4): multiple labels or tags per node, affinity and anti-affinity ("not on the same node as X"), taints and tolerations, and spreading across failure domains. Node class is the only placement constraint. Replicas of one service are spread across eligible nodes anyway, by the scheduler's tie-break.

## Storage

| Kind | Status | Data lives | Survives |
|---|---|---|---|
| Container filesystem | Built in | The container's writable layer | Restarts of the same container; not a new workload |
| `/alloc` (ephemeral disk) | Every workload | `<data_dir>/alloc/<workload_id>/` on the node, limited by `resources.disk` | Restarts of the same container; deleted when the workload stops |
| **Host volume** | Supported | A directory on one node, declared in `agent.toml` | New containers and new workloads **on the same node** |
| Network block volume (EBS, Persistent Disk) | **Not built** (spec P2, FR-STO-10…15) | | |

### Host Volumes

1. Declare the directory on the node, by name, in `agent.toml`:

   ```toml
   [host_volumes.pgdata]
   path      = "/data/pgdata"
   read_only = false
   ```

   The agent reports the names in `Register`; `valkvork node list -o json` and the Nodes page show them.

2. Mount it by name in the manifest (`examples/postgres.toml`):

   ```toml
   [service]
   id       = "postgres-main"
   image    = "docker.io/library/postgres:16.4"
   replicas = 1

   [[volumes]]
   type   = "host"
   source = "pgdata"                       # the name from agent.toml
   target = "/var/lib/postgresql/data"
   # read_only = true                      # optional
   ```

The scheduler only places the workload on nodes that declare **every** volume it mounts. The agent bind-mounts the directory into the container, read-only if either the manifest or `agent.toml` says so. The container needs no privileges; the agent, running as root, does the mount. The Docker socket, Docker's data directory and containerd's directories can never be mounted, even through a host volume or a symlink.

On the golden image, declare volumes with `VALKVORK_HOST_VOLUMES=pgdata=/data/pgdata` in user data. Put them on the data volume (`/data`), not the root disk.

### Data Is Node-Local

Valkvork never moves, copies, detaches or reattaches storage, and makes no cloud API calls. The directory belongs to one VM:

| Event | What happens |
|---|---|
| The container crashes | Restarted in place by the agent: same container, same data |
| A new workload on the same node (redeploy, delete and re-apply, a replacement placed on the same node) | The new container mounts the same directory and sees the data |
| The node becomes UNHEALTHY | The workload goes LOST. Its replacement needs a node that declares the volume. If only that node does, the replacement stays PENDING (`NO_NODE: host volume pgdata missing on …`) |
| That node comes back | The original container is still there with its data, so it is **re-adopted** and the pending replacement cancelled (FR-CON-03) |
| The node is terminated for good | The data is gone with it. The replacement waits for a node that declares the volume, and starts with whatever is in that node's directory, usually nothing |

> **Use a volume name only one node declares** for single-writer data such as a database, or pin the service with a unique `required_class`. If two nodes both declared `pgdata`, a replacement after a failure could start on the other node, against a different, probably empty, directory.

Keep `replicas = 1` for services on a single-writer host volume: every replica on a node shares the same directory.

### Rolling Updates of a Host-Volume Service

**Limitation.** Rolling updates and drains start the new workload *before* stopping the old one, so the RUNNING count never dips (FR-CON-07). A service whose host volume exists on only one node has nowhere to put that new workload. So a "replacing" apply (new image, env or resources) leaves the new workload PENDING and the old one running; the rollout does not complete. With a static host port it shows `NO_NODE: host_port 15432 in use`. Start-new-first is also the wrong order for a single-writer data directory, because two processes would share it for a moment.

Until this is addressed (see [NEXT_STEPS.md](../NEXT_STEPS.md)), update such a service by deleting it and applying the new manifest. The data stays in the directory:

```bash
valkvork delete service postgres-main      # wait until `valkvork status` no longer lists it
valkvork apply -f postgres.toml            # the new container reads the existing data
```

### Draining a Node with Host Volumes

A drain moves workloads to nodes that declare the same volumes. If none does, the workload stays where it is and its replacement stays PENDING, so the drain never completes. That protects the data, but means such nodes need their data moved by you before they are replaced.

### What Cloud Volumes Would Add (Not Built)

The spec's phase 2 (§7.3) describes network block volumes declared inline:

```toml
[[volumes]]          # NOT SUPPORTED YET: fails validation
type   = "block"
name   = "orders-db"
size   = "100GiB"
class  = "gp3"
target = "/data"
```

It would create the volume on first placement, attach and mount it as part of the workload's start, and, before attaching it to a new node after a failure, **fence** the old node: confirm through the cloud API that the old instance is stopped or terminated, or force-detach the volume. The availability zone would become a placement constraint. None of that exists today: such a manifest fails validation, with `UNKNOWN_FIELD at volumes.name`.

## Try It Locally

`make dev-up` declares two host volumes on the local node, `site` and `pgdata`, under `.dev/volumes/`. Set `VALKVORK_DEV_HOST_VOLUMES` (comma separated) and `VALKVORK_DEV_NODE_CLASS` to change them; run `make dev-down` and `make dev-up` again to apply.

```bash
make dev-up && eval "$(make -s env)" && alias valkvork=target/debug/valkvork

# A host volume, read-only, served by nginx
valkvork apply -f examples/static-site.toml
curl http://localhost:18081/
echo '<h1>edited on the host</h1>' > .dev/volumes/site/index.html
curl http://localhost:18081/                     # served immediately

# Data that outlives the workload
valkvork apply -f examples/postgres.toml
docker exec $(docker ps -q --filter label=valkvork.service_id=postgres-main) \
  psql -U postgres -c "create table notes(t text); insert into notes values ('hello')"
valkvork delete service postgres-main            # wait until it is gone
valkvork apply -f examples/postgres.toml         # a new workload and container
docker exec $(docker ps -q --filter label=valkvork.service_id=postgres-main) \
  psql -U postgres -c "select * from notes"      # → hello

# Pinning: stays PENDING on a "general" node
valkvork apply -f examples/pinned.toml
valkvork workloads --service pinned-worker
make dev-down && VALKVORK_DEV_NODE_CLASS=db make dev-up   # now it is placed
```

The first Postgres start initialises the data directory, which takes about 30 s.
