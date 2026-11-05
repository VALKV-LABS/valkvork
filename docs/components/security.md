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

Every operator action (apply, scale, delete, drain, undrain) writes a `cluster_events` row of kind `AUDIT` with the actor (`cli` or `ui:<audit id>`), the target and what was done. They are kept for 30 days. `valkvork events --kind AUDIT` or the Events page show them.

## The Docker Socket

The Docker socket gives root on the host (FR-SEC-06). Only the agent uses it; dockerd must not listen on TCP. The agent refuses to mount the socket, `/var/lib/docker`, containerd's directories or Docker's actual data root into any workload, even through a host volume or a symlink. See [supervisor.md](supervisor.md#protected-paths). Workloads never run privileged and get no extra capabilities (FR-STO-03).

## Secrets in Manifests

`[env]` values are stored in plaintext in SQLite and appear in `valkvork status <service>` and the UI. Do not put secrets in manifests in this release (FR-SEC-04); secret references are planned.

## Network

The spec's network rules (FR-SEC-05):

| Port | Allow from |
|---|---|
| `50051` | Worker subnets only |
| `50052` | The operator and CI network, or a bastion |
| `9400` | The monitoring system |
| Workload host ports | The load balancer's security group |

The agent opens no inbound ports, apart from metrics on loopback.
