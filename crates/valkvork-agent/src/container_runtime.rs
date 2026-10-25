//! Container runtime abstraction.
//!
//! This module provides a trait for container runtime operations and a Docker implementation.
//!
//! As per FR-AGE-09, the agent MUST access the container runtime only through
//! an internal ContainerRuntime trait.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use valkvork_common::models::{PortMapping, VolumeMount, RestartPolicy, WorkloadStatus, WorkloadState};
use valkvork_common::errors::{ValkvorkError, RuntimeError};
use bollard::container::{Config, CreateContainerOptions, ListContainersOptions, StartContainerOptions, StopContainerOptions};
use bollard::image::CreateImageOptions;
use bollard::models::{ContainerCreateResponse, ContainerInspectResponse, ContainerState, HostConfig, PortBinding};
use bollard::Docker;

// ============================================================================
// ContainerRuntime Trait
// ============================================================================

/// Container runtime abstraction trait.
/// 
/// All container runtime operations MUST go through this trait to ensure
/// the agent can be runtime-agnostic (FR-AGE-09).
pub trait ContainerRuntime: Send + Sync {
    /// Pull an image from a registry
    fn pull(&self, image: &str, auth: Option<&str>) -> Result<(), ValkvorkError>;
    
    /// Create a container
    fn create(
        &self,
        name: &str,
        config: &ContainerConfig,
    ) -> Result<String, ValkvorkError>;
    
    /// Start a container
    fn start(&self, container_id: &str) -> Result<(), ValkvorkError>;
    
    /// Stop a container with grace period
    fn stop(
        &self,
        container_id: &str,
        timeout: Duration,
    ) -> Result<(), ValkvorkError>;
    
    /// Remove a container
    fn remove(&self, container_id: &str) -> Result<(), ValkvorkError>;
    
    /// List containers by label
    fn list_by_label(&self, label: &str) -> Result<Vec<ContainerInfo>, ValkvorkError>;
    
    /// Inspect a container
    fn inspect(&self, container_id: &str) -> Result<ContainerInspectInfo, ValkvorkError>;
    
    /// Get container events stream
    fn events(&self) -> Result<(), ValkvorkError>;
    
    /// Get cgroup path for a container
    fn cgroup_path(&self, container_id: &str) -> Result<PathBuf, ValkvorkError>;
    
    /// Get writable layer size for a container
    fn writable_layer_size(&self, container_id: &str) -> Result<u64, ValkvorkError>;
    
    /// Check if container exists
    fn exists(&self, container_id: &str) -> Result<bool, ValkvorkError>;
    
    /// Get runtime driver name and version
    fn driver_info(&self) -> (&str, &str);
}

// ============================================================================
// Container Configuration
// ============================================================================

/// Container configuration for creating a workload container.
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    pub image: String,
    pub cpu_millicores: u32,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub env: HashMap<String, String>,
    pub ports: Vec<PortMapping>,
    pub volumes: Vec<VolumeMount>,
    pub restart_policy: RestartPolicy,
    pub workdir: Option<String>,
    pub user: Option<String>,
}

/// Container information from listing.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub state: ContainerState,
    pub labels: HashMap<String, String>,
    pub created_at: u64,
}

/// Container inspect information.
#[derive(Debug, Clone)]
pub struct ContainerInspectInfo {
    pub id: String,
    pub name: String,
    pub state: ContainerState,
    pub pid: Option<u32>,
    pub exit_code: Option<i64>,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub error: Option<String>,
    pub oom_killed: bool,
    pub cgroup_path: Option<String>,
}

impl From<ContainerInspectResponse> for ContainerInspectInfo {
    fn from(inspect: ContainerInspectResponse) -> Self {
        Self {
            id: inspect.id.unwrap_or_default(),
            name: inspect.name.unwrap_or_default(),
            state: inspect.state.unwrap_or_default(),
            pid: inspect.state.and_then(|s| s.pid),
            exit_code: inspect.state.and_then(|s| s.exit_code),
            started_at: inspect.state.and_then(|s| s.started_at).map(|s| s.timestamp()),
            finished_at: inspect.state.and_then(|s| s.finished_at).map(|s| s.timestamp()),
            error: inspect.state.and_then(|s| s.error),
            oom_killed: inspect.state.map(|s| s.oom_killed).unwrap_or(false),
            cgroup_path: None,
        }
    }
}

