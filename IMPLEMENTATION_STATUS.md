# Implementation status

Status of `spec.md` requirements. **Done** means implemented and covered by
tests; ✓e2e means also exercised end to end (controller, agent and CLI over
TLS against a real Docker Engine).

## Controller

| Requirement | Status | Notes |
|---|---|---|
| FR-CON-01 State store | Done | WAL, `synchronous=FULL`, versioned migrations. One core actor owns the only write connection and commits before sending commands or replies. History reads use a separate read-only connection. Heartbeat times are flushed every 30 s and on node status changes. |
| FR-CON-02 Node health | Done ✓e2e | UNHEALTHY after 10 s of silence; workloads go LOST and get replacements. |
| FR-CON-03 Reconnect rules | Done ✓e2e | Re-adopt when the replacement is PENDING, stop the old copy when the replacement is placed, re-stop desired=STOP workloads, stop orphans after grace. |
| FR-CON-04 Recovery grace | Done ✓e2e | 30 s default; `kill -9` + restart leaves containers untouched. |
| FR-CON-05 Reconcile loop | Done | Event-driven plus a 5 s tick; excess order PENDING → ASSIGNED → RUNNING on the most-allocated node, newest first; replacement backoff 5 s…5 min; DEGRADED after 5 failures. |
| FR-CON-06 Scheduler | Done ✓e2e | Filters: health, drain, class, CPU/memory, static host ports, host volumes. Least-allocated score; tie-break on same-service count, then node id. `NO_NODE: …` reasons. |
| FR-CON-07 Rolling updates | Done ✓e2e | Start new, wait `min_healthy_time`, stop old, `max_parallel` at a time; halt on failure; the next apply resumes. |
| FR-CON-08 Drain | Done ✓e2e | Same migration path. Undrain cancels in-flight replacements. |
| FR-CON-09 Command dispatch | Done | Re-send once after 30 s and on reconnect; `START_TIMEOUT` after 5 min. Rejected starts are rescheduled. |
| FR-CON-10 Admin API and CLI | Done ✓e2e | All commands; bearer token; exit codes 0/1/2; manual scale clamps to the autoscale range; delete drains, then removes the row. |
| FR-CON-11 History retention | Done | 24 h / last 10 terminal workloads per service; events 30 days. |
| FR-UI-01 Self-contained | Done ✓e2e | Hand-written HTML/CSS/ES-module JS in `ui/`, `include_bytes!` plus build-time gzip; `build.rs` fails above 250 KB (currently about 32 KB); strict CSP on every response, no inline script or style. |
| FR-UI-02 Where served | Done ✓e2e | Shares `:50052`: a tower layer sends `application/grpc` to AdminService and everything else to the UI; `/` redirects to `/ui/`; `[ui] enabled = false` turns it off. |
| FR-UI-03 Pages | Done ✓e2e | Overview, Services, Service detail (scale, delete confirmed by typing the id), Nodes, Node detail (drain/undrain), Workloads (filters), Events (kind filter), Deploy (plan, then apply). Checked in headless Chrome. |
| FR-UI-04 Live updates | Done ✓e2e | SSE at `/ui/api/events`, one `change` per commit; falls back to 5 s polling. |
| FR-UI-05 One code path | Done | `/ui/api/*` call the same core operations as AdminService. |
| FR-UI-06 Auth and audit | Done ✓e2e | Admin token login; random session cookie (`HttpOnly; SameSite=Strict; Path=/ui`, `Secure` with TLS), 12 h, in memory; `X-Valkvork-CSRF` on mutations (403 without); every apply/scale/delete/drain is an `AUDIT` event with actor `ui:<audit id>`, which is not the cookie value. |
| FR-UI-07 Chart data | Done | 60 one-minute samples per service (CPU) and per class (allocation), in memory. |
| FR-UI-08 Usability | Done | Light/dark via `prefers-color-scheme`, text plus colour status badges, deep links, 1024 px minimum layout. |
| FR-UI-09 Isolation | Done | Each SSE client has a bounded broadcast buffer and is dropped when it lags; the core never waits for UI clients. RSS budget not measured yet (AT-11). |

## Agent

| Requirement | Status | Notes |
|---|---|---|
| FR-AGE-01 Packaging | Done | Static musl via `Dockerfile` (verified `statically linked`). Image work in goldenimage#7. |
| FR-AGE-02 Identity | Done | UUIDv5 of `/etc/machine-id`, persisted in `data_dir/node_id`. Full inventory in `Register`. |
| FR-AGE-03 Telemetry | Done | cgroup v2 `cpu.stat` / `memory.current`, node `/proc`. Not observable when the agent runs on macOS against a VM. |
| FR-AGE-04 Commands | Done ✓e2e | Idempotent; `vk-<id>` names; NanoCpus, Memory = MemorySwap, ports, mounts, labels, restart `no`; 409 means already exists; registry auth from Docker config format. |
| FR-AGE-05 Partition autonomy | Done ✓e2e | Only reconnects: 1 s doubling to 15 s, ±20 % jitter. |
| FR-AGE-06 Local restarts | Done ✓e2e | Driven by Docker events (plus a 10 s resync as a safety net); FAILED with `EXITED`/`OOM_KILLED`; exit code 0 also counts as a failure. |
| FR-AGE-07 Adoption | Done ✓e2e | Agent restart keeps container IDs and start times. Labelled containers created later are adopted too, so the controller sees orphans. |
| FR-AGE-08 Ephemeral disk | Done | Alloc dir every 10 s, `SizeRw` every 60 s, `EPHEMERAL_DISK_EXCEEDED` with no restart. |
| FR-AGE-09 Runtime trait | Done | No bollard types outside `runtime.rs`. |
| FR-AGE-10 containerd | P2 | |

## Other

| Requirement | Status | Notes |
|---|---|---|
| FR-MAN-01…04, VE-01…07, VW-01 | Done ✓e2e | serde `deny_unknown_fields` with TOML field paths; canonical spec plus SHA-256 hash; scale-only / policy-only / replacing; autoscaled re-apply never resets the target. |
| FR-AUT-01…05 | Done (unit-tested) | 60 s window, ±10 % dead zone, 2 min sustain, 3 min cooldown, guards, `scaling_events`. Not exercised live yet. |
| FR-STO-01/02/03 | Done | `/alloc` bind; host volumes; the Docker socket, `/var/lib/docker` and Docker's actual data root can never be mounted (FR-SEC-06). |
| FR-SEC-01/03 | Done ✓e2e | Server TLS (rustls) with agents verifying the CA; join token; admin bearer token; constant-time compare. |
| NFR-03 Static binaries | Done | `docker buildx build --target export`. |
| NFR-07 Observability | Done ✓e2e | JSON `tracing` logs; Prometheus `/metrics` on the controller (`:9400`, the full minimum set plus extras) and the agent (`127.0.0.1:9401`, loopback enforced). |
| §10 Deployment | Partial | Packer image: goldenimage#7. Terraform, ENI/EBS boot script and Litestream are not started. |

## Known limitations

- Outdated workloads are accounted with the *current* spec's resources (old specs are not stored per version).
