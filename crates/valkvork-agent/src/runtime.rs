//! Container runtime abstraction (FR-AGE-09) and the Docker Engine driver.
//!
//! Everything outside this module sees only the runtime-agnostic types below;
//! no `bollard` type leaks out.

use async_trait::async_trait;
use bollard::container::{
    Config, CreateContainerOptions, InspectContainerOptions, ListContainersOptions,
    RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::errors::Error as DockerError;
use bollard::image::CreateImageOptions;
use bollard::models::{
    HostConfig, Mount, MountTypeEnum, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::system::EventsOptions;
use bollard::Docker;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use valkvork_common::errors::RuntimeError;

/// Label every Valkvork container carries (adoption key, FR-AGE-07).
pub const LABEL_WORKLOAD: &str = "valkvork.workload_id";

#[derive(Debug, Clone)]
pub struct BindMount {
    pub source: PathBuf,
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedPort {
    pub container_port: u16,
    pub host_port: u16,
    /// Host address to bind; None for every address.
    pub host_ip: Option<String>,
}

/// What to create (FR-AGE-04).
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub cpu_millicores: u32,
    pub memory_bytes: u64,
    pub env: Vec<(String, String)>,
    /// None: the image's.
    pub entrypoint: Option<Vec<String>>,
    /// None: the image's.
    pub command: Option<Vec<String>>,
    /// TCP ports to publish.
    pub ports: Vec<PublishedPort>,
    pub mounts: Vec<BindMount>,
    pub labels: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Created,
    Running,
    Restarting,
    Exited,
    Other,
}

#[derive(Debug, Clone)]
pub struct ContainerSummary {
    pub id: String,
    pub labels: HashMap<String, String>,
    pub state: RunState,
}

#[derive(Debug, Clone)]
pub struct ContainerDetails {
    pub id: String,
    pub state: RunState,
    pub exit_code: i64,
    pub oom_killed: bool,
}

#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// The container's main process exited.
    Died {
        container_id: String,
        workload_id: String,
        exit_code: i64,
    },
    /// The kernel OOM killer fired inside the container.
    Oom {
        container_id: String,
        workload_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct RegistryCredentials {
    pub username: String,
    pub password: String,
    pub server: String,
}

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[async_trait]
pub trait ContainerRuntime: Send + Sync + 'static {
    /// Driver and version for `Register.runtime`, e.g. `docker/27.3.1`.
    async fn version(&self) -> Result<String>;
    /// Total CPU (millicores) and memory (bytes) of the host the runtime uses.
    async fn host_resources(&self) -> Result<(u32, u64)>;
    /// Where the runtime keeps its state; never mountable into a workload
    /// (FR-SEC-06).
    async fn data_root(&self) -> Option<PathBuf> {
        None
    }
    async fn pull(&self, image: &str, auth: Option<RegistryCredentials>) -> Result<()>;
    /// Create; returns the container id. An existing container with the same
    /// name is returned instead of an error (idempotent create).
    async fn create(&self, spec: &ContainerSpec) -> Result<String>;
    async fn start(&self, id: &str) -> Result<()>;
    /// SIGTERM, wait `timeout_secs`, SIGKILL. Missing containers are fine.
    async fn stop(&self, id: &str, timeout_secs: u32) -> Result<()>;
    async fn remove(&self, id: &str) -> Result<()>;
    async fn list_by_label(&self, label: &str) -> Result<Vec<ContainerSummary>>;
    async fn inspect(&self, id: &str) -> Result<ContainerDetails>;
    /// Exit/OOM events for containers with `label`.
    fn events(&self, label: &str) -> BoxStream<'static, Result<RuntimeEvent>>;
    /// cgroup v2 directory candidates for a container (FR-AGE-03).
    fn cgroup_paths(&self, id: &str) -> Vec<PathBuf>;
    /// Writable layer size (`SizeRw`), expensive (FR-AGE-08).
    async fn writable_layer_size(&self, id: &str) -> Result<u64>;
}

// ============================================================================
// Docker Engine driver
// ============================================================================

pub struct DockerRuntime {
    docker: Docker,
}

impl DockerRuntime {
    pub fn connect(socket: &Path) -> Result<Self> {
        let docker =
            Docker::connect_with_unix(&socket.to_string_lossy(), 120, bollard::API_DEFAULT_VERSION)
                .map_err(|e| RuntimeError::Unavailable(e.to_string()))?;
        Ok(Self { docker })
    }
}

fn status_code(e: &DockerError) -> Option<u16> {
    match e {
        DockerError::DockerResponseServerError { status_code, .. } => Some(*status_code),
        _ => None,
    }
}

fn map_err(e: DockerError) -> RuntimeError {
    match &e {
        DockerError::DockerResponseServerError {
            status_code: 404,
            message,
        } => RuntimeError::NotFound(message.clone()),
        DockerError::DockerResponseServerError {
            status_code: 409,
            message,
        } => RuntimeError::AlreadyExists(message.clone()),
        DockerError::DockerResponseServerError { .. } => RuntimeError::Other(e.to_string()),
        _ => RuntimeError::Unavailable(e.to_string()),
    }
}

fn run_state(s: Option<&str>) -> RunState {
    match s.unwrap_or("") {
        "created" => RunState::Created,
        "running" => RunState::Running,
        "restarting" => RunState::Restarting,
        "exited" | "dead" => RunState::Exited,
        _ => RunState::Other,
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn version(&self) -> Result<String> {
        let v = self.docker.version().await.map_err(map_err)?;
        Ok(format!(
            "docker/{}",
            v.version.unwrap_or_else(|| "unknown".into())
        ))
    }

    async fn host_resources(&self) -> Result<(u32, u64)> {
        let info = self.docker.info().await.map_err(map_err)?;
        let cpus = info.ncpu.unwrap_or(1).max(1) as u32;
        let mem = info.mem_total.unwrap_or(0).max(0) as u64;
        Ok((cpus * 1000, mem))
    }

    async fn data_root(&self) -> Option<PathBuf> {
        self.docker
            .info()
            .await
            .ok()?
            .docker_root_dir
            .map(PathBuf::from)
    }

    async fn pull(&self, image: &str, auth: Option<RegistryCredentials>) -> Result<()> {
        // Skip the network when the exact reference is already present.
        if !image.ends_with(":latest") && self.docker.inspect_image(image).await.is_ok() {
            return Ok(());
        }
        let creds = auth.map(|a| bollard::auth::DockerCredentials {
            username: Some(a.username),
            password: Some(a.password),
            serveraddress: Some(a.server),
            ..Default::default()
        });
        let opts = CreateImageOptions {
            from_image: image.to_string(),
            ..Default::default()
        };
        let mut stream = self.docker.create_image(Some(opts), None, creds);
        while let Some(item) = stream.next().await {
            match item {
                Ok(info) => {
                    if let Some(err) = info.error {
                        return Err(RuntimeError::ImagePullFailed(err));
                    }
                }
                Err(e) if status_code(&e).is_some() => {
                    return Err(RuntimeError::ImagePullFailed(e.to_string()))
                }
                Err(e) => return Err(RuntimeError::Unavailable(e.to_string())),
            }
        }
        Ok(())
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<String> {
        let mut port_bindings = HashMap::new();
        let mut exposed = HashMap::new();
        for p in &spec.ports {
            let key = format!("{}/tcp", p.container_port);
            exposed.insert(key.clone(), HashMap::new());
            port_bindings.insert(
                key,
                Some(vec![PortBinding {
                    host_ip: p.host_ip.clone(),
                    host_port: Some(p.host_port.to_string()),
                }]),
            );
        }
        let mounts = spec
            .mounts
            .iter()
            .map(|m| Mount {
                typ: Some(MountTypeEnum::BIND),
                source: Some(m.source.to_string_lossy().into_owned()),
                target: Some(m.target.clone()),
                read_only: Some(m.read_only),
                ..Default::default()
            })
            .collect();
        let host_config = HostConfig {
            nano_cpus: Some(spec.cpu_millicores as i64 * 1_000_000),
            // Memory == MemorySwap: no swap.
            memory: Some(spec.memory_bytes as i64),
            memory_swap: Some(spec.memory_bytes as i64),
            port_bindings: Some(port_bindings),
            mounts: Some(mounts),
            // FR-AGE-04: the agent owns restarts.
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::NO),
                maximum_retry_count: None,
            }),
            privileged: Some(false),
            ..Default::default()
        };
        let config = Config {
            image: Some(spec.image.clone()),
            env: Some(spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
            entrypoint: spec.entrypoint.clone(),
            cmd: spec.command.clone(),
            labels: Some(spec.labels.clone()),
            exposed_ports: Some(exposed),
            host_config: Some(host_config),
            ..Default::default()
        };
        let opts = CreateContainerOptions {
            name: spec.name.clone(),
            platform: None,
        };
        match self.docker.create_container(Some(opts), config).await {
            Ok(r) => Ok(r.id),
            // Name taken: the container already exists (idempotent create).
            Err(e) if status_code(&e) == Some(409) => {
                let existing = self
                    .docker
                    .inspect_container(&spec.name, None)
                    .await
                    .map_err(map_err)?;
                existing
                    .id
                    .ok_or_else(|| RuntimeError::Other("existing container has no id".into()))
            }
            Err(e) => Err(map_err(e)),
        }
    }

    async fn start(&self, id: &str) -> Result<()> {
        match self
            .docker
            .start_container(id, None::<StartContainerOptions<String>>)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if status_code(&e) == Some(304) => Ok(()), // already running
            Err(e) => Err(map_err(e)),
        }
    }

    async fn stop(&self, id: &str, timeout_secs: u32) -> Result<()> {
        match self
            .docker
            .stop_container(
                id,
                Some(StopContainerOptions {
                    t: timeout_secs as i64,
                }),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if matches!(status_code(&e), Some(304) | Some(404)) => Ok(()),
            Err(e) => Err(map_err(e)),
        }
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let opts = RemoveContainerOptions {
            force: true,
            v: true,
            link: false,
        };
        match self.docker.remove_container(id, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(e) if status_code(&e) == Some(404) => Ok(()),
            Err(e) => Err(map_err(e)),
        }
    }

    async fn list_by_label(&self, label: &str) -> Result<Vec<ContainerSummary>> {
        let opts = ListContainersOptions {
            all: true,
            filters: HashMap::from([("label".to_string(), vec![label.to_string()])]),
            ..Default::default()
        };
        let list = self
            .docker
            .list_containers(Some(opts))
            .await
            .map_err(map_err)?;
        Ok(list
            .into_iter()
            .filter_map(|c| {
                Some(ContainerSummary {
                    id: c.id?,
                    labels: c.labels.unwrap_or_default(),
                    state: run_state(c.state.as_deref()),
                })
            })
            .collect())
    }

    async fn inspect(&self, id: &str) -> Result<ContainerDetails> {
        let r = self
            .docker
            .inspect_container(id, None)
            .await
            .map_err(map_err)?;
        let st = r.state.unwrap_or_default();
        Ok(ContainerDetails {
            id: r.id.unwrap_or_default(),
            state: run_state(st.status.map(|s| s.to_string()).as_deref()),
            exit_code: st.exit_code.unwrap_or(0),
            oom_killed: st.oom_killed.unwrap_or(false),
        })
    }

    fn events(&self, label: &str) -> BoxStream<'static, Result<RuntimeEvent>> {
        let opts = EventsOptions::<String> {
            filters: HashMap::from([
                ("type".to_string(), vec!["container".to_string()]),
                ("label".to_string(), vec![label.to_string()]),
                (
                    "event".to_string(),
                    vec!["die".to_string(), "oom".to_string()],
                ),
            ]),
            ..Default::default()
        };
        self.docker
            .events(Some(opts))
            .filter_map(|ev| async move {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(e) => return Some(Err(RuntimeError::Unavailable(e.to_string()))),
                };
                let actor = ev.actor?;
                let attrs = actor.attributes.unwrap_or_default();
                let container_id = actor.id?;
                let workload_id = attrs.get(LABEL_WORKLOAD)?.clone();
                match ev.action.as_deref()? {
                    "die" => {
                        let exit_code = attrs
                            .get("exitCode")
                            .and_then(|c| c.parse().ok())
                            .unwrap_or(-1);
                        Some(Ok(RuntimeEvent::Died {
                            container_id,
                            workload_id,
                            exit_code,
                        }))
                    }
                    "oom" => Some(Ok(RuntimeEvent::Oom {
                        container_id,
                        workload_id,
                    })),
                    _ => None,
                }
            })
            .boxed()
    }

    fn cgroup_paths(&self, id: &str) -> Vec<PathBuf> {
        vec![
            // systemd cgroup driver (spec §10.2 daemon.json)
            PathBuf::from(format!("/sys/fs/cgroup/system.slice/docker-{id}.scope")),
            // cgroupfs driver
            PathBuf::from(format!("/sys/fs/cgroup/docker/{id}")),
        ]
    }

    async fn writable_layer_size(&self, id: &str) -> Result<u64> {
        let r = self
            .docker
            .inspect_container(id, Some(InspectContainerOptions { size: true }))
            .await
            .map_err(map_err)?;
        Ok(r.size_rw.unwrap_or(0).max(0) as u64)
    }
}