// ============================================================================
// Docker Runtime Implementation
// ============================================================================

/// Docker runtime implementation.
/// 
/// Uses the bollard crate to interact with the Docker Engine API.
pub struct DockerRuntime {
    docker: Docker,
    socket_path: PathBuf,
}

impl DockerRuntime {
    pub fn new(socket_path: PathBuf) -> Result<Self, ValkvorkError> {
        let socket = format!("unix://{}", socket_path.display());
        let docker = Docker::connect_with_unix_defaults()
            .map_err(|e| ValkvorkError::Runtime(RuntimeError::Docker(e.to_string())))?;
        
        Ok(Self { docker, socket_path })
    }
}

impl ContainerRuntime for DockerRuntime {
    fn pull(&self, image: &str, auth: Option<&str>) -> Result<(), ValkvorkError> {
        let options = CreateImageOptions {
            from_image: image,
            ..Default::default()
        };
        
        // Set registry auth if provided
        if let Some(auth) = auth {
            // auth is base64 encoded "username:password"
            // Docker API expects this in the X-Registry-Auth header
            // bollard handles this through the from_image option
        }
        
        let mut stream = self.docker.create_image(Some(options), None, None);
        
        while let Some(result) = stream.next() {
            result.map_err(|e| ValkvorkError::Runtime(RuntimeError::ImagePullFailed(e.to_string())))?;
        }
        
        Ok(())
    }
    
    fn create(
        &self,
        name: &str,
        config: &ContainerConfig,
    ) -> Result<String, ValkvorkError> {
        // Convert millicores to NanoCpus (1 core = 1e9 NanoCpus)
        let nano_cpus = config.cpu_millicores as i64 * 1_000_000;
        
        // Build environment variables
        let mut env = Vec::new();
        for (key, value) in &config.env {
            env.push(format!("{}=", key, value));
        }
        
        // Build port bindings
        let mut port_bindings = HashMap::new();
        for port in &config.ports {
            let host_binding = PortBinding {
                host_ip: None,
                host_port: port.host_port as u16,
            };
            port_bindings.insert(
                format!("{}/{}", port.container_port, "tcp"),
                vec![host_binding],
            );
        }
        
        // Build volume mounts
        let mut mounts = Vec::new();
        for volume in &config.volumes {
            use bollard::models::Mount;
            let mount = Mount {
                r#type: Some("bind".to_string()),
                source: Some(volume.source.clone()),
                target: Some(volume.target.clone()),
                read_only: Some(volume.read_only),
                ..Default::default()
            };
            mounts.push(mount);
        }
        
        // Build container config
        let container_config = Config {
            image: Some(config.image.clone()),
            env: if env.is_empty() { None } else { Some(env) },
            cmd: None,
            hostname: None,
            user: config.user.clone(),
            working_dir: config.workdir.clone(),
            labels: Some({
                let mut labels = HashMap::new();
                labels.insert("valkvork.workload_id".to_string(), name.to_string());
                // Note: service_id and service_version will be set by the agent
                labels
            }),
            exposed_ports: None, // Not needed, we use host ports
            stop_signal: Some("SIGTERM".to_string()),
            ..Default::default()
        };
        
        // Build host config
        let host_config = HostConfig {
            binds: None, // We use mounts instead
            port_bindings: Some(port_bindings),
            memory: Some(config.memory_bytes as i64),
            memory_swap: Some(config.memory_bytes as i64), // No swap
            memory_swappiness: None,
            nano_cpus: Some(nano_cpus),
            cpu_shares: None,
            cpu_quota: None,
            cpu_period: None,
            cpu_realtime_period: None,
            cpu_realtime_runtime: None,
            cpuset_cpus: None,
            cpuset_mems: None,
            devices: None,
            device_cgroup_rules: None,
            kernel_memory: None,
            kernel_memory_tcp: None,
            blkio_weight: None,
            blkio_weight_device: None,
            blkio_device_read_bps: None,
            blkio_device_write_bps: None,
            blkio_device_read_iops: None,
            blkio_device_write_iops: None,
            storage_opt: None,
            network_mode: None,
            ulimits: None,
            log_config: None,
            volume_driver: None,
            volumes_from: None,
            console_size: None,
            isolation: None,
            privileged: Some(false),
            publish_all_ports: Some(false),
            readonly_rootfs: Some(false),
            security_opt: None,
            tmpfs: None,
            pids_limit: None,
            cap_add: None,
            cap_drop: None,
            group_add: None,
            userns_mode: None,
            cgroup: None,
            cgroup_parent: None,
            extra_hosts: None,
            volumes: None,
            disk_quota: None,
            mounts: Some(mounts),
            init: None,
            init_path: None,
            shm_size: None,
            runtime: None,
            sysctls: None,
            dns: None,
            dns_opts: None,
            dns_search: None,
            auto_remove: Some(false),
            restart_policy: Some(bollard::models::RestartPolicy {
                name: Some("no".to_string()), // FR-AGE-04: agent owns restarts
                maximum_retry_count: Some(0),
            }),
            ..Default::default()
        };
        
        let options = CreateContainerOptions {
            name: name.to_string(),
            ..Default::default()
        };
        
        let response: ContainerCreateResponse = self.docker.create_container(
            Some(options),
            container_config,
            Some(host_config),
        ).await?;
        
        Ok(response.id)
    }
    
