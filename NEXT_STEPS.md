# Next steps

What is not built yet. The rest of the [spec](spec.md)'s MVP scope is implemented and described in [docs/](docs/components/index.md); items tagged [P2] or [P3] in the spec are deliberately out of scope for now and are collected, with design sketches, in [docs/phase-2.md](docs/phase-2.md).

1. **Deploy the dev cell.** The golden image is built and on AWS, and the Terraform for a cell
   exists in goldenimage (`environments/template-valkvork`, applied from
   `environments/aws-us-west-1-valkvork`). Set `ami_id` (or `use_ssm_ami_lookup = true`) in
   `terraform.tfvars`, add a remote state backend, then `terraform apply`. The UI and admin API
   listen on the controller's localhost: use `tofu output ui_port_forward_command`.
2. **Auto-scaling groups in the cell template.** The template runs a fixed `node_count` of
   instances. The spec's layout (§10) wants a controller ASG 1/1/1 that re-attaches its EBS volume
   and a fixed-IP ENI, and one worker ASG per node class.
3. **Litestream** backup of the controller database to S3 (§10.3). Nothing is built yet: add the
   binary and a systemd unit to the golden image (replicating `/data/valkvork/db`), an S3 bucket
   and IAM permissions in the cell template, and a restore on boot when the data volume is empty.
4. **Publish metrics to CloudWatch** for the per-class infrastructure scaling loop (§10.4), and the
   ASG lifecycle hook that calls `valkvork node drain`, then `valkvork node remove` once the
   instance is gone.
5. **Measure memory** at the NFR-01 load (AT-11), including 10 UI sessions.
6. **Live autoscaler test** (AT-07) on a Linux worker, where cgroup telemetry is available.
7. **Expire long-UNHEALTHY nodes.** `valkvork node remove <id>` (and **Remove** in the UI) forgets
   a dead node by hand. An optional controller setting could do the same for nodes UNHEALTHY for
   longer than, say, 24 h, for clusters without a lifecycle hook.
8. **Container hardening options** for less trusted images: a PID limit, `no-new-privileges`,
    capability drops and a read-only root filesystem, as manifest settings or agent defaults.
    Today containers get Docker's defaults (see docs/components/security.md#process-permissions).
