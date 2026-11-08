# Next steps

**Current plan: [platform workloads](docs/platform-workloads.md).** Stages 0–2 are done
and verified on AWS (2026-11-08); Stage 3 (`global-cp`) is next. It comes before
everything below and before [phase-2.md](docs/phase-2.md). In stages, it builds what
Valkvork needs to run the Valkv platform's own services, and verifies each one on the AWS
dev cell:

0. **Test bed:** separate node groups and workload-secret access in the goldenimage
   template, and a dev-cell upgrade script.
1. **Postgres** (`global-pg`): secret references, bind address per port, health checks,
   `command` and `args`.
2. **The gateway control plane** (`valkv-apg`): secrets inside values, batch jobs, and a
   private DNS name for Postgres.

The items below remain, after that plan.

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
- **`exec` health checks** (finding F2): a `tcp` check can't detect a hung process, so
  Postgres's check can't catch a frozen database. `type = "exec"` running `pg_isready` in
  the container would.
- **Retrying a failed in-place restart** (finding F4) under the restart policy, instead of
  failing the workload at once.
- **Containers' access to the node's role.** Every container on a node can use its instance
  role, so it can read every secret that node can. Look at per-workload credentials, or
  blocking the metadata service from containers that don't need it.