    fn start(&self, container_id: &str) -> Result<(), ValkvorkError> {
        self.docker.start_container(container_id, None)
            .map_err(|e| ValkvorkError::Runtime(RuntimeError::StartFailed(e.to_string())))?;
        Ok(())
    }
    
    fn stop(&self, container_id: &str, timeout: Duration) -> Result<(), ValkvorkError> {
        let options = StopContainerOptions {
            t: Some(timeout.as_secs() as i64),
            ..Default::default()
        };
        
        self.docker.stop_container(container_id, Some(options))
            .map_err(|e| ValkvorkError::Runtime(RuntimeError::StopFailed(e.to_string())))?;
        Ok(())
    }
    
    fn remove(&self, container_id: &str) -> Result<(), ValkvorkError> {
        self.docker.remove_container(container_id, None, false)
            .map_err(|e| ValkvorkError::Runtime(RuntimeError::CreateFailed(e.to_string())))?;
        Ok(())
    }
    
    fn list_by_label(&self, label: &str) -> Result<Vec<ContainerInfo>, ValkvorkError> {
        let options = ListContainersOptions {
            all: true,
            filters: Some({
                let mut filters = HashMap::new();
                filters.insert("label".to_string(), vec![label.to_string()]);
                filters
            }),
            ..Default::default()
        };
        
        let containers = self.docker.list_containers(Some(options))?;
        
        let result: Vec<ContainerInfo> = containers.iter().map(|c| {
            let labels = c.labels.clone().unwrap_or_default();
            ContainerInfo {
                id: c.id.clone().unwrap_or_default(),
                name: c.names.clone().unwrap_or_default().first().cloned().unwrap_or_default(),
                state: c.state.clone().unwrap_or_default(),
                labels,
                created_at: c.created,
            }
        }).collect();
        
        Ok(result)
    }
    
    fn inspect(&self, container_id: &str) -> Result<ContainerInspectInfo, ValkvorkError> {
        let inspect = self.docker.inspect_container(container_id, None)?;
        Ok(inspect.into())
    }
    
    fn events(&self) -> Result<(), ValkvorkError> {
        // Start listening to Docker events
        // This will be implemented as an async stream
        Ok(())
    }
    
    fn cgroup_path(&self, container_id: &str) -> Result<PathBuf, ValkvorkError> {
        // Docker with systemd cgroup driver uses:
        // /sys/fs/cgroup/system.slice/docker-<container_id>.scope/
        let path = PathBuf::from(format!("/sys/fs/cgroup/system.slice/docker-{}.scope", container_id));
        Ok(path)
    }
    
    fn writable_layer_size(&self, container_id: &str) -> Result<u64, ValkvorkError> {
        // Get container inspect and extract SizeRw
        let inspect = self.inspect(container_id)?;
        // In a real implementation, we would parse Docker's internal data
        // For now, return a dummy value
        Ok(0)
    }
    
    fn exists(&self, container_id: &str) -> Result<bool, ValkvorkError> {
        let containers = self.list_by_label(&format!("valkvork.workload_id={}", container_id))?;
        Ok(containers.iter().any(|c| c.id == container_id))
    }
    
