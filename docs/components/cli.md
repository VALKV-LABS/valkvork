# Component: CLI (`valkvork`)

## High-Level Function

`valkvork` is the operator tool for the controller's admin API (FR-CON-10). It runs on an operator's machine or in CI, and talks gRPC to `:50052`. It holds no state. The code is `crates/valkvork-cli/src/main.rs`.

## Connecting

| Flag | Environment | Default |
|---|---|---|
| `--endpoint` | `VALKVORK_ADDR` | `https://controlplane.valkvork.internal:50052` |
| `--token` | `VALKVORK_ADMIN_TOKEN` | none |
| `--ca-file` | `VALKVORK_CA_FILE` | none; required for an `https://` endpoint |
| `-o, --output` | | `table` (`json` for scripts) |

With an `https://` endpoint the controller's certificate is verified against `--ca-file`, which is required: the binaries carry no built-in list of public CAs. On a controller built from the golden image, `sudo -i; set -a; . /etc/valkvork.d/cli.env` sets all three variables. For the local dev cluster, `eval "$(make -s env)"` does.

## Commands

| Command | Does |
|---|---|
| `apply -f <file>` | Create or update a service. Prints the change (`created`, `unchanged`, `scale-only`, `policy-only`, `replacing`) and warnings |
| `plan -f <file>` | Show the diff, the new version and the placement check; change nothing |
| `delete service <id>` | Stop every workload, then remove the service |
| `scale service <id> <n>` | Set the target replicas (clamped to the autoscale range) |
| `status` | Node counts and every service: version, running/target, CPU, status, image |
| `status <service>` | One service: status, rollout, counts, workloads, scaling events, the canonical spec |
| `workloads [--service S] [--node N] [--status S]` | Workloads with version, node, status, restarts, age, reason |
| `events [--kind K] [--limit N]` | Cluster events, newest first. Kinds: `NODE`, `WORKLOAD`, `DEPLOY`, `SCALE`, `AUDIT` |
| `node list [--class C]` | Nodes with allocation, workloads, heartbeat age and runtime |
| `node drain <id>` / `node undrain <id>` | Move workloads off a node, or allow placements again |
| `node remove <id>` | Forget an UNHEALTHY node, such as a terminated VM. Its LOST workloads are marked STOPPED. If it comes back, it joins as a new node |

Manifests are validated locally before anything is sent, so a bad file fails with exit code 2 even when no controller is reachable.

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Server or transport error: unreachable, unauthenticated, not found, … |
| `2` | Validation error, local or from the server (`INVALID_ARGUMENT`) |

## Examples

```bash
$ valkvork apply -f examples/web.toml
service "web": created (version 1)

$ valkvork apply -f examples/web.toml
service "web": unchanged (version 1)

$ valkvork scale service api 4
service "api": replicas 2 → 4

$ valkvork node list
NODE                                  HOSTNAME    CLASS    STATUS   CPU ALLOC  MEM ALLOC       WORKLOADS  HEARTBEAT  RUNTIME
590060ce-04ae-4caf-84e6-3814a15888de  worker-1    general  HEALTHY  450/5500m  128MiB/11.2GiB  3          1s ago     docker/29.5.2

$ valkvork events --kind AUDIT
TIME                 KIND   ACTOR        MESSAGE
2026-11-04 17:30:31  AUDIT  ui:edb91b11  apply service e2e-web: created
2026-11-04 17:30:30  AUDIT  cli          drain node worker-1

$ valkvork -o json workloads --service api | jq '.[].status'
```

`-o json` prints the API's messages as JSON, for scripts and CI.
