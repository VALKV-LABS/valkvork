# Component: Web UI

## High-Level Function

The controller binary serves an operator UI at `/ui`, as Nomad does, so there is nothing extra to deploy, install or keep version-matched (FR-UI). The server side is `crates/valkvork-controller/src/ui/` and `src/http.rs`; the page assets are `crates/valkvork-controller/ui/`.

## Where It Is Served

The UI shares the admin listener, `:50052`, with `AdminService` (FR-UI-02): one port, one TLS certificate, one firewall rule.

```
request on :50052
  ├── content-type: application/grpc* ──► AdminService (bearer admin token)
  └── anything else ────────────────────► UI router
        GET  /                → 302 /ui/
        GET  /ui/assets/<f>   → embedded asset
        *    /ui/api/...      → JSON API
        GET  /ui/<anything>   → index.html (the page script routes deep links)
```

The split is a tower layer (`UiLayer`) wrapped around tonic's server, which also accepts HTTP/1.1 so browsers work over plaintext in development. With TLS, browsers negotiate HTTP/2. Set `[ui] enabled = false` to turn the UI off; non-gRPC requests then get 404.

## Pages

| Page | URL | Shows | Actions |
|---|---|---|---|
| Overview | `/ui/` | Controller health (version, grace period, uptime, database size, TLS), nodes by status, workloads by status, unplaced PENDING count, autoscaler actions in the last hour, capacity by class with allocation bars and a 60-minute sparkline, the last 20 events | |
| Services | `/ui/services` | Id, image, version, running/target, CPU, status, rollout status | |
| Service detail | `/ui/services/<id>` | Status, rollout progress, a 60-minute CPU sparkline, workloads with node, status, reason and restarts, scaling events, the canonical spec as TOML | **Scale**; **Delete**, enabled only once you type the service id |
| Nodes | `/ui/nodes` | Id, hostname, class, status and draining, CPU and memory allocation bars, workload count, heartbeat age, agent and runtime versions | |
| Node detail | `/ui/nodes/<id>` | Capacity and allocation, address, host volumes, the workloads on the node | **Drain** (with a confirmation) / **Undrain** |
| Workloads | `/ui/workloads?service=&node=&status=` | Every workload, filterable | |
| Events | `/ui/events?kind=` | Cluster events, newest first, filterable by kind | |
| Deploy | `/ui/deploy` | Paste or upload a manifest | **Plan** (diff, placement check, warnings or validation errors), then **Apply**, enabled only after a successful plan of the same text |

Every page has a deep-linkable URL. Status is always text plus colour, never colour alone. Light and dark themes follow `prefers-color-scheme`. The layout targets 1024 px and wider (FR-UI-08).

## Sign-in, Sessions and CSRF

(FR-UI-06)

1. The operator signs in with the admin token: `POST /ui/api/login {"token": "…"}`. A wrong token costs a 500 ms delay and gets 401.
2. The controller creates a session with 32 random bytes for the id and another 32 for a CSRF token, and sets:
   ```
   Set-Cookie: vk_session=<id>; Path=/ui; HttpOnly; SameSite=Strict; Max-Age=43200[; Secure]
   ```
   `Secure` is set when the listener uses TLS. The lifetime is `[ui] session_ttl_seconds` (12 h).
3. The page keeps the CSRF token in memory (never in storage) and sends it as `X-Valkvork-CSRF` on every mutating request. A mutating request without it, or with the wrong one, gets **403**.
4. Sessions live in memory, at most 256 (the one closest to expiry is evicted). A controller restart signs everyone out.

Each session also has a separate random **audit id**. Every action from the UI is recorded in `cluster_events` with actor `ui:<audit id>`. The audit id is not the cookie value, so the event log never reveals a usable session. Apply, scale, delete and drain write the same events whether they come from the CLI or the UI, apart from the actor (AT-16).

## JSON API

Internal and unversioned (FR-UI-05); scripts should use the gRPC API or the CLI. Every endpoint except `session` and `login` needs a session; every `POST` needs the CSRF header.

| Method and path | Does |
|---|---|
| `GET /ui/api/session` | `{authenticated, csrf?}` |
| `POST /ui/api/login`, `POST /ui/api/logout` | Start or end a session |
| `GET /ui/api/overview` | Overview counts, capacity by class with series, last 20 events, controller info |
| `GET /ui/api/services`, `GET /ui/api/services/<id>` | Service list; detail with CPU series and scaling events |
| `POST /ui/api/services/<id>/scale` `{replicas}` | Manual scale |
| `POST /ui/api/services/<id>/delete` `{confirm}` | Delete; `confirm` must equal the id |
| `GET /ui/api/nodes`, `GET /ui/api/nodes/<id>` | Node list; detail with its workloads |
| `POST /ui/api/nodes/<id>/drain`, `…/undrain` | Drain or undrain |
| `GET /ui/api/workloads?service=&node=&status=` | Workloads |
| `GET /ui/api/cluster-events?kind=&limit=` | Events (default 100, max 1000) |
| `POST /ui/api/plan`, `POST /ui/api/apply` `{manifest}` | Plan or apply a manifest |
| `GET /ui/api/events` | Server-Sent Events stream |

Errors are JSON `{error, errors?}` with 400 (validation; `errors` lists `{code, field, message}`), 401, 403, 404, 409 (service being deleted), 413 (body over 1 MiB) or 500.

## Live Updates

(FR-UI-04, FR-UI-09)

The page opens `EventSource('/ui/api/events')`. After every commit that changed state, the server sends `event: change` with a sequence number. The page waits 300 ms to coalesce a burst, then re-fetches the current page, unless the operator is typing in a form on it. The stream also sends a comment every 15 s to keep proxies from closing it, and `retry: 5000`.

If the stream drops, the page polls every 5 s until it reconnects. The header shows `● live` or `○ polling`.

Each client reads from its own bounded slot in a broadcast channel. A client that falls behind is disconnected and reconnects; the control loop never waits for a browser.

## Assets and Security Headers

(FR-UI-01)

- Hand-written HTML, CSS and ES-module JavaScript, with no third-party code, CDNs, web fonts or npm step. Bars and sparklines are inline SVG drawn by the page script.
- `build.rs` embeds every file in `ui/` with `include_bytes!` plus a gzip copy made at build time, and fails the build if the total exceeds 250 KB (it is about 32 KB). Assets are served gzipped when the browser accepts it, with an ETag and `Cache-Control: no-cache`.
- The page script builds the DOM with `createElement` and `textContent` only, never `innerHTML`, because service ids, env values and event messages are operator-controlled.
- Every response, including errors, carries:

  ```
  Content-Security-Policy: default-src 'self'; img-src 'self' data:; frame-ancestors 'none'
  X-Content-Type-Options: nosniff
  X-Frame-Options: DENY
  Referrer-Policy: no-referrer
  ```

  There are no inline scripts or `style` attributes, so the policy holds without `unsafe-inline`. The UI works unchanged in air-gapped networks.

## Chart Data

The core keeps 60 one-minute samples per service (CPU utilisation) and per node class (CPU and memory allocation), in memory only (FR-UI-07). The first point appears a minute after the controller starts. Longer history belongs in Prometheus; see [metrics.md](metrics.md).

## Changing the UI

Edit the files in `crates/valkvork-controller/ui/` and rebuild; `build.rs` re-embeds them. `make dev-up` and a browser reload is the quickest loop (see [local-development.md](../local-development.md)). Keep to the rules above: no inline script or style, no `innerHTML`, no third-party code.
