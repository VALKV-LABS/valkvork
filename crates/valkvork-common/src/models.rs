//! Core data models for Valkvork.
//!
//! This module contains the primary data structures used throughout Valkvork,
//! including services, workloads, nodes, and their states.

use crate::units::{Bytes, Millicores};
use chrono::{DateTime, Utc};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use uuid::Uuid;

// ============================================================================
// Node Models
// ============================================================================

/// Node status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NodeStatus {
    Healthy,
    Unhealthy,
}

impl Default for NodeStatus {
    fn default() -> Self {
        NodeStatus::Healthy
    }
}

impl From<&str> for NodeStatus {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "healthy" => NodeStatus::Healthy,
            _ => NodeStatus::Unhealthy,
        }
    }
}

impl std::fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeStatus::Healthy => write!(f, "HEALTHY"),
            NodeStatus::Unhealthy => write!(f, "UNHEALTHY"),
        }
    }
}

/// Node information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub node_id: String,
    pub hostname: String,
    pub ip_address: String,
    pub node_class: String,
    pub agent_version: String,
    pub runtime: String,
    pub allocatable_cpu_millicores: u32,
    pub allocatable_memory_bytes: u64,
    pub host_volumes: Vec<String>,
    pub status: NodeStatus,
    pub draining: bool,
    pub last_heartbeat_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Node {
    pub fn new(
        node_id: String,
        hostname: String,
        ip_address: String,
        node_class: String,
        agent_version: String,
        runtime: String,
        allocatable_cpu_millicores: u32,
        allocatable_memory_bytes: u64,
        host_volumes: Vec<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            node_id,
            hostname,
            ip_address,
            node_class,
            agent_version,
            runtime,
            allocatable_cpu_millicores,
            allocatable_memory_bytes,
            host_volumes,
            status: NodeStatus::Healthy,
            draining: false,
            last_heartbeat_at: now,
            created_at: now,
            updated_at: now,
        }
    }
    
    /// Check if node is eligible for scheduling
    pub fn is_eligible(&self) -> bool {
        !self.draining && self.status == NodeStatus::Healthy
    }
    
    /// Calculate allocated CPU ratio (0.0 to 1.0)
    pub fn cpu_alloc_ratio(&self, allocated_millicores: u32) -> f64 {
        allocated_millicores as f64 / self.allocatable_cpu_millicores as f64
    }
    
    /// Calculate allocated memory ratio (0.0 to 1.0)
    pub fn memory_alloc_ratio(&self, allocated_bytes: u64) -> f64 {
        allocated_bytes as f64 / self.allocatable_memory_bytes as f64
    }
}

// ============================================================================
// Service Models
// ============================================================================

/// Service status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ServiceStatus {
    Active,
    Degraded,
    Deleting,
}

impl Default for ServiceStatus {
    fn default() -> Self {
        ServiceStatus::Active
    }
}

/// Rollout status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RolloutStatus {
    None,
    InProgress,
    Halted,
}

impl Default for RolloutStatus {
    fn default() -> Self {
        RolloutStatus::None
    }
}

/// Restart policy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartPolicy {
    pub attempts: u32,
    pub interval_seconds: u64,
    pub delay_seconds: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            interval_seconds: 300, // 5 minutes
            delay_seconds: 5,
        }
    }
}

/// Update configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    pub max_parallel: u32,
    pub min_healthy_time_seconds: u64,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            max_parallel: 1,
            min_healthy_time_seconds: 10,
        }
    }
}

/// Autoscaling configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoscaleConfig {
    pub min_replicas: u32,
    pub max_replicas: u32,
    pub target_cpu_percent: f64,
}

/// Port mapping
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMapping {
    pub name: String,
    pub container_port: u16,
    pub host_port: u16,
}

/// Volume mount
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeMount {
    pub r#type: String,
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

/// Service specification (from manifest)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSpec {
    pub id: String,
    pub image: String,
    pub replicas: u32,
    pub kill_timeout_seconds: u32,
    pub cpu_millicores: u32,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub required_class: Option<String>,
    pub env: HashMap<String, String>,
    pub ports: Vec<PortMapping>,
    pub volumes: Vec<VolumeMount>,
    pub autoscale: Option<AutoscaleConfig>,
    pub update: UpdateConfig,
    pub restart: RestartPolicy,
}

impl Default for ServiceSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            image: String::new(),
            replicas: 1,
            kill_timeout_seconds: 10,
            cpu_millicores: 100,
            memory_bytes: 128 * 1024 * 1024, // 128 MiB
            disk_bytes: 1 * 1024 * 1024 * 1024, // 1 GiB
            required_class: None,
            env: HashMap::new(),
            ports: Vec::new(),
            volumes: Vec::new(),
            autoscale: None,
            update: UpdateConfig::default(),
            restart: RestartPolicy::default(),
        }
    }
}

