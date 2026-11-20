# Next steps

**The [platform workloads plan](docs/platform-workloads.md) is done.** Stages 0–3 were
verified on AWS (2026-11-08): every platform service runs on the dev cell. Three of its
follow-ups are built on branch `feat/failed-logs-exec-checks-restart-retry` and pass
`make e2e`, not yet on AWS: FAILED containers kept for their logs, `exec` health checks (F2),
and retrying a failed in-place restart (F4).

**Service discovery is built** on branch `feat/service-discovery` ([discovery.md](docs/components/discovery.md)):
`<service>.svc.valkvork.internal` answered by each agent from the controller's healthy endpoints, and
`make e2e` resolves global-pg by name from a container. To use it on the dev cell: allow port 53 from
`docker0` in the goldenimage UFW rules, set `[discovery] enabled = true` in agent.toml, redeploy the
services so their containers get the new nameserver, and point URLs such as global-cp's at
`<service>.svc.valkvork.internal` in place of the per-node-group Route53 names.

The items below remain.

What is not built yet. The rest of the [spec](spec.md)'s MVP scope is implemented and described in [docs/](docs/components/index.md); items tagged [P2], [P3] or [Later] in the spec are deliberately out of scope for now and are collected, with design sketches, in [docs/phase-2.md](docs/phase-2.md).

1. **Roll the dev cell onto an image built from goldenimage `main`.** The cell in us-west-1 runs
   on an image built before goldenimage#9. Rebuild to get the `local` log driver, the explicit
   systemd cgroup driver and the 15 GiB root, then check `tofu plan` before applying the new
   `ami_id`: the smaller root replaces the existing instances (their `/data` volumes are kept).
2. **Auto-scaling groups in the cell template.** The template runs a fixed `node_count` of
   instances. The spec's layout (§10) wants a controller ASG 1/1/1 that re-attaches its EBS volume
   and a fixed-IP ENI, and one worker ASG per node class.
3. **Measure controller memory** at the NFR-01 load (AT-11): under 50 MB RSS at 50 nodes and
   500 workloads with 10 UI sessions open. Needs a load harness that registers simulated agents
   from one machine. The agent half passed on the AWS dev cell on 2026-11-07: 6.8 MB RSS (peak
   7.2 MB) with 21 running workloads, against a 15 MB limit. The controller used 8.9 MB at
   2 nodes and ~42 workloads, which is not the required load.
4. **Container hardening options** for less trusted images: a PID limit, `no-new-privileges`,
    capability drops and a read-only root filesystem, as manifest settings or agent defaults.
    Today containers get Docker's defaults (see docs/components/security.md#process-permissions).

## Re-evaluate After the Platform Workloads Plan

Decisions taken for the [platform workloads plan](docs/platform-workloads.md#decisions-2026-11-07)
that should be revisited once Stage 2 is verified:

- **The default bind address.** Ports publish on `0.0.0.0` unless `host_ip` is set. Consider
  making the node's private address (`advertise`) the default, since cell instances can have
  public IPs.
- **Secrets as files.** Secrets are delivered as environment variables. Add delivery as files on
  an in-memory filesystem, readable only by the container's user: kept out of `docker inspect`
  and `/proc/<pid>/environ`, and rotatable without a restart.
- **Caching resolved secrets** on the agent, so a restart while SSM is unreachable can still
  start the container.
- **Use the `exec` check for global-pg** (`command = ["pg_isready", "-U", "valkv"]`) and
  repeat Stage 1's pause check on the dev cell.
- **Containers' access to the node's role.** Every container on a node can use its instance
  role, so it can read every secret that node can. Look at per-workload credentials, or
  blocking the metadata service from containers that don't need it.
