# Next steps

Status by requirement: [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md).

1. **Merge the golden image** ([goldenimage#7](https://github.com/VALKV-LABS/goldenimage/pull/7)),
   then run a real `packer build -var orchestrator=valkvork` on AWS.
2. **Terraform** for a Valkvork cell: controller ASG 1/1/1 with an EBS volume and
   fixed-IP ENI, worker ASGs per class, the SSM parameters listed in the
   golden image README, and security groups (50051 from workers, 50052 from operators).
3. **Litestream** backup of the controller database to S3 (§10.3).
4. **Publish metrics to CloudWatch** for the per-class infrastructure scaling loop (§10.4), and the ASG lifecycle hook that calls `valkvork node drain`.
5. **Measure memory** at the NFR-01 load (AT-11), including 10 UI sessions.
6. **Live autoscaler test** (AT-07) on a Linux worker, where cgroup telemetry is available.
