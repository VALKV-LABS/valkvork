# Platform Workloads Plan

**This is the current plan, ahead of everything in [NEXT_STEPS.md](../NEXT_STEPS.md) and
[phase-2.md](phase-2.md).** It builds what Valkvork needs to run the Valkv platform's own
services. Each stage adds the features for one service, then that service runs on the
AWS dev cell from the goldenimage repo and is verified.

The services' manifests and the full list of gaps are in valkv-platform-infra,
`environments/dev-workload-example/` (README.md there). This plan covers the first two:

| Stage | Service | Manifest |
|---|---|---|
| 0 | none: the test bed | |
| 1 | Postgres | `global-pg.toml` |
| 2 | The gateway control plane (apg-admin) | `valkv-apg.toml` |

Later stages are listed [at the end](#after-stage-2) but not planned in detail.

---

## How Each Stage Runs

| Step | Who | What |
|---|---|---|
| 1. Build | Claude | The stage's features, with unit tests and a check on the local dev cluster (`make dev-up`) |
| 2. Release | Claude | Merge to `main`, tag, and release it (`gh workflow run release.yml -f tag=vX.Y.Z`) |
| 3. Image | You | Build the Valkvork golden image with `valkvork_version=X.Y.Z` (goldenimage `docs/deployment.md`, step 1) |
| 4. Cell | You | Apply `environments/aws-us-west-1-valkvork`, or replace its instances with the new image |
| 5. Workload | Together | `valkvork plan`, then `valkvork apply` the stage's manifest |
| 6. Verify | Together | Every check in the stage's table; results recorded in this file |

A stage is done when every check passes on AWS. The next stage starts after that.

Steps 3 and 4 take about half an hour per change. Stage 0 adds a faster path for fixes
found during verification.

---

## Stage 0: The Test Bed

No Valkvork features. Infrastructure and tooling that Stages 1 and 2 need.

| # | Change | Where | Why |
|---|---|---|---|
| 0.1 | Node groups with their own `node_class` and `host_volumes`: the controller node and the agent nodes set separately | goldenimage `environments/template-valkvork` | Today every node gets the same class and host volumes. Postgres's `pgdata` volume must be declared on exactly one node, or after a failure it can start elsewhere with an empty directory |
| 0.2 | Let nodes read workload secrets: a `workload_ssm_prefixes` input added to the nodes' IAM, e.g. `/valkv/<cell>/workloads/` | goldenimage template | Node roles can read only Valkvork's own parameters today. Stage 1's agent resolves secrets with the instance role |
| 0.3 | Workload secrets created in SSM: `POSTGRES_PASSWORD` and `APG_SECRET_KEY`, `APG_ADMIN_TOKEN`, `APG_NODE_TOKEN` under that prefix | You, with `aws ssm put-parameter` | The manifests reference them |
| 0.4 | A dev-cell upgrade script: copy unreleased binaries to the nodes over SSM and restart the agent and controller | valkvork `scripts/` | Lets a fix be tried in minutes, without an image build. Releases still go through the image |

**Cell layout for Stages 1 and 2** (`aws-us-west-1-valkvork`, two nodes):

| Node | Class | Host volumes |
|---|---|---|
| Controller node (controller + agent) | `app` | `pgdata=/data/postgresql` |
| Agent node | `app` | none |

**Verify:**
- [ ] `valkvork node list` shows both nodes as class `app`.
- [ ] `valkvork node list -o json` lists `pgdata` in `host_volumes` on the controller node only.
- [ ] From the agent node, `aws ssm get-parameter --with-decryption --name /valkv/<cell>/workloads/POSTGRES_PASSWORD` works.
- [ ] The same call for `/valkv/<cell>/valkvork/controller/key` is denied.

---

## Stage 1: Postgres (`global-pg.toml`)

### Features

| # | Feature | Manifest | Notes |
|---|---|---|---|
| 1.1 | **Secret references.** Resolved by the agent with the instance role when it starts the container, and passed as environment variables. | `[secrets]`<br>`POSTGRES_PASSWORD = "ssm:/valkv/<cell>/workloads/POSTGRES_PASSWORD"` | Design in [phase-2.md](phase-2.md#secret-references). See below |
| 1.2 | **Bind address per port.** | `host_ip = "advertise"` (the node's private address), or an IP such as `"127.0.0.1"` | Default stays `0.0.0.0`, for compatibility. These instances have public IPs |
| 1.3 | **Health checks:** `tcp` and `http`. | `[check]` | Design in [phase-2.md](phase-2.md#application-health-checks). Health shows in `status`, the API, the UI and metrics. Rollouts wait for healthy. Repeated failures restart the container |
| 1.4 | **`command` and `args`.** | `[service]`<br>`args = ["-c", "wal_level=logical"]` | Small. Stage 2's init job needs `command` |

**What 1.1 must do:**
- The controller stores, returns and shows only references, never values. The API, `valkvork status`, the UI, events, the database and logs all carry the reference.
- Two schemes: `ssm:` (SSM Parameter Store, decrypted) and `file:` (a path on the node, for the local dev cluster and tests).
- An unresolvable reference fails the start with a clear event naming the reference, not the value. The restart policy applies.
- A changed reference is a replacing change. A changed value in SSM is picked up at the next container start.
- Values are never written to disk by Valkvork. They are passed to Docker only, so `docker inspect` on the node shows them. That's acceptable for now, because only root can run it.

**Manifest changes:** `global-pg.toml` uses all four features.

### Verify on AWS

**Placement and secrets**
- [ ] `valkvork plan -f global-pg.toml` places it on the controller node, the only one with `pgdata`.
- [ ] Postgres starts with the password from SSM. `psql` from the agent node, with that password, connects.
- [ ] The password appears nowhere in `valkvork status global-pg`, the UI, or `valkvork events`. `grep -a` for it finds nothing under `/data/valkvork/db/` (the database and its WAL). Only the reference appears.
- [ ] A wrong SSM path fails the start with an event naming the path. Correcting the manifest recovers.

**Network**
- [ ] `ss -ltn` on the node shows 5432 on the private address only (`host_ip = "advertise"`). The port is not reachable on the public IP.

**Health**
- [ ] `status` shows the workload healthy.
- [ ] Stopping the postgres process inside the container makes it unhealthy, then restarted.

**Data survives**
- [ ] A table created before each of these is still there after:
  - a `valkvork apply` with a changed `args` (stop-first update: the old container is gone before the new one starts);
  - `systemctl restart valkvork-agent` (adoption, not a restart);
  - a reboot of the node.
- [ ] `SHOW wal_level` returns `logical`.

---

## Stage 2: The Gateway Control Plane (`valkv-apg.toml`)

### Features

| # | Feature | Manifest | Notes |
|---|---|---|---|
| 2.1 | **Secrets inside a value**, for URLs that embed a password. | `[env]`<br>`APG_DATABASE_URL = "postgresql://valkv:${secret:POSTGRES_PASSWORD}@postgres.<zone>:5432/valkv_apg"` | Builds on 1.1. Resolved by the agent. Any value that uses a secret is treated as secret, and shown with the secret masked |
| 2.2 | **Batch jobs**: run to completion. | `[service]`<br>`type = "batch"` | Design in [phase-2.md](phase-2.md#batch-and-system-jobs). Exit 0 is `COMPLETED` with no restart; failures retry under the restart policy. A rule for keeping finished runs. Needed for `global-pg-init`, which creates `valkv_apg` and the other databases idempotently |
| 2.3 | **An address for Postgres.** | `postgres.<zone>` in the URL | **Infrastructure, not Valkvork**, for now: the goldenimage template adds a private DNS record for the node that declares `pgdata`. Service discovery in Valkvork comes later |
| 2.4 | **Loopback-only admin port**, from 1.2. | `host_ip = "127.0.0.1"` on port 9000 | Nothing new to build; this stage verifies it |

**New manifest:** `global-pg-init.toml`. A batch job using the postgres image and
`command`, which creates the databases (`valkv_apg`, `valkv_ingress`, `valkv_krocks`,
`valkv_cloudlaunch`, `valkv_supavisor`) and the `supavisor` role. It replaces compose's
three `*-db-init` services.

### Verify on AWS

**Init job**
- [ ] `global-pg-init` runs once and shows `COMPLETED`; the databases exist.
- [ ] Re-applying it runs again and still completes (it's idempotent).
- [ ] If Postgres is down, the job retries under its restart policy, then completes once Postgres is up.

**apg-admin**
- [ ] It starts, runs its migrations into `valkv_apg`, and is healthy.
- [ ] Port 9001 is reachable from the VPC. Port 9000 is not, and listens on `127.0.0.1` only.
- [ ] An SSM port forward to the node's `127.0.0.1:9000` opens the console, and the bootstrap admin token logs in.
- [ ] `APG_DATABASE_URL` and the three secrets appear nowhere in `status`, the UI, events or the database. Only references or masked values do.

**Recovery**
- [ ] Updating `global-pg` (a short Postgres outage): apg-admin reconnects, or restarts under its policy, and is healthy again without intervention.
- [ ] Rebooting the node that runs both: both come back with no manual steps, whichever starts first.

---

## Decisions (2026-11-07)

**1. Containers can use the node's role: accepted for Stages 1 and 2.**
- The golden image allows containers to reach the instance metadata service (hop limit 2), and some platform services need the role, such as global-cp launching EC2 instances.
- So any container on a node can read whatever that node's role can, including every workload secret under the prefix.
- On the controller node, that includes the controller's TLS key and admin token.
- Mitigation: run only trusted platform services on the controller node. Per-workload credentials come later.

**2. Secrets as environment variables, for now.** Files are the more secure delivery:
- A variable is visible in `docker inspect`, in the process's `/proc/<pid>/environ`, and to child processes, and it can end up in crash reports and debug output.
- A file on an in-memory filesystem, readable only by the container's user, avoids all of that and can be rotated without a restart.
- Variables come first because every service here reads its secrets from them, while only some images read secrets from files (Postgres can, with `POSTGRES_PASSWORD_FILE`).
- File delivery is a planned follow-up (NEXT_STEPS.md, "Re-evaluate").

**3. No cache of resolved secrets.**
- A container restart while SSM is unreachable fails, and the restart policy retries it.
- Revisit if it shows up in testing.

**4. Bind address: `0.0.0.0` stays the default.**
- Ports opt in per port with `host_ip`, so existing manifests behave the same.
- Re-evaluate after Stage 2: making `advertise` the default (NEXT_STEPS.md, "Re-evaluate").

---

## After Stage 2

In the order the gap list suggests. Each gets its own stage plan when Stage 2 is done.

| Stage | Service | Main features |
|---|---|---|
| 3 | `global-cp` | Per-node values in `[env]` (`${node.advertise_ip}`, `${node.id}`), service addresses, its migration as a batch job |
| 4 | `cachedb-cp`, `portal` | Mostly covered by Stage 3 |
| 5 | `supavisor` | System jobs (one per node), host networking, log shipping |
| 6 | `sqldb-node` | Stable node identity, log shipping |

## Results

Record each stage's verification here: the date, the Valkvork version, the image, and
anything that failed and was fixed.

| Stage | Date | Version | Result |
|---|---|---|---|
| 0 | | | |
| 1 | | | |
| 2 | | | |