/// Service entity with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub service_id: String,
    pub version: u64,
    pub spec: ServiceSpec,
    pub spec_hash: String,
    pub status: ServiceStatus,
    pub rollout_status: RolloutStatus,
    pub target_replicas: u32,
    pub min_replicas: Option<u32>,
    pub max_replicas: Option<u32>,
    pub target_cpu_percent: Option<f64>,
    pub last_scaled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Service {
    pub fn new(spec: ServiceSpec) -> Self {
        let now = Utc::now();
        Self {
            service_id: spec.id.clone(),
            version: 1,
            spec,
            spec_hash: String::new(),
            status: ServiceStatus::Active,
            rollout_status: RolloutStatus::None,
            target_replicas: 1,
            min_replicas: None,
            max_replicas: None,
            target_cpu_percent: None,
            last_scaled_at: None,
            created_at: now,
            updated_at: now,
        }
    }
    
    pub fn autoscale_enabled(&self) -> bool {
        self.spec.autoscale.is_some()
    }
    
    pub fn bump_version(&mut self) {
        self.version += 1;
    }
}

// ============================================================================
// Workload Models
// ============================================================================

/// Workload desired state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DesiredState {
    Run,
    Stop,
}

impl Default for DesiredState {
    fn default() -> Self {
        DesiredState::Run
    }
}

/// Workload status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkloadStatus {
    Pending,
    Assigned,
    Running,
    Stopping,
    Lost,
    Stopped,
    Failed,
}

impl Default for WorkloadStatus {
    fn default() -> Self {
        WorkloadStatus::Pending
    }
}

impl WorkloadStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, WorkloadStatus::Stopped | WorkloadStatus::Failed)
    }
    
    pub fn is_running(&self) -> bool {
        matches!(self, WorkloadStatus::Running)
    }
}

/// Workload entity
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workload {
    pub workload_id: String,
    pub service_id: String,
    pub service_version: u64,
    pub node_id: Option<String>,
    pub container_id: Option<String>,
    pub desired_state: DesiredState,
    pub status: WorkloadStatus,
    pub status_reason: Option<String>,
    pub replaces_workload_id: Option<String>,
    pub restart_count: u32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl Workload {
    pub fn new(service_id: String, service_version: u64) -> Self {
        let now = Utc::now();
        Self {
            workload_id: Uuid::new_v7().to_string(),
            service_id,
            service_version,
            node_id: None,
            container_id: None,
            desired_state: DesiredState::Run,
            status: WorkloadStatus::Pending,
            status_reason: None,
            replaces_workload_id: None,
            restart_count: 0,
            created_at: now,
            started_at: None,
            updated_at: now,
        }
    }
    
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }
    
    pub fn is_live(&self) -> bool {
        self.desired_state == DesiredState::Run && !self.is_terminal()
    }
}

// ============================================================================
// Event Models
// ============================================================================

/// Event kinds
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventKind {
    Node,
    Workload,
    Deploy,
    Scale,
    Audit,
}

/// Subject types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubjectType {
    Node,
    Service,
    Workload,
}

/// Cluster event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterEvent {
    pub event_id: i64,
    pub kind: EventKind,
    pub subject_type: SubjectType,
    pub subject_id: String,
    pub message: String,
    pub actor: String,
    pub created_at: DateTime<Utc>,
}

impl ClusterEvent {
    pub fn new(
        kind: EventKind,
        subject_type: SubjectType,
        subject_id: String,
        message: String,
        actor: String,
    ) -> Self {
        Self {
            event_id: 0, // Will be set by DB
            kind,
            subject_type,
            subject_id,
            message,
            actor,
            created_at: Utc::now(),
        }
    }
}

/// Scaling event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalingEvent {
    pub event_id: i64,
    pub service_id: String,
    pub from_replicas: u32,
    pub to_replicas: u32,
    pub metric_value: f64,
    pub reason: String,
    pub created_at: DateTime<Utc>,
}

impl ScalingEvent {
    pub fn new(
        service_id: String,
        from_replicas: u32,
        to_replicas: u32,
        metric_value: f64,
        reason: String,
    ) -> Self {
        Self {
            event_id: 0,
            service_id,
            from_replicas,
            to_replicas,
            metric_value,
            reason,
            created_at: Utc::now(),
        }
    }
}

// ============================================================================
// Health and Metrics
// ============================================================================

/// Node health info for heartbeats
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHealth {
    pub cpu_usage_percent: f64,
    pub memory_used_bytes: u64,
}

/// Workload health info from agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadHealth {
    pub workload_id: String,
    pub service_id: String,
    pub service_version: u64,
    pub container_id: String,
    pub state: WorkloadStatus,
    pub reason: Option<String>,
    pub exit_code: Option<i32>,
    pub restart_count: u32,
    pub cpu_usage_millicores: u32,
    pub memory_used_bytes: u64,
    pub disk_used_bytes: Option<u64>,
}

// ============================================================================
// Commands
// ============================================================================

/// Command types sent from controller to agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentCommand {
    StartWorkload(StartWorkloadCommand),
    StopWorkload(StopWorkloadCommand),
}

/// Start workload command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartWorkloadCommand {
    pub command_id: String,
    pub workload_id: String,
    pub service_id: String,
    pub service_version: u64,
    pub image: String,
    pub cpu_millicores: u32,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub env: HashMap<String, String>,
    pub ports: Vec<PortMapping>,
    pub volumes: Vec<VolumeMount>,
    pub restart: RestartPolicy,
    pub kill_timeout_seconds: u32,
}

/// Stop workload command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopWorkloadCommand {
    pub command_id: String,
    pub workload_id: String,
    pub kill_timeout_seconds: u32,
}

/// Command result from agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub command_id: String,
    pub workload_id: String,
    pub accepted: bool,
    pub error: Option<String>,
}
