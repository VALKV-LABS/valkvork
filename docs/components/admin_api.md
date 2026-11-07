# Component: Admin API (`AdminService`)

## High-Level Function

`AdminService` is the controller's gRPC API for operators, CI and the `valkvork` CLI (FR-CON-10, spec §11.2). It is served on `admin_listen_addr` (`:50052`), the same listener as the web UI; requests with `content-type: application/grpc` go to this service and everything else to the UI. The proto is `crates/valkvork-proto/src/admin.v1.proto` (package `valkvork.v1`); the adapter is `crates/valkvork-controller/src/grpc.rs`. Each RPC turns into one core operation (see [controller_core.md](controller_core.md#messages)), the same one the UI uses.

## Authentication

Every request must carry the admin token:

```
authorization: Bearer <admin_token>
```

A tonic interceptor compares it in constant time and answers `UNAUTHENTICATED` otherwise. The token comes from `admin_token` or `admin_token_file` in `controller.toml` and should be at least 32 random bytes (FR-SEC-03); the controller warns at startup if it is shorter. There is no RBAC in this release. All admin RPCs are recorded with actor `cli`.

## RPCs

| RPC | Request → response | Does |
|---|---|---|
| `Apply` | `manifest_toml` → `service_id`, `version`, `change`, `warnings` | Validate, then create or update the service (FR-MAN-02). Replies after the commit |
| `Plan` | `manifest_toml` → as Apply, plus `feasible`, `reason`, `diff`, `to_place`, `placeable`, `target_replicas`, `max_parallel` | Same classification and a placement check; writes nothing (FR-MAN-03) |
| `DeleteService` | `service_id` → `accepted` | Mark DELETING; workloads stop; the row goes once all are terminal |
| `GetService` | `service_id` → `ServiceDetail` | Canonical spec as TOML, status, rollout status, counts, every workload, the last 20 scaling events |
| `ListServices` | optional `service_id` → `ServiceSummary[]` | Image, version, target and running replicas, CPU utilisation, status, rollout status |
| `ListWorkloads` | optional `service_id`, `node_id`, `status` → `WorkloadSummary[]` | Status, reason, restarts, version, node, timestamps |
| `ListNodes` | optional `node_id`, `node_class` → `NodeSummary[]` | Capacity and allocation, status, draining, workload count, last heartbeat, agent and runtime versions, host volumes |
| `DrainNode` / `UndrainNode` | `node_id` → `accepted` | Set or clear `draining` (FR-CON-08) |
| `RemoveNode` | `node_id` → `accepted`, `workloads_stopped` | Forget an UNHEALTHY node: its LOST workloads become STOPPED (`NODE_REMOVED`) and its row is deleted. A HEALTHY node is refused with `FAILED_PRECONDITION` |
| `ScaleService` | `service_id`, `replicas` → `old_replicas`, `new_replicas` | Set `target_replicas`, clamped to `[min, max]` with autoscaling (FR-CON-10) |
| `ListEvents` | optional `kind`, `since` (ms), `limit` (default 50, max 1000) → `ClusterEvent[]` | Cluster events, newest first |

`change` is one of `created`, `unchanged`, `scale-only`, `policy-only` and `replacing`. See [manifests.md](manifests.md#change-classification).

`used_cpu_millicores` and `used_memory_bytes` in `NodeSummary` are **allocated** (sum of requests), the figure placement uses, not observed usage.

## Errors

| gRPC status | When |
|---|---|
| `INVALID_ARGUMENT` | Manifest validation failed. The message lists every error as `CODE at field.path: message`, e.g. `UNKNOWN_FIELD at service.replica: …`. The CLI exits with code 2 |
| `NOT_FOUND` | Unknown service or node |
| `FAILED_PRECONDITION` | The service is being deleted |
| `UNAUTHENTICATED` | Missing or wrong admin token |
| `INTERNAL` | Anything else, e.g. the core stopped |

## Compatibility

Field numbers are never reused or renumbered; removed fields are marked `reserved` (spec §11.2). Fields added since the spec's draft, all additive: `PlanResponse.diff`, `to_place`, `placeable`, `target_replicas`, `max_parallel`; `WorkloadSummary.service_id`, `service_version`; `ServiceSummary.rollout_status`.

## Example with `grpcurl`

The protos are not served by reflection, so point `grpcurl` at the proto file:

```bash
grpcurl -cacert ca.pem -import-path crates/valkvork-proto/src -proto admin.v1.proto \
  -H "authorization: Bearer $VALKVORK_ADMIN_TOKEN" \
  controlplane.valkvork.internal:50052 valkvork.v1.AdminService/ListNodes
```

The CLI ([cli.md](cli.md)) wraps every RPC.
