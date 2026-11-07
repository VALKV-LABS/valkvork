# Deploying Valkvork

This covers building release artefacts, the golden image, how the daemons run on hosts, and the production layout from spec §10. For a cluster on your own machine, see [local-development.md](local-development.md).

## Artefacts

All three binaries are statically linked musl executables with no glibc or OpenSSL dependency (NFR-03), so the same file runs on any x86_64 or aarch64 Linux.

| Artefact | How | Contents |
|---|---|---|
| Binaries | `make dist PLATFORM=linux/amd64` (or `linux/arm64`) | `dist/valkvork-controller`, `dist/valkvork-agent`, `dist/valkvork` |
| Images | `make image`, or the release workflow | `valkvlabs/valkvork-controller` (controller and CLI) and `valkvlabs/valkvork-agent` (agent and CLI), FROM scratch, about 8 MB each |
| Release tarballs | Releasing a tag `vX.Y.Z` (below) | `valkvork-X.Y.Z-linux-x86_64.tar.gz` and `…-linux-aarch64.tar.gz` with all three binaries, plus `SHA256SUMS`, on the GitHub release |

### Release workflow

Tagging is automatic; releasing is your choice.

| Workflow | When | Does |
|---|---|---|
| `ci.yml` | Every pull request and every push to `main` | `cargo fmt --check`, `clippy -D warnings`, the tests, and a static-linking check of the musl build |
| `tag.yml` | CI passed on a push to `main` | Tags that commit with the next version. Nothing is published |
| `release.yml` | Every push to `main` | Publishes both images as `edge` |
| `release.yml` | You run it for a tag, or push a tag yourself | Releases that tag: versioned images and the GitHub release |

**Version numbers.** `scripts/next-version.sh` picks each tag: the next patch by default (`v0.1.3` → `v0.1.4`). Put `#minor` or `#major` in a commit message, or a pull request title that becomes the merge commit's message, to bump that part instead; `#skip-tag` leaves that commit untagged. The first tag is the crate version, `v0.1.0`. Commits that finish CI out of order never get a lower number than an older tag.

**Releasing.** Pick any tag on `main` whose commit passed CI:

```bash
git fetch --tags && git tag --sort=-v:refname | head     # see what is available
gh workflow run release.yml -f tag=v0.1.4                # or Actions → Release → Run workflow
```

The release builds that tag's commit with its version stamped into the crates (`scripts/set-version.sh`), so the binaries report it in `--version`, which the golden image installer checks. The version committed in `Cargo.toml` is not changed. It publishes both images for amd64 and arm64 as `X.Y.Z`, `X.Y` and `latest`, and a GitHub release with the tarballs and `SHA256SUMS`. Pushing a tag yourself releases it the same way. Tags created by `tag.yml` do not trigger a release, because GitHub never starts workflows from events made with the workflow token.

Each architecture builds natively on its own runner. The workflows need the repository secrets `DOCKERHUB_USERNAME` and `DOCKERHUB_TOKEN`.

`make ci` runs CI's first three checks locally.

## Production Layout

```
            Internet (end users → apps only)
                       │
          ┌────────────▼────────────┐
          │  Public ALB / NLB       │  one target group per (node_class, host_port),
          └─────┬──────────────┬────┘  health-checking the host port on every worker
                │              │
   ┌────────────▼─────┐ ┌──────▼───────────┐   worker ASG per node class
   │ worker VM        │ │ worker VM        │   (golden image, private subnets,
   │ agent + Docker   │ │ agent + Docker   │    several AZs)
   └────────┬─────────┘ └──────┬───────────┘
            │  outbound gRPC/TLS :50051
            └────────┬─────────┘
          ┌──────────▼──────────────────────────┐
          │ controller VM  (ASG 1/1/1, one AZ)  │──► S3 (Litestream WAL replica)
          │ fixed-IP ENI + DNS name             │
          │ EBS volume for the database         │
          └─────────────────────────────────────┘
```

- **One controller.** An auto-scaling group of exactly one, pinned to one AZ because EBS is AZ-scoped. Its boot script attaches the database volume and a fixed-IP network interface, so `controlplane.valkvork.internal` keeps resolving to the same address after a replacement and agents never need reconfiguring (FR-NET-04).
- **Workers.** One auto-scaling group per node class, from the golden image. New workers boot, register, and receive pending workloads.
- **Traffic.** Users reach workloads through your load balancer on static host ports. Each target group health-checks its port on every worker of the class, so traffic follows replicas as they move, without Valkvork calling the load balancer (FR-NET-02).

The controller's availability comes from replacing the VM and re-attaching the volume (RTO under 5 minutes), not from clustering. Running workloads keep serving throughout.

