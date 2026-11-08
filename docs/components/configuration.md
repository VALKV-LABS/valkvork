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

[secrets]                  # how manifests' [secrets] references are resolved
aws_cli         = "aws"    # runs `aws ssm get-parameter` for ssm: references, with the instance role
# aws_region    = "us-west-1"   # default: the instance's region (EC2 metadata), else the CLI's default
file_dirs       = []       # directories file: references may read; empty disables file: references
timeout_seconds = 15       # per reference

[host_volumes.pgdata]      # one table per volume manifests can mount by name
path      = "/data/pgdata"
read_only = false
# owner   = "101:101"      # optional numeric uid or uid:gid, applied at startup
# mode    = "0750"         # optional octal permission bits, applied at startup
```

When `owner` or `mode` is set, the agent creates the directory if it is missing and applies them at every startup, so an image that runs as a non-root user can write to the volume. Only the directory itself changes, never the files in it. Both must be numeric (`owner = "nginx"` is rejected, since names differ between the host and the image), and `--check-config` validates them. The agent refuses to change a symlink, a non-directory, or a protected path such as Docker's data directory.

`file:` references are off unless `secrets.file_dirs` lists directories: the agent runs as root, so a manifest could otherwise read any file on the node, such as the controller's TLS key. A file must resolve, after symlinks, to a path inside one of them. `--check-config` requires them to be absolute.

`data_dir` must be visible to the Docker daemon at the same path, because the agent bind-mounts alloc directories from it. On a normal Linux host it is. With Docker in a VM (Colima, Docker Desktop), use a path under your home directory, which those share.

## Environment

| Variable | Used by | Does |
|---|---|---|
| `RUST_LOG` | both daemons | Log level and filters (default `info`); logs are JSON lines on stdout |
| `VALKVORK_CONTROLLER_CONFIG`, `VALKVORK_AGENT_CONFIG` | daemons | Config path, instead of `--config` |
| `VALKVORK_ADDR`, `VALKVORK_ADMIN_TOKEN`, `VALKVORK_CA_FILE` | CLI | See [cli.md](cli.md) |

## On the Golden Image

The image's bootstrap renders both files from SSM Parameter Store at every boot, under `/etc/valkvork.d/`, and validates them with `--check-config` before use. See [deployment.md](../deployment.md#golden-image).
