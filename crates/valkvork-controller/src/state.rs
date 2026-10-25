//! Cluster state management.
//!
//! This module maintains the in-memory state of the cluster for fast access
//! by the reconciler, scheduler, and other components.
//!
//! As per FR-CON-01, node liveness and telemetry MUST live in an in-memory
//! concurrent map (DashMap or equivalent).

use std::sync::Arc;
use std::collections::HashMap;
use dashmap::DashMap;
use valkvork_common::models::{Node, Service, Workload, NodeStatus, WorkloadStatus, DesiredState};
use valkvork_common::errors::ValkvorkError;

// ============================================================================
// Node State
// ============================================================================

/// In-memory node information with additional runtime data.
#[derive(Debug, Clone)]
pub struct NodeState {
    pub node: Node,
    pub used_cpu_millicores: u32,
    pub used_memory_bytes: u64,
    pub workload_count: u32,
}

impl NodeState {
    pub fn new(node: Node) -> Self {
        Self {
            node,
            used_cpu_millicores: 0,
            used_memory_bytes: 0,
            workload_count: 0,
        }
    }
    
    /// Check if node is eligible for scheduling.
    pub fn is_eligible(&self) -> bool {
        !self.node.draining && self.node.status == NodeStatus::Healthy
    }
    
    /// Get free CPU in millicores.
    pub fn free_cpu(&self) -> u32 {
        self.node.allocatable_cpu_millicores.saturating_sub(self.used_cpu_millicores)
    }
    
    /// Get free memory in bytes.
    pub fn free_memory(&self) -> u64 {
        self.node.allocatable_memory_bytes.saturating_sub(self.used_memory_bytes)
    }
    
    /// Check if node has a specific host volume.
    pub fn has_volume(&self, volume_name: &str) -> bool {
        self.node.host_volumes.contains(&volume_name.to_string())
    }
    
    /// Check if a port is available.
    pub fn is_port_available(&self, port: u16) -> bool {
        // In a real implementation, track which ports are in use
        // For now, assume all ports are available
        true
    }
}

// ============================================================================
// Cluster State
// ============================================================================

/// The main cluster state.
pub struct ClusterState {
    // Node table: node_id -> NodeState
    nodes: Arc<DashMap<String, NodeState>>,
    
    // Service table: service_id -> Service
    services: Arc<DashMap<String, Service>>,
    
    // Workload table: workload_id -> Workload
    workloads: Arc<DashMap<String, Workload>>,
    
    // Track which workloads are on which nodes
    node_workloads: Arc<DashMap<String, Vec<String>>>, // node_id -> [workload_id]
    
    // Track service workloads
    service_workloads: Arc<DashMap<String, Vec<String>>>, // service_id -> [workload_id]
}

impl ClusterState {
    pub fn new() -> Self {
        Self {
            nodes: Arc::new(DashMap::new()),
            services: Arc::new(DashMap::new()),
            workloads: Arc::new(DashMap::new()),
            node_workloads: Arc::new(DashMap::new()),
            service_workloads: Arc::new(DashMap::new()),
        }
    }
    
    // ================================================================
    // Node Operations
    // ================================================================
    
    /// Add or update a node.
    pub fn upsert_node(&self, node: Node) -> NodeState {
        let mut node_state = self.nodes.entry(node.node_id.clone()).or_insert_with(|| {
            NodeState::new(node.clone())
        });
        
        *node_state = NodeState::new(node);
        node_state.clone()
    }
    
    /// Get a node by ID.
    pub fn get_node(&self, node_id: &str) -> Option<NodeState> {
        self.nodes.get(node_id).map(|entry| entry.value().clone())
    }
    
    /// List all nodes.
    pub fn list_nodes(&self) -> Vec<NodeState> {
        self.nodes.iter().map(|entry| entry.value().clone()).collect()
    }
    
    /// Update node heartbeat timestamp.
    pub fn update_node_heartbeat(&self, node_id: &str) {
        if let Some(mut entry) = self.nodes.get_mut(node_id) {
            entry.node.last_heartbeat_at = chrono::Utc::now();
        }
    }
    
