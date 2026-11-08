//! In-memory rows of the controller's tables (spec §12) plus runtime-only
//! fields that are never persisted.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;
use valkvork_common::models::{
    DesiredState, NodeStatus, RolloutStatus, ServiceSpec, ServiceStatus, WorkloadStatus,
};
use valkvork_proto::v1::ControllerMessage;

pub type AgentSender = tokio::sync::mpsc::Sender<Result<ControllerMessage, tonic::Status>>;

#[derive(Debug, Clone)]
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
    pub last_heartbeat_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Live, in-memory view of a node (FR-CON-01: liveness never hits SQLite on
/// every heartbeat).
#[derive(Debug)]
pub struct NodeEntry {
    pub node: Node,
    /// Controller-clock time of the last message (FR-ARC-02).
    pub last_seen: Instant,
    /// Open stream: (connection id, sender).
    pub conn: Option<(u64, AgentSender)>,
    pub cpu_usage_percent: f64,
    pub used_memory_bytes: u64,
    /// From the agent's last Register; empty until it registers.
    pub features: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Service {
    pub service_id: String,
    pub version: u64,
    pub spec: ServiceSpec,
    pub spec_hash: String,
    pub target_replicas: u32,
    pub status: ServiceStatus,
    pub rollout_status: RolloutStatus,
    pub last_scaled_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Runtime-only per-service bookkeeping.
#[derive(Debug, Default)]
pub struct ServiceRuntime {
    /// Consecutive FAILED workloads of the current version (FR-CON-05).
    pub fail_streak: u32,
    pub last_failure: Option<Instant>,
    /// Autoscaler: when utilisation first went above / below the band.
    pub above_since: Option<Instant>,
    pub below_since: Option<Instant>,
    /// Stop-first moves that could not proceed because the replacement had
    /// nowhere to go; each is reported once.
    pub move_blocked: std::collections::HashSet<String>,
}

#[derive(Debug, Clone)]
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
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub updated_at: i64,
    /// From the agent's reports; not stored (re-reported within a heartbeat).
    pub health: Health,
    /// When the workload last became healthy.
    pub healthy_since: Option<i64>,
}

/// A workload's health-check state, as its agent reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Health {
    /// No health check, or not reported yet.
    #[default]
    Unknown,
    Starting,
    Healthy,
    Unhealthy,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Unknown => "",
            Health::Starting => "STARTING",
            Health::Healthy => "HEALTHY",
            Health::Unhealthy => "UNHEALTHY",
        }
    }
}

impl Workload {
    /// Counts toward replicas (FR-CON-05).
    pub fn is_live(&self) -> bool {
        self.desired_state == DesiredState::Run
            && !matches!(
                self.status,
                WorkloadStatus::Stopped | WorkloadStatus::Failed | WorkloadStatus::Lost
            )
    }

    /// Holds a resource reservation on its node (FR-CON-06 filter).
    pub fn holds_reservation(&self) -> bool {
        self.node_id.is_some() && self.is_live()
    }
}

/// Per-workload CPU samples for the autoscaler (FR-AUT-01): (time, millicores).
pub type CpuSamples = HashMap<String, VecDeque<(Instant, u32)>>;