    fn driver_info(&self) -> (&str, &str) {
        ("docker", "27.0.0") // Will be updated with actual version
    }
}

// ============================================================================
// Workload Management
// ============================================================================

/// Manage workload containers
pub struct WorkloadManager {
    runtime: Box<dyn ContainerRuntime>,
    alloc_dir: PathBuf,
}

impl WorkloadManager {
    pub fn new(runtime: Box<dyn ContainerRuntime>, alloc_dir: PathBuf) -> Self {
        Self { runtime, alloc_dir }
    }
    
    /// Get alloc directory for a workload
    pub fn alloc_dir(&self, workload_id: &str) -> PathBuf {
        self.alloc_dir.join(workload_id)
    }
    
    /// Start a workload container
    pub fn start_workload(
        &self,
        workload_id: &str,
        service_id: &str,
        service_version: u64,
        config: ContainerConfig,
    ) -> Result<String, ValkvorkError> {
        // Create alloc directory
        let alloc_path = self.alloc_dir(workload_id);
        std::fs::create_dir_all(&alloc_path)?;
        
        // Add workload labels
        let mut env = config.env.clone();
        env.insert("VALKVORK_WORKLOAD_ID".to_string(), workload_id.to_string());
        env.insert("VALKVORK_SERVICE_ID".to_string(), service_id.to_string());
        env.insert("VALKVORK_SERVICE_VERSION".to_string(), service_version.to_string());
        
        let config = ContainerConfig { env, ..config };
        
        // Create and start container
        let container_id = self.runtime.create(
            &format!("vk-{}", workload_id),
            &config,
        )?;
        
        self.runtime.start(&container_id)?;
        
        Ok(container_id)
    }
    
    /// Stop a workload container
    pub fn stop_workload(
        &self,
        workload_id: &str,
        timeout: Duration,
    ) -> Result<(), ValkvorkError> {
        let containers = self.runtime.list_by_label(&format!("valkvork.workload_id={}", workload_id))?;
        
        for container in containers {
            self.runtime.stop(&container.id, timeout)?;
            self.runtime.remove(&container.id)?;
            
            // Clean up alloc directory
            let alloc_path = self.alloc_dir(workload_id);
            if alloc_path.exists() {
                std::fs::remove_dir_all(&alloc_path)?;
            }
        }
        
        Ok(())
    }
    
    /// Get workload status from container
    pub fn get_workload_status(&self, workload_id: &str) -> Result<Option<WorkloadStatus>, ValkvorkError> {
        let containers = self.runtime.list_by_label(&format!("valkvork.workload_id={}", workload_id))?;
        
        if containers.is_empty() {
            return Ok(None);
        }
        
        let container = &containers[0];
        let inspect = self.runtime.inspect(&container.id)?;
        
        let status = match container.state.as_deref() {
            Some("running") => WorkloadState::Running,
            Some("created") | Some("paused") => WorkloadState::Starting,
            Some("restarting") => WorkloadState::Restarting,
            Some("exited") => {
                if inspect.oom_killed {
                    WorkloadState::Failed
                } else if let Some(exit_code) = inspect.exit_code {
                    if exit_code != 0 {
                        WorkloadState::Failed
                    } else {
                        WorkloadState::Stopped
                    }
                } else {
                    WorkloadState::Stopped
                }
            }
            Some("dead") => WorkloadState::Stopped,
            _ => WorkloadState::Unspecified,
        };
        
        // Get restart count from labels if available
        let restart_count = container.labels.get("valkvork.restart_count")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        
        Ok(Some(WorkloadStatus {
            workload_id: workload_id.to_string(),
            service_id: container.labels.get("valkvork.service_id").cloned().unwrap_or_default(),
            service_version: container.labels.get("valkvork.service_version").and_then(|s| s.parse().ok()).unwrap_or(0),
            container_id: container.id.clone(),
            state: status,
            reason: None,
            exit_code: inspect.exit_code.map(|c| c as i32),
            restart_count,
            cpu_usage_millicores: 0, // Will be set by heartbeat
            memory_used_bytes: 0, // Will be set by heartbeat
            disk_used_bytes: Some(0), // Will be set by disk enforcement
        }))
    }
}