    /// Set node status.
    pub fn set_node_status(&self, node_id: &str, status: NodeStatus) {
        if let Some(mut entry) = self.nodes.get_mut(node_id) {
            entry.node.status = status;
        }
    }
    
    /// Set node draining status.
    pub fn set_node_draining(&self, node_id: &str, draining: bool) {
        if let Some(mut entry) = self.nodes.get_mut(node_id) {
            entry.node.draining = draining;
        }
    }
    
    /// Get nodes by class.
    pub fn get_nodes_by_class(&self, class: &str) -> Vec<NodeState> {
        self.nodes.iter()
            .filter(|entry| entry.node_class == class)
            .map(|entry| entry.value().clone())
            .collect()
    }
    
    /// Get healthy, non-draining nodes.
    pub fn get_eligible_nodes(&self) -> Vec<NodeState> {
        self.nodes.iter()
            .filter(|entry| entry.value().is_eligible())
            .map(|entry| entry.value().clone())
            .collect()
    }
    
    // ================================================================
    // Service Operations
    // ================================================================
    
    /// Add or update a service.
    pub fn upsert_service(&self, service: Service) {
        self.services.insert(service.service_id.clone(), service);
    }
    
    /// Get a service by ID.
    pub fn get_service(&self, service_id: &str) -> Option<Service> {
        self.services.get(service_id).map(|entry| entry.value().clone())
    }
    
    /// List all services.
    pub fn list_services(&self) -> Vec<Service> {
        self.services.iter().map(|entry| entry.value().clone()).collect()
    }
    
    /// Delete a service.
    pub fn delete_service(&self, service_id: &str) {
        self.services.remove(service_id);
    }
    
    // ================================================================
    // Workload Operations
    // ================================================================
    
    /// Add or update a workload.
    pub fn upsert_workload(&self, workload: Workload) {
        // Update service workloads index
        let service_workloads = self.service_workloads.entry(workload.service_id.clone())
            .or_insert_with(Vec::new);
        
        if !service_workloads.contains(&workload.workload_id) {
            service_workloads.push(workload.workload_id.clone());
        }
        
        // Update node workloads index
        if let Some(node_id) = &workload.node_id {
            let node_workloads = self.node_workloads.entry(node_id.clone())
                .or_insert_with(Vec::new);
            
            if !node_workloads.contains(&workload.workload_id) {
                node_workloads.push(workload.workload_id.clone());
            }
        }
        
        // Update workload
        self.workloads.insert(workload.workload_id.clone(), workload);
    }
    
    /// Get a workload by ID.
    pub fn get_workload(&self, workload_id: &str) -> Option<Workload> {
        self.workloads.get(workload_id).map(|entry| entry.value().clone())
    }
    
    /// List all workloads.
    pub fn list_workloads(&self) -> Vec<Workload> {
        self.workloads.iter().map(|entry| entry.value().clone()).collect()
    }
    
    /// List workloads by service.
    pub fn list_workloads_by_service(&self, service_id: &str) -> Vec<Workload> {
        if let Some(workload_ids) = self.service_workloads.get(service_id) {
            workload_ids.value().iter()
                .filter_map(|id| self.get_workload(id))
                .collect()
        } else {
            Vec::new()
        }
    }
    
    /// List workloads by node.
    pub fn list_workloads_by_node(&self, node_id: &str) -> Vec<Workload> {
        if let Some(workload_ids) = self.node_workloads.get(node_id) {
            workload_ids.value().iter()
                .filter_map(|id| self.get_workload(id))
                .collect()
        } else {
            Vec::new()
        }
    }
    
    /// List workloads by status.
    pub fn list_workloads_by_status(&self, status: WorkloadStatus) -> Vec<Workload> {
        self.workloads.iter()
            .filter(|entry| entry.status == status)
            .map(|entry| entry.value().clone())
            .collect()
    }
    
    /// Count live workloads for a service.
    pub fn count_live_workloads(&self, service_id: &str) -> u32 {
        self.list_workloads_by_service(service_id)
            .iter()
            .filter(|w| w.is_live())
            .count() as u32
    }
    
