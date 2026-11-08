# Component: Container Runtime

## High-Level Function

The agent reaches the container runtime only through the `ContainerRuntime` trait (FR-AGE-09). No Docker type leaks out of `crates/valkvork-agent/src/runtime.rs`, so a second driver can be added without touching the supervisor, the controller, the proto or manifests. The one implementation in this release is `DockerRuntime`, which talks to the Docker Engine API over its Unix socket using `bollard`.

## The Trait

```rust
#[async_trait]
pub trait ContainerRuntime: Send + Sync + 'static {
    async fn version(&self) -> Result<String>;                  // "docker/29.5.2", for Register.runtime
    async fn host_resources(&self) -> Result<(u32, u64)>;       // millicores, bytes
    async fn data_root(&self) -> Option<PathBuf> { None }       // never mountable
    async fn pull(&self, image: &str, auth: Option<RegistryCredentials>) -> Result<()>;
    async fn create(&self, spec: &ContainerSpec) -> Result<String>;   // idempotent by name
    async fn start(&self, id: &str) -> Result<()>;
    async fn stop(&self, id: &str, timeout_secs: u32) -> Result<()>;
    async fn remove(&self, id: &str) -> Result<()>;
    async fn list_by_label(&self, label: &str) -> Result<Vec<ContainerSummary>>;
    async fn inspect(&self, id: &str) -> Result<ContainerDetails>;
    fn events(&self, label: &str) -> BoxStream<'static, Result<RuntimeEvent>>;   // Died, Oom
    fn cgroup_paths(&self, id: &str) -> Vec<PathBuf>;
    async fn writable_layer_size(&self, id: &str) -> Result<u64>;
}
```

`ContainerSpec`, `ContainerSummary`, `ContainerDetails`, `RunState` and `RuntimeEvent` are the driver-neutral types. Errors are `RuntimeError`: `Unavailable`, `ImagePullFailed`, `NotFound`, `AlreadyExists` and `Other`.

## Docker Driver

| Operation | Docker API | Notes |
|---|---|---|
| `version`, `host_resources`, `data_root` | `GET /version`, `GET /info` | `NCPU`, `MemTotal`, `DockerRootDir` |
| `pull` | `POST /images/create` | Skipped when the exact reference is already present, unless the tag is `:latest`. Credentials go in `X-Registry-Auth` |
| `create` | `POST /containers/create?name=vk-<id>` | `409 Conflict` means it already exists; the existing container's id is returned |
| `start` | `POST /containers/{id}/start` | `304` (already running) is success |
| `stop` | `POST /containers/{id}/stop?t=<kill_timeout>` | SIGTERM (or the image's `StopSignal`), wait, SIGKILL. `304` and `404` are success |
| `remove` | `DELETE /containers/{id}?force=true&v=true` | `404` is success |
| `list_by_label` | `GET /containers/json?all=true&filters=label=…` | |
| `inspect` | `GET /containers/{id}/json` | state, exit code, OOM flag |
| `events` | `GET /events` filtered by `type=container`, the label, and `event=die,oom` | `exitCode` from the event attributes |
| `writable_layer_size` | `GET /containers/{id}/json?size=true` | `SizeRw` |
| `cgroup_paths` | none | `/sys/fs/cgroup/system.slice/docker-<id>.scope`, `/sys/fs/cgroup/docker/<id>` |

## Registry Credentials

`registry_auth_file` (default `/etc/valkvork/registry-auth.json`) uses Docker's `config.json` format:

```json
{ "auths": { "ghcr.io": { "auth": "base64(user:password)" } } }
```

The entry is chosen by the image's registry: the part before the first `/` when it contains a `.` or `:` or is `localhost`, else Docker Hub (`docker.io`, `index.docker.io`, `https://index.docker.io/v1/`). `username`/`password` fields are accepted as well as `auth`. A missing file means anonymous pulls.

**Amazon ECR** (`<account>.dkr.ecr.<region>.amazonaws.com/...`) needs no entry. When the file has none for the registry, the agent logs in with `aws ecr get-login-password --region <region>` (agent.toml `secrets.aws_cli`), using the node's instance role, which needs `ecr:GetAuthorizationToken` and read access to the repository (`AmazonEC2ContainerRegistryReadOnly`). The login is reused for 6 hours; ECR's are valid for 12. A failed login fails the start with `IMAGE_PULL_FAILED: ECR login: <cause>`.

## Docker Host Requirements

For the guarantees in the spec, the Docker daemon on workers should use (FR-AGE-01, spec §10.2):

```json
{
  "live-restore": true,
  "exec-opts": ["native.cgroupdriver=systemd"],
  "storage-driver": "overlay2",
  "log-driver": "local",
  "log-opts": { "max-size": "10m", "max-file": "3" }
}
```

- `live-restore` keeps containers running while dockerd restarts or upgrades (AT-15).
- The systemd cgroup driver on cgroup v2 puts containers at the cgroup path the agent reads first.
- dockerd must not listen on TCP; only the agent may use the socket (FR-SEC-06).

## Adding a Driver

The spec plans a native containerd driver (FR-AGE-10), selected with `runtime = "containerd"`. It would implement the same trait, including CNI networking for ports, log capture, exit events and adoption by label, and pass the same tests. Today `agent.toml` accepts only `runtime = "docker"`.
