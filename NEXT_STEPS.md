# Next steps

What is not built yet. The rest of the [spec](spec.md)'s MVP scope is implemented and described in [docs/](docs/components/index.md); items tagged [P2] or [P3] in the spec are deliberately out of scope for now.

1. **Merge the golden image** ([goldenimage#7](https://github.com/VALKV-LABS/goldenimage/pull/7)),
   then run a real `packer build -var orchestrator=valkvork` on AWS.
2. **Terraform** for a Valkvork cell: controller ASG 1/1/1 with an EBS volume and
   fixed-IP ENI, worker ASGs per class, the SSM parameters listed in the
   golden image README, and security groups (50051 from workers, 50052 from operators).
3. **Litestream** backup of the controller database to S3 (§10.3).
4. **Publish metrics to CloudWatch** for the per-class infrastructure scaling loop (§10.4), and the ASG lifecycle hook that calls `valkvork node drain`.
5. **Measure memory** at the NFR-01 load (AT-11), including 10 UI sessions.
6. **Live autoscaler test** (AT-07) on a Linux worker, where cgroup telemetry is available.
7. **Forget terminated workers.** When an ASG replaces a VM, the old node stays UNHEALTHY and its
   workloads stay LOST (not terminal, so never pruned). Add a way to remove a node, for example
   `valkvork node remove <id>` or expiry after a long time UNHEALTHY, that marks its LOST
   workloads STOPPED. The spec does not define this yet.
8. **Docker daemon settings in the golden image** to match FR-AGE-01: the `local` log driver
   (the image uses `json-file`) and `native.cgroupdriver=systemd` set explicitly.