    /// Get Pending workloads for a service.
    pub fn get_pending_workloads(&self, service_id: &str) -> Vec<Workload> {
        self.list_workloads_by_service(service_id)
            .into_iter()
            .filter(|w| w.status == WorkloadStatus::Pending)
            .collect()
    }
    
    /// Get running workloads for a service.
    pub fn get_running_workloads(&self, service_id: &str) -> Vec<Workload> {
        self.list_workloads_by_service(service_id)
            .into_iter()
            .filter(|w| w.status == WorkloadStatus::Running)
            .collect()
    }
    
    // ================================================================
    // Resource Tracking
    // ================================================================
    
    /// Update node resource usage.
    pub fn update_node_resources(&self, node_id: &str, cpu_millicores: u32, memory_bytes: u64) {
        if let Some(mut entry) = self.nodes.get_mut(node_id) {
            entry.used_cpu_millicores = cpu_millicores;
            entry.used_memory_bytes = memory_bytes;
        }
    }
    
    /// Add workload to node resource tracking.
    pub fn add_workload_to_node(&self, node_id: &str, cpu_millicores: u32, memory_bytes: u64) {
        if let Some(mut entry) = self.nodes.get_mut(node_id) {
            entry.used_cpu_millicores += cpu_millicores;
            entry.used_memory_bytes += memory_bytes;
            entry.workload_count += 1;
        }
    }
    
    /// Remove workload from node resource tracking.
    pub fn remove_workload_from_node(&self, node_id: &str, cpu_millicores: u32, memory_bytes: u64) {
        if let Some(mut entry) = self.nodes.get_mut(node_id) {
            entry.used_cpu_millicores = entry.used_cpu_millicores.saturating_sub(cpu_millicores);
            entry.used_memory_bytes = entry.used_memory_bytes.saturating_sub(memory_bytes);
            entry.workload_count = entry.workload_count.saturating_sub(1);
        }
    }
    
    /// Update workload count for a node.
    pub fn update_node_workload_count(&self, node_id: &str, workload_count: u32) {
        if let Some(mut entry) = self.nodes.get_mut(node_id) {
            entry.workload_count = workload_count;
        }
    }
    
    // ================================================================
    // State Synchronization
    // ================================================================
    
    /// Load state from database.
    pub fn load_from_database(&self, db: &crate::database::DatabasePool) -> Result<(), ValkvorkError> {
        // Load nodes
        let nodes = db.list_nodes()?;
        for node in nodes {
            self.upsert_node(node);
        }
        
        // Load services
        let services = db.list_services()?;
        for service in services {
            self.upsert_service(service);
        }
        
        // Load workloads
        let workloads = db.list_workloads()?;
        for workload in workloads {
            self.upsert_workload(workload);
        }
        
        Ok(())
    }
    
    /// Flush node heartbeat times to database.
    pub fn flush_heartbeats(&self, db: &crate::database::DatabasePool) -> Result<(), ValkvorkError> {
        for entry in self.nodes.iter() {
            let node_id = entry.key();
            let node_state = entry.value();
            db.update_node_heartbeat(node_id)?;
        }
        Ok(())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_cluster_state_creation() {
        let state = ClusterState::new();
        assert!(state.list_nodes().is_empty());
        assert!(state.list_services().is_empty());
        assert!(state.list_workloads().is_empty());
    }
    
    #[test]
    fn test_node_operations() {
        let state = ClusterState::new();
        
        let node = Node::new(
            "node-1".to_string(),
            "hostname-1".to_string(),
            "10.0.0.1".to_string(),
            "general".to_string(),
            "0.1.0".to_string(),
            "docker/27.0.0".to_string(),
            4000, // 4 cores
            8 * 1024 * 1024 * 1024, // 8 GiB
            vec!["pgdata".to_string()],
        );
        
        state.upsert_node(node);
        
        assert_eq!(state.list_nodes().len(), 1);
        assert!(state.get_node("node-1").is_some());
        assert!(state.get_node("node-2").is_none());
    }
}
