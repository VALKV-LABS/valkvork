# Next steps

**The [platform workloads plan](docs/platform-workloads.md) is done.** Stages 0–3 were
verified on AWS (2026-11-08). Merged since, passing `make e2e` but not yet run on AWS: FAILED
containers kept for their logs, `exec` health checks, retrying a failed in-place restart,
service discovery, `[security]` and `[logging]`, `[sysctls]`, per-node client certificates
(#19, [security.md](docs/components/security.md#node-certificates)), ports on the node's address
and instance metadata blocking (#20, [security.md](docs/components/security.md#instance-metadata)),
and PID limits.

**The AWS cells are down.** The platform's dev cell is `dev-cell-us-west-1` in
valkv-platform-infra (`environments/dev-cell-us-west-1`, module `platform-cell`); it takes the
newest Valkvork golden image unless `ami_id` pins one. goldenimage's own
`aws-us-west-1-valkvork` is a test cell only. Before a cell runs a release with #20, add
`[security] instance_metadata = true` to the global-cp and sqldb-node templates in
valkv-platform-infra: both use the node's role from inside their containers. After it runs
#19, switch the controller to `[node_certificates] mode = "require"` once every node shows "was
issued a client certificate" in `valkvork events --kind NODE`.

What is not built yet. The rest of the [spec](spec.md)'s MVP scope is implemented and described in [docs/](docs/components/index.md); items tagged [P2], [P3] or [Later] in the spec are deliberately out of scope for now and are collected, with design sketches, in [docs/phase-2.md](docs/phase-2.md).

1. **Measure controller memory** at the NFR-01 load (AT-11): under 50 MB RSS at 50 nodes and
   500 workloads with 10 UI sessions open. Needs a load harness that registers simulated agents
   from one machine. The agent half passed on the AWS dev cell on 2026-11-07: 6.8 MB RSS (peak
   7.2 MB) with 21 running workloads, against a 15 MB limit. The controller used 8.9 MB at
   2 nodes and ~42 workloads, which is not the required load.
2. **Run the merged features on AWS** when the dev cell is next up: discovery in place of the
   per-node-group Route53 names, the `exec` check for global-pg (repeat Stage 1's pause check),
   node certificates, metadata blocking.
3. **Auto-scaling groups** for the cell (now valkv-platform-infra's `platform-cell` module): a
   controller ASG 1/1/1 that re-attaches its EBS volume and a fixed-IP ENI (spec §10), and one
   worker ASG per node class.

## Re-evaluate After the Platform Workloads Plan

Decisions taken for the [platform workloads plan](docs/platform-workloads.md#decisions-2026-11-07)
that should be revisited once Stage 2 is verified:

- **Secrets as files** and **caching resolved secrets** are built: `[secrets]` entries with
  `file = true`, on an in-memory filesystem at `/run/secrets`, rotated in place; the last
  resolved values are reused, from memory, when the secrets store fails at a start.
- **Per-workload credentials.** Containers are blocked from the metadata service unless their
  manifest sets `[security] instance_metadata = true` (security.md#instance-metadata), but those
  that may reach it still share the node's whole role. Look at per-workload credentials.
