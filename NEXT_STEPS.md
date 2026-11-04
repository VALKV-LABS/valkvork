# Next steps

Status by requirement: [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md).

1. **Merge the golden image** ([goldenimage#7](https://github.com/VALKV-LABS/goldenimage/pull/7)),
   then run a real `packer build -var orchestrator=valkvork` on AWS.
2. **Terraform** for a Valkvork cell: controller ASG 1/1/1 with an EBS volume and
   fixed-IP ENI, worker ASGs per class, the SSM parameters listed in the
   golden image README, and security groups (50051 from workers, 50052 from operators).
3. **Litestream** backup of the controller database to S3 (§10.3).
4. **Metrics** (NFR-07): controller `:9400`, agent `127.0.0.1:9401`. This unblocks
   infrastructure autoscaling (§10.4).
5. **Web UI** (FR-UI-01…09), served on `:50052` next to the gRPC API.
6. **Live autoscaler test** (AT-07) on a Linux worker, where cgroup telemetry is available.
