# Next steps

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
