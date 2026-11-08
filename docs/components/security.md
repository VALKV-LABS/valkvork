# Component: Security

## High-Level Function

This page collects how Valkvork authenticates each party and limits what each can do (spec §9).

| Path | Protected by |
|---|---|
| Agent → controller (`:50051`) | TLS, with the agent verifying the controller's certificate against its CA bundle, plus a **join token** in the first message |
| CLI and CI → controller (`:50052`, gRPC) | TLS plus a bearer **admin token** |
| Browser → controller (`:50052`, `/ui`) | TLS, admin-token sign-in, a session cookie and a CSRF header |
| Metrics (`:9400`, `127.0.0.1:9401`) | Network placement only: no authentication |
| Controller → agent | Never: commands travel down the agent's own stream |
| Users → workloads | Not Valkvork's concern: traffic goes from your load balancer to host ports |

## Process Permissions

What each process runs as, what it can touch, and why. "Golden image" means the systemd units and paths from [deployment.md](../deployment.md#golden-image).

### Summary

| Process | Runs as | Linux capabilities | Network | Writes |
|---|---|---|---|---|
| `valkvork-controller` | Unprivileged `valkvork` user (uid 65532 in the image) | None | Listens on 50051, 50052, 9400. Makes no outbound connections | Its SQLite directory only |
| `valkvork-agent` | **root** | All (root) | One outbound connection to the controller. Listens on 127.0.0.1:9401 only | `data_dir` (node id, alloc dirs, failure markers). Through Docker: anything a container can be told to do |
| Workload containers | The image's `USER` (often root inside the container) | Docker's default set | Docker bridge network, host ports bound on all addresses | Their own filesystem, `/alloc`, their host volumes |
| `valkvork` CLI | The operator | None | One outbound connection to the admin API | Nothing |
| Docker Engine (`dockerd`, `containerd`) | root | All | Unix socket only (must not listen on TCP) | Its data root |

### Controller

The controller needs no privileges: its ports are above 1024, and it never touches containers, the Docker socket or other hosts.

On the golden image, `valkvork-controller.service` runs it as the `valkvork` system user, with this systemd sandbox:

```ini
User=valkvork
Group=valkvork
NoNewPrivileges=true      # setuid binaries cannot raise privileges
PrivateTmp=true           # its own /tmp
ProtectSystem=strict      # the whole filesystem is read-only to it ...
ReadWritePaths=/data/valkvork   # ... except its database directory
ProtectHome=true          # no access to /home, /root
```

| Path | Owner and mode | Access |
|---|---|---|
| `/etc/valkvork.d/controller.toml` | `root:valkvork 0640` | Read |
| `/etc/valkvork.d/secrets/` (TLS key, join and admin tokens, CA) | `root:valkvork 0750`, files `0640` | Read. It cannot change its own secrets |
| `/data/valkvork/db/` | `valkvork:valkvork 0750`; SQLite files `0600` | Read and write |
| `/etc/valkvork.d/cli.env` (operator convenience, holds the admin token) | `root:root 0600` | None: for `sudo -i` sessions only |

The controller image (`valkvlabs/valkvork-controller`) is FROM scratch and runs as uid and gid 65532, with no shell or other binaries; the database volume must be writable by that uid.

What a compromised controller could do: change desired state, so it could run any image it chose on every worker through the agents. It holds the admin and join tokens. It cannot reach workers directly, because agents dial out and it never connects to them, and it has no access to the Docker socket.

### Agent

The agent **runs as root**, deliberately (FR-STO-03):

- It drives the Docker socket. Access to the socket is equivalent to root on the host, so running as a user in the `docker` group would not reduce what it can do.
- It creates per-workload directories under `data_dir` and asks Docker to bind-mount them and the host volumes, so workloads never need privileges to use storage.
- It reads cgroup v2 files for every container and `/proc` for node usage.

`valkvork-agent.service` on the golden image has no `User=` and no sandboxing, and `Requires=docker.service`.

| Path | Owner and mode | Access |
|---|---|---|
| `/etc/valkvork.d/agent.toml`, `secrets/` (join token, CA) | root, `0640` | Read |
| `/etc/valkvork.d/registry-auth.json` | `root 0600` | Read |
| `/var/lib/valkvork/` (`node_id`, `failed/`) | `root 0700` | Read and write |
| `/var/lib/valkvork/alloc` → `/data/valkvork/alloc/<workload_id>/` | `root 0711`; each workload's directory `root 1777` (world-writable with the sticky bit, like `/tmp`, so images that run as a non-root `USER` can write to `/alloc`; it is mounted only into its own container) | Created and deleted by the agent; bind-mounted at `/alloc` |
| `/var/run/docker.sock` | root | Read and write, and only the agent |

Network: the agent opens no listening socket except metrics on loopback (FR-ARC-01); it refuses a non-loopback metrics address. Its only outbound connection is to `controller_endpoint`. Docker pulls images using its own network access.

Limits the agent enforces on itself: it never mounts the Docker socket, `/var/lib/docker`, containerd's directories or Docker's actual data root into a workload (see [supervisor.md](supervisor.md#protected-paths)), and it never creates privileged containers. A compromised agent is root on its worker, but it holds only the join token, so it cannot act as an administrator or reach other workers.

### Workload Containers

The agent creates containers with:

| Setting | Value |
|---|---|
| User | The image's `USER`. Valkvork does not override it |
| Privileged | Never; there is no option for it (FR-STO-03) |
| Capabilities | Docker's default set (for example `CHOWN`, `DAC_OVERRIDE`, `NET_BIND_SERVICE`, `SETUID`, `SETGID`, `KILL`). None added, none dropped |
| Seccomp, AppArmor | Docker's default profiles |
| Devices | None |
| CPU, memory | `NanoCpus` from `resources.cpu`; `Memory = MemorySwap` from `resources.memory`, so no swap |
| Disk | `/alloc` plus the writable layer, limited by polling (`resources.disk`) |
| Network | Docker's default bridge; each `host_port` is published on all host addresses |
| Mounts | `/alloc` (read-write) and declared host volumes (read-only if either side says so); never the Docker socket |
| Restart policy | `no`: the agent restarts containers itself |
| User namespaces | Not configured: root inside a container is uid 0 on the host, confined by namespaces, capabilities and seccomp |

**`/alloc` is writable by any user.** Each workload's directory is created by the agent as `root 1777` (world-writable with the sticky bit, like `/tmp`), so images that run as a non-root `USER` can write to `/alloc`. It is mounted only into its own container, and the parent `alloc/` stays `root 0711`, so other users on the host cannot list it. Host volume directories are `root 0755` unless the operator sets `owner` and `mode` for them in `agent.toml`, which the agent applies at startup (see [configuration.md](configuration.md#agent)). Images such as Postgres, whose entrypoint starts as root and changes ownership of its data directory, need neither. For an image that starts as a non-root user, set `owner` to its numeric uid. Only the operator, never a manifest, decides who owns a host directory, because volumes outlive services and can be shared between them.

Not set, and worth adding for untrusted images (also tracked there): a PID limit, `no-new-privileges`, dropping capabilities, and a read-only root filesystem. Valkvork assumes workloads are trusted code you deploy; it is not a sandbox for hostile tenants.

### CLI and Browser

The CLI needs no privileges. It reads the manifest files you pass it and holds the admin token only for the request. Anyone with the admin token is a full administrator; there are no roles yet. The web UI runs in the operator's browser with a session cookie it cannot read from JavaScript (`HttpOnly`); see below.

### Local Development

With `make dev-up`, the controller and agent run as **your user**, and the agent uses your Docker's socket (Colima, Docker Desktop, OrbStack), which is root inside that Docker VM. Generated secrets are written `0600` under `.dev/`. Nothing listens beyond `127.0.0.1` except what Docker publishes for workload host ports.

## TLS

Both gRPC listeners use rustls through tonic, with one certificate and key: `tls_cert_file` and `tls_key_file`, PEM, key in PKCS#8. The certificate's subject alternative names must cover the name agents and operators use, for example `controlplane.valkvork.internal`.

Agents trust only the CA bundle in `ca_file` (FR-SEC-01), and the CLI only `--ca-file`. Neither carries a built-in list of public CAs, so a CA bundle is required for every `https://` connection. Without a certificate the controller serves plaintext and logs a warning at startup, which is for development only. The binaries link no OpenSSL.

Per-node mutual TLS, with certificates issued by the controller, is planned (FR-SEC-02).

## Join Token

Every agent sends the join token in `Register`. The controller compares it in constant time and rejects a mismatch with `RegisterAck { accepted: false }`, closing the stream. The token comes from a file (`join_token_file`) that cloud-init or the golden image's bootstrap writes from a secrets store at boot. It is never baked into an image.

Every worker holds the join token, so it grants only the right to register as a node. It never grants admin access.

## Admin Token

The admin token protects `AdminService` and the UI's sign-in. It must be at least 32 random bytes (FR-SEC-03); the controller warns when it is shorter. It is compared in constant time. Keep it separate from the join token: the golden image reads them from separate SSM parameters. There is no RBAC yet; anyone with the token is an administrator.

## Web UI Sessions

See [web_ui.md](web_ui.md#sign-in-sessions-and-csrf) for the details. In short:

- the session id and CSRF token are each 32 random bytes from the OS;
- the cookie is `HttpOnly; SameSite=Strict; Path=/ui`, plus `Secure` under TLS;
- mutating requests need `X-Valkvork-CSRF` and get 403 without it;
- a failed sign-in is delayed 500 ms;
- sessions live in memory for 12 h;
- the audit log records a separate audit id, never the cookie value;
- every response carries a strict Content-Security-Policy, and the page builds its DOM without `innerHTML`.

## Audit

Every operator action (apply, scale, delete, drain, undrain, node remove) writes a `cluster_events` row of kind `AUDIT` with the actor (`cli` or `ui:<audit id>`), the target and what was done. They are kept for 30 days. `valkvork events --kind AUDIT` or the Events page show them.

## The Docker Socket

The Docker socket gives root on the host (FR-SEC-06). Only the agent uses it; dockerd must not listen on TCP. The agent refuses to mount the socket, `/var/lib/docker`, containerd's directories or Docker's actual data root into any workload, even through a host volume or a symlink. See [supervisor.md](supervisor.md#protected-paths). Workloads never run privileged and get no extra capabilities (FR-STO-03).

## Secrets in Manifests

`[env]` values are stored in plaintext in SQLite and appear in `valkvork status <service>` and the UI: never put secrets in `[env]`. Use `[secrets]` references instead (FR-SEC-04; see [manifests.md](manifests.md#secrets)):

- **The controller never sees a value.** It stores and sends only references (`ssm:…`, `file:…`). The database, the API, the CLI, the UI and events show the reference.
- **The agent resolves them at container start.** For `ssm:` it runs the AWS CLI with the node's instance role. `file:` references are allowed only inside agent.toml `secrets.file_dirs`, and are off by default, because the agent runs as root. Values are never logged, written to disk by Valkvork, or reported back. Errors name the reference and the cause.
- **On the node, values are ordinary environment variables.** Root can read them with `docker inspect`, and the workload's processes and their children can read them. Delivery as files on an in-memory filesystem, which avoids both, is planned.
- **Containers share the node's role.** A container can reach the instance metadata service and use the node's role, so any workload on a node can read every parameter that role may read. On a node that also runs the controller, that includes the controller's own secrets. Run only trusted workloads there, and give each node group's role only the prefixes its workloads need.

## Network

The spec's network rules (FR-SEC-05):

| Port | Allow from |
|---|---|
| `50051` | Worker subnets only |
| `50052` | The operator and CI network, or a bastion |
| `9400` | The monitoring system |
| Workload host ports | The load balancer's security group |

The agent opens no inbound ports, apart from metrics on loopback.
