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
| Drain | `make dev-add-node`, then Nodes page → Drain on the first node, or `valkvork node drain <id>` | workloads start on node2 before stopping on the drained node; with only one node they stay put with `NO_NODE` replacements |
| Node failure | `make dev-add-node`, then `make dev-stop-node` | node2 UNHEALTHY after 10 s, replacements elsewhere; `make dev-add-node` again and the old copies are stopped or re-adopted |
| Validation | `valkvork plan -f` on a manifest with a typo | exit code 2 and the field path, for example `UNKNOWN_FIELD at service.replica` |
| Events and audit | Events page, or `valkvork events --kind AUDIT` | every UI and CLI action, with actor `cli` or `ui:<session>` |
| Host volumes | `valkvork apply -f examples/static-site.toml`, then `curl http://localhost:18081/` | nginx serves `.dev/volumes/site/`; edits on the host show up at once |
| Data across workloads | [placement-and-storage.md](placement-and-storage.md#try-it-locally) | a Postgres row survives deleting and re-applying the service |
| Pinning | `valkvork apply -f examples/pinned.toml` | PENDING with `NO_NODE: no healthy nodes in class db`, until you run `VALKVORK_DEV_NODE_CLASS=db make dev-up` |
| Metrics | `curl -s http://127.0.0.1:9400/metrics`, `curl -s http://127.0.0.1:9401/metrics` | controller and agent Prometheus series |

## Managing the dev cluster

| Command | Does |
|---|---|
| `make dev-status` | process state, nodes and services |
| `make dev-logs` | follow `.dev/controller.log` and `.dev/agent.log` (JSON) |
| `make dev-down` | stop both processes; containers keep running, as they would in production |
| `make dev-up` | start again; the agent re-adopts running containers |
| `make dev-reset` | stop, remove the containers this dev cluster created (on every node), delete `.dev/` |

`dev-reset` only removes containers whose `/alloc` directory is under this
repository's `.dev/data`; Valkvork containers from anything else on the same
Docker are left alone.

`make dev-up` declares the host volumes `site` and `pgdata` (in `.dev/volumes/`) and node class `general`. Change them with `VALKVORK_DEV_HOST_VOLUMES` (comma separated) and `VALKVORK_DEV_NODE_CLASS`, then `make dev-down && make dev-up`.

Ports default to 50051 (agents), 50052 (admin API and UI), 9400 and 9401
(metrics). Override them with `VALKVORK_DEV_AGENT_PORT`,
`VALKVORK_DEV_ADMIN_PORT`, `VALKVORK_DEV_METRICS_PORT` and
`VALKVORK_DEV_AGENT_METRICS_PORT`.

## More nodes

`make dev-up` gives you one node. Each extra node needs a **Docker daemon of its own**: an agent manages every Valkvork-labelled container on its Docker, so two agents sharing one daemon would treat each other's containers as orphans and stop them. With [Colima](https://github.com/abiosoft/colima), each node gets its own small VM:

```bash
make dev-add-node                 # node2: a Colima VM (2 CPUs, 2 GiB) plus an agent for it
make dev-add-node NAME=node3      # as many as you like
make dev-status                   # every agent and node
```

| Command | Does |
|---|---|
| `make dev-add-node NAME=node2` | Start the VM `node2` if needed (the first start takes a minute or two), then an agent with its own data dir, node id, hostname and metrics port (9402, 9403, …). Run it again on a stopped node to bring it back |
| `make dev-stop-node NAME=node2` | Stop only that agent. Its containers keep running, as in a partition: the controller marks the node UNHEALTHY after 10 s and places replacements; `dev-add-node` brings it back and the old copies are stopped or re-adopted |
| `make dev-remove-node NAME=node2` | Drain it, wait until its workloads have moved, then stop the agent and remove its containers. If a workload cannot move (a host port or host volume no other node offers), it stops and tells you why; `FORCE=1` removes the node anyway, like a VM dying |
| `colima stop --profile node2` | Stop the VM when you are done; `colima delete --profile node2` removes it |

Settings for new nodes: `VALKVORK_DEV_NODE_CLASS_EXTRA` (node class, default `general`), `VALKVORK_DEV_NODE_VOLUMES` (host volumes, default none, so `pgdata` stays on the first node only), `VALKVORK_DEV_NODE_CPUS` and `VALKVORK_DEV_NODE_MEMORY` (VM size, default 2 and 2 GiB). Without Colima, point `VALKVORK_DEV_NODE_SOCKET` at any other Docker daemon's socket.

`add-node` leaves your active Docker context as it was, although `colima start` switches it.

Things to know:

- **Placement is by allocation ratio, not replica count.** New workloads go to the node with the lowest `max(cpu, memory)` allocation after placement; the number of replicas of the same service only breaks ties. A small VM fills up proportionally faster, so replicas may well all land on the larger first node.
- **Removed nodes stay listed as UNHEALTHY**, and a re-added node gets a new node id, because node removal is not built yet ([NEXT_STEPS.md](../NEXT_STEPS.md)). `make dev-reset` starts clean.
- **Host ports from a second VM** may not be reachable from macOS's `localhost` when the first VM already forwards the same port; the containers still run.

## Limitations of the local setup

- **CPU telemetry on macOS.** The agent reads container CPU and memory from
  cgroup v2 files, which live inside the Docker VM on macOS, so CPU shows 0 %
  and the autoscaler never acts. On a Linux host it works.
- **One node by default.** Host ports allow one replica per node, so `web` cannot scale past 1, and drains have nowhere to move workloads, until you add nodes (above).
- **Node identity.** Without `/etc/machine-id` (macOS) the agent uses a random
  node id, saved in `.dev/data/node_id`.

## Tests and CI

```bash
make test        # unit and behaviour tests, no Docker needed
make ci          # fmt check, clippy -D warnings, tests: what CI runs
make dist        # static musl binaries in dist/ (PLATFORM=linux/amd64 to cross-build)
make image       # valkvork-controller:dev and valkvork-agent:dev images
```
