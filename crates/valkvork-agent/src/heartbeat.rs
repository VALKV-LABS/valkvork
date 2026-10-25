//! Heartbeat management.
//!
//! This module handles periodic heartbeats from the agent to the controller.
//!
//! As per FR-AGE-03, the agent MUST send a Heartbeat every 2 seconds containing
//! node CPU and memory usage and a WorkloadStatus for every Valkvork-managed container.

use std::time::{Instant, Duration};
use std::collections::HashMap;
use valkvork_common::models::{NodeHealth, WorkloadStatus};
use valkvork_common::units::Millicores;

// ============================================================================
// CPU/Memory Reading
// ============================================================================

/// Read CPU usage from cgroup v2 files.
/// 
/// As per FR-AGE-03, the agent MUST read these values directly from the
/// container's cgroup v2 files at /sys/fs/cgroup/system.slice/docker-<container_id>.scope/
/// 
pub fn read_cpu_usage(cgroup_path: &std::path::Path) -> Result<u64, std::io::Error> {
    let cpu_stat_path = cgroup_path.join("cpu.stat");
    let content = std::fs::read_to_string(&cpu_stat_path)?;
    
    // Parse cpu.stat: "usage_usec <user_usec> <system_usec>"
    let mut total_usage = 0u64;
    for line in content.lines() {
        if line.starts_with("usage_usec") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                total_usage = parts[1].parse().unwrap_or(0);
                break;
            }
        }
    }
    
    Ok(total_usage)
}

/// Read memory usage from cgroup v2 files.
/// 
/// Read memory.current which gives total memory usage in bytes.
/// 
pub fn read_memory_usage(cgroup_path: &std::path::Path) -> Result<u64, std::io::Error> {
    let memory_current_path = cgroup_path.join("memory.current");
    let content = std::fs::read_to_string(&memory_current_path)?;
    content.trim().parse().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to parse memory.current")
    })
}

/// Read node-level CPU usage.
/// 
/// This reads from /proc/stat to get total CPU usage.
pub fn read_node_cpu_usage() -> Result<f64, std::io::Error> {
    let stat_path = std::path::Path::new("/proc/stat");
    let content = std::fs::read_to_string(stat_path)?;
    
    // Parse first line: "cpu  <user> <nice> <system> <idle> <iowait> <irq> <softirq> <steal> <guest> <guest_nice>"
    let mut total = 0u64;
    let mut idle = 0u64;
    
    for line in content.lines() {
        if line.starts_with("cpu ") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 9 {
                // Sum all non-idle time (user + nice + system + irq + softirq + steal)
                total += parts[1].parse::<u64>().unwrap_or(0);
                total += parts[2].parse::<u64>().unwrap_or(0);
                total += parts[3].parse::<u64>().unwrap_or(0);
                total += parts[6].parse::<u64>().unwrap_or(0);
                total += parts[7].parse::<u64>().unwrap_or(0);
                total += parts[8].parse::<u64>().unwrap_or(0);
                
                idle += parts[4].parse::<u64>().unwrap_or(0);
            }
            break;
        }
    }
    
    let total_time = total + idle;
    if total_time == 0 {
        return Ok(0.0);
    }
    
    let usage_ratio = total as f64 / total_time as f64;
    Ok(usage_ratio * 100.0)
}

/// Read node-level memory usage.
/// 
/// This reads from /proc/meminfo to get total memory usage.
pub fn read_node_memory_usage() -> Result<u64, std::io::Error> {
    let meminfo_path = std::path::Path::new("/proc/meminfo");
    let content = std::fs::read_to_string(meminfo_path)?;
    
    let mut mem_total = 0u64;
    let mut mem_available = 0u64;
    
    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                mem_total = parts[1].parse::<u64>().unwrap_or(0) * 1024; // KB to bytes
            }
        } else if line.starts_with("MemAvailable:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                mem_available = parts[1].parse::<u64>().unwrap_or(0) * 1024;
            }
        }
    }
    
    Ok(mem_total - mem_available)
}

// ============================================================================
// Container CPU/Memory Tracking
// ============================================================================

/// Tracks CPU and memory usage for containers.
pub struct ContainerMetrics {
    last_cpu_time: HashMap<String, u64>,
    last_cpu_reading: HashMap<String, u64>,
    last_system_cpu: HashMap<String, u64>,
}

impl Default for ContainerMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl ContainerMetrics {
    pub fn new() -> Self {
        Self {
            last_cpu_time: HashMap::new(),
            last_cpu_reading: HashMap::new(),
            last_system_cpu: HashMap::new(),
        }
    }
    
    /// Update metrics for a container.
    pub fn update(
        &mut self,
        container_id: &str,
        cgroup_path: &std::path::Path,
    ) -> Result<(u32, u64), std::io::Error> {
        let cpu_usage = read_cpu_usage(cgroup_path)?;
        let memory_usage = read_memory_usage(cgroup_path)?;
        
        // Calculate CPU usage in millicores
        // For now, return raw values - the percentage calculation needs node CPU info
        Ok((cpu_usage as u32, memory_usage))
    }
}

// ============================================================================
// Heartbeat Manager
// ============================================================================

/// Manages periodic heartbeats.
pub struct HeartbeatManager {
    interval: Duration,
    container_metrics: ContainerMetrics,
    last_node_health: Option<NodeHealth>,
}

impl HeartbeatManager {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            container_metrics: ContainerMetrics::new(),
            last_node_health: None,
        }
    }
    
    /// Collect node health information.
    pub fn collect_node_health(&mut self) -> Result<NodeHealth, std::io::Error> {
        let cpu_usage = read_node_cpu_usage()?;
        let memory_usage = read_node_memory_usage()?;
        
        let health = NodeHealth {
            cpu_usage_percent: cpu_usage,
            memory_used_bytes: memory_usage,
        };
        
        self.last_node_health = Some(health.clone());
        Ok(health)
    }
    
    /// Collect workload statuses.
    pub fn collect_workload_statuses(
        &mut self,
        workload_ids: &[String],
    ) -> Vec<WorkloadStatus> {
        workload_ids.iter().map(|workload_id| {
            // In a real implementation, this would query the container runtime
            // For now, return a placeholder
            WorkloadStatus {
                workload_id: workload_id.clone(),
                service_id: String::new(),
                service_version: 0,
                container_id: String::new(),
                state: valkvork_proto::agent::WorkloadState::Unspecified,
                reason: None,
                exit_code: 0,
                restart_count: 0,
                cpu_usage_millicores: 0,
                memory_used_bytes: 0,
                disk_used_bytes: None,
            }
        }).collect()
    }
    
    /// Get the next heartbeat interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_heartbeat_manager_creation() {
        let manager = HeartbeatManager::new(Duration::from_secs(2));
        assert_eq!(manager.interval(), Duration::from_secs(2));
    }
}
