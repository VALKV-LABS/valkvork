# Component: Configuration

## High-Level Function

Each daemon reads one TOML file, given with `--config` (or `VALKVORK_CONTROLLER_CONFIG` / `VALKVORK_AGENT_CONFIG`), defaulting to `/etc/valkvork/controller.toml` and `/etc/valkvork/agent.toml`. Every key has a default, so a short file works, and **unknown keys are rejected** so typos fail at startup. Both binaries accept `--check-config`, which loads the file, resolves token and TLS files, prints `configuration OK` and exits with 0, or exits with 1 and the error. The types are in `crates/valkvork-common/src/config.rs`. The file is read once at startup; restart the daemon to apply changes.

## Controller

```toml
# Listeners
agent_listen_addr   = "0.0.0.0:50051"    # agent streams; reachable from worker subnets only
admin_listen_addr   = "0.0.0.0:50052"    # admin API and web UI; operators and CI only
metrics_listen_addr = "0.0.0.0:9400"     # Prometheus; omit to disable

# TLS for both gRPC listeners. Set both or neither; neither = plaintext (development only).
tls_cert_file = "/etc/valkvork/controller.pem"
tls_key_file  = "/etc/valkvork/controller.key"   # PKCS#8 PEM

database_path = "/var/lib/valkvork/db/valkvork.db"   # block storage only, never NFS/EFS

# Secrets: the *_file form wins when both are set.
join_token_file  = "/etc/valkvork/join-token"
# join_token     = "..."
admin_token_file = "/etc/valkvork/admin-token"   # >= 32 random bytes
# admin_token    = "..."

# Timing (spec defaults)
node_unhealthy_timeout_seconds   = 10    # silence before a node is UNHEALTHY
recovery_grace_period_seconds    = 30    # after start: no UNHEALTHY marking, replacements or orphan stops
heartbeat_interval_ms            = 2000  # told to agents in RegisterAck
command_timeout_seconds          = 30    # re-send an unacknowledged command once
start_timeout_seconds            = 300   # ASSIGNED → FAILED START_TIMEOUT
reconcile_interval_seconds       = 5
heartbeat_flush_interval_seconds = 30

[autoscaler]
enabled                     = true
evaluation_interval_seconds = 10
window_seconds              = 60     # CPU averaging window
sustain_seconds             = 120    # how long a breach must last
cooldown_seconds            = 180

[ui]
enabled             = true
session_ttl_seconds = 43200          # 12 h
```

Keep `recovery_grace_period_seconds` above the agents' maximum reconnect delay (18 s), and `node_unhealthy_timeout_seconds` at several heartbeat intervals.

## Agent

```toml
controller_endpoint = "https://controlplane.valkvork.internal:50051"   # https verifies with ca_file
ca_file             = "/etc/valkvork/ca.pem"
# tls_domain        = "controlplane.valkvork.internal"   # expected certificate name, if not the endpoint host
join_token_file     = "/etc/valkvork/join-token"

node_class          = "general"              # matched by manifests' required_class
data_dir            = "/var/lib/valkvork"    # node_id, alloc dirs, failed markers
runtime             = "docker"               # the only driver in this release
docker_socket       = "/var/run/docker.sock"
registry_auth_file  = "/etc/valkvork/registry-auth.json"   # optional, Docker config.json format
# advertise_ip      = "10.0.1.23"            # default: the local address routed to the controller
metrics_listen_addr = "127.0.0.1:9401"       # must be loopback; omit to disable

[reserved]                 # kept back from scheduling for the OS, dockerd and the agent
cpu    = 0.25
memory = "384MiB"

[host_volumes.pgdata]      # one table per volume manifests can mount by name
path      = "/data/pgdata"
read_only = false
```

`data_dir` must be visible to the Docker daemon at the same path, because the agent bind-mounts alloc directories from it. On a normal Linux host it is. With Docker in a VM (Colima, Docker Desktop), use a path under your home directory, which those share.

## Environment

| Variable | Used by | Does |
|---|---|---|
| `RUST_LOG` | both daemons | Log level and filters (default `info`); logs are JSON lines on stdout |
| `VALKVORK_CONTROLLER_CONFIG`, `VALKVORK_AGENT_CONFIG` | daemons | Config path, instead of `--config` |
| `VALKVORK_ADDR`, `VALKVORK_ADMIN_TOKEN`, `VALKVORK_CA_FILE` | CLI | See [cli.md](cli.md) |

## On the Golden Image

The image's bootstrap renders both files from SSM Parameter Store at every boot, under `/etc/valkvork.d/`, and validates them with `--check-config` before use. See [deployment.md](../deployment.md#golden-image).
