# Running Valkvork locally

This runs a real controller and agent on your machine, with TLS on. The agent
drives your local Docker Engine, so workloads are ordinary Docker containers
labelled `valkvork.*`.

## Prerequisites

- Rust (stable) and `protoc` (`brew install protobuf` or `apt install protobuf-compiler`)
- Docker running locally: Docker Desktop, Colima, OrbStack, or Docker Engine on Linux
- `openssl` and `curl`

On macOS the Makefile sets `SDKROOT` to the Command Line Tools SDK, which
avoids a linker problem with newer Xcode SDKs.

## Quick start

```bash
make dev-up      # build, generate certs/tokens in .dev/, start controller + agent
make demo        # deploy examples/web.toml and examples/api.toml
make ui          # open https://localhost:50052/ui/ and print the admin token
```

`make dev-up` prints the admin token and the URLs. The certificate is signed
by a throwaway CA in `.dev/tls/ca.pem`, so the browser warns once; accept it,
or trust that CA.

To use the CLI, load its environment into your shell:

```bash
eval "$(make -s env)"
alias valkvork=target/debug/valkvork

valkvork status
valkvork workloads
valkvork node list
curl -s http://localhost:18080/          # nginx from examples/web.toml
```

## Things to try

| Try | How | What you should see |
|---|---|---|
| Idempotent apply | `valkvork apply -f examples/web.toml` again | `unchanged`, nothing restarts |
| Scale | `valkvork scale service api 4`, or the Scale form in the UI | two more `api` workloads go RUNNING |
| Rolling update | change `RELEASE` in `examples/api.toml`, `valkvork plan -f …`, then `apply` | replaced one at a time, new before old, never fewer than the target running |
| Local restart | `docker kill $(docker ps -q --filter label=valkvork.service_id=web)` | same container restarted in place after 5 s; `RESTARTS` goes to 1 |
| Agent restart | `make dev-down && make dev-up` | containers keep their IDs and are re-adopted |
| Drain | Nodes page → Drain, or `valkvork node drain <id>` | on one node replacements cannot be placed, so workloads stay put and show `NO_NODE` replacements until you undrain |
| Validation | `valkvork plan -f` on a manifest with a typo | exit code 2 and the field path, for example `UNKNOWN_FIELD at service.replica` |
| Events and audit | Events page, or `valkvork events --kind AUDIT` | every UI and CLI action, with actor `cli` or `ui:<session>` |
| Metrics | `curl -s http://127.0.0.1:9400/metrics`, `curl -s http://127.0.0.1:9401/metrics` | controller and agent Prometheus series |

## Managing the dev cluster

| Command | Does |
|---|---|
| `make dev-status` | process state, nodes and services |
| `make dev-logs` | follow `.dev/controller.log` and `.dev/agent.log` (JSON) |
| `make dev-down` | stop both processes; containers keep running, as they would in production |
| `make dev-up` | start again; the agent re-adopts running containers |
| `make dev-reset` | stop, remove the containers this dev cluster created, delete `.dev/` |

`dev-reset` only removes containers whose `/alloc` directory is under this
repository's `.dev/data`; Valkvork containers from anything else on the same
Docker are left alone.

Ports default to 50051 (agents), 50052 (admin API and UI), 9400 and 9401
(metrics). Override them with `VALKVORK_DEV_AGENT_PORT`,
`VALKVORK_DEV_ADMIN_PORT`, `VALKVORK_DEV_METRICS_PORT` and
`VALKVORK_DEV_AGENT_METRICS_PORT`.

## Limitations of the local setup

- **CPU telemetry on macOS.** The agent reads container CPU and memory from
  cgroup v2 files, which live inside the Docker VM on macOS, so CPU shows 0 %
  and the autoscaler never acts. On a Linux host it works.
- **One node.** Host ports allow one replica per node, so `web` cannot scale
  past 1, and drains have nowhere to move workloads.
- **Node identity.** Without `/etc/machine-id` (macOS) the agent uses a random
  node id, saved in `.dev/data/node_id`.

## Tests and CI

```bash
make test        # unit and behaviour tests, no Docker needed
make ci          # fmt check, clippy -D warnings, tests: what CI runs
make dist        # static musl binaries in dist/ (PLATFORM=linux/amd64 to cross-build)
make image       # valkvork-controller:dev and valkvork-agent:dev images
```
