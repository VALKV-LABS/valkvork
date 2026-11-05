# Component: Agent Stream

## High-Level Function

Each agent keeps exactly one long-lived, bidirectional gRPC stream to the controller, which the agent opens (FR-ARC-01). Everything travels on it: registration and heartbeats up, commands down, acknowledgements up. The controller never connects to a worker. The proto is `crates/valkvork-proto/src/agent.v1.proto` (package `valkvork.v1`); the agent side is `crates/valkvork-agent/src/connection.rs` and the controller side `crates/valkvork-controller/src/grpc.rs` plus `core/agents.rs`.

```protobuf
service AgentService {
  rpc Connect(stream AgentMessage) returns (stream ControllerMessage);
}
message AgentMessage      { oneof msg { Register register = 1; Heartbeat heartbeat = 2; CommandResult command_result = 3; } }
message ControllerMessage { oneof msg { RegisterAck register_ack = 1; StartWorkload start_workload = 2; StopWorkload stop_workload = 3; } }
```

tonic's generated clients have a built-in `connect()` constructor, which clashes with an RPC named `Connect`. So the agent proto is built server-only and the client is hand-written in `valkvork-proto/src/lib.rs` as `AgentServiceClient::connect_stream`. The wire path is still `/valkvork.v1.AgentService/Connect`.

## A Session

```
 agent                                               controller
   │── TLS to controller_endpoint (CA from ca_file) ──►│
   │── Register ──────────────────────────────────────►│ must arrive within 10 s, first
   │     node_id, join_token, hostname, ip, class,     │ check join token (constant time)
   │     allocatable CPU/memory, host volumes,         │ upsert node → HEALTHY
   │     agent version, runtime, inventory             │ reconcile the inventory (FR-CON-03)
   │◄────────────────────────── RegisterAck ───────────│ accepted, heartbeat_interval_ms
   │◄──────────────── re-sent outstanding commands ────│
   │── Heartbeat every heartbeat_interval_ms ─────────►│ last_seen = now (controller clock)
   │◄──────────────── StartWorkload / StopWorkload ────│
   │── CommandResult (accepted or rejected) ──────────►│
```

- **Register first.** The first message on every stream must be `Register` (FR-AGE-02); anything else is rejected with `INVALID_ARGUMENT`. A wrong join token gets `RegisterAck { accepted: false, reject_reason }` and the stream ends.
- **Inventory.** `Register` carries a `WorkloadStatus` for every container labelled `valkvork.workload_id` on the host, so the controller can re-adopt or stop old copies.
- **Ack first.** The controller always sends `RegisterAck` before any command. The agent waits up to 15 s for it.
- **Heartbeats.** The interval comes from the ack (default 2000 ms, at least 500). Each carries node CPU and memory and a `WorkloadStatus` per managed container.

## Commands and Acks

| Command | Carries | The agent |
|---|---|---|
| `StartWorkload` | command id, workload id, service id and version, image, CPU millicores, memory and disk bytes, env, ports, volumes, restart policy, kill timeout | Checks Docker answers within 3 s, records the workload, and acks; then pulls, creates and starts in the background |
| `StopWorkload` | command id, workload id, kill timeout | Marks the workload stopping and acks; then stops and removes the container and its alloc directory |

Every command gets exactly one `CommandResult` (FR-AGE-04). The ack means "accepted" or "rejected"; actual progress shows up in later heartbeats. Commands are idempotent by workload id:

- a repeated `StartWorkload` for a known workload is acked and does nothing;
- a `StopWorkload` for an unknown workload is acked as success (it also removes a container named `vk-<workload_id>` if one exists).

A start is rejected with `RUNTIME_UNAVAILABLE` when Docker does not answer. The controller then returns the workload to PENDING and places it again. See [controller_core.md](controller_core.md#dispatcher) for how the controller retries unacknowledged commands.

## Workload States Reported

| `WorkloadState` | Meaning | Controller reaction |
|---|---|---|
| `STARTING` | Pulling or creating | Stays ASSIGNED |
| `RUNNING` | Container running | ASSIGNED → RUNNING; sets `started_at` |
| `RESTARTING` | Restarting in place under the restart policy | No change; `restart_count` updated |
| `FAILED` | Failed for good; `reason` says why (`EXITED`, `OOM_KILLED`, `IMAGE_PULL_FAILED`, `CREATE_FAILED`, `START_FAILED`, `EPHEMERAL_DISK_EXCEEDED`, `FORBIDDEN_MOUNT`, `HOST_VOLUME_NOT_FOUND`, …) | FAILED, counted against the service; the controller sends a stop to remove the container |
| `STOPPED` | Stopped on request | STOPPING → STOPPED |

A container that disappears from heartbeats confirms a STOPPING workload as STOPPED. A RUNNING workload that disappears becomes FAILED with `CONTAINER_MISSING`.

## Reconnects

When the stream breaks, the agent does exactly one thing: reconnect (FR-AGE-05). It never stops, restarts or changes a container because the controller is unreachable.

```
delay = min(1 s × 2^attempt, 15 s) × (1 ± 20 % jitter)
```

The attempt counter resets once a `RegisterAck` is accepted. Every new stream starts with a full `Register`, so whatever happened during the outage, the controller decides from the inventory. The controller's recovery grace period (30 s) is longer than the maximum delay (18 s), so all agents are back before it ends.

Keepalives: HTTP/2 pings every 10 s, with a 5 s timeout, on both sides, so a dead connection is noticed even when the stream is idle.

## Transport Security

`controller_endpoint` should be `https://…`. The agent then verifies the controller's certificate against `ca_file`, using `tls_domain` as the expected name if set, else the endpoint's host. With `http://` the stream is unencrypted and the agent logs a warning; that is for development only. Per-node mutual TLS is planned (FR-SEC-02). See [security.md](security.md).
