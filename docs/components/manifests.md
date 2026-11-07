# Component: Manifests

## High-Level Function

A manifest is a TOML file that declares one service: what to run, how many, with what resources, where, and how to update and restart it (spec §6). Parsing, validation and the canonical form are in `crates/valkvork-common/src/validation.rs` and `models.rs`. The CLI validates locally before sending; the controller validates again and is the authority.

## Format

```toml
[service]
id           = "frontend-web"                        # required; DNS label, immutable
image        = "docker.io/library/nginx:1.27.2-alpine-slim"   # required; OCI reference, digest allowed
replicas     = 3                                     # required; >= 0
kill_timeout = "10s"                                 # SIGTERM → SIGKILL grace period

[resources]
cpu    = 0.5          # required; cores, rounded up to the nearest millicore
memory = "512MiB"     # required; >= 4MiB; IEC units KiB, MiB, GiB, TiB
disk   = "1GiB"       # ephemeral alloc directory limit

[constraints]
required_class = "general"     # only nodes whose agent.toml node_class matches

[env]
LOG_LEVEL = "info"             # plaintext: not for secrets

[[ports]]
name           = "http"
container_port = 80
host_port      = 8080          # static host port: one replica per node

[[volumes]]
type      = "host"             # only "host" in this release
source    = "pgdata"           # a [host_volumes.<name>] entry in agent.toml
target    = "/var/lib/postgresql/data"
read_only = false
# single_writer = false        # default true for writable volumes: one live workload at a time,
                               # stop-first updates. false = shared (several workloads may use it)

[autoscale]                    # presence enables autoscaling
min_replicas       = 3
max_replicas       = 10
target_cpu_percent = 75

[update]
max_parallel     = 1           # rolling update and drain concurrency
min_healthy_time = "10s"       # how long a new workload must run before its predecessor stops

[restart]
attempts = 3                   # local restarts allowed ...
interval = "5m"                # ... within this window
delay    = "5s"                # wait before each restart
```

| Key | Default |
|---|---|
| `service.kill_timeout` | `"10s"` |
| `resources.disk` | `"1GiB"` |
| `constraints.required_class` | none: any node |
| `update.max_parallel`, `update.min_healthy_time` | `1`, `"10s"` |
| `restart.attempts`, `restart.interval`, `restart.delay` | `3`, `"5m"`, `"5s"` |

Durations are `<n>s`, `<n>m`, `<n>h`, or a bare number of seconds. Sizes are an integer with an IEC unit, or a bare number of bytes. `examples/` has ready-to-apply manifests.

`cpu` is both the scheduler's reservation and a hard limit: `cpu = 0.5` reserves 500 millicores on a node and caps the container at half a core's worth of CPU time across all cores. `memory` likewise is reserved, and the container is OOM-killed above it, with no swap.

## Validation

Validation runs before any database write. A failure writes nothing, returns gRPC `INVALID_ARGUMENT` listing every error with a machine-readable code and the TOML field path, and makes the CLI exit with code 2 (spec §6.2).

| Rule | Checks | Code |
|---|---|---|
| VE-01 | `cpu > 0`, `memory ≥ 4MiB`, `disk > 0`, `replicas ≥ 0`, valid durations and sizes, `max_parallel ≥ 1` | `INVALID_RESOURCE_SPEC` |
| VE-02 | `image` is a valid OCI reference (`[registry[:port]/]path[:tag][@sha256:digest]`). A missing tag becomes `:latest` with a **warning** | `INVALID_IMAGE_REF` |
| VE-03 | `service.id` matches `^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$` | `INVALID_SERVICE_ID` |
| VE-04 | `1 ≤ min_replicas ≤ max_replicas`, `0 < target_cpu_percent < 100` | `INVALID_AUTOSCALE_SPEC` |
| VE-05 | Ports in 1–65535; `host_port` unique within the manifest; `host_port < 1024` is a **warning** | `INVALID_PORT_SPEC` |
| VE-06 | No unknown keys or tables (`deny_unknown_fields`). The path names the table, e.g. `service.replica` or `ports.hostport` | `UNKNOWN_FIELD` |
| VE-07 | Each volume has a known `type`, a source, an absolute `target`, and a unique `target` | `INVALID_VOLUME_SPEC` |
| VW-01 | **Warning** at apply or plan: no current healthy node of the class could fit one replica, or none has the host volumes | `UNSATISFIABLE_NOW` |

Example:

```
$ valkvork plan -f bad.toml
validation failed:
  UNKNOWN_FIELD at service.replica: unknown field `replica`, expected one of `id`, `image`, `replicas`, `kill_timeout`
```

### About `:latest`

`:latest` is allowed; the warning is about updates. Re-applying an identical manifest changes nothing, so it never pulls a newer image. New containers of a `:latest` service always pull, so replicas started at different times can run different builds. Pin a tag or a digest for anything you want to roll out and roll back predictably.

## Canonical Form and Hash

A valid manifest becomes a `ServiceSpec`: fixed field order, env in sorted order, CPU in millicores, sizes in bytes, durations in seconds, defaults filled in, and a missing tag made explicit as `:latest`. The controller stores it as `spec_json`, and `spec_hash` is the SHA-256 of that JSON. Two manifests that differ only in formatting, comments or key order have the same hash.

## Change Classification

Applying to an existing service compares the stored spec with the new one (FR-MAN-02):

| Change | When | Effect |
|---|---|---|
| `created` | No such service | Version 1; `target_replicas` = `replicas`, clamped to `[min, max]` with autoscaling |
| `unchanged` | Same hash, and target equals `replicas` (or the service is autoscaled) | Nothing written, nothing sent (AT-01) |
| `scale-only` | Only `replicas` differs, or a manual scale drifted from the manifest | `target_replicas` updated; version unchanged |
| `policy-only` | `[autoscale]` or `[update]` differs | Stored; running workloads untouched; version unchanged |
| `replacing` | Anything else: image, resources, env, ports, volumes, restart policy, kill timeout, class | `version` + 1 and a rolling update |

Without `[autoscale]`, an apply restores the manifest's `replicas` after a manual `valkvork scale`, and `plan` shows that drift. With `[autoscale]`, `replicas` is only the initial count: later applies clamp the current target into `[min, max]` but never reset it (FR-MAN-04).

An apply also clears a halted rollout and a DEGRADED status, so it can retry or supersede them.

## Plan

`valkvork plan -f` runs the same parse, validation and classification, plus a placement check against current capacity, and writes nothing (FR-MAN-03):

```
$ valkvork plan -f examples/api.toml
~ service "api"   version 1 → 2   [rolling update, max_parallel=1]
    env: {"RELEASE": "v1"} → {"RELEASE": "v2"}
    replicas: 2 (unchanged)
  placement: 2/2 workloads placeable
```

For a replacing change, the placement check counts every live workload, since each gets a replacement; for a scale-up, only the new ones.
