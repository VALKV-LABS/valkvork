# Next steps

**The [platform workloads plan](docs/platform-workloads.md) is done.** Stages 0–3 were
verified on AWS (2026-11-08): every platform service runs on the dev cell. Merged since, and
passing `make e2e` but not yet run on AWS: FAILED containers kept for their logs, `exec` health
checks (F2), retrying a failed in-place restart (F4), service discovery
([discovery.md](docs/components/discovery.md)), `[security]` and `[logging]`, and `[sysctls]`
(PR #18, open).

**Built on branches, not yet merged or on AWS:**

- `feat/node-mtls`: per-node client certificates (FR-SEC-02,
  [security.md](docs/components/security.md#node-certificates)). The controller defaults to
  `[node_certificates] mode = "issue"`; switch the cell to `require` once every node shows
  "was issued a client certificate" in `valkvork events --kind NODE`.
- `feat/secure-defaults` (on top of it): ports without `host_ip` publish on the node's address
  (agent.toml `default_host_ip = "advertise"`), and containers are blocked from the instance
  metadata service unless their manifest sets `[security] instance_metadata = true`
  ([security.md](docs/components/security.md#instance-metadata)). **Before the cell runs this
  agent**, add `instance_metadata = true` to the global-cp and sqldb-node templates in
  valkv-platform-infra: both use the node's role from inside their containers.

**Rolling it onto the dev cell:** allow port 53 from `docker0` in the goldenimage UFW rules (done in
goldenimage#23), set `[discovery] enabled = true` in agent.toml, redeploy the services so their
containers get the new nameserver, and point URLs such as global-cp's at
`<service>.svc.valkvork.internal` in place of the per-node-group Route53 names. Use the `exec`
check for global-pg and repeat Stage 1's pause check.

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
4. **A PID limit** for less trusted images. The rest of the hardening options are built
   (`[security]`, PR #17).

## Re-evaluate After the Platform Workloads Plan

Decisions taken for the [platform workloads plan](docs/platform-workloads.md#decisions-2026-11-07)
that should be revisited once Stage 2 is verified:

- **Secrets as files.** Secrets are delivered as environment variables. Add delivery as files on
  an in-memory filesystem, readable only by the container's user: kept out of `docker inspect`
  and `/proc/<pid>/environ`, and rotatable without a restart.
- **Caching resolved secrets** on the agent, so a restart while SSM is unreachable can still
  start the container.
- **Per-workload credentials.** Containers are blocked from the metadata service unless their
  manifest sets `[security] instance_metadata = true` (security.md#instance-metadata), but those
  that may reach it still share the node's whole role. Look at per-workload credentials.
