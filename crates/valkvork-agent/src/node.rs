//! Node information and management.
//!
//! This module handles node identity, capacity calculation, and host volume management.

use std::fs;
use std::path::Path;
use uuid::{Uuid, Uuidv5};
use crate::container_runtime::{ContainerRuntime, DockerRuntime};
use valkvork_common::models::{Node, NodeStatus};
use valkvork_common::units::{Bytes, Millicores, parse_size, parse_cpu};
use valkvork_common::config::AgentConfig;
use valkvork_common::errors::{ValkvorkError, NodeError};

/// Namespace UUID for node IDs
const NODE_ID_NAMESPACE: &str = "4e157846-1971-478f-93f8-c99703b28555";

/// Node manager
pub struct NodeManager {
    config: AgentConfig,
    runtime: Box<dyn ContainerRuntime>,
}

impl NodeManager {
    pub fn new(config: AgentConfig) -> Result<Self, ValkvorkError> {
        let runtime = DockerRuntime::new(config.docker_socket.clone().unwrap_or_default())?;
        Ok(Self { config, runtime })
    }
    
    /// Get or create node ID
    pub fn get_node_id(&self) -> Result<String, ValkvorkError> {
        let node_id_path = Path::new(&self.config.data_dir).join("node_id");
        
        // Try to read existing node ID
        if node_id_path.exists() {
            let node_id = fs::read_to_string(&node_id_path)?;
            return Ok(node_id.trim().to_string());
        }
        
        // Generate new node ID from /etc/machine-id
        let machine_id = self.read_machine_id()?;
        let node_id = generate_node_id(&machine_id);
        
        // Persist node ID
        fs::create_dir_all(&self.config.data_dir)?;
        fs::write(&node_id_path, &node_id)?;
        
        Ok(node_id)
    }
    
    /// Read /etc/machine-id
    fn read_machine_id(&self) -> Result<String, ValkvorkError> {
        let machine_id = fs::read_to_string("/etc/machine-id")?;
        Ok(machine_id.trim().to_string())
    }
    
    /// Get node hostname
    pub fn get_hostname(&self) -> Result<String, ValkvorkError> {
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "unknown".to_string());
        Ok(hostname)
    }
    
    /// Get node IP address
    pub fn get_ip_address(&self) -> Result<String, ValkvorkError> {
        // Try to get the primary IP address
        // This is a simplified implementation
        use std::net::{IpAddr, Ipv4Addr};
        
        // Try to get from environment
        if let Ok(ip) = std::env::var("POD_IP") {
            return Ok(ip);
        }
        
        // Try to resolve hostname
        let hostname = self.get_hostname()?;
        if let Ok(ips) = std::net::lookup_host(&hostname) {
            if !ips.is_empty() {
                return Ok(ips[0].to_string());
            }
        }
        
        // Fallback to localhost
        Ok("127.0.0.1".to_string())
    }
    
    /// Calculate allocatable resources
    pub fn calculate_allocatable(&self) -> Result<(u32, u64), ValkvorkError> {
        // Get total system resources
        let (total_cpu, total_memory) = self.get_system_resources()?;
        
        // Subtract reserved resources
        let reserved_cpu = parse_cpu(self.config.reserved.cpu).as_u32();
        let reserved_memory = parse_size(&self.config.reserved.memory)?.as_u64();
        
        let allocatable_cpu = total_cpu.saturating_sub(reserved_cpu);
        let allocatable_memory = total_memory.saturating_sub(reserved_memory);
        
        Ok((allocatable_cpu, allocatable_memory))
    }
    
    /// Get system resources
    fn get_system_resources(&self) -> Result<(u32, u64), ValkvorkError> {
        // Read from /proc or use sysinfo
        use std::fs;
        
        // CPU: count cores
        let cpu_cores = if Path::new("/proc/cpuinfo").exists() {
            let content = fs::read_to_string("/proc/cpuinfo")?;
            content.lines().filter(|line| line.starts_with("processor")).count() as u32
        } else {
            // Fallback to num_cpus
            std::thread::available_parallelism().unwrap_or(1) as u32
        };
        
        // Memory: read from /proc/meminfo
        let memory_bytes = if Path::new("/proc/meminfo").exists() {
            let content = fs::read_to_string("/proc/meminfo")?;
            for line in content.lines() {
                if line.starts_with("MemTotal:") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        let kb: u64 = parts[1].parse().unwrap_or(0);
                        return Ok((cpu_cores * 1000, kb * 1024));
                    }
                }
            }
            (cpu_cores * 1000, 0)
        } else {
            // Fallback to sysinfo
            (cpu_cores * 1000, 4 * 1024 * 1024 * 1024) // 4 GiB default
        };
        
        Ok((cpu_cores * 1000, memory_bytes))
    }
    
    /// Get host volumes
    pub fn get_host_volumes(&self) -> Vec<String> {
        self.config.host_volumes.keys().cloned().collect()
    }
    
    /// Build node information for registration
    pub fn build_node_info(&self) -> Result<Node, ValkvorkError> {
        let node_id = self.get_node_id()?;
        let hostname = self.get_hostname()?;
        let ip_address = self.get_ip_address()?;
        let (allocatable_cpu, allocatable_memory) = self.calculate_allocatable()?;
        let host_volumes = self.get_host_volumes();
        
        Ok(Node::new(
            node_id,
            hostname,
            ip_address,
            self.config.node_class.clone(),
            "0.1.0".to_string(), // agent version
            self.config.runtime.clone(),
            allocatable_cpu,
            allocatable_memory,
            host_volumes,
        ))
    }
}

/// Generate node ID from machine ID
pub fn generate_node_id(machine_id: &str) -> String {
    let namespace = Uuid::parse_str(NODE_ID_NAMESPACE).unwrap_or_else(|_| Uuid::NAMESPACE_DNS);
    let uuid = Uuidv5::new(&namespace, machine_id.as_bytes());
    uuid.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_generate_node_id() {
        let id1 = generate_node_id("machine-1");
        let id2 = generate_node_id("machine-2");
        let id3 = generate_node_id("machine-1");
        
        assert_ne!(id1, id2);
        assert_eq!(id1, id3);
    }
}