The Terraform for a cell lives in the [goldenimage](https://github.com/VALKV-LABS/goldenimage) repository (`environments/template-valkvork`); it runs a fixed number of instances rather than auto-scaling groups so far. Not built yet (see [NEXT_STEPS.md](../NEXT_STEPS.md)): the auto-scaling groups, Litestream backups, publishing metrics to CloudWatch, and the ASG lifecycle hook that drains a worker before it terminates and then runs `valkvork node remove`.

## Golden Image

The [goldenimage](https://github.com/VALKV-LABS/goldenimage) repository builds the image with Packer, from Ubuntu 22.04 Minimal with a 15 GiB root disk (Docker data and alloc directories live on the `/data` volume). `-var orchestrator=valkvork` installs Valkvork instead of Nomad, and publishes it separately, as `valkv-generic-valkvork-*` with SSM parameter `/valkv/<cell>/images/generic-valkvork-<arch>`, so Nomad environments are unaffected. One image serves both roles; user data picks one at boot.

| In the image | |
|---|---|
| Binaries | `/usr/local/bin/valkvork-controller`, `valkvork-agent`, `valkvork`, version-checked at build time |
| Docker | `docker-ce` with `live-restore`, overlay2, data root `/data/docker` on the instance's data volume |
| Config templates | `/usr/local/share/valkvork/controller.toml.tmpl` and `agent.toml.tmpl`: the commented base configuration of every node, from goldenimage `packer/generic/files/valkvork/` |
| Units | `valkvork-controller.service`, `valkvork-agent.service` (both disabled), `valkv-valkvork-bootstrap.service` |
| Identity | `/etc/machine-id` truncated as the last build step, so every instance gets its own `node_id` |

At every boot, the bootstrap reads `/etc/valkv/valkvork.env` from user data:

```bash
VALKVORK_ROLES=controller,agent     # controller, agent, or both
VALKVORK_REGION=us-west-2
VALKVORK_SSM_PREFIX=/valkv/aws-us-west-2/valkvork
VALKVORK_HOST_VOLUMES=pgdata=/data/pgdata   # agent, optional
```

It then fetches secrets from SSM under the prefix (`ca_cert`, `join_token`, and `controller/cert`, `controller/key`, `controller/admin_token` for the controller, `registry_auth` optionally for agents). It renders `/etc/valkvork.d/<role>.toml` from the role's template, validates it with `--check-config`, and starts the role's unit. The controller's database goes on the data volume at `/data/valkvork/db`. The full contract is in the goldenimage README.

The bootstrap fills in only what differs per instance:

| Setting | Source |
|---|---|
| `node_class` | The instance's `valkvork-node-class` tag, read from instance metadata (the cell template sets it and enables metadata tags); else `VALKVORK_NODE_CLASS`; else `general`. Retag and reboot to change it |
| `[reserved]` | Scaled to the instance with GKE's node reservation tiers, for example 0.1 core and ~475 MiB on 2 vCPU / 2 GiB, ~3.5 GiB on 8 vCPU / 32 GiB; `VALKVORK_RESERVED_CPU` / `_MEMORY` override |
| `advertise_ip` | The instance's private IPv4 |
| `[host_volumes.*]` | `VALKVORK_HOST_VOLUMES` |

**Operator access.** On the golden image the admin API and web UI listen on the controller's localhost only (`admin_listen_addr = "127.0.0.1:50052"`); no security group or firewall rule admits `:50052`. Forward the port, then use `https://localhost:50052/ui` or point the CLI at `https://localhost:50052` (the controller certificate covers `localhost`):

```bash
# Session Manager, no open port (the cell's `tofu output ui_port_forward_command`)
aws ssm start-session --target <controller instance id> \
  --document-name AWS-StartPortForwardingSession \
  --parameters portNumber=50052,localPortNumber=50052
# or SSH
ssh -L 50052:localhost:50052 ubuntu@<controller address>
```

On the controller itself, `sudo -i; set -a; . /etc/valkvork.d/cli.env` sets up the CLI.

## systemd Units

```ini
# valkvork-agent.service: root, because it drives Docker and does the bind mounts
[Unit]
Wants=network-online.target
After=network-online.target docker.service
Requires=docker.service

[Service]
ExecStart=/usr/local/bin/valkvork-agent --config /etc/valkvork/agent.toml
Restart=always
RestartSec=3s
LimitNOFILE=65535
```

```ini
# valkvork-controller.service: an unprivileged user, database on block storage
[Service]
User=valkvork
ExecStart=/usr/local/bin/valkvork-controller --config /etc/valkvork/controller.toml
Restart=always
RestartSec=3s
ProtectSystem=strict
ReadWritePaths=/var/lib/valkvork
```

Containers are children of containerd's shims, not of the agent or dockerd, so restarting the agent unit leaves them alone (AT-04), and `live-restore` keeps them through a dockerd restart (AT-15). `Requires=docker.service` restarts the agent with dockerd, and the agent re-adopts its containers.

## Running the Images Instead

The images are FROM scratch with the binary as entrypoint, for environments that prefer containers:

```bash
docker run -d --name valkvork-controller \
  -v /etc/valkvork:/etc/valkvork:ro \
  -v /var/lib/valkvork/db:/var/lib/valkvork/db \
  -p 50051:50051 -p 50052:50052 -p 9400:9400 \
  valkvlabs/valkvork-controller:latest

docker run -d --name valkvork-agent --network host \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /etc/machine-id:/etc/machine-id:ro \
  -v /etc/valkvork:/etc/valkvork:ro \
  -v /var/lib/valkvork:/var/lib/valkvork \
  valkvlabs/valkvork-agent:latest
```

The controller image runs as uid 65532, so its database directory must be writable by that uid. Mount the agent's data directory at the **same path** inside and out, because the agent bind-mounts alloc directories from it through the host's Docker. Host networking lets the agent report the host's address. On VMs, the systemd units above are the reference.

## Upgrades

- **Upgrade order.** Upgrade the controller first, then roll the workers (spec §11.2). Restarting the controller enters the recovery grace period and never touches running workloads. Proto fields are only ever added, so agents built from older code keep working.
- **Workers** are immutable: drain a node (`valkvork node drain <id>`), wait until it has no workloads, then replace it with an instance of the new image. Restarting an agent in place also works: it re-adopts its containers.
- **Database** migrations run automatically at controller start and are forward-only. Snapshot the volume before upgrading.
