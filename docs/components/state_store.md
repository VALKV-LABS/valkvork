# Component: State Store (SQLite)

## High-Level Function

Desired state lives only in SQLite on the controller (FR-ARC-03). The code is in `crates/valkvork-controller/src/db.rs`. SQLite is compiled into the binary (`rusqlite` with `bundled`), so the host needs no database library.

| Rule | How |
|---|---|
| Durable writes (FR-CON-01) | WAL journal, `synchronous = FULL`; every commit is fsync'd before the controller replies |
| One writer | The core task owns the only write connection (`Db`) |
| Reads do not contend with the writer | History queries use a separate read-only connection (`EventReader`) |
| Liveness never hits the disk per heartbeat | Heartbeat times live in memory and are flushed every 30 s and on status changes |
| Versioned schema | `schema_migrations` plus an append-only list of migrations |
| Bounded page cache | `cache_size = -8192` (8 MiB) |

Every connection sets `foreign_keys = ON`, `synchronous = FULL` and `busy_timeout = 5000`. `journal_mode = WAL` is set once and persists in the file.

Keep the database on block storage. Never put it on NFS, EFS or another network filesystem.

## Schema

The schema is spec §12, created by migration 1.

| Table | Key columns | Notes |
|---|---|---|
| `nodes` | `node_id`, `hostname`, `ip_address`, `node_class`, `agent_version`, `runtime`, allocatable CPU and memory, `host_volumes` (JSON), `status`, `draining`, `last_heartbeat_at` | Upserted on every Register; deleted by `node remove`, which sets `node_id` to NULL on the node's remaining workload rows |
| `services` | `service_id`, `version`, `spec_json`, `spec_hash`, `image`, CPU and memory, `required_class`, `target_replicas`, autoscale bounds, `last_scaled_at`, `status`, `rollout_status` | `spec_json` is the canonical manifest; `spec_hash` its SHA-256 |
| `workloads` | `workload_id` (UUIDv7, time-ordered), `service_id`, `service_version`, `node_id`, `container_id`, `desired_state`, `status`, `status_reason`, `replaces_workload_id`, `restart_count`, timestamps | `ON DELETE CASCADE` from services |
| `scaling_events` | `service_id`, from/to replicas, `metric_value`, `reason` | Autoscaler actions |
| `cluster_events` | `kind` (NODE, WORKLOAD, DEPLOY, SCALE, AUDIT), `subject_type`, `subject_id`, `message`, `actor` | The Events page and `valkvork events` |
| `schema_migrations` | `version`, `applied_at` | |

`CHECK` constraints enforce the enums, positive capacities and, for autoscaled services, `1 ≤ min ≤ max` and `0 < target < 100`. Timestamps are Unix epoch milliseconds.

A service row is deleted only once all its workloads are terminal, so the cascade removes history, never live workloads.

## Writing

The core collects everything one step changed into a `Batch` and calls `Db::commit`, which writes it in a single transaction, in this order:

1. nodes (upsert), then heartbeat times;
2. services (upsert);
3. workloads (upsert), then deleted workloads;
4. scaling events, then deleted services (so a scaling event is never written for a service deleted in the same batch);
5. cluster events.

An empty batch writes nothing.

## Reading

At startup the core loads all nodes, services and workloads into memory, and from then on reads only from memory. The only reads that go to SQLite at runtime are history queries, through `EventReader`:

- `events(kind, since, limit)`: cluster events, newest first. Used by `ListEvents`, the Events page and the overview's recent events.
- `scaling_events(service_id, limit)`: used by `GetService` and the service page.

`EventReader` is a read-only connection with its own `busy_timeout`. Its queries run on Tokio's blocking pool, so they never stall the core.

## Migrations

`MIGRATIONS` in `db.rs` is a list of `(version, SQL)`. At open, the controller applies every migration newer than `MAX(version)` in `schema_migrations`, each in its own transaction, and records it. Never edit a migration that has shipped; add a new one.

## Retention

Every 10 minutes the core prunes history (FR-CON-11):

- terminal workloads (STOPPED, FAILED) are deleted when they are both older than 24 h and not among the newest 10 for their service;
- `cluster_events` and `scaling_events` older than 30 days are deleted.

## Backups

Backups are not part of the controller. On the golden image, the cell takes a daily EBS snapshot of the volume that holds the database (7 kept). A snapshot is crash-consistent, which SQLite in WAL mode recovers from like a power loss, and a restore loses up to a day of changes (spec §10.3). Continuous WAL streaming with Litestream is deferred until needed; see [phase-2.md](../phase-2.md#continuous-controller-backups-litestream).