// ============================================================================
// Registry credentials (Docker config format, FR-AGE-04)
// ============================================================================

/// Find credentials for `image` in a Docker `config.json`-style file.
pub fn registry_credentials(path: &Path, image: &str) -> Option<RegistryCredentials> {
    use base64::Engine;
    let content = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let auths = json.get("auths")?.as_object()?;
    let registry = image_registry(image);
    let wanted: Vec<String> = if registry == "docker.io" {
        vec![
            "docker.io".into(),
            "index.docker.io".into(),
            "https://index.docker.io/v1/".into(),
            "registry-1.docker.io".into(),
        ]
    } else {
        vec![
            registry.clone(),
            format!("https://{registry}"),
            format!("https://{registry}/v1/"),
            format!("https://{registry}/v2/"),
        ]
    };
    let (server, entry) = auths.iter().find(|(k, _)| wanted.iter().any(|w| w == *k))?;
    if let (Some(u), Some(p)) = (
        entry.get("username").and_then(|v| v.as_str()),
        entry.get("password").and_then(|v| v.as_str()),
    ) {
        return Some(RegistryCredentials {
            username: u.into(),
            password: p.into(),
            server: server.clone(),
        });
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(entry.get("auth")?.as_str()?)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (u, p) = decoded.split_once(':')?;
    Some(RegistryCredentials {
        username: u.into(),
        password: p.into(),
        server: server.clone(),
    })
}

/// Registry host of an image reference (`docker.io` when implicit).
pub fn image_registry(image: &str) -> String {
    match image.split_once('/') {
        Some((first, _)) if first.contains('.') || first.contains(':') || first == "localhost" => {
            first.to_string()
        }
        _ => "docker.io".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_of_image() {
        assert_eq!(image_registry("nginx:1"), "docker.io");
        assert_eq!(image_registry("library/nginx"), "docker.io");
        assert_eq!(image_registry("docker.io/library/nginx:1"), "docker.io");
        assert_eq!(
            image_registry("registry.local:5000/a/b:1"),
            "registry.local:5000"
        );
        assert_eq!(image_registry("ghcr.io/x/y@sha256:abc"), "ghcr.io");
    }

    #[test]
    fn reads_docker_config_auth() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.json");
        // "user:pass"
        std::fs::write(&p, r#"{"auths":{"ghcr.io":{"auth":"dXNlcjpwYXNz"}}}"#).unwrap();
        let c = registry_credentials(&p, "ghcr.io/org/app:1").unwrap();
        assert_eq!((c.username.as_str(), c.password.as_str()), ("user", "pass"));
        assert!(registry_credentials(&p, "nginx:1").is_none());
    }
}
